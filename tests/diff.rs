//! Diff scenarios: missing (added), excessive (removed), moved (incl. duplicate content),
//! changed, unchanged, the --eager-checksum feature, and hard-link handling.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use filesync::diff::{diff, Diff, Move};
use filesync::manifest::{DstRoot, SrcRoot};
use filesync::scan::scan;

fn run_diff(src: &Path, dst: &Path, eager: bool) -> Diff {
    run_diff_jobs(src, dst, eager, 1)
}

fn run_diff_jobs(src: &Path, dst: &Path, eager: bool, jobs: usize) -> Diff {
    let (s, d) = (SrcRoot::new(src), DstRoot::new(dst));
    let (sm, dm) = (scan(src), scan(dst));
    diff(&s, &sm, &d, &dm, eager, jobs).unwrap()
}

fn dirs() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

#[test]
fn missing_file_is_added() {
    let (s, d) = dirs();
    common::file(s.path(), "a.txt", b"hi");
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.added_paths(), vec![PathBuf::from("a.txt")]);
    assert!(r.removed.is_empty() && r.changed.is_empty() && r.moved.is_empty());
}

#[test]
fn excessive_file_is_removed() {
    let (s, d) = dirs();
    common::file(d.path(), "a.txt", b"hi");
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.removed_paths(), vec![PathBuf::from("a.txt")]);
    assert!(r.added.is_empty() && r.moved.is_empty());
}

#[test]
fn moved_file_is_detected_as_a_move() {
    let (s, d) = dirs();
    common::file(s.path(), "b.txt", b"hello"); // new path in source
    common::file(d.path(), "a.txt", b"hello"); // old path at dest, identical content
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.moved, vec![Move { from: PathBuf::from("a.txt"), to: PathBuf::from("b.txt") }]);
    assert!(r.added.is_empty() && r.removed.is_empty());
}

#[test]
fn same_size_but_different_content_is_not_a_move() {
    let (s, d) = dirs();
    common::file(s.path(), "b.txt", b"AAAA");
    common::file(d.path(), "a.txt", b"BBBB"); // same size, different content
    let r = run_diff(s.path(), d.path(), false);
    assert!(r.moved.is_empty());
    assert_eq!(r.added_paths(), vec![PathBuf::from("b.txt")]);
    assert_eq!(r.removed_paths(), vec![PathBuf::from("a.txt")]);
}

#[test]
fn identical_file_is_unchanged() {
    let (s, d) = dirs();
    common::file(s.path(), "a.txt", b"same");
    common::file(d.path(), "a.txt", b"same");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    common::set_mtime(s.path(), "a.txt", t);
    common::set_mtime(d.path(), "a.txt", t);
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.unchanged, 1);
    assert!(r.changed.is_empty() && r.added.is_empty() && r.removed.is_empty());
}

#[test]
fn size_change_is_detected_in_default_mode() {
    let (s, d) = dirs();
    common::file(s.path(), "a.txt", b"a much longer content");
    common::file(d.path(), "a.txt", b"short");
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.changed_paths(), vec![PathBuf::from("a.txt")]);
}

// --- duplicate content exercises the hash→queue matching ---

#[test]
fn duplicate_content_pairs_each_move_once() {
    let (s, d) = dirs();
    common::file(s.path(), "p1.txt", b"SAME");
    common::file(s.path(), "p2.txt", b"SAME");
    common::file(d.path(), "q1.txt", b"SAME");
    common::file(d.path(), "q2.txt", b"SAME");
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.moved.len(), 2);
    assert!(r.added.is_empty() && r.removed.is_empty());
    let tos: HashSet<PathBuf> = r.moved.iter().map(|m| m.to.clone()).collect();
    let froms: HashSet<PathBuf> = r.moved.iter().map(|m| m.from.clone()).collect();
    assert_eq!(tos, HashSet::from([PathBuf::from("p1.txt"), PathBuf::from("p2.txt")]));
    assert_eq!(froms, HashSet::from([PathBuf::from("q1.txt"), PathBuf::from("q2.txt")]));
}

#[test]
fn more_adds_than_matching_removes_yields_move_plus_copy() {
    let (s, d) = dirs();
    common::file(s.path(), "p1.txt", b"SAME");
    common::file(s.path(), "p2.txt", b"SAME"); // two identical adds...
    common::file(d.path(), "q.txt", b"SAME"); // ...but only one matching remove
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.moved.len(), 1, "one of the two becomes a move");
    assert_eq!(r.moved[0].from, PathBuf::from("q.txt"));
    assert_eq!(r.added.len(), 1, "the other becomes a copy");
    assert!(r.removed.is_empty());
}

// --- --eager-checksum feature ---

#[test]
fn eager_checksum_catches_same_size_same_mtime_change() {
    let (s, d) = dirs();
    common::file(s.path(), "a.txt", b"AAAA");
    common::file(d.path(), "a.txt", b"BBBB"); // same size, different content
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    common::set_mtime(s.path(), "a.txt", t);
    common::set_mtime(d.path(), "a.txt", t); // ...and the same mtime

    let quick = run_diff(s.path(), d.path(), false);
    assert_eq!(quick.unchanged, 1);
    assert!(quick.changed.is_empty());

    let eager = run_diff(s.path(), d.path(), true);
    assert_eq!(eager.changed_paths(), vec![PathBuf::from("a.txt")]);
    assert_eq!(eager.unchanged, 0);
}

// --- hard-links ---

#[test]
fn hardlinked_source_files_are_both_copied() {
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    common::file(s.path(), "x.txt", b"linked");
    common::hardlink(s.path(), "x.txt", "y.txt"); // two paths, identical content
    let r = run_diff(s.path(), d.path(), false);
    let mut added = r.added_paths();
    added.sort();
    assert_eq!(added, vec![PathBuf::from("x.txt"), PathBuf::from("y.txt")]);
    // NOTE: the hard-link relationship is not preserved (both are planned as copies) — documented
    // v1 behavior; a future flag could re-link identical files at the destination.
}

// --- kind swap (file↔dir at the same path) is a delete + add, never an in-place change ---

#[test]
fn kind_swap_is_delete_plus_add_not_change() {
    let (s, d) = dirs();
    common::file(d.path(), "x", b"file"); // dest: x is a file
    common::file(s.path(), "x/inner.txt", b"y"); // src: x is a directory
    let r = run_diff(s.path(), d.path(), false);
    assert!(r.changed.is_empty(), "a kind-swap must not be classified as a change");
    let added = r.added_paths();
    assert!(added.contains(&PathBuf::from("x")), "src dir x is added");
    assert!(added.contains(&PathBuf::from("x/inner.txt")), "its contents are added");
    assert_eq!(r.removed_paths(), vec![PathBuf::from("x")], "dest file x is removed");
}

#[test]
fn move_detection_is_correct_with_multiple_jobs() {
    let (s, d) = dirs();
    // 20 relocations (same content, different path) — sizes deliberately collide across items so
    // the parallel hashing has to disambiguate within size buckets.
    for i in 0..20 {
        let content = format!("payload-{i}");
        common::file(s.path(), &format!("s{i}.bin"), content.as_bytes());
        common::file(d.path(), &format!("d{i}.bin"), content.as_bytes());
    }
    let r = run_diff_jobs(s.path(), d.path(), false, 4);
    assert_eq!(r.moved.len(), 20, "all 20 detected as moves with jobs=4");
    assert!(r.added.is_empty() && r.removed.is_empty());
    // parallel result matches the sequential one
    assert_eq!(run_diff_jobs(s.path(), d.path(), false, 1).moved.len(), 20);
}
