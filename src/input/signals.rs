//! Releasing held input when the bridge is told to stop.
//!
//! A virtual keyboard's held keys go away with the device when the process
//! exits, but the depressed mask sent with `modifiers()` is compositor state:
//! smithay (niri) never resets it when the device is destroyed, so a bridge
//! killed in the middle of `hold-key ctrl ...` or `pointer-click --modifier`
//! would leave Ctrl held for the rest of the session. Commands that hold a
//! modifier across a wait install [`StopSignals`]: SIGTERM, SIGINT and SIGHUP
//! then only set a flag, the wait ends early, the command runs its normal
//! release path (keys up, mask 0, roundtrip) and the process exits with
//! `128 + signal`. SIGKILL cannot be caught.

use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const SIGHUP: i32 = 1;
pub(crate) const SIGINT: i32 = 2;
pub(crate) const SIGTERM: i32 = 15;

/// Granularity of [`StopSignals::sleep`]: `thread::sleep` resumes after a
/// signal, so a long wait is cut into slices that check the flag.
const SLICE_MS: u64 = 10;

/// The last stop signal received, or 0.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

/// Install a signal handler.
///
/// SAFETY: `handler` must be async-signal-safe (ours only do an atomic store).
pub(crate) unsafe fn install_handler(sig: i32, handler: extern "C" fn(i32)) {
    unsafe {
        signal(sig, handler);
    }
}

extern "C" fn on_stop(sig: i32) {
    RECEIVED.store(sig, Ordering::SeqCst);
}

/// Catches SIGTERM / SIGINT / SIGHUP for the rest of the process so a command
/// can release what it holds before exiting.
pub(crate) struct StopSignals;

impl StopSignals {
    pub(crate) fn install() -> Self {
        // SAFETY: on_stop is a single atomic store.
        unsafe {
            install_handler(SIGTERM, on_stop);
            install_handler(SIGINT, on_stop);
            install_handler(SIGHUP, on_stop);
        }
        Self
    }

    /// Whether a stop signal has arrived.
    fn received(&self) -> bool {
        RECEIVED.load(Ordering::SeqCst) != 0
    }

    /// Sleep for `duration`, returning early (with `true`) on a stop signal.
    pub(crate) fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.received() {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            thread::sleep(left.min(Duration::from_millis(SLICE_MS)));
        }
    }

    /// Call once everything held is released: exit `128 + signal` if a stop
    /// signal arrived, as the default action would have.
    pub(crate) fn exit_if_received() {
        let sig = RECEIVED.load(Ordering::SeqCst);
        if sig != 0 {
            std::process::exit(128 + sig);
        }
    }
}
