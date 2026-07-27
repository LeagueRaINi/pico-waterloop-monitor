//! Updating the Pico's firmware, from the same program that talks to it.
//!
//! The bridge already owns the port, already knows which COM number is a
//! Raspberry Pi and already has to be stopped before anything else can write to
//! the device — so making it do the deploy as well removes the whole "exit the
//! app, run a script, start the app" dance.
//!
//! What actually goes over the wire is MicroPython's raw REPL (`repl`), the
//! files come from the copy baked in at build time (`firmware`), and `manifest`
//! is how a second deploy avoids re-sending 165 KB of unchanged font tables.

pub mod firmware;
pub mod manifest;
mod repl;

use crate::bridge::resolve_port;
use crate::control::Control;
use base64::Engine as _;
use manifest::{Entry, Manifest};
use repl::Repl;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long to wait for the running display to answer `?M`. It looks at its
/// input about four times a second, so this is generous on purpose — and it
/// is only ever paid when the firmware is too old to reply at all.
const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
const QUERY_POLL: Duration = Duration::from_millis(100);

/// Bytes of file content per round trip. Base64 inflates this by a third, and
/// the device compiles each statement it is sent, so the point is to keep the
/// generated line comfortable rather than to fill a buffer.
const CHUNK: usize = 1024;

const BUSY_HINT: &str = "Only one program can hold the port at a time. If the bridge \
                         is running, pause it first — the tray menu has Pause, and its \
                         \"Update Pico\" item does this for you.";

/// Helpers the device gets once per session; everything below calls into them.
/// Kept as a `.py` file so CI can parse it — see the file for why.
const PRELUDE: &str = include_str!("prelude.py");

pub struct Options {
    pub port: Option<String>,
    /// Same meaning as the bridge's: write to a named port that is not a
    /// Raspberry Pi device.
    pub force: bool,
    /// Deploy this directory instead of the copy baked into the binary.
    pub firmware_dir: Option<PathBuf>,
    /// Upload everything, whatever the manifest says.
    pub all: bool,
    /// Hash every file on the device rather than trusting the manifest.
    pub verify: bool,
    /// Soft-reset at the end, so `main.py` starts again.
    pub reset: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            port: None,
            force: false,
            firmware_dir: None,
            all: false,
            verify: false,
            // A Pico left at the REPL is a Pico running nothing, which is
            // never what someone deploying firmware wanted.
            reset: true,
        }
    }
}

pub struct Summary {
    pub port: String,
    pub uploaded: usize,
    pub skipped: usize,
    pub removed: usize,
    pub bytes: u64,
    /// False when the device's MicroPython has no `hashlib`, so uploads could
    /// only be checked by size.
    pub hashed: bool,
    pub reset: bool,
    /// Whether `main.py` had to be stopped to do this. False means the running
    /// display answered for itself and was left alone, graphs and all.
    pub interrupted: bool,
}

impl Summary {
    pub fn describe(&self) -> String {
        let mut text = if self.uploaded == 0 {
            format!("{}: already up to date", self.port)
        } else {
            format!(
                "{}: uploaded {} file{} ({:.1} KB)",
                self.port,
                self.uploaded,
                if self.uploaded == 1 { "" } else { "s" },
                self.bytes as f64 / 1024.0
            )
        };
        let _ = write!(text, ", {} unchanged", self.skipped);
        if self.removed > 0 {
            let _ = write!(text, ", {} removed", self.removed);
        }
        text.push('.');
        if !self.hashed {
            text.push_str(
                "\n\nThis MicroPython build has no hashlib, so uploads were checked \
                 by size only and the next update cannot skip unchanged files as \
                 confidently.",
            );
        }
        if !self.interrupted {
            text.push_str(
                "\n\nThe display answered for itself, so it was never stopped — \
                 its graph history is intact.",
            );
        } else if self.reset {
            text.push_str("\n\nThe Pico was soft-reset; the display should restart.");
        } else {
            text.push_str("\n\nThe Pico was left at the REPL and is running nothing. Use --deploy-reset to start it.");
        }
        text
    }
}

// ------------------------------------------------------------------ encoding

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(data) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// ------------------------------------------------------------------- device

fn find_port(opts: &Options, report: &dyn Fn(&str)) -> Result<String, String> {
    let found = resolve_port(opts.port.as_deref(), opts.force)?.ok_or_else(|| {
        "no Raspberry Pi USB serial port present — is the Pico plugged in, with \
         MicroPython flashed?"
            .to_string()
    })?;
    if found.forced {
        report(&format!(
            "WARNING: {} is not a Raspberry Pi device, --force given",
            found.name
        ));
    } else {
        report(&format!("{} is {}", found.name, found.device));
    }
    Ok(found.name)
}

fn connect(opts: &Options, report: &dyn Fn(&str)) -> Result<Repl, String> {
    open_repl(&find_port(opts, report)?, report)
}

fn open_repl(port: &str, report: &dyn Fn(&str)) -> Result<Repl, String> {
    let mut repl = Repl::open(port).map_err(|err| {
        if crate::serial::is_busy(&err) {
            format!("{err}\n{BUSY_HINT}")
        } else {
            err
        }
    })?;
    report("interrupting main.py and entering the raw REPL");
    repl.enter()?;
    repl.exec(PRELUDE)?;
    Ok(repl)
}

/// Ask the *running* display what it was last given, over the normal wire
/// protocol.
///
/// Working this out through the REPL means interrupting `main.py` first, and
/// `main.py` cannot be resumed without a soft reset that empties the graphs.
/// So an update with nothing to send would cost 30 minutes of history to
/// discover it had nothing to do. Firmware that does not know the question —
/// or a Pico already sitting at a prompt — simply does not answer, and the
/// caller falls back to looking for itself.
fn ask_deploy_id(port: &str) -> Option<String> {
    let mut port = crate::serial::Port::open_with_timeout(port, QUERY_POLL).ok()?;
    port.discard_input();
    port.write_bytes(b"?M\n").ok()?;

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        match port.read_some(&mut chunk) {
            Ok(0) => {}
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(id) = parse_deploy_id(&buf) {
                    return Some(id);
                }
                // Whatever this is, it is not a display answering.
                if buf.len() > 4096 {
                    return None;
                }
            }
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

/// `M,<hex>` on a line of its own. `M,-` means the device could not hash its
/// record, which is not an answer we can act on.
fn parse_deploy_id(buf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(buf);
    let line = text.lines().find_map(|l| l.trim().strip_prefix("M,"))?;
    let id = line.trim();
    (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
}

/// Every file on the device, by path relative to the root, with its size.
fn device_files(repl: &mut Repl) -> Result<BTreeMap<String, u64>, String> {
    let listing = repl.exec("_walk('/')")?;
    let mut files = BTreeMap::new();
    for line in listing.lines() {
        let Some((size, path)) = line.trim().split_once(' ') else {
            continue;
        };
        let Ok(size) = size.parse::<u64>() else {
            continue;
        };
        files.insert(path.trim_start_matches('/').to_string(), size);
    }
    Ok(files)
}

fn read_manifest(repl: &mut Repl) -> Result<Manifest, String> {
    let text = repl.exec(&format!("_cat('{}')", manifest::PATH))?;
    Ok(Manifest::parse(&text))
}

/// `(sha256, size)` as the device sees the file. The hash is `None` when this
/// MicroPython has no `hashlib`.
fn device_check(repl: &mut Repl, path: &str) -> Result<(Option<String>, u64), String> {
    let out = repl.exec(&format!("_check('{path}')"))?;
    let out = out.trim();
    let (sha, size) = out
        .split_once(' ')
        .ok_or_else(|| format!("{path}: the Pico gave an unreadable answer: {out:?}"))?;
    let size = size
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{path}: the Pico gave an unreadable size: {out:?}"))?;
    Ok(((sha != "-").then(|| sha.to_string()), size))
}

fn write_file(repl: &mut Repl, path: &str, data: &[u8]) -> Result<(), String> {
    // Every chunk below allocates a string and its decoded bytes, so sweep
    // between files rather than letting that pile up across a whole deploy.
    repl.exec(&format!("gc.collect()\n_f=open('{path}','wb')"))?;
    let mut wrote = Ok(());
    for chunk in data.chunks(CHUNK) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        wrote = repl
            .exec(&format!("_f.write(_ba.a2b_base64('{encoded}'))"))
            .map(|_| ());
        if wrote.is_err() {
            break;
        }
    }
    // Close it whatever happened: a half-written file with a live handle is
    // worse than a half-written one without.
    let closed = repl.exec("_f.close()").map(|_| ());
    wrote.and(closed)
}

// ------------------------------------------------------------------ actions

/// The record this deploy would leave behind, derived from the local files
/// alone. Also what the running display's answer to `?M` is compared against.
fn local_manifest(source: &[firmware::SourceFile]) -> Manifest {
    let mut manifest = Manifest::default();
    for file in source {
        manifest.insert(
            &file.path,
            Entry {
                sha256: sha256_hex(&file.data),
                size: file.data.len() as u64,
            },
        );
    }
    manifest
}

/// Copy the firmware to the device, sending only what differs from what is
/// already there.
pub fn deploy(opts: &Options, report: &dyn Fn(&str)) -> Result<Summary, String> {
    let source = firmware::load(opts.firmware_dir.as_deref())?;
    let current = local_manifest(&source);
    let port = find_port(opts, report)?;

    // Ask before interrupting. Both --all and --verify exist to distrust the
    // record, so neither takes this shortcut.
    if !opts.all && !opts.verify {
        let want = sha256_hex(current.render().as_bytes());
        if ask_deploy_id(&port).is_some_and(|id| id == want) {
            report("the display already has this firmware, and is still running");
            return Ok(Summary {
                port,
                uploaded: 0,
                skipped: source.len(),
                removed: 0,
                bytes: 0,
                hashed: true,
                reset: false,
                interrupted: false,
            });
        }
    }

    let mut repl = open_repl(&port, report)?;
    let outcome = transfer(&mut repl, opts, &source, &current, report);
    // Hand the device back even if a transfer failed part way, or it is left
    // sitting at a prompt with the display frozen.
    let _ = repl.leave(opts.reset);
    outcome
}

fn transfer(
    repl: &mut Repl,
    opts: &Options,
    source: &[firmware::SourceFile],
    current: &Manifest,
    report: &dyn Fn(&str),
) -> Result<Summary, String> {
    report("checking what is already on the device");
    let on_device = device_files(repl)?;
    let previous = read_manifest(repl)?;

    // With --verify the device's own hashes decide, so a file edited in Thonny
    // is noticed even though the manifest still describes what we last sent.
    let mut device_hashes = BTreeMap::new();
    let mut hashed = true;
    if opts.verify {
        report(&format!("hashing {} files on the device", on_device.len()));
        for path in on_device.keys() {
            let (sha, _) = device_check(repl, path)?;
            match sha {
                Some(sha) => {
                    device_hashes.insert(path.clone(), sha);
                }
                None => hashed = false,
            }
        }
    }

    let mut planned: Vec<(&firmware::SourceFile, Entry)> = Vec::new();
    let mut skipped = 0usize;

    for file in source {
        // Already computed for the whole set, so the fast path above and this
        // one can never disagree about what a file hashes to.
        let entry = current
            .get(&file.path)
            .expect("every source file is in the local manifest")
            .clone();
        let unchanged = !opts.all
            && if opts.verify {
                device_hashes.get(&file.path) == Some(&entry.sha256)
            } else {
                // The manifest says what we last sent; the size says whether
                // anything has touched the file since. Together they catch
                // everything short of an edit that preserves the length.
                previous.get(&file.path) == Some(&entry)
                    && on_device.get(&file.path) == Some(&entry.size)
            };
        if unchanged {
            skipped += 1;
        } else {
            planned.push((file, entry));
        }
    }

    // Only ever remove what a previous deploy put there. Anything else on the
    // device belongs to whoever put it there.
    let wanted: BTreeSet<&str> = source.iter().map(|f| f.path.as_str()).collect();
    let stale: Vec<String> = previous
        .paths()
        .filter(|path| !wanted.contains(path) && on_device.contains_key(*path))
        .map(str::to_string)
        .collect();

    report(&format!(
        "{} to upload, {skipped} unchanged, {} to remove",
        planned.len(),
        stale.len()
    ));

    if !planned.is_empty() {
        let dirs = firmware::directories(source);
        if !dirs.is_empty() {
            let mkdirs = dirs
                .iter()
                .map(|d| format!("_mkdir('{d}')"))
                .collect::<Vec<_>>()
                .join("\n");
            repl.exec(&mkdirs)?;
        }
    }

    let mut bytes = 0u64;
    let total = planned.len();
    for (index, (file, entry)) in planned.iter().enumerate() {
        report(&format!("uploading {} ({}/{total})", file.path, index + 1));
        write_file(repl, &file.path, &file.data)?;

        // Ask the device what it ended up with rather than assuming the
        // transfer was clean.
        let (sha, size) = device_check(repl, &file.path)?;
        match sha {
            Some(sha) if sha != entry.sha256 => {
                return Err(format!(
                    "{}: the copy on the Pico does not match what was sent \
                     (expected {}, got {sha}). Nothing was left half-written, but \
                     try again.",
                    file.path, entry.sha256
                ));
            }
            Some(_) => {}
            None => hashed = false,
        }
        if size != entry.size {
            return Err(format!(
                "{}: only {size} of {} bytes arrived on the Pico",
                file.path, entry.size
            ));
        }
        bytes += entry.size;
    }

    for path in &stale {
        report(&format!("removing {path}"));
        repl.exec(&format!("_rm('{path}')"))?;
    }

    // Last, so that a deploy which died half way leaves a manifest describing
    // the *older* state — which costs an extra upload next time, rather than
    // skipping a file that never made it.
    let rendered = current.render();
    if rendered != previous.render() {
        report("recording hashes on the device");
        write_file(repl, manifest::PATH, rendered.as_bytes())?;
    }

    Ok(Summary {
        port: repl.port_name().to_string(),
        uploaded: total,
        skipped,
        removed: stale.len(),
        bytes,
        hashed,
        reset: opts.reset,
        interrupted: true,
    })
}

pub struct Listing {
    pub files: Vec<(String, u64)>,
    /// Total and free bytes of the device filesystem, when it can say.
    pub space: Option<(u64, u64)>,
}

/// What is on the device now, with sizes and how much room is left.
pub fn list(opts: &Options, report: &dyn Fn(&str)) -> Result<Listing, String> {
    let mut repl = connect(opts, report)?;
    let files = device_files(&mut repl);
    let space = device_space(&mut repl);
    let _ = repl.leave(opts.reset);
    Ok(Listing {
        files: files?.into_iter().collect(),
        space,
    })
}

/// Zeroes, an unparsable answer or an error all mean the same thing here:
/// nothing worth reporting. Knowing the free space is never load-bearing.
fn device_space(repl: &mut Repl) -> Option<(u64, u64)> {
    let out = repl.exec("_space()").ok()?;
    let (total, free) = out.trim().split_once(' ')?;
    let total: u64 = total.trim().parse().ok()?;
    let free: u64 = free.trim().parse().ok()?;
    (total > 0).then_some((total, free))
}

/// Soft-reset the device, so `main.py` starts again.
pub fn reset(opts: &Options, report: &dyn Fn(&str)) -> Result<String, String> {
    let mut repl = connect(opts, report)?;
    let name = repl.port_name().to_string();
    repl.leave(true)?;
    Ok(name)
}

/// Pause the bridge, wait for it to let go of the port, run `action`, then put
/// the bridge back the way it was.
///
/// Restoring on the way out is the whole reason this exists: an update that
/// left the bridge paused would look like an update that broke it.
pub fn with_bridge_paused<T>(
    control: &Control,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let was_running = control.running();
    control.set_running(false);
    let released = control.wait_idle(Duration::from_secs(5));
    let outcome = if released {
        action()
    } else {
        Err(format!(
            "the bridge did not release the serial port within 5 seconds.\n{BUSY_HINT}"
        ))
    };
    control.set_running(was_running);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_comes_back_however_the_action_ends() {
        // An update that left the bridge paused would be indistinguishable
        // from an update that broke it.
        let control = Control::new(true);
        let ok = with_bridge_paused(&control, || {
            assert!(
                !control.running(),
                "the port must be free during the action"
            );
            Ok(42)
        });
        assert_eq!(ok.unwrap(), 42);
        assert!(control.running());

        let failed: Result<(), String> = with_bridge_paused(&control, || Err("boom".into()));
        assert_eq!(failed.unwrap_err(), "boom");
        assert!(control.running());
    }

    #[test]
    fn a_bridge_that_was_already_paused_stays_paused() {
        let control = Control::new(false);
        assert!(with_bridge_paused(&control, || Ok(())).is_ok());
        assert!(!control.running(), "the user's own pause must be respected");
    }

    #[test]
    fn only_a_real_answer_counts_as_one() {
        let id = "a".repeat(64);
        // Framed by whatever else was on the wire.
        assert_eq!(
            parse_deploy_id(format!("junk\r\nM,{id}\r\n").as_bytes()).as_deref(),
            Some(id.as_str())
        );
        for not_an_answer in [
            "",
            "M,-\n",                            // device has no hashlib
            "M,\n",                             // nothing at all
            &format!("M,{}\n", "a".repeat(63)), // truncated
            &format!("M,{}\n", "z".repeat(64)), // not hex
            ">>> \n",                           // sitting at the REPL
            "T,64.5,71.0,1532,142,318\n",       // our own traffic echoed back
        ] {
            assert!(
                parse_deploy_id(not_an_answer.as_bytes()).is_none(),
                "{not_an_answer:?} should not be taken as an answer"
            );
        }
    }

    #[test]
    fn the_local_manifest_is_what_the_device_is_asked_about() {
        // The fast path compares a hash of this against what the display
        // reports, so it has to be the same bytes `transfer` would write.
        let source = [("main.py", &b"print(1)"[..]), ("lib/a.py", &b""[..])]
            .into_iter()
            .map(|(path, data)| firmware::SourceFile {
                path: path.to_string(),
                data: std::borrow::Cow::Borrowed(data),
            })
            .collect::<Vec<_>>();

        let manifest = local_manifest(&source);
        assert_eq!(manifest.get("main.py").unwrap().size, 8);
        assert_eq!(
            manifest.get("lib/a.py").unwrap().sha256,
            sha256_hex(b""),
            "an empty file still gets a real hash"
        );
        assert_eq!(
            manifest.paths().collect::<Vec<_>>(),
            ["lib/a.py", "main.py"]
        );
    }

    #[test]
    fn sha256_matches_the_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
