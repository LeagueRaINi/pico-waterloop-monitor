//! Sensor selection and the sampling loop, shared by the console and tray
//! front ends. Everything user-visible goes through `Sink`, so the two front
//! ends differ only in how they present it, and everything that starts or
//! stops the loop goes through `Control`.

use crate::control::Control;
use crate::hwinfo::{
    Reading, SharedMem, READING_TYPE_FAN, READING_TYPE_POWER, READING_TYPE_TEMPERATURE,
};
use crate::serial;
use std::time::{Duration, Instant};

/// How long HWiNFO's poll counter may stand still before we assume the
/// instance behind it is gone and go looking for a new one.
///
/// Time, not iterations: HWiNFO polls its sensors about every two seconds
/// whatever `--interval` we sample at, so counting rounds of the loop would
/// declare a perfectly healthy HWiNFO dead as soon as anyone passed a short
/// interval.
const HWINFO_STALL: Duration = Duration::from_secs(30);

/// Defaults when `--cpu` is not given. Exact label matches are tried in this
/// order, then the loose rules in `pick`.
///
/// These are hotspot readings, i.e. the hottest point on the die rather than a
/// package or average sensor. On AMD, Tctl/Tdie *is* the hotspot — it is the
/// maximum across the distributed die sensors. Note that "CPU IOD Hotspot" is
/// deliberately not preferred: it is only the I/O die, and on a 7000-series
/// part it reads several degrees below the real hotspot.
pub const CPU_LABELS: &[&str] = &[
    "cpu hot spot",
    "cpu hotspot",
    "cpu (tctl/tdie)", // AMD: max across die sensors
    "core max",        // Intel: hottest core
    "cpu package",
    "cpu ccd1 (tdie)",
    "cpu temperature",
    "cpu die (average)",
];

pub const GPU_LABELS: &[&str] = &[
    "gpu hot spot temperature",
    "gpu hotspot temperature",
    "gpu hot spot",
    "gpu hotspot",
    "gpu temperature",
    "gpu core temperature",
];

/// On a custom loop the pump is normally wired to the CPU fan header so the
/// board can tacho it.
pub const PUMP_LABELS: &[&str] = &["pump", "water pump", "pump rpm", "cpu"];

/// Whole-package figures rather than per-core or per-rail ones, since the
/// point is correlating total heat output against coolant temperature.
pub const CPU_POWER_LABELS: &[&str] =
    &["cpu package power", "cpu ppt", "package power", "cpu power"];

/// Board power beats chip power here: the coolant sees everything the card
/// dissipates, not just the GPU die.
pub const GPU_POWER_LABELS: &[&str] = &[
    "total board power (tbp)",
    "gpu board power",
    "total graphics power (tgp)",
    "gpu power",
];

#[derive(Default, Clone)]
pub struct Config {
    pub port: Option<String>,
    pub cpu: Option<String>,
    pub gpu: Option<String>,
    pub pump: Option<String>,
    pub cpu_power: Option<String>,
    pub gpu_power: Option<String>,
    pub interval_ms: u32,
    pub force: bool,
    /// Resolve sensors and report readings, but never touch the serial port.
    pub dry_run: bool,
    /// Wait for HWiNFO instead of giving up. The tray sets this, because at
    /// login it may well start before HWiNFO does.
    pub wait_for_hwinfo: bool,
}

#[derive(Default, Clone)]
pub struct Status {
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
    pub pump: Option<f64>,
    pub cpu_power: Option<f64>,
    pub gpu_power: Option<f64>,
    pub port: Option<String>,
    pub hwinfo_ok: bool,
    pub connected: bool,
    /// Standing down at the front end's request, with the serial port released.
    pub paused: bool,
    /// What is currently wrong, if anything.
    pub problem: Option<String>,
}

impl Status {
    pub fn healthy(&self) -> bool {
        self.hwinfo_ok && self.connected && !self.paused && self.problem.is_none()
    }
}

/// How a front end surfaces what the loop is doing.
pub trait Sink {
    fn log(&self, _line: &str) {}
    fn error(&self, _line: &str) {}
    /// Health changed (link up/down, HWiNFO came or went).
    fn status(&self, _status: &Status) {}
    /// A fresh set of readings was taken. Distinct from `status` so that a
    /// front end printing one line per sample does not also print one for
    /// every connection state change.
    fn sample(&self, _status: &Status) {}
}

/// Index of the best matching reading of the given type.
pub fn pick(
    readings: &[Reading],
    kind: u32,
    exact_labels: &[&str],
    keyword: &str,
    explicit: Option<&str>,
) -> Option<usize> {
    let candidates: Vec<usize> = readings
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == kind && r.value.is_finite())
        .map(|(i, _)| i)
        .collect();

    if let Some(needle) = explicit {
        let needle = needle.to_lowercase();
        // exact label, then label substring, then "sensor + label" substring
        for pass in 0..3 {
            for &i in &candidates {
                let label = readings[i].label.to_lowercase();
                let hit = match pass {
                    0 => label == needle,
                    1 => label.contains(&needle),
                    _ => format!("{} {}", readings[i].sensor, readings[i].label)
                        .to_lowercase()
                        .contains(&needle),
                };
                if hit {
                    return Some(i);
                }
            }
        }
        return None;
    }

    for want in exact_labels {
        let hit = candidates
            .iter()
            .find(|&&i| readings[i].label.to_lowercase() == *want);
        if let Some(&i) = hit {
            return Some(i);
        }
    }
    // Fall back to anything of the right type, first by label and then on a
    // device whose name mentions the part we are after.
    let by_label = candidates
        .iter()
        .find(|&&i| readings[i].label.to_lowercase().starts_with(keyword));
    if let Some(&i) = by_label {
        return Some(i);
    }
    candidates
        .iter()
        .find(|&&i| readings[i].sensor.to_lowercase().contains(keyword))
        .copied()
}

/// A resolved sensor: where it sits in the reading table, plus enough of its
/// identity to notice when that position stops meaning the same thing.
#[derive(Clone)]
struct Choice {
    idx: usize,
    kind: u32,
    sensor: String,
    label: String,
}

impl Choice {
    fn resolve(
        readings: &[Reading],
        kind: u32,
        labels: &[&str],
        keyword: &str,
        explicit: Option<&str>,
    ) -> Option<Choice> {
        let idx = pick(readings, kind, labels, keyword, explicit)?;
        let reading = &readings[idx];
        Some(Choice {
            idx,
            kind: reading.kind,
            sensor: reading.sensor.clone(),
            label: reading.label.clone(),
        })
    }

    fn matches(&self, readings: &[Reading]) -> bool {
        readings.get(self.idx).is_some_and(|r| {
            r.kind == self.kind && r.label == self.label && r.sensor == self.sensor
        })
    }
}

/// The five readings the display wants.
///
/// These are resolved once and then reused by index, because searching every
/// reading by label each round would be wasteful. That shortcut is only safe
/// while the table is the one we searched: HWiNFO renumbers when it rescans, a
/// GPU wakes up, or a monitoring source is enabled — and a stale index does not
/// fail, it silently reports some *other* sensor's value. So each round checks
/// the identities still line up and re-resolves when they do not.
#[derive(Default)]
struct Picks {
    cpu: Option<Choice>,
    gpu: Option<Choice>,
    pump: Option<Choice>,
    cpu_power: Option<Choice>,
    gpu_power: Option<Choice>,
    /// How many readings HWiNFO published when this was resolved.
    len: usize,
}

impl Picks {
    fn resolve(readings: &[Reading], config: &Config) -> Picks {
        let temp = READING_TYPE_TEMPERATURE;
        Picks {
            cpu: Choice::resolve(readings, temp, CPU_LABELS, "cpu", config.cpu.as_deref()),
            gpu: Choice::resolve(readings, temp, GPU_LABELS, "gpu", config.gpu.as_deref()),
            pump: Choice::resolve(
                readings,
                READING_TYPE_FAN,
                PUMP_LABELS,
                "pump",
                config.pump.as_deref(),
            ),
            cpu_power: Choice::resolve(
                readings,
                READING_TYPE_POWER,
                CPU_POWER_LABELS,
                "cpu",
                config.cpu_power.as_deref(),
            ),
            gpu_power: Choice::resolve(
                readings,
                READING_TYPE_POWER,
                GPU_POWER_LABELS,
                "gpu",
                config.gpu_power.as_deref(),
            ),
            len: readings.len(),
        }
    }

    /// Tag, the flag that overrides it, and what it resolved to.
    fn rows(&self) -> [(&'static str, &'static str, &Option<Choice>); 5] {
        [
            ("CPU", "cpu", &self.cpu),
            ("GPU", "gpu", &self.gpu),
            ("PUMP", "pump", &self.pump),
            ("CPU W", "cpu-power", &self.cpu_power),
            ("GPU W", "gpu-power", &self.gpu_power),
        ]
    }

    fn stale(&self, readings: &[Reading]) -> bool {
        // A different number of readings means HWiNFO republished the table;
        // even entries that still look right may have moved.
        readings.len() != self.len
            || self
                .rows()
                .iter()
                .any(|(_, _, choice)| choice.as_ref().is_some_and(|c| !c.matches(readings)))
    }

    /// One line per reading, for the log. Also what tells us whether a
    /// re-resolve actually changed anything worth saying.
    fn describe(&self) -> String {
        let mut out = String::new();
        for (tag, flag, choice) in self.rows() {
            match choice {
                Some(c) => out.push_str(&format!("{tag}: {} / {}\n", c.sensor, c.label)),
                None => out.push_str(&format!(
                    "{tag}: no match — run with --list, then pass --{flag} \"...\"\n"
                )),
            }
        }
        out
    }

    fn value(choice: &Option<Choice>, readings: &[Reading]) -> Option<f64> {
        readings.get(choice.as_ref()?.idx).map(|r| r.value)
    }
}

pub fn fmt_temp(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() && (-50.0..=150.0).contains(&v) => format!("{v:.1}"),
        _ => "-".to_string(),
    }
}

pub fn fmt_rpm(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() && (0.0..=20_000.0).contains(&v) => format!("{v:.0}"),
        _ => "-".to_string(),
    }
}

pub fn fmt_watts(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() && (0.0..=2_000.0).contains(&v) => format!("{v:.0}"),
        _ => "-".to_string(),
    }
}

/// A port that is safe to write to.
pub struct ResolvedPort {
    pub name: String,
    /// Human-readable identification, shown on connect.
    pub device: String,
    /// Accepted only because `--force` was given: it is not a Pi device.
    pub forced: bool,
}

/// Decide which port to write to.
///
/// `Ok(None)` means "nothing suitable is plugged in yet, keep waiting".
/// `Err` means the request was unsafe and we should stop rather than retry.
pub fn resolve_port(requested: Option<&str>, force: bool) -> Result<Option<ResolvedPort>, String> {
    let Some(name) = requested else {
        return Ok(serial::find_pico_ports()
            .into_iter()
            .next()
            .map(|p| ResolvedPort {
                name: p.name,
                device: p.device,
                forced: false,
            }));
    };

    match serial::check_port(name) {
        serial::PortCheck::Pico(device) => Ok(Some(ResolvedPort {
            name: name.to_string(),
            device,
            forced: false,
        })),
        serial::PortCheck::Absent => Ok(None),
        serial::PortCheck::NotPico(_) if force => Ok(Some(ResolvedPort {
            name: name.to_string(),
            device: "not a Raspberry Pi device".to_string(),
            forced: true,
        })),
        serial::PortCheck::NotPico(others) => {
            let hint = if others.is_empty() {
                "no Raspberry Pi serial ports are present".to_string()
            } else {
                format!("Raspberry Pi ports present: {}", others.join(", "))
            };
            Err(format!(
                "{name} exists but is not a Raspberry Pi USB device ({hint}).\n\
                 Refusing to write to it — that could send garbage to whatever \
                 owns it. Pass --force to override."
            ))
        }
    }
}

enum Opened {
    Ready(SharedMem),
    /// HWiNFO is not there and we were told not to wait.
    Gone,
    /// The front end asked us to stop while we were waiting.
    Shutdown,
}

fn open_hwinfo(config: &Config, sink: &dyn Sink, status: &mut Status, control: &Control) -> Opened {
    loop {
        match SharedMem::open() {
            Ok(shm) => {
                status.hwinfo_ok = true;
                status.problem = None;
                sink.status(status);
                return Opened::Ready(shm);
            }
            Err(err) => {
                status.hwinfo_ok = false;
                status.problem = Some(err.clone());
                sink.status(status);
                if !config.wait_for_hwinfo {
                    sink.error(&err);
                    return Opened::Gone;
                }
                // Interruptible: waiting for HWiNFO must not hold up an exit,
                // and it must not outlast a pause either.
                if !control.sleep(3000) {
                    return Opened::Shutdown;
                }
            }
        }
    }
}

fn as_millis(duration: Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX)) as u32
}

/// Reports the port free once the loop is done with it, on every way out —
/// including the fatal ones. Anything waiting to talk to the Pico is blocked
/// until this says so, so a missed release would look like a hung device.
struct ReleaseOnDrop<'a>(&'a Control);

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.set_idle(true);
    }
}

pub fn run(config: &Config, sink: &dyn Sink, control: &Control) -> Result<(), String> {
    let mut status = Status::default();
    // Declared before `port` so it is dropped after it: the port has to be
    // closed before anyone is told it is free.
    let _release = ReleaseOnDrop(control);
    // Nothing is held until a port is opened, so the updater may start at once.
    control.set_idle(true);

    let mut shm = match open_hwinfo(config, sink, &mut status, control) {
        Opened::Ready(shm) => shm,
        Opened::Gone => return Err(status.problem.unwrap_or_default()),
        Opened::Shutdown => return Ok(()),
    };

    let mut port: Option<serial::Port> = None;
    let mut port_error_shown = false;

    let mut picks = Picks::default();
    let mut described = String::new();
    let mut resolved = false;

    let mut last_poll_time = 0i64;
    let mut last_poll_change = Instant::now();

    let interval = Duration::from_millis(u64::from(config.interval_ms.max(1)));
    let mut next_sample = Instant::now();

    loop {
        if control.shutting_down() {
            return Ok(());
        }

        // Standing down: let go of the port so the updater — or Thonny, or a
        // terminal — can have it, and park until the front end asks for it
        // back. The HWiNFO mapping is kept, so resuming is immediate.
        if !control.running() {
            if port.take().is_some() {
                sink.log("paused — released the serial port");
            }
            control.set_idle(true);
            status.connected = false;
            status.port = None;
            status.paused = true;
            status.problem = None;
            sink.status(&status);
            if !control.wait_until_running() {
                return Ok(());
            }
            status.paused = false;
            port_error_shown = false;
            next_sample = Instant::now();
            sink.log("resumed");
            sink.status(&status);
            continue;
        }

        // Claim the port *before* going anywhere near it, not after opening it.
        // Otherwise a pause arriving between the check above and the open below
        // would see "idle" and hand the port to the updater while this loop was
        // still on its way to taking it.
        if !config.dry_run {
            control.set_idle(false);
        }

        // Detect an HWiNFO restart: the block either goes invalid or freezes,
        // because a fresh instance publishes a brand new section.
        let readings = match shm.read_all() {
            Ok(r) => r,
            Err(err) => {
                sink.error(&format!("{err}; reconnecting to HWiNFO..."));
                resolved = false;
                status.hwinfo_ok = false;
                status.problem = Some(err);
                sink.status(&status);
                if !control.sleep(2000) {
                    return Ok(());
                }
                match open_hwinfo(config, sink, &mut status, control) {
                    Opened::Ready(fresh) => shm = fresh,
                    Opened::Gone => return Err(status.problem.unwrap_or_default()),
                    Opened::Shutdown => return Ok(()),
                }
                last_poll_change = Instant::now();
                continue;
            }
        };

        let poll_time = shm.poll_time();
        if poll_time != last_poll_time {
            last_poll_time = poll_time;
            last_poll_change = Instant::now();
        } else if last_poll_change.elapsed() > HWINFO_STALL {
            sink.error("HWiNFO stopped updating; reopening shared memory");
            match open_hwinfo(config, sink, &mut status, control) {
                Opened::Ready(fresh) => shm = fresh,
                // Losing the block entirely is the caller's problem; a frozen
                // one we can keep staring at in case it comes back.
                Opened::Gone => {}
                Opened::Shutdown => return Ok(()),
            }
            last_poll_change = Instant::now();
            resolved = false;
            continue;
        }
        status.hwinfo_ok = true;

        if !resolved || picks.stale(&readings) {
            picks = Picks::resolve(&readings, config);
            resolved = true;
            // Re-resolving is routine once HWiNFO rescans; only say so when
            // the answer actually changed.
            let fresh = picks.describe();
            if fresh != described {
                for line in fresh.lines() {
                    sink.log(line);
                }
                described = fresh;
            }
        }

        status.cpu = Picks::value(&picks.cpu, &readings);
        status.gpu = Picks::value(&picks.gpu, &readings);
        status.pump = Picks::value(&picks.pump, &readings);
        status.cpu_power = Picks::value(&picks.cpu_power, &readings);
        status.gpu_power = Picks::value(&picks.gpu_power, &readings);

        let line = format!(
            "T,{},{},{},{},{}\n",
            fmt_temp(status.cpu),
            fmt_temp(status.gpu),
            fmt_rpm(status.pump),
            fmt_watts(status.cpu_power),
            fmt_watts(status.gpu_power)
        );

        if !config.dry_run {
            if port.is_none() {
                match resolve_port(config.port.as_deref(), config.force)? {
                    Some(found) => {
                        if found.forced {
                            sink.error(&format!(
                                "WARNING: {} is not a Raspberry Pi device, --force given",
                                found.name
                            ));
                        } else {
                            sink.log(&format!("{} is {}", found.name, found.device));
                        }
                        match serial::Port::open(&found.name) {
                            Ok(p) => {
                                sink.log(&format!("connected to {}", p.name));
                                status.port = Some(p.name.clone());
                                status.connected = true;
                                status.problem = None;
                                port = Some(p);
                                port_error_shown = false;
                            }
                            Err(err) => {
                                if !port_error_shown {
                                    sink.error(&err);
                                    port_error_shown = true;
                                }
                                status.connected = false;
                                status.problem = Some(err);
                                sink.status(&status);
                                if !control.sleep(2000) {
                                    return Ok(());
                                }
                                continue;
                            }
                        }
                    }
                    None => {
                        let msg = "no Raspberry Pi USB serial port present";
                        if !port_error_shown {
                            sink.error(&format!("{msg}; waiting (or pass --port COMn)"));
                            port_error_shown = true;
                        }
                        status.connected = false;
                        status.port = None;
                        status.problem = Some(msg.to_string());
                        sink.status(&status);
                        if !control.sleep(2000) {
                            return Ok(());
                        }
                        continue;
                    }
                }
            }

            if let Some(p) = &mut port {
                if let Err(err) = p.write_line(&line) {
                    sink.error(&format!("{err}; will reconnect"));
                    port = None;
                    port_error_shown = false;
                    status.connected = false;
                    status.problem = Some(err);
                    sink.status(&status);
                    if !control.sleep(1000) {
                        return Ok(());
                    }
                    continue;
                }
            }
        }

        sink.sample(&status);

        // Pace against a fixed clock, so a round that took a while does not
        // push the next sample out by that much again.
        next_sample += interval;
        let now = Instant::now();
        if next_sample <= now {
            next_sample = now + interval; // fell behind; do not burst to catch up
        }
        if !control.sleep(as_millis(next_sample - now)) {
            return Ok(());
        }
    }
}
