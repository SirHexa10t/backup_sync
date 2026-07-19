//! End-to-end checks on the public `run` entry point: root validation (overlap rejection) and that
//! scan-time read errors reach the on-disk report. Cli is built directly (its fields are public),
//! so these don't depend on argument parsing.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use filesync::cli::{Cli, Command, Common, SyncArgs};
use filesync::run;

fn mk_sync_cli(
    from: &Path,
    to: &Path,
    report: Option<PathBuf>,
    relative_symlinks: bool,
    backup_dir: Option<PathBuf>,
) -> Cli {
    Cli {
        command: Command::Sync(SyncArgs {
            common: Common {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                eager_checksum: false,
                report,
                relative_symlinks,
                unelevated: false,
            },
            no_verify: false,
            fsync_each: false,
            backup_dir,
        }),
    }
}

fn sync_cli(from: &Path, to: &Path, report: Option<PathBuf>) -> Cli {
    mk_sync_cli(from, to, report, false, None)
}

fn diff_cli(from: &Path, to: &Path, report: &Path) -> Cli {
    Cli {
        command: Command::Diff(filesync::cli::DiffArgs {
            common: Common {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                eager_checksum: false,
                report: Some(report.to_path_buf()),
                relative_symlinks: false,
                unelevated: false,
            },
            include_same: false,
        }),
    }
}

#[test]
fn run_rejects_destination_inside_source() {
    let outer = tempfile::tempdir().unwrap();
    let inner = outer.path().join("inner");
    fs::create_dir(&inner).unwrap();
    common::file(&inner, "keep.txt", b"precious");

    let report_dir = tempfile::tempdir().unwrap();

    let _ = run(sync_cli(outer.path(), &inner, Some(report_dir.path().to_path_buf())));

    // Rejected up front, before any work: no report was written, nothing was copied, and the
    // pre-existing destination content is untouched.
    assert!(
        find_output(report_dir.path(), ".findings.txt").is_none(),
        "run should reject before creating a report"
    );
    assert!(!inner.join("inner").exists(), "no nested copy happened");
    assert_eq!(fs::read(inner.join("keep.txt")).unwrap(), b"precious");
}

#[test]
fn run_rejects_empty_source() {
    let src = tempfile::tempdir().unwrap(); // nothing inside
    let dst = tempfile::tempdir().unwrap();
    common::file(dst.path(), "existing.txt", b"keep me");

    let report_dir = tempfile::tempdir().unwrap();

    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));

    // Refused before any deletion: the destination survives and no report was written.
    assert!(
        dst.path().join("existing.txt").is_file(),
        "an empty source must not wipe the destination"
    );
    assert!(
        find_output(report_dir.path(), ".findings.txt").is_none(),
        "rejected before creating a report"
    );
}

#[cfg(unix)]
#[test]
fn run_writes_unreadable_directory_into_the_errors_file() {
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

    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));
    common::restore_perms(src.path(), "vault");

    // An unreadable directory is an issue → it lands in the companion errors file, labeled by side.
    let errors = read_output(report_dir.path(), ".errors.txt");
    assert!(errors.contains("vault"), "errors file should name the unreadable dir:\n{errors}");
    assert!(errors.contains("source:"), "…and label its side:\n{errors}");
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
    let _ = run(mk_sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf()), true, None));

    let target = std::fs::read_link(dst.path().join("abs")).unwrap();
    assert!(
        target.is_relative() && !target.starts_with(src.path()),
        "the flag should retarget the link inside the mirror, got {target:?}"
    );
    assert_eq!(std::fs::read(dst.path().join("abs")).unwrap(), b"payload");
}

/// An output directory for tests that don't inspect the report — kept out of the project's CWD
/// (where the default would land) so `cargo test` never litters the repo. `--report` takes a dir.
fn scratch_report(dir: &tempfile::TempDir) -> Option<PathBuf> {
    Some(dir.path().to_path_buf())
}

/// The single output file in `dir` whose name ends with `suffix` (e.g. `.findings.txt`), if any.
/// Filenames are timestamped, so tests discover them by kind rather than hard-coding the name.
fn find_output(dir: &Path, suffix: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(suffix)))
}

/// Read the one output file of the given kind, panicking (with the dir listing) if it's missing.
fn read_output(dir: &Path, suffix: &str) -> String {
    let p = find_output(dir, suffix)
        .unwrap_or_else(|| panic!("no {suffix} file in {:?}", fs::read_dir(dir).map(|r| r.count())));
    fs::read_to_string(p).unwrap()
}

#[test]
fn run_creates_a_missing_destination() {
    let src = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");
    let holder = tempfile::tempdir().unwrap();
    let dst = holder.path().join("mirror/depth"); // doesn't exist yet, two levels
    let rep = tempfile::tempdir().unwrap();

    let _ = run(sync_cli(src.path(), &dst, scratch_report(&rep)));

    assert_eq!(fs::read(dst.join("f.txt")).unwrap(), b"data", "destination created and mirrored");
}

/// The report must not land inside the source (a read-only tree) — including via the DEFAULT
/// report path when the current directory is inside the source.
#[test]
fn run_rejects_report_inside_source() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");

    // --report names a directory; the source itself lies inside the source → rejected up front.
    let _ = run(sync_cli(src.path(), dst.path(), Some(src.path().to_path_buf())));

    assert!(
        find_output(src.path(), ".findings.txt").is_none(),
        "nothing may be written into the source"
    );
    assert!(!dst.path().join("f.txt").exists(), "the run must be refused before copying");
}

/// The report must not land inside the destination — the next run would mirror-delete it.
#[test]
fn run_rejects_report_inside_destination() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");

    let _ = run(sync_cli(src.path(), dst.path(), Some(dst.path().to_path_buf())));

    assert!(find_output(dst.path(), ".findings.txt").is_none());
    assert!(!dst.path().join("f.txt").exists(), "the run must be refused before copying");
}

#[test]
fn run_resumes_after_interrupted_copy_and_reports_counts() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "a.txt", b"aaa");
    common::file(src.path(), "sub/b.txt", b"bbb");
    // simulate a previous interrupted run: stray atomic-copy temp files at the destination
    common::file(dst.path(), ".filesync_staging.tmp.4242.a.txt", b"partial");
    common::file(dst.path(), "sub/.filesync_staging.tmp.7.b.txt", b"partial");

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));

    // strays swept, real content copied
    assert!(!dst.path().join(".filesync_staging.tmp.4242.a.txt").exists());
    assert!(!dst.path().join("sub/.filesync_staging.tmp.7.b.txt").exists());
    assert_eq!(fs::read(dst.path().join("a.txt")).unwrap(), b"aaa");
    assert_eq!(fs::read(dst.path().join("sub/b.txt")).unwrap(), b"bbb");

    // the report carries the final counts with no issues …
    let rep = read_output(report_dir.path(), ".findings.txt");
    assert!(rep.contains("copied:  2"), "report should count both copies:\n{rep}");
    assert!(rep.contains("issues:  0"), "no issues expected:\n{rep}");
    // … and a clean run leaves NO errors file at all
    assert!(
        find_output(report_dir.path(), ".errors.txt").is_none(),
        "a clean run must not create an errors file"
    );
}

/// THE data-safety valve (audit finding #1): when any part of the source can't be read, its files
/// are invisible — a mirror would classify their destination twins as "extra" and delete the
/// (possibly last) copy. With an unreadable source subtree, NOTHING may be deleted.
#[cfg(unix)]
#[test]
fn unreadable_source_subtree_suspends_all_deletes() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(src.path(), "ok.txt", b"new content");
    common::file(src.path(), "vault/precious.txt", b"the only other copy is at dst");
    // destination already mirrors vault/, and has one genuinely-extra file elsewhere
    common::file(dst.path(), "vault/precious.txt", b"the only other copy is at dst");
    common::file(dst.path(), "genuinely_extra.txt", b"stale but must survive this run");
    common::set_no_perms(src.path(), "vault"); // source subtree becomes unreadable

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));
    common::restore_perms(src.path(), "vault");
    // dir-metadata mirroring faithfully copied the 000 mode onto the destination's vault/ —
    // reopen it so the assertions (and the tempdir cleanup) can see inside.
    common::restore_perms(dst.path(), "vault");

    // nothing was deleted — neither the invisible subtree's twin nor even the genuine extra
    assert!(
        dst.path().join("vault/precious.txt").is_file(),
        "the destination copy of the unreadable subtree must survive"
    );
    assert!(
        dst.path().join("genuinely_extra.txt").is_file(),
        "ALL deletions are suspended while the source view is incomplete"
    );
    // additive work still happened
    assert_eq!(fs::read(dst.path().join("ok.txt")).unwrap(), b"new content");
    // the report counts zero deletions …
    let rep = read_output(report_dir.path(), ".findings.txt");
    assert!(rep.contains("deleted: 0"), "no deletions may be counted:\n{rep}");
    // … and the errors file states the suspension
    let errors = read_output(report_dir.path(), ".errors.txt");
    assert!(
        errors.contains("suspended"),
        "errors file must state deletions were suspended:\n{errors}"
    );
}

/// Fix B: a source *file* that's listable but unreadable (parent dir readable, so the scan sees it
/// with no walkdir error) must still suspend deletions. The dangerous case: it's a move-candidate
/// (source-only, e.g. a rename), so its would-be partner among the destination extras would be
/// deleted instead of matched — potentially the last copy of that content. Nothing may be deleted.
#[cfg(unix)]
#[test]
fn unreadable_source_file_suspends_deletes() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(src.path(), "readable.txt", b"so the source isn't empty");
    // a source-only, unreadable file (a rename's new name), same size as a destination extra
    common::file(src.path(), "renamed.bin", b"PRECIOUS-PAYLOAD");
    common::set_no_perms(src.path(), "renamed.bin");
    // the destination holds the old name (the move partner) AND an unrelated extra
    common::file(dst.path(), "original.bin", b"PRECIOUS-PAYLOAD");
    common::file(dst.path(), "unrelated_extra.txt", b"also must survive");

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));
    common::restore_perms(src.path(), "renamed.bin");

    // no walkdir error occurred (the file was listable), yet ALL deletes are suspended:
    assert!(
        dst.path().join("original.bin").is_file(),
        "the move partner must NOT be deleted — it may be the only readable copy of that content"
    );
    assert!(dst.path().join("unrelated_extra.txt").is_file(), "every deletion is suspended");
    let rep = read_output(report_dir.path(), ".findings.txt");
    assert!(rep.contains("deleted: 0"), "no deletions counted:\n{rep}");
    let errors = read_output(report_dir.path(), ".errors.txt");
    assert!(errors.contains("suspended"), "errors file states deletions were suspended:\n{errors}");
    assert!(errors.contains("source:"), "the unreadable source file is reported, labeled:\n{errors}");
}

/// The showstoppers file: an unreadable source file (mode 000) must be PREDICTED from the scan's
/// owner/mode bits — before any operation even fails on it — listed as a bash array entry with a
/// paste-able remedy loop, quoted, with the owner/mode/confidence comment.
#[cfg(unix)]
#[test]
fn showstoppers_predicts_unreadable_source_file() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(src.path(), "fine.txt", b"ok");
    common::file(src.path(), "locked.bin", b"cannot-read-me"); // unique size → not a move candidate
    common::set_no_perms(src.path(), "locked.bin");

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(diff_cli(src.path(), dst.path(), report_dir.path()));
    common::restore_perms(src.path(), "locked.bin");

    let stoppers = read_output(report_dir.path(), ".showstoppers.txt");
    assert!(
        stoppers.contains("filesync_source_unreadable_files=("),
        "the verbose array is defined:\n{stoppers}"
    );
    let abs_src = fs::canonicalize(src.path()).unwrap();
    let quoted = format!("'{}'", abs_src.join("locked.bin").display());
    assert!(stoppers.contains(&quoted), "the ABSOLUTE path is quoted in the array:\n{stoppers}");
    assert!(stoppers.contains("0000 predicted"), "mode + confidence comment:\n{stoppers}");
    assert!(
        stoppers.contains("for f in \"${filesync_source_unreadable_files[@]}\"; do"),
        "the remedy loop follows the array:\n{stoppers}"
    );
}

/// Undeletable destination extras are predicted from the PARENT directory's write bits: an extra
/// inside a read-only dir can't be unlinked, so it lands in the showstoppers before sync fails on
/// it. Clean runs, by contrast, must not create the file at all.
#[cfg(unix)]
#[test]
fn showstoppers_predicts_undeletable_extras_and_absent_when_clean() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(src.path(), "keep.txt", b"k");

    // clean pass first: no showstoppers file at all
    let clean_dir = tempfile::tempdir().unwrap();
    let _ = run(diff_cli(src.path(), dst.path(), clean_dir.path()));
    assert!(
        find_output(clean_dir.path(), ".showstoppers.txt").is_none(),
        "a clean run must not leave a showstoppers file"
    );

    // now: an extra inside a directory the user can't write into (r-x) — deletion will fail
    common::file(dst.path(), "frozen/extra.bin", b"stale");
    let frozen = dst.path().join("frozen");
    let mut perms = fs::metadata(&frozen).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o555);
    fs::set_permissions(&frozen, perms).unwrap();

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(diff_cli(src.path(), dst.path(), report_dir.path()));

    // reopen for cleanup before asserting
    let mut perms = fs::metadata(&frozen).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&frozen, perms).unwrap();

    let stoppers = read_output(report_dir.path(), ".showstoppers.txt");
    assert!(
        stoppers.contains("filesync_destination_undeletable_files=("),
        "undeletable section present:\n{stoppers}"
    );
    let abs_dst = fs::canonicalize(dst.path()).unwrap();
    assert!(
        stoppers.contains(&format!("'{}'", abs_dst.join("frozen/extra.bin").display())),
        "the walled extra is listed by absolute path:\n{stoppers}"
    );
    assert!(stoppers.contains("predicted"), "{stoppers}");
    assert!(stoppers.contains("sudo rm -- \"$f\""), "the remedy is the explicit rm loop:\n{stoppers}");
}

/// Under deletion suspension, a planned rename whose TARGET is occupied at the destination must be
/// deferred: its target-clearing delete was suspended, and a raw `fs::rename` would silently
/// replace the occupant (file/symlink) with no record and no --backup-dir move-aside. The occupant
/// might be the last copy of something the unreadable part of the source is hiding.
#[cfg(unix)]
#[test]
fn suspension_defers_renames_onto_occupied_targets() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    // suspension trigger: an unreadable source subtree
    common::file(src.path(), "vault/hidden.txt", b"invisible");
    common::set_no_perms(src.path(), "vault");
    // kind-swap + move: the source has a FILE at `swap`; the destination has a SYMLINK there,
    // while the file's content sits at a different destination path (the move partner)
    common::file(src.path(), "swap", b"MOVE-PAYLOAD");
    common::file(dst.path(), "old_home.bin", b"MOVE-PAYLOAD");
    std::os::unix::fs::symlink("anywhere", dst.path().join("swap")).unwrap();

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));
    common::restore_perms(src.path(), "vault");
    if dst.path().join("vault").exists() {
        common::restore_perms(dst.path(), "vault"); // dir-metadata mirroring copied the 000 mode
    }

    // the occupant survived (the rename was deferred, not executed over it) …
    let md = fs::symlink_metadata(dst.path().join("swap")).unwrap();
    assert!(md.file_type().is_symlink(), "the occupant at the rename target must survive");
    // … the move partner is untouched, and nothing was deleted or renamed
    assert_eq!(fs::read(dst.path().join("old_home.bin")).unwrap(), b"MOVE-PAYLOAD");
    let rep = read_output(report_dir.path(), ".findings.txt");
    assert!(rep.contains("deleted: 0"), "{rep}");
    assert!(rep.contains("moved:   0"), "the rename must not have run:\n{rep}");
    let errors = read_output(report_dir.path(), ".errors.txt");
    assert!(
        errors.contains("deferred rename") && errors.contains("old_home.bin"),
        "the deferred rename is recorded, per-path, in the errors file:\n{errors}"
    );
}

/// Audit finding #2b: the backup dir receives writes, so it must never be inside the read-only
/// source. Rejected before anything is copied, deleted, or created.
#[test]
fn run_rejects_backup_dir_inside_source() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");

    let backup = src.path().join("backups"); // inside the source
    let _ = run(mk_sync_cli(src.path(), dst.path(), None, false, Some(backup.clone())));

    assert!(!dst.path().join("f.txt").exists(), "sync must be refused before copying");
    assert!(!backup.exists(), "nothing may be written into the source");
}

/// Audit finding #2a, part 1: a backup dir may live inside the destination — it gets a marker
/// file, and the NEXT run's scan ignores it instead of mirror-deleting the saved files.
#[test]
fn backup_dir_inside_destination_survives_the_next_run() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "keep.txt", b"k");
    common::file(dst.path(), "doomed.txt", b"save me"); // extra → will be moved aside

    // run 1: extras land in dst/.trash, which gets the marker
    let rep = tempfile::tempdir().unwrap();
    let trash = dst.path().join(".trash");
    let _ = run(mk_sync_cli(src.path(), dst.path(), scratch_report(&rep), false, Some(trash.clone())));
    assert_eq!(fs::read(trash.join("doomed.txt")).unwrap(), b"save me", "moved aside, not erased");
    assert!(trash.join(".filesync-backup-dir").is_file(), "backup dir must carry the marker");

    // run 2: NO backup dir — the marked dir must be invisible, not treated as an extra
    let rep2 = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), scratch_report(&rep2)));
    assert_eq!(
        fs::read(trash.join("doomed.txt")).unwrap(),
        b"save me",
        "a later run must never mirror-delete the marked backup dir"
    );
    assert!(trash.join(".filesync-backup-dir").is_file());
    assert!(dst.path().join("keep.txt").is_file(), "the mirror itself still syncs normally");
}

/// Audit finding #2a, part 2: a used backup dir (marker present ⇒ not empty) can't be reused —
/// same-named files from different runs would silently overwrite each other.
#[test]
fn backup_dir_reuse_is_rejected() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let holder = tempfile::tempdir().unwrap();
    let backup = holder.path().join("bk");

    // run 1 uses the backup dir (one extra gets moved aside into it)
    let rep = tempfile::tempdir().unwrap();
    common::file(src.path(), "a.txt", b"a");
    common::file(dst.path(), "extra.txt", b"first run's version");
    let _ = run(mk_sync_cli(src.path(), dst.path(), scratch_report(&rep), false, Some(backup.clone())));
    assert_eq!(fs::read(backup.join("extra.txt")).unwrap(), b"first run's version");

    // run 2 with the SAME dir must be refused outright: nothing synced, backups untouched
    let rep2 = tempfile::tempdir().unwrap();
    common::file(src.path(), "b.txt", b"b");
    let _ = run(mk_sync_cli(src.path(), dst.path(), scratch_report(&rep2), false, Some(backup.clone())));
    assert!(!dst.path().join("b.txt").exists(), "reuse must refuse the whole run");
    assert_eq!(
        fs::read(backup.join("extra.txt")).unwrap(),
        b"first run's version",
        "the previous run's backups stay untouched"
    );
}

/// Run the real binary so stderr is a pipe (non-terminal): progress must stream to stderr, the
/// compact summary and suspension preview to stdout, and the full listing + issues to the two
/// output files — none of these mixing with the others.
#[cfg(unix)]
#[test]
fn diff_subprocess_streams_progress_and_writes_findings_and_errors_files() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(src.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(src.path(), "ok.txt", b"fine");
    common::file(src.path(), "vault/hidden.txt", b"x");
    common::file(dst.path(), "stale_extra.txt", b"would be deleted");
    common::set_no_perms(src.path(), "vault"); // source view becomes incomplete

    // --report is a directory; it keeps the output files out of the repo (and exercises that path).
    let report_dir = tempfile::tempdir().unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_filesync"))
        .args(["diff", "--from"])
        .arg(src.path())
        .arg("--to")
        .arg(dst.path())
        .arg("--report")
        .arg(report_dir.path())
        .output()
        .expect("run the filesync binary");
    common::restore_perms(src.path(), "vault");

    let err = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // progress streams to stderr (non-tty log mode) …
    assert!(
        err.contains("scanned") && err.contains("entries"),
        "non-tty stderr should carry log-mode scan summaries:\n{err}"
    );
    // … while the count summary and the suspension preview go to stdout (never the full listing)
    assert!(stdout.contains("to delete: 1"), "the count summary is on stdout:\n{stdout}");
    assert!(
        stdout.contains("SUSPEND"),
        "diff must say a sync would suspend the listed deletions:\n{stdout}"
    );
    assert!(out.status.success(), "diff is a preview — it exits 0");

    // the full per-file listing is in the findings file …
    let findings = read_output(report_dir.path(), ".findings.txt");
    assert!(findings.contains("- stale_extra.txt"), "findings file lists the deletion:\n{findings}");
    // … the conclusions file is always written …
    let conclusions = read_output(report_dir.path(), ".conclusions.txt");
    assert!(conclusions.contains("DATA-LOSS WATCH"), "conclusions written:\n{conclusions}");
    // … and the unreadable source dir is recorded, labeled, in the errors file
    let errors = read_output(report_dir.path(), ".errors.txt");
    assert!(
        errors.contains("source:") && errors.contains("vault"),
        "errors file records the labeled source read failure:\n{errors}"
    );
}

/// Concurrent syncs on one destination are forbidden: they'd sweep each other's staging files and
/// plan from snapshots the other invalidates. A held lock refuses the second run outright.
#[test]
fn run_refuses_a_locked_destination() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");
    // a lock held by a LIVE process (ours) — as if another filesync were mid-run
    fs::write(dst.path().join(".filesync.lock"), format!("{}\n", std::process::id())).unwrap();

    let rep = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), scratch_report(&rep)));

    assert!(!dst.path().join("f.txt").exists(), "the locked destination must not be touched");
    assert!(dst.path().join(".filesync.lock").is_file(), "the other run's lock stays");
}

/// The lockfile is the running sync's own artifact: it must be released afterwards, and it must
/// never be treated as destination content (mirror-deleted or backed up).
#[test]
fn lock_is_released_and_never_treated_as_content() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");

    let rep = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), scratch_report(&rep)));

    assert_eq!(fs::read(dst.path().join("f.txt")).unwrap(), b"data", "sync ran normally");
    assert!(!dst.path().join(".filesync.lock").exists(), "lock released after the run");
}

/// Special files land in the report's `skipped` section — visible, but not a failure.
#[cfg(unix)]
#[test]
fn run_reports_special_files_as_skipped_not_issues() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "data.txt", b"x");
    if !common::make_fifo(src.path(), "pipe") {
        eprintln!("skipping: filesystem lacks fifo support");
        return;
    }

    let report_dir = tempfile::tempdir().unwrap();
    let _ = run(sync_cli(src.path(), dst.path(), Some(report_dir.path().to_path_buf())));

    let rep = read_output(report_dir.path(), ".findings.txt");
    assert!(rep.contains("skipped: 1"), "skip counted:\n{rep}");
    assert!(rep.contains("issues:  0"), "…without becoming an issue:\n{rep}");
    assert!(rep.contains("  ~ pipe"), "…and listed with the ~ marker:\n{rep}");
    assert!(rep.contains("run completed"));
}

/// `--backup-dir` on a different filesystem must be rejected up front (its rename-based move-aside
/// can't cross devices). Uses /dev/shm (tmpfs) when it's on a different device than the tempdir;
/// skips otherwise.
#[cfg(unix)]
#[test]
fn run_rejects_backup_dir_on_a_different_filesystem() {
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    common::file(src.path(), "f.txt", b"data");

    let shm = Path::new("/dev/shm");
    let same_dev = match (fs::metadata(shm), fs::metadata(dst.path())) {
        (Ok(a), Ok(b)) => a.dev() == b.dev(),
        _ => true,
    };
    if same_dev {
        eprintln!("skipping: no second filesystem available to test against");
        return;
    }

    // backup dir on tmpfs (never created — validation must reject before any work)
    let backup = shm.join("filesync-test-backup-probe");
    let _ = run(mk_sync_cli(src.path(), dst.path(), None, false, Some(backup.clone())));

    assert!(!dst.path().join("f.txt").exists(), "sync must be refused before copying");
    assert!(!backup.exists(), "backup dir must not be created on the wrong filesystem");
}
