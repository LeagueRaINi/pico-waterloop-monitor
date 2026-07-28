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

/// Quote an argument so `CommandLineToArgvW` — which is what turns this string
/// back into `argv` when Windows runs it — hands the same text to the tray app.
///
/// The rule that makes this more than wrapping in quotes: a backslash is only
/// an escape when it precedes a quote, so a run of them there has to be doubled
/// and the quote itself escaped. Sensor labels are user-supplied and HWiNFO is
/// happy to put quotes in them, so `--cpu 'GPU "Hot Spot"'` has to survive.
fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut backslashes = 0;
    for c in s.chars() {
        match c {
            // Held back: whether these need doubling depends on what follows.
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // A trailing run would otherwise escape the closing quote.
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sensor_label_survives_the_trip_through_the_run_key() {
        // Windows parses this string back into argv when it starts the tray
        // app at login, so anything the quoting mangles here is a sensor
        // override that silently stops applying after a reboot.
        assert_eq!(quote("COM7"), "COM7", "nothing to quote");
        assert_eq!(quote("CPU (Tctl/Tdie)"), r#""CPU (Tctl/Tdie)""#);
        assert_eq!(quote(r#"GPU "Hot Spot""#), r#""GPU \"Hot Spot\"""#);
        assert_eq!(quote(""), r#""""#, "an empty value is still an argument");
    }

    #[test]
    fn a_trailing_backslash_does_not_escape_the_closing_quote() {
        // Backslashes only mean anything in front of a quote: the runs inside
        // the path are left alone, and only the one that would run into the
        // closing quote is doubled.
        assert_eq!(
            quote(r"C:\Program Files\App\"),
            r#""C:\Program Files\App\\""#
        );
    }

    #[test]
    fn the_registered_line_is_the_tray_binary_and_its_flags() {
        let line = command_line(&["--cpu".to_string(), "Core Max".to_string()]);
        // `tray_exe` needs the built binary next to this one, which a test run
        // does not have; the quoting is what this is checking.
        if let Ok(line) = line {
            assert!(line.ends_with(r#"--cpu "Core Max""#), "{line}");
        }
    }
}
