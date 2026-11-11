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
        // No external CDN or font: the board must work on a captive-portal LAN. Checked
        // on the attributes that fetch rather than on the bare scheme, because the agent
        // panel's hint spells out `http://` and `https://` as instructions to the user.
        let lower = INDEX_HTML.to_ascii_lowercase();
        for needle in ["=\"http://", "='http://", "=\"https://", "='https://", "url(http"] {
            assert!(!lower.contains(needle), "the portal fetches something external: {needle}");
        }
        assert!(!STYLE_CSS.to_ascii_lowercase().contains("url(http"), "an external font or image in CSS");
    }

    #[test]
    fn the_portal_offers_the_manifest_workflow() {
        for needle in ["manifest-validate", "manifest-apply", "devices-probe", "wifi-connect"] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
    }

    /// The console ships in English only. One pasted Chinese label would reach the bench
    /// as text nobody there reads, and a paste is all it takes, so the absence is pinned
    /// here instead of being remembered.
    #[test]
    fn the_portal_is_english_only() {
        for (name, text) in [("index.html", INDEX_HTML), ("app.js", APP_JS), ("style.css", STYLE_CSS)] {
            if let Some((offset, ch)) = text.char_indices().find(|(_, c)| is_cjk(*c)) {
                let line = text[..offset].lines().count() + 1;
                panic!("{name}:{line} contains the CJK character {ch:?}");
            }
        }
        // A language switch left behind would have nothing to switch to.
        assert!(!INDEX_HTML.contains("data-i18n"), "index.html still carries i18n hooks");
        assert!(!APP_JS.contains("I18N"), "app.js still carries an I18N table");
        assert!(INDEX_HTML.contains("<html lang=\"en\">"), "the document language must be English");
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
        for key in ["goal_objective", "goal_notes", "goal_started", "goal_status_stopped"] {
            assert!(APP_JS.contains(&format!("{key}:")), "{key} must have a label in app.js");
        }
        // The card's own explanation: a goal ends on these words, so they are worth
        // keeping in front of whoever writes an objective.
        assert!(INDEX_HTML.contains("GOAL MET") && INDEX_HTML.contains("GOAL BLOCKED"));
    }

    /// CJK punctuation and fullwidth forms are rejected along with the ideographs: a
    /// Chinese full stop in an otherwise English sentence is the same problem.
    fn is_cjk(c: char) -> bool {
        matches!(c, '\u{3000}'..='\u{303f}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
    }
}
