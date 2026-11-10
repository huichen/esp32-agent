//! One objective the agent works through on its own, one small step at a time.
//!
//! This is the "rough loop" of gnhf (kunchenguid/gnhf) cut down to what fits on an
//! ESP32: the objective is stated once, every turn of the loop is prompted with
//! that objective plus the notes written by earlier turns, the reply is classified
//! as met / blocked / continue, and the loop decides whether to schedule another
//! iteration or stop.
//!
//! Simplifications versus the original, stated so nobody mistakes them for bugs:
//!
//! * no git: a one-line note replaces a commit, so there is no diff and no
//!   history to bisect - the notes *are* the record;
//! * no rollback: a physical action that a failed iteration already performed is
//!   not undone, and a failed iteration still consumes one of the budgeted
//!   iterations;
//! * notes live in NVS and are therefore small: each is truncated to
//!   [`NOTE_MAX_CHARS`] and at most [`NOTE_CAP`] are kept, oldest dropped first;
//! * one goal at a time, because there is one agent thread.
//!
//! Nothing here reads a clock, a store or HTTP: `now_ms` is passed in by the
//! caller, so every rule below is a unit test rather than a bench experiment.
//! `app.rs` owns the thread, the store and the "a turn finished" wiring.

use serde_json::{json, Value};
use std::fmt::Write as _;

/// Iterations a goal gets when the caller does not say.
pub const DEFAULT_MAX_ITERATIONS: u32 = 10;

/// Longest single note, in characters. Longer replies are cut on a char boundary.
pub const NOTE_MAX_CHARS: usize = 240;

/// How many notes are remembered; the oldest one is dropped past this.
pub const NOTE_CAP: usize = 24;

/// Consecutive failed iterations after which the goal is given up as blocked.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Lower bound applied to `max_iterations` by [`Goal::new`] and [`Goal::from_json`].
pub const MIN_MAX_ITERATIONS: u32 = 1;

/// Upper bound applied to `max_iterations` by [`Goal::new`] and [`Goal::from_json`].
pub const MAX_MAX_ITERATIONS: u32 = 100;

/// The iteration prompt is kept to this many characters, notes included.
pub const PROMPT_MAX_CHARS: usize = 1200;

/// The objective is quoted up to this many characters inside the prompt.
const PROMPT_OBJECTIVE_CHARS: usize = 200;

/// Room kept free for the "(n earlier note(s) omitted)" line.
const PROMPT_OMIT_RESERVE: usize = 40;

/// Longest `stop_reason`, in characters.
const STOP_REASON_MAX_CHARS: usize = 200;

/// Where a goal is in its life. Only [`GoalStatus::Running`] is active.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GoalStatus {
    /// No goal has been set (or one was deleted).
    Idle,
    /// The loop may queue the next iteration.
    Running,
    /// The model reported the objective achieved.
    Met,
    /// The objective cannot proceed, or too many iterations failed in a row.
    Blocked,
    /// The iteration budget ran out first.
    Exhausted,
    /// A person stopped it. A stopped goal never resumes.
    Stopped,
}

impl GoalStatus {
    /// The spelling used in JSON, the portal and `/api/status`.
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Idle => "idle",
            GoalStatus::Running => "running",
            GoalStatus::Met => "met",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Exhausted => "exhausted",
            GoalStatus::Stopped => "stopped",
        }
    }

    /// Only `Running` drives the loop; every terminal state is inactive.
    pub fn is_active(self) -> bool {
        matches!(self, GoalStatus::Running)
    }

    /// Parse [`GoalStatus::as_str`] back. `None` for an unknown word, which is
    /// what makes [`Goal::from_json`] reject a corrupted state document.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "idle" => Some(GoalStatus::Idle),
            "running" => Some(GoalStatus::Running),
            "met" => Some(GoalStatus::Met),
            "blocked" => Some(GoalStatus::Blocked),
            "exhausted" => Some(GoalStatus::Exhausted),
            "stopped" => Some(GoalStatus::Stopped),
            _ => None,
        }
    }
}

/// What a turn's reply says about the goal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    /// The reply contained `GOAL MET`.
    Met,
    /// The reply contained `GOAL BLOCKED`.
    Blocked,
    /// Anything else: one more iteration is worth a try.
    Continue,
}

/// The objective, how far it got, and what the earlier iterations learned.
#[derive(Clone, Debug)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    /// Completed iterations, failures included.
    pub iteration: u32,
    /// Iteration budget, always within `MIN_MAX_ITERATIONS..=MAX_MAX_ITERATIONS`.
    pub max_iterations: u32,
    /// Consecutive failed iterations; reset by a turn that completed.
    pub failures: u32,
    /// Oldest first, each at most [`NOTE_MAX_CHARS`], at most [`NOTE_CAP`] of them.
    pub notes: Vec<String>,
    /// One line from the last completed iteration.
    pub last_result: String,
    /// Why the goal stopped, when it did not simply run out of iterations.
    pub stop_reason: String,
    /// Milliseconds since boot of the last state change.
    pub updated_ms: u64,
}

impl Goal {
    /// Start a goal. `None` when the objective is empty once trimmed, because a
    /// goal without an objective would loop on nothing until the cap.
    pub fn new(objective: &str, max_iterations: u32, now_ms: u64) -> Option<Self> {
        let objective = objective.trim();
        if objective.is_empty() {
            return None;
        }
        Some(Self {
            objective: objective.to_string(),
            status: GoalStatus::Running,
            iteration: 0,
            max_iterations: clamp_max_iterations(max_iterations),
            failures: 0,
            notes: Vec::new(),
            last_result: String::new(),
            stop_reason: String::new(),
            updated_ms: now_ms,
        })
    }

    /// The prompt for the next iteration, or `None` when the loop must not run.
    ///
    /// It carries the objective, the iteration number and cap, the notes written
    /// so far and the two marker words the model must print. The note list is
    /// narrowed from the oldest end when the prompt would exceed
    /// [`PROMPT_MAX_CHARS`]: 24 notes of full length cannot fit in one prompt, and
    /// the newest notes are the ones the next step needs.
    pub fn next_prompt(&self) -> Option<String> {
        if self.status != GoalStatus::Running {
            return None;
        }
        let objective = take_chars(self.objective.trim(), PROMPT_OBJECTIVE_CHARS);
        let mut out = String::with_capacity(PROMPT_MAX_CHARS.min(2048));
        let _ = writeln!(out, "Goal: {objective}");
        let _ = writeln!(out, "Iteration {} of {}.", self.iteration.saturating_add(1), self.max_iterations);
        out.push_str("Notes so far:\n");

        // Everything except the notes, plus room for the omission marker.
        let tail = "\nTake exactly one concrete step towards this goal, then reply \
                    with one line saying what you did.\nPrint a line containing GOAL MET \
                    when the objective is achieved, or GOAL BLOCKED when it cannot proceed.\n";
        let fixed = out.chars().count() + tail.chars().count() + PROMPT_OMIT_RESERVE;
        let mut budget = PROMPT_MAX_CHARS.saturating_sub(fixed);

        let total = self.notes.len();
        let mut lines: Vec<String> = Vec::new();
        for (idx, note) in self.notes.iter().enumerate().rev() {
            let line = format!("{}. {}\n", idx + 1, note.trim());
            let cost = line.chars().count();
            if cost > budget {
                break;
            }
            budget -= cost;
            lines.push(line);
        }
        let shown = lines.len();
        let omitted = total.saturating_sub(shown);
        if omitted > 0 {
            let _ = writeln!(out, "({omitted} earlier note(s) omitted)");
        }
        if shown == 0 {
            out.push_str("(none yet)\n");
        } else {
            for line in lines.iter().rev() {
                out.push_str(line);
            }
        }
        out.push_str(tail);

        // Belt and braces: the arithmetic above keeps us under the cap, but a
        // prompt for the model is not worth a panic if it ever does not.
        if out.chars().count() > PROMPT_MAX_CHARS {
            out = take_chars(&out, PROMPT_MAX_CHARS);
        }
        Some(out)
    }

    /// Record a completed iteration from the turn's reply and say what happens next.
    pub fn observe(&mut self, reply: &str, now_ms: u64) -> GoalStatus {
        self.iteration = self.iteration.saturating_add(1);
        let line = one_line(reply);
        let line = if line.is_empty() { "(empty reply)" } else { line };
        self.push_note(line);
        self.last_result = take_chars(line, NOTE_MAX_CHARS);
        self.failures = 0;
        self.status = match classify(reply) {
            Reply::Met => GoalStatus::Met,
            Reply::Blocked => {
                if self.stop_reason.is_empty() {
                    self.stop_reason = take_chars(line, STOP_REASON_MAX_CHARS);
                }
                GoalStatus::Blocked
            }
            Reply::Continue => {
                if self.iteration >= self.max_iterations {
                    self.stop_reason = format!("iteration cap reached ({})", self.max_iterations);
                    GoalStatus::Exhausted
                } else {
                    GoalStatus::Running
                }
            }
        };
        self.updated_ms = now_ms;
        self.status
    }

    /// Record an iteration whose turn failed, for example a model call that timed out.
    ///
    /// This still consumes an iteration, as in gnhf: retrying for free would let a
    /// board with a flaky uplink loop forever. Three in a row give up entirely.
    pub fn fail_iteration(&mut self, error: &str, now_ms: u64) -> GoalStatus {
        self.iteration = self.iteration.saturating_add(1);
        self.failures = self.failures.saturating_add(1);
        let line = one_line(error);
        let line = if line.is_empty() { "no error text" } else { line };
        self.push_note(&format!("failed: {line}"));
        self.last_result = take_chars(&format!("failed: {line}"), NOTE_MAX_CHARS);
        self.status = if self.failures >= MAX_CONSECUTIVE_FAILURES {
            self.stop_reason = format!("{MAX_CONSECUTIVE_FAILURES} consecutive iterations failed");
            GoalStatus::Blocked
        } else if self.iteration >= self.max_iterations {
            self.stop_reason = format!("iteration cap reached ({})", self.max_iterations);
            GoalStatus::Exhausted
        } else {
            GoalStatus::Running
        };
        self.updated_ms = now_ms;
        self.status
    }

    /// Stop the loop. There is no resume: start a new goal instead.
    pub fn stop(&mut self, reason: &str, now_ms: u64) {
        self.status = GoalStatus::Stopped;
        let reason = reason.trim();
        self.stop_reason =
            if reason.is_empty() { "stopped".to_string() } else { take_chars(reason, STOP_REASON_MAX_CHARS) };
        self.updated_ms = now_ms;
    }

    /// Every field, in the shape `GET /api/goal` returns (minus `prompt`, which
    /// the route adds so that a stored goal and a fresh one look the same).
    pub fn to_json(&self) -> Value {
        let notes: Vec<Value> = self.notes.iter().map(|n| json!(n)).collect();
        json!({
            "objective": self.objective,
            "status": self.status.as_str(),
            "iteration": self.iteration,
            "max_iterations": self.max_iterations,
            "failures": self.failures,
            "notes": notes,
            "last_result": self.last_result,
            "stop_reason": self.stop_reason,
            "updated_ms": self.updated_ms,
        })
    }

    /// Read back what [`Goal::to_json`] wrote, tolerating a document that gained
    /// or lost keys.
    ///
    /// `None` only when the document cannot be a goal at all: no objective, or a
    /// status word this build does not know. Everything else has a safe default,
    /// and `max_iterations` is clamped so a stored goal cannot be talked into
    /// 4 billion iterations.
    pub fn from_json(v: &Value) -> Option<Self> {
        let objective = v.get("objective")?.as_str()?.trim();
        if objective.is_empty() {
            return None;
        }
        let status = match v.get("status") {
            Some(Value::String(s)) => GoalStatus::parse(s)?,
            // A status of another type is not something to guess about.
            Some(Value::Null) | None => GoalStatus::Idle,
            Some(_) => return None,
        };
        let notes = value_to_notes(v.get("notes"));
        let max_iterations = clamp_max_iterations(u32_field(v, "max_iterations", DEFAULT_MAX_ITERATIONS));
        Some(Self {
            objective: objective.to_string(),
            status,
            iteration: u32_field(v, "iteration", 0),
            max_iterations,
            failures: u32_field(v, "failures", 0),
            notes,
            last_result: take_chars(&str_field(v, "last_result"), NOTE_MAX_CHARS),
            stop_reason: take_chars(&str_field(v, "stop_reason"), STOP_REASON_MAX_CHARS),
            updated_ms: u64_field(v, "updated_ms", 0),
        })
    }

    /// Truncate to [`NOTE_MAX_CHARS`], append, then drop the oldest past [`NOTE_CAP`].
    fn push_note(&mut self, note: &str) {
        self.notes.push(take_chars(note.trim(), NOTE_MAX_CHARS));
        let over = self.notes.len().saturating_sub(NOTE_CAP);
        if over > 0 {
            self.notes.drain(..over);
        }
    }
}

/// Keep a requested iteration budget inside the supported range.
pub fn clamp_max_iterations(n: u32) -> u32 {
    n.clamp(MIN_MAX_ITERATIONS, MAX_MAX_ITERATIONS)
}

/// Read a reply. The marker words are the whole contract with the model, so the
/// check is case-insensitive and looks at the entire text, not one line.
///
/// A reply that says "not GOAL MET yet" counts as met: negation is not parsed,
/// because a model that prints the marker has made a claim either way and the
/// alternative is guessing at its intent. `GOAL MET` wins over `GOAL BLOCKED`
/// when both appear, since the loop only needs to stop.
pub fn classify(reply: &str) -> Reply {
    let upper = reply.to_ascii_uppercase();
    if upper.contains("GOAL MET") {
        Reply::Met
    } else if upper.contains("GOAL BLOCKED") {
        Reply::Blocked
    } else {
        Reply::Continue
    }
}

/// The first non-empty line, trimmed. A reply usually opens with a blank line, and
/// a leading blank line is not the note.
fn one_line(text: &str) -> &str {
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    ""
}

/// Cut to at most `max` characters on a char boundary, never mid-codepoint.
fn take_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => s.get(..cut).unwrap_or(s).to_string(),
        None => s.to_string(),
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn u64_field(v: &Value, key: &str, default: u64) -> u64 {
    match v.get(key) {
        Some(Value::Number(n)) => {
            n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)).unwrap_or(default)
        }
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(default),
        _ => default,
    }
}

fn u32_field(v: &Value, key: &str, default: u32) -> u32 {
    u32::try_from(u64_field(v, key, u64::from(default))).unwrap_or(u32::MAX)
}

/// Notes as stored: strings only, each bounded, only the newest [`NOTE_CAP`].
fn value_to_notes(v: Option<&Value>) -> Vec<String> {
    let Some(items) = v.and_then(Value::as_array) else { return Vec::new() };
    let mut out: Vec<String> =
        items.iter().filter_map(Value::as_str).map(|n| take_chars(n.trim(), NOTE_MAX_CHARS)).collect();
    let over = out.len().saturating_sub(NOTE_CAP);
    if over > 0 {
        out.drain(..over);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn goal(max: u32) -> Goal {
        Goal::new("water the plants", max, 1_000).expect("objective is not empty")
    }

    // ------------------------------------------------------------ creation

    #[test]
    fn new_rejects_an_empty_objective_and_clamps_the_budget() {
        assert!(Goal::new("", 10, 0).is_none());
        assert!(Goal::new("   \n\t ", 10, 0).is_none());

        let g = Goal::new("  water the plants  ", 0, 5).expect("goal");
        assert_eq!(g.objective, "water the plants", "the objective is trimmed");
        assert_eq!(g.max_iterations, 1, "a zero budget becomes one iteration");
        assert_eq!(Goal::new("x", 1_000, 0).expect("goal").max_iterations, 100);
        assert_eq!(Goal::new("x", 10, 0).expect("goal").max_iterations, 10);
        assert_eq!(g.status, GoalStatus::Running, "a started goal is active");
        assert_eq!(g.iteration, 0);
        assert!(g.notes.is_empty());
        assert_eq!(g.updated_ms, 5);
        assert!(g.status.is_active());
    }

    #[test]
    fn only_running_has_a_prompt() {
        let mut g = goal(3);
        assert!(g.next_prompt().is_some());

        for status in [
            GoalStatus::Idle,
            GoalStatus::Met,
            GoalStatus::Blocked,
            GoalStatus::Exhausted,
            GoalStatus::Stopped,
        ] {
            g.status = status;
            assert!(g.next_prompt().is_none(), "{} must not prompt", status.as_str());
            assert!(!status.is_active());
        }
    }

    // -------------------------------------------------------------- prompt

    #[test]
    fn the_prompt_carries_the_objective_notes_and_both_markers() {
        let mut g = goal(10);
        g.observe("checked the soil: dry", 2_000);
        g.observe("filled the can", 3_000);

        let p = g.next_prompt().expect("running goals have a prompt");
        assert!(p.contains("water the plants"), "objective: {p}");
        // Two iterations are done, so this prompt asks for the third.
        assert!(p.contains("Iteration 3 of 10"), "position in the loop: {p}");
        assert!(p.contains("1. checked the soil: dry"), "oldest note first: {p}");
        assert!(p.contains("2. filled the can"), "newest note last: {p}");
        assert!(p.contains("GOAL MET"), "the met marker: {p}");
        assert!(p.contains("GOAL BLOCKED"), "the blocked marker: {p}");
        assert!(p.contains("one concrete step"), "one step per turn: {p}");
    }

    #[test]
    fn the_prompt_says_so_when_there_are_no_notes_yet() {
        let p = goal(10).next_prompt().expect("prompt");
        assert!(p.contains("(none yet)"), "{p}");
        assert!(!p.contains("earlier note"), "nothing to omit: {p}");
    }

    #[test]
    fn a_full_note_list_still_fits_the_prompt_budget() {
        // The worst case the contract names: 24 notes of full length. They cannot
        // all fit in 1200 characters, so the prompt keeps the newest ones and says
        // how many it dropped.
        let mut g = goal(100);
        for i in 0..NOTE_CAP * 2 {
            g.observe(&format!("step {i} {}", "x".repeat(300)), 1_000 + i as u64);
        }
        assert_eq!(g.notes.len(), NOTE_CAP, "the ring keeps the cap");
        assert_eq!(g.notes[0].chars().count(), NOTE_MAX_CHARS, "and truncates each note");

        let p = g.next_prompt().expect("prompt");
        assert!(
            p.chars().count() <= PROMPT_MAX_CHARS,
            "prompt is {} chars, budget is {PROMPT_MAX_CHARS}",
            p.chars().count()
        );
        assert!(p.contains("earlier note(s) omitted"), "the omission is stated: {p}");
        assert!(p.contains("water the plants"));
        assert!(p.contains("GOAL MET") && p.contains("GOAL BLOCKED"));
        // The newest notes are the ones that survive; the oldest are the ones cut.
        assert!(p.contains(&format!("{NOTE_CAP}. step {}", NOTE_CAP * 2 - 1)), "{p}");
        assert!(!p.contains("1. step 0 "), "the oldest note is the first to go: {p}");
    }

    #[test]
    fn a_long_objective_is_quoted_within_budget() {
        let long = "y".repeat(PROMPT_OBJECTIVE_CHARS * 3);
        let g = Goal::new(&long, 5, 0).expect("goal");
        let p = g.next_prompt().expect("prompt");
        assert!(p.chars().count() <= PROMPT_MAX_CHARS);
        // Stored in full for the portal, quoted short in the prompt.
        assert_eq!(g.objective.chars().count(), PROMPT_OBJECTIVE_CHARS * 3);
    }

    // ----------------------------------------------------------- classify

    #[test]
    fn classify_is_case_insensitive_and_reads_the_whole_reply() {
        assert_eq!(classify("GOAL MET"), Reply::Met);
        assert_eq!(classify("done here: goal met."), Reply::Met);
        assert_eq!(classify("Goal met - the lamp is on"), Reply::Met);

        assert_eq!(classify("GOAL BLOCKED"), Reply::Blocked);
        assert_eq!(classify("i cannot: goal blocked, no sensor"), Reply::Blocked);

        assert_eq!(classify("filled the can"), Reply::Continue);
        assert_eq!(classify(""), Reply::Continue);
        assert_eq!(classify("goal me"), Reply::Continue, "a near miss is not a marker");
        assert_eq!(classify("MET"), Reply::Continue);

        // Both markers: stopping is the safe reading.
        assert_eq!(classify("GOAL MET but GOAL BLOCKED"), Reply::Met);
    }

    // ------------------------------------------------------------ observe

    #[test]
    fn observe_continue_advances_and_stops_at_the_cap() {
        let mut g = goal(3);

        assert_eq!(g.observe("step one", 2_000), GoalStatus::Running);
        assert_eq!((g.iteration, g.failures), (1, 0));
        assert_eq!(g.last_result, "step one");
        assert_eq!(g.updated_ms, 2_000);

        assert_eq!(g.observe("step two", 3_000), GoalStatus::Running);
        assert_eq!(g.observe("step three", 4_000), GoalStatus::Exhausted);
        assert_eq!(g.iteration, 3);
        assert_eq!(g.notes.len(), 3);
        assert!(g.stop_reason.contains('3'), "the reason names the cap: {}", g.stop_reason);
        assert!(g.next_prompt().is_none());
    }

    #[test]
    fn observe_met_ends_the_goal_early() {
        let mut g = goal(10);
        assert_eq!(g.observe("watered them\nGOAL MET", 7_000), GoalStatus::Met);
        assert_eq!(g.iteration, 1, "one iteration was enough");
        assert_eq!(g.last_result, "watered them", "the note is the first line");
        assert_eq!(g.updated_ms, 7_000);
        assert!(g.next_prompt().is_none());
    }

    #[test]
    fn observe_blocked_ends_the_goal_and_keeps_the_claim() {
        let mut g = goal(10);
        assert_eq!(g.observe("no tap in reach\nGOAL BLOCKED", 8_000), GoalStatus::Blocked);
        assert_eq!(g.status, GoalStatus::Blocked);
        assert!(g.stop_reason.contains("no tap in reach"));
        assert!(g.next_prompt().is_none());
    }

    #[test]
    fn a_successful_turn_resets_the_failure_count() {
        let mut g = goal(10);
        g.fail_iteration("timeout", 1);
        g.fail_iteration("timeout", 2);
        assert_eq!(g.failures, 2);
        assert_eq!(g.observe("recovered", 3), GoalStatus::Running);
        assert_eq!(g.failures, 0, "a completed turn clears the streak");
        assert_eq!(g.iteration, 3, "failures and successes share the budget");
    }

    // ----------------------------------------------------- fail_iteration

    #[test]
    fn three_consecutive_failures_block_the_goal() {
        let mut g = goal(50);
        assert_eq!(g.fail_iteration("model call failed", 10), GoalStatus::Running);
        assert_eq!(g.fail_iteration("model call failed", 11), GoalStatus::Running);
        assert_eq!(g.failures, 2);
        assert_eq!(g.fail_iteration("model call failed", 12), GoalStatus::Blocked);
        assert_eq!(g.failures, MAX_CONSECUTIVE_FAILURES);
        assert_eq!(g.iteration, 3, "a failed iteration still consumes budget");
        assert!(g.stop_reason.contains('3'), "the reason names the count: {}", g.stop_reason);
        assert!(g.next_prompt().is_none());
        assert_eq!(g.notes.len(), 3);
        assert!(g.notes.iter().all(|n| n.starts_with("failed: ")), "{:?}", g.notes);
        assert_eq!(g.last_result, "failed: model call failed");
    }

    #[test]
    fn failures_run_out_of_budget() {
        let mut g = goal(2);
        assert_eq!(g.fail_iteration("boom", 1), GoalStatus::Running);
        assert_eq!(g.fail_iteration("boom", 2), GoalStatus::Exhausted);
        assert_eq!(g.failures, 2, "below the abort threshold, so the cap decides");
    }

    #[test]
    fn an_empty_error_still_leaves_a_readable_note() {
        let mut g = goal(10);
        g.fail_iteration("   \n", 1);
        assert_eq!(g.notes[0], "failed: no error text");
    }

    // --------------------------------------------------------------- stop

    #[test]
    fn stop_freezes_the_goal_for_good() {
        let mut g = goal(10);
        g.observe("step", 1);
        g.stop("  operator pressed stop  ", 2);
        assert_eq!(g.status, GoalStatus::Stopped);
        assert_eq!(g.stop_reason, "operator pressed stop");
        assert_eq!(g.updated_ms, 2);
        assert!(g.next_prompt().is_none(), "a stopped goal never resumes");

        // Calling stop again is harmless, and an empty reason is still a reason.
        g.stop("", 3);
        assert_eq!(g.status, GoalStatus::Stopped);
        assert_eq!(g.stop_reason, "stopped");
    }

    // -------------------------------------------------------------- notes

    #[test]
    fn notes_keep_only_the_newest_cap() {
        let mut g = goal(100);
        for i in 0..NOTE_CAP + 5 {
            g.observe(&format!("step {i}"), i as u64);
        }
        assert_eq!(g.notes.len(), NOTE_CAP);
        assert_eq!(g.notes[0], "step 5", "the oldest five were dropped");
        assert_eq!(g.notes[NOTE_CAP - 1], format!("step {}", NOTE_CAP + 4));
    }

    #[test]
    fn a_long_reply_is_cut_on_a_char_boundary() {
        let mut g = goal(10);
        // Two-byte characters: cutting on a byte index would panic or produce
        // mojibake, which is exactly what a note must never be.
        let reply = "é".repeat(NOTE_MAX_CHARS * 2);
        g.observe(&reply, 1);
        assert_eq!(g.notes[0].chars().count(), NOTE_MAX_CHARS);
        assert!(g.notes[0].chars().all(|c| c == 'é'), "no broken codepoint");
        assert_eq!(g.last_result.chars().count(), NOTE_MAX_CHARS);
    }

    #[test]
    fn only_the_first_non_empty_line_becomes_the_note() {
        let mut g = goal(10);
        g.observe("\n\n   measured the soil  \nand wrote it down", 1);
        assert_eq!(g.notes[0], "measured the soil");
        assert_eq!(g.last_result, "measured the soil");

        // A reply with nothing in it must still leave one readable note.
        let mut g = goal(10);
        g.observe("", 1);
        assert_eq!(g.notes[0], "(empty reply)");
    }

    // -------------------------------------------------------- json / nvs

    #[test]
    fn every_field_survives_the_round_trip() {
        let mut g = goal(7);
        g.observe("checked the soil", 100);
        g.fail_iteration("upload failed", 200);

        let back = Goal::from_json(&g.to_json()).expect("its own output parses");
        assert_eq!(back.to_json(), g.to_json(), "round trip is lossless");
        assert_eq!(back.status, GoalStatus::Running);
        assert_eq!(back.notes, g.notes);
        assert_eq!(back.iteration, 2);
        assert_eq!(back.failures, 1);
        assert_eq!(back.max_iterations, 7);
        assert_eq!(back.updated_ms, 200);
        assert!(back.next_prompt().is_some());

        // Through an actual JSON string, the way NVS holds it.
        let text = serde_json::to_string(&g.to_json()).expect("serialise");
        let parsed: Value = serde_json::from_str(&text).expect("parse");
        assert_eq!(Goal::from_json(&parsed).expect("goal").to_json(), g.to_json());
    }

    #[test]
    fn a_corrupt_document_is_refused_rather_than_guessed() {
        assert!(Goal::from_json(&json!({})).is_none(), "no objective");
        assert!(Goal::from_json(&json!({"objective": "   "})).is_none(), "blank objective");
        assert!(Goal::from_json(&json!({"objective": 7})).is_none(), "objective is not a string");
        assert!(
            Goal::from_json(&json!({"objective": "x", "status": "halfway"})).is_none(),
            "an unknown status word"
        );
        assert!(
            Goal::from_json(&json!({"objective": "x", "status": 3})).is_none(),
            "a status that is not a string"
        );
    }

    #[test]
    fn a_document_that_lost_keys_still_loads() {
        let g = Goal::from_json(&json!({"objective": "x"})).expect("minimal document");
        assert_eq!(g.status, GoalStatus::Idle, "a goal without a status is idle");
        assert_eq!(g.max_iterations, DEFAULT_MAX_ITERATIONS);
        assert_eq!(g.iteration, 0);
        assert_eq!(g.failures, 0);
        assert!(g.notes.is_empty());
        assert_eq!(g.last_result, "");
        assert_eq!(g.updated_ms, 0);
        assert!(g.next_prompt().is_none(), "an idle goal does not run");
    }

    #[test]
    fn loading_clamps_what_the_document_claims() {
        // A stored budget out of range, numeric strings, unknown keys, junk notes.
        let v = json!({
            "objective": "x",
            "status": "RUNNING",
            "max_iterations": 4_000,
            "iteration": "3",
            "failures": 2.0,
            "notes": ["a", 7, "b"],
            "last_result": "z",
            "stop_reason": "",
            "updated_ms": 42.0,
            "something_new": true,
        });
        let g = Goal::from_json(&v).expect("tolerant load");
        assert_eq!(g.max_iterations, MAX_MAX_ITERATIONS, "clamped on load");
        assert_eq!(g.iteration, 3, "a numeric string is still a number");
        assert_eq!(g.failures, 2, "a float is still a count");
        assert_eq!(g.notes, vec!["a".to_string(), "b".to_string()], "non-strings are skipped");
        assert_eq!(g.updated_ms, 42);
        assert_eq!(g.status, GoalStatus::Running, "the status word is case-insensitive");

        let zero = Goal::from_json(&json!({"objective": "x", "max_iterations": 0})).expect("load");
        assert_eq!(zero.max_iterations, 1, "a zero budget is raised to one");
    }

    #[test]
    fn a_stored_note_list_is_cut_down_to_the_cap() {
        let notes: Vec<String> = (0..NOTE_CAP + 10).map(|i| format!("n{i}")).collect();
        let g = Goal::from_json(&json!({"objective": "x", "notes": notes})).expect("load");
        assert_eq!(g.notes.len(), NOTE_CAP);
        assert_eq!(g.notes[0], "n10", "the newest survive");
        assert_eq!(g.notes[NOTE_CAP - 1], format!("n{}", NOTE_CAP + 9));
    }

    #[test]
    fn the_wire_status_words_are_the_documented_ones() {
        for (status, word) in [
            (GoalStatus::Idle, "idle"),
            (GoalStatus::Running, "running"),
            (GoalStatus::Met, "met"),
            (GoalStatus::Blocked, "blocked"),
            (GoalStatus::Exhausted, "exhausted"),
            (GoalStatus::Stopped, "stopped"),
        ] {
            assert_eq!(status.as_str(), word);
            assert_eq!(GoalStatus::parse(word), Some(status));
        }
        assert_eq!(GoalStatus::parse("nope"), None);
    }
}
