//! Live progress on stderr for long syncs. Purely cosmetic: indicatif hides itself when stderr
//! isn't a terminal (cron, pipes), and [`Progress::hidden`] is a no-op handle for tests — the
//! recorded results never depend on it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

pub struct Progress {
    bar: Option<ProgressBar>,
    done_actions: AtomicU64,
    total_actions: u64,
}

impl Progress {
    /// A no-op handle (tests, library callers that don't want output).
    pub fn hidden() -> Self {
        Self { bar: None, done_actions: AtomicU64::new(0), total_actions: 0 }
    }

    /// A live bar for a sync: the bar itself tracks the bytes to copy (the time-dominating work);
    /// the message counts actions (renames/deletes/creates/copies) as they complete.
    pub fn for_sync(copy_bytes: u64, total_actions: u64) -> Self {
        let bar = ProgressBar::new(copy_bytes);
        bar.set_style(
            ProgressStyle::with_template(
                "  {prefix:<7} [{bar:28}] {percent:>3}%  {msg}  {binary_bytes_per_sec}  eta {eta}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        bar.set_prefix("sync");
        bar.enable_steady_tick(Duration::from_millis(200));
        let p = Self { bar: Some(bar), done_actions: AtomicU64::new(0), total_actions };
        p.update_message();
        p
    }

    /// Bytes written by a copy (advances the bar).
    pub fn add_bytes(&self, n: u64) {
        if let Some(b) = &self.bar {
            b.inc(n);
        }
    }

    /// One action (of any kind) finished.
    pub fn action_done(&self) {
        self.done_actions.fetch_add(1, Ordering::Relaxed);
        self.update_message();
    }

    /// Switch to the verify phase: `bytes` will be re-read back from the device.
    pub fn start_verify(&self, bytes: u64) {
        if let Some(b) = &self.bar {
            b.set_prefix("verify");
            b.set_length(bytes);
            b.set_position(0);
            b.set_message(String::new());
        }
    }

    pub fn finish(&self) {
        if let Some(b) = &self.bar {
            b.finish_and_clear();
        }
    }

    fn update_message(&self) {
        if let Some(b) = &self.bar {
            b.set_message(format!(
                "{}/{} actions",
                self.done_actions.load(Ordering::Relaxed),
                self.total_actions
            ));
        }
    }
}
