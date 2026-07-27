//! Start-at-login registration, on top of the `winreg` crate.
//!
//! Uses the per-user Run key rather than a real Windows service. A service
//! runs in session 0, which has no desktop (so no tray icon) and would be
//! reading HWiNFO's shared memory across a session boundary. The Run key
//! starts the bridge in the same session as HWiNFO, needs no admin rights,
//! and is trivial to inspect or remove.

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "HwinfoPicoBridge";
pub const TRAY_EXE: &str = "hwinfo-pico-bridge-tray.exe";

/// Quote a path for a command line if it needs it.
fn quote(s: &str) -> String {
    if s.contains(' ') && !s.starts_with('"') {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// The tray binary, which lives next to whichever binary is running.
pub fn tray_exe() -> Result<String, String> {
    let here = std::env::current_exe().map_err(|e| format!("cannot determine own path: {e}"))?;
    let dir = here
        .parent()
        .ok_or_else(|| "cannot determine own directory".to_string())?;
    let tray = dir.join(TRAY_EXE);
    if !tray.exists() {
        return Err(format!(
            "{TRAY_EXE} is not next to this binary (looked in {}). \
             Build it with: cargo build --release",
            dir.display()
        ));
    }
    Ok(tray.to_string_lossy().into_owned())
}

/// The command line that will be registered: the tray binary plus `args`.
pub fn command_line(args: &[String]) -> Result<String, String> {
    let mut line = quote(&tray_exe()?);
    for arg in args {
        line.push(' ');
        line.push_str(&quote(arg));
    }
    Ok(line)
}

fn run_key(access: u32) -> Result<RegKey, String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, access)
        .map_err(|e| format!("cannot open the Run key: {e}"))
}

pub fn install(args: &[String]) -> Result<String, String> {
    let line = command_line(args)?;
    run_key(KEY_WRITE)?
        .set_value(VALUE_NAME, &line)
        .map_err(|e| format!("could not write the Run key: {e}"))?;
    Ok(line)
}

/// Returns false if there was nothing registered in the first place.
pub fn uninstall() -> Result<bool, String> {
    match run_key(KEY_WRITE)?.delete_value(VALUE_NAME) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("could not remove the Run key: {e}")),
    }
}

/// The currently registered command line, if any.
pub fn installed() -> Option<String> {
    run_key(KEY_READ).ok()?.get_value(VALUE_NAME).ok()
}
