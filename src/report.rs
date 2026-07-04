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
    pub deleted: u64,
    pub bytes_copied: u64,
    /// Files/dirs that were skipped or failed and need the user's attention.
    pub issues: Vec<String>,
    /// When present, issues are streamed here (flushed each time) as they're recorded.
    sink: Option<BufWriter<File>>,
}

impl Report {
    /// In-memory only (no streamed file) — used in tests and when a report file can't be opened.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open `path` and stream the report to it as the run proceeds.
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut sink = BufWriter::new(File::create(path)?);
        writeln!(sink, "filesync report — in progress")?;
        sink.flush()?;
        Ok(Self { sink: Some(sink), ..Self::default() })
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

    /// Append the final counts to the streamed file (issues were already streamed above them).
    pub fn finish(&mut self) {
        let counts = self.counts();
        if let Some(sink) = self.sink.as_mut() {
            let _ = write!(sink, "\n{counts}");
            let _ = sink.flush();
        }
    }

    fn counts(&self) -> String {
        format!(
            "copied:  {} ({} bytes)\nmoved:   {}\ndeleted: {}\nissues:  {}\n",
            self.copied, self.bytes_copied, self.moved, self.deleted, self.issues.len()
        )
    }

    /// Full summary for the terminal (counts + the issue list).
    pub fn render(&self) -> String {
        let mut s = self.counts();
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
    }
}
