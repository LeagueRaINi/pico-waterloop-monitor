//! Shared guts of the HWiNFO -> Pico bridge.
//!
//! Two front ends sit on top of this: `hwinfo-pico-bridge` (console, for setup and
//! diagnostics) and `hwinfo-pico-bridge-tray` (silent, with a tray icon, for
//! running at login). They differ only in how they present what `bridge::run`
//! reports.

pub mod autostart;
pub mod bridge;
pub mod cli;
pub mod control;
pub mod hwinfo;
pub mod pico;
pub mod serial;
pub mod tray;
