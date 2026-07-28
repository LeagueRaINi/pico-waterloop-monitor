// No console window: this is the one that runs at login.
#![windows_subsystem = "windows"]

//! Tray front end — runs the bridge silently in the background with a status
//! icon, and can pause itself and update the Pico's firmware from the menu.

use clap::Parser;
use hwinfo_pico_bridge::bridge::{self, Sink, Status};
use hwinfo_pico_bridge::cli::SensorArgs;
use hwinfo_pico_bridge::control::Control;
use hwinfo_pico_bridge::pico;
use hwinfo_pico_bridge::tray;
use std::sync::Arc;

const AFTER_HELP: &str = "\
To start it automatically at login, use the console binary:
    hwinfo-pico-bridge --install [OPTIONS]

Right-click the tray icon for the current readings, Pause, Update Pico and
Exit. The icon is teal while data is flowing, amber if the Pico link is down,
red if HWiNFO is not available and grey while paused.

Pause releases the serial port, so Thonny or a terminal can have the device
without closing the app. Update Pico copies this build's firmware to the
device, pausing and resuming around it on its own.";

#[derive(Parser)]
#[command(
    // Both binaries are one package, so the name has to be given or each would
    // report the other's.
    name = "hwinfo-pico-bridge-tray",
    version = env!("BRIDGE_VERSION"),
    about = "run the HWiNFO bridge in the background with a tray icon",
    after_help = AFTER_HELP,
    // Everything this binary says arrives in a message box, which would render
    // ANSI colour codes as literal noise.
    color = clap::ColorChoice::Never,
)]
struct Args {
    #[command(flatten)]
    sensors: SensorArgs,
}

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

fn main() {
    // A windows-subsystem binary has no stdout to print to, so clap must not do
    // its own printing. `--help`, `--version` and a bad option all arrive here
    // as an `Error` that renders to exactly the text clap would have shown, and
    // carries the exit code it would have used.
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(err) => {
            tray::message_box("HWiNFO Pico Bridge", &err.to_string());
            std::process::exit(err.exit_code());
        }
    };

    let mut config = args.sensors.config();
    // At login this may well start before HWiNFO does, so wait rather than give
    // up on the first look.
    config.wait_for_hwinfo = true;

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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// What `main` would put in the message box for a given command line.
    fn rendered(args: &[&str]) -> String {
        let mut argv = vec!["hwinfo-pico-bridge-tray"];
        argv.extend_from_slice(args);
        Args::try_parse_from(argv)
            .err()
            .expect("this should not have parsed")
            .to_string()
    }

    #[test]
    fn the_only_way_this_binary_can_talk_is_plain_text() {
        // There is no console to print to, so help, --version and usage errors
        // all end up in a MessageBox. An escape sequence would be shown there
        // literally rather than as colour.
        for args in [&["--help"][..], &["--version"][..], &["--nonsense"][..]] {
            let text = rendered(args);
            assert!(!text.is_empty(), "{args:?} rendered to nothing");
            assert!(
                !text.contains('\x1b'),
                "{args:?} rendered with ANSI escapes: {text:?}"
            );
        }
    }

    #[test]
    fn version_says_which_binary_this_is() {
        // Both binaries live in one package, so this is the only thing telling
        // them apart in a --version box.
        assert!(
            rendered(&["--version"]).starts_with("hwinfo-pico-bridge-tray "),
            "{}",
            rendered(&["--version"])
        );
    }

    #[test]
    fn the_shared_options_are_all_accepted_here_too() {
        // --install registers these against this binary, so anything the
        // console accepts and passes on has to parse here.
        let flags = ["--port", "COM7", "--cpu", "Core Max", "--interval", "2"];
        assert!(Args::try_parse_from(["tray"].iter().chain(flags.iter())).is_ok());
        Args::command().debug_assert();
    }
}
