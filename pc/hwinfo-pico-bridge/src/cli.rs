//! The options both front ends accept.
//!
//! Declared once so `--cpu "..."` means the same thing whichever binary it is
//! given to, and so `--install` can hand them on to the tray app without either
//! side keeping its own copy of the list.

use crate::bridge::Config;
use clap::Args;

/// Seconds between updates when `--interval` is not given.
const DEFAULT_INTERVAL: f64 = 1.0;

/// Bounds on `--interval`. Below this there is nothing new to send — HWiNFO
/// only polls its own sensors every couple of seconds — and above it the
/// display's graphs stop tracking anything you would notice.
const MIN_INTERVAL: f64 = 0.1;
const MAX_INTERVAL: f64 = 60.0;

#[derive(Args)]
pub struct SensorArgs {
    /// Pico serial port (default: autodetect by USB VID)
    #[arg(long, value_name = "COMn")]
    pub port: Option<String>,

    /// Pick the CPU sensor whose label contains this text
    #[arg(long, value_name = "TEXT")]
    pub cpu: Option<String>,

    /// Pick the GPU sensor whose label contains this text
    #[arg(long, value_name = "TEXT")]
    pub gpu: Option<String>,

    /// Pick the pump RPM sensor whose label contains this text
    #[arg(long, value_name = "TEXT")]
    pub pump: Option<String>,

    /// Pick the CPU power sensor whose label contains this text
    #[arg(long, value_name = "TEXT")]
    pub cpu_power: Option<String>,

    /// Pick the GPU power sensor whose label contains this text
    #[arg(long, value_name = "TEXT")]
    pub gpu_power: Option<String>,

    /// Seconds between updates [default: 1.0]
    #[arg(long, value_name = "SECS", value_parser = interval_secs)]
    pub interval: Option<f64>,

    /// Write to --port even if it is not a Raspberry Pi device
    #[arg(long)]
    pub force: bool,
}

/// clap prefixes this with "invalid value '<x>' for '--interval <SECS>'", so it
/// only has to say what is wrong with it.
fn interval_secs(text: &str) -> Result<f64, String> {
    let secs: f64 = text.parse().map_err(|_| "not a number".to_string())?;
    if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&secs) {
        return Err(format!(
            "must be between {MIN_INTERVAL} and {MAX_INTERVAL} seconds"
        ));
    }
    Ok(secs)
}

impl SensorArgs {
    /// The sampling loop's configuration. `dry_run` and `wait_for_hwinfo` are
    /// left at their defaults: they are decisions the front end makes, not
    /// options a user passes.
    pub fn config(&self) -> Config {
        Config {
            port: self.port.clone(),
            cpu: self.cpu.clone(),
            gpu: self.gpu.clone(),
            pump: self.pump.clone(),
            cpu_power: self.cpu_power.clone(),
            gpu_power: self.gpu_power.clone(),
            interval_ms: (self.interval.unwrap_or(DEFAULT_INTERVAL) * 1000.0) as u32,
            force: self.force,
            ..Default::default()
        }
    }

    /// The flags that would produce these options again, for `--install` to
    /// register against the tray binary.
    ///
    /// Rebuilt from what was parsed rather than copied out of `argv`, so what
    /// lands in the Run key is quoted exactly once, by `autostart`, under one
    /// set of rules.
    pub fn to_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for (flag, value) in [
            ("--port", &self.port),
            ("--cpu", &self.cpu),
            ("--gpu", &self.gpu),
            ("--pump", &self.pump),
            ("--cpu-power", &self.cpu_power),
            ("--gpu-power", &self.gpu_power),
        ] {
            if let Some(value) = value {
                flags.push(flag.to_string());
                flags.push(value.clone());
            }
        }
        if let Some(secs) = self.interval {
            flags.push("--interval".to_string());
            flags.push(secs.to_string());
        }
        if self.force {
            flags.push("--force".to_string());
        }
        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        sensors: SensorArgs,
    }

    fn parse(flags: &[&str]) -> SensorArgs {
        let mut argv = vec!["hwinfo-pico-bridge"];
        argv.extend_from_slice(flags);
        Harness::parse_from(argv).sensors
    }

    #[test]
    fn what_install_registers_parses_back_to_the_same_options() {
        // --install registers these against the tray binary, so an option that
        // did not survive the round trip would silently stop being applied the
        // next time the machine booted.
        let original = parse(&[
            "--port",
            "COM7",
            "--cpu",
            "CPU (Tctl/Tdie)",
            "--gpu-power",
            "Total Board Power (TBP)",
            "--interval",
            "2.5",
            "--force",
        ]);

        let flags = original.to_flags();
        let again = parse(&flags.iter().map(String::as_str).collect::<Vec<_>>());

        assert_eq!(again.to_flags(), flags);
        assert_eq!(again.cpu.as_deref(), Some("CPU (Tctl/Tdie)"));
        assert_eq!(again.gpu_power.as_deref(), Some("Total Board Power (TBP)"));
        assert_eq!(again.interval, Some(2.5));
        assert!(again.force);
    }

    #[test]
    fn options_that_were_not_given_are_not_registered() {
        // Otherwise the Run key would pin today's defaults forever.
        assert!(parse(&[]).to_flags().is_empty());
        assert_eq!(parse(&["--force"]).to_flags(), ["--force"]);
    }

    #[test]
    fn the_interval_has_to_be_a_number_in_range() {
        for bad in ["0", "0.05", "61", "-1", "abc", ""] {
            assert!(
                Harness::try_parse_from(["hwinfo-pico-bridge", "--interval", bad]).is_err(),
                "--interval {bad:?} should be rejected"
            );
        }
        assert_eq!(parse(&["--interval", "0.1"]).config().interval_ms, 100);
        assert_eq!(parse(&["--interval", "60"]).config().interval_ms, 60_000);
        assert_eq!(parse(&[]).config().interval_ms, 1000);
    }
}
