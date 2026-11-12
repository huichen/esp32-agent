//! When a scheduled task fires, and when a sensor rule counts as matched.
//!
//! Everything here is a pure function of its arguments plus the struct fields:
//! no clock, no `SystemTime`, no ESP timer. The scheduler tick owns the clock and
//! passes `now_ms` (milliseconds since boot) and `now_min_of_day` (0..1440, local
//! time) in. That keeps "does a 60 s task fire twice in the same minute?" a unit
//! test instead of a bench experiment, and it lets the simulator run a whole day
//! of schedules in microseconds.
//!
//! Two rules that are easy to get wrong and are enforced here:
//!
//! * A task never fires twice in the same minute. Intervals are checked with
//!   `>=`, the daily-time case additionally requires more than 60 s since the
//!   last run, so a tick that lands twice inside 07:30 does not run the prompt
//!   twice.
//! * A task that has never run (`last_run_ms == 0`) waits one full interval
//!   instead of firing on the first tick, so boot does not immediately flush every
//!   interval task at once.

use serde_json::Value;

/// A prompt that the scheduler pushes into the agent queue on a timer.
///
/// `every_s > 0` wins over `at_hhmm`; with both empty/zero the task never fires.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(default)]
pub struct ScheduledTask {
    /// Stable id, e.g. `t1`. The portal and `POST /api/sched` address tasks by it.
    pub id: String,
    pub prompt: String,
    /// A script to run instead of asking the model. Empty means "use `prompt`".
    ///
    /// A timed script costs no tokens and no model call, so a rule that should pulse a
    /// relay every five minutes can be scheduled without paying for a chat turn. When both
    /// are set the script wins, and the prompt is kept as documentation of the intent.
    pub script: String,
    /// Interval in seconds; `0` = use `at_hhmm`.
    pub every_s: u32,
    /// Local daily time, `"07:30"` or `""`.
    pub at_hhmm: String,
    pub enabled: bool,
    /// Milliseconds since boot of the last run, `0` = never.
    pub last_run_ms: u64,
}

impl Default for ScheduledTask {
    fn default() -> Self {
        // A task the portal adds without an explicit `enabled` should work, not
        // sit there silently disabled.
        Self {
            id: String::new(),
            prompt: String::new(),
            script: String::new(),
            every_s: 0,
            at_hhmm: String::new(),
            enabled: true,
            last_run_ms: 0,
        }
    }
}

/// A tool result that, when it matches `value`, pushes `prompt` into the queue.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(default)]
pub struct SensorRule {
    pub id: String,
    /// Tool to poll, e.g. `buttons_read`.
    pub tool: String,
    /// Dotted path inside that tool's result, e.g. `boot` or `wifi.rssi`.
    pub path: String,
    /// `"eq"` | `"ne"` | `"gt"` | `"lt"` | `"true"`.
    pub op: String,
    /// Right-hand side for the comparison operators; ignored by `"true"`.
    pub value: Value,
    pub prompt: String,
    /// Floor between two fires, so a noisy sensor cannot flood the agent.
    pub min_interval_s: u32,
    /// Milliseconds since boot of the last fire, `0` = never.
    pub last_fire_ms: u64,
    pub enabled: bool,
}

impl Default for SensorRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            tool: String::new(),
            path: String::new(),
            op: "true".to_string(),
            value: Value::Null,
            prompt: String::new(),
            min_interval_s: 60,
            last_fire_ms: 0,
            enabled: true,
        }
    }
}

/// Parse `"07:30"` into a minute of the day (0..1440). `1..2` digit hours are
/// accepted (`"7:05"`), anything out of range or malformed is `None`, so the
/// caller can reject it at the API edge instead of storing a task that never fires.
pub fn parse_hhmm(at: &str) -> Option<u32> {
    let (h, m) = at.trim().split_once(':')?;
    if h.is_empty()
        || m.is_empty()
        || !h.chars().all(|c| c.is_ascii_digit())
        || !m.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let hours: u32 = h.parse().ok()?;
    let minutes: u32 = m.parse().ok()?;
    if hours > 23 || minutes > 59 || h.len() > 2 || m.len() > 2 {
        return None;
    }
    Some(hours * 60 + minutes)
}

/// Should `task` run now?
///
/// * disabled, or no schedule at all -> never;
/// * `every_s > 0` -> at least `every_s` seconds since the last run;
/// * otherwise `at_hhmm` -> once per day, on that minute, at least a minute after
///   the previous run.
pub fn due(task: &ScheduledTask, now_ms: u64, now_min_of_day: u32) -> bool {
    if !task.enabled {
        return false;
    }
    let since_last = now_ms.saturating_sub(task.last_run_ms);
    if task.every_s > 0 {
        let interval = (task.every_s as u64).saturating_mul(1000);
        return since_last >= interval;
    }
    let Some(minute) = parse_hhmm(&task.at_hhmm) else { return false };
    // The caller may hand in a wrapped minute-of-day; 1440 mod is cheap insurance.
    if minute != now_min_of_day % 1440 {
        return false;
    }
    since_last > 60_000
}

/// Does a tool result satisfy the rule's operator? A disabled rule never matches.
pub fn rule_matches(rule: &SensorRule, value: &Value) -> bool {
    if !rule.enabled {
        return false;
    }
    match rule.op.as_str() {
        "eq" => json_eq(value, &rule.value),
        "ne" => !json_eq(value, &rule.value),
        "gt" => compare(value, &rule.value).map(|o| o.is_gt()).unwrap_or(false),
        "lt" => compare(value, &rule.value).map(|o| o.is_lt()).unwrap_or(false),
        "true" => truthy(value),
        // Unknown operator: never fire rather than fire on every poll.
        _ => false,
    }
}

/// Follow a dotted path into a JSON value: `"wifi.ip"`, `"items.0.name"` and
/// `"items[0].name"` all work. An empty path is the value itself. A missing key,
/// an out-of-range index or a path that walks into a scalar gives `None`; a key
/// that exists and holds `null` gives `Some(null)`.
pub fn json_path(v: &Value, path: &str) -> Option<Value> {
    let path = path.trim();
    if path.is_empty() || path == "." || path == "/" {
        return Some(v.clone());
    }
    let mut cur = v;
    for seg in path_segments(path) {
        cur = match cur {
            Value::Object(map) => map.get(&seg)?,
            Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur.clone())
}

/// `a[0].b` -> `["a", "0", "b"]`.
fn path_segments(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in path.chars() {
        match ch {
            '.' | '[' | ']' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (num(a), num(b)) {
        // `1`, `1.0` and `"1"` are the same reading of a sensor value.
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    num(a)?.partial_cmp(&num(b)?)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => {
            let s = s.trim().to_ascii_lowercase();
            !(s.is_empty() || s == "0" || s == "false" || s == "off" || s == "no")
        }
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(every_s: u32, at_hhmm: &str, last_run_ms: u64) -> ScheduledTask {
        // Built explicitly: `..Default::default()` would be tempting, but the
        // point of these cases is exactly the `enabled` / `every_s` combination.
        ScheduledTask {
            id: "t1".into(),
            prompt: "report temperature".into(),
            script: String::new(),
            every_s,
            at_hhmm: at_hhmm.into(),
            enabled: true,
            last_run_ms,
        }
    }

    fn rule(op: &str, value: Value) -> SensorRule {
        SensorRule {
            id: "r1".into(),
            tool: "buttons_read".into(),
            path: "boot".into(),
            op: op.into(),
            value,
            prompt: "boot button pressed".into(),
            min_interval_s: 60,
            last_fire_ms: 0,
            enabled: true,
        }
    }

    // -------------------------------------------------------------- hh:mm

    #[test]
    fn hhmm_parses_only_real_times() {
        assert_eq!(parse_hhmm("07:30"), Some(450));
        assert_eq!(parse_hhmm("7:05"), Some(425));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm(" 08:00 "), Some(480));
        for bad in ["", "0730", "24:00", "07:60", "ab:cd", "07:", ":30", "-1:00", "007:00", "07:5x"] {
            assert_eq!(parse_hhmm(bad), None, "{bad} should not parse");
        }
    }

    // ---------------------------------------------------------------- due

    #[test]
    fn interval_task_waits_one_full_interval() {
        // Never run: the brief's explicit case — nothing at boot.
        let t = task(60, "", 0);
        assert!(!due(&t, 0, 0));
        assert!(!due(&t, 59_999, 0));
        assert!(due(&t, 60_000, 0));

        // Has run: strictly interval-based, not phase-based.
        let t = task(60, "", 10_000);
        assert!(!due(&t, 69_999, 0));
        assert!(due(&t, 70_000, 0));
        assert!(due(&t, 200_000, 0));

        // Short intervals and overflow-ish intervals both stay sane.
        assert!(due(&task(1, "", 0), 1_000, 0));
        assert!(!due(&task(u32::MAX, "", 0), 10_000, 0));
    }

    #[test]
    fn daily_task_fires_once_on_its_minute_and_not_twice() {
        let t = task(0, "07:30", 0);
        // Wrong minute: never.
        assert!(!due(&t, 100_000, 449));
        assert!(!due(&t, 100_000, 451));
        // Right minute, never run, board up for a while: fires.
        assert!(due(&t, 100_000, 450));

        // It ran: the same minute must not fire again, however many ticks land.
        let fired = ScheduledTask { last_run_ms: 100_000, ..t.clone() };
        assert!(!due(&fired, 100_000, 450));
        assert!(!due(&fired, 160_000, 450));
        // A minute later in the same 24 h day is a different minute anyway.
        assert!(!due(&fired, 220_000, 451));
        // Next day, same minute: fires again.
        assert!(due(&fired, 100_000 + 86_400_000, 450));
    }

    #[test]
    fn every_s_wins_over_at_hhmm_and_disabled_never_fires() {
        let both = task(60, "07:30", 0);
        assert!(due(&both, 60_000, 0), "interval takes precedence");

        let off = ScheduledTask { enabled: false, last_run_ms: 0, ..task(60, "", 0) };
        assert!(!due(&off, 10_000_000, 450));
        let off_daily = ScheduledTask { enabled: false, ..task(0, "07:30", 0) };
        assert!(!due(&off_daily, 10_000_000, 450));

        // No schedule at all: never, and never an infinite retry loop.
        let empty = task(0, "", 0);
        assert!(!due(&empty, 10_000_000, 450));
        assert!(!due(&task(0, "7:30", 0), 10_000_000, 449));
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_fire() {
        // last_run_ms in the future (NVS restored, counter wrapped): saturating,
        // and definitely not a fire.
        let t = task(60, "", 500_000);
        assert!(!due(&t, 10_000, 0));
    }

    // ---------------------------------------------------------- json path

    #[test]
    fn json_path_walks_nested_objects_and_arrays() {
        let v = json!({
            "boot": true,
            "wifi": { "ip": "192.0.2.42", "rssi": -61 },
            "items": [ { "name": "a" }, { "name": "b" } ],
            "null_field": null,
        });
        assert_eq!(json_path(&v, "boot"), Some(json!(true)));
        assert_eq!(json_path(&v, "wifi.ip"), Some(json!("192.0.2.42")));
        assert_eq!(json_path(&v, "wifi.rssi"), Some(json!(-61)));
        assert_eq!(json_path(&v, "items.0.name"), Some(json!("a")));
        assert_eq!(json_path(&v, "items[1].name"), Some(json!("b")));
        assert_eq!(json_path(&v, "wifi"), Some(json!({"ip": "192.0.2.42", "rssi": -61})));
        assert_eq!(json_path(&v, "null_field"), Some(Value::Null));
        // Empty path = the whole value.
        assert_eq!(json_path(&v, ""), Some(v.clone()));
        assert_eq!(json_path(&v, "."), Some(v.clone()));
    }

    #[test]
    fn json_path_returns_none_for_missing_keys_and_bad_walks() {
        let v = json!({ "a": { "b": 1 }, "list": [1, 2] });
        assert_eq!(json_path(&v, "missing"), None);
        assert_eq!(json_path(&v, "a.missing"), None);
        assert_eq!(json_path(&v, "a.b.deep"), None, "cannot index a scalar");
        assert_eq!(json_path(&v, "list.9"), None, "index out of range");
        assert_eq!(json_path(&v, "list.x"), None, "non-numeric index");
        assert_eq!(json_path(&json!(7), "x"), None, "path into a scalar");
        assert_eq!(json_path(&json!(null), "x"), None);
    }

    // ------------------------------------------------------- rule_matches

    #[test]
    fn rule_matches_eq_and_ne() {
        assert!(rule_matches(&rule("eq", json!(true)), &json!(true)));
        assert!(!rule_matches(&rule("eq", json!(true)), &json!(false)));
        assert!(rule_matches(&rule("eq", json!(1)), &json!(1)));
        assert!(rule_matches(&rule("eq", json!(1)), &json!(1.0)));
        assert!(rule_matches(&rule("eq", json!("on")), &json!("on")));
        assert!(!rule_matches(&rule("eq", json!("on")), &json!("off")));
        assert!(rule_matches(&rule("eq", json!({"a": 1})), &json!({"a": 1})), "objects compare structurally");

        assert!(rule_matches(&rule("ne", json!(true)), &json!(false)));
        assert!(!rule_matches(&rule("ne", json!(true)), &json!(true)));
        // A missing value is `null`, which is not equal to `true`.
        assert!(rule_matches(&rule("ne", json!(true)), &Value::Null));
    }

    #[test]
    fn rule_matches_gt_and_lt() {
        assert!(rule_matches(&rule("gt", json!(30)), &json!(31)));
        assert!(!rule_matches(&rule("gt", json!(30)), &json!(30)));
        assert!(rule_matches(&rule("lt", json!(30)), &json!(29.5)));
        assert!(!rule_matches(&rule("lt", json!(30)), &json!(30)));
        // A numeric string from a tool result still compares as a number.
        assert!(rule_matches(&rule("gt", json!(30)), &json!("31")));
        // Not comparable: never fire.
        assert!(!rule_matches(&rule("gt", json!(30)), &json!("hot")));
        assert!(!rule_matches(&rule("lt", json!(30)), &Value::Null));
    }

    #[test]
    fn rule_matches_true_operator() {
        assert!(rule_matches(&rule("true", Value::Null), &json!(true)));
        assert!(!rule_matches(&rule("true", Value::Null), &json!(false)));
        assert!(rule_matches(&rule("true", Value::Null), &json!(1)));
        assert!(!rule_matches(&rule("true", Value::Null), &json!(0)));
        assert!(!rule_matches(&rule("true", Value::Null), &Value::Null));
        assert!(rule_matches(&rule("true", Value::Null), &json!("pressed")));
        assert!(!rule_matches(&rule("true", Value::Null), &json!("")));
        assert!(!rule_matches(&rule("true", Value::Null), &json!("false")));
    }

    #[test]
    fn unknown_operator_and_disabled_rules_never_fire() {
        assert!(!rule_matches(&rule("gtte", json!(1)), &json!(2)));
        assert!(!rule_matches(&rule("", json!(1)), &json!(1)));
        let mut r = rule("true", Value::Null);
        r.enabled = false;
        assert!(!rule_matches(&r, &json!(true)));
    }

    // ---------------------------------------------------------- wire format

    #[test]
    fn structs_survive_the_json_the_portal_posts() {
        let t: ScheduledTask = serde_json::from_str(r#"{"id":"t1","prompt":"p","every_s":60}"#).unwrap();
        assert_eq!(t.every_s, 60);
        assert!(t.enabled, "a task posted without `enabled` must be enabled");
        assert_eq!(t.at_hhmm, "");

        let r: SensorRule = serde_json::from_str(
            r#"{"id":"r1","tool":"buttons_read","path":"boot","op":"eq","value":true,"prompt":"p"}"#,
        )
        .unwrap();
        assert_eq!(r.min_interval_s, 60);
        assert!(r.enabled);

        // Round trip through the API shape.
        let back: ScheduledTask = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }
}
