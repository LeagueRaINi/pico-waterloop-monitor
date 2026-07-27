// No console window: this is the one that runs at login.
#![windows_subsystem = "windows"]

//! Tray front end — runs the bridge silently in the background with a status
//! icon, and can pause itself and update the Pico's firmware from the menu.

use hwinfo_pico_bridge::bridge::{self, Config, Sink, Status};
use hwinfo_pico_bridge::control::Control;
use hwinfo_pico_bridge::pico;
use hwinfo_pico_bridge::tray;
use std::sync::Arc;

const USAGE: &str = "\
hwinfo-pico-bridge-tray — run the HWiNFO bridge in the background with a tray icon

USAGE:
    hwinfo-pico-bridge-tray [OPTIONS]

To start it automatically at login, use the console binary:
    hwinfo-pico-bridge --install [OPTIONS]

OPTIONS:
    --port <COMn>      Pico serial port (default: autodetect by USB VID)
    --cpu <text>       Pick the CPU sensor whose label contains <text>
    --gpu <text>       Pick the GPU sensor whose label contains <text>
    --pump <text>      Pick the pump RPM sensor whose label contains <text>
    --cpu-power <text> Pick the CPU power sensor whose label contains <text>
    --gpu-power <text> Pick the GPU power sensor whose label contains <text>
    --interval <secs>  Seconds between updates (default: 1.0)
    --force            Write to --port even if it is not a Raspberry Pi device
    -h, --help         Show this help
    -V, --version      Show which build this is

Right-click the tray icon for the current readings, Pause, Update Pico and
Exit. The icon is teal while data is flowing, amber if the Pico link is down,
red if HWiNFO is not available and grey while paused.

Pause releases the serial port, so Thonny or a terminal can have the device
without closing the app. Update Pico copies this build's firmware to the
device, pausing and resuming around it on its own.
";

/// The tray reads everything it shows out of `Status`, so this only has to
/// forward it.
struct TraySink;

impl Sink for TraySink {
    fn status(&self, status: &Status) {
        tray::set_status(status);
    }

    fn sample(&self, status: &Status) {
        tray::set_status(status);
    }
}

fn parse() -> Result<Option<Config>, String> {
    let mut config = Config {
        interval_ms: 1000,
        wait_for_hwinfo: true,
        ..Default::default()
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                tray::message_box(
                    "HWiNFO Pico Bridge",
                    &format!("hwinfo-pico-bridge-tray {}", env!("BRIDGE_VERSION")),
                );
                std::process::exit(0);
            }
            "--force" => config.force = true,
            "--port" | "--cpu" | "--gpu" | "--pump" | "--cpu-power" | "--gpu-power"
            | "--interval" => {
                i += 1;
                let value = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("{arg} needs a value"))?;
                match arg {
                    "--port" => config.port = Some(value),
                    "--cpu" => config.cpu = Some(value),
                    "--gpu" => config.gpu = Some(value),
                    "--pump" => config.pump = Some(value),
                    "--cpu-power" => config.cpu_power = Some(value),
                    "--gpu-power" => config.gpu_power = Some(value),
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
            }
            other => return Err(format!("unknown option '{other}'")),
        }
        i += 1;
    }
    Ok(Some(config))
}

fn main() {
    let config = match parse() {
        Ok(Some(config)) => config,
        Ok(None) => {
            tray::message_box("HWiNFO Pico Bridge", USAGE);
            return;
        }
        Err(err) => {
            tray::message_box("HWiNFO Pico Bridge", &format!("{err}\n\n{USAGE}"));
            std::process::exit(1);
        }
    };

    let control = Control::new(true);
    // The updater needs these, and `config` is about to move into the worker.
    let deploy_port = config.port.clone();
    let deploy_force = config.force;

    let worker_control = Arc::clone(&control);
    std::thread::spawn(move || loop {
        match bridge::run(&config, &TraySink, &worker_control) {
            // Shut down, or paused and resumed cleanly.
            Ok(()) => return,
            Err(err) => {
                // Only a fatal misconfiguration gets here — a named port that
                // belongs to something else, say. Standing down beats spinning
                // on it, and leaves Resume as the way to try again once the
                // user has sorted it out.
                tray::set_status(&Status {
                    problem: Some(err),
                    ..Default::default()
                });
                worker_control.set_running(false);
                if !worker_control.wait_until_running() {
                    return;
                }
            }
        }
    });

    let update_control = Arc::clone(&control);
    let start_update = move || {
        if tray::busy() {
            return;
        }
        tray::set_busy(true);
        tray::set_activity(Some("Updating the Pico..."));

        let control = Arc::clone(&update_control);
        let options = pico::Options {
            port: deploy_port.clone(),
            force: deploy_force,
            ..Default::default()
        };
        // On a thread of its own: the tray has to keep pumping messages, and a
        // deploy takes seconds.
        std::thread::spawn(move || {
            let outcome = pico::with_bridge_paused(&control, || {
                pico::deploy(&options, &|line| tray::set_activity(Some(line)))
            });
            tray::set_activity(None);
            tray::set_busy(false);
            match outcome {
                Ok(summary) => tray::notify("Pico updated", &summary.describe()),
                Err(err) => tray::notify("Pico update failed", &err),
            }
        });
    };

    // The tray owns the main thread because it needs the message loop. If the
    // shell refuses the icon, keep running headless rather than leaving the
    // user with nothing.
    if let Err(err) = tray::run(&control, &start_update) {
        tray::message_box(
            "HWiNFO Pico Bridge",
            &format!("{err}\n\nContinuing without a tray icon."),
        );
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
    control.shutdown();
}
