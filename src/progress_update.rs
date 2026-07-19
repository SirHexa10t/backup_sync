//! Live progress **updates** on stderr for long runs — updates, not reports: nothing here is ever
//! written to a file (everything a run *reports* lives in [`crate::reports`]). Purely cosmetic:
//! indicatif hides itself when stderr isn't a terminal (cron, pipes — where plain heartbeat lines
//! take over), and the `hidden()` constructors are no-op handles for tests — the recorded results
//! never depend on any of this.
//!
//! Two facilities: [`Progress`] for the apply/verify phases (totals are known → a real bar), and
//! [`ScanProgress`] for scans — which can't know their total in advance (discovering the total IS
//! the scan), so they report what they honestly know: entries seen and file bytes covered so far.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::units::{human_bytes, human_count, human_elapsed};

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
    /// the message counts actions (renames/deletes/creates/copies) as they complete. Above it, a
    /// one-line reminder that Ctrl+C stops the run CLEANLY (unix), so nobody hard-kills a sync out
    /// of caution.
    pub fn for_sync(copy_bytes: u64, total_actions: u64) -> Self {
        #[cfg(unix)]
        if std::io::stderr().is_terminal() {
            eprintln!(
                "filesync: Ctrl+C stops the run cleanly — the file in flight completes and the \
                 reports written so far are kept (renamed with an '-interrupted' marker); a \
                 second Ctrl+C aborts immediately"
            );
        }
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
                human_count(self.done_actions.load(Ordering::Relaxed)),
                human_count(self.total_actions)
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
            ScanMode::Terminal(scan_spinner(root))
        } else {
            ScanMode::Log { root: root.display().to_string(), last_heartbeat: Instant::now() }
        };
        Self { mode, entries: 0, bytes: 0, started: Instant::now() }
    }

    /// Like [`start`](Self::start), but the terminal spinner is added to a shared [`MultiProgress`]
    /// so two concurrent scans draw on their own lines instead of clobbering each other. Off a
    /// terminal it's identical to `start` (independent heartbeat lines, each naming its root).
    pub fn start_in(group: &MultiProgress, root: &Path) -> Self {
        let mode = if std::io::stderr().is_terminal() {
            ScanMode::Terminal(group.add(scan_spinner(root)))
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
                    bar.set_message(format!("{} entries  {}", human_count(self.entries), human_bytes(self.bytes)));
                }
            }
            ScanMode::Log { root, last_heartbeat } => {
                if last_heartbeat.elapsed() >= SCAN_HEARTBEAT {
                    *last_heartbeat = Instant::now();
                    eprintln!(
                        "filesync: scanning {root}: {} entries, {} so far",
                        human_count(self.entries),
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
                human_count(self.entries),
                human_bytes(self.bytes),
                human_elapsed(self.started.elapsed())
            ),
            ScanMode::Hidden => {}
        }
    }
}

/// A scan spinner with the shared style, ready to tick.
fn scan_spinner(root: &Path) -> ProgressBar {
    let bar = ProgressBar::new_spinner();
    bar.set_style(ProgressStyle::with_template("  {spinner} scan {prefix}  {msg}").unwrap());
    bar.set_prefix(root.display().to_string());
    bar.enable_steady_tick(Duration::from_millis(200));
    bar
}

/// Keeps the [`MultiProgress`] alive for the duration of two paired (concurrent) scans — hold it
/// until both finish (dropping it stops the coordinated drawing).
pub struct ScanPair {
    _group: MultiProgress,
}

/// Progress handles for two scans that will run concurrently (source and destination on different
/// devices). Both terminal spinners share one [`MultiProgress`] so they occupy separate lines.
/// Returns the guard (keep it alive) plus the two handles (move each into its scan thread).
pub fn scan_pair(a: &Path, b: &Path) -> (ScanPair, ScanProgress, ScanProgress) {
    let group = MultiProgress::new();
    let pa = ScanProgress::start_in(&group, a);
    let pb = ScanProgress::start_in(&group, b);
    (ScanPair { _group: group }, pa, pb)
}

/// Live feedback for the classification phase — the content hashing done for move-detection (and,
/// under `--eager` or on mtime drift, for same-path comparisons), which is otherwise silent. Like
/// [`Progress`] it uses `&self` + atomics, so the two parallel move-detection hash passes can share
/// one handle. On a terminal: a spinner counting files/bytes hashed. Off-terminal: quiet during the
/// work, with one summary line at the end (skipped entirely when nothing was hashed).
pub struct CompareProgress {
    mode: CompareMode,
    files: AtomicU64,
    bytes: AtomicU64,
    /// Move-detection candidate count; 0 until known — once set, the spinner shows `N/total`.
    move_total: AtomicU64,
    started: Instant,
}

enum CompareMode {
    Terminal(ProgressBar),
    Log,
    Hidden,
}

impl CompareProgress {
    /// A no-op handle (tests, and library paths that don't want output).
    pub fn hidden() -> Self {
        Self::with_mode(CompareMode::Hidden)
    }

    /// Start feedback for the compare phase, picking the terminal or log style automatically.
    pub fn start() -> Self {
        let mode = if std::io::stderr().is_terminal() {
            let bar = ProgressBar::new_spinner();
            bar.set_style(ProgressStyle::with_template("  {spinner} compare  {msg}").unwrap());
            bar.enable_steady_tick(Duration::from_millis(200));
            CompareMode::Terminal(bar)
        } else {
            CompareMode::Log
        };
        Self::with_mode(mode)
    }

    fn with_mode(mode: CompareMode) -> Self {
        Self {
            mode,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            move_total: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    /// Announce how many move-detection candidates are about to be hashed, so the spinner can show
    /// `N/total`. Call once, before that hashing.
    pub fn set_move_total(&self, n: u64) {
        self.move_total.store(n, Ordering::Relaxed);
    }

    /// Record one file hashed (`bytes` of content read). Cheap — two relaxed atomic adds; the
    /// terminal message is refreshed only occasionally (the steady tick redraws it anyway).
    pub fn hashed(&self, bytes: u64) {
        let f = self.files.fetch_add(1, Ordering::Relaxed) + 1;
        let b = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if let CompareMode::Terminal(bar) = &self.mode {
            if f == 1 || f % 16 == 0 {
                let total = self.move_total.load(Ordering::Relaxed);
                bar.set_message(if total > 0 {
                    format!("{}/{} files, {} hashed", human_count(f), human_count(total), human_bytes(b))
                } else {
                    format!("{} files, {} hashed", human_count(f), human_bytes(b))
                });
            }
        }
    }

    /// Done: clear the spinner (terminal), or leave one summary line (log) — but only if any
    /// content was actually hashed (a plain diff with no moves and no `--eager` hashes nothing).
    pub fn finish(&self) {
        let files = self.files.load(Ordering::Relaxed);
        match &self.mode {
            CompareMode::Terminal(bar) => bar.finish_and_clear(),
            CompareMode::Log if files > 0 => eprintln!(
                "filesync: compared {} file(s) ({}) in {}",
                human_count(files),
                human_bytes(self.bytes.load(Ordering::Relaxed)),
                human_elapsed(self.started.elapsed())
            ),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_scan_progress_is_a_quiet_counter() {
        let mut p = ScanProgress::hidden();
        p.tick(100);
        p.tick(0);
        assert_eq!((p.entries, p.bytes), (2, 100));
        p.finish(); // no output, no panic
    }
}
