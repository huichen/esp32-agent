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
        // The page still talks to the board for the things a person does by hand.
        assert!(APP_JS.contains("/api/devices") && APP_JS.contains("/api/wifi/connect"));
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
    fn the_portal_puts_the_controls_where_the_data_is() {
        // Spending a device probe is a devices-page action; the button lives there.
        for needle in ["devices-probe", "devices-refresh", "wifi-connect"] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        // A device or a tool row expands into its own JSON: `<details>` per row, one
        // renderer each, and no truncation in the markup.
        assert!(APP_JS.contains("item-detail"), "rows must be expandable");
        assert!(APP_JS.contains("JSON.stringify(dev, null, 2)"), "a device row shows its detail");
        assert!(
            APP_JS.contains("JSON.stringify(t, null, 2)") || APP_JS.contains("requires: t.requires"),
            "a tool row shows its declaration"
        );
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

    /// Six tabs, and only six: the conversation is the page, so everything else has to
    /// earn a slot. Order is by how often a person needs it.
    #[test]
    fn the_board_sheet_stays_small_and_ordered() {
        let tabs: Vec<&str> =
            INDEX_HTML.split("data-panel=\"").skip(1).filter_map(|s| s.split('"').next()).collect();
        assert_eq!(
            tabs,
            vec!["devices", "tools", "agent", "schedule", "logs", "settings"],
            "the sheet is ordered by importance and nothing else is in it"
        );
        assert!(!INDEX_HTML.contains("manifest-"), "the manifest editor moved to the agent's job");
    }

    /// Tool calls collapse into one line and expand on tap, like the rest of the chat.
    #[test]
    fn a_tool_call_is_a_collapsed_row() {
        assert!(APP_JS.contains("<details") || APP_JS.contains("document.createElement(\"details\")"));
        assert!(APP_JS.contains("tool_result") && APP_JS.contains("result: "));
        assert!(STYLE_CSS.contains("details.tool") || STYLE_CSS.contains(".tool"), "the row needs a style");
    }

    /// CJK punctuation and fullwidth forms are rejected along with the ideographs: a
    /// Chinese full stop in an otherwise English sentence is the same problem.
    fn is_cjk(c: char) -> bool {
        matches!(c, '\u{3000}'..='\u{303f}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
    }
}
