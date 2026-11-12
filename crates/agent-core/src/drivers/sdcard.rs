//! SD card over SPI: raw 512-byte blocks, and **no filesystem**.
//!
//! This driver does one job: put the card into SPI mode (CMD0, CMD8, ACMD41, CMD58),
//! read its card-specific data (CMD9) and hand back whole raw blocks (CMD17). It does
//! not mount, create, format or parse a FAT (or exFAT) volume, and every tool it
//! publishes says so. Mounting a filesystem is a separate job: it needs cluster
//! chains, directory entries, long names, an allocation table kept in step with
//! writes and the wear behaviour that comes with a real volume. A half-written FAT
//! driver is worse than a raw block reader, because a bench can trust the block
//! reader.
//!
//! What a block read *is* good for: block 0 is the MBR or the boot sector, so a bench
//! can see the partition table, the volume label and whether the card is blank long
//! before anyone writes a filesystem.
//!
//! Chip select is a GPIO the caller names and this driver drives (as the MFRC522
//! driver does), so the pin is visible to a manifest instead of hidden inside an SPI
//! transaction.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::tool::{integer, object_schema, ToolDef};
use crate::util::b64_encode;
use serde_json::{json, Map, Value};

// Command indices, SD spec part 1 (SPI mode command set).
const CMD0_GO_IDLE_STATE: u8 = 0;
const CMD8_SEND_IF_COND: u8 = 8;
const CMD9_SEND_CSD: u8 = 9;
const CMD17_READ_SINGLE_BLOCK: u8 = 17;
const CMD41_SD_SEND_OP_COND: u8 = 41;
const CMD55_APP_CMD: u8 = 55;
const CMD58_READ_OCR: u8 = 58;

/// R1 bits. Bit 7 means "no response" and never arrives here as an R1.
const R1_IDLE: u8 = 0x01;
const R1_CRC_ERROR: u8 = 0x08;
const R1_ILLEGAL_COMMAND: u8 = 0x04;

/// The start token that opens a data packet, and the size of a block.
const TOKEN_START_BLOCK: u8 = 0xFE;
const BLOCK_SIZE: usize = 512;
const CSD_SIZE: usize = 16;

/// A card answers a command within eight bytes (N_CR).
// TODO(verify): N_CR is at most eight bytes in the SD spec (SPI mode); a slow card on a
// long wire may want more.
const ANSWER_POLLS: usize = 8;

/// Rounds of "clock 0xFF, look for the start token" before a read is called a timeout.
/// Each round waits 1 ms, so this is roughly 200 ms.
// TODO(verify): the spec allows up to 100 ms of read access time before the token; this
// doubles it. Measure with the slowest card that matters.
const TOKEN_ROUNDS: u32 = 200;

/// ACMD41 is polled until the card leaves the idle state, at most this long.
// TODO(verify): initialisation takes up to 1 s after power-up per the spec; the board is
// powered long before this runs, so a card needing the full second is unusual.
const INIT_TIMEOUT_MS: u32 = 1_000;

/// CMD8's check pattern, which a card that supports 2.7-3.6 V echoes back.
const IF_COND_ARG: u32 = 0x1AA;

/// OCR bit 30: the card is block addressed (SDHC/SDXC).
const OCR_CCS: u32 = 1 << 30;

/// CRC-7 over the command's bytes (polynomial 0x09, seven bits).
fn crc7(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        let mut m = byte;
        for _ in 0..8 {
            crc <<= 1;
            if (m & 0x80) ^ (crc & 0x80) != 0 {
                crc ^= 0x09;
            }
            m <<= 1;
        }
    }
    crc & 0x7F
}

/// The CRC byte of a command frame: CRC-7 over the five bytes, shifted left once with
/// the trailing one bit the frame requires.
fn frame_crc(command: &[u8]) -> u8 {
    (crc7(command) << 1) | 1
}

/// CRC-16/CCITT (polynomial 0x1021, init 0): the check on a data packet.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// How the card is addressed. The two modes take a different CMD17 argument, and
/// confusing them reads the wrong block or none at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// SDSC: the argument is a byte offset.
    Byte,
    /// SDHC/SDXC: the argument is a block index.
    Block,
}

impl Addressing {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Byte => "byte",
            Self::Block => "block",
        }
    }
}

/// Capacity in bytes from the card-specific data, or `None` for a CSD structure this
/// driver does not know (anything other than version 1.0 or 2.0).
pub fn csd_capacity(csd: &[u8; CSD_SIZE]) -> Option<u64> {
    match csd[0] >> 6 {
        // CSD version 2.0 (SDHC/SDXC): C_SIZE counts 512 KiB units.
        1 => {
            let c_size = ((u64::from(csd[7]) & 0x3F) << 16) | (u64::from(csd[8]) << 8) | u64::from(csd[9]);
            Some((c_size + 1) * 512 * 1024)
        }
        // CSD version 1.0 (SDSC): C_SIZE, C_SIZE_MULT and READ_BL_LEN.
        0 => {
            let read_bl_len = u32::from(csd[5] & 0x0F);
            let c_size =
                (u64::from(csd[6] & 0x03) << 10) | (u64::from(csd[7]) << 2) | (u64::from(csd[8] & 0xC0) >> 6);
            let c_size_mult = (u64::from(csd[9] & 0x03) << 1) | (u64::from(csd[10] & 0x80) >> 7);
            let block_len = 1u64 << read_bl_len;
            let mult = 1u64 << (c_size_mult + 2);
            Some((c_size + 1) * mult * block_len)
        }
        _ => None,
    }
}

/// The R1 bits spelled out, so a failure says *which* bit the card raised.
fn r1_text(r1: u8) -> String {
    if r1 == 0 {
        return "ready".into();
    }
    let mut why = Vec::new();
    if r1 & R1_IDLE != 0 {
        why.push("still in idle state");
    }
    if r1 & R1_ILLEGAL_COMMAND != 0 {
        why.push("illegal command");
    }
    if r1 & R1_CRC_ERROR != 0 {
        why.push("CRC error on the command");
    }
    if r1 & 0x40 != 0 {
        why.push("parameter error");
    }
    if r1 & 0x20 != 0 {
        why.push("address error");
    }
    if r1 & 0x10 != 0 {
        why.push("erase sequence error");
    }
    if why.is_empty() {
        why.push("an R1 bit this driver does not name");
    }
    format!("0x{r1:02X} ({})", why.join(", "))
}

fn r1_error(command: &str, r1: u8) -> McpError {
    McpError::hardware(format!("{command} answered {}", r1_text(r1)))
}

pub struct SdCard {
    pub bus: u8,
    pub cs: i32,
    addressing: Addressing,
    /// True when the card answered CMD8, i.e. it is a version 2 part.
    v2: bool,
    ocr: u32,
    capacity: Option<u64>,
    /// Why the capacity is unknown, when it is.
    capacity_note: String,
    reads: u32,
    failures: u32,
    last_block: Option<u64>,
    last_error: Option<String>,
    id: String,
}

impl SdCard {
    /// Put the card into SPI mode and read what it says about itself.
    pub fn init(hw: &mut dyn Hardware, bus: u8, cs: i32) -> Result<Self> {
        hw.gpio_config(cs, PinMode::Output)?;
        hw.gpio_write(cs, true)?;
        // At least 74 clocks with CS high and MOSI high: the card samples the mode it
        // was powered into during these, and that is how SPI mode is entered.
        // TODO(verify): 74 clocks from the SD spec (SPI mode entry); this sends 80.
        let clocks = hw.spi_transfer(bus, &[0xFF; 10])?;
        if clocks.len() != 10 {
            return Err(McpError::hardware(format!(
                "SPI returned {} byte(s) for the 10 clocks that enter SPI mode",
                clocks.len()
            )));
        }

        let mut card = Self {
            bus,
            cs,
            addressing: Addressing::Byte,
            v2: false,
            ocr: 0,
            capacity: None,
            capacity_note: "the card has not been initialised".into(),
            reads: 0,
            failures: 0,
            last_block: None,
            last_error: None,
            id: "sdcard".into(),
        };

        // CMD0: idle state. R1 = 0x01 is the answer that says "an SD card on SPI".
        let (r1, _) = card.command(hw, CMD0_GO_IDLE_STATE, 0, 0)?;
        if r1 != R1_IDLE {
            return Err(McpError::hardware(format!(
                "CMD0 answered {} instead of 0x01 (still in idle state): not an SD card in SPI mode, or the card is not powered",
                r1_text(r1)
            )));
        }

        // CMD8: version 2 cards echo the check pattern. A version 1 card rejects the
        // command outright, which is a normal answer rather than a failure.
        let (r1, echo) = card.command(hw, CMD8_SEND_IF_COND, IF_COND_ARG, 4)?;
        card.v2 = if r1 & R1_ILLEGAL_COMMAND != 0 {
            false
        } else if r1 == R1_IDLE {
            if echo.len() < 4 || echo[..4] != [0x00, 0x00, 0x01, 0xAA] {
                return Err(McpError::hardware(format!(
                    "CMD8 echoed {echo:02X?} instead of 00 00 01 AA: the card does not accept the 2.7-3.6 V supply this driver assumes"
                )));
            }
            true
        } else {
            return Err(r1_error("CMD8", r1));
        };

        // ACMD41: wait for the card to leave the idle state. HCS asks for block
        // addressing, and only a version 2 card may see that bit.
        let hcs = if card.v2 { 1 << 30 } else { 0 };
        let mut waited_ms = 0u32;
        loop {
            let (r1, _) = card.command(hw, CMD55_APP_CMD, 0, 0)?;
            if r1 != 0 && r1 != R1_IDLE {
                return Err(r1_error("CMD55", r1));
            }
            let (r1, _) = card.command(hw, CMD41_SD_SEND_OP_COND, hcs, 0)?;
            if r1 == 0 {
                break;
            }
            if waited_ms >= INIT_TIMEOUT_MS {
                return Err(McpError::hardware(format!(
                    "the card stayed in the idle state for {INIT_TIMEOUT_MS} ms of ACMD41 ({})",
                    r1_text(r1)
                )));
            }
            hw.delay_us(1_000);
            waited_ms += 1;
        }

        // CMD58: the OCR says which voltages were accepted and, in bit 30, whether the
        // card is block addressed.
        let (r1, ocr) = card.command(hw, CMD58_READ_OCR, 0, 4)?;
        if r1 != 0 {
            return Err(r1_error("CMD58", r1));
        }
        if ocr.len() < 4 {
            return Err(McpError::hardware(format!("CMD58 returned {} of its four OCR bytes", ocr.len())));
        }
        card.ocr = u32::from_be_bytes([ocr[0], ocr[1], ocr[2], ocr[3]]);
        card.addressing = if card.ocr & OCR_CCS != 0 { Addressing::Block } else { Addressing::Byte };

        // CMD9: the card-specific data, where the capacity lives. A card that will not
        // hand it over still reads blocks, so this degrades instead of failing.
        match card.read_csd(hw) {
            Ok(csd) => match csd_capacity(&csd) {
                Some(bytes) => card.capacity = Some(bytes),
                None => {
                    card.capacity_note = format!(
                        "the CSD structure byte is 0x{:02X}, which is neither version 1.0 nor 2.0",
                        csd[0] >> 6
                    )
                }
            },
            Err(e) => card.capacity_note = format!("the CSD read (CMD9) failed: {}", e.message),
        }
        Ok(card)
    }

    /// One command in its own chip-select window: the six-byte frame, then the R1 and
    /// the `extra` bytes some commands send after it.
    fn command(&mut self, hw: &mut dyn Hardware, cmd: u8, arg: u32, extra: usize) -> Result<(u8, Vec<u8>)> {
        hw.gpio_write(self.cs, false)?;
        let result = self.exchange(hw, cmd, arg, extra);
        // Release the card even when the exchange failed.
        hw.gpio_write(self.cs, true)?;
        result
    }

    /// The framed command, with chip select already low.
    fn exchange(&mut self, hw: &mut dyn Hardware, cmd: u8, arg: u32, extra: usize) -> Result<(u8, Vec<u8>)> {
        let mut frame = [0x40 | cmd, 0, 0, 0, 0, 0];
        frame[1..5].copy_from_slice(&arg.to_be_bytes());
        frame[5] = frame_crc(&frame[..5]);
        let echo = hw.spi_transfer(self.bus, &frame)?;
        if echo.len() != frame.len() {
            return Err(McpError::hardware(format!(
                "SPI returned {} byte(s) for the {} of a command frame",
                echo.len(),
                frame.len()
            )));
        }

        // R1 is the first byte after the frame without bit 7 set; a card that is not
        // there holds the line high (0xFF) until the polls run out.
        let mut r1 = 0xFF;
        for _ in 0..ANSWER_POLLS {
            let byte = hw.spi_transfer(self.bus, &[0xFF])?;
            match byte.first() {
                Some(b) if b & 0x80 == 0 => {
                    r1 = *b;
                    break;
                }
                _ => {}
            }
        }
        if r1 == 0xFF {
            return Err(McpError::hardware(format!(
                "the card did not answer CMD{cmd} with an R1 within {ANSWER_POLLS} bytes (check 3.3 V, CS GPIO{} and that MISO is wired)",
                self.cs
            )));
        }

        let mut payload = Vec::new();
        if extra > 0 {
            payload = hw.spi_transfer(self.bus, &vec![0xFF; extra])?;
            if payload.len() < extra {
                return Err(McpError::hardware(format!(
                    "SPI returned {} byte(s) for the {extra} that follow an R1",
                    payload.len()
                )));
            }
        }
        Ok((r1, payload))
    }

    /// The 16 CSD bytes, in a chip-select window of its own.
    fn read_csd(&mut self, hw: &mut dyn Hardware) -> Result<[u8; CSD_SIZE]> {
        hw.gpio_write(self.cs, false)?;
        let result = self.read_csd_low(hw);
        hw.gpio_write(self.cs, true)?;
        result
    }

    fn read_csd_low(&mut self, hw: &mut dyn Hardware) -> Result<[u8; CSD_SIZE]> {
        let (r1, _) = self.exchange(hw, CMD9_SEND_CSD, 0, 0)?;
        if r1 != 0 {
            return Err(r1_error("CMD9", r1));
        }
        let data = self.packet(hw, CSD_SIZE)?;
        let mut csd = [0u8; CSD_SIZE];
        csd.copy_from_slice(&data);
        Ok(csd)
    }

    /// Read one data packet of `len` bytes plus its CRC, with chip select already low.
    ///
    /// The card chooses how long to take before the start token, so the line is clocked
    /// 0xFF in small chunks until the token appears. Chip select stays low throughout,
    /// which is what keeps the packet aligned.
    fn packet(&mut self, hw: &mut dyn Hardware, len: usize) -> Result<Vec<u8>> {
        let mut payload: Vec<u8> = Vec::with_capacity(len + 2);
        let mut seen = false;
        for _ in 0..TOKEN_ROUNDS {
            let chunk = hw.spi_transfer(self.bus, &[0xFF; 8])?;
            if let Some(token) = chunk.iter().position(|b| *b == TOKEN_START_BLOCK) {
                payload.extend_from_slice(&chunk[token + 1..]);
                seen = true;
                break;
            }
            hw.delay_us(1_000);
        }
        if !seen {
            return Err(McpError::hardware(format!(
                "the card never sent the data start token (0xFE) in {TOKEN_ROUNDS} rounds of clocking"
            )));
        }

        let need = len + 2;
        if payload.len() < need {
            let mut rest = hw.spi_transfer(self.bus, &vec![0xFF; need - payload.len()])?;
            payload.append(&mut rest);
        }
        if payload.len() < need {
            return Err(McpError::hardware(format!(
                "the data packet stopped after {} of its {need} bytes",
                payload.len()
            )));
        }
        let crc = u16::from_be_bytes([payload[len], payload[len + 1]]);
        let computed = crc16(&payload[..len]);
        if crc != computed {
            return Err(McpError::hardware(format!(
                "the card's CRC-16 for the {len}-byte packet is 0x{crc:04X}; the bytes compute to 0x{computed:04X}"
            )));
        }
        payload.truncate(len);
        Ok(payload)
    }

    /// The CMD17 argument for a block: a block index on SDHC/SDXC, a byte offset on
    /// SDSC.
    // TODO(verify): the block length stays at the 512-byte default the spec gives an
    // SDSC card, so CMD16 is not issued; confirm on a version 1 card that CMD17 reads
    // 512 bytes without it.
    fn block_argument(&self, block: u64) -> Result<u32> {
        match self.addressing {
            Addressing::Block => u32::try_from(block).map_err(|_| {
                McpError::invalid_params(format!("block {block} is past what a 32-bit block address reaches"))
            }),
            Addressing::Byte => block
                .checked_mul(BLOCK_SIZE as u64)
                .filter(|bytes| *bytes <= u64::from(u32::MAX))
                .map(|bytes| bytes as u32)
                .ok_or_else(|| {
                    McpError::invalid_params(format!(
                        "block {block} is past the 4 GiB a byte-addressed SDSC card can reach"
                    ))
                }),
        }
    }

    /// One 512-byte block. The bookkeeping happens here, so a failed read leaves the
    /// reason where `sdcard_status` can show it.
    pub fn read_block(&mut self, hw: &mut dyn Hardware, block: u64) -> Result<Vec<u8>> {
        self.reads = self.reads.saturating_add(1);
        match self.read_block_inner(hw, block) {
            Ok(data) => {
                self.last_block = Some(block);
                self.last_error = None;
                Ok(data)
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(e.message.clone());
                Err(e)
            }
        }
    }

    fn read_block_inner(&mut self, hw: &mut dyn Hardware, block: u64) -> Result<Vec<u8>> {
        let arg = self.block_argument(block)?;
        hw.gpio_write(self.cs, false)?;
        let result = self.read_block_low(hw, block, arg);
        hw.gpio_write(self.cs, true)?;
        result
    }

    fn read_block_low(&mut self, hw: &mut dyn Hardware, block: u64, arg: u32) -> Result<Vec<u8>> {
        let (r1, _) = self.exchange(hw, CMD17_READ_SINGLE_BLOCK, arg, 0)?;
        if r1 != 0 {
            let hint = if r1 & 0x20 != 0 {
                " (an SDSC card addresses bytes, so a block past its capacity lands nowhere)"
            } else {
                ""
            };
            return Err(McpError::hardware(format!(
                "CMD17 for block {block} answered {}{hint}",
                r1_text(r1)
            )));
        }
        self.packet(hw, BLOCK_SIZE)
    }

    pub fn addressing(&self) -> Addressing {
        self.addressing
    }

    pub fn ocr(&self) -> u32 {
        self.ocr
    }

    pub fn capacity(&self) -> Option<u64> {
        self.capacity
    }

    /// What the card is, for a person.
    pub fn card_type(&self) -> &'static str {
        match (self.v2, self.addressing) {
            (true, Addressing::Block) => "SDHC/SDXC",
            (true, Addressing::Byte) => "SD version 2 card in byte addressing",
            (false, _) => "SDSC (standard capacity)",
        }
    }

    /// The fields `sdcard_info` and `sdcard_status` share.
    fn card_json(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("card_type".into(), json!(self.card_type()));
        out.insert("addressing".into(), json!(self.addressing.as_str()));
        out.insert("ocr".into(), json!(format!("0x{:08X}", self.ocr)));
        out.insert("ccs".into(), json!(self.ocr & OCR_CCS != 0));
        match self.capacity {
            Some(bytes) => {
                out.insert("capacity_bytes".into(), json!(bytes));
                out.insert("capacity_mb".into(), json!(bytes / (1024 * 1024)));
                out.insert("capacity_source".into(), json!("CSD from CMD9"));
            }
            None => {
                out.insert("capacity_bytes".into(), Value::Null);
                out.insert("capacity_unknown".into(), json!(self.capacity_note));
            }
        }
        out.insert("bus".into(), json!(self.bus));
        out.insert("cs_pin".into(), json!(self.cs));
        out
    }
}

impl Device for SdCard {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "sdcard",
            &format!("{} on SPI{} CS GPIO{}", self.card_type(), self.bus, self.cs),
            "spi",
        );
        i.id = self.id.clone();
        i.pins = vec![self.cs];
        i.source = "config".into();
        i.caps = vec!["sdcard".into(), "spi".into()];
        i.detail = json!({
            "addressing": self.addressing.as_str(),
            "capacity_bytes": self.capacity,
            "ocr": format!("0x{:08X}", self.ocr),
        });
        i.tools = vec!["sdcard_info".into(), "sdcard_read_block".into(), "sdcard_status".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let raw_only = "Raw block device: this driver does not mount, create or parse a filesystem, so mounting FAT is a separate job";

        let info_id = id.clone();
        let info = ToolDef::new(
            "sdcard_info",
            &format!(
                "What the SPI SD card on CS GPIO{} says about itself: type from CMD58's OCR (SDHC/SDXC or SDSC), addressing mode, and capacity from the card-specific data (CMD9). {raw_only}",
                self.cs
            ),
        )
        .title("SD card info")
        .category("storage")
        .requires(&["sdcard"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = info_id.clone();
            ctx.devices.call(&id, "sdcard_info", args, ctx.hw)
        });

        let read_id = id.clone();
        let read = ToolDef::new(
            "sdcard_read_block",
            &format!(
                "Read one raw 512-byte block from the SPI SD card on CS GPIO{} and return it as base64. Block 0 is the boot sector or the MBR, which is what a bench usually wants first. {raw_only}: these are the bytes on the medium, not a file",
                self.cs
            ),
        )
        .title("SD read block")
        .category("storage")
        .requires(&["sdcard"])
        .schema(object_schema(&[("block", integer(0, u32::MAX as i64, "block index, default 0"))], &[]))
        .bind(move |ctx, args| {
            let id = read_id.clone();
            ctx.devices.call(&id, "sdcard_read_block", args, ctx.hw)
        });

        let status_id = id;
        let status = ToolDef::new(
            "sdcard_status",
            &format!(
                "The SPI SD driver's own state for the card on CS GPIO{}: initialised, card type, addressing, capacity, how many block reads ran and what the last one failed with. It does not re-poll the card. {raw_only}",
                self.cs
            ),
        )
        .title("SD card status")
        .category("storage")
        .requires(&["sdcard"])
        .schema(object_schema(&[], &[]))
        .bind(move |ctx, args| {
            let id = status_id.clone();
            ctx.devices.call(&id, "sdcard_status", args, ctx.hw)
        });

        vec![info, read, status]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "sdcard_info" => {
                let mut out = self.card_json();
                out.insert(
                    "note".into(),
                    json!("raw block device: no filesystem is mounted and nothing here parses FAT"),
                );
                Ok(Value::Object(out))
            }
            "sdcard_read_block" => {
                let block = crate::json::ranged_i64(args, "block", 0, 0, u32::MAX as i64)? as u64;
                let data = self.read_block(hw, block)?;
                let mut out = Map::new();
                out.insert("block".into(), json!(block));
                out.insert("bytes".into(), json!(data.len()));
                out.insert("data_base64".into(), json!(b64_encode(&data)));
                out.insert("crc_ok".into(), json!(true));
                if block == 0 {
                    // 0x55 0xAA at the end of a boot sector is what tells a bench the
                    // block is a boot record rather than an erased card.
                    out.insert(
                        "boot_signature".into(),
                        json!(data.len() >= 512 && data[510] == 0x55 && data[511] == 0xAA),
                    );
                }
                out.insert(
                    "note".into(),
                    json!("one raw block, not a file: no filesystem is mounted and FAT is not parsed"),
                );
                Ok(Value::Object(out))
            }
            "sdcard_status" => {
                let mut out = self.card_json();
                out.insert("initialised".into(), json!(true));
                out.insert("version_2_card".into(), json!(self.v2));
                out.insert("block_reads".into(), json!(self.reads));
                out.insert("block_read_failures".into(), json!(self.failures));
                out.insert("last_block".into(), json!(self.last_block));
                out.insert("last_error".into(), json!(self.last_error));
                out.insert(
                    "note".into(),
                    json!("state from the initialisation and the last read; the card itself is not re-polled, and no filesystem is mounted"),
                );
                Ok(Value::Object(out))
            }
            other => Err(McpError::not_found(format!("sdcard has no tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::code;
    use crate::hardware::BoardInfo;
    use crate::util::b64_decode;

    /// A card on a virtual SPI bus: it parses the command frames it is given, records
    /// them, and answers from a script. Every byte the driver sends is kept in `sent`,
    /// so the tests can assert the exact wire traffic.
    struct FakeSpi {
        bus: u8,
        cs: i32,
        cs_low: bool,
        sent: Vec<u8>,
        frames: Vec<[u8; 6]>,
        crc_ok: Vec<bool>,
        /// `[R1, extras...]` for the command being answered.
        reply: Vec<u8>,
        /// Start token, payload and CRC queued by CMD9/CMD17.
        packet: Vec<u8>,
        /// Bytes of 0xFF before the first response byte (the card's N_CR).
        silence: usize,
        /// Answers every command with nothing at all.
        silent: bool,
        withhold_token: bool,
        bad_block_crc: bool,
        v2: bool,
        ready: bool,
        ocr: u32,
        csd: [u8; CSD_SIZE],
        block: [u8; BLOCK_SIZE],
        /// Successive R1 answers to ACMD41; the last one repeats.
        op_cond: Vec<u8>,
        op_cond_seen: usize,
    }

    impl FakeSpi {
        fn new(csd: [u8; CSD_SIZE], ocr: u32, v2: bool) -> Self {
            let mut block = [0u8; BLOCK_SIZE];
            for (i, b) in block.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            Self {
                bus: 0,
                cs: 5,
                cs_low: false,
                sent: Vec::new(),
                frames: Vec::new(),
                crc_ok: Vec::new(),
                reply: Vec::new(),
                packet: Vec::new(),
                silence: 1,
                silent: false,
                withhold_token: false,
                bad_block_crc: false,
                v2,
                ready: false,
                ocr,
                csd,
                block,
                op_cond: vec![0x01, 0x00],
                op_cond_seen: 0,
            }
        }

        /// A version 2 card: 8 GiB, block addressed.
        fn v2_card() -> Self {
            let mut csd = [0u8; CSD_SIZE];
            csd[0] = 0x40; // CSD version 2.0
            csd[8] = 0x3F;
            csd[9] = 0xFF; // C_SIZE = 0x3FFF -> (0x3FFF + 1) * 512 KiB = 8 GiB
            Self::new(csd, OCR_CCS | 0x00FF_8000, true)
        }

        /// A version 1 card: 128 MiB, byte addressed.
        fn v1_card() -> Self {
            let mut csd = [0u8; CSD_SIZE];
            csd[5] = 0x09; // READ_BL_LEN = 9 -> 512-byte blocks
            csd[7] = 0x7F;
            csd[8] = 0xC0; // C_SIZE = 511
            csd[9] = 0x03;
            csd[10] = 0x80; // C_SIZE_MULT = 7
            Self::new(csd, 0x00FF_8000, false)
        }

        fn answer(&mut self, cmd: u8) -> Vec<u8> {
            match cmd {
                CMD0_GO_IDLE_STATE => {
                    self.ready = false;
                    vec![R1_IDLE]
                }
                CMD8_SEND_IF_COND => {
                    if self.v2 {
                        vec![R1_IDLE, 0x00, 0x00, 0x01, 0xAA]
                    } else {
                        vec![R1_ILLEGAL_COMMAND | R1_IDLE]
                    }
                }
                CMD55_APP_CMD => vec![if self.ready { 0x00 } else { R1_IDLE }],
                CMD41_SD_SEND_OP_COND => {
                    let last = self.op_cond.len().saturating_sub(1);
                    let r1 = self.op_cond[self.op_cond_seen.min(last)];
                    self.op_cond_seen += 1;
                    if r1 == 0 {
                        self.ready = true;
                    }
                    vec![r1]
                }
                CMD58_READ_OCR => {
                    let o = self.ocr;
                    vec![0x00, (o >> 24) as u8, (o >> 16) as u8, (o >> 8) as u8, o as u8]
                }
                CMD9_SEND_CSD => {
                    let packet = packet_of(&self.csd, false);
                    self.packet = packet;
                    vec![0x00]
                }
                CMD17_READ_SINGLE_BLOCK => {
                    if !self.withhold_token {
                        let packet = packet_of(&self.block, self.bad_block_crc);
                        self.packet = packet;
                    }
                    vec![0x00]
                }
                _ => vec![0x00],
            }
        }
    }

    /// A start token, the payload, and its CRC-16 (or a deliberately wrong one).
    fn packet_of(data: &[u8], corrupt_crc: bool) -> Vec<u8> {
        let mut v = vec![TOKEN_START_BLOCK];
        v.extend_from_slice(data);
        let crc = crc16(data);
        let hi = if corrupt_crc { (crc >> 8) as u8 ^ 0xFF } else { (crc >> 8) as u8 };
        v.push(hi);
        v.push(crc as u8);
        v
    }

    impl Hardware for FakeSpi {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), firmware: "test".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
            Ok(())
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin != self.cs {
                return Err(McpError::not_found(format!("not the chip select: GPIO{pin}")));
            }
            self.cs_low = !high;
            Ok(())
        }

        fn spi_transfer(&mut self, bus: u8, data: &[u8]) -> Result<Vec<u8>> {
            if bus != self.bus {
                return Err(McpError::not_found(format!("the card is on SPI{}, not SPI{bus}", self.bus)));
            }
            self.sent.extend_from_slice(data);
            if data.len() >= 6 && data[0] & 0xC0 == 0x40 {
                // A command frame. CS has to be low for it, or the card ignores it.
                if !self.cs_low {
                    return Err(McpError::hardware("a command frame was sent with CS high"));
                }
                let mut frame = [0u8; 6];
                frame.copy_from_slice(&data[..6]);
                self.frames.push(frame);
                let expected = frame_crc(&[frame[0], frame[1], frame[2], frame[3], frame[4]]);
                self.crc_ok.push(frame[5] == expected);
                if self.silent {
                    return Ok(vec![0xFF; data.len()]);
                }
                let cmd = frame[0] & 0x3F;
                self.reply = self.answer(cmd);
                self.silence = 1;
                return Ok(vec![0xFF; data.len()]);
            }

            // A read: the R1, then the extra bytes, then a queued data packet.
            let mut out = Vec::with_capacity(data.len());
            while out.len() < data.len() {
                if self.silence > 0 {
                    self.silence -= 1;
                    out.push(0xFF);
                    continue;
                }
                if !self.reply.is_empty() {
                    out.push(self.reply.remove(0));
                    continue;
                }
                if !self.packet.is_empty() {
                    out.push(self.packet.remove(0));
                    continue;
                }
                out.push(0xFF);
            }
            Ok(out)
        }
    }

    #[test]
    fn the_crc_helpers_match_the_documented_values() {
        // The two frame CRCs the SD spec states for the first two commands.
        assert_eq!(frame_crc(&[0x40, 0, 0, 0, 0]), 0x95, "CMD0's frame CRC");
        assert_eq!(frame_crc(&[0x48, 0, 0, 0x01, 0xAA]), 0x87, "CMD8's frame CRC");
        // CRC-16/XMODEM's published check value for "123456789".
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    #[test]
    fn initialisation_sends_the_documented_command_sequence() {
        let mut hw = FakeSpi::v2_card();
        let card = SdCard::init(&mut hw, 0, 5).expect("a scripted card initialises");
        assert_eq!(card.card_type(), "SDHC/SDXC");
        assert_eq!(card.addressing(), Addressing::Block);
        assert_eq!(card.capacity(), Some(8 * 1024 * 1024 * 1024));

        // 74+ clocks with CS high enter SPI mode, before any command.
        assert_eq!(&hw.sent[..10], &[0xFF; 10], "the prelude clocks MOSI high with CS high");
        // The exact bytes, CRC included. The ACMD41 pair is polled until the card
        // answers \"ready\", which the scripted card does on its second try.
        assert_eq!(hw.frames[0], [0x40, 0x00, 0x00, 0x00, 0x00, 0x95], "CMD0");
        assert_eq!(hw.frames[1], [0x48, 0x00, 0x00, 0x01, 0xAA, 0x87], "CMD8 with its check pattern");
        assert_eq!(hw.frames[2], [0x77, 0x00, 0x00, 0x00, 0x00, 0x65], "CMD55 opens the ACMD");
        assert_eq!(hw.frames[3], [0x69, 0x40, 0x00, 0x00, 0x00, 0x77], "CMD41 with HCS set");
        assert_eq!(hw.frames[4], [0x77, 0x00, 0x00, 0x00, 0x00, 0x65], "CMD55 again while idle");
        assert_eq!(hw.frames[5], [0x69, 0x40, 0x00, 0x00, 0x00, 0x77], "CMD41 again with HCS set");
        assert_eq!(hw.frames[6], [0x7A, 0x00, 0x00, 0x00, 0x00, 0xFD], "CMD58 reads the OCR");
        assert_eq!(hw.frames[7], [0x49, 0x00, 0x00, 0x00, 0x00, 0xAF], "CMD9 reads the CSD");
        // Nothing else: CMD16 is not issued, because the block length is already 512.
        assert_eq!(hw.frames.len(), 8, "the initialisation sends eight command frames");
        assert!(hw.crc_ok.iter().all(|ok| *ok), "every frame carried the right CRC: {:?}", hw.crc_ok);
    }

    #[test]
    fn a_version_1_card_is_byte_addressed_and_gets_no_hcs_bit() {
        let mut hw = FakeSpi::v1_card();
        let card = SdCard::init(&mut hw, 0, 5).expect("an SDSC card initialises");
        assert_eq!(card.card_type(), "SDSC (standard capacity)");
        assert_eq!(card.addressing(), Addressing::Byte);
        // 128 MiB from a version 1 CSD: (511 + 1) * 2^9 * 2^9.
        assert_eq!(card.capacity(), Some(128 * 1024 * 1024));
        assert_eq!(hw.frames[1], [0x48, 0x00, 0x00, 0x01, 0xAA, 0x87], "CMD8 is still sent");
        assert_eq!(hw.frames[3], [0x69, 0x00, 0x00, 0x00, 0x00, 0xE5], "CMD41 without HCS");
    }

    #[test]
    fn a_block_read_returns_the_bytes_and_verifies_their_crc() {
        let mut hw = FakeSpi::v2_card();
        let mut card = SdCard::init(&mut hw, 0, 5).expect("a scripted card initialises");
        let data = card.read_block(&mut hw, 3).expect("block 3 reads");

        assert_eq!(data.len(), 512);
        assert_eq!(data, hw.block, "the block came through byte for byte");
        // On an SDHC card the argument is the block index itself.
        assert_eq!(hw.frames.last().expect("the CMD17 frame")[..5], [0x51, 0x00, 0x00, 0x00, 0x03]);
        // The reply that carries it is small: 512 bytes -> 684 base64 characters.
        let encoded = b64_encode(&data);
        assert_eq!(encoded.len(), 684);
        let mut decoded = Vec::new();
        b64_decode(&encoded, &mut decoded).expect("the encoding round-trips");
        assert_eq!(decoded, data);
    }

    #[test]
    fn a_byte_addressed_card_takes_a_byte_offset_for_the_same_block() {
        let mut hw = FakeSpi::v1_card();
        let mut card = SdCard::init(&mut hw, 0, 5).expect("a scripted card initialises");
        card.read_block(&mut hw, 3).expect("block 3 reads");
        // Block 3 on an SDSC card is byte 1536 = 0x00000600.
        assert_eq!(hw.frames.last().expect("the CMD17 frame")[..5], [0x51, 0x00, 0x00, 0x06, 0x00]);
    }

    #[test]
    fn the_tools_say_what_the_card_is_and_admit_there_is_no_filesystem() {
        let mut hw = FakeSpi::v2_card();
        hw.block[510] = 0x55;
        hw.block[511] = 0xAA;
        let mut card = SdCard::init(&mut hw, 0, 5).expect("a scripted card initialises");

        let v = card.call("sdcard_read_block", &json!({ "block": 0 }), &mut hw).expect("block 0 reads");
        assert_eq!(v["block"], 0);
        assert_eq!(v["bytes"], 512);
        assert_eq!(v["boot_signature"], true);
        assert_eq!(v["crc_ok"], true);
        let encoded = v["data_base64"].as_str().expect("base64 text");
        let mut decoded = Vec::new();
        b64_decode(encoded, &mut decoded).expect("the reply decodes");
        assert_eq!(decoded, hw.block);

        let info = card.call("sdcard_info", &json!({}), &mut hw).expect("info is cached state");
        assert_eq!(info["card_type"], "SDHC/SDXC");
        assert_eq!(info["capacity_bytes"], 8u64 * 1024 * 1024 * 1024);
        assert!(
            info["note"].as_str().expect("a note").contains("no filesystem"),
            "the tool says what it cannot do: {info}"
        );

        let status = card.call("sdcard_status", &json!({}), &mut hw).expect("status is cached state");
        assert_eq!(status["block_reads"], 1);
        assert_eq!(status["last_block"], 0);
        assert_eq!(status["block_read_failures"], 0);

        // Every description repeats the limit, because that is where a model reads it.
        for tool in card.tools() {
            assert!(
                tool.spec.description.contains("filesystem")
                    || tool.spec.description.contains("separate job"),
                "{} does not say it has no filesystem: {}",
                tool.spec.name,
                tool.spec.description
            );
        }
    }

    #[test]
    fn a_card_that_never_answers_is_a_timeout_not_a_card() {
        let mut hw = FakeSpi::v2_card();
        hw.silent = true;
        let e = SdCard::init(&mut hw, 0, 5).map(|_| ()).expect_err("a silent bus cannot initialise");
        assert_eq!(e.code, code::HARDWARE);
        assert!(e.message.contains("did not answer CMD0"), "message: {}", e.message);
    }

    #[test]
    fn a_block_whose_crc_does_not_match_is_refused() {
        let mut hw = FakeSpi::v2_card();
        hw.bad_block_crc = true;
        let mut card = SdCard::init(&mut hw, 0, 5).expect("a scripted card initialises");
        let e = card.read_block(&mut hw, 1).expect_err("a corrupted block must not be returned");
        assert_eq!(e.code, code::HARDWARE);
        assert!(e.message.contains("CRC-16"), "message: {}", e.message);
        // And the failure is visible to the next `sdcard_status`.
        assert_eq!(card.failures, 1);
        assert!(card.last_error.as_deref().unwrap_or("").contains("CRC-16"));
    }

    #[test]
    fn a_card_that_accepts_the_command_but_never_sends_data_times_out() {
        let mut hw = FakeSpi::v2_card();
        hw.withhold_token = true;
        let mut card = SdCard::init(&mut hw, 0, 5).expect("a scripted card initialises");
        let e = card.read_block(&mut hw, 0).expect_err("no token, no block");
        assert_eq!(e.code, code::HARDWARE);
        assert!(e.message.contains("start token"), "message: {}", e.message);
    }

    #[test]
    fn a_csd_structure_the_driver_does_not_know_leaves_the_capacity_unknown() {
        // Structure 0b10 is neither version 1.0 nor 2.0.
        let mut csd = [0u8; CSD_SIZE];
        csd[0] = 0x80;
        let mut hw = FakeSpi::new(csd, OCR_CCS | 0x00FF_8000, true);
        let card = SdCard::init(&mut hw, 0, 5).expect("the card still initialises");
        assert_eq!(card.capacity(), None, "an unknown structure cannot be turned into a size");
        let info = card.card_json();
        assert!(info["capacity_unknown"].as_str().expect("a reason").contains("0x02"));
    }
}
