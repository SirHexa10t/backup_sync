//! filesync — cheaply and reliably mirror one directory onto another.
//!
//! See `README.md` for the CLI/UX and `docs/theory.md` for the design rationale and the
//! benchmark data behind it.
//!
//! Pipeline: scan both trees → `diff` (classify + move-detect) → `plan` (ordered actions) →
//! `apply` (renames/deletes/atomic copies → end-sync → verify) → `report`.

pub mod apply;
pub mod cli;
pub mod diff;
pub mod hash;
pub mod manifest;
pub mod parallel;
pub mod plan;
pub mod report;
pub mod scan;
pub mod target;

pub use cli::{Cli, Command};

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use manifest::{DstRoot, Kind, SrcRoot};

/// Program entry point, called from `main`.
pub fn run(cli: Cli) -> ExitCode {
    let common = cli.command.common();

    if let Err(msg) = validate_roots(&common.from, &common.to) {
        eprintln!("filesync: {msg}");
        return ExitCode::FAILURE;
    }

    let src = SrcRoot::new(&common.from);
    let dst = DstRoot::new(&common.to);

    match &cli.command {
        Command::Diff(a) => {
            let (src_m, src_errs) = scan::scan_with_errors(src.path());
            let (dst_m, dst_errs) = scan::scan_with_errors(dst.path());
            for e in src_errs.iter().chain(dst_errs.iter()) {
                eprintln!("filesync diff: {e}");
            }
            // Hashing is sequential — the --jobs flag was removed (no measured benefit).
            match diff::diff(&src, &src_m, &dst, &dst_m, a.common.eager_checksum, 1) {
                Ok(d) => {
                    print!("{}", d.render());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("filesync diff: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Sync(a) => run_sync(&src, &dst, a),
    }
}

fn run_sync(src: &SrcRoot, dst: &DstRoot, a: &cli::SyncArgs) -> ExitCode {
    if let Err(e) = fs::create_dir_all(dst.path()) {
        eprintln!("filesync sync: cannot create destination {}: {e}", dst.path().display());
        return ExitCode::FAILURE;
    }

    // A backup dir must live on the same filesystem as the destination: files are moved aside with
    // rename, which can't cross filesystems.
    if let Some(bdir) = &a.backup_dir {
        match same_filesystem(bdir, dst.path()) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "filesync sync: --backup-dir must be on the same filesystem as the destination \
                     (backup-dir={}, destination={})",
                    bdir.display(),
                    dst.path().display()
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("filesync sync: cannot check --backup-dir location: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Clean up any temp files a previous, interrupted run left behind.
    let swept = apply::sweep_temp_files(dst);
    if swept > 0 {
        eprintln!("filesync: removed {swept} leftover temp file(s) from a previous run");
    }

    let (src_m, mut scan_errors) = scan::scan_with_errors(src.path());
    if src_m.is_empty() {
        eprintln!(
            "filesync sync: source {} is empty — refusing to mirror, which would delete everything \
             in the destination. If the source drive simply isn't mounted, mount it and retry; to \
             deliberately empty the destination, remove it yourself.",
            src.path().display()
        );
        return ExitCode::FAILURE;
    }
    let (dst_m, dst_errors) = scan::scan_with_errors(dst.path());
    scan_errors.extend(dst_errors);

    // Hashing is sequential — the --jobs flag was removed (no measured benefit; docs/theory.md).
    let d = match diff::diff(src, &src_m, dst, &dst_m, a.common.eager_checksum, 1) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("filesync sync: {e}");
            return ExitCode::FAILURE;
        }
    };

    let actions = plan::plan(&d);
    let opts = apply::Options {
        verify: !a.no_verify,
        fsync_each: a.fsync_each,
        backup_dir: a.backup_dir.clone(),
        jobs: 1, // verify hashing is sequential (--jobs removed)
    };

    // Open the (streamed) report; fall back to in-memory if the file can't be created.
    let report_path = a
        .common
        .report
        .clone()
        .unwrap_or_else(|| report::default_report_path(src.path(), SystemTime::now()));
    let mut report = report::Report::create(&report_path).unwrap_or_else(|e| {
        eprintln!("filesync sync: cannot open report {} ({e}); continuing without a report file", report_path.display());
        report::Report::new()
    });

    // Record anything we couldn't read while scanning, up front, so an interrupted run still
    // shows what was missed (its contents were omitted from the mirror).
    for e in &scan_errors {
        report.issue_msg(e.clone());
    }

    // Warn up front about destination limitations that will force skips.
    let caps = target::probe(dst);
    if !caps.symlinks {
        let n = src_m.iter().filter(|e| e.kind == Kind::Symlink).count();
        if n > 0 {
            report.issue_msg(format!("destination cannot store symlinks; {n} will be skipped"));
        }
    }

    apply::apply(src, dst, &actions, &opts, &mut report);

    // Post-sync: rewrite into-source symlinks to point inside the mirror (opt-in).
    if a.relative_symlinks {
        apply::relink_internal_symlinks(src, dst, &src_m, &mut report);
    }

    report.finish();

    print!("{}", report.render());
    println!("report: {}", report_path.display());

    if report.issues.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Validate the source/destination pair before doing anything. Rejects a non-directory source and
/// any overlap between the two roots — identical, or one nested inside the other. Comparison is on
/// *canonical* paths, so an overlap can't be hidden behind a symlink, `..`, or a trailing-slash
/// alias.
fn validate_roots(from: &Path, to: &Path) -> Result<(), String> {
    if !from.is_dir() {
        return Err(format!("source is not a directory: {}", from.display()));
    }
    let cf = fs::canonicalize(from)
        .map_err(|e| format!("cannot resolve --from {}: {e}", from.display()))?;
    let ct = canonicalize_lenient(to);

    if cf == ct {
        Err("--from and --to are the same directory".to_string())
    } else if ct.starts_with(&cf) {
        Err(format!(
            "--to is inside --from — that would copy the tree into itself (from={}, to={})",
            cf.display(),
            ct.display()
        ))
    } else if cf.starts_with(&ct) {
        Err(format!(
            "--from is inside --to — mirror-delete could erase the source (from={}, to={})",
            cf.display(),
            ct.display()
        ))
    } else {
        Ok(())
    }
}

/// Canonicalize `path`, tolerating a not-yet-created tail: resolve the deepest existing ancestor
/// (following symlinks) and re-append the components that don't exist yet. This lets us compare a
/// destination that hasn't been created against the source while still resolving any symlinks in
/// its existing prefix.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(resolved) = fs::canonicalize(&cur) {
            let mut out = resolved;
            for comp in tail.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match cur.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                cur.pop();
            }
            None => return path.to_path_buf(), // nothing resolvable — use as given
        }
    }
}

/// Whether `a` and `b` live on the same filesystem (device). Off-unix, device introspection isn't
/// portable, so the check is skipped (returns `true`).
#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> std::io::Result<bool> {
    Ok(fs_device(a)? == fs_device(b)?)
}

/// Device id of the filesystem holding `path`, or — if `path` doesn't exist yet — of its nearest
/// existing ancestor (so a not-yet-created backup dir is judged by where it *would* be created).
#[cfg(unix)]
fn fs_device(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut cur = path;
    loop {
        if let Ok(m) = fs::metadata(cur) {
            return Ok(m.dev());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("cannot resolve {}", path.display()),
                ))
            }
        }
    }
}

#[cfg(not(unix))]
fn same_filesystem(_a: &Path, _b: &Path) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn backup_on_same_filesystem_is_allowed() {
        let t = tempfile::tempdir().unwrap();
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        // backup dir doesn't exist yet → judged by its nearest existing ancestor (the tempdir)
        assert!(same_filesystem(&t.path().join("backup"), &dst).unwrap());
    }

    #[test]
    fn validate_rejects_nonexistent_source() {
        let t = tempfile::tempdir().unwrap();
        let err = validate_roots(&t.path().join("nope"), &t.path().join("dst")).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn validate_rejects_file_source() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        fs::write(&f, b"x").unwrap();
        assert!(validate_roots(&f, &t.path().join("dst")).unwrap_err().contains("not a directory"));
    }

    #[test]
    fn validate_rejects_identical_roots() {
        let t = tempfile::tempdir().unwrap();
        assert!(validate_roots(t.path(), t.path()).unwrap_err().contains("same directory"));
    }

    #[test]
    fn validate_rejects_destination_inside_source() {
        let t = tempfile::tempdir().unwrap();
        // destination need not exist yet — canonicalize_lenient resolves its existing prefix
        let err = validate_roots(t.path(), &t.path().join("backup")).unwrap_err();
        assert!(err.contains("--to is inside --from"), "{err}");
    }

    #[test]
    fn validate_rejects_source_inside_destination() {
        let t = tempfile::tempdir().unwrap();
        let sub = t.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let err = validate_roots(&sub, t.path()).unwrap_err();
        assert!(err.contains("--from is inside --to"), "{err}");
    }

    #[test]
    fn validate_accepts_siblings_with_shared_name_prefix() {
        // `foo` must not count as "inside" `foobar` (component-wise, not string prefix)
        let t = tempfile::tempdir().unwrap();
        let foo = t.path().join("foo");
        let foobar = t.path().join("foobar");
        fs::create_dir(&foo).unwrap();
        fs::create_dir(&foobar).unwrap();
        assert!(validate_roots(&foo, &foobar).is_ok());
        assert!(validate_roots(&foobar, &foo).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn validate_detects_overlap_through_symlink() {
        let t = tempfile::tempdir().unwrap();
        let inside = t.path().join("inside");
        fs::create_dir(&inside).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&inside, &link).unwrap();
        // --to is a symlink resolving to a dir inside --from → must be caught
        let err = validate_roots(t.path(), &link).unwrap_err();
        assert!(err.contains("--to is inside --from"), "{err}");
    }

    #[test]
    fn lenient_canonicalize_extends_existing_prefix() {
        let t = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(t.path()).unwrap();
        assert_eq!(
            canonicalize_lenient(&t.path().join("nope/deep")),
            base.join("nope").join("deep")
        );
    }

    #[test]
    fn lenient_canonicalize_equals_canonicalize_when_present() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(canonicalize_lenient(t.path()), fs::canonicalize(t.path()).unwrap());
    }
}
