//! Sync (apply) invariants: DST == SRC (round-trip), source untouched, moves execute as renames
//! (inode preserved), mirror deletes, atomic overwrite, backup-dir, verify, and interrupt-safety.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use filesync::apply::{apply, verify_matches, Options};
use filesync::diff::diff;
use filesync::manifest::{DstRoot, Kind, SrcRoot};
use filesync::plan::{plan, Action};
use filesync::progress::Progress;
use filesync::report::Report;
use filesync::scan::scan;

fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

fn default_opts() -> Options {
    Options { verify: true, fsync_each: false, backup_dir: None, relative_symlinks: false }
}

/// Full pipeline: scan → diff → plan → apply. Honors `opts.relative_symlinks` in the diff too,
/// exactly like `run_sync` does.
fn sync_with(src: &Path, dst: &Path, opts: &Options) -> Report {
    let (s, d) = (SrcRoot::new(src), DstRoot::new(dst));
    let (sm, dm) = (scan(src), scan(dst));
    let df = diff(&s, &sm, &d, &dm, false, opts.relative_symlinks, false);
    let actions = plan(&df);
    let mut r = Report::new();
    for issue in df.issues {
        r.issue_msg(issue);
    }
    apply(&s, &d, &sm, &actions, opts, &mut r, &Progress::hidden(), &AtomicBool::new(false));
    r
}

/// rel → content-hash for every file (ignores mtime, for content comparison).
fn content_map(root: &Path) -> BTreeMap<PathBuf, [u8; 32]> {
    common::snapshot_files(root).into_iter().map(|(k, (_, _, h))| (k, h)).collect()
}

#[test]
fn round_trip_destination_matches_source() {
    let (s, d) = dirs();
    common::build_corpus(s.path());
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "unexpected issues: {:?}", r.issues);

    // every file present with identical content
    assert_eq!(content_map(s.path()), content_map(d.path()));
    // an empty directory was reproduced
    assert!(d.path().join("empty_dir").is_dir());

    // a symlink was reproduced (when the fs supports them)
    #[cfg(unix)]
    if scan(s.path()).iter().any(|e| e.rel == PathBuf::from("links/rel")) {
        let dm = scan(d.path());
        let sl = dm.iter().find(|e| e.rel == PathBuf::from("links/rel")).expect("symlink copied");
        assert_eq!(sl.kind, Kind::Symlink);
        assert_eq!(sl.link_target.as_deref(), Some(Path::new("../f1/b.txt")));
    }
}

#[test]
fn sync_does_not_modify_the_source() {
    let (s, d) = dirs();
    common::build_corpus(s.path());
    let before = common::snapshot_files(s.path());
    let _ = sync_with(s.path(), d.path(), &default_opts());
    let after = common::snapshot_files(s.path());
    assert_eq!(before, after, "source must be untouched");
}

#[cfg(unix)]
#[test]
fn move_executes_as_rename_preserving_inode() {
    use std::os::unix::fs::MetadataExt;
    let (s, d) = dirs();
    common::file(s.path(), "new/here.txt", b"PAYLOAD");
    common::file(d.path(), "old/there.txt", b"PAYLOAD"); // same content, old path

    let before = fs::metadata(d.path().join("old/there.txt")).unwrap().ino();
    let r = sync_with(s.path(), d.path(), &default_opts());
    let after = fs::metadata(d.path().join("new/here.txt")).unwrap().ino();

    assert_eq!(r.moved, 1);
    assert_eq!(before, after, "same inode ⇒ renamed in place, not re-copied");
    assert!(!d.path().join("old/there.txt").exists());
    assert!(!d.path().join("old").exists(), "emptied dir removed");
}

#[test]
fn mirror_deletes_extras() {
    let (s, d) = dirs();
    common::file(s.path(), "keep.txt", b"k");
    common::file(d.path(), "extra.txt", b"e");
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(d.path().join("keep.txt").is_file());
    assert!(!d.path().join("extra.txt").exists());
    assert_eq!(r.deleted, 1);
}

#[test]
fn changed_file_is_overwritten() {
    let (s, d) = dirs();
    common::file(s.path(), "f.txt", b"NEW");
    common::file(d.path(), "f.txt", b"old-and-longer");
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty());
    assert_eq!(fs::read(d.path().join("f.txt")).unwrap(), b"NEW");
}

#[test]
fn copy_preserves_mtime() {
    let (s, d) = dirs();
    common::file(s.path(), "f.txt", b"x");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    common::set_mtime(s.path(), "f.txt", t);
    sync_with(s.path(), d.path(), &default_opts());
    let dm = fs::metadata(d.path().join("f.txt")).unwrap().modified().unwrap();
    let delta = dm.duration_since(t).or_else(|_| t.duration_since(dm)).unwrap();
    assert!(delta < Duration::from_secs(1), "mtime preserved (within fs precision)");
}

#[test]
fn backup_dir_preserves_deleted_files() {
    let (s, d) = dirs();
    let backup = tempfile::tempdir().unwrap();
    common::file(d.path(), "gone.txt", b"important");
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), relative_symlinks: false };
    let r = sync_with(s.path(), d.path(), &opts);
    assert_eq!(r.deleted, 1);
    assert!(!d.path().join("gone.txt").exists());
    assert_eq!(fs::read(backup.path().join("gone.txt")).unwrap(), b"important");
}

#[test]
fn backup_dir_preserves_overwritten_files() {
    let (s, d) = dirs();
    let backup = tempfile::tempdir().unwrap();
    common::file(s.path(), "f.txt", b"NEW-CONTENT");
    common::file(d.path(), "f.txt", b"OLD-CONTENT-and-longer"); // will be overwritten
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), relative_symlinks: false };
    let r = sync_with(s.path(), d.path(), &opts);
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    // new content in place at the destination
    assert_eq!(fs::read(d.path().join("f.txt")).unwrap(), b"NEW-CONTENT");
    // the overwritten version was preserved in the backup dir
    assert_eq!(fs::read(backup.path().join("f.txt")).unwrap(), b"OLD-CONTENT-and-longer");
}

#[test]
fn backup_dir_ignores_fresh_additions() {
    // a brand-new file has no prior version, so nothing should land in the backup dir
    let (s, d) = dirs();
    let backup = tempfile::tempdir().unwrap();
    common::file(s.path(), "new.txt", b"fresh");
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), relative_symlinks: false };
    sync_with(s.path(), d.path(), &opts);
    assert!(d.path().join("new.txt").is_file());
    assert!(!backup.path().join("new.txt").exists(), "no backup for a fresh add");
}

#[cfg(unix)]
#[test]
fn unreadable_source_file_is_reported_and_skipped() {
    let (s, d) = dirs();
    if !common::permissions_enforced(s.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(s.path(), "ok.txt", b"copyable");
    common::file(s.path(), "locked.bin", b"secret");
    common::set_no_perms(s.path(), "locked.bin"); // present in the listing, but can't be read

    let r = sync_with(s.path(), d.path(), &default_opts());
    common::restore_perms(s.path(), "locked.bin");

    assert!(d.path().join("ok.txt").is_file(), "readable file still copied");
    assert!(!d.path().join("locked.bin").exists(), "unreadable file not copied");
    assert!(
        r.issues.iter().any(|i| i.contains("locked.bin")),
        "expected an issue for locked.bin: {:?}",
        r.issues
    );
}

#[test]
fn fsync_each_still_round_trips() {
    let (s, d) = dirs();
    common::file(s.path(), "a/x.txt", b"content");
    common::file(s.path(), "b.txt", b"more");
    let opts = Options { verify: true, fsync_each: true, backup_dir: None, relative_symlinks: false };
    let r = sync_with(s.path(), d.path(), &opts);
    assert!(r.issues.is_empty());
    assert_eq!(content_map(s.path()), content_map(d.path()));
}

#[test]
fn no_temp_files_remain_after_sync() {
    let (s, d) = dirs();
    common::build_corpus(s.path());
    sync_with(s.path(), d.path(), &default_opts());
    // a destination scan afterwards sweeps nothing ⇒ atomic copies left no temp files
    let (_, swept) = filesync::scan::scan_destination(&DstRoot::new(d.path()), &mut filesync::progress::ScanProgress::hidden());
    assert_eq!(swept, 0);
}

#[test]
fn destination_scan_sweeps_leftover_temp_files_only() {
    let (_s, d) = dirs();
    common::file(d.path(), "keep.txt", b"k");
    common::file(d.path(), ".filesync_staging.tmp.999.old", b"junk");
    common::file(d.path(), "sub/.filesync_staging.tmp.999.old2", b"junk");
    let (outcome, swept) = filesync::scan::scan_destination(&DstRoot::new(d.path()), &mut filesync::progress::ScanProgress::hidden());
    assert_eq!(swept, 2);
    assert!(d.path().join("keep.txt").is_file());
    assert!(!d.path().join(".filesync_staging.tmp.999.old").exists());
    assert!(!d.path().join("sub/.filesync_staging.tmp.999.old2").exists());
    // and the manifest holds the real content, not the swept scratch
    assert!(outcome.manifest.iter().any(|e| e.rel == PathBuf::from("keep.txt")));
    assert!(!outcome.manifest.iter().any(|e| e.rel.to_string_lossy().contains(".filesync_staging.tmp.")));
}

#[test]
fn verify_matches_detects_mismatch() {
    let t = tempfile::tempdir().unwrap();
    common::file(t.path(), "f.txt", b"hello");
    common::file(t.path(), "g.txt", b"different");
    let good = filesync::hash::hash_file(&t.path().join("f.txt")).unwrap();
    let other = filesync::hash::hash_file(&t.path().join("g.txt")).unwrap();
    assert!(verify_matches(&t.path().join("f.txt"), &good).unwrap());
    assert!(!verify_matches(&t.path().join("f.txt"), &other).unwrap());
}

#[test]
fn failed_copy_leaves_no_partial_file() {
    // A Copy whose source doesn't exist (as if interrupted / vanished): it must be reported, and
    // must not leave a half-written real file at the destination.
    let (s, d) = dirs();
    let actions = vec![Action::Copy(PathBuf::from("ghost.txt"))];
    let mut r = Report::new();
    apply(
        &SrcRoot::new(s.path()),
        &DstRoot::new(d.path()),
        &filesync::manifest::Manifest::default(),
        &actions,
        &default_opts(),
        &mut r,
        &Progress::hidden(),
        &AtomicBool::new(false),
    );
    assert_eq!(r.issues.len(), 1);
    assert!(!d.path().join("ghost.txt").exists());
}

/// Graceful early-stop: a set flag ends the apply loop between actions, so nothing further is
/// written and the report is marked incomplete (which makes the run exit non-zero). The current
/// file always finishes because the flag is only checked between actions.
#[test]
fn graceful_stop_halts_before_the_next_action_and_marks_the_report() {
    let (s, d) = dirs();
    common::file(s.path(), "a.txt", b"one");
    common::file(s.path(), "b.txt", b"two");
    let sm = scan(s.path());
    let actions = plan(&diff(
        &SrcRoot::new(s.path()),
        &sm,
        &DstRoot::new(d.path()),
        &scan(d.path()),
        false,
        false,
        false,
    ));
    assert!(!actions.is_empty(), "there is real work to stop");

    // stop already requested → the loop breaks before the first action
    let mut r = Report::new();
    apply(
        &SrcRoot::new(s.path()),
        &DstRoot::new(d.path()),
        &sm,
        &actions,
        &default_opts(),
        &mut r,
        &Progress::hidden(),
        &AtomicBool::new(true),
    );
    assert_eq!(r.copied, 0, "a requested stop performs no further copies");
    assert!(!d.path().join("a.txt").exists(), "nothing new is written after the stop");
    assert!(r.was_stopped_early(), "the report records the incomplete run");
}

#[test]
fn kind_swap_file_to_directory_is_resolved() {
    let (s, d) = dirs();
    common::file(d.path(), "x", b"i am a file"); // destination: x is a FILE
    common::file(s.path(), "x/inner.txt", b"now a dir"); // source: x is a DIRECTORY
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert!(d.path().join("x").is_dir(), "x should now be a directory");
    assert_eq!(fs::read(d.path().join("x/inner.txt")).unwrap(), b"now a dir");
}

#[test]
fn kind_swap_directory_to_file_is_resolved() {
    let (s, d) = dirs();
    common::file(d.path(), "y/old.txt", b"old"); // destination: y is a DIRECTORY
    common::file(s.path(), "y", b"i am a file now"); // source: y is a FILE
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert!(d.path().join("y").is_file(), "y should now be a file");
    assert_eq!(fs::read(d.path().join("y")).unwrap(), b"i am a file now");
}

// A kind-swap that also has --backup-dir: the lingering entry must be moved aside, not erased.

#[test]
fn kind_swap_file_to_directory_backs_up_the_old_file() {
    let (s, d) = dirs();
    let backup = tempfile::tempdir().unwrap();
    common::file(d.path(), "x", b"was a file"); // destination: x is a FILE
    common::file(s.path(), "x/inner.txt", b"now a dir"); // source: x is a DIRECTORY
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), relative_symlinks: false };
    let r = sync_with(s.path(), d.path(), &opts);
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert!(d.path().join("x").is_dir());
    assert_eq!(fs::read(d.path().join("x/inner.txt")).unwrap(), b"now a dir");
    // the lingering file was preserved in the backup dir, not deleted
    assert_eq!(fs::read(backup.path().join("x")).unwrap(), b"was a file");
}

#[test]
fn kind_swap_directory_to_file_backs_up_the_old_contents() {
    let (s, d) = dirs();
    let backup = tempfile::tempdir().unwrap();
    common::file(d.path(), "y/old.txt", b"old data"); // destination: y is a DIRECTORY
    common::file(s.path(), "y", b"now a file"); // source: y is a FILE
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), relative_symlinks: false };
    let r = sync_with(s.path(), d.path(), &opts);
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert!(d.path().join("y").is_file());
    assert_eq!(fs::read(d.path().join("y")).unwrap(), b"now a file");
    // the file that lived under the old directory was preserved in backup
    assert_eq!(fs::read(backup.path().join("y/old.txt")).unwrap(), b"old data");
}

// --- idempotence: the core "write as little as possible" property ---

#[test]
fn second_sync_is_a_complete_noop() {
    let (s, d) = dirs();
    common::build_corpus(s.path());
    let r1 = sync_with(s.path(), d.path(), &default_opts());
    assert!(r1.issues.is_empty(), "first sync issues: {:?}", r1.issues);

    let before = common::snapshot_files(d.path());
    let r2 = sync_with(s.path(), d.path(), &default_opts());
    assert_eq!((r2.copied, r2.moved, r2.deleted), (0, 0, 0), "second sync must write nothing");
    assert!(r2.issues.is_empty(), "second sync issues: {:?}", r2.issues);
    assert_eq!(common::snapshot_files(d.path()), before, "dest untouched (content AND mtimes)");
}

// --- symlink updates and kind-swaps involving symlinks ---

#[cfg(unix)]
#[test]
fn retargeted_symlink_is_updated() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    common::file(s.path(), "a.txt", b"content");
    common::file(d.path(), "a.txt", b"content");
    std::os::unix::fs::symlink("a.txt", s.path().join("link")).unwrap(); // src: link -> a.txt
    std::os::unix::fs::symlink("elsewhere", d.path().join("link")).unwrap(); // dst: stale target

    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert_eq!(fs::read_link(d.path().join("link")).unwrap(), PathBuf::from("a.txt"));
}

#[cfg(unix)]
#[test]
fn kind_swap_symlink_to_file_is_resolved() {
    let (s, d) = dirs();
    if !common::symlinks_supported(d.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    common::file(s.path(), "x", b"now a real file"); // source: x is a FILE
    std::os::unix::fs::symlink("dangling", d.path().join("x")).unwrap(); // dest: x is a SYMLINK

    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    let md = fs::symlink_metadata(d.path().join("x")).unwrap();
    assert!(md.is_file(), "x should now be a regular file");
    assert_eq!(fs::read(d.path().join("x")).unwrap(), b"now a real file");
}

#[cfg(unix)]
#[test]
fn kind_swap_file_to_symlink_is_resolved() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    std::os::unix::fs::symlink("target-path", s.path().join("y")).unwrap(); // source: y is a SYMLINK
    common::file(d.path(), "y", b"old regular file"); // dest: y is a FILE

    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    let md = fs::symlink_metadata(d.path().join("y")).unwrap();
    assert!(md.is_symlink(), "y should now be a symlink");
    assert_eq!(fs::read_link(d.path().join("y")).unwrap(), PathBuf::from("target-path"));
}

// --- destination-side permission failure: reported, not fatal, rest still syncs ---

#[cfg(unix)]
#[test]
fn unwritable_destination_subdir_reports_issue_and_continues() {
    let (s, d) = dirs();
    if !common::permissions_enforced(d.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(s.path(), "ok.txt", b"fine");
    common::file(s.path(), "vault/blocked.txt", b"unreachable");
    common::dir(d.path(), "vault");
    common::set_no_perms(d.path(), "vault"); // destination dir exists but can't be written

    let r = sync_with(s.path(), d.path(), &default_opts());
    common::restore_perms(d.path(), "vault");

    assert!(d.path().join("ok.txt").is_file(), "unaffected file still copied");
    assert!(!d.path().join("vault/blocked.txt").exists());
    assert!(
        r.issues.iter().any(|i| i.contains("blocked.txt")),
        "expected an issue naming blocked.txt: {:?}",
        r.issues
    );
}

// --- special files (fifo/socket/device): nothing to copy → `skipped`, never an issue ---

#[cfg(unix)]
#[test]
fn special_file_goes_to_skipped_not_issues() {
    let (s, d) = dirs();
    common::file(s.path(), "normal.txt", b"data");
    if !common::make_fifo(s.path(), "pipe") {
        eprintln!("skipping: filesystem lacks fifo support");
        return;
    }
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(d.path().join("normal.txt").is_file(), "the regular file is still copied");
    assert!(!d.path().join("pipe").exists(), "the special file is not reproduced");
    assert!(
        r.issues.is_empty(),
        "a special file is not a failure — issues must stay empty: {:?}",
        r.issues
    );
    assert!(
        r.skipped.iter().any(|m| m.contains("pipe") && m.contains("special file")),
        "…but it must be listed under skipped: {:?}",
        r.skipped
    );
}

#[cfg(unix)]
#[test]
fn special_files_on_both_sides_are_unchanged() {
    // A fifo at the same path on both sides: nothing to compare, nothing to do — must not loop
    // through "changed → skip" forever.
    let (s, d) = dirs();
    if !common::make_fifo(s.path(), "pipe") || !common::make_fifo(d.path(), "pipe") {
        eprintln!("skipping: filesystem lacks fifo support");
        return;
    }
    common::file(s.path(), "data.txt", b"x");
    common::file(d.path(), "data.txt", b"x");
    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert!(r.skipped.is_empty(), "no action attempted ⇒ nothing skipped: {:?}", r.skipped);
    assert_eq!((r.copied, r.deleted), (0, 0));
}

// --- content-identical files with drifted mtime: refreshed, never re-copied ---

#[test]
fn mtime_drift_with_identical_content_refreshes_instead_of_copying() {
    let (s, d) = dirs();
    common::file(s.path(), "f.bin", b"IDENTICAL-BYTES");
    common::file(d.path(), "f.bin", b"IDENTICAL-BYTES");
    common::set_mtime(s.path(), "f.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000));
    common::set_mtime(d.path(), "f.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000));

    let r1 = sync_with(s.path(), d.path(), &default_opts());
    assert_eq!(r1.copied, 0, "identical bytes must not be re-copied");
    assert_eq!(r1.refreshed, 1, "…their metadata is aligned instead");
    assert!(r1.issues.is_empty(), "issues: {:?}", r1.issues);

    // and the alignment sticks: the next run is a complete no-op
    let r2 = sync_with(s.path(), d.path(), &default_opts());
    assert_eq!((r2.copied, r2.refreshed, r2.moved, r2.deleted), (0, 0, 0, 0));
}

// --- hard-link groups: content copied once, other names linked at the destination ---

#[cfg(unix)]
#[test]
fn hardlinked_pair_mirrors_as_one_inode() {
    use std::os::unix::fs::MetadataExt;
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) || !common::hardlinks_supported(d.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    common::file(s.path(), "a.txt", b"SHARED-CONTENT");
    common::hardlink(s.path(), "a.txt", "b.txt");

    let r1 = sync_with(s.path(), d.path(), &default_opts());
    assert!(r1.issues.is_empty(), "issues: {:?}", r1.issues);
    assert_eq!(r1.copied, 1, "content written exactly once");
    assert_eq!(r1.linked, 1, "the second name is a link, not a copy");
    assert_eq!(
        fs::metadata(d.path().join("a.txt")).unwrap().ino(),
        fs::metadata(d.path().join("b.txt")).unwrap().ino(),
        "one inode at the destination, like the source"
    );
    assert_eq!(fs::read(d.path().join("b.txt")).unwrap(), b"SHARED-CONTENT");

    // idempotence: correct linkage is recognized, nothing re-done
    let r2 = sync_with(s.path(), d.path(), &default_opts());
    assert_eq!((r2.copied, r2.linked, r2.moved, r2.deleted, r2.refreshed), (0, 0, 0, 0, 0));
    assert!(r2.issues.is_empty(), "issues: {:?}", r2.issues);
}

/// THE trust-condition test: when the source inode's content changes, the leader is re-copied
/// atomically (temp+rename ⇒ NEW destination inode) — every follower must be relinked in the SAME
/// run, or it keeps serving the old bytes through the old inode.
#[cfg(unix)]
#[test]
fn stale_followers_are_relinked_when_the_source_inode_updates() {
    use std::os::unix::fs::MetadataExt;
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) || !common::hardlinks_supported(d.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    common::file(s.path(), "a.txt", b"version one");
    common::hardlink(s.path(), "a.txt", "b.txt");
    let r1 = sync_with(s.path(), d.path(), &default_opts());
    assert!(r1.issues.is_empty());

    // edit through ONE source name — the shared inode means both names now carry the new bytes
    common::file(s.path(), "a.txt", b"version two, deliberately longer");

    let r2 = sync_with(s.path(), d.path(), &default_opts());
    assert!(r2.issues.is_empty(), "issues: {:?}", r2.issues);
    assert_eq!(r2.copied, 1, "leader re-copied once");
    assert_eq!(r2.linked, 1, "follower relinked to the leader's NEW inode");
    assert_eq!(
        fs::read(d.path().join("b.txt")).unwrap(),
        b"version two, deliberately longer",
        "the follower must never keep serving stale bytes through the old inode"
    );
    assert_eq!(
        fs::metadata(d.path().join("a.txt")).unwrap().ino(),
        fs::metadata(d.path().join("b.txt")).unwrap().ino()
    );
}

/// Pre-feature backups hold hard-linked sources as two independent files: converging them is a
/// pure metadata operation — no content is copied.
#[cfg(unix)]
#[test]
fn independent_destination_duplicates_converge_into_links() {
    use std::os::unix::fs::MetadataExt;
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) || !common::hardlinks_supported(d.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    common::file(s.path(), "a.txt", b"same bytes");
    common::hardlink(s.path(), "a.txt", "b.txt");
    common::set_mtime(s.path(), "a.txt", t);
    // destination: same content at both names, but two separate inodes (old-style backup)
    common::file(d.path(), "a.txt", b"same bytes");
    common::file(d.path(), "b.txt", b"same bytes");
    common::set_mtime(d.path(), "a.txt", t);
    common::set_mtime(d.path(), "b.txt", t);

    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert_eq!(r.copied, 0, "convergence must not write any content");
    assert_eq!(r.linked, 1);
    assert_eq!(
        fs::metadata(d.path().join("a.txt")).unwrap().ino(),
        fs::metadata(d.path().join("b.txt")).unwrap().ino(),
        "the duplicate collapsed into a link"
    );
}

// --- directory metadata is mirrored (quick diff never classifies dirs as changed) ---

#[cfg(unix)]
#[test]
fn directory_permissions_and_mtime_are_mirrored() {
    use std::os::unix::fs::PermissionsExt;
    let (s, d) = dirs();
    common::file(s.path(), "docs/readme.txt", b"content");
    fs::set_permissions(s.path().join("docs"), fs::Permissions::from_mode(0o750)).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_400_000_000);
    common::set_dir_mtime(s.path(), "docs", t);

    let r = sync_with(s.path(), d.path(), &default_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);

    let md = fs::metadata(d.path().join("docs")).unwrap();
    assert_eq!(md.permissions().mode() & 0o7777, 0o750, "dir permissions mirrored");
    let drift = md.modified().unwrap().duration_since(t).unwrap_or(Duration::ZERO);
    assert!(drift <= Duration::from_secs(2), "dir mtime mirrored (within tolerance): {drift:?}");
}

// --- --relative-symlinks: retargeting happens AT copy time, and the diff agrees with it ---

fn relative_opts() -> Options {
    Options { verify: true, fsync_each: false, backup_dir: None, relative_symlinks: true }
}

#[cfg(unix)]
#[test]
fn relative_symlinks_retarget_absolute_into_source() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    common::file(s.path(), "f1/b.txt", b"hello world");
    // an ABSOLUTE symlink pointing into the source — the case that isn't self-contained
    std::os::unix::fs::symlink(s.path().join("f1/b.txt"), s.path().join("abs")).unwrap();

    let r = sync_with(s.path(), d.path(), &relative_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);

    let target = fs::read_link(d.path().join("abs")).unwrap();
    assert!(target.is_relative(), "link should be rewritten relative, got {target:?}");
    assert!(!target.starts_with(s.path()), "link must not point back into the source");
    assert_eq!(fs::read(d.path().join("abs")).unwrap(), b"hello world", "resolves within the mirror");
}

/// THE churn regression: an absolute-internal link used to be re-copied every run (the old
/// post-sync relink left the destination holding a different target than the source's). The diff
/// now compares against the target a copy would write, so run 2 must be a no-op.
#[cfg(unix)]
#[test]
fn relative_symlinks_second_sync_is_a_noop() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    common::file(s.path(), "f1/b.txt", b"x");
    std::os::unix::fs::symlink(s.path().join("f1/b.txt"), s.path().join("abs")).unwrap();

    let r1 = sync_with(s.path(), d.path(), &relative_opts());
    assert!(r1.issues.is_empty(), "run 1 issues: {:?}", r1.issues);
    let r2 = sync_with(s.path(), d.path(), &relative_opts());
    assert_eq!((r2.copied, r2.moved, r2.deleted), (0, 0, 0), "run 2 must rewrite nothing");
    assert!(r2.issues.is_empty(), "run 2 issues: {:?}", r2.issues);
}

#[cfg(unix)]
#[test]
fn relative_symlinks_leave_internal_relative_links_unchanged() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    common::file(s.path(), "f1/b.txt", b"x");
    common::dir(s.path(), "links"); // symlink() won't create the parent
    std::os::unix::fs::symlink("../f1/b.txt", s.path().join("links/rel")).unwrap();

    let r = sync_with(s.path(), d.path(), &relative_opts());
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert_eq!(fs::read_link(d.path().join("links/rel")).unwrap(), Path::new("../f1/b.txt"));
}

#[cfg(unix)]
#[test]
fn relative_symlinks_keep_out_of_tree_verbatim_and_note_broken() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    let outside = tempfile::tempdir().unwrap();
    common::file(outside.path(), "ext.txt", b"external");
    std::os::unix::fs::symlink(outside.path().join("ext.txt"), s.path().join("out")).unwrap();
    std::os::unix::fs::symlink("does_not_exist", s.path().join("broken")).unwrap();

    let r = sync_with(s.path(), d.path(), &relative_opts());

    // out-of-tree link is left exactly as copied (still absolute, into `outside`)
    assert_eq!(fs::read_link(d.path().join("out")).unwrap(), outside.path().join("ext.txt"));
    // a dangling link is still copied (its relative form is unchanged here) and noted once
    assert_eq!(fs::read_link(d.path().join("broken")).unwrap(), Path::new("does_not_exist"));
    assert!(
        r.issues.iter().any(|i| i.contains("broken") && i.contains("does not exist")),
        "dangling symlink should be noted: {:?}",
        r.issues
    );
}

// --- moved files must not be re-copied by the NEXT run (mtime refresh after rename) ---

#[test]
fn moved_file_is_not_recopied_by_the_next_run() {
    let (s, d) = dirs();
    common::file(s.path(), "new/place.bin", b"IDENTICAL-PAYLOAD");
    common::file(d.path(), "old/place.bin", b"IDENTICAL-PAYLOAD");
    // mtimes differ by far more than the 2s tolerance — the pre-fix behavior would re-copy
    common::set_mtime(s.path(), "new/place.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000));
    common::set_mtime(d.path(), "old/place.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000));

    let r1 = sync_with(s.path(), d.path(), &default_opts());
    assert_eq!(r1.moved, 1, "content-identical relocation is a rename");
    assert!(r1.issues.is_empty(), "issues: {:?}", r1.issues);

    let r2 = sync_with(s.path(), d.path(), &default_opts());
    assert_eq!(
        (r2.copied, r2.moved, r2.deleted),
        (0, 0, 0),
        "the rename must refresh the mtime so run 2 sees the moved file as unchanged"
    );
}

// --- a move whose target path holds a doomed wrong-kind entry must succeed in ONE run ---

#[test]
fn move_onto_a_kind_swapped_path_succeeds_in_one_run() {
    let (s, d) = dirs();
    common::file(s.path(), "x", b"THE-PAYLOAD"); // source: x is a FILE...
    common::file(d.path(), "x/inner.txt", b"old"); // ...but at dest, x is currently a DIRECTORY
    common::file(d.path(), "elsewhere.bin", b"THE-PAYLOAD"); // and the content already exists here

    let r = sync_with(s.path(), d.path(), &default_opts());

    assert!(r.issues.is_empty(), "one run must be enough, without errors: {:?}", r.issues);
    assert_eq!(r.moved, 1, "satisfied by rename, not copy");
    assert_eq!(r.copied, 0, "no bytes copied — the whole point of the move");
    assert!(d.path().join("x").is_file());
    assert_eq!(fs::read(d.path().join("x")).unwrap(), b"THE-PAYLOAD");
    assert!(!d.path().join("elsewhere.bin").exists(), "move source relocated");
}

#[test]
fn move_onto_a_kind_swapped_path_backs_up_the_doomed_directory_contents() {
    let (s, d) = dirs();
    let backup = tempfile::tempdir().unwrap();
    common::file(s.path(), "x", b"THE-PAYLOAD");
    common::file(d.path(), "x/inner.txt", b"save me"); // doomed dir contents
    common::file(d.path(), "elsewhere.bin", b"THE-PAYLOAD");
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), relative_symlinks: false };

    let r = sync_with(s.path(), d.path(), &opts);

    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert_eq!(fs::read(d.path().join("x")).unwrap(), b"THE-PAYLOAD");
    assert_eq!(
        fs::read(backup.path().join("x/inner.txt")).unwrap(),
        b"save me",
        "the pre-deleted blocking entry goes through the normal backup-aside path"
    );
}
