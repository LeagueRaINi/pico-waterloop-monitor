//! System tray icon, on top of the `tray-icon` crate.
//!
//! `TrayIcon` is not `Send`, so the worker threads never touch it: they park
//! the latest status behind a mutex and raise a flag, and the UI thread — which
//! also has to pump Windows messages for the shell to deliver clicks — picks
//! the change up and applies it.
//!
//! That is also why the menu actions are callbacks rather than work done here:
//! updating the Pico takes seconds, and doing it on this thread would freeze
//! the icon and the menu while it ran.

use crate::bridge::Status;
use crate::control::Control;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const ICON_SIZE: u32 = 16;
/// How often the UI thread looks for a status change and drains messages.
const UI_TICK: Duration = Duration::from_millis(150);

static STATE: OnceLock<Mutex<Status>> = OnceLock::new();
/// What a long-running action wants said instead of the usual summary.
static ACTIVITY: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// A message box the UI thread should put up when it next gets a turn.
static NOTICE: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
static BUSY: AtomicBool = AtomicBool::new(false);
static DIRTY: AtomicBool = AtomicBool::new(true);
static QUIT: AtomicBool = AtomicBool::new(false);

/// A poisoned lock here would mean losing the icon over a status string.
fn guard<T: Default>(cell: &'static OnceLock<Mutex<T>>) -> MutexGuard<'static, T> {
    cell.get_or_init(|| Mutex::new(T::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Called from the worker thread.
pub fn set_status(status: &Status) {
    *guard(&STATE) = status.clone();
    DIRTY.store(true, Ordering::Relaxed);
}

/// One line about whatever is currently going on, or `None` for the usual
/// status readout.
pub fn set_activity(line: Option<&str>) {
    *guard(&ACTIVITY) = line.map(str::to_string);
    DIRTY.store(true, Ordering::Relaxed);
}

/// True while a menu action is still running.
pub fn busy() -> bool {
    BUSY.load(Ordering::SeqCst)
}

pub fn set_busy(on: bool) {
    BUSY.store(on, Ordering::SeqCst);
    DIRTY.store(true, Ordering::Relaxed);
}

/// Queue a message box. Worker threads use this rather than calling
/// `message_box` themselves, so the dialog belongs to the UI thread.
pub fn notify(title: &str, text: &str) {
    *guard(&NOTICE) = Some((title.to_string(), text.to_string()));
    DIRTY.store(true, Ordering::Relaxed);
}

/// A message box is the only way a windows-subsystem binary can say anything,
/// so keep it for genuinely user-facing messages.
///
/// Blocks until dismissed, so it belongs on the UI thread — which is what
/// `NOTICE` exists to arrange for the worker threads.
pub fn message_box(title: &str, text: &str) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_level(rfd::MessageLevel::Info)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

// ------------------------------------------------------------------ icon

/// A flat disc in the given colour.
fn disc(r: u8, g: u8, b: u8) -> Option<Icon> {
    let size = ICON_SIZE as usize;
    let mut rgba = vec![0u8; size * size * 4];
    let centre = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            if (dx * dx + dy * dy).sqrt() <= radius {
                let i = (y * size + x) * 4;
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).ok()
}

/// What the icon is saying. Kept as a value so the icon is only rebuilt when
/// the meaning changes rather than once per sample.
#[derive(Clone, Copy, PartialEq)]
enum Look {
    Sending,
    LinkDown,
    NoData,
    Standby,
}

fn look(status: &Status) -> Look {
    if status.paused || busy() {
        Look::Standby
    } else if status.healthy() {
        Look::Sending
    } else if status.hwinfo_ok {
        Look::LinkDown
    } else {
        Look::NoData
    }
}

fn icon_for(look: Look) -> Option<Icon> {
    match look {
        Look::Sending => disc(0, 200, 170),   // teal: sending
        Look::LinkDown => disc(255, 170, 60), // amber: HWiNFO fine, link isn't
        Look::NoData => disc(230, 70, 70),    // red: no data at all
        Look::Standby => disc(140, 146, 155), // grey: deliberately not sending
    }
}

// --------------------------------------------------------------- text

/// How much text the shell will actually show, in UTF-16 units.
///
/// Not the 127 you would expect: `tray-icon` leaves `cbSize` zeroed in the
/// `NOTIFYICONDATAW` it passes to `Shell_NotifyIconW`, and with no recognisable
/// struct version the shell falls back to the original 64-unit `szTip` rather
/// than the 128-unit one. Overrun that and the tail is silently dropped — which
/// is how a four-digit pump reading came to be displayed as three digits.
const TOOLTIP_UNITS: usize = 63;

/// Same bounds the wire format applies, so the tooltip says `--` exactly when
/// the display does — and so one deranged sensor reading cannot push the pump
/// off the end of a fixed-size tooltip.
fn rounded(value: Option<f64>, lo: f64, hi: f64) -> String {
    match value {
        Some(v) if v.is_finite() && (lo..=hi).contains(&v) => format!("{v:.0}"),
        _ => "--".to_string(),
    }
}

fn temp(value: Option<f64>) -> String {
    rounded(value, -50.0, 150.0)
}

fn watts(value: Option<f64>) -> String {
    rounded(value, 0.0, 2_000.0)
}

fn rpm(value: Option<f64>) -> String {
    rounded(value, 0.0, 20_000.0)
}

fn clamp_to_tooltip(text: &str) -> String {
    if text.encode_utf16().count() <= TOOLTIP_UNITS {
        return text.to_string();
    }
    let mut out = String::new();
    let mut units = 0;
    for ch in text.chars() {
        units += ch.len_utf16();
        if units > TOOLTIP_UNITS {
            break;
        }
        out.push(ch);
    }
    out
}

fn tooltip(status: &Status, activity: Option<&str>) -> String {
    // Which port it is goes in the menu instead: at 63 units, spelling it out
    // here would cost the readings it exists to show.
    let text = if let Some(line) = activity {
        format!("Pico bridge\n{line}")
    } else if status.paused {
        "Pico bridge\nPaused — the Pico's port is free".to_string()
    } else if let Some(problem) = &status.problem {
        format!("Pico bridge\n{problem}")
    } else {
        format!(
            "Pico bridge\nCPU {} C {} W\nGPU {} C {} W\nPump {} RPM",
            temp(status.cpu),
            watts(status.cpu_power),
            temp(status.gpu),
            watts(status.gpu_power),
            rpm(status.pump)
        )
    };
    clamp_to_tooltip(&text)
}

fn summary(status: &Status, activity: Option<&str>) -> String {
    if let Some(line) = activity {
        return line.to_string();
    }
    if status.paused {
        return "Paused — the Pico's port is free".to_string();
    }
    match &status.port {
        Some(port) if status.connected => format!("Sending to {port}"),
        _ => "Not connected".to_string(),
    }
}

// ------------------------------------------------------------- lifecycle

struct Tray {
    icon: TrayIcon,
    summary_item: MenuItem,
    pause_item: MenuItem,
    update_item: MenuItem,
    pause_id: MenuId,
    update_id: MenuId,
    exit_id: MenuId,
    /// What the icon currently shows, so it is not rebuilt every second.
    shown: Option<Look>,
}

fn build() -> Result<Tray, String> {
    let status = guard(&STATE).clone();

    // A status readout, so it is deliberately disabled. The readings are not
    // repeated here: Windows only reads a menu item's text as the popup opens,
    // so they would sit frozen on screen while the tooltip shows them live.
    let summary_item = MenuItem::new(summary(&status, None), false, None);
    let pause_item = MenuItem::new("Pause", true, None);
    let update_item = MenuItem::new("Update Pico...", true, None);
    let exit_item = MenuItem::new("Exit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &summary_item,
        &PredefinedMenuItem::separator(),
        &pause_item,
        &update_item,
        &PredefinedMenuItem::separator(),
        &exit_item,
    ])
    .map_err(|e| format!("could not build the tray menu: {e}"))?;

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip(&status, None))
        .with_menu_on_left_click(true);
    if let Some(icon) = icon_for(look(&status)) {
        builder = builder.with_icon(icon);
    }

    let icon = builder
        .build()
        .map_err(|e| format!("could not create the tray icon: {e}"))?;

    Ok(Tray {
        icon,
        summary_item,
        pause_id: pause_item.id().clone(),
        update_id: update_item.id().clone(),
        exit_id: exit_item.id().clone(),
        pause_item,
        update_item,
        shown: Some(look(&status)),
    })
}

fn apply(tray: &mut Tray, control: &Control) {
    let status = guard(&STATE).clone();
    let activity = guard(&ACTIVITY).clone();
    let activity = activity.as_deref();

    let _ = tray.icon.set_tooltip(Some(tooltip(&status, activity)));

    let wanted = look(&status);
    if tray.shown != Some(wanted) {
        if let Some(icon) = icon_for(wanted) {
            let _ = tray.icon.set_icon(Some(icon));
            tray.shown = Some(wanted);
        }
    }

    tray.summary_item.set_text(summary(&status, activity));
    tray.pause_item
        .set_text(if control.running() { "Pause" } else { "Resume" });
    // Both would fight over the serial port, and pausing under a running
    // update would strand it.
    tray.pause_item.set_enabled(!busy());
    tray.update_item.set_enabled(!busy());
}

/// Drain the Windows message queue. The shell delivers tray clicks as window
/// messages, so without this the icon would be inert.
fn pump_messages() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
    };

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                QUIT.store(true, Ordering::Relaxed);
                return;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Create the icon and service it until the user picks Exit. Must run on the
/// thread that owns the message loop, i.e. the main thread.
///
/// `start_update` is called when the user asks to update the Pico; it is
/// expected to return immediately and do the work on a thread of its own.
///
/// Returns an error if the icon could not be created, so the caller can decide
/// to carry on headless rather than give up entirely.
pub fn run(control: &Control, start_update: &dyn Fn()) -> Result<(), String> {
    let mut tray = build()?;
    let menu_events = MenuEvent::receiver();

    while !QUIT.load(Ordering::Relaxed) {
        pump_messages();

        while let Ok(event) = menu_events.try_recv() {
            if event.id == tray.exit_id {
                if busy() {
                    // Exiting mid-transfer would leave the Pico half-written
                    // and sitting at a prompt.
                    notify(
                        "HWiNFO Pico Bridge",
                        "The Pico is being updated. Exit once that has finished.",
                    );
                    continue;
                }
                return Ok(());
            } else if event.id == tray.pause_id && !busy() {
                control.set_running(!control.running());
                DIRTY.store(true, Ordering::Relaxed);
            } else if event.id == tray.update_id && !busy() {
                start_update();
            }
        }

        if DIRTY.swap(false, Ordering::Relaxed) {
            apply(&mut tray, control);
        }

        // Taken out of the slot before the dialog goes up: MessageBoxW blocks
        // here until dismissed, and a worker finishing meanwhile should queue
        // its own notice rather than have this one show twice.
        let notice = guard(&NOTICE).take();
        if let Some((title, text)) = notice {
            message_box(&title, &text);
        }

        std::thread::sleep(UI_TICK);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> usize {
        text.encode_utf16().count()
    }

    #[test]
    fn the_worst_case_tooltip_still_shows_the_whole_pump_reading() {
        // The regression: the tooltip ran to 69 units, the shell cut it at 63,
        // and the pump's four digits arrived as three.
        let status = Status {
            cpu: Some(150.0),
            gpu: Some(150.0),
            cpu_power: Some(1999.0),
            gpu_power: Some(1999.0),
            pump: Some(19999.0),
            port: Some("COM256".to_string()),
            hwinfo_ok: true,
            connected: true,
            ..Default::default()
        };

        let text = tooltip(&status, None);
        assert!(
            text.ends_with("Pump 19999 RPM"),
            "the pump reading was cut: {text:?}"
        );
        assert!(
            units(&text) <= TOOLTIP_UNITS,
            "{} units, budget is {TOOLTIP_UNITS}",
            units(&text)
        );
    }

    #[test]
    fn a_nonsense_reading_reads_as_missing_rather_than_overflowing() {
        let status = Status {
            pump: Some(1.0e9),
            ..Default::default()
        };
        assert!(tooltip(&status, None).ends_with("Pump -- RPM"));
    }

    #[test]
    fn long_problem_text_is_cut_rather_than_dropped_by_the_shell() {
        let status = Status {
            problem: Some("x".repeat(200)),
            ..Default::default()
        };
        assert_eq!(units(&tooltip(&status, None)), TOOLTIP_UNITS);
    }
}
