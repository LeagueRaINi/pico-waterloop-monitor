//! Shared guts of the HWiNFO -> Pico bridge.
//!
//! Two front ends sit on top of this: `hwinfo-pico-bridge` (console, for setup and
//! diagnostics) and `hwinfo-pico-bridge-tray` (silent, with a tray icon, for
//! running at login). They differ only in how they present what `bridge::run`
//! reports.
//!
//! Reading HWiNFO itself lives in the separate `hwinfo` crate — it has no
//! dependency on anything here and is reusable on its own.

pub mod autostart;
pub mod bridge;
pub mod cli;
pub mod control;
pub mod pico;
pub mod serial;
pub mod tray;

/// What `--version` reports. The release workflow sets `BRIDGE_VERSION` to the
/// tag being built (see `build.rs`); a local build reports the crate version,
/// which is all it can honestly claim to be.
pub const BRIDGE_VERSION: &str = env!("BRIDGE_VERSION");
