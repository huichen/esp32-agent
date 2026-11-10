//! Panel geometry is configuration, not a constant.
//!
//! The bench board carries a 128x32 panel. With the height hard-coded at 64 the
//! driver writes into the second half of a frame the panel does not have, and the
//! lower lines are simply invisible - the model sees "ok" and nothing appears on
//! screen. These tests pin the clipping and the config-to-driver path, so a future
//! driver that assumes 64 fails here instead of on somebody's desk.

use esp32_agent_core::drivers::ssd1306::{Controller, Ssd1306};
use esp32_agent_core::virtual_board::VirtualBoard;

#[test]
fn a_128x32_panel_never_receives_text_below_row_32() {
    let mut hw = VirtualBoard::new();
    let mut panel = Ssd1306::probe(&mut hw, 0, 0x3C, 32, Controller::Ssd1306, 1).expect("panel");
    assert_eq!(panel.fb_len(), 128 * 4, "128x32 means four pages");

    panel.clear();
    panel.text(0, 40, "LINE5", true);
    assert!(
        panel.framebuffer().iter().all(|b| *b == 0),
        "text below row 32 must be dropped, not written past the panel"
    );

    panel.text(0, 24, "LINE3", true);
    assert!(panel.framebuffer().iter().any(|b| *b != 0), "text inside the panel must land");
}

#[test]
fn a_128x64_panel_uses_the_full_frame() {
    let mut hw = VirtualBoard::new();
    let mut panel = Ssd1306::probe(&mut hw, 0, 0x3C, 64, Controller::Ssd1306, 1).expect("panel");
    assert_eq!(panel.fb_len(), 128 * 8);
    panel.clear();
    panel.text(0, 56, "LINE7", true);
    assert!(panel.framebuffer().iter().any(|b| *b != 0));
}

#[test]
fn the_configured_height_reaches_the_driver() {
    // `pins["oled.height"]` is what the portal writes; the detector must pass it on.
    use esp32_agent_core::boards;
    use esp32_agent_core::config::Config;
    use esp32_agent_core::drivers;
    use esp32_agent_core::logbuf::LogBuf;

    let mut cfg = Config::default();
    let profile = boards::find("esp32s3-xiaozhi-bread").expect("profile");
    cfg.pins.insert("oled.height".into(), 32);
    let mut hw = VirtualBoard::new();
    let log = LogBuf::new(16);
    let res = drivers::detect(&mut hw, &cfg, profile, &log);
    let oled = res.devices.infos().into_iter().find(|d| d.kind == "ssd1306").expect("OLED detected");
    assert_eq!(oled.detail["height"], 32, "detail must report the real geometry");
    assert_eq!(oled.detail["framebuffer_bytes"], 128 * 4, "a 32-row panel has four pages");
}
