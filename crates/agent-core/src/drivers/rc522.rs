//! MFRC522 (and the pin-compatible clones) 13.56 MHz RFID reader: SPI plus a
//! GPIO reset line.
//!
//! This is the one SPI part in the tree that can identify itself: register 0x37
//! holds a version byte, and a floating MISO reads back 0x00 or 0xFF instead.
//! Those two are rejected, the documented values are accepted, and the two ids
//! the common clones report are kept as clones rather than turned away — they run
//! the same command set, and a reader that refused them would work on the bench
//! and not on the shelf.
//!
//! A card is read the ISO 14443-3 way: REQA with a 7-bit frame, anticollision
//! level 1 for the UID and its check byte, then SELECT with the CRC_A the chip's
//! own coprocessor computes. Every SPI byte is a register address (left-shifted
//! one place, bit 0 of the shifted value marks a read) followed by the data.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::tool::{object_schema, ToolDef};
use serde_json::{json, Value};

// Register map, datasheet chapter 9. Only what this driver touches.
const REG_COMMAND: u8 = 0x01;
const REG_COM_IRQ: u8 = 0x04;
const REG_FIFO_DATA: u8 = 0x09;
const REG_FIFO_LEVEL: u8 = 0x0A;
const REG_BIT_FRAMING: u8 = 0x0D;
const REG_COLL: u8 = 0x0E;
const REG_MODE: u8 = 0x11;
const REG_TX_CONTROL: u8 = 0x14;
const REG_TX_ASK: u8 = 0x15;
const REG_CRC_RESULT_H: u8 = 0x21;
const REG_CRC_RESULT_L: u8 = 0x22;
const REG_T_MODE: u8 = 0x2A;
const REG_T_PRESCALER: u8 = 0x2B;
const REG_T_RELOAD_H: u8 = 0x2C;
const REG_T_RELOAD_L: u8 = 0x2D;
const REG_VERSION: u8 = 0x37;

const CMD_IDLE: u8 = 0x00;
const CMD_CALC_CRC: u8 = 0x03;
const CMD_TRANSCEIVE: u8 = 0x0C;
const CMD_SOFT_RESET: u8 = 0x0F;

const IRQ_TIMER: u8 = 0x01;
const IRQ_ERR: u8 = 0x02;
const IRQ_RX: u8 = 0x20;

const CMD_REQA: u8 = 0x26;
const CMD_SELECT_CL1: u8 = 0x93;
const NVB_ANTICOLLISION: u8 = 0x20;
const NVB_SELECT: u8 = 0x70;

/// Version-register values this driver accepts, with the name it reports.
///
/// 0x91 and 0x92 are the genuine v1.0 and v2.0. 0x12 and 0x88 are what the two
/// widespread clones answer: same command set, so they are accepted and labelled.
pub const KNOWN_VERSIONS: [(u8, &str); 4] =
    [(0x91, "MFRC522 v1.0"), (0x92, "MFRC522 v2.0"), (0x12, "MFRC522 clone"), (0x88, "MFRC522 clone")];

/// How long a card has to answer. ISO 14443-3 allows a few hundred microseconds
/// of frame delay; a millisecond-granularity poll of 30 ms is generous and keeps
/// the tool from holding the SPI bus.
const CARD_TIMEOUT_MS: u32 = 30;

/// Milliseconds, from the microsecond delay the trait offers.
fn delay_ms(hw: &mut dyn Hardware, ms: u32) {
    hw.delay_us(ms.saturating_mul(1_000));
}

pub struct Rc522 {
    pub bus: u8,
    pub cs: i32,
    pub rst: i32,
    version: u8,
    id: String,
}

/// One card, as the anticollision and select steps described it.
#[derive(Debug, Clone)]
pub struct Card {
    pub uid: Vec<u8>,
    pub bcc: u8,
    pub atqa: [u8; 2],
    /// Select acknowledge, when the SELECT step completed.
    pub sak: Option<u8>,
    /// Empty when every step succeeded; explains a card that answered and then
    /// stopped, which happens when it leaves the field mid-sequence.
    pub note: String,
}

impl Card {
    pub fn uid_hex(&self) -> String {
        self.uid.iter().map(|b| format!("{b:02X}")).collect()
    }

    /// The card family the SAK names, for the handful that matter.
    pub fn kind(&self) -> &'static str {
        match self.sak {
            Some(0x00) => "MIFARE Ultralight / NTAG",
            Some(0x08) => "MIFARE Classic 1K",
            Some(0x09) => "MIFARE Mini",
            Some(0x18) => "MIFARE Classic 4K",
            Some(0x20) => "ISO 14443-4 (DESFire, Plus, ...)",
            Some(0x28) => "ISO 18092 (NFC)",
            Some(_) => "other ISO 14443-A card",
            None => "unknown (the SELECT step did not complete)",
        }
    }
}

impl Rc522 {
    fn bare(bus: u8, cs: i32, rst: i32) -> Self {
        Self { bus, cs, rst, version: 0, id: "rc522".into() }
    }

    /// The version register, which is also the identification: `None` when the
    /// byte is not one of [`KNOWN_VERSIONS`].
    pub fn identify(hw: &mut dyn Hardware, bus: u8, cs: i32) -> Option<u8> {
        let mut dev = Self::bare(bus, cs, -1);
        match dev.version(hw) {
            Ok(v) if KNOWN_VERSIONS.iter().any(|(known, _)| *known == v) => Some(v),
            _ => None,
        }
    }

    pub fn probe(hw: &mut dyn Hardware, bus: u8, cs: i32, rst: i32, id_suffix: usize) -> Result<Self> {
        let mut dev = Self::bare(bus, cs, rst);
        if id_suffix > 1 {
            dev.id = format!("rc522_{id_suffix}");
        }
        // Hard reset: RST is active low, and the oscillator then needs 50 ms
        // before the registers answer (datasheet, reset and oscillator start-up).
        hw.gpio_config(rst, PinMode::Output)?;
        hw.gpio_write(rst, false)?;
        delay_ms(hw, 1);
        hw.gpio_write(rst, true)?;
        delay_ms(hw, 50);

        let version = dev
            .version(hw)
            .map_err(|e| McpError::not_found(format!("no MFRC522 on CS GPIO{cs}: {}", e.message)))?;
        if !KNOWN_VERSIONS.iter().any(|(known, _)| *known == version) {
            return Err(McpError::not_found(format!(
                "no MFRC522 on CS GPIO{cs}: version register reads 0x{version:02X}"
            )));
        }
        dev.version = version;
        dev.configure(hw)?;
        dev.antenna_on(hw)?;
        Ok(dev)
    }

    /// The datasheet's start-up table, then the reader is ready to transmit.
    fn configure(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        self.write_reg(hw, REG_COMMAND, CMD_SOFT_RESET)?;
        // The datasheet allows 50 ms for the reset to reload the register defaults.
        delay_ms(hw, 50);
        // The part clears the command itself once the reset has run; the bit that
        // says so is PowerDown in the same register.
        let mut waited = 0u32;
        while self.read_reg(hw, REG_COMMAND)? & 0x10 != 0 {
            if waited >= 50 {
                return Err(McpError::hardware("MFRC522 stayed in power-down after a soft reset"));
            }
            delay_ms(hw, 1);
            waited += 1;
        }
        // 25 us timer, 100 % ASK, CRC preset 0x6363.
        self.write_reg(hw, REG_T_MODE, 0x8D)?;
        self.write_reg(hw, REG_T_PRESCALER, 0x3E)?;
        self.write_reg(hw, REG_T_RELOAD_H, 0x00)?;
        self.write_reg(hw, REG_T_RELOAD_L, 0x1E)?;
        self.write_reg(hw, REG_TX_ASK, 0x40)?;
        self.write_reg(hw, REG_MODE, 0x3D)?;
        Ok(())
    }

    fn antenna_on(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        let value = self.read_reg(hw, REG_TX_CONTROL)?;
        if value & 0x03 != 0x03 {
            self.write_reg(hw, REG_TX_CONTROL, value | 0x03)?;
        }
        Ok(())
    }

    pub fn version(&mut self, hw: &mut dyn Hardware) -> Result<u8> {
        self.read_reg(hw, REG_VERSION)
    }

    pub fn chip_name(&self) -> &'static str {
        KNOWN_VERSIONS.iter().find(|(v, _)| *v == self.version).map(|(_, name)| *name).unwrap_or("unknown")
    }

    // --- SPI framing -------------------------------------------------------

    /// One chip-select window. Chip select is driven here rather than left to the
    /// bus so the pin shows up in the manifest like every other one.
    fn transfer(&self, hw: &mut dyn Hardware, tx: &[u8]) -> Result<Vec<u8>> {
        hw.gpio_config(self.cs, PinMode::Output)?;
        hw.gpio_write(self.cs, false)?;
        let rx = hw.spi_transfer(self.bus, tx);
        // Release the part even if the transfer failed.
        hw.gpio_write(self.cs, true)?;
        let rx = rx?;
        if rx.len() != tx.len() {
            return Err(McpError::hardware(format!(
                "SPI returned {} byte(s) for the {} sent",
                rx.len(),
                tx.len()
            )));
        }
        Ok(rx)
    }

    fn read_reg(&mut self, hw: &mut dyn Hardware, reg: u8) -> Result<u8> {
        // Bit 7 of the address byte marks a read; the value arrives on the second
        // byte, while the first one shifts the address out.
        let rx = self.transfer(hw, &[address_byte(reg, true), 0x00])?;
        Ok(rx[1])
    }

    fn read_regs(&mut self, hw: &mut dyn Hardware, reg: u8, out: &mut [u8]) -> Result<()> {
        let mut tx = vec![address_byte(reg, true)];
        tx.resize(out.len() + 1, 0x00);
        let rx = self.transfer(hw, &tx)?;
        out.copy_from_slice(&rx[1..]);
        Ok(())
    }

    fn write_reg(&mut self, hw: &mut dyn Hardware, reg: u8, value: u8) -> Result<()> {
        self.transfer(hw, &[address_byte(reg, false), value])?;
        Ok(())
    }

    /// A burst write, which is how the FIFO is loaded.
    fn write_regs(&mut self, hw: &mut dyn Hardware, reg: u8, data: &[u8]) -> Result<()> {
        let mut tx = Vec::with_capacity(data.len() + 1);
        tx.push(address_byte(reg, false));
        tx.extend_from_slice(data);
        self.transfer(hw, &tx)?;
        Ok(())
    }

    // --- ISO 14443-3 steps -------------------------------------------------

    /// Send a frame and collect the answer. `None` means the card did not answer
    /// (the timer fired or the frame was rejected) — that is a normal result for
    /// an empty field, not a bus failure.
    fn transceive(
        &mut self,
        hw: &mut dyn Hardware,
        payload: &[u8],
        tx_last_bits: u8,
    ) -> Result<Option<Vec<u8>>> {
        self.write_reg(hw, REG_COMMAND, CMD_IDLE)?;
        self.write_reg(hw, REG_COM_IRQ, 0x7F)?; // clear every interrupt flag
        self.write_reg(hw, REG_FIFO_LEVEL, 0x80)?; // flush the FIFO
        self.write_regs(hw, REG_FIFO_DATA, payload)?;
        self.write_reg(hw, REG_BIT_FRAMING, tx_last_bits)?;
        self.write_reg(hw, REG_COMMAND, CMD_TRANSCEIVE)?;
        // TxLastBits is set, so the next write starts the transmission.
        let framing = tx_last_bits | 0x80;
        self.write_reg(hw, REG_BIT_FRAMING, framing)?;

        let mut waited = 0u32;
        loop {
            let irq = self.read_reg(hw, REG_COM_IRQ)?;
            if irq & IRQ_RX != 0 {
                break;
            }
            if irq & (IRQ_TIMER | IRQ_ERR) != 0 {
                self.write_reg(hw, REG_COMMAND, CMD_IDLE)?;
                return Ok(None);
            }
            if waited >= CARD_TIMEOUT_MS {
                self.write_reg(hw, REG_COMMAND, CMD_IDLE)?;
                return Ok(None);
            }
            delay_ms(hw, 1);
            waited += 1;
        }
        self.write_reg(hw, REG_COMMAND, CMD_IDLE)?;
        let level = self.read_reg(hw, REG_FIFO_LEVEL)? & 0x7F;
        let mut reply = vec![0u8; level as usize];
        if !reply.is_empty() {
            self.read_regs(hw, REG_FIFO_DATA, &mut reply)?;
        }
        Ok(Some(reply))
    }

    /// CRC_A from the chip's coprocessor: the datasheet's own path for a SELECT
    /// frame, and one fewer algorithm to get wrong here.
    fn crc_a(&mut self, hw: &mut dyn Hardware, data: &[u8]) -> Result<[u8; 2]> {
        self.write_reg(hw, REG_COMMAND, CMD_IDLE)?;
        self.write_reg(hw, REG_FIFO_LEVEL, 0x80)?;
        self.write_regs(hw, REG_FIFO_DATA, data)?;
        self.write_reg(hw, REG_COMMAND, CMD_CALC_CRC)?;
        let mut waited = 0u32;
        while self.read_reg(hw, REG_COMMAND)? & 0x0F != 0 {
            if waited >= 50 {
                return Err(McpError::hardware("MFRC522 CRC coprocessor did not finish"));
            }
            delay_ms(hw, 1);
            waited += 1;
        }
        let hi = self.read_reg(hw, REG_CRC_RESULT_H)?;
        let lo = self.read_reg(hw, REG_CRC_RESULT_L)?;
        Ok([hi, lo])
    }

    /// REQA, anticollision level 1 and SELECT. `Ok(None)` is an empty field.
    pub fn read_uid(&mut self, hw: &mut dyn Hardware) -> Result<Option<Card>> {
        // Values after collision: cleared, so a collision does not leave a stale
        // register behind.
        self.write_reg(hw, REG_COLL, 0x00)?;
        let Some(atqa) = self.transceive(hw, &[CMD_REQA], 7)? else {
            return Ok(None);
        };
        if atqa.len() < 2 {
            return Err(McpError::hardware(format!(
                "a card answered REQA with {} byte(s); an ATQA is two",
                atqa.len()
            )));
        }

        // Anticollision CL1: four UID bytes plus the check byte, which is their XOR.
        self.write_reg(hw, REG_COLL, 0x00)?;
        let Some(answer) = self.transceive(hw, &[CMD_SELECT_CL1, NVB_ANTICOLLISION], 0)? else {
            return Ok(None);
        };
        if answer.len() < 5 {
            return Err(McpError::hardware(format!(
                "anticollision returned {} byte(s); a single-size UID is five",
                answer.len()
            )));
        }
        let uid = answer[..4].to_vec();
        let bcc = answer[4];
        let xored = uid.iter().fold(0u8, |acc, byte| acc ^ byte);
        if xored != bcc {
            return Err(McpError::hardware(format!(
                "UID check byte is 0x{bcc:02X}, the four UID bytes XOR to 0x{xored:02X}"
            )));
        }

        // SELECT CL1: the frame carries its own CRC_A, which the chip computes.
        let mut select = vec![CMD_SELECT_CL1, NVB_SELECT];
        select.extend_from_slice(&uid);
        select.push(bcc);
        let crc = self.crc_a(hw, &select)?;
        select.extend_from_slice(&crc);

        let mut card = Card { uid, bcc, atqa: [atqa[0], atqa[1]], sak: None, note: String::new() };
        match self.transceive(hw, &select, 0)? {
            Some(sak) if !sak.is_empty() => card.sak = Some(sak[0]),
            Some(_) => card.note = "SELECT was acknowledged with an empty frame".into(),
            None => card.note = "the card stopped answering during SELECT".into(),
        }
        Ok(Some(card))
    }
}

impl Device for Rc522 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "rc522",
            &format!("{} RFID reader on SPI{} CS GPIO{}", self.chip_name(), self.bus, self.cs),
            "spi",
        );
        i.id = self.id.clone();
        i.pins = vec![self.cs, self.rst];
        i.caps = vec!["rc522".into(), "rfid".into(), "spi".into()];
        i.source = "config".into();
        i.detail = json!({
            "version": format!("0x{:02X}", self.version),
            "chip": self.chip_name(),
            "bus": self.bus,
            "reset_pin": self.rst,
        });
        i.tools = vec!["rc522_uid".into(), "rc522_version".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        let version = id.clone();
        vec![
            ToolDef::new(
                "rc522_uid",
                "Read the UID of the RFID card in the field, with its select acknowledge. Reports found=false when no card is present",
            )
            .title("RFID UID")
            .category("sensor")
            .requires(&["rc522"])
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "rc522_uid", args, ctx.hw)
            }),
            ToolDef::new(
                "rc522_version",
                "Read the MFRC522 version register (0x37) and the chip name it maps to",
            )
            .title("RFID version")
            .category("sensor")
            .requires(&["rc522"])
            .schema(object_schema(&[], &[]))
            .bind(move |ctx, args| {
                let id = version.clone();
                ctx.devices.call(&id, "rc522_version", args, ctx.hw)
            }),
        ]
    }

    fn call(&mut self, tool: &str, _args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "rc522_uid" => match self.read_uid(hw)? {
                Some(card) => Ok(json!({
                    "found": true,
                    "uid": card.uid_hex(),
                    "uid_bytes": card.uid,
                    "bcc": format!("0x{:02X}", card.bcc),
                    "atqa": format!("0x{:02X}{:02X}", card.atqa[0], card.atqa[1]),
                    "sak": card.sak.map(|s| format!("0x{s:02X}")),
                    "type": card.kind(),
                    "selected": card.sak.is_some(),
                    "note": card.note,
                })),
                None => Ok(json!({
                    "found": false,
                    "note": "no card in the field",
                })),
            },
            "rc522_version" => {
                let v = self.version(hw)?;
                Ok(json!({
                    "version": format!("0x{v:02X}"),
                    "chip": KNOWN_VERSIONS
                        .iter()
                        .find(|(known, _)| *known == v)
                        .map(|(_, name)| *name)
                        .unwrap_or("unknown"),
                    "known": KNOWN_VERSIONS.iter().any(|(known, _)| *known == v),
                    "register": "0x37",
                }))
            }
            other => Err(McpError::not_found(format!("rc522 has no tool `{other}`"))),
        }
    }
}

/// The SPI address byte: the register shifted one place, with bit 7 marking a read
/// (datasheet, SPI framing). Bit 0 of the shifted value is unused and stays clear.
fn address_byte(reg: u8, read: bool) -> u8 {
    ((reg << 1) & 0x7E) | if read { 0x80 } else { 0x00 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::BoardInfo;

    const CS: i32 = 10;
    const RST: i32 = 9;
    const UID: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
    const BCC: u8 = 0x22; // DE ^ AD ^ BE ^ EF, the ISO 14443-3 check byte
    const CRC: [u8; 2] = [0xC3, 0x55]; // whatever the coprocessor returns in this fake
    const SAK: u8 = 0x08; // MIFARE Classic 1K

    /// The reader end of the wire: a register file, a FIFO and a scripted card.
    /// It answers a Transceive command by looking at what was loaded into the
    /// FIFO, which is exactly the step the driver's frames have to get right.
    struct FakeReader {
        regs: [u8; 64],
        fifo: Vec<u8>,
        version: u8,
        card: bool,
        card_bcc: u8,
        cs: Vec<bool>,
        rst: Vec<bool>,
        transfers: Vec<Vec<u8>>,
        /// Register writes in order, for the start-up assertion.
        writes: Vec<(u8, u8)>,
        /// FIFO bursts, in order: the frames the driver sent.
        frames: Vec<Vec<u8>>,
    }

    impl FakeReader {
        fn with_version(version: u8) -> Self {
            Self {
                regs: [0u8; 64],
                fifo: Vec::new(),
                version,
                card: true,
                card_bcc: BCC,
                cs: Vec::new(),
                rst: Vec::new(),
                transfers: Vec::new(),
                writes: Vec::new(),
                frames: Vec::new(),
            }
        }

        fn working() -> Self {
            Self::with_version(0x92)
        }

        /// What the card answers, or `None` when it does not answer at all.
        fn card_answer(&self) -> Option<Vec<u8>> {
            if !self.card {
                return None;
            }
            let req = &self.fifo;
            if req.as_slice() == [CMD_REQA] {
                return Some(vec![0x04, 0x00]);
            }
            if req.as_slice() == [CMD_SELECT_CL1, NVB_ANTICOLLISION] {
                let mut answer = UID.to_vec();
                answer.push(self.card_bcc);
                return Some(answer);
            }
            if req.len() >= 2 && req[0] == CMD_SELECT_CL1 && req[1] == NVB_SELECT {
                return Some(vec![SAK, CRC[0], CRC[1]]);
            }
            None
        }

        fn read_byte(&mut self, reg: u8) -> u8 {
            match reg {
                REG_FIFO_DATA => {
                    if self.fifo.is_empty() {
                        0
                    } else {
                        self.fifo.remove(0)
                    }
                }
                REG_FIFO_LEVEL => self.fifo.len() as u8,
                REG_VERSION => self.version,
                other => self.regs[other as usize],
            }
        }

        fn write_byte(&mut self, reg: u8, value: u8) {
            match reg {
                REG_FIFO_DATA => {
                    self.fifo.push(value);
                }
                REG_FIFO_LEVEL if value & 0x80 != 0 => self.fifo.clear(),
                REG_COM_IRQ => self.regs[REG_COM_IRQ as usize] &= !value,
                REG_COMMAND => {
                    self.writes.push((reg, value));
                    match value & 0x0F {
                        CMD_TRANSCEIVE => {
                            let answer = self.card_answer();
                            self.fifo = answer.unwrap_or_default();
                            // RxIRq for an answer, TimerIRq for an empty field.
                            self.regs[REG_COM_IRQ as usize] =
                                if self.fifo.is_empty() { IRQ_TIMER } else { IRQ_RX };
                            self.regs[REG_COMMAND as usize] = CMD_IDLE;
                        }
                        CMD_CALC_CRC => {
                            self.regs[REG_CRC_RESULT_H as usize] = CRC[0];
                            self.regs[REG_CRC_RESULT_L as usize] = CRC[1];
                            self.regs[REG_COMMAND as usize] = CMD_IDLE;
                        }
                        CMD_SOFT_RESET => self.regs[REG_COMMAND as usize] = CMD_IDLE,
                        _ => self.regs[REG_COMMAND as usize] = value & 0x0F,
                    }
                }
                other => {
                    self.writes.push((other, value));
                    self.regs[other as usize] = value;
                }
            }
        }
    }

    impl Hardware for FakeReader {
        fn board_info(&mut self) -> BoardInfo {
            BoardInfo { soc: "fake".into(), ..Default::default() }
        }

        fn gpio_config(&mut self, _pin: i32, _mode: PinMode) -> Result<()> {
            Ok(())
        }

        fn gpio_write(&mut self, pin: i32, high: bool) -> Result<()> {
            if pin == CS {
                self.cs.push(high);
            }
            if pin == RST {
                self.rst.push(high);
            }
            Ok(())
        }

        fn spi_transfer(&mut self, _bus: u8, data: &[u8]) -> Result<Vec<u8>> {
            self.transfers.push(data.to_vec());
            let addr = data[0];
            let reg = (addr >> 1) & 0x3F;
            let read = addr & 0x80 != 0;
            let mut rx = vec![0u8; data.len()];
            if read {
                for (offset, slot) in rx.iter_mut().enumerate().skip(1) {
                    // The FIFO pointer advances on every byte; the register file
                    // advances its address for the other bursts.
                    let at = if reg == REG_FIFO_DATA { REG_FIFO_DATA } else { reg + offset as u8 - 1 };
                    *slot = self.read_byte(at);
                }
            } else {
                if reg == REG_FIFO_DATA && data.len() > 1 {
                    self.frames.push(data[1..].to_vec());
                }
                for (offset, value) in data.iter().enumerate().skip(1) {
                    let at = if reg == REG_FIFO_DATA { REG_FIFO_DATA } else { reg + offset as u8 - 1 };
                    self.write_byte(at, *value);
                }
            }
            Ok(rx)
        }
    }

    fn probed(hw: &mut FakeReader) -> Rc522 {
        Rc522::probe(hw, 0, CS, RST, 1).expect("a reader")
    }

    #[test]
    fn a_register_read_sets_bit_seven_of_the_shifted_address() {
        let mut hw = FakeReader::working();
        let mut dev = Rc522::bare(0, CS, RST);
        assert_eq!(dev.version(&mut hw).unwrap(), 0x92);
        // 0x37 << 1 = 0x6E, with the read bit set = 0xEE. The value arrives on the
        // second byte, so a one-byte transfer would return the address echo.
        assert_eq!(hw.transfers[0], vec![0xEE, 0x00]);
        dev.write_reg(&mut hw, REG_T_MODE, 0x8D).unwrap();
        assert_eq!(hw.transfers[1], vec![0x54, 0x8D], "0x2A << 1 with the read bit clear");
        assert_eq!(hw.cs, vec![false, true, false, true]);
    }

    #[test]
    fn the_reset_pulse_and_startup_table_are_the_datasheet_ones() {
        let mut hw = FakeReader::working();
        probed(&mut hw);
        assert_eq!(hw.rst, vec![false, true], "RST is active low: low, then released");
        let table: Vec<(u8, u8)> = hw
            .writes
            .iter()
            .copied()
            .filter(|(reg, _)| *reg != REG_COMMAND && *reg != REG_COM_IRQ && *reg != REG_FIFO_LEVEL)
            .collect();
        assert_eq!(
            table,
            vec![
                (REG_T_MODE, 0x8D),
                (REG_T_PRESCALER, 0x3E),
                (REG_T_RELOAD_H, 0x00),
                (REG_T_RELOAD_L, 0x1E),
                (REG_TX_ASK, 0x40),
                (REG_MODE, 0x3D),
                (REG_TX_CONTROL, 0x03),
            ],
            "timer, 100 % ASK, CRC preset, then the antenna on"
        );
    }

    #[test]
    fn a_uid_read_sends_reqa_anticollision_and_a_crc_terminated_select() {
        let mut hw = FakeReader::working();
        let mut dev = probed(&mut hw);
        let card = dev.read_uid(&mut hw).unwrap().expect("a card");
        assert_eq!(card.uid, UID.to_vec());
        assert_eq!(card.uid_hex(), "DEADBEEF");
        assert_eq!(card.bcc, BCC);
        assert_eq!(card.atqa, [0x04, 0x00]);
        assert_eq!(card.sak, Some(SAK));
        assert_eq!(card.kind(), "MIFARE Classic 1K");
        assert!(card.note.is_empty());

        // Every FIFO burst is one frame the driver sent. The CRC step loads the
        // SELECT payload once and the transmission carries it again with the CRC.
        assert_eq!(hw.frames[0], vec![0x26], "REQA, a 7-bit short frame");
        assert_eq!(hw.frames[1], vec![0x93, 0x20], "anticollision level 1");
        let selects: Vec<&Vec<u8>> = hw.frames.iter().filter(|f| f.starts_with(&[0x93, 0x70])).collect();
        assert_eq!(selects.len(), 2, "once for the CRC coprocessor, once over the air");
        assert_eq!(selects[0].len(), 7, "the CRC is computed over the frame without it");
        assert_eq!(
            *selects[1],
            vec![0x93, 0x70, 0xDE, 0xAD, 0xBE, 0xEF, 0x22, 0xC3, 0x55],
            "SELECT carries the UID, its check byte and the CRC_A from the coprocessor"
        );
    }

    #[test]
    fn an_empty_field_is_an_answer_not_an_error() {
        let mut hw = FakeReader::working();
        hw.card = false;
        let mut dev = probed(&mut hw);
        assert!(dev.read_uid(&mut hw).unwrap().is_none());
        let out = dev.call("rc522_uid", &json!({}), &mut hw).unwrap();
        assert_eq!(out["found"], false);
        assert!(out["note"].as_str().unwrap().contains("no card"));
    }

    #[test]
    fn a_wrong_uid_check_byte_is_rejected() {
        let mut hw = FakeReader::working();
        hw.card_bcc = 0x00;
        let mut dev = probed(&mut hw);
        let e = dev.read_uid(&mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::HARDWARE);
        assert!(e.message.contains("check byte"), "{}", e.message);
    }

    #[test]
    fn a_floating_bus_is_not_an_mfrc522() {
        for version in [0x00u8, 0xFF, 0x55] {
            let mut hw = FakeReader::with_version(version);
            assert_eq!(Rc522::identify(&mut hw, 0, CS), None, "version 0x{version:02X}");
            let e = Rc522::probe(&mut hw, 0, CS, RST, 1).err().expect("the probe must fail");
            assert!(e.message.contains("no MFRC522"), "{}", e.message);
        }
        assert_eq!(Rc522::identify(&mut FakeReader::working(), 0, CS), Some(0x92));
        assert_eq!(Rc522::identify(&mut FakeReader::with_version(0x88), 0, CS), Some(0x88));
    }

    #[test]
    fn the_version_tool_reports_the_register_byte() {
        let mut hw = FakeReader::working();
        let mut dev = probed(&mut hw);
        let out = dev.call("rc522_version", &json!({}), &mut hw).unwrap();
        assert_eq!(out["version"], "0x92");
        assert_eq!(out["chip"], "MFRC522 v2.0");
        assert_eq!(out["known"], true);
        let e = dev.call("rc522_wait", &json!({}), &mut hw).unwrap_err();
        assert_eq!(e.code, crate::error::code::NOT_FOUND);
    }
}
