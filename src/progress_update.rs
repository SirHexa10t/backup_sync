//! Live progress **updates** on stderr for long runs — updates, not reports: nothing here is ever
//! written to a file (everything a run *reports* lives in [`crate::reports`]). Purely cosmetic:
//! everything hides itself when stderr isn't a terminal (cron, pipes — where plain heartbeat lines
//! take over), and the `hidden()` constructors are no-op handles for tests — the recorded results
//! never depend on any of this.
//!
//! Three facilities: [`Progress`] for the apply/verify phases (totals are known → a real bar),
//! [`ScanProgress`] for scans — which can't know their total in advance (discovering the total IS
//! the scan), so they report what they honestly know: entries seen and file bytes covered so far —
//! and [`CompareProgress`] for the classification phase's content hashing.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

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
                human_count(self.done_actions.load(Ordering::Relaxed)),
                human_count(self.total_actions)
            ));
        }
    }
}

/// One-line guidance printed at the very start of a run, ABOVE all status updates — before the
/// first Ctrl+C temptation, not at the copy phase (a scan of a big drive takes minutes, and the
/// user must know the clean-stop option exists while watching it). Terminal-only (it's interactive
/// guidance, noise in a log) and unix-only (elsewhere Ctrl+C is a plain abort).
pub fn print_stop_hint() {
    #[cfg(unix)]
    if std::io::stderr().is_terminal() {
        eprintln!(
            "filesync: press Ctrl+C once to stop cleanly — filesync wraps up, keeps what it \
             finished, and reports what was done; press it twice to abort immediately"
        );
    }
}

/// How often the log-mode heartbeat line is emitted during a scan.
const SCAN_HEARTBEAT: Duration = Duration::from_secs(10);

/// Live feedback for one scan. On a terminal: a single updating spinner line (or one of the two
/// lines of a paired scan — see [`scan_pair`]). With stderr redirected (cron, `2> log`): an
/// occasional plain heartbeat line plus a completion summary — so an unattended overnight run
/// stays visibly alive in its log, and a hung drive is distinguishable from a slow one.
pub struct ScanProgress {
    mode: ScanMode,
    entries: u64,
    bytes: u64,
    started: Instant,
}

enum ScanMode {
    Terminal(ProgressBar),
    /// One side of a paired scan: two stacked lines drawn by our own renderer (see [`pair_ticker`]).
    PairTerminal { state: Arc<PairState>, side: usize },
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
            bar.set_style(ProgressStyle::with_template("  {spinner} scan {prefix}  {msg}").unwrap());
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
                    bar.set_message(format!(
                        "{} entries  {}",
                        human_count(self.entries),
                        human_bytes(self.bytes)
                    ));
                }
            }
            ScanMode::PairTerminal { state, side } => {
                // atomics only — the renderer thread reads them on its own cadence
                state.entries[*side].store(self.entries, Ordering::Relaxed);
                state.bytes[*side].store(self.bytes, Ordering::Relaxed);
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

    /// Scan done: clear the live line (terminal), or leave one summary line (log mode). A paired
    /// side posts its final numbers; the renderer erases both lines once BOTH sides finished.
    pub fn finish(self) {
        match self.mode {
            ScanMode::Terminal(bar) => bar.finish_and_clear(),
            ScanMode::PairTerminal { state, side } => {
                state.entries[side].store(self.entries, Ordering::Relaxed);
                state.bytes[side].store(self.bytes, Ordering::Relaxed);
                state.finished.fetch_add(1, Ordering::SeqCst);
            }
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

/// The shared state of two concurrent scans, displayed as TWO stacked lines (one per drive) by our
/// own minimal ANSI renderer. indicatif's `MultiProgress` was tried first and orphans early frames
/// on real terminals (verified by terminal emulation: frozen "⠁ scan …" lines survive the run,
/// under every call pattern) — so the pair is painted by hand: ONE ticker thread owns the drawing,
/// always erases exactly what it drew (cursor-up + clear-line per frame), truncates each line to
/// the terminal width so wrapping can never corrupt the redraw arithmetic, and clears both lines
/// when the second scan finishes. The scan threads only bump atomics — they never touch the
/// terminal.
struct PairState {
    labels: [String; 2],
    entries: [AtomicU64; 2],
    bytes: [AtomicU64; 2],
    finished: AtomicU64,
}

impl PairState {
    fn line(&self, side: usize, glyph: char) -> String {
        format!(
            "  {glyph} scan {}  {} entries  {}",
            self.labels[side],
            human_count(self.entries[side].load(Ordering::Relaxed)),
            human_bytes(self.bytes[side].load(Ordering::Relaxed))
        )
    }
}

/// Guard for a paired scan — hold it until both handles finished; dropping it joins the renderer
/// thread, which guarantees the two lines are erased before any later output prints.
pub struct ScanPair {
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ScanPair {
    fn drop(&mut self) {
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
    }
}

/// Progress handles for two scans that will run concurrently (source and destination on different
/// devices), each on its own terminal line. Returns the guard (keep it alive) plus the two handles
/// (move each into its scan thread). Off a terminal: two independent heartbeat logs, as before.
pub fn scan_pair(a: &Path, b: &Path) -> (ScanPair, ScanProgress, ScanProgress) {
    if !std::io::stderr().is_terminal() {
        let log = |root: &Path| ScanProgress {
            mode: ScanMode::Log { root: root.display().to_string(), last_heartbeat: Instant::now() },
            entries: 0,
            bytes: 0,
            started: Instant::now(),
        };
        return (ScanPair { ticker: None }, log(a), log(b));
    }

    let state = Arc::new(PairState {
        labels: [a.display().to_string(), b.display().to_string()],
        entries: [AtomicU64::new(0), AtomicU64::new(0)],
        bytes: [AtomicU64::new(0), AtomicU64::new(0)],
        finished: AtomicU64::new(0),
    });
    let ticker = std::thread::spawn({
        let state = Arc::clone(&state);
        move || pair_ticker(state)
    });
    let handle = |state: Arc<PairState>, side: usize| ScanProgress {
        mode: ScanMode::PairTerminal { state, side },
        entries: 0,
        bytes: 0,
        started: Instant::now(),
    };
    let (pa, pb) = (handle(Arc::clone(&state), 0), handle(state, 1));
    (ScanPair { ticker: Some(ticker) }, pa, pb)
}

/// The renderer: redraw the two lines every ~120ms until both scans finish, then erase them.
/// Never holds the stderr lock across a sleep (that would block every other stderr writer).
fn pair_ticker(state: Arc<PairState>) {
    use std::io::Write;
    const GLYPHS: [char; 8] = ['⠁', '⠂', '⠄', '⡀', '⢀', '⠠', '⠐', '⠈'];
    let mut frame = 0usize;
    let mut drew = false;
    loop {
        if state.finished.load(Ordering::SeqCst) >= 2 {
            break;
        }
        let glyph = GLYPHS[frame % GLYPHS.len()];
        frame += 1;
        let width = term_width().unwrap_or(80).max(20) as usize;
        let clip = |s: String| s.chars().take(width - 1).collect::<String>();
        let (l1, l2) = (clip(state.line(0, glyph)), clip(state.line(1, glyph)));
        let mut out = String::new();
        if drew {
            out.push_str("\x1b[2A"); // back to the top of our two lines
        } else {
            out.push_str("\x1b[?25l"); // first frame: hide the cursor
        }
        out.push_str("\r\x1b[2K");
        out.push_str(&l1);
        out.push('\n');
        out.push_str("\r\x1b[2K");
        out.push_str(&l2);
        out.push('\n');
        {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(out.as_bytes());
            let _ = err.flush();
        }
        drew = true;
        std::thread::sleep(Duration::from_millis(120));
    }
    if drew {
        // erase exactly our two lines and restore the cursor; later output starts where line 1 was
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(b"\x1b[2A\r\x1b[J\x1b[?25h");
        let _ = err.flush();
    }
}

/// The terminal's column count (unix `TIOCGWINSZ`); `None` when unavailable.
#[cfg(unix)]
fn term_width() -> Option<u16> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0).then_some(ws.ws_col)
}

#[cfg(not(unix))]
fn term_width() -> Option<u16> {
    None
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

    #[test]
    fn pair_lines_carry_both_labels_and_counts() {
        let state = PairState {
            labels: ["/data".into(), "/mnt/backup".into()],
            entries: [AtomicU64::new(1234), AtomicU64::new(56)],
            bytes: [AtomicU64::new(5 << 20), AtomicU64::new(0)],
            finished: AtomicU64::new(0),
        };
        assert_eq!(state.line(0, '⠁'), "  ⠁ scan /data  1,234 entries  5.0 MiB");
        assert_eq!(state.line(1, '⠂'), "  ⠂ scan /mnt/backup  56 entries  0 B");
    }
}
