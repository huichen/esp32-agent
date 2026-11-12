//! Rectangles, not strings: the composer has to fit the screen it is shown on.
//!
//! The bug this locks down was invisible to a source-level assertion, which is why the
//! assertion it replaces never caught it: `.attach-chip { display: flex }` outranked the
//! UA `[hidden] { display: none }`, so the chip stayed a 366 px flex item while it was
//! `hidden`, claimed the composer's single (nowrap) line, and pushed the field and the
//! send button past the right edge. Measured on a 390 px phone: the field began at x=428
//! and the send button at x=466. Every rule was written as intended; only the boxes were
//! wrong. So this test renders the shipped page in a headless browser, at the widths a
//! phone reports, and asserts on `getBoundingClientRect()`.
//!
//! It needs a Chromium-family browser. `ESP32_AGENT_BROWSER` points at one that is not in
//! a standard place (see `browser()`).

use esp32_agent_core::web::{APP_JS, INDEX_HTML, STYLE_CSS};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;

/// The widths a phone actually reports, in CSS pixels. 390 is an iPhone 14/15, 360 a
/// common Android. The composer is the bottom bar: it either fits inside these or the page
/// cannot be typed into.
const PHONE_WIDTHS: &[u32] = &[390, 360];
const PHONE_HEIGHT: u32 = 844;

/// Served into the shipped page after `app.js`. It measures three states — nothing
/// attached, an attachment chip shown, and the chip removed again — because each one puts
/// a different set of boxes in the composer.
///
/// The numbers come back through an attribute on the dumped DOM, percent-encoded so the
/// HTML serialisation cannot touch a single character of the JSON.
const MEASURE_JS: &str = r##"
<script>
window.__esp32Measure = function () {
  var q = function (s) { return document.querySelector(s); };
  var r = function (el) {
    var b = el.getBoundingClientRect();
    return { left: b.left, right: b.right, top: b.top, bottom: b.bottom, width: b.width, height: b.height };
  };
  var snap = function () { return { chip: r(q("#attach-chip")), input: r(q("#input")), send: r(q("#send")) }; };
  var attach = function (a) { S.attach = a; showAttach(); };
  var out = { width: innerWidth, height: innerHeight };
  out.empty = snap();
  attach({ name: "IMG_20261112_1942.jpg", mime: "image/jpeg", data_b64: "", kb: 21 });
  out.attached = snap();
  attach(null);
  out.restored = snap();
  return out;
};
</script>
"##;

/// The page under measurement, served so that `app.js` and the stylesheet are the shipped
/// files and the origin is a normal one (`localStorage`, `fetch` and the module-scope
/// helpers behind `showAttach` all behave as they do on the board).
fn page() -> String {
    INDEX_HTML.replace("</body>", &format!("{MEASURE_JS}</body>"))
}

/// The harness: one iframe per phone width, because an iframe *is* the viewport — that is
/// the only way to get a fixed-position composer measured at 390 px without depending on
/// how a browser treats `--window-size`. Measurement runs on `load`, after the page's own
/// `DOMContentLoaded` handler has wired the composer up.
fn harness() -> String {
    let widths = PHONE_WIDTHS.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>phone widths</title></head>
<body style="margin:0;background:#000">
<script>
const widths = [{widths}];
const frames = widths.map((w) => {{
  const f = document.createElement("iframe");
  f.style.cssText = "display:block;border:0;width:" + w + "px;height:{height}px";
  f.src = "/page";
  document.body.appendChild(f);
  return {{ w, f }};
}});
function measure() {{
  const out = frames.map(({{ w, f }}) => ({{
    requested: w,
    metrics: f.contentWindow && f.contentWindow.__esp32Measure ? f.contentWindow.__esp32Measure() : null,
  }}));
  document.body.dataset.esp32Layout = encodeURIComponent(JSON.stringify(out));
}}
// The dump happens at the end of the virtual time budget; `load` normally lands first, and
// the timer is the same measurement again in case it did not.
window.addEventListener("load", measure);
setTimeout(measure, 300);
</script>
</body></html>
"#,
        height = PHONE_HEIGHT,
    )
}

/// The three files the board would serve, on a loopback port. The test does not need the
/// rest of the API: `/api/*` answers 404 and the page's own error handling is what it does
/// on a board whose agent is not configured.
fn serve() -> String {
    let routes: Vec<(String, String)> = vec![
        ("/harness".into(), harness()),
        ("/page".into(), page()),
        ("/assets/style.css".into(), STYLE_CSS.to_string()),
        ("/assets/app.js".into(), APP_JS.to_string()),
    ];
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port for the harness");
    let address = listener.local_addr().expect("the harness address");
    let routes = Arc::new(routes);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let routes = Arc::clone(&routes);
            // One thread per request: the page asks for the stylesheet and the script at
            // the same time, and a single-threaded accept loop deadlocks a browser.
            thread::spawn(move || {
                let _ = answer(&mut stream, &routes);
            });
        }
    });
    format!("http://{address}")
}

fn answer(stream: &mut TcpStream, routes: &[(String, String)]) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    let read = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();
    let body = routes.iter().find(|(route, _)| *route == path).map(|(_, body)| body);
    let (status, kind, body) = match body {
        Some(body) => ("200 OK", content_type(&path), body.as_str()),
        None => ("404 Not Found", "text/plain", "not found"),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else {
        "text/html"
    }
}

/// Chrome, Edge or Chromium. `ESP32_AGENT_BROWSER` wins, so a CI image can point at the one
/// it installed; otherwise the standard locations, then `PATH`.
fn browser() -> PathBuf {
    if let Some(exe) = std::env::var_os("ESP32_AGENT_BROWSER") {
        return PathBuf::from(exe);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(dir) = std::env::var_os(base) {
                let dir = PathBuf::from(dir);
                candidates.push(dir.join("Google/Chrome/Application/chrome.exe"));
                candidates.push(dir.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        candidates.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
        candidates.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    } else {
        candidates.extend(
            ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge"]
                .into_iter()
                .filter_map(in_path),
        );
    }
    candidates.into_iter().find(|path| path.exists()).unwrap_or_else(|| {
        panic!(
            "this test measures the page in a browser and found none; install Chrome/Chromium \
             or set ESP32_AGENT_BROWSER to the executable"
        )
    })
}

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.exists())
}

/// Render the harness and hand back the dumped DOM. A unique profile per run matters: with
/// the default one Chrome hands the request to a browser that is already running and prints
/// nothing at all.
fn dump_dom(browser: &Path, url: &str) -> String {
    let profile = std::env::temp_dir().join(format!("esp32-agent-layout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let out = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-sync",
            // A phone-sized iframe inside a wide window: the window size is irrelevant.
            "--window-size=1200,1000",
            "--virtual-time-budget=3000",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--dump-dom")
        .arg(url)
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", browser.display()));
    let _ = std::fs::remove_dir_all(&profile);
    let dom = String::from_utf8_lossy(&out.stdout).into_owned();
    if !dom.contains("esp32Layout") {
        panic!(
            "{} printed no measurement (status {}): {}\nstderr: {}",
            browser.display(),
            out.status,
            url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    dom
}

/// `data-esp32-layout="<percent-encoded JSON>"` → one measurement per requested width.
fn measurements(dom: &str) -> BTreeMap<u32, Value> {
    let marker = "data-esp32-layout=\"";
    let start = dom.find(marker).unwrap_or_else(|| panic!("the harness left no measurement in:\n{dom}"))
        + marker.len();
    let raw = &dom[start..];
    let encoded = &raw[..raw.find('"').unwrap_or_else(|| panic!("unterminated measurement attribute"))];
    let json = percent_decode(encoded);
    let rows: Vec<Value> =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("the measurement is not JSON ({e}): {json}"));
    rows.into_iter()
        .map(|row| {
            let width = row["requested"].as_u64().unwrap_or(0) as u32;
            let metrics = row["metrics"].clone();
            assert!(
                !metrics.is_null(),
                "the page did not expose __esp32Measure at {width} px: app.js did not run"
            );
            (width, metrics)
        })
        .collect()
}

fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    width: f64,
    height: f64,
}

fn rect(metrics: &Value, state: &str, part: &str) -> Rect {
    let at = &metrics[state][part];
    let num = |key: &str| at[key].as_f64().unwrap_or(f64::NAN);
    Rect {
        left: num("left"),
        right: num("right"),
        top: num("top"),
        bottom: num("bottom"),
        width: num("width"),
        height: num("height"),
    }
}

#[test]
fn the_composer_fits_a_phone_screen() {
    let browser = browser();
    let url = format!("{}/harness", serve());
    let dom = dump_dom(&browser, &url);
    let measured = measurements(&dom);

    for width in PHONE_WIDTHS {
        let viewport = *width as f64;
        let metrics = measured.get(width).unwrap_or_else(|| {
            panic!("no measurement for a {width} px viewport; the harness returned {measured:?}")
        });
        let reported = metrics["width"].as_f64().unwrap_or(f64::NAN);
        assert!(
            (reported - viewport).abs() < 1.0,
            "the page measured {reported} px wide, not {width}: the harness did not get a phone viewport"
        );

        // A failure should not send anyone back to the bench for the numbers: the whole
        // measurement is printed, and libtest shows it when the assertion trips.
        println!("{width} px viewport (the page reports {reported} px)");
        for state in ["empty", "attached", "restored"] {
            let c = rect(metrics, state, "chip");
            let i = rect(metrics, state, "input");
            let s = rect(metrics, state, "send");
            println!(
                "  {state:8} chip {:6.1}..{:6.1} top {:6.1} w {:5.1} h {:4.1} | \
                 field {:6.1}..{:6.1} w {:5.1} | send {:6.1}..{:6.1}",
                c.left, c.right, c.top, c.width, c.height, i.left, i.right, i.width, s.left, s.right
            );
        }

        for state in ["empty", "attached", "restored"] {
            let chip = rect(metrics, state, "chip");
            let input = rect(metrics, state, "input");
            let send = rect(metrics, state, "send");

            if state == "attached" {
                // The chip is the reason the composer wraps: on its own line, above the
                // field, inside the screen. Without `flex-wrap` it shares the one line and
                // pushes everything out again.
                assert!(
                    chip.width > 0.0 && chip.height > 0.0,
                    "{width} px, chip attached: the chip has no box ({chip:?}), so this state did not happen"
                );
                assert!(
                    chip.left >= 0.0 && chip.right <= viewport + 0.01,
                    "{width} px, chip attached: the chip spans {:.1}..{:.1}, outside the screen",
                    chip.left,
                    chip.right
                );
                assert!(
                    chip.bottom <= input.top + 0.5,
                    "{width} px, chip attached: the chip ends at y={:.1} and the field starts at y={:.1} — \
                     they are on one line, so the composer is not wrapping",
                    chip.bottom,
                    input.top
                );
            } else {
                // `hidden` has to mean no box at all. A hidden element that keeps its
                // `display` keeps its size, and a 366 px one is what shoved the field out.
                assert!(
                    chip.width == 0.0 && chip.height == 0.0,
                    "{width} px, chip {state}: a hidden chip still occupies {:.1}x{:.1} at x={:.1}",
                    chip.width,
                    chip.height,
                    chip.left
                );
            }

            assert!(
                input.left >= 0.0 && input.right <= viewport + 0.01,
                "{width} px, chip {state}: the field spans {:.1}..{:.1}, outside the screen",
                input.left,
                input.right
            );
            assert!(
                input.width >= 150.0,
                "{width} px, chip {state}: the field is only {:.1} px wide — not a field to type into",
                input.width
            );
            assert!(
                send.left >= 0.0 && send.right <= viewport + 0.01,
                "{width} px, chip {state}: the send button spans {:.1}..{:.1}, outside the screen",
                send.left,
                send.right
            );
            assert!(
                send.width >= 30.0,
                "{width} px, chip {state}: the send button is only {:.1} px wide",
                send.width
            );
        }
    }
}
