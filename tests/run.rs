//! End-to-end checks on the public `run` entry point: root validation (overlap rejection) and that
//! scan-time read errors reach the on-disk report. Cli is built directly (its fields are public),
//! so these don't depend on argument parsing.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use filesync::cli::{Cli, Command, Common, SyncArgs};
use filesync::run;

fn mk_sync_cli(from: &Path, to: &Path, report: Option<PathBuf>, relative_symlinks: bool) -> Cli {
    Cli {
        command: Command::Sync(SyncArgs {
            common: Common {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                eager_checksum: false,
                report,
            },
            no_verify: false,
            fsync_each: false,
            backup_dir: None,
            relative_symlinks,
        }),
    }
}

fn sync_cli(from: &Path, to: &Path, report: Option<PathBuf>) -> Cli {
    mk_sync_cli(from, to, report, false)
}

#[test]
fn run_rejects_destination_inside_source() {
    let outer = tempfile::tempdir().unwrap();
    let inner = outer.path().join("inner");
    fs::create_dir(&inner).unwrap();
    common::file(&inner, "keep.txt", b"precious");

    let report_dir = tempfile::tempdir().unwrap();
    let report_path = report_dir.path().join("r.txt");

    let _ = run(sync_cli(outer.path(), &inner, Some(report_path.clone())));

    // Rejected up front, before any work: no report was written, nothing was copied, and the
    // pre-existing destination content is untouched.
    assert!(!report_path.exists(), "run should reject before creating a report");
    assert!(!inner.join("inner").exists(), "no nested copy happened");
    assert_eq!(fs::read(inner.join("keep.txt")).unwrap(), b"precious");
}

#[test]
fn run_rejects_empty_source() {
    let src = tempfile::tempdir().unwrap(); // nothing inside
    let dst = tempfile::tempdir().unwrap();
    common::file(dst.path(), "existing.txt", b"keep me");

    let report_dir = tempfile::tempdir().unwrap();
    let report_path = report_dir.path().join("r.txt");

    let _ = run(sync_cli(src.path(), dst.path(), Some(report_path.clone())));

    // Refused before any deletion: the destination survives and no report was written.
    assert!(
        dst.path().join("existing.txt").is_file(),
        "an empty source must not wipe the destination"
    );
    assert!(!report_path.exists(), "rejected before creating a report");
}

#[cfg(unix)]
#[test]
fn run_writes_unreadable_directory_into_the_report() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(src.path(), "visible.txt", b"hi");
    common::file(src.path(), "vault/secret.txt", b"hidden");
    common::set_no_perms(src.path(), "vault");

    let report_dir = tempfile::tempdir().unwrap();
    let report_path = report_dir.path().join("r.txt");

    let _ = run(sync_cli(src.path(), dst.path(), Some(report_path.clone())));
    common::restore_perms(src.path(), "vault");

    let report = fs::read_to_string(&report_path).unwrap();
    assert!(report.contains("vault"), "report should name the unreadable dir:\n{report}");
    assert!(dst.path().join("visible.txt").is_file(), "readable content still synced");
}

#[cfg(unix)]
#[test]
fn run_relative_symlinks_flag_retargets_into_source_links() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f1/b.txt", b"payload");
    if std::os::unix::fs::symlink(src.path().join("f1/b.txt"), src.path().join("abs")).is_err() {
        eprintln!("skipping: no symlink support");
        return;
    }

    let report_dir = tempfile::tempdir().unwrap();
    let report_path = report_dir.path().join("r.txt");
    let _ = run(mk_sync_cli(src.path(), dst.path(), Some(report_path), true));

    let target = std::fs::read_link(dst.path().join("abs")).unwrap();
    assert!(
        target.is_relative() && !target.starts_with(src.path()),
        "the flag should retarget the link inside the mirror, got {target:?}"
    );
    assert_eq!(std::fs::read(dst.path().join("abs")).unwrap(), b"payload");
}
