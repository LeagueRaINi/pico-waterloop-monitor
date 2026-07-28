//! Console front end — sensor listing, firmware deployment, diagnostics, and a
//! plain foreground run. For always-on use see `hwinfo-pico-bridge-tray`.

use clap::Parser;
use hwinfo::{Reading, ReadingKind, SharedMem};
use hwinfo_pico_bridge_core::bridge::{self, Sink, Status};
use hwinfo_pico_bridge_core::cli::SensorArgs;
use hwinfo_pico_bridge_core::control::Control;
use hwinfo_pico_bridge_core::{autostart, pico, BRIDGE_VERSION};
use std::path::PathBuf;

const AFTER_HELP: &str = "\
The tray app holds the serial port. Pause it from its menu before deploying
from here, or use its own \"Update Pico\" item, which handles that for you.";

#[derive(Parser)]
#[command(
    name = "hwinfo-pico-bridge",
    version = BRIDGE_VERSION,
    about = "send HWiNFO CPU/GPU temperatures to the Pico display",
    after_help = AFTER_HELP,
)]
struct Args {
    #[command(flatten)]
    sensors: SensorArgs,

    /// List the temperature, fan and power sensors HWiNFO has
    #[arg(long)]
    list: bool,

    /// Print the lines instead of opening the serial port
    #[arg(long)]
    dry_run: bool,

    #[command(flatten)]
    autostart: AutostartArgs,

    #[command(flatten)]
    firmware: FirmwareArgs,
}

#[derive(clap::Args)]
#[command(next_help_heading = "Background use")]
struct AutostartArgs {
    /// Run hwinfo-pico-bridge-tray at login, with any sensor options given
    /// alongside, then exit
    #[arg(long)]
    install: bool,

    /// Remove the login entry
    #[arg(long)]
    uninstall: bool,

    /// Show whether autostart is registered
    #[arg(long)]
    status: bool,
}

#[derive(clap::Args)]
#[command(next_help_heading = "Pico firmware")]
struct FirmwareArgs {
    /// Copy the firmware to the Pico, sending only the files that differ from
    /// what is already on it
    #[arg(long)]
    deploy: bool,

    /// List what is on the device, then exit
    #[arg(long)]
    deploy_list: bool,

    /// Soft-reset the Pico, so main.py starts again
    #[arg(long)]
    deploy_reset: bool,

    /// Deploy this directory instead of the built-in copy
    #[arg(long, value_name = "DIR")]
    firmware: Option<PathBuf>,

    /// With --deploy: send every file, ignoring the record of what was
    /// deployed last time
    #[arg(long)]
    all: bool,

    /// With --deploy: hash every file on the device rather than trusting that
    /// record
    #[arg(long)]
    verify: bool,

    /// With --deploy: leave the Pico at the REPL, running nothing
    #[arg(long)]
    no_reset: bool,
}

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
    let listed = |r: &Reading| {
        matches!(
            r.kind,
            ReadingKind::Temperature | ReadingKind::Fan | ReadingKind::Power
        )
    };
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
        let value = if r.kind == ReadingKind::Temperature {
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
enum Firmware {
    Deploy,
    List,
    Reset,
}

impl FirmwareArgs {
    fn action(&self) -> Option<Firmware> {
        if self.deploy_reset {
            Some(Firmware::Reset)
        } else if self.deploy_list {
            Some(Firmware::List)
        } else if self.deploy {
            Some(Firmware::Deploy)
        } else {
            None
        }
    }

    /// Deploying is a one-shot action, so none of this belongs in what
    /// `--install` registers for the background app.
    fn options(&self, sensors: &SensorArgs) -> pico::Options {
        pico::Options {
            port: sensors.port.clone(),
            force: sensors.force,
            firmware_dir: self.firmware.clone(),
            all: self.all,
            verify: self.verify,
            reset: !self.no_reset,
        }
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();

    if args.autostart.status {
        match autostart::installed() {
            Some(line) => println!("[hwinfo-pico-bridge] starts at login:\n  {line}"),
            None => println!("[hwinfo-pico-bridge] not registered to start at login"),
        }
        return Ok(());
    }

    if args.autostart.uninstall {
        match autostart::uninstall()? {
            true => println!("[hwinfo-pico-bridge] removed from login startup"),
            false => println!("[hwinfo-pico-bridge] it was not registered to start at login"),
        }
        return Ok(());
    }

    if args.autostart.install {
        let line = autostart::install(&args.sensors.to_flags())?;
        println!("[hwinfo-pico-bridge] registered to start at login:\n  {line}");
        println!(
            "[hwinfo-pico-bridge] start it now with:\n  {}",
            autostart::tray_exe()?
        );
        return Ok(());
    }

    if let Some(action) = args.firmware.action() {
        let deploy = args.firmware.options(&args.sensors);
        match action {
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
            Firmware::Deploy => {
                println!("{}", pico::deploy(&deploy, &report)?.describe());
            }
        }
        return Ok(());
    }

    if args.list {
        list_sensors(&SharedMem::open()?.read_all()?);
        return Ok(());
    }

    let mut config = args.sensors.config();
    config.dry_run = args.dry_run;
    // Nothing pauses a foreground run, so the loop is simply always on.
    let control = Control::new(true);
    bridge::run(
        &config,
        &ConsoleSink {
            dry_run: args.dry_run,
        },
        &control,
    )
}

fn main() {
    if let Err(err) = run() {
        eprintln!("[hwinfo-pico-bridge] {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_options_do_not_collide() {
        // Three flattened structs contribute flags; clap's own check catches a
        // name or short reused across them.
        Args::command().debug_assert();
    }

    #[test]
    fn the_firmware_actions_are_one_shot_and_ordered() {
        let parse = |flags: &[&str]| {
            let mut argv = vec!["hwinfo-pico-bridge"];
            argv.extend_from_slice(flags);
            Args::parse_from(argv)
        };
        assert!(parse(&[]).firmware.action().is_none());
        assert!(matches!(
            parse(&["--deploy"]).firmware.action(),
            Some(Firmware::Deploy)
        ));
        // --no-reset only ever means "leave it at the REPL"; deploying resets.
        assert!(
            parse(&["--deploy"])
                .firmware
                .options(&parse(&[]).sensors)
                .reset
        );
        assert!(
            !parse(&["--deploy", "--no-reset"])
                .firmware
                .options(&parse(&[]).sensors)
                .reset
        );
    }

    #[test]
    fn deploying_does_not_leak_into_what_install_registers() {
        // These are one-shot actions; a Run key carrying --deploy would
        // re-flash the Pico at every login.
        let args = parse_all(&["--deploy", "--all", "--cpu", "Core Max", "--force"]);
        assert_eq!(
            args.sensors.to_flags(),
            ["--cpu", "Core Max", "--force"],
            "only the sensor options belong in the login entry"
        );
    }

    fn parse_all(flags: &[&str]) -> Args {
        let mut argv = vec!["hwinfo-pico-bridge"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
    }
}
