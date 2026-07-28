//! Build-time stamping, and the rebuild trigger for the baked-in firmware.
//!
//! `include_dir!` in `pico::firmware` is what actually embeds `firmware/`, and
//! `include_bytes!` under it makes rustc rebuild when a file's *contents*
//! change. It cannot see a file being added or removed, though — the macro
//! would need nightly's `track_path` for that — so the directory is declared
//! here instead, which cargo scans recursively.

use std::path::PathBuf;

/// What `--version` reports. The release workflow sets `BRIDGE_VERSION` to the
/// tag being built; a local build reports the crate version, which is all it
/// can honestly claim to be.
fn stamp_version() {
    println!("cargo:rerun-if-env-changed=BRIDGE_VERSION");
    let version = std::env::var("BRIDGE_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=BRIDGE_VERSION={version}");
}

fn main() {
    stamp_version();

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../../firmware");
    println!("cargo:rerun-if-changed={}", root.display());
}
