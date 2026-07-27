//! Bakes `firmware/` into the binaries.
//!
//! The point is that updating the Pico is something the app can do on its own:
//! one exe, no loose firmware directory to keep beside it and no chance of it
//! having drifted from the build. `--firmware <dir>` still overrides this at
//! runtime for anyone iterating on the display code.

use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Build products of a local `python -m compileall`, which CI runs over
/// `firmware/`. They are not firmware and must not reach the device.
fn skip(name: &str) -> bool {
    name == "__pycache__" || name.starts_with('.') || name.ends_with(".pyc")
}

fn collect(root: &Path) -> Vec<(String, PathBuf)> {
    WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !skip(&e.file_name().to_string_lossy()))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?;
            Some((
                rel.to_string_lossy().replace('\\', "/"),
                e.path().to_path_buf(),
            ))
        })
        .collect()
}

/// Rust string literals, for the generated table.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

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
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("firmware.rs");

    println!("cargo:rerun-if-changed={}", root.display());

    let mut files = Vec::new();
    if root.is_dir() {
        files = collect(&root);
    } else {
        println!(
            "cargo:warning=firmware/ not found at {} — the built-in copy will be \
             empty, so --firmware <dir> becomes required to deploy",
            root.display()
        );
    }

    let mut code = String::from("pub static EMBEDDED: &[(&str, &[u8])] = &[\n");
    for (rel, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        // include_bytes! takes forward slashes on Windows too, and they avoid
        // a wall of escaping in the generated file.
        let literal = path.to_string_lossy().replace('\\', "/");
        code.push_str(&format!(
            "    ({}, include_bytes!({})),\n",
            quote(rel),
            quote(&literal)
        ));
    }
    code.push_str("];\n");

    fs::write(&out, code).unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}
