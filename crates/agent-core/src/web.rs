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
        // Clearing is not a restart either. The console has one restart of its own — the
        // Settings card, owner-only, `POST /api/reboot` — and it must stay the only one: this
        // button, the network switches and the clear button all leave the board running.
        assert_eq!(APP_JS.matches("/api/reboot").count(), 1, "exactly one restart in the page");
        assert!(INDEX_HTML.contains("id=\"board-restart\""), "the restart lives in Settings");
        assert!(!INDEX_HTML.contains("id=\"reboot\""), "and not in the header, next to Clear chat");
        assert!(STYLE_CSS.contains(".clear-chat:focus-visible"), "keyboard focus must be visible");
        assert!(INDEX_HTML.contains("<html lang=\"en\">"), "the page declares one language");
    }

    /// Six tabs, and only six: the conversation is the page, so everything else has to earn
    /// a slot. Order is by how often a person needs it. The schedule sits inside Agent (it
    /// queues turns for that agent) and the log inside Settings (it is the other thing to read
    /// when the board misbehaves), so nine panels never had to exist. The terminal is the sixth
    /// and the exception on purpose: it is the one panel that talks to the board the way an
    /// external agent does (`POST /mcp`), so it is the board's own tool surface with a prompt.
    #[test]
    fn the_board_sheet_stays_small_and_ordered() {
        let tabs: Vec<&str> =
            INDEX_HTML.split("data-panel=\"").skip(1).filter_map(|s| s.split('"').next()).collect();
        assert_eq!(
            tabs,
            vec!["devices", "tools", "agent", "files", "terminal", "settings"],
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

    /// A blank credential field means "keep the stored one", so removing a PIN or a token
    /// needs its own control. Without it the only way to clear one was to speak the API.
    #[test]
    fn a_credential_can_be_cleared_from_the_access_card() {
        assert!(INDEX_HTML.contains("id=\"auth-clear-pin\""), "no way to clear the PIN");
        assert!(INDEX_HTML.contains("id=\"auth-clear-token\""), "no way to clear the MCP token");
        assert!(
            APP_JS.contains("body.auth[field] = \"\""),
            "an explicit empty string is what clears a credential; __set__ keeps it"
        );
        for field in ["portal_pin", "mcp_token"] {
            assert!(APP_JS.contains(&format!("clearCredential(\"{field}\"")), "{field} has no handler");
        }
    }

    /// `tz_offset_h` is written to `/api/config` and edited on the Settings panel, so the
    /// Settings panel fills it from the config it already fetched. It used to be filled by
    /// the Agent panel from the agent document, which never carried it: Settings showed 0 on
    /// every visit while the board held the right value.
    #[test]
    fn the_timezone_is_read_from_the_panel_that_edits_it() {
        let settings = APP_JS
            .split("async function loadSettings")
            .nth(1)
            .and_then(|tail| tail.split("async function").next())
            .expect("loadSettings exists");
        assert!(
            settings.contains("$('#auth-tz')") || settings.contains("$(\"#auth-tz\")"),
            "the Settings panel has to fill the timezone field"
        );
        assert!(settings.contains("tz_offset_h"), "from the config document");
        let agent = APP_JS
            .split("async function loadAgent")
            .nth(1)
            .and_then(|tail| tail.split("async function").next())
            .expect("loadAgent exists");
        assert!(
            !agent.contains("$('#auth-tz').value =") && !agent.contains("$(\"#auth-tz\").value ="),
            "the Agent panel is not where the timezone field is filled"
        );
    }
}
