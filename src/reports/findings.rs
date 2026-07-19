//! The `….findings.txt` file — the report proper: what the run did (sync) or would do (diff).
//!
//! Two writers, matching the two commands. **sync** uses the streaming [`Report`]: skips, root
//! assists, and the final counts are flushed as the run proceeds, so an interrupted run still
//! leaves a usable record (its issues stream to the errors companion — see
//! [`super::errors::LazyErrors`]); a completed report ends with a `run completed` line, so a
//! cut-short one is recognizable. **diff** produces its whole classification at once and writes it
//! one-shot via [`write_diff`].

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use super::errors::LazyErrors;
use crate::units::human_count;

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
    /// Operations that needed root to get past a permission wall (see [`crate::runtime::elevation`]) —
    /// the accountability trail for a sudo-launched run. Not issues; no effect on the exit code.
    pub root_assisted: Vec<String>,
    /// When present, the report (header, skips, final counts) is streamed here as it's recorded.
    sink: Option<BufWriter<std::fs::File>>,
    /// The lazily-created errors companion (`None` for in-memory reports) — issues stream to it.
    errors: Option<LazyErrors>,
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
    /// existing report (`create_new`) — [`super::OutputPaths::build`] hands out a de-duplicated stem.
    pub fn create(report_path: &Path, errors_path: &Path, context: &str) -> io::Result<Self> {
        let mut sink = BufWriter::new(std::fs::File::create_new(report_path)?);
        writeln!(sink, "filesync report — {context}")?;
        writeln!(sink, "(any issues are recorded in {})", super::file_name_of(errors_path))?;
        sink.flush()?;
        Ok(Self { sink: Some(sink), errors: Some(LazyErrors::new(errors_path)), ..Self::default() })
    }

    /// Whether this report is backed by a file (false = in-memory fallback).
    pub fn has_file(&self) -> bool {
        self.sink.is_some()
    }

    /// The errors file path — but only once issues have actually been written to it. `None` when
    /// there were no issues, when no file backs this report, or when the errors file couldn't open.
    pub fn errors_file(&self) -> Option<&Path> {
        self.errors.as_ref().and_then(|e| e.written_path())
    }

    /// Record a failed operation on `path`.
    pub fn issue(&mut self, path: PathBuf, err: &io::Error) {
        self.issue_msg(format!("{}: {}", path.display(), err));
    }

    /// Record a free-form issue: streamed + flushed to the errors companion (created on demand, so
    /// it never exists for a clean run), and kept in memory for the count and the exit code.
    pub fn issue_msg(&mut self, msg: String) {
        if let Some(errors) = self.errors.as_mut() {
            errors.record(&msg);
        }
        self.issues.push(msg);
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

    /// Record one root-assisted operation (streamed to the report, marked `%` — done work, not an
    /// issue). The audit trail of a sudo-launched run.
    pub fn root_op(&mut self, msg: String) {
        if let Some(sink) = self.sink.as_mut() {
            let _ = writeln!(sink, "  % root: {msg}");
            let _ = sink.flush();
        }
        self.root_assisted.push(msg);
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
                if let Some(errors) = self.errors.as_ref() {
                    let _ = writeln!(
                        sink,
                        "{} issue(s) recorded in {}",
                        self.issues.len(),
                        errors.file_name()
                    );
                }
            }
            match self.stopped_early {
                Some((done, total)) => {
                    let _ = writeln!(
                        sink,
                        "run stopped early by request — {} of {} planned action(s) done; \
                         the mirror is incomplete, re-run to finish",
                        human_count(done as u64),
                        human_count(total as u64)
                    );
                }
                None => {
                    let _ = writeln!(sink, "run completed");
                }
            }
            let _ = sink.flush();
        }
        if let Some(errors) = self.errors.as_mut() {
            errors.flush();
        }
    }

    fn counts(&self) -> String {
        let mut s = format!(
            "copied:  {} ({} bytes)\nmoved:   {}\nlinked:  {}\ndeleted: {}\nrefreshed: {}\nskipped: {}\nissues:  {}\n",
            self.copied,
            self.bytes_copied,
            self.moved,
            self.linked,
            self.deleted,
            self.refreshed,
            self.skipped.len(),
            self.issues.len()
        );
        // only when root actually helped — a normal run's counts stay exactly as they were
        if !self.root_assisted.is_empty() {
            s.push_str(&format!("root-assisted: {}\n", self.root_assisted.len()));
        }
        s
    }

    /// Summary for the terminal: counts + the benign skip list (+ root assists). Issues are NOT
    /// included here — they live in the errors file, and the caller prints its path (or, with no
    /// file, the issues).
    pub fn render(&self) -> String {
        let mut s = self.counts();
        for m in &self.skipped {
            s.push_str("    ~ ");
            s.push_str(m);
            s.push('\n');
        }
        for m in &self.root_assisted {
            s.push_str("    % root: ");
            s.push_str(m);
            s.push('\n');
        }
        if let Some((done, total)) = self.stopped_early {
            s.push_str(&format!(
                "STOPPED EARLY by request — {}/{} actions done; re-run to finish\n",
                human_count(done as u64),
                human_count(total as u64)
            ));
        }
        s
    }
}

/// `diff`'s one-shot findings file: header, the full classification, and the root-assist audit (if
/// any). Returns whether the file was written.
pub(crate) fn write_diff(
    path: &Path,
    src_disp: &str,
    dst_disp: &str,
    rendered_diff: &str,
    audit: &[String],
) -> bool {
    let content = format!(
        "filesync diff — comparing {src_disp} -> {dst_disp}\n\n{rendered_diff}{}",
        audit_block(audit)
    );
    super::write_diag(path, &content, "findings")
}

/// The presentation of a classified diff — a method on [`crate::diff::Diff`] whose impl lives here
/// with the rest of the findings rendering (the analysis module stays presentation-free).
impl crate::diff::Diff {
    /// A git-diff-like textual summary. `detail` controls whether the per-file lines are included:
    /// the findings file gets the full listing (`true`); the terminal gets only the count lines
    /// (`false`), so a diff of a huge tree never floods the screen — the detail is in the file.
    pub fn render(&self, detail: bool) -> String {
        use crate::manifest::Kind;
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "moved:     {}", self.moved.len());
        if detail {
            for m in &self.moved {
                let _ = writeln!(s, "    ~ {}  ->  {}", m.from.display(), m.to.display());
            }
        }
        let _ = writeln!(s, "to copy:   {}", self.added.len());
        if detail {
            for c in &self.added {
                if c.kind == Kind::Other {
                    let _ = writeln!(s, "    + {} (special file — no content; will be skipped)", c.rel.display());
                } else {
                    let _ = writeln!(s, "    + {}", c.rel.display());
                }
            }
        }
        let _ = writeln!(s, "to delete: {}", self.removed.len());
        if detail {
            for c in &self.removed {
                let _ = writeln!(s, "    - {}", c.rel.display());
            }
        }
        let _ = writeln!(s, "to update: {}", self.changed.len());
        if detail {
            for c in &self.changed {
                let _ = writeln!(s, "    * {}", c.rel.display());
            }
        }
        let _ = writeln!(s, "to link:   {} (hard links — content written once via the leader)", self.to_link.len());
        if detail {
            for l in &self.to_link {
                let _ = writeln!(s, "    & {}  ->  {}", l.name.display(), l.leader.display());
            }
        }
        let _ = writeln!(s, "to refresh (content identical, metadata drift): {}", self.touched.len());
        if detail {
            for c in &self.touched {
                let _ = writeln!(s, "    ≈ {}", c.rel.display());
            }
        }
        let _ = writeln!(s, "unchanged: {}", self.unchanged);
        if detail {
            // Only ever populated under --include-same; listed here so the exhaustive findings
            // account for every entry, not just the ones that change.
            for rel in &self.unchanged_paths {
                let _ = writeln!(s, "    = {}", rel.display());
            }
        }
        s
    }
}

/// The root-assist audit as a text block (empty when there were no assists) — appended to diff's
/// findings; the sync report streams the same lines live via [`Report::root_op`] instead.
fn audit_block(audit: &[String]) -> String {
    if audit.is_empty() {
        return String::new();
    }
    let mut s = format!("\nroot-assisted operations: {}\n", audit.len());
    for a in audit {
        s.push_str("  % root: ");
        s.push_str(a);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tmp: &tempfile::TempDir, stem: &str) -> (PathBuf, PathBuf) {
        (tmp.path().join(format!("{stem}.findings.txt")), tmp.path().join(format!("{stem}.errors.txt")))
    }

    #[test]
    fn issues_are_streamed_to_the_errors_file_before_finish() {
        let tmp = tempfile::tempdir().unwrap();
        let (report, errors) = paths(&tmp, "r");
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
        let (clean, clean_err) = paths(&tmp, "clean");
        {
            let mut r = Report::create(&clean, &clean_err, "clean").unwrap();
            r.finish();
        }
        assert!(clean.exists(), "the report is always written");
        assert!(!clean_err.exists(), "a clean run must not leave an errors file");
        assert!(Report::create(&clean, &clean_err, "x").is_err(), "report is create_new");

        // run with an issue: errors file exists, report points at it
        let (dirty, dirty_err) = paths(&tmp, "dirty");
        {
            let mut r = Report::create(&dirty, &dirty_err, "dirty").unwrap();
            r.issue_msg("something: broke".into());
            r.finish();
        }
        assert!(std::fs::read_to_string(&dirty_err).unwrap().contains("something: broke"));
        let rep = std::fs::read_to_string(&dirty).unwrap();
        assert!(
            rep.contains("recorded in dirty.errors.txt"),
            "report must point at the errors file:\n{rep}"
        );
    }

    #[test]
    fn finished_report_carries_a_completion_line() {
        let tmp = tempfile::tempdir().unwrap();
        let (report, errors) = paths(&tmp, "r");
        let mut r = Report::create(&report, &errors, "ctx").unwrap();
        r.finish();
        assert!(std::fs::read_to_string(&report).unwrap().contains("run completed"));
    }
}
