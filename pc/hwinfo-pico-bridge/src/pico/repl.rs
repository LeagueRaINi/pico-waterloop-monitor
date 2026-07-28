//! MicroPython's raw REPL, over the Pico's USB CDC port.
//!
//! This is the same protocol `mpremote` and Thonny use, and it is why updating
//! the device needs nothing installed: MicroPython is already listening on the
//! port the bridge writes to.
//!
//! ```text
//!   Ctrl-C Ctrl-C   interrupt whatever main.py is doing
//!   Ctrl-A          enter raw mode      -> "raw REPL; CTRL-B to exit\r\n>"
//!   <code> Ctrl-D   run it              -> "OK" <stdout> \x04 <stderr> \x04 ">"
//!   Ctrl-B          leave raw mode
//!   Ctrl-D          soft reset, so main.py starts again
//! ```
//!
//! Globals persist between `exec` calls, which is what lets a file be opened
//! once and written in chunks.

use crate::serial::Port;
use std::time::{Duration, Instant};

const CTRL_A: u8 = 0x01;
const CTRL_B: u8 = 0x02;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;

const BANNER: &[u8] = b"raw REPL; CTRL-B to exit";
/// End of a response: the stderr terminator followed by a fresh prompt.
const DONE: &[u8] = b"\x04>";

/// Long enough for a font table to be hashed on device, short enough that a
/// wedged Pico does not hang the tray.
const EXEC_TIMEOUT: Duration = Duration::from_secs(20);
/// Tries at getting the device into raw mode, at roughly 2.5 s each.
const HANDSHAKE_ATTEMPTS: usize = 4;
/// A response this large means we are talking to something that is not a raw
/// REPL, and reading it to the end would only make things worse.
const MAX_RESPONSE: usize = 512 * 1024;

pub struct Repl {
    port: Port,
    /// Bytes read past the end of the last response. Keeping them means a
    /// device that answers faster than we look cannot desynchronise us.
    pending: Vec<u8>,
}

impl Repl {
    /// The one entry point that reports a typed error: whether the port was
    /// merely busy decides what the caller tells the user to do about it.
    pub fn open(name: &str) -> Result<Repl, crate::serial::Error> {
        // Short enough that `read_until` can check its own, much longer,
        // deadline; long enough that writing a full chunk to a device that is
        // briefly busy is not mistaken for a failure.
        let port = Port::open_with_timeout(name, Duration::from_millis(500))?;
        Ok(Repl {
            port,
            pending: Vec::new(),
        })
    }

    pub fn port_name(&self) -> &str {
        &self.port.name
    }

    /// Read until `marker` appears; return everything up to and including it,
    /// keeping the remainder for the next call.
    fn read_until(&mut self, marker: &[u8], timeout: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + timeout;
        let mut buf = std::mem::take(&mut self.pending);
        let mut searched = 0usize;
        let mut chunk = [0u8; 1024];

        loop {
            // Overlap the search by the marker length so one that arrives
            // split across two reads is still found.
            let from = searched.saturating_sub(marker.len().saturating_sub(1));
            if let Some(at) = memchr::memmem::find(&buf[from..], marker) {
                let end = from + at + marker.len();
                self.pending = buf.split_off(end);
                return Ok(buf);
            }
            searched = buf.len();

            let n = self.port.read_some(&mut chunk)?;
            if n > 0 {
                if buf.len() + n > MAX_RESPONSE {
                    return Err(format!(
                        "{} sent more than {} KB without finishing — it does not \
                         look like a MicroPython raw REPL",
                        self.port.name,
                        MAX_RESPONSE / 1024
                    ));
                }
                buf.extend_from_slice(&chunk[..n]);
                continue;
            }
            if Instant::now() >= deadline {
                let tail = String::from_utf8_lossy(&buf[buf.len().saturating_sub(120)..])
                    .trim()
                    .to_string();
                return Err(format!(
                    "timed out waiting for {}. Last thing it said: {tail:?}",
                    self.port.name
                ));
            }
        }
    }

    /// Interrupt whatever the device is running and put it in raw mode.
    ///
    /// Retried rather than waited out, because the case this exists for is a
    /// Pico still coming back from the soft reset at the end of the *previous*
    /// deploy: while MicroPython is reinitialising it discards whatever is in
    /// the CDC buffer, so the Ctrl-A simply vanishes and no amount of further
    /// waiting produces a banner. A device that is merely running `main.py`
    /// answers on the first attempt, so this costs nothing in the normal case.
    pub fn enter(&mut self) -> Result<(), String> {
        let mut last = String::new();
        for attempt in 0..HANDSHAKE_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(400));
            }
            match self.handshake() {
                Ok(()) => return Ok(()),
                Err(err) => last = err,
            }
        }
        Err(format!(
            "{last}\nThe device did not answer as MicroPython. Is MicroPython \
             flashed, and is anything else holding the port?"
        ))
    }

    fn handshake(&mut self) -> Result<(), String> {
        // Twice: the first breaks out of a sleep, the second out of a loop
        // that swallowed the first.
        for _ in 0..2 {
            self.port.write_bytes(&[CTRL_C])?;
            std::thread::sleep(Duration::from_millis(60));
        }
        self.port.discard_input();
        self.pending.clear();

        self.port.write_bytes(&[CTRL_A])?;
        self.read_until(BANNER, Duration::from_secs(2))?;
        // Swallow the prompt that follows the banner, so the first exec is not
        // matched against it.
        self.read_until(b">", Duration::from_secs(2))?;
        Ok(())
    }

    /// Run `code` on the device and return what it printed.
    pub fn exec(&mut self, code: &str) -> Result<String, String> {
        self.exec_for(code, EXEC_TIMEOUT)
    }

    pub fn exec_for(&mut self, code: &str, timeout: Duration) -> Result<String, String> {
        self.port.write_bytes(code.as_bytes())?;
        self.port.write_bytes(&[CTRL_D])?;

        let raw = self.read_until(DONE, timeout)?;
        // "OK" <stdout> \x04 <stderr> \x04 ">"
        let body = match memchr::memmem::find(&raw, b"OK") {
            Some(at) => &raw[at + 2..],
            None => &raw[..],
        };
        let mut parts = body.split(|&b| b == CTRL_D);
        let stdout = parts.next().unwrap_or_default();
        let stderr = parts.next().unwrap_or_default();

        let stderr = String::from_utf8_lossy(stderr).trim().to_string();
        if !stderr.is_empty() {
            return Err(format!("the Pico reported an error:\n{stderr}"));
        }
        Ok(String::from_utf8_lossy(stdout).into_owned())
    }

    /// Hand the device back. `reset` soft-resets it, which is what restarts
    /// `main.py`; without it the Pico sits at a prompt running nothing.
    pub fn leave(&mut self, reset: bool) -> Result<(), String> {
        self.port.write_bytes(&[CTRL_B])?;
        std::thread::sleep(Duration::from_millis(60));
        if reset {
            self.port.discard_input();
            self.pending.clear();
            self.port.write_bytes(&[CTRL_D])?;
        }
        Ok(())
    }
}
