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

    /// The page speaks one language. A mixed page — a Chinese subtitle under an English
    /// button, a bilingual dialog body — looked unfinished, and on a Chinese phone the
    /// native `confirm()` drew Chinese buttons around the English text.
    #[test]
    fn the_portal_never_shows_chinese() {
        fn first_non_latin(name: &str, text: &str) {
            if let Some((at, ch)) = text.char_indices().find(|(_, c)| is_cjk(*c)) {
                let line = text[..at].matches('\n').count() + 1;
                panic!("{name}:{line} contains `{ch}` — the portal is English only");
            }
        }
        for (name, text) in [("index.html", INDEX_HTML), ("app.js", APP_JS), ("style.css", STYLE_CSS)] {
            first_non_latin(name, text);
        }
    }

    /// CJK ideographs, kana, hangul and the full-width forms a Chinese or Japanese dialog
    /// would bring with it. Punctuation blocks are included on purpose: `。` and `、` are as
    /// out of place in an English page as a character is.
    fn is_cjk(c: char) -> bool {
        matches!(c as u32,
            0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF)
    }

    /// The page decides how large an attachment may be before it is sent, and the board
    /// enforces the same number in Rust. Two copies of a limit is how the page came to send
    /// 26 328 bytes into a 24 576-byte refusal.
    #[test]
    fn the_portal_attachment_limit_is_the_boards_limit() {
        let kilobytes = crate::app::MAX_ATTACHMENT_BYTES / 1024;
        let needle = format!("const ATTACH_BYTES_MAX = {kilobytes} * 1024;");
        assert!(
            APP_JS.contains(&needle),
            "app.js must carry `{needle}`, or the page and MAX_ATTACHMENT_BYTES disagree"
        );
    }

    /// Clearing is available in the conversation itself, in English, with an accessible
    /// status for cancellation and refusal feedback. It is not a settings reset.
    #[test]
    fn conversation_clearing_is_english_only_and_accessible() {
        let header = INDEX_HTML.split("</header>").next().expect("the chat header");
        assert!(header.contains("id=\"clear-chat\""), "clearing must not require a sheet");
        assert!(header.contains("aria-label=\"Clear conversation\""), "the button needs its own name");
        assert!(header.contains(">Clear chat</button>"), "one label, not two");
        assert!(
            INDEX_HTML.contains("id=\"confirm-dialog\""),
            "the confirmation has to be ours: window.confirm() speaks the device language"
        );
        assert!(INDEX_HTML.contains("role=\"status\" aria-live=\"polite\""));
        assert!(APP_JS.contains("changeConversation(\"/api/agent/reset\", {}, true)"));
        assert!(APP_JS.contains("changeConversation(\"/api/agent/forget\", { keep: 1 }, false)"));
        assert!(!APP_JS.contains("confirm(CONVERSATION_TEXT.confirm)"), "the native dialog is gone");
        assert!(APP_JS.contains("await askConfirm(\"Clear conversation\""));
        assert!(APP_JS.contains("Instructions, skills, files and model settings stay."));
        assert!(!APP_JS.contains("/api/factory-reset"), "clearing must not erase the workspace or config");
        assert!(!APP_JS.contains("/api/reboot"), "no button may restart the board");
        assert!(!INDEX_HTML.contains("id=\"reboot\""), "the Reboot button is gone");
        assert!(STYLE_CSS.contains(".clear-chat:focus-visible"), "keyboard focus must be visible");
        assert!(INDEX_HTML.contains("<html lang=\"en\">"), "the page declares one language");
    }

    /// Five tabs, and only five: the conversation is the page, so everything else has to
    /// earn a slot. Order is by how often a person needs it. The schedule sits inside Agent
    /// (it queues turns for that agent) and the log inside Settings (it is the other thing to
    /// read when the board misbehaves), so nine panels never had to exist.
    #[test]
    fn the_board_sheet_stays_small_and_ordered() {
        let tabs: Vec<&str> =
            INDEX_HTML.split("data-panel=\"").skip(1).filter_map(|s| s.split('"').next()).collect();
        assert_eq!(
            tabs,
            vec!["devices", "tools", "agent", "files", "settings"],
            "the sheet is ordered by importance and nothing else is in it"
        );
        assert!(!INDEX_HTML.contains("manifest-"), "the manifest editor moved to the agent's job");

        // Each moved card really is inside the panel it now belongs to: a tab that hides its
        // own contents is the failure this catches, and it is invisible in a unit test that
        // only counts ids.
        let panel = |id: &str| -> &str {
            let start = INDEX_HTML.find(&format!("id=\"panel-{id}\"")).expect("panel");
            let rest = &INDEX_HTML[start..];
            let end = rest.find("<div id=\"panel-").unwrap_or(rest.len());
            &rest[..end]
        };
        assert!(panel("agent").contains("sched-list"), "the schedule belongs to the agent panel");
        assert!(panel("settings").contains("id=\"logs\""), "the log belongs to settings");
        assert!(
            panel("files").contains("files-tree") && panel("files").contains("files-viewer"),
            "the files panel needs the tree and the viewer"
        );
    }

    /// The Files tab reads the workspace as one tree and writes nothing: every write stays
    /// behind the `write` tool, which is where the 8 KB budget and the NVS blob are enforced.
    #[test]
    fn the_files_tab_only_reads() {
        assert!(APP_JS.contains("/api/files/tree"));
        assert!(APP_JS.contains("/api/files/read?path="));
        for writer in ["method: \"POST\"", "method: \"PUT\"", "method: \"DELETE\""] {
            let segment = APP_JS
                .split("async function loadTree")
                .nth(1)
                .and_then(|s| s.split("async function loadLogs").next())
                .expect("the files functions");
            assert!(!segment.contains(writer), "the files tab must not write ({writer})");
        }

        // A tree, not a list: nested directories render as folds, and the CSS says so.
        assert!(APP_JS.contains("tree-dir") && APP_JS.contains("tree-kids"));
        assert!(STYLE_CSS.contains(".tree .tree-kids"), "the nesting needs a style");
    }

    /// Tool calls collapse into one line and expand on tap, like the rest of the chat.
    #[test]
    fn a_tool_call_is_a_collapsed_row() {
        assert!(APP_JS.contains("<details") || APP_JS.contains("document.createElement(\"details\")"));
        assert!(APP_JS.contains("tool_result") && APP_JS.contains("result: "));
        assert!(STYLE_CSS.contains("details.tool") || STYLE_CSS.contains(".tool"), "the row needs a style");
    }
}
