//! Sync (apply) invariants: DST == SRC (round-trip), source untouched, moves execute as renames
//! (inode preserved), mirror deletes, atomic overwrite, backup-dir, verify, and interrupt-safety.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use filesync::apply::{apply, relink_internal_symlinks, verify_matches, Options};
use filesync::diff::diff;
use filesync::manifest::{DstRoot, Kind, SrcRoot};
use filesync::plan::{plan, Action};
use filesync::report::Report;
use filesync::scan::scan;

fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

fn default_opts() -> Options {
    Options { verify: true, fsync_each: false, backup_dir: None, jobs: 1 }
}

/// Full pipeline: scan → diff → plan → apply.
fn sync_with(src: &Path, dst: &Path, opts: &Options) -> Report {
    let (s, d) = (SrcRoot::new(src), DstRoot::new(dst));
    let (sm, dm) = (scan(src), scan(dst));
    let df = diff(&s, &sm, &d, &dm, false, 1).unwrap();
    let actions = plan(&df);
    let mut r = Report::new();
    apply(&s, &d, &actions, opts, &mut r);
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
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), jobs: 1 };
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
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), jobs: 1 };
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
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), jobs: 1 };
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
    let opts = Options { verify: true, fsync_each: true, backup_dir: None, jobs: 1 };
    let r = sync_with(s.path(), d.path(), &opts);
    assert!(r.issues.is_empty());
    assert_eq!(content_map(s.path()), content_map(d.path()));
}

#[test]
fn no_temp_files_remain_after_sync() {
    let (s, d) = dirs();
    common::build_corpus(s.path());
    sync_with(s.path(), d.path(), &default_opts());
    // a sweep afterwards finds nothing to remove ⇒ atomic copies left no temp files
    assert_eq!(filesync::apply::sweep_temp_files(&DstRoot::new(d.path())), 0);
}

#[test]
fn sweep_removes_leftover_temp_files_only() {
    let (_s, d) = dirs();
    common::file(d.path(), "keep.txt", b"k");
    common::file(d.path(), ".filesync.tmp.999.old", b"junk");
    common::file(d.path(), "sub/.filesync.tmp.999.old2", b"junk");
    let removed = filesync::apply::sweep_temp_files(&DstRoot::new(d.path()));
    assert_eq!(removed, 2);
    assert!(d.path().join("keep.txt").is_file());
    assert!(!d.path().join(".filesync.tmp.999.old").exists());
    assert!(!d.path().join("sub/.filesync.tmp.999.old2").exists());
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
    apply(&SrcRoot::new(s.path()), &DstRoot::new(d.path()), &actions, &default_opts(), &mut r);
    assert_eq!(r.issues.len(), 1);
    assert!(!d.path().join("ghost.txt").exists());
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
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), jobs: 1 };
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
    let opts = Options { verify: true, fsync_each: false, backup_dir: Some(backup.path().to_path_buf()), jobs: 1 };
    let r = sync_with(s.path(), d.path(), &opts);
    assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    assert!(d.path().join("y").is_file());
    assert_eq!(fs::read(d.path().join("y")).unwrap(), b"now a file");
    // the file that lived under the old directory was preserved in backup
    assert_eq!(fs::read(backup.path().join("y/old.txt")).unwrap(), b"old data");
}

// --- special files (fifo/socket/device) can't be copied → skipped and reported ---

#[cfg(unix)]
#[test]
fn special_file_is_skipped_and_reported() {
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
        r.issues.iter().any(|i| i.contains("pipe") && i.contains("unsupported")),
        "expected a skip issue for the fifo: {:?}",
        r.issues
    );
}

// --- --relative-symlinks post-stage ---

/// Sync verbatim, then run the relative-symlinks post-stage in isolation, returning its report.
#[cfg(unix)]
fn relink_after_sync(s: &Path, d: &Path) -> Report {
    let r0 = sync_with(s, d, &default_opts());
    assert!(r0.issues.is_empty(), "setup sync had issues: {:?}", r0.issues);
    let mut r = Report::new();
    relink_internal_symlinks(&SrcRoot::new(s), &DstRoot::new(d), &scan(s), &mut r);
    r
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

    relink_after_sync(s.path(), d.path());

    let target = fs::read_link(d.path().join("abs")).unwrap();
    assert!(target.is_relative(), "link should be rewritten relative, got {target:?}");
    assert!(!target.starts_with(s.path()), "link must not point back into the source");
    assert_eq!(fs::read(d.path().join("abs")).unwrap(), b"hello world", "resolves within the mirror");
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

    relink_after_sync(s.path(), d.path());

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

    let r = relink_after_sync(s.path(), d.path());

    // out-of-tree link is left exactly as copied (still absolute, into `outside`)
    assert_eq!(fs::read_link(d.path().join("out")).unwrap(), outside.path().join("ext.txt"));
    // broken link is left verbatim and reported
    assert_eq!(fs::read_link(d.path().join("broken")).unwrap(), Path::new("does_not_exist"));
    assert!(
        r.issues.iter().any(|i| i.contains("broken")),
        "broken symlink should be noted: {:?}",
        r.issues
    );
}
