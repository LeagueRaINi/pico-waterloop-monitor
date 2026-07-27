//! Start/stop handle shared between a front end and the sampling loop.
//!
//! The point of this is the serial port: only one program can hold it, so
//! anything that wants to talk to the Pico — the built-in updater, Thonny, a
//! terminal — needs the loop to let go first. Pausing is therefore not just
//! "stop sending"; it is a handshake, and `idle` is the loop answering "the
//! port is yours".
//!
//! Everything lives under one mutex with a condvar rather than a set of
//! atomics, so a paused loop parks instead of polling and a pause takes effect
//! within a round of the loop instead of after the sampling interval.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Flags {
    /// What the front end wants: sample and send, or stand down.
    running: bool,
    /// Set once, to unwind the loop for good.
    shutdown: bool,
    /// What the loop reports: true while it is *not* holding the serial port.
    idle: bool,
}

pub struct Control {
    flags: Mutex<Flags>,
    changed: Condvar,
}

impl Control {
    pub fn new(running: bool) -> Arc<Control> {
        Arc::new(Control {
            flags: Mutex::new(Flags {
                running,
                shutdown: false,
                idle: true,
            }),
            changed: Condvar::new(),
        })
    }

    /// A poisoned lock only means some other thread panicked while holding it;
    /// these are three bools, and refusing to run the bridge over it would be
    /// worse than carrying on.
    fn lock(&self) -> MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn running(&self) -> bool {
        let flags = self.lock();
        flags.running && !flags.shutdown
    }

    pub fn shutting_down(&self) -> bool {
        self.lock().shutdown
    }

    pub fn set_running(&self, on: bool) {
        let mut flags = self.lock();
        if flags.running != on {
            flags.running = on;
            self.changed.notify_all();
        }
    }

    pub fn shutdown(&self) {
        self.lock().shutdown = true;
        self.changed.notify_all();
    }

    /// Called by the loop whenever it takes or releases the serial port.
    pub fn set_idle(&self, idle: bool) {
        let mut flags = self.lock();
        if flags.idle != idle {
            flags.idle = idle;
            self.changed.notify_all();
        }
    }

    /// Wait for the loop to confirm it has let go of the port. False on
    /// timeout, which means something else must still be holding it.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut flags = self.lock();
        while !flags.idle {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, wait) = self
                .changed
                .wait_timeout(flags, left)
                .unwrap_or_else(|e| e.into_inner());
            flags = guard;
            if wait.timed_out() && !flags.idle {
                return false;
            }
        }
        true
    }

    /// Sleep up to `ms`, returning early if anything changed. False means the
    /// caller should return: the loop is being shut down.
    ///
    /// Every wait in the sampling loop goes through here, so a pause or an exit
    /// is never held up by an interval or a retry backoff.
    pub fn sleep(&self, ms: u32) -> bool {
        let deadline = Instant::now() + Duration::from_millis(u64::from(ms));
        let mut flags = self.lock();
        let was_running = flags.running;
        while !flags.shutdown && flags.running == was_running {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let (guard, _) = self
                .changed
                .wait_timeout(flags, left)
                .unwrap_or_else(|e| e.into_inner());
            flags = guard;
        }
        !flags.shutdown
    }

    /// Park until the front end asks for the loop again. False means shut down.
    pub fn wait_until_running(&self) -> bool {
        let mut flags = self.lock();
        while !flags.running && !flags.shutdown {
            flags = self.changed.wait(flags).unwrap_or_else(|e| e.into_inner());
        }
        !flags.shutdown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Nudge the control from another thread once the waiter is parked.
    fn shortly(control: &Arc<Control>, change: impl FnOnce(&Control) + Send + 'static) {
        let control = Arc::clone(control);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            change(&control);
        });
    }

    #[test]
    fn a_pause_cuts_a_long_sleep_short() {
        // The whole point: the tray must not have to wait out a sampling
        // interval, or a retry backoff, before the port comes free.
        let control = Control::new(true);
        shortly(&control, |c| c.set_running(false));

        let start = Instant::now();
        assert!(control.sleep(10_000), "a pause is not a shutdown");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the sleep was not interrupted"
        );
    }

    #[test]
    fn a_shutdown_ends_a_sleep_and_says_so() {
        let control = Control::new(true);
        shortly(&control, Control::shutdown);
        assert!(!control.sleep(10_000));
    }

    #[test]
    fn wait_idle_waits_for_the_loop_to_let_go() {
        let control = Control::new(true);
        control.set_idle(false);
        assert!(
            !control.wait_idle(Duration::from_millis(100)),
            "should time out while the port is still held"
        );

        shortly(&control, |c| c.set_idle(true));
        assert!(control.wait_idle(Duration::from_secs(5)));
    }

    #[test]
    fn a_paused_loop_parks_until_resumed() {
        let control = Control::new(false);
        shortly(&control, |c| c.set_running(true));
        assert!(control.wait_until_running());
        assert!(control.running());
    }

    #[test]
    fn a_shutdown_releases_a_parked_loop() {
        let control = Control::new(false);
        shortly(&control, Control::shutdown);
        assert!(!control.wait_until_running());
        assert!(!control.running(), "shutting down is not running");
    }
}
