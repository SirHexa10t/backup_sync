//! Live progress on stderr for long syncs. Purely cosmetic: indicatif hides itself when stderr
//! isn't a terminal (cron, pipes), and the `hidden()` constructors are no-op handles for tests —
//! the recorded results never depend on any of this.
//!
//! Two facilities: [`Progress`] for the apply/verify phases (totals are known → a real bar), and
//! [`ScanProgress`] for scans — which can't know their total in advance (discovering the total IS
//! the scan), so they report what they honestly know: entries seen and file bytes covered so far.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

/// How often the log-mode heartbeat line is emitted during a scan.
const SCAN_HEARTBEAT: Duration = Duration::from_secs(10);

/// Live feedback for one scan. On a terminal: a single updating spinner line. With stderr
/// redirected (cron, `2> log`): an occasional plain heartbeat line plus a completion summary —
/// so an unattended overnight run stays visibly alive in its log, and a hung drive is
/// distinguishable from a slow one.
pub struct ScanProgress {
    mode: ScanMode,
    entries: u64,
    bytes: u64,
    started: Instant,
}

enum ScanMode {
    Terminal(ProgressBar),
    Log { root: String, last_heartbeat: Instant },
    Hidden,
}

impl ScanProgress {
    /// A no-op handle (tests, and the plain `scan()` helper).
    pub fn hidden() -> Self {
        Self { mode: ScanMode::Hidden, entries: 0, bytes: 0, started: Instant::now() }
    }

    /// Start feedback for scanning `root`, picking the terminal or log style automatically.
    pub fn start(root: &Path) -> Self {
        let mode = if std::io::stderr().is_terminal() {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("  {spinner} scan {prefix}  {msg}").unwrap(),
            );
            bar.set_prefix(root.display().to_string());
            bar.enable_steady_tick(Duration::from_millis(200));
            ScanMode::Terminal(bar)
        } else {
            ScanMode::Log { root: root.display().to_string(), last_heartbeat: Instant::now() }
        };
        Self { mode, entries: 0, bytes: 0, started: Instant::now() }
    }

    /// One entry examined; `bytes` is its file size (0 for non-files).
    pub fn tick(&mut self, bytes: u64) {
        self.entries += 1;
        self.bytes += bytes;
        match &mut self.mode {
            ScanMode::Terminal(bar) => {
                // refresh the text every 64 entries — the steady tick redraws it at 200ms anyway
                if self.entries % 64 == 0 {
                    bar.set_message(format!("{} entries  {}", self.entries, human_bytes(self.bytes)));
                }
            }
            ScanMode::Log { root, last_heartbeat } => {
                if last_heartbeat.elapsed() >= SCAN_HEARTBEAT {
                    *last_heartbeat = Instant::now();
                    eprintln!(
                        "filesync: scanning {root}: {} entries, {} so far",
                        self.entries,
                        human_bytes(self.bytes)
                    );
                }
            }
            ScanMode::Hidden => {}
        }
    }

    /// Scan done: clear the live line (terminal), or leave one summary line (log mode).
    pub fn finish(self) {
        match self.mode {
            ScanMode::Terminal(bar) => bar.finish_and_clear(),
            ScanMode::Log { root, .. } => eprintln!(
                "filesync: scanned {root}: {} entries ({}) in {}",
                self.entries,
                human_bytes(self.bytes),
                human_elapsed(self.started.elapsed())
            ),
            ScanMode::Hidden => {}
        }
    }
}

fn human_bytes(b: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if b >= GIB {
        format!("{:.1} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.1} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.0} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} bytes")
    }
}

fn human_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_humanized() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(8 << 10), "8 KiB");
        assert_eq!(human_bytes(5 << 20), "5.0 MiB");
        assert_eq!(human_bytes(3 << 30), "3.0 GiB");
    }

    #[test]
    fn elapsed_is_humanized() {
        assert_eq!(human_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(human_elapsed(Duration::from_secs(125)), "2m5s");
        assert_eq!(human_elapsed(Duration::from_secs(3700)), "1h1m");
    }

    #[test]
    fn hidden_scan_progress_is_a_quiet_counter() {
        let mut p = ScanProgress::hidden();
        p.tick(100);
        p.tick(0);
        assert_eq!((p.entries, p.bytes), (2, 100));
        p.finish(); // no output, no panic
    }
}
