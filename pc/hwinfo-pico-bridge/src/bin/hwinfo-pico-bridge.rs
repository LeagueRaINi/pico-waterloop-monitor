//! Console front end — sensor listing, firmware deployment, diagnostics, and a
//! plain foreground run. For always-on use see `hwinfo-pico-bridge-tray`.

use hwinfo_pico_bridge::autostart;
use hwinfo_pico_bridge::bridge::{self, Config, Sink, Status};
use hwinfo_pico_bridge::control::Control;
use hwinfo_pico_bridge::hwinfo::{Reading, SharedMem};
use hwinfo_pico_bridge::pico;
use std::path::PathBuf;

const USAGE: &str = "\
hwinfo-pico-bridge — send HWiNFO CPU/GPU temperatures to the Pico display

USAGE:
    hwinfo-pico-bridge [OPTIONS]

OPTIONS:
    --port <COMn>      Pico serial port (default: autodetect by USB VID)
    --cpu <text>       Pick the CPU sensor whose label contains <text>
    --gpu <text>       Pick the GPU sensor whose label contains <text>
    --pump <text>      Pick the pump RPM sensor whose label contains <text>
    --cpu-power <text> Pick the CPU power sensor whose label contains <text>
    --gpu-power <text> Pick the GPU power sensor whose label contains <text>
    --interval <secs>  Seconds between updates (default: 1.0)
    --list             List the temperature, fan and power sensors HWiNFO has
    --dry-run          Print the lines instead of opening the serial port
    --force            Write to --port even if it is not a Raspberry Pi device
    -h, --help         Show this help
    -V, --version      Show which build this is

BACKGROUND USE:
    --install          Run hwinfo-pico-bridge-tray at login, with any sensor
                       options given alongside, then exit
    --uninstall        Remove the login entry
    --status           Show whether autostart is registered

PICO FIRMWARE:
    --deploy           Copy the firmware to the Pico, sending only the files
                       that differ from what is already on it
    --deploy-list      List what is on the device, then exit
    --deploy-reset     Soft-reset the Pico, so main.py starts again
    --firmware <dir>   Deploy this directory instead of the built-in copy
    --all              With --deploy: send every file, ignoring the record of
                       what was deployed last time
    --verify           With --deploy: hash every file on the device rather
                       than trusting that record
    --no-reset         With --deploy: leave the Pico at the REPL, running
                       nothing

The tray app holds the serial port. Pause it from its menu before deploying
from here, or use its own \"Update Pico\" item, which handles that for you.
";

struct ConsoleSink {
    dry_run: bool,
}

impl Sink for ConsoleSink {
    fn log(&self, line: &str) {
        println!("[hwinfo-pico-bridge] {line}");
    }

    fn error(&self, line: &str) {
        eprintln!("[hwinfo-pico-bridge] {line}");
    }

    fn sample(&self, status: &Status) {
        if self.dry_run {
            println!(
                "T,{},{},{},{},{}",
                bridge::fmt_temp(status.cpu),
                bridge::fmt_temp(status.gpu),
                bridge::fmt_rpm(status.pump),
                bridge::fmt_watts(status.cpu_power),
                bridge::fmt_watts(status.gpu_power)
            );
        }
    }
}

fn report(line: &str) {
    println!("[hwinfo-pico-bridge] {line}");
}

fn list_sensors(readings: &[Reading]) {
    let listed = |r: &Reading| r.is_temperature() || r.is_fan() || r.is_power();
    let width = readings
        .iter()
        .filter(|r| listed(r))
        .map(|r| r.sensor.chars().count())
        .max()
        .unwrap_or(10);
    println!(
        "{:>4}  {:<width$}  {:<32} {:>8}",
        "idx", "SENSOR", "LABEL", "VALUE"
    );
    for (i, r) in readings.iter().enumerate() {
        if !listed(r) {
            continue;
        }
        let value = if r.is_temperature() {
            format!("{:.1}", r.value)
        } else {
            format!("{:.0}", r.value)
        };
        println!(
            "{i:>4}  {:<width$}  {:<32} {value:>8} {}",
            r.sensor, r.label, r.unit
        );
    }
}

/// Which of the one-shot firmware actions was asked for.
#[derive(PartialEq)]
enum Firmware {
    None,
    Deploy,
    List,
    Reset,
}

fn run() -> Result<(), String> {
    let mut config = Config {
        interval_ms: 1000,
        ..Default::default()
    };
    let mut list = false;
    let mut dry_run = false;
    let mut install = false;
    let mut uninstall = false;
    let mut show_status = false;
    let mut firmware = Firmware::None;
    let mut deploy = pico::Options::default();
    // Sensor options are re-registered verbatim by --install.
    let mut passthrough: Vec<String> = Vec::new();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let take_value = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg {
            "--port" | "--cpu" | "--gpu" | "--pump" | "--cpu-power" | "--gpu-power"
            | "--interval" => {
                let value = take_value(&mut i)?;
                match arg {
                    "--port" => config.port = Some(value.clone()),
                    "--cpu" => config.cpu = Some(value.clone()),
                    "--gpu" => config.gpu = Some(value.clone()),
                    "--pump" => config.pump = Some(value.clone()),
                    "--cpu-power" => config.cpu_power = Some(value.clone()),
                    "--gpu-power" => config.gpu_power = Some(value.clone()),
                    _ => {
                        let secs: f64 = value
                            .parse()
                            .map_err(|_| format!("--interval: '{value}' is not a number"))?;
                        if !(0.1..=60.0).contains(&secs) {
                            return Err("--interval must be between 0.1 and 60 seconds".into());
                        }
                        config.interval_ms = (secs * 1000.0) as u32;
                    }
                }
                passthrough.push(arg.to_string());
                passthrough.push(value);
            }
            // Deploying is a one-shot action, so none of these belong in what
            // --install registers for the background app.
            "--firmware" => deploy.firmware_dir = Some(PathBuf::from(take_value(&mut i)?)),
            "--deploy" => firmware = Firmware::Deploy,
            "--deploy-list" => firmware = Firmware::List,
            "--deploy-reset" => firmware = Firmware::Reset,
            "--all" => deploy.all = true,
            "--verify" => deploy.verify = true,
            "--no-reset" => deploy.reset = false,
            "--list" => list = true,
            "--dry-run" => dry_run = true,
            "--force" => {
                config.force = true;
                passthrough.push(arg.to_string());
            }
            "--install" => install = true,
            "--uninstall" => uninstall = true,
            "--status" => show_status = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("hwinfo-pico-bridge {}", env!("BRIDGE_VERSION"));
                return Ok(());
            }
            other => return Err(format!("unknown option '{other}'\n\n{USAGE}")),
        }
        i += 1;
    }

    if show_status {
        match autostart::installed() {
            Some(line) => println!("[hwinfo-pico-bridge] starts at login:\n  {line}"),
            None => println!("[hwinfo-pico-bridge] not registered to start at login"),
        }
        return Ok(());
    }

    if uninstall {
        match autostart::uninstall()? {
            true => println!("[hwinfo-pico-bridge] removed from login startup"),
            false => println!("[hwinfo-pico-bridge] it was not registered to start at login"),
        }
        return Ok(());
    }

    if install {
        let line = autostart::install(&passthrough)?;
        println!("[hwinfo-pico-bridge] registered to start at login:\n  {line}");
        println!(
            "[hwinfo-pico-bridge] start it now with:\n  {}",
            autostart::tray_exe()?
        );
        return Ok(());
    }

    if firmware != Firmware::None {
        deploy.port = config.port.clone();
        deploy.force = config.force;
        match firmware {
            Firmware::Reset => {
                let port = pico::reset(&deploy, &report)?;
                println!("[hwinfo-pico-bridge] soft-reset {port}; the display should restart");
            }
            Firmware::List => {
                let listing = pico::list(&deploy, &report)?;
                println!("\nOn the device:");
                if listing.files.is_empty() {
                    println!("  (empty)");
                }
                for (path, size) in &listing.files {
                    println!("  {size:>8} B  {path}");
                }
                if let Some((total, free)) = listing.space {
                    let kb = |n: u64| n as f64 / 1024.0;
                    println!(
                        "\n  {:.0} KB free of {:.0} KB ({:.0} KB used)",
                        kb(free),
                        kb(total),
                        kb(total - free)
                    );
                }
            }
            Firmware::Deploy | Firmware::None => {
                println!("{}", pico::deploy(&deploy, &report)?.describe());
            }
        }
        return Ok(());
    }

    if list {
        list_sensors(&SharedMem::open()?.read_all()?);
        return Ok(());
    }

    config.dry_run = dry_run;
    // Nothing pauses a foreground run, so the loop is simply always on.
    let control = Control::new(true);
    bridge::run(&config, &ConsoleSink { dry_run }, &control)
}

fn main() {
    if let Err(err) = run() {
        eprintln!("[hwinfo-pico-bridge] {err}");
        std::process::exit(1);
    }
}
