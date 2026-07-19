//! The `….errors.txt` companion — everything needing the user's attention, one issue per line,
//! each labeled with its side (`source:` / `destination:`).
//!
//! Created **only if at least one issue occurs**, so "no errors file" always means "clean run".
//! Two writers, matching the two commands: sync streams issues as they happen (an interrupted
//! multi-day run still leaves its record) via [`LazyErrors`], owned by the streaming
//! [`Report`](super::findings::Report); `diff` collects and writes once via [`write_diff_errors`].

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// A stream to the errors file that opens itself on the FIRST recorded issue — a clean run never
/// creates the file. Open failure is remembered (noted once, never retried); the caller keeps the
/// issues in memory regardless, so nothing is lost either way.
pub(crate) struct LazyErrors {
    path: PathBuf,
    sink: Option<BufWriter<File>>,
    failed: bool,
}

impl LazyErrors {
    pub(crate) fn new(path: &Path) -> Self {
        Self { path: path.to_path_buf(), sink: None, failed: false }
    }

    /// Append one issue (opening the file, with its header, on the first call). Flushed each time,
    /// so an interruption can't lose recorded issues.
    pub(crate) fn record(&mut self, msg: &str) {
        if self.sink.is_none() && !self.failed {
            match File::create_new(&self.path) {
                Ok(f) => {
                    let mut s = BufWriter::new(f);
                    let _ = writeln!(s, "filesync issues (one per line)");
                    self.sink = Some(s);
                }
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(s) = self.sink.as_mut() {
            let _ = writeln!(s, "{msg}");
            let _ = s.flush(); // survive an interruption
        }
    }

    /// The file's path — but only once issues have actually been written to it.
    pub(crate) fn written_path(&self) -> Option<&Path> {
        self.sink.as_ref().map(|_| self.path.as_path())
    }

    /// The file's name, for "N issue(s) recorded in <name>" pointers.
    pub(crate) fn file_name(&self) -> String {
        super::file_name_of(&self.path)
    }

    pub(crate) fn flush(&mut self) {
        if let Some(s) = self.sink.as_mut() {
            let _ = s.flush();
        }
    }
}

/// `diff`'s one-shot errors file: the collected issues, written together at the end (a diff's
/// issues don't exist until its classification completes). Returns whether the file was written.
pub(crate) fn write_diff_errors(path: &Path, issues: &[String]) -> bool {
    let body = format!("filesync diff issues (one per line)\n{}\n", issues.join("\n"));
    super::write_diag(path, &body, "issues")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_sink_creates_the_file_only_on_first_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("e.errors.txt");
        let mut le = LazyErrors::new(&path);
        assert!(!path.exists(), "no issues yet → no file");
        assert!(le.written_path().is_none());

        le.record("a.txt: bad");
        assert!(path.exists(), "first issue creates the file");
        assert_eq!(le.written_path(), Some(path.as_path()));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("filesync issues (one per line)"), "{content}");
        assert!(content.contains("a.txt: bad"), "streamed + flushed immediately: {content}");
    }
}
