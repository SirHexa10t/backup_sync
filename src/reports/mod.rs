//! Everything a run writes for the user to read — one module per output file, shared naming and
//! writing machinery here.
//!
//! The map (all four share one timestamped stem, built by [`OutputPaths`]):
//!
//! | file                  | module            | written by       | how                       |
//! |-----------------------|-------------------|------------------|---------------------------|
//! | `….findings.txt`      | [`findings`]      | both commands    | sync: streamed ([`Report`]); diff: one-shot |
//! | `….errors.txt`        | [`errors`]        | both, if any     | sync: lazy stream; diff: one-shot |
//! | `….conclusions.txt`   | [`conclusions`]   | diff             | one-shot (analysis + render) |
//! | `….showstoppers.txt`  | [`showstoppers`]  | both, if any     | one-shot (analysis + render) |
//!
//! The per-command modules own everything each command *reports* — including its terminal summary
//! (the compact counts, file pointers, issue surfacing, hints): [`diff_cmd`] writes the diff's
//! files and prints its summary; [`sync_cmd`] adds sync's showstoppers forecast and end-of-run
//! summary around the streamed [`Report`].
//!
//! Live terminal progress is deliberately *not* here (`crate::progress_update`): those are updates, not
//! reports — ephemeral, never landing in a file. That boundary is the point of the split.

pub mod conclusions;
pub(crate) mod diff_cmd;
pub mod errors;
pub mod findings;
pub mod showstoppers;
pub(crate) mod sync_cmd;

pub use findings::Report;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The set of output files a run writes, all sharing one stem inside the output directory:
/// `filesync-<command>-<source>-<YYYY-mm-DD_HHMM>.<kind>.txt`. `conclusions` is used only by `diff`.
#[derive(Debug, Clone)]
pub struct OutputPaths {
    pub report: PathBuf,
    pub errors: PathBuf,
    pub conclusions: PathBuf,
    pub showstoppers: PathBuf,
}

impl OutputPaths {
    /// Build the output paths for `command` inside `dir`, de-duplicating the stem so a same-minute
    /// re-run never truncates the previous one (it checks the `.findings.txt` file). `command` is
    /// `sync` or `diff`, so a diff and a sync of the same source don't collide.
    pub fn build(dir: &Path, command: &str, src_root: &Path, now: SystemTime) -> Self {
        let stem = unique_stem(dir, &raw_stem(command, src_root, now));
        let f = |kind: &str| dir.join(format!("{stem}.{kind}.txt"));
        Self {
            report: f("findings"),
            errors: f("errors"),
            conclusions: f("conclusions"),
            showstoppers: f("showstoppers"),
        }
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

pub(crate) fn file_name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// After a graceful stop: rename this run's existing output files so their stem carries an
/// `-interrupted` marker (`…-interrupted.findings.txt`) — a partial record must never be mistaken
/// for a complete one at a glance. All files keep one shared stem (a same-minute earlier
/// interruption is sidestepped with `-interrupted-2`, …), a note is appended inside the report
/// explaining the rename, and the updated paths are returned. Best-effort: a file whose rename
/// fails keeps its original path.
pub(crate) fn rename_interrupted(paths: &OutputPaths) -> OutputPaths {
    // our names always end ".<kind>.txt" — peel those two segments off the right (the stem itself
    // may contain dots: the source folder's name is embedded in it)
    let variant = |p: &Path, n: u32| -> PathBuf {
        let name = file_name_of(p);
        let mut parts = name.rsplitn(3, '.');
        let txt = parts.next().unwrap_or("txt");
        let kind = parts.next().unwrap_or("out");
        let stem = parts.next().unwrap_or(&name);
        let tag =
            if n == 0 { "-interrupted".to_string() } else { format!("-interrupted-{}", n + 1) };
        p.with_file_name(format!("{stem}{tag}.{kind}.{txt}"))
    };

    let all = [&paths.report, &paths.errors, &paths.conclusions, &paths.showstoppers];
    let n = (0..100u32).find(|&n| all.iter().all(|p| !variant(p, n).exists())).unwrap_or(0);

    let rename = |p: &Path| -> PathBuf {
        if !p.exists() {
            return variant(p, n); // nothing on disk — just report the would-be name
        }
        let target = variant(p, n);
        match fs::rename(p, &target) {
            Ok(()) => target,
            Err(_) => p.to_path_buf(),
        }
    };
    let renamed = OutputPaths {
        report: rename(&paths.report),
        errors: rename(&paths.errors),
        conclusions: rename(&paths.conclusions),
        showstoppers: rename(&paths.showstoppers),
    };

    // The report's header/footer reference the errors file by its ORIGINAL name (they were
    // streamed before the stop) — append a note so the file explains its own renaming.
    if renamed.report.exists() {
        use io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&renamed.report) {
            let _ = writeln!(
                f,
                "note: this run was interrupted — its output files were renamed with an \
                 '-interrupted' marker (names mentioned above refer to the original names)"
            );
        }
    }
    renamed
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect()
}

/// Write `content` to a brand-new file at `path`, never overwriting (`create_new`) — output stems
/// are de-duplicated up front, so an existing file means something is wrong; fail rather than
/// truncate it.
pub(crate) fn write_fresh(path: &Path, content: &str) -> io::Result<()> {
    use io::Write;
    let mut f = fs::File::create_new(path)?;
    f.write_all(content.as_bytes())?;
    f.flush()
}

/// Write a one-shot report file, reporting (but never failing the run on) an I/O error. Returns
/// whether the file was written. `label` names the file kind for the error message.
pub(crate) fn write_diag(path: &Path, content: &str, label: &str) -> bool {
    match write_fresh(path, content) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("filesync: cannot write {label} to {} ({e})", path.display());
            false
        }
    }
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
        assert_eq!(p.showstoppers, PathBuf::from(format!("{stem}.showstoppers.txt")));
    }

    #[test]
    fn interrupted_rename_tags_existing_files_and_dodges_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        let paths = OutputPaths::build(tmp.path(), "sync", Path::new("/x/my.dotted.src"), now);
        // report + errors exist (streamed during the run); showstoppers/conclusions don't
        std::fs::write(&paths.report, b"partial report").unwrap();
        std::fs::write(&paths.errors, b"some issues").unwrap();

        let renamed = rename_interrupted(&paths);
        // existing files were physically renamed, stem dots survived the tagging
        assert!(
            renamed.report.to_string_lossy().ends_with("-interrupted.findings.txt"),
            "{:?}",
            renamed.report
        );
        assert!(renamed.report.exists() && !paths.report.exists(), "report renamed on disk");
        assert!(renamed.errors.exists() && !paths.errors.exists(), "errors renamed on disk");
        // the report explains its own renaming
        let content = std::fs::read_to_string(&renamed.report).unwrap();
        assert!(content.contains("interrupted"), "{content}");
        // a second interruption in the same minute must not clobber the first record
        std::fs::write(&paths.report, b"second partial run").unwrap();
        let renamed2 = rename_interrupted(&paths);
        assert!(
            renamed2.report.to_string_lossy().ends_with("-interrupted-2.findings.txt"),
            "{:?}",
            renamed2.report
        );
        assert_eq!(std::fs::read(&renamed.report).unwrap()[..14], b"partial report"[..]);
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
        // … and the whole quartet stays on that one deduped stem
        assert!(second.errors.to_string_lossy().ends_with("-2.errors.txt"));
        assert!(second.conclusions.to_string_lossy().ends_with("-2.conclusions.txt"));
        assert!(second.showstoppers.to_string_lossy().ends_with("-2.showstoppers.txt"));
    }
}
