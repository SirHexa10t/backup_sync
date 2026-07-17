//! The run report and its companion errors file.
//!
//! A run writes up to two files (in the current directory, unless `--report` overrides the path):
//! - the **report** — a human summary: what the run did (counts) plus any benign skips;
//! - the **errors file** — one issue per line, at the sibling path `…​.errors.txt`, created **only
//!   if at least one issue occurs** (so "no errors file" means "clean run").
//!
//! Keeping them apart is the point: the program routes findings, issues, and live progress by
//! *meaning* (report file / errors file / terminal), which shell redirection can't — progress and
//! errors would otherwise share stderr. When backed by files (`create`), both are **streamed and
//! flushed as they occur**, so an interrupted run still leaves its record on disk; the final counts
//! are appended by `finish`. Neither file is ever placed inside the read-only source or the mirror
//! (the caller enforces that before creating them).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct Report {
    pub copied: u64,
    pub moved: u64,
    /// Hard links created — another destination name for an already-copied inode (no data write).
    pub linked: u64,
    pub deleted: u64,
    /// Content-identical files whose metadata (mtime/permissions) was refreshed instead of
    /// re-copying — the deep pre-overwrite check confirmed no bytes needed to move.
    pub refreshed: u64,
    pub bytes_copied: u64,
    /// Things that went wrong or need the user's attention — these make the run exit non-zero.
    pub issues: Vec<String>,
    /// Things deliberately not done because there is nothing to do (special files have no
    /// copyable content). Listed for transparency; they do NOT affect the exit code.
    pub skipped: Vec<String>,
    /// When present, the report (header, skips, final counts) is streamed here as it's recorded.
    sink: Option<BufWriter<File>>,
    /// Where the companion errors file goes; the sink is opened lazily on the first issue, so a
    /// clean run leaves no errors file behind.
    errors_path: Option<PathBuf>,
    errors_sink: Option<BufWriter<File>>,
    /// Set if the errors file couldn't be opened — the caller then surfaces issues on the terminal.
    errors_failed: bool,
    /// Set when a graceful stop cut the run short: (actions performed, actions planned). Its
    /// presence means the mirror is incomplete — the run exits non-zero and says so.
    stopped_early: Option<(usize, usize)>,
}

impl Report {
    /// In-memory only (no streamed files) — used in tests and when a report file can't be opened.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open `report_path` and stream the report to it as the run proceeds. `errors_path` names the
    /// companion errors file (opened lazily on the first issue); `context` is a one-line
    /// description of the run recorded in the header (e.g. `sync /a → /b`). Refuses to overwrite an
    /// existing report (`create_new`) — [`OutputPaths::build`] hands out a de-duplicated stem.
    pub fn create(report_path: &Path, errors_path: &Path, context: &str) -> io::Result<Self> {
        let mut sink = BufWriter::new(File::create_new(report_path)?);
        writeln!(sink, "filesync report — {context}")?;
        writeln!(sink, "(any issues are recorded in {})", file_name_of(errors_path))?;
        sink.flush()?;
        Ok(Self {
            sink: Some(sink),
            errors_path: Some(errors_path.to_path_buf()),
            ..Self::default()
        })
    }

    /// Whether this report is backed by a file (false = in-memory fallback).
    pub fn has_file(&self) -> bool {
        self.sink.is_some()
    }

    /// The errors file path — but only once issues have actually been written to it. `None` when
    /// there were no issues, when no file backs this report, or when the errors file couldn't open.
    pub fn errors_file(&self) -> Option<&Path> {
        self.errors_sink.as_ref().and(self.errors_path.as_deref())
    }

    /// Record a failed operation on `path`.
    pub fn issue(&mut self, path: PathBuf, err: &io::Error) {
        self.issue_msg(format!("{}: {}", path.display(), err));
    }

    /// Record a free-form issue. Streamed + flushed to the errors file, which is opened on demand
    /// (so it never exists for a clean run), and kept in memory for the count and the exit code.
    pub fn issue_msg(&mut self, msg: String) {
        self.stream_issue(&msg);
        self.issues.push(msg);
    }

    /// Append `msg` to the errors file, opening it on the first call. On open failure, note it once
    /// and carry on — the issue is still held in memory, and the caller surfaces it another way.
    fn stream_issue(&mut self, msg: &str) {
        if self.errors_sink.is_none() && !self.errors_failed {
            let Some(path) = self.errors_path.clone() else {
                return; // in-memory report: nothing to stream to
            };
            match File::create_new(&path) {
                Ok(f) => {
                    let mut s = BufWriter::new(f);
                    let _ = writeln!(s, "filesync issues (one per line)");
                    self.errors_sink = Some(s);
                }
                Err(_) => {
                    self.errors_failed = true;
                    return;
                }
            }
        }
        if let Some(s) = self.errors_sink.as_mut() {
            let _ = writeln!(s, "{msg}");
            let _ = s.flush(); // survive an interruption
        }
    }

    /// Record that a graceful stop ended the run before all planned actions ran (so the mirror is
    /// incomplete). Not an error — kept separate from `issues` and the errors file.
    pub fn mark_stopped_early(&mut self, performed: usize, planned: usize) {
        self.stopped_early = Some((performed, planned));
    }

    /// Whether the run was cut short by a requested stop.
    pub fn was_stopped_early(&self) -> bool {
        self.stopped_early.is_some()
    }

    /// Record a benign skip — something with nothing to copy. Streamed to the report (marked `~`),
    /// never to the errors file, and never affecting the exit code.
    pub fn skip_msg(&mut self, msg: String) {
        if let Some(sink) = self.sink.as_mut() {
            let _ = writeln!(sink, "  ~ {msg}");
            let _ = sink.flush();
        }
        self.skipped.push(msg);
    }

    /// Append the final counts to the report file, plus a completion line — its absence marks a
    /// report cut short by an interruption. Flushes the errors file too.
    pub fn finish(&mut self) {
        let counts = self.counts();
        if let Some(sink) = self.sink.as_mut() {
            let _ = write!(sink, "\n{counts}");
            if !self.issues.is_empty() {
                if let Some(p) = self.errors_path.as_ref() {
                    let _ = writeln!(sink, "{} issue(s) recorded in {}", self.issues.len(), file_name_of(p));
                }
            }
            match self.stopped_early {
                Some((done, total)) => {
                    let _ = writeln!(
                        sink,
                        "run stopped early by request — {done} of {total} planned action(s) done; \
                         the mirror is incomplete, re-run to finish"
                    );
                }
                None => {
                    let _ = writeln!(sink, "run completed");
                }
            }
            let _ = sink.flush();
        }
        if let Some(s) = self.errors_sink.as_mut() {
            let _ = s.flush();
        }
    }

    fn counts(&self) -> String {
        format!(
            "copied:  {} ({} bytes)\nmoved:   {}\nlinked:  {}\ndeleted: {}\nrefreshed: {}\nskipped: {}\nissues:  {}\n",
            self.copied,
            self.bytes_copied,
            self.moved,
            self.linked,
            self.deleted,
            self.refreshed,
            self.skipped.len(),
            self.issues.len()
        )
    }

    /// Summary for the terminal: counts + the benign skip list. Issues are NOT included here — they
    /// live in the errors file, and the caller prints its path (or, with no file, the issues).
    pub fn render(&self) -> String {
        let mut s = self.counts();
        for m in &self.skipped {
            s.push_str("    ~ ");
            s.push_str(m);
            s.push('\n');
        }
        if let Some((done, total)) = self.stopped_early {
            s.push_str(&format!(
                "STOPPED EARLY by request — {done}/{total} actions done; re-run to finish\n"
            ));
        }
        s
    }
}

/// The set of output files a run writes, all sharing one stem inside the output directory:
/// `filesync-<command>-<source>-<YYYY-mm-DD_HHMM>.<kind>.txt`. `conclusions` is used only by `diff`.
#[derive(Debug, Clone)]
pub struct OutputPaths {
    pub report: PathBuf,
    pub errors: PathBuf,
    pub conclusions: PathBuf,
}

impl OutputPaths {
    /// Build the output paths for `command` inside `dir`, de-duplicating the stem so a same-minute
    /// re-run never truncates the previous one (it checks the `.findings.txt` file). `command` is
    /// `sync` or `diff`, so a diff and a sync of the same source don't collide.
    pub fn build(dir: &Path, command: &str, src_root: &Path, now: SystemTime) -> Self {
        let stem = unique_stem(dir, &raw_stem(command, src_root, now));
        let f = |kind: &str| dir.join(format!("{stem}.{kind}.txt"));
        Self { report: f("findings"), errors: f("errors"), conclusions: f("conclusions") }
    }
}

/// `filesync-<command>-<source-folder-name>-<YYYY-mm-DD_HHMM>` (UTC) — the shared filename stem,
/// without directory or extension.
fn raw_stem(command: &str, src_root: &Path, now: SystemTime) -> String {
    let name = src_root
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("filesync-{command}-{name}-{}", timestamp_utc(secs))
}

/// A stem whose `<stem>.findings.txt` doesn't yet exist in `dir`: the stem itself, else `…-2`,
/// `…-3`, … Keeps a same-minute re-run from colliding with the previous run's files. After 100
/// collisions, returns the last try (creation then fails loudly rather than truncating anything).
fn unique_stem(dir: &Path, stem: &str) -> String {
    let taken = |candidate: &str| dir.join(format!("{candidate}.findings.txt")).exists();
    if !taken(stem) {
        return stem.to_string();
    }
    for n in 2..=100u32 {
        let candidate = format!("{stem}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    stem.to_string()
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// Format a UTC unix timestamp as `YYYY-mm-DD_HHMM`.
fn timestamp_utc(secs: u64) -> String {
    let (h, mi) = ((secs % 86400) / 3600, (secs % 3600) / 60);
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}_{h:02}{mi:02}")
}

/// Days-since-epoch → (year, month, day), via Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamp_is_formatted_utc() {
        assert_eq!(timestamp_utc(1_609_459_200), "2021-01-01_0000");
        assert_eq!(timestamp_utc(1_609_459_200 + 13 * 3600 + 37 * 60), "2021-01-01_1337");
    }

    #[test]
    fn output_paths_share_a_stem_named_by_command_and_source() {
        let p = OutputPaths::build(
            Path::new("/out"),
            "diff",
            Path::new("/home/someone/My Docs"),
            UNIX_EPOCH + Duration::from_secs(1_609_459_200),
        );
        let stem = "/out/filesync-diff-My_Docs-2021-01-01_0000";
        assert_eq!(p.report, PathBuf::from(format!("{stem}.findings.txt")));
        assert_eq!(p.errors, PathBuf::from(format!("{stem}.errors.txt")));
        assert_eq!(p.conclusions, PathBuf::from(format!("{stem}.conclusions.txt")));
    }

    #[test]
    fn build_dedups_the_stem_when_findings_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        let first = OutputPaths::build(tmp.path(), "sync", Path::new("/x/src"), now);
        std::fs::write(&first.report, b"a prior run's findings").unwrap();

        // same minute → the stem gains a `-2` so the prior findings file is never clobbered …
        let second = OutputPaths::build(tmp.path(), "sync", Path::new("/x/src"), now);
        assert_ne!(second.report, first.report);
        assert!(second.report.to_string_lossy().ends_with("-2.findings.txt"), "{:?}", second.report);
        // … and the whole trio stays on that one deduped stem
        assert!(second.errors.to_string_lossy().ends_with("-2.errors.txt"));
        assert!(second.conclusions.to_string_lossy().ends_with("-2.conclusions.txt"));
    }

    #[test]
    fn issues_are_streamed_to_the_errors_file_before_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("r.findings.txt");
        let errors = tmp.path().join("r.errors.txt");
        {
            let mut r = Report::create(&report, &errors, "test").unwrap();
            r.issue_msg("a.txt: bad".into());
            r.issue_msg("b.txt: worse".into());
            // dropped here WITHOUT finish() — simulating an interruption
        }
        let e = std::fs::read_to_string(&errors).unwrap();
        assert!(e.contains("a.txt: bad") && e.contains("b.txt: worse"), "issue not on disk: {e}");
        // issues live in the errors file, not the report — and an interrupted report has no
        // completion line
        let rep = std::fs::read_to_string(&report).unwrap();
        assert!(!rep.contains("a.txt: bad"), "issues must not leak into the report file:\n{rep}");
        assert!(!rep.contains("run completed"));
    }

    #[test]
    fn errors_file_appears_only_when_there_are_issues() {
        let tmp = tempfile::tempdir().unwrap();

        // clean run: report written, NO errors file
        let clean = tmp.path().join("clean.findings.txt");
        let clean_err = tmp.path().join("clean.errors.txt");
        {
            let mut r = Report::create(&clean, &clean_err, "clean").unwrap();
            r.finish();
        }
        assert!(clean.exists(), "the report is always written");
        assert!(!clean_err.exists(), "a clean run must not leave an errors file");
        assert!(Report::create(&clean, &clean_err, "x").is_err(), "report is create_new");

        // run with an issue: errors file exists, report points at it
        let dirty = tmp.path().join("dirty.findings.txt");
        let dirty_err = tmp.path().join("dirty.errors.txt");
        {
            let mut r = Report::create(&dirty, &dirty_err, "dirty").unwrap();
            r.issue_msg("something: broke".into());
            r.finish();
        }
        assert!(std::fs::read_to_string(&dirty_err).unwrap().contains("something: broke"));
        let rep = std::fs::read_to_string(&dirty).unwrap();
        assert!(rep.contains("recorded in dirty.errors.txt"), "report must point at the errors file:\n{rep}");
    }

    #[test]
    fn finished_report_carries_a_completion_line() {
        let tmp = tempfile::tempdir().unwrap();
        let report = tmp.path().join("r.findings.txt");
        let mut r = Report::create(&report, &tmp.path().join("r.errors.txt"), "ctx").unwrap();
        r.finish();
        assert!(std::fs::read_to_string(&report).unwrap().contains("run completed"));
    }
}
