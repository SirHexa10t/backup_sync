//! The run report: what a sync did, and anything that needs attention.
//!
//! When backed by a file (`create`), issues are **streamed and flushed as they occur**, so an
//! interrupted run still leaves the actionable list on disk. The final counts are appended by
//! `finish`. Written to the current directory — never into the read-only source or the mirror.

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
    /// When present, issues/skips are streamed here (flushed each time) as they're recorded.
    sink: Option<BufWriter<File>>,
}

impl Report {
    /// In-memory only (no streamed file) — used in tests and when a report file can't be opened.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open `path` and stream the report to it as the run proceeds. Refuses to overwrite an
    /// existing file (`create_new`) — pick a free name with [`unique_path`] first.
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut sink = BufWriter::new(File::create_new(path)?);
        writeln!(sink, "filesync report (issues stream below; final counts appear at the end)")?;
        sink.flush()?;
        Ok(Self { sink: Some(sink), ..Self::default() })
    }

    /// Whether this report is backed by a file (false = in-memory fallback).
    pub fn has_file(&self) -> bool {
        self.sink.is_some()
    }

    /// Record a failed operation on `path`.
    pub fn issue(&mut self, path: PathBuf, err: &io::Error) {
        self.issue_msg(format!("{}: {}", path.display(), err));
    }

    /// Record a free-form issue/note (streamed + flushed if a file is attached).
    pub fn issue_msg(&mut self, msg: String) {
        if let Some(sink) = self.sink.as_mut() {
            let _ = writeln!(sink, "  ! {msg}");
            let _ = sink.flush(); // survive an interruption
        }
        self.issues.push(msg);
    }

    /// Record a benign skip — something with nothing to copy (streamed like issues, but marked
    /// `~` and never affecting the exit code).
    pub fn skip_msg(&mut self, msg: String) {
        if let Some(sink) = self.sink.as_mut() {
            let _ = writeln!(sink, "  ~ {msg}");
            let _ = sink.flush();
        }
        self.skipped.push(msg);
    }

    /// Append the final counts to the streamed file (issues were already streamed above them),
    /// plus a completion line — its absence marks a report cut short by an interruption.
    pub fn finish(&mut self) {
        let counts = self.counts();
        if let Some(sink) = self.sink.as_mut() {
            let _ = write!(sink, "\n{counts}");
            let _ = writeln!(sink, "run completed");
            let _ = sink.flush();
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

    /// Full summary for the terminal (counts + skip and issue lists).
    pub fn render(&self) -> String {
        let mut s = self.counts();
        for m in &self.skipped {
            s.push_str("    ~ ");
            s.push_str(m);
            s.push('\n');
        }
        for i in &self.issues {
            s.push_str("    ! ");
            s.push_str(i);
            s.push('\n');
        }
        s
    }
}

/// `./filesync-report-<source-folder-name>-<YYYY-mm-DD_HHMM>.txt` (UTC), in the current directory.
pub fn default_report_path(src_root: &Path, now: SystemTime) -> PathBuf {
    let name = src_root
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    PathBuf::from(format!("filesync-report-{name}-{}.txt", timestamp_utc(secs)))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// A variant of `path` that doesn't exist yet: `path` itself, else `…-2`, `…-3`, … (before the
/// extension). Keeps a same-minute re-run from silently truncating the previous run's report.
/// After 100 collisions, gives up and returns the original (creation will then fail loudly).
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..=100u32 {
        let name = match &ext {
            Some(ext) => format!("{stem}-{n}.{ext}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = path.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
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
    fn default_path_uses_source_name_and_is_impersonal() {
        let p = default_report_path(
            Path::new("/home/someone/My Docs"),
            UNIX_EPOCH + Duration::from_secs(1_609_459_200),
        );
        assert_eq!(p, PathBuf::from("filesync-report-My_Docs-2021-01-01_0000.txt"));
    }

    #[test]
    fn issues_are_streamed_to_disk_before_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.txt");
        {
            let mut r = Report::create(&path).unwrap();
            r.issue_msg("a.txt: bad".into());
            r.issue_msg("b.txt: worse".into());
            // dropped here WITHOUT finish() — simulating an interruption
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("a.txt: bad"), "issue not on disk: {content}");
        assert!(content.contains("b.txt: worse"));
        // an interrupted report is recognizable: no completion line
        assert!(!content.contains("run completed"));
    }

    #[test]
    fn finished_report_carries_a_completion_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.txt");
        let mut r = Report::create(&path).unwrap();
        r.finish();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("run completed"), "{content}");
    }

    #[test]
    fn create_refuses_to_overwrite_and_unique_path_sidesteps() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.txt");
        std::fs::write(&path, b"previous run's report").unwrap();

        assert!(Report::create(&path).is_err(), "an existing report must never be truncated");
        let alt = unique_path(&path);
        assert_eq!(alt, tmp.path().join("r-2.txt"));
        assert!(Report::create(&alt).is_ok());
        assert_eq!(std::fs::read(&path).unwrap(), b"previous run's report", "original intact");
    }
}
