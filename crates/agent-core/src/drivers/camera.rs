//! OV2640 / OV3660 camera: the SCCB control channel, and one frame through DVP.
//!
//! A camera module is two devices in one package, and only one of the two can live in
//! this crate:
//!
//! * the **control** side is SCCB, which is I2C with a different name: the same
//!   `i2c_read` / `i2c_write` on the bus and address a manifest declares. That is enough
//!   to identify the part (PID/VER) and to read a register while sitting at the bench;
//! * the **image** side is an 8-bit parallel DVP bus with DMA into PSRAM. That half
//!   belongs to the platform, and it is already behind [`Hardware::dvp_capture`], which
//!   takes the pins and geometry and returns packed RGB888. Its default implementation
//!   refuses, so a board without a camera says so instead of inventing frames.
//!
//! This module deliberately carries **no sensor register table**. Bringing up an image
//! pipeline means a few hundred tuning registers per module, and every one of them is a
//! value nobody measured here. The platform's capture path owns that; what this driver
//! owns is the identification, the wiring, and turning a frame into something a tool
//! result can carry.
//!
//! ## Why a thumbnail
//!
//! A tool result is cut to [`crate::agent::MAX_TOOL_RESULT_BYTES`] (64 KB). A QVGA frame
//! is 320x240x3 = 230,400 bytes, so even the smallest useful frame does not cross the wire.
//! `camera_capture` returns the geometry, the byte count and a 160x120 greyscale thumbnail:
//! 19,200 bytes, 25,600 characters of base64, about 40% of one result. The thumbnail is an
//! image summary, not the picture — a 1080p frame is 6.2 MB, which is why a real frame
//! belongs on an HTTP route rather than in a tool result.
//!
//! ## Which ids are accepted
//!
//! Exactly two: PID 0x26 is an OV2640 and PID 0x36 is an OV3660. Nothing else is
//! claimed. The VER register (the mask revision) is read and reported, but it is not used
//! to decide anything: the revisions of both parts are many, and a driver that rejected a
//! revision it had not seen would call a working camera broken.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::Hardware;
use crate::json as j;
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// SCCB register holding the product id (PID).
// TODO(verify): 0x0A/0x0B are the datasheet register numbers for both parts; no
// read on hardware backs them in this repository yet.
const REG_PID: u8 = 0x0A;
/// SCCB register holding the mask revision (VER). Reported, never used to decide.
const REG_VER: u8 = 0x0B;
/// System control register 7, whose top bit is the software reset (SRST).
// TODO(verify): 0x12 with bit 7 as SRST is the datasheet's reset path for both parts;
// nothing on this bench has confirmed that a write lands where it should.
const REG_COM7: u8 = 0x12;
const COM7_SRST: u8 = 0x80;
/// How long the part is given after SRST before its SCCB interface is read again.
// TODO(verify): 5 ms is a conservative datasheet-shaped number, not a measured one.
const RESET_SETTLE_US: u32 = 5_000;

/// The only two product ids this driver claims.
pub const PID_OV2640: u8 = 0x26;
pub const PID_OV3660: u8 = 0x36;

/// Thumbnail size returned by `camera_capture`. 19,200 bytes of grey level is 25,600
/// characters of base64 — about 40% of a 64 KB result, which leaves the geometry and the
/// note the other 60%. 32x24 and then 64x48 were the earlier sizes; each move came from a
/// result budget that was bigger than the code assumed.
pub const THUMB_W: u32 = 160;
pub const THUMB_H: u32 = 120;

const DEFAULT_WIDTH: u32 = 320;
const DEFAULT_HEIGHT: u32 = 240;
/// XCLK the capture path is asked for when the manifest does not say.
// TODO(verify): 20 MHz is the customary XCLK for these modules; the sensor's own limit
// and this board's clock divider have not been checked here.
const DEFAULT_XCLK_HZ: u32 = 20_000_000;

/// PID and VER as read back over SCCB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub pid: u8,
    pub ver: u8,
}

impl Ids {
    /// True when this is one of the two parts in the comment at the top of the file.
    pub fn accepted(&self) -> bool {
        matches!(self.pid, PID_OV2640 | PID_OV3660)
    }

    /// `"ov2640"`, `"ov3660"`, or `"unknown"` for an id nobody here has seen.
    pub fn sensor_name(&self) -> &'static str {
        match self.pid {
            PID_OV2640 => "ov2640",
            PID_OV3660 => "ov3660",
            _ => "unknown",
        }
    }

    /// The pair as the vendor documents it: PID then VER, e.g. `0x2642`.
    pub fn sensor_id(&self) -> String {
        format!("0x{:02X}{:02X}", self.pid, self.ver)
    }
}

/// The DVP wiring and geometry, as the manifest writes them under `devices[].options`.
///
/// This is the same `BTreeMap<String, String>` that `instantiate` parses for every other
/// driver's options (`height`, `shunt_mohm`, ...); the camera simply has more of them,
/// because a parallel bus is twelve GPIOs and a bus cannot name its own pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub xclk: i32,
    pub pclk: i32,
    pub vsync: i32,
    pub href: i32,
    /// D0..D7, in that order: D0 is `options.d0`.
    pub data: [i32; 8],
    pub width: u32,
    pub height: u32,
    pub freq_hz: u32,
}

impl Options {
    /// Parse `devices[].options`. The twelve pins are required; the geometry has
    /// defaults (QVGA, 20 MHz) because a wrong default is visible in the first capture's
    /// metadata while a missing pin is not something the driver may invent.
    pub fn from_options(o: &BTreeMap<String, String>) -> Result<Self> {
        Ok(Self {
            xclk: pin_of(o, "xclk")?,
            pclk: pin_of(o, "pclk")?,
            vsync: pin_of(o, "vsync")?,
            href: pin_of(o, "href")?,
            data: [
                pin_of(o, "d0")?,
                pin_of(o, "d1")?,
                pin_of(o, "d2")?,
                pin_of(o, "d3")?,
                pin_of(o, "d4")?,
                pin_of(o, "d5")?,
                pin_of(o, "d6")?,
                pin_of(o, "d7")?,
            ],
            width: dim_of(o, "width", DEFAULT_WIDTH)?,
            height: dim_of(o, "height", DEFAULT_HEIGHT)?,
            freq_hz: hz_of(o, "freq_hz", DEFAULT_XCLK_HZ)?,
        })
    }

    /// Bytes of one packed RGB888 frame at this geometry.
    pub fn frame_bytes(&self) -> usize {
        (self.width as usize) * (self.height as usize) * 3
    }

    /// The value handed to [`Hardware::dvp_capture`].
    ///
    /// One flat object with the pin names the manifest used plus `width`, `height`,
    /// `freq_hz` and `format`, so the platform reads the same shape it was declared in.
    pub fn dvp_request(&self) -> Value {
        json!({
            "xclk": self.xclk,
            "pclk": self.pclk,
            "vsync": self.vsync,
            "href": self.href,
            "d0": self.data[0],
            "d1": self.data[1],
            "d2": self.data[2],
            "d3": self.data[3],
            "d4": self.data[4],
            "d5": self.data[5],
            "d6": self.data[6],
            "d7": self.data[7],
            "width": self.width,
            "height": self.height,
            "freq_hz": self.freq_hz,
            "format": "rgb888",
        })
    }
}

fn pin_of(o: &BTreeMap<String, String>, key: &str) -> Result<i32> {
    let raw = o.get(key).ok_or_else(|| {
        McpError::invalid_params(format!(
            "camera needs `options.{key}`: a DVP bus is twelve GPIOs and the driver will not \
             guess one of them"
        ))
    })?;
    let pin = raw
        .trim()
        .parse::<i32>()
        .map_err(|_| McpError::invalid_params(format!("`options.{key}` = `{raw}` is not a GPIO number")))?;
    Ok(pin)
}

fn dim_of(o: &BTreeMap<String, String>, key: &str, default: u32) -> Result<u32> {
    let Some(raw) = o.get(key) else { return Ok(default) };
    let v = raw
        .trim()
        .parse::<u32>()
        .map_err(|_| McpError::invalid_params(format!("`options.{key}` = `{raw}` is not a number")))?;
    if !(1..=4096).contains(&v) {
        return Err(McpError::invalid_params(format!("`options.{key}` = {v} is outside 1..=4096")));
    }
    Ok(v)
}

/// A clock rate. The ceiling is only a sanity bound: no sensor datasheet limit is
/// enforced here, and the platform rejects a rate its clock divider cannot produce.
fn hz_of(o: &BTreeMap<String, String>, key: &str, default: u32) -> Result<u32> {
    let Some(raw) = o.get(key) else { return Ok(default) };
    let v = raw
        .trim()
        .parse::<u32>()
        .map_err(|_| McpError::invalid_params(format!("`options.{key}` = `{raw}` is not a rate in Hz")))?;
    if !(1..=40_000_000).contains(&v) {
        return Err(McpError::invalid_params(format!("`options.{key}` = {v} is outside 1..=40000000")));
    }
    Ok(v)
}

/// BT.601 luma of one RGB888 pixel, rounded half up. A grey pixel `(v, v, v)` maps to
/// `v` exactly, which is what makes a scripted test frame predictable.
pub fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((299 * r as u32 + 587 * g as u32 + 114 * b as u32 + 500) / 1000) as u8
}

/// The source rows and columns covered by thumbnail cell `i` of `n`.
///
/// Integer arithmetic, no floating point: cell `i` starts at `i * src / n` and ends where
/// the next cell starts, so the cells tile the source exactly however it divides. A cell
/// is never empty, and a source smaller than the thumbnail repeats its last row/column
/// rather than reading out of bounds.
fn block(i: u32, n: u32, src: u32) -> (u32, u32) {
    let start = ((i * src) / n).min(src.saturating_sub(1));
    let end = ((((i + 1) * src) / n).max(start + 1)).min(src);
    (start, end)
}

/// 64x48 greyscale thumbnail of a packed RGB888 frame, as block averages of luma.
///
/// The one job this has is to be predictable: with a 320x240 frame each cell covers
/// exactly 10x10 source pixels, so a frame made of four constant quadrants produces four
/// constant thumbnail quadrants and the arithmetic can be checked by eye.
pub fn grey_thumbnail(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let stride = width as usize * 3;
    let mut out = Vec::with_capacity((THUMB_W * THUMB_H) as usize);
    for ty in 0..THUMB_H {
        let (y0, y1) = block(ty, THUMB_H, height);
        for tx in 0..THUMB_W {
            let (x0, x1) = block(tx, THUMB_W, width);
            let mut sum: u64 = 0;
            let mut n: u64 = 0;
            for y in y0..y1 {
                let row = y as usize * stride;
                for x in x0..x1 {
                    let i = row + x as usize * 3;
                    let (Some(r), Some(g), Some(b)) =
                        (src.get(i).copied(), src.get(i + 1).copied(), src.get(i + 2).copied())
                    else {
                        continue;
                    };
                    sum += luma(r, g, b) as u64;
                    n += 1;
                }
            }
            // Round half up; `checked_div` covers the empty-block case (a zero-length
            // source), which reads as black rather than as a division by zero.
            out.push((sum + n / 2).checked_div(n).unwrap_or(0) as u8);
        }
    }
    out
}

/// One camera on the board.
#[derive(Debug)]
pub struct Camera {
    pub bus: u8,
    pub addr: u8,
    pub ids: Ids,
    pub opts: Options,
    id: String,
    frames: u64,
    last: Option<(u32, u32, usize)>,
    last_error: Option<String>,
}

impl Camera {
    /// SCCB write addresses these parts ship with.
    // TODO(verify): OV2640 modules are wired at 0x30 (0x60/0x61 as 8-bit SCCB) and
    // OV3660 modules usually at 0x3C; which one a board has is a wiring fact, so the
    // manifest's `address` always wins over this list.
    pub const ADDRESSES: &'static [u8] = &[0x30, 0x3C];

    /// Read PID and VER. `None` means either the address answered nothing or the id is
    /// not one of the two parts above; a caller that needs the difference must call
    /// [`Camera::read_register`] itself.
    pub fn identify(hw: &mut dyn Hardware, bus: u8, addr: u8) -> Option<Ids> {
        let pid = Self::read_register(hw, bus, addr, REG_PID).ok()?;
        let ver = Self::read_register(hw, bus, addr, REG_VER).ok()?;
        let ids = Ids { pid, ver };
        ids.accepted().then_some(ids)
    }

    /// One SCCB register byte.
    pub fn read_register(hw: &mut dyn Hardware, bus: u8, addr: u8, reg: u8) -> Result<u8> {
        let mut buf = [0u8; 1];
        hw.i2c_read(bus, addr, Some(reg), &mut buf)?;
        Ok(buf[0])
    }

    /// Identify, reset the part, then read the ids back.
    ///
    /// The read-back is the point: an address that answers before a reset and not after
    /// is a wiring or a reset problem and says so, rather than being reported as "no
    /// camera here".
    pub fn probe(hw: &mut dyn Hardware, bus: u8, addr: u8, opts: Options, id_suffix: usize) -> Result<Self> {
        let before = Self::identify(hw, bus, addr)
            .ok_or_else(|| McpError::not_found(format!("no OV2640/OV3660 at 0x{addr:02X} on bus {bus}")))?;
        hw.i2c_write(bus, addr, Some(REG_COM7), &[COM7_SRST])?;
        hw.delay_us(RESET_SETTLE_US);
        let ids = Self::identify(hw, bus, addr).ok_or_else(|| {
            McpError::hardware(format!(
                "0x{addr:02X} answered as {} before the soft reset and not after it",
                before.sensor_id()
            ))
        })?;
        Ok(Self {
            bus,
            addr,
            ids,
            opts,
            id: if id_suffix > 1 { format!("camera_{id_suffix}") } else { "camera".into() },
            frames: 0,
            last: None,
            last_error: None,
        })
    }

    /// Take one frame and reduce it to metadata plus a thumbnail.
    pub fn capture(&mut self, hw: &mut dyn Hardware) -> Result<Value> {
        let want = self.opts.frame_bytes();
        let request = self.opts.dvp_request();
        let frame = match hw.dvp_capture(&request) {
            Ok(f) => f,
            Err(e) => {
                self.last_error = Some(e.message.clone());
                return Err(e);
            }
        };
        if frame.len() < want {
            let message = format!(
                "capture returned {} bytes but a {}x{} RGB888 frame needs at least {want}: the \
                 frame buffer is short or `options.width`/`options.height` do not match the sensor",
                frame.len(),
                self.opts.width,
                self.opts.height
            );
            self.last_error = Some(message.clone());
            return Err(McpError::hardware(message));
        }
        let thumb = grey_thumbnail(&frame, self.opts.width, self.opts.height);
        self.frames += 1;
        self.last = Some((self.opts.width, self.opts.height, frame.len()));
        self.last_error = None;
        Ok(json!({
            "ok": true,
            "width": self.opts.width,
            "height": self.opts.height,
            "format": "rgb888",
            "bytes": frame.len(),
            "frame_bytes_used": want,
            "extra_bytes_ignored": frame.len() - want,
            "thumbnail": {
                "width": THUMB_W,
                "height": THUMB_H,
                "format": "grey8",
                "encoding": "base64",
                "bytes": thumb.len(),
                "data_b64": crate::util::b64_encode(&thumb),
            },
            "note": format!(
                "thumbnail only: {THUMB_W}x{THUMB_H} block averages of BT.601 luma, not the frame; the frame itself is {want} bytes"
            ),
        }))
    }

    fn info_value(&self) -> Value {
        let (capture, note) = match (&self.last_error, self.frames) {
            (Some(e), _) => ("failing", format!("the last capture failed: {e}")),
            (None, 0) => (
                "untested",
                "nothing on the SCCB side proves the DVP path works; this reads `working` only \
                 after camera_capture returned a frame"
                    .to_string(),
            ),
            (None, n) => ("working", format!("{n} frame(s) captured since boot")),
        };
        json!({
            "ok": true,
            "sensor": self.ids.sensor_name(),
            "sensor_id": self.ids.sensor_id(),
            "pid": format!("0x{:02X}", self.ids.pid),
            "ver": format!("0x{:02X}", self.ids.ver),
            "bus": self.bus,
            "address": format!("0x{:02X}", self.addr),
            "width": self.opts.width,
            "height": self.opts.height,
            "freq_hz": self.opts.freq_hz,
            "frame_bytes_expected": self.opts.frame_bytes(),
            "frames_captured": self.frames,
            "last_capture": self.last.map(|(w, h, b)| json!({ "width": w, "height": h, "bytes": b })),
            "frame_capture": capture,
            "frame_capture_note": note,
        })
    }

    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "camera",
            &format!(
                "{} camera {}x{} on DVP (SCCB at 0x{:02X})",
                self.ids.sensor_name().to_uppercase(),
                self.opts.width,
                self.opts.height,
                self.addr
            ),
            "i2c",
        );
        i.id = self.id.clone();
        i.address = Some(self.addr);
        i.source = "config".into();
        i.pins = vec![
            self.opts.xclk,
            self.opts.pclk,
            self.opts.vsync,
            self.opts.href,
            self.opts.data[0],
            self.opts.data[1],
            self.opts.data[2],
            self.opts.data[3],
            self.opts.data[4],
            self.opts.data[5],
            self.opts.data[6],
            self.opts.data[7],
        ];
        i.caps = vec!["camera".into(), "i2c".into(), "dvp".into()];
        i.detail = json!({
            "sensor": self.ids.sensor_name(),
            "pid": format!("0x{:02X}", self.ids.pid),
            "ver": format!("0x{:02X}", self.ids.ver),
            "bus": self.bus,
            "width": self.opts.width,
            "height": self.opts.height,
            "freq_hz": self.opts.freq_hz,
            "data_bus": "dvp",
            "thumbnail": format!("{THUMB_W}x{THUMB_H}"),
        });
        i.tools = vec!["camera_info".into(), "camera_capture".into(), "camera_register".into()];
        i
    }
}

impl Device for Camera {
    fn info(&self) -> DeviceInfo {
        Camera::info(self)
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let id_capture = id.clone();
        let id_reg = id.clone();
        vec![
            ToolDef::new(
                "camera_info",
                "Camera identification: sensor, the PID/VER read back over SCCB, the declared \
                 geometry and whether a frame has been captured yet",
            )
            .title("Camera info")
            .category("camera")
            .requires(&["camera"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| ctx.devices.call(&id, "camera_info", args, ctx.hw)),
            ToolDef::new(
                "camera_capture",
                "Take one frame and return its metadata plus a 160x120 greyscale thumbnail as \
                 base64. The thumbnail is block averages of luma, not the frame: a full frame is \
                 far larger than a tool result can carry",
            )
            .title("Camera capture")
            .category("camera")
            .requires(&["camera"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| ctx.devices.call(&id_capture, "camera_capture", args, ctx.hw)),
            ToolDef::new(
                "camera_register",
                "Read one SCCB register (one 8-bit address) from the camera, for bench work",
            )
            .title("Camera register")
            .category("camera")
            .requires(&["camera"])
            .schema(object_schema(
                &[("reg", integer(0, 255, "SCCB register address, e.g. 0x0A for PID"))],
                &["reg"],
            ))
            .bind(move |ctx, args| ctx.devices.call(&id_reg, "camera_register", args, ctx.hw)),
        ]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "camera_info" => Ok(self.info_value()),
            "camera_capture" => self.capture(hw),
            "camera_register" => {
                let reg = j::ranged_i64(args, "reg", 0, 0, 255)? as u8;
                let value = Camera::read_register(hw, self.bus, self.addr, reg)?;
                Ok(json!({
                    "ok": true,
                    "reg": format!("0x{reg:02X}"),
                    "value": value,
                    "value_hex": format!("0x{value:02X}"),
                }))
            }
            other => Err(McpError::not_found(format!("camera has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use std::collections::BTreeMap;

    /// A board whose SCCB answers from a map and whose DVP path returns whatever the
    /// test scripted. Reads are recorded per register, so a test can say which registers
    /// were touched without counting them.
    #[derive(Default)]
    struct Fake {
        regs: BTreeMap<u8, u8>,
        reads: Vec<u8>,
        writes: Vec<(u8, Vec<u8>)>,
        /// A register whose read fails, for the bus-error cases.
        bad_reg: Option<u8>,
        /// A part that stops answering after the soft reset.
        forgets_on_reset: bool,
        frame: Option<Result<Vec<u8>>>,
        capture_opts: Vec<Value>,
        delays_us: Vec<u32>,
    }

    impl Hardware for Fake {
        fn board_info(&mut self) -> crate::hardware::BoardInfo {
            crate::hardware::BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn i2c_read(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, out: &mut [u8]) -> Result<()> {
            let reg = reg.ok_or_else(|| McpError::internal("the camera always names a register"))?;
            self.reads.push(reg);
            if Some(reg) == self.bad_reg {
                return Err(McpError::hardware(format!("SCCB NAK reading 0x{reg:02X}")));
            }
            let v = self
                .regs
                .get(&reg)
                .copied()
                .ok_or_else(|| McpError::hardware(format!("no scripted answer for 0x{reg:02X}")))?;
            assert_eq!(out.len(), 1, "registers are read one byte at a time");
            out[0] = v;
            Ok(())
        }

        fn i2c_write(&mut self, _bus: u8, _addr: u8, reg: Option<u8>, data: &[u8]) -> Result<()> {
            let reg = reg.unwrap_or(0);
            self.writes.push((reg, data.to_vec()));
            // A real SRST puts the part back into its power-on register state; the test
            // only needs the "it stopped answering" variant of that.
            if reg == REG_COM7 && data.first().is_some_and(|b| b & COM7_SRST != 0) && self.forgets_on_reset {
                self.regs.clear();
            }
            Ok(())
        }

        fn dvp_capture(&mut self, opts: &Value) -> Result<Vec<u8>> {
            self.capture_opts.push(opts.clone());
            match self.frame.take() {
                Some(r) => r,
                None => Err(McpError::unsupported("no DVP camera in this fake")),
            }
        }

        fn delay_us(&mut self, us: u32) {
            self.delays_us.push(us);
        }
    }

    fn scripted(pid: u8, ver: u8) -> Fake {
        let mut regs = BTreeMap::new();
        regs.insert(REG_PID, pid);
        regs.insert(REG_VER, ver);
        Fake { regs, ..Default::default() }
    }

    fn opts() -> Options {
        Options {
            xclk: 15,
            pclk: 13,
            vsync: 6,
            href: 7,
            data: [11, 12, 9, 8, 10, 16, 17, 18],
            width: 320,
            height: 240,
            freq_hz: 20_000_000,
        }
    }

    fn dev() -> Camera {
        Camera {
            bus: 0,
            addr: 0x30,
            ids: Ids { pid: PID_OV2640, ver: 0x42 },
            opts: opts(),
            id: "camera".into(),
            frames: 0,
            last: None,
            last_error: None,
        }
    }

    /// A packed RGB888 frame whose four quadrants are constant greys.
    fn quadrants(width: u32, height: u32, tl: u8, tr: u8, bl: u8, br: u8) -> Vec<u8> {
        let mut frame = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                let v = match (x < width / 2, y < height / 2) {
                    (true, true) => tl,
                    (false, true) => tr,
                    (true, false) => bl,
                    (false, false) => br,
                };
                frame.extend_from_slice(&[v, v, v]);
            }
        }
        frame
    }

    fn thumb_bytes(out: &Value) -> Vec<u8> {
        let mut thumb = Vec::new();
        crate::util::b64_decode(out["thumbnail"]["data_b64"].as_str().expect("base64 field"), &mut thumb)
            .expect("the thumbnail is base64");
        thumb
    }

    #[test]
    fn identification_reads_pid_and_ver() {
        let mut hw = scripted(PID_OV2640, 0x42);
        let ids = Camera::identify(&mut hw, 0, 0x30).expect("an OV2640 identifies");
        assert_eq!(ids, Ids { pid: 0x26, ver: 0x42 });
        assert_eq!(ids.sensor_name(), "ov2640");
        assert_eq!(ids.sensor_id(), "0x2642");
        assert_eq!(hw.reads, vec![REG_PID, REG_VER], "PID then VER, one byte each");

        let mut hw = scripted(PID_OV3660, 0x60);
        let ids = Camera::identify(&mut hw, 0, 0x3C).expect("an OV3660 identifies");
        assert_eq!(ids.sensor_name(), "ov3660");
        assert_eq!(ids.sensor_id(), "0x3660");
    }

    #[test]
    fn a_part_whose_id_is_not_claimed_is_left_alone() {
        // 0x56 is an OV5640's PID, which this driver does not drive and will not guess.
        let mut hw = scripted(0x56, 0x40);
        assert!(Camera::identify(&mut hw, 0, 0x30).is_none());
        let mut hw = scripted(0xFF, 0xFF);
        assert!(Camera::identify(&mut hw, 0, 0x30).is_none(), "a floating bus reads 0xFF");
        assert_eq!(Ids { pid: 0x56, ver: 0x40 }.sensor_name(), "unknown");
    }

    #[test]
    fn a_bus_error_is_surfaced_rather_than_reported_as_absence() {
        let mut hw = scripted(PID_OV2640, 0x42);
        hw.bad_reg = Some(REG_PID);
        // `identify` answers "no", because a caller on a scan has nothing to report.
        assert!(Camera::identify(&mut hw, 0, 0x30).is_none());
        // The register tool, which is a deliberate read, passes the error on.
        let err = dev().call("camera_register", &json!({ "reg": REG_PID }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::HARDWARE);
        assert!(err.message.contains("SCCB NAK"), "{}", err.message);
    }

    #[test]
    fn probe_soft_resets_and_reads_the_ids_back() {
        let mut hw = scripted(PID_OV2640, 0x42);
        let cam = Camera::probe(&mut hw, 0, 0x30, opts(), 1).expect("the part probes");
        assert_eq!(cam.ids, Ids { pid: 0x26, ver: 0x42 });
        assert_eq!(cam.id, "camera");
        assert_eq!(
            hw.writes,
            vec![(REG_COM7, vec![COM7_SRST])],
            "one soft reset, no image-pipeline registers guessed"
        );
        assert_eq!(hw.delays_us, vec![RESET_SETTLE_US]);
        assert_eq!(
            hw.reads,
            vec![REG_PID, REG_VER, REG_PID, REG_VER],
            "the ids are read on both sides of the reset"
        );
    }

    #[test]
    fn a_reset_that_loses_the_part_is_reported_as_a_hardware_failure() {
        let mut hw = scripted(PID_OV2640, 0x42);
        hw.forgets_on_reset = true;
        let err = Camera::probe(&mut hw, 0, 0x30, opts(), 1).unwrap_err();
        assert_eq!(err.code, code::HARDWARE);
        assert!(err.message.contains("not after it"), "{}", err.message);
    }

    #[test]
    fn a_second_camera_gets_a_suffixed_id() {
        let mut hw = scripted(PID_OV3660, 0x60);
        let cam = Camera::probe(&mut hw, 1, 0x3C, opts(), 2).expect("the second part probes");
        assert_eq!(cam.id, "camera_2");
    }

    #[test]
    fn nothing_at_the_address_probes_as_not_found() {
        let mut hw = Fake::default();
        let err = Camera::probe(&mut hw, 0, 0x30, opts(), 1).unwrap_err();
        assert_eq!(err.code, code::NOT_FOUND);
        assert!(err.message.contains("0x30"), "{}", err.message);
    }

    #[test]
    fn the_register_tool_reads_one_sccb_byte() {
        let mut hw = scripted(PID_OV2640, 0x42);
        let out = dev().call("camera_register", &json!({ "reg": 0x0B }), &mut hw).unwrap();
        assert_eq!(out["value"], 0x42);
        assert_eq!(out["reg"], "0x0B");
        assert_eq!(out["value_hex"], "0x42");
        assert_eq!(hw.reads, vec![REG_VER]);

        let err = dev().call("camera_register", &json!({ "reg": 300 }), &mut hw).unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS, "a register address is one byte");
    }

    #[test]
    fn a_capture_returns_metadata_and_a_thumbnail_of_the_quadrants() {
        let mut cam = dev();
        let mut hw = Fake { frame: Some(Ok(quadrants(320, 240, 0, 64, 128, 255))), ..Default::default() };
        let out = cam.call("camera_capture", &json!({}), &mut hw).expect("a scripted frame captures");
        assert_eq!(out["width"], 320);
        assert_eq!(out["height"], 240);
        assert_eq!(out["bytes"], 320 * 240 * 3);
        assert_eq!(out["frame_bytes_used"], 320 * 240 * 3);
        assert_eq!(out["extra_bytes_ignored"], 0);
        assert_eq!(out["thumbnail"]["width"], THUMB_W);
        assert_eq!(out["thumbnail"]["height"], THUMB_H);
        assert_eq!(out["thumbnail"]["bytes"], (THUMB_W * THUMB_H) as u64);

        let thumb = thumb_bytes(&out);
        let (w, h) = (THUMB_W as usize, THUMB_H as usize);
        assert_eq!(thumb.len(), w * h, "{THUMB_W} x {THUMB_H} bytes");
        // Each cell covers 5x5 pixels of a 320x240 frame, so the quadrant boundaries
        // fall exactly between cells 31 and 32 and between rows 23 and 24.
        assert_eq!(thumb[0], 0, "top-left quadrant is black");
        assert_eq!(thumb[w - 1], 64, "top-right quadrant");
        assert_eq!(thumb[(h - 1) * w], 128, "bottom-left quadrant");
        assert_eq!(thumb[(h - 1) * w + w - 1], 255, "bottom-right quadrant");
        assert_eq!(thumb[(h / 2 - 1) * w + w / 2 - 1], 0, "last cell of the top-left quadrant");
        assert_eq!(thumb[(h / 2 - 1) * w + w / 2], 64, "first cell of the top-right quadrant");
        assert_eq!(thumb[(h / 2) * w], 128, "the quadrant split is exactly between rows {h}/2 - 1 and {h}/2");
        assert_eq!(cam.frames, 1);
        assert!(cam.last_error.is_none());
    }

    #[test]
    fn the_thumbnail_averages_each_block_instead_of_sampling_it() {
        // 320x240: one thumbnail cell covers two source columns and two rows, so a column
        // pattern of 0/200 averages to 100 everywhere. Sampling would give 0 or 200.
        let mut frame = Vec::with_capacity(320 * 240 * 3);
        for _ in 0..240 {
            for x in 0..320u32 {
                let v = if x % 2 == 0 { 0u8 } else { 200 };
                frame.extend_from_slice(&[v, v, v]);
            }
        }
        let thumb = grey_thumbnail(&frame, 320, 240);
        assert_eq!(thumb.len(), (THUMB_W * THUMB_H) as usize);
        assert!(thumb.iter().all(|v| *v == 100), "every cell is the block average: {thumb:?}");
    }

    #[test]
    fn luma_follows_bt601_and_keeps_greys_exact() {
        assert_eq!(luma(0, 0, 0), 0);
        assert_eq!(luma(255, 255, 255), 255);
        for v in [1u8, 17, 64, 128, 200, 254] {
            assert_eq!(luma(v, v, v), v, "a grey pixel (v, v, v) must stay v");
        }
        assert_eq!(luma(255, 0, 0), 76, "0.299 * 255");
        assert_eq!(luma(0, 255, 0), 150, "0.587 * 255 rounds up from 149.7");
        assert_eq!(luma(0, 0, 255), 29, "0.114 * 255");
    }

    #[test]
    fn a_source_smaller_than_the_thumbnail_does_not_read_out_of_bounds() {
        let thumb = grey_thumbnail(&[10, 20, 30], 1, 1);
        assert_eq!(thumb.len(), (THUMB_W * THUMB_H) as usize);
        assert_eq!(luma(10, 20, 30), 18);
        assert!(thumb.iter().all(|v| *v == 18), "one pixel spreads over every cell");
    }

    #[test]
    fn a_short_frame_is_refused_with_the_size_it_needed() {
        let mut cam = dev();
        let mut hw = Fake { frame: Some(Ok(vec![0u8; 320 * 240 * 3 - 1])), ..Default::default() };
        let err = cam.call("camera_capture", &json!({}), &mut hw).unwrap_err();
        assert_eq!(err.code, code::HARDWARE);
        assert!(err.message.contains("230400"), "{}", err.message);
        assert!(err.message.contains("at least"), "{}", err.message);
        assert_eq!(cam.frames, 0, "a refused frame is not counted");
        assert!(cam.last_error.is_some());
    }

    #[test]
    fn a_platform_without_a_camera_says_so_and_info_stops_claiming_a_capture() {
        let mut cam = dev();
        let mut hw = crate::hardware::NullHardware;
        let info = cam.call("camera_info", &json!({}), &mut hw).unwrap();
        assert_eq!(info["frame_capture"], "untested", "nothing has been captured yet");
        assert_eq!(info["frames_captured"], 0);

        let err = cam.call("camera_capture", &json!({}), &mut hw).unwrap_err();
        assert_eq!(err.code, code::UNSUPPORTED, "the default dvp_capture refusal reaches the model");

        let info = cam.call("camera_info", &json!({}), &mut hw).unwrap();
        assert_eq!(info["frame_capture"], "failing");
        assert!(
            info["frame_capture_note"].as_str().unwrap().contains("not available"),
            "{}",
            info["frame_capture_note"]
        );
    }

    #[test]
    fn a_successful_capture_makes_info_say_the_capture_path_works() {
        let mut cam = dev();
        let mut hw = Fake { frame: Some(Ok(vec![9u8; 320 * 240 * 3])), ..Default::default() };
        cam.call("camera_capture", &json!({}), &mut hw).unwrap();
        let info = cam.call("camera_info", &json!({}), &mut hw).unwrap();
        assert_eq!(info["frame_capture"], "working");
        assert_eq!(info["frames_captured"], 1);
        assert_eq!(info["last_capture"]["bytes"], 320 * 240 * 3);
        assert_eq!(info["sensor"], "ov2640");
        assert_eq!(info["pid"], "0x26");
        assert_eq!(info["ver"], "0x42");
        assert_eq!(info["frame_bytes_expected"], 320 * 240 * 3);
    }

    #[test]
    fn a_capture_hands_the_platform_the_declared_wiring_and_geometry() {
        let mut cam = dev();
        let mut hw = Fake { frame: Some(Ok(vec![7u8; 320 * 240 * 3])), ..Default::default() };
        cam.call("camera_capture", &json!({}), &mut hw).unwrap();
        let req = &hw.capture_opts[0];
        assert_eq!(req["format"], "rgb888");
        assert_eq!(req["width"], 320);
        assert_eq!(req["height"], 240);
        assert_eq!(req["freq_hz"], 20_000_000);
        assert_eq!(req["xclk"], 15);
        assert_eq!(req["pclk"], 13);
        assert_eq!(req["vsync"], 6);
        assert_eq!(req["href"], 7);
        assert_eq!(req["d0"], 11);
        assert_eq!(req["d7"], 18);
    }

    #[test]
    fn the_wiring_and_geometry_come_from_the_manifest_options() {
        let mut o = BTreeMap::new();
        for (k, v) in [
            ("xclk", "15"),
            ("pclk", "13"),
            ("vsync", "6"),
            ("href", "7"),
            ("d0", "11"),
            ("d1", "12"),
            ("d2", "9"),
            ("d3", "8"),
            ("d4", "10"),
            ("d5", "16"),
            ("d6", "17"),
            ("d7", "18"),
        ] {
            o.insert(k.to_string(), v.to_string());
        }
        let parsed = Options::from_options(&o).expect("all twelve pins present");
        assert_eq!(parsed, opts(), "geometry falls back to QVGA at 20 MHz");
        assert_eq!(parsed.frame_bytes(), 230400);

        let mut small = o.clone();
        small.insert("width".into(), "160".into());
        small.insert("height".into(), "120".into());
        small.insert("freq_hz".into(), "10000000".into());
        let parsed = Options::from_options(&small).expect("an explicit geometry wins");
        assert_eq!((parsed.width, parsed.height, parsed.freq_hz), (160, 120, 10_000_000));
        assert_eq!(parsed.frame_bytes(), 57600);

        let mut missing = o.clone();
        missing.remove("d3");
        let err = Options::from_options(&missing).unwrap_err();
        assert!(err.message.contains("options.d3"), "{}", err.message);

        let mut bad = o.clone();
        bad.insert("xclk".into(), "GPIO15".into());
        assert!(Options::from_options(&bad).is_err());
        let mut wide = o;
        wide.insert("width".into(), "99999".into());
        assert!(Options::from_options(&wide).is_err());
    }

    #[test]
    fn a_capture_result_fits_the_tool_result_limit() {
        let mut cam = dev();
        let mut hw = Fake { frame: Some(Ok(quadrants(320, 240, 0, 64, 128, 255))), ..Default::default() };
        let out = cam.call("camera_capture", &json!({}), &mut hw).unwrap();
        let text = serde_json::to_string(&out).expect("the result serialises");
        assert!(
            text.len() < crate::agent::MAX_TOOL_RESULT_BYTES,
            "{} bytes would be cut off at {}",
            text.len(),
            crate::agent::MAX_TOOL_RESULT_BYTES
        );
        // The same for info, which a model may call on every turn.
        let info = serde_json::to_string(&cam.info_value()).unwrap();
        assert!(info.len() < crate::agent::MAX_TOOL_RESULT_BYTES, "{} bytes", info.len());
    }

    #[test]
    fn the_device_advertises_three_tools_and_its_wiring() {
        let mut cam = dev();
        let names: Vec<String> = cam.tools().iter().map(|t| t.spec.name.clone()).collect();
        assert_eq!(
            names,
            vec!["camera_info".to_string(), "camera_capture".to_string(), "camera_register".to_string()]
        );
        let info = cam.info();
        assert_eq!(info.kind, "camera");
        assert_eq!(info.id, "camera");
        assert_eq!(info.address, Some(0x30));
        assert_eq!(info.source, "config", "a DVP camera is declared, never scanned for");
        assert_eq!(info.pins.len(), 12, "xclk, pclk, vsync, href and d0..d7");
        assert!(info.caps.iter().any(|c| c == "camera"));
        assert_eq!(info.detail["data_bus"], "dvp");
        assert_eq!(info.tools.len(), 3);

        let err = cam.call("camera_nope", &json!({}), &mut scripted(PID_OV2640, 0x42)).unwrap_err();
        assert_eq!(err.code, code::NOT_FOUND);
    }
}
