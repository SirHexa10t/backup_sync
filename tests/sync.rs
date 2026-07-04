//! Sync (apply) invariants: DST == SRC (round-trip), source untouched, moves execute as renames
//! (inode preserved), mirror deletes, atomic overwrite, backup-dir, verify, and interrupt-safety.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use filesync::apply::{apply, verify_matches, Options};
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
