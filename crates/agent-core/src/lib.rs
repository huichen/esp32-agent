//! esp32-agent core: everything that is not board-specific.
//!
//! Layout, bottom-up:
//!
//! ```text
//! hardware.rs      the one trait the firmware implements (GPIO, I2C, PWM, ADC, WiFi)
//! drivers/         compiled chip drivers, one module per family
//! manifest.rs      devices + tools as JSON, validated and applied without a reflash
//! tool.rs          ToolDef/ToolCtx/ToolRegistry, capability gating
//! tools/           board, system and manifest-facing tools
//! protocol.rs      JSON-RPC 2.0 / MCP framing
//! http.rs          a small blocking HTTP/1.1 server (std only)
//! app.rs           state, dispatch, routes for /mcp and the HTML portal
//! keys.rs          every NVS key name, assembled in one place (15 bytes is the limit)
//! web.rs           the embedded console
//! virtual_board.rs an in-memory board: the simulator and the test fixture
//! ```

pub mod agent;
pub mod app;
pub mod boards;
pub mod capability;
pub mod clips;
pub mod config;
pub mod device;
pub mod drivers;
pub mod error;
pub mod font6x8;
pub mod hardware;
pub mod hash;
pub mod http;
pub mod http_client;
pub mod json;
pub mod keys;
pub mod llm;
pub mod logbuf;
pub mod manifest;
pub mod protocol;
pub mod provision;
pub mod sched;
pub mod script;
pub mod seed;
pub mod settings;
pub mod sitemap;
pub mod state;
pub mod store;
pub mod tool;
pub mod tools;
pub mod util;
pub mod virtual_board;
pub mod web;
pub mod zip;
pub mod zlib;

pub use app::{App, FwMeta, Shared};
pub use config::Config;
pub use error::{McpError, Result};
pub use hardware::Hardware;
pub use manifest::Manifest;

/// Version of the core crate, reported in `initialize` and the portal.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
