//! HC-SR04 ultrasonic distance sensor (and compatible JSN-SR04T).
//!
//! Detection cannot be passive — the module answers only when pinged — so a
//! device configured by the user is kept even when the first ping times out, and
//! reported as `degraded` with the note that explains it.

use crate::device::{Device, DeviceInfo};
use crate::error::{McpError, Result};
use crate::hardware::{Hardware, PinMode};
use crate::tool::{integer, object_schema, ToolDef};
use serde_json::{json, Value};

pub struct Hcsr04 {
    pub trig: i32,
    pub echo: i32,
    id: String,
    note: String,
    status: String,
}

impl Hcsr04 {
    pub fn new(trig: i32, echo: i32) -> Self {
        Self { trig, echo, id: "hcsr04".into(), note: String::new(), status: "ok".into() }
    }

    /// Configure the pins and take one reading to decide the initial status.
    pub fn probe(&mut self, hw: &mut dyn Hardware) -> Result<()> {
        hw.gpio_config(self.trig, PinMode::Output)?;
        hw.gpio_config(self.echo, PinMode::Input)?;
        hw.gpio_write(self.trig, false)?;
        hw.delay_us(2_000);
        match self.measure(hw) {
            Ok(cm) if cm.is_finite() && cm > 0.0 => {
                self.status = "ok".into();
                self.note = format!("first ping {cm:.1} cm");
            }
            Ok(_) => {
                self.status = "degraded".into();
                self.note = "no echo returned (nothing in range?)".into();
            }
            Err(e) => {
                self.status = "degraded".into();
                self.note = format!("no echo: {}", e.message);
            }
        }
        Ok(())
    }

    /// One ping, distance in centimetres.
    pub fn measure(&mut self, hw: &mut dyn Hardware) -> Result<f32> {
        hw.gpio_write(self.trig, false)?;
        hw.delay_us(2);
        hw.gpio_write(self.trig, true)?;
        hw.delay_us(10);
        hw.gpio_write(self.trig, false)?;

        // Wait for the echo to rise (≈ 60 ms for the datasheet's 4 m maximum).
        let mut waited = 0u32;
        while !hw.gpio_read(self.echo)? {
            hw.delay_us(5);
            waited += 5;
            if waited > 60_000 {
                return Err(McpError::hardware("HC-SR04 echo never went high"));
            }
        }
        let mut width = 0u32;
        while hw.gpio_read(self.echo)? {
            hw.delay_us(5);
            width += 5;
            if width > 40_000 {
                return Err(McpError::hardware("HC-SR04 echo stuck high"));
            }
        }
        if width < 100 {
            return Err(McpError::hardware("HC-SR04 echo pulse too short"));
        }
        // Speed of sound at 20 °C: 343 m/s -> 58 µs per centimetre round trip.
        Ok(width as f32 / 58.0)
    }
}

impl Device for Hcsr04 {
    fn info(&self) -> DeviceInfo {
        let mut i = DeviceInfo::new(
            "hcsr04",
            &format!("HC-SR04 distance sensor (trig GPIO{}, echo GPIO{})", self.trig, self.echo),
            "gpio",
        );
        i.id = self.id.clone();
        i.pins = vec![self.trig, self.echo];
        i.caps = vec!["hcsr04".into(), "gpio".into()];
        i.source = "config".into();
        i.status = self.status.clone();
        i.note = self.note.clone();
        i.detail = json!({ "trig": self.trig, "echo": self.echo });
        i.tools = vec!["distance_cm".into()];
        i
    }

    fn tools(&self) -> Vec<ToolDef> {
        let id = self.id.clone();
        vec![ToolDef::new("distance_cm", "Measure distance with the HC-SR04 ultrasonic sensor")
            .title("Distance")
            .category("sensor")
            .requires(&["hcsr04"])
            .schema(object_schema(
                &[("samples", integer(1, 5, "ping this many times and report the median, default 1"))],
                &[],
            ))
            .bind(move |ctx, args| {
                let id = id.clone();
                ctx.devices.call(&id, "distance_cm", args, ctx.hw)
            })]
    }

    fn call(&mut self, tool: &str, args: &Value, hw: &mut dyn Hardware) -> Result<Value> {
        match tool {
            "distance_cm" => {
                let n = crate::json::ranged_i64(args, "samples", 1, 1, 5)? as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    match self.measure(hw) {
                        Ok(cm) => v.push(cm),
                        Err(e) if v.is_empty() => return Err(e),
                        Err(_) => {}
                    }
                    hw.delay_us(30_000);
                }
                if v.is_empty() {
                    return Err(McpError::hardware("no echo from the HC-SR04"));
                }
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median = v[v.len() / 2];
                self.status = "ok".into();
                Ok(json!({
                    "distance_cm": (median * 10.0).round() / 10.0,
                    "samples": v.iter().map(|x| (x * 10.0).round() / 10.0).collect::<Vec<f32>>(),
                }))
            }
            other => Err(McpError::not_found(format!("hcsr04 has no tool `{other}`"))),
        }
    }
}
