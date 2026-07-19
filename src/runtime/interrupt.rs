//! Cooperative early-stop.
//!
//! A signal (SIGINT from Ctrl+C, or SIGTERM from `kill <pid>`) flips a process-global flag; the
//! apply loop checks it **between files** and stops after finishing the current one — so the run
//! ends cleanly (what was written is made durable, verified, and reported) instead of being killed
//! mid-write. The current file completes because the flag is only ever checked between actions.
//!
//! A **second** signal aborts immediately: the handler restores the default disposition on the
//! first signal, so the next one terminates the process — the escape hatch when the in-flight file
//! is large. filesync is atomic + resumable, so even a hard abort is safe (a re-run finishes the
//! job); the graceful path just additionally flushes and reports what it managed to do.
//!
//! The flag is global because a signal handler can carry no context, but `apply` receives it as an
//! explicit `&AtomicBool` (see [`global`]) so its stop logic stays testable without real signals.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

/// The process-wide stop flag the signal handlers set. `run_sync` hands this to `apply`; tests pass
/// their own [`AtomicBool`] instead, so they can exercise the stop path in isolation.
pub fn global() -> &'static AtomicBool {
    &STOP
}

/// Whether a graceful stop has been requested (the flag is set).
pub fn requested() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// Install the handlers: the first SIGINT/SIGTERM requests a graceful stop; the next aborts (the
/// default disposition is restored, so it terminates the process). Unix only — elsewhere Ctrl+C
/// keeps its default behavior (a hard stop, but still safe: writes are atomic and the run resumes).
#[cfg(unix)]
pub fn arm() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(not(unix))]
pub fn arm() {}

/// The signal handler — restricted to async-signal-safe operations only (an atomic store, `write`,
/// and `signal`; NOT `println!`/allocation/etc.).
#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
    const MSG: &[u8] =
        b"\nfilesync: stopping cleanly - finishing the current step... (signal again to abort immediately)\n";
    unsafe {
        let _ = libc::write(2, MSG.as_ptr() as *const libc::c_void, MSG.len() as libc::size_t);
        // Restore the default disposition so a SECOND signal aborts the process immediately.
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
}
