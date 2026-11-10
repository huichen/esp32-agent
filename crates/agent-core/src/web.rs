//! The HTML console, compiled into the firmware.
//!
//! Three files, no build step: the board serves them exactly as they are, so an
//! edit here is visible after a `cargo build` and nothing else.

pub const INDEX_HTML: &str = include_str!("web/index.html");
pub const APP_JS: &str = include_str!("web/app.js");
pub const STYLE_CSS: &str = include_str!("web/style.css");

/// The files the portal consists of, for the `/api/status` "where is the UI" line.
pub const ASSETS: &[(&str, &str)] =
    &[("/", INDEX_HTML), ("/assets/app.js", APP_JS), ("/assets/style.css", STYLE_CSS)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_is_self_contained() {
        assert!(INDEX_HTML.contains("/assets/app.js"));
        assert!(INDEX_HTML.contains("/assets/style.css"));
        assert!(APP_JS.contains("/api/manifest"));
        assert!(APP_JS.contains("/api/wifi/connect"));
        assert!(STYLE_CSS.contains("--accent"));
        // No external CDN or font: the board must work on a captive-portal LAN.
        assert!(!INDEX_HTML.contains("http://"));
        assert!(!INDEX_HTML.contains("https://"));
    }

    #[test]
    fn the_portal_offers_the_manifest_workflow() {
        for needle in ["manifest-validate", "manifest-apply", "devices-probe", "wifi-connect"] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
    }

    /// The console is bilingual, and a key present in one language only shows up as the
    /// raw key text to whoever is reading in the other one. Nothing enforced that parity
    /// before: it was checked by hand once, which is not a check.
    #[test]
    fn both_languages_define_the_same_keys() {
        let zh = i18n_block(APP_JS, "zh");
        let en = i18n_block(APP_JS, "en");
        assert!(!zh.is_empty(), "no zh table in app.js");
        assert!(!en.is_empty(), "no en table in app.js");

        let missing_in_en: Vec<&String> = zh.difference(&en).collect();
        let missing_in_zh: Vec<&String> = en.difference(&zh).collect();
        assert!(missing_in_en.is_empty(), "keys with no english translation: {missing_in_en:?}");
        assert!(missing_in_zh.is_empty(), "keys with no chinese translation: {missing_in_zh:?}");
        assert!(zh.len() > 50, "the table shrank to {} keys", zh.len());
    }

    /// The goal card is the only way to start a goal from a phone, so its wiring is worth
    /// pinning: markup in the HTML, calls in the JS.
    #[test]
    fn the_portal_can_start_and_stop_a_goal() {
        for needle in ["goal-objective", "goal-max-iterations", "goal-start", "goal-stop", "goal-notes"] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        assert!(APP_JS.contains("/api/goal"));
        assert!(APP_JS.contains("/api/goal/stop"));
        for key in ["goal_title", "goal_start", "goal_stop", "goal_notes"] {
            assert!(
                i18n_block(APP_JS, "zh").contains(key) && i18n_block(APP_JS, "en").contains(key),
                "{key} must be translated in both languages"
            );
        }
    }

    /// The keys of one language table in `app.js`, found by brace matching so a nested
    /// object or a brace inside a string cannot truncate the block.
    fn i18n_block(js: &str, lang: &str) -> std::collections::BTreeSet<String> {
        let marker = format!("{lang}: {{");
        let start = match js.find(&marker) {
            Some(i) => i + marker.len() - 1,
            None => return Default::default(),
        };
        let body = &js[start..];
        let mut depth = 0usize;
        let mut in_str: Option<char> = None;
        let mut end = body.len();
        let mut keys = std::collections::BTreeSet::new();
        let mut entry_start = false;
        let chars: Vec<char> = body.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            match in_str {
                Some(q) => {
                    if c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == q {
                        in_str = None;
                    }
                }
                None => match c {
                    '"' | '\'' => in_str = Some(c),
                    '{' => {
                        depth += 1;
                        entry_start = true;
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + 1;
                            break;
                        }
                    }
                    ',' => entry_start = true,
                    c if c.is_alphanumeric() || c == '_' => {
                        if entry_start && depth == 1 {
                            let mut j = i;
                            let mut name = String::new();
                            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                                name.push(chars[j]);
                                j += 1;
                            }
                            while j < chars.len() && chars[j].is_whitespace() {
                                j += 1;
                            }
                            if j < chars.len() && chars[j] == ':' {
                                keys.insert(name);
                            }
                            i = j;
                            continue;
                        }
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        let _ = (body, end);
        keys
    }
}
