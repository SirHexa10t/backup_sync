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
    run_diff_flag(src, dst, eager, false)
}

fn run_diff_flag(src: &Path, dst: &Path, eager: bool, relative_symlinks: bool) -> Diff {
    let (s, d) = (SrcRoot::new(src), DstRoot::new(dst));
    let (sm, dm) = (scan(src), scan(dst));
    diff(&s, &sm, &d, &dm, eager, relative_symlinks, false)
}

fn run_diff_same(src: &Path, dst: &Path) -> Diff {
    let (s, d) = (SrcRoot::new(src), DstRoot::new(dst));
    let (sm, dm) = (scan(src), scan(dst));
    diff(&s, &sm, &d, &dm, false, false, true)
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
fn include_same_collects_and_lists_identical_files_only_on_demand() {
    let (s, d) = dirs();
    common::file(s.path(), "same.txt", b"identical");
    common::file(d.path(), "same.txt", b"identical");
    common::file(s.path(), "changed.txt", b"new"); // different size → a real change
    common::file(d.path(), "changed.txt", b"old-and-longer");

    // default: identical files are counted, never collected or rendered
    let plain = run_diff(s.path(), d.path(), false);
    assert_eq!(plain.unchanged, 1);
    assert!(plain.unchanged_paths.is_empty(), "default must not collect the identical list");
    assert!(!plain.render(true).contains("= same.txt"), "…and must not render it");

    // --include-same: the identical file is collected and appears in the detailed findings only
    let full = run_diff_same(s.path(), d.path());
    assert_eq!(full.unchanged, 1);
    assert_eq!(full.unchanged_paths, vec![PathBuf::from("same.txt")]);
    assert!(full.render(true).contains("= same.txt"), "detail must list it");
    assert!(!full.render(false).contains("= same.txt"), "the terminal summary never lists it");
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
fn empty_files_pair_as_a_move() {
    // Two zero-byte files hash identically, so an empty file that changed path is a rename —
    // pinned: equivalent cost to create+delete, and it keeps the "moved" report truthful.
    let (s, d) = dirs();
    common::file(s.path(), "empty_new", b"");
    common::file(d.path(), "empty_old", b"");
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(
        r.moved,
        vec![Move { from: PathBuf::from("empty_old"), to: PathBuf::from("empty_new") }]
    );
    assert!(r.added.is_empty() && r.removed.is_empty());
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
fn hardlinked_source_files_become_one_copy_plus_a_link() {
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    common::file(s.path(), "x.txt", b"linked");
    common::hardlink(s.path(), "x.txt", "y.txt"); // two names, one inode
    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.added_paths(), vec![PathBuf::from("x.txt")], "only the leader is copied");
    assert_eq!(
        r.to_link,
        vec![filesync::diff::Link { leader: PathBuf::from("x.txt"), name: PathBuf::from("y.txt") }],
        "the follower is realized as a hard link — content written once"
    );
}

#[test]
fn correctly_linked_destination_pair_is_all_unchanged() {
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) || !common::hardlinks_supported(d.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    common::file(s.path(), "x.txt", b"linked");
    common::hardlink(s.path(), "x.txt", "y.txt");
    common::set_mtime(s.path(), "x.txt", t); // shared inode ⇒ covers y.txt too
    common::file(d.path(), "x.txt", b"linked");
    common::hardlink(d.path(), "x.txt", "y.txt");
    common::set_mtime(d.path(), "x.txt", t);

    let r = run_diff(s.path(), d.path(), false);
    assert!(r.to_link.is_empty(), "correct linkage must not be re-done: {:?}", r.to_link);
    assert!(r.added.is_empty() && r.changed.is_empty());
    assert_eq!(r.unchanged, 2, "both names count as unchanged");
}

#[test]
fn follower_is_relinked_when_the_leader_will_be_rewritten() {
    // THE stale-inode trap: a re-copied leader lands via temp+rename ⇒ NEW destination inode.
    // The follower's existing (correct-looking) link would keep serving the OLD bytes — so a
    // leader rewrite must force a follower relink, even though the dst linkage looks fine.
    let (s, d) = dirs();
    if !common::hardlinks_supported(s.path()) || !common::hardlinks_supported(d.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    common::file(s.path(), "x.txt", b"NEW CONTENT, LONGER"); // source inode updated
    common::hardlink(s.path(), "x.txt", "y.txt");
    common::file(d.path(), "x.txt", b"old content"); // dst pair properly linked, but stale
    common::hardlink(d.path(), "x.txt", "y.txt");

    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.changed_paths(), vec![PathBuf::from("x.txt")], "leader is re-copied");
    assert_eq!(r.to_link.len(), 1, "…which invalidates the follower's link");
    assert_eq!(r.to_link[0].name, PathBuf::from("y.txt"));
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

// --- degradation: one unreadable file must never abort the whole diff ---

#[cfg(unix)]
#[test]
fn unreadable_move_candidate_degrades_to_copy_plus_delete() {
    let (s, d) = dirs();
    if !common::permissions_enforced(s.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(s.path(), "locked.bin", b"AAAA"); // same size as the dst extra → move candidate
    common::file(d.path(), "extra.bin", b"BBBB");
    common::file(s.path(), "normal.txt", b"fine"); // the rest of the diff must still happen
    common::set_no_perms(s.path(), "locked.bin");

    let r = run_diff(s.path(), d.path(), false);
    common::restore_perms(s.path(), "locked.bin");

    assert!(r.moved.is_empty(), "an unhashable candidate must not become a move");
    assert!(r.added_paths().contains(&PathBuf::from("locked.bin")), "falls back to a plain copy");
    assert!(r.removed_paths().contains(&PathBuf::from("extra.bin")), "…and a plain delete");
    assert!(r.added_paths().contains(&PathBuf::from("normal.txt")), "the run continues");
    // Fix A: the issue names the SIDE (source), not just the path.
    assert!(
        r.issues.iter().any(|i| i.contains("locked.bin") && i.contains("source:")),
        "the degradation is reported and labeled source: {:?}",
        r.issues
    );
    // Fix B: an unreadable SOURCE file marks the view incomplete → the caller will suspend deletes.
    assert!(r.source_unreadable, "an unreadable source move-candidate must flag the incomplete view");
}

/// A destination-side unreadable candidate is reported and labeled — but must NOT set
/// `source_unreadable`: a destination read gap can't cause a wrong deletion, so deletes proceed.
#[cfg(unix)]
#[test]
fn unreadable_destination_candidate_is_labeled_but_does_not_flag_the_source() {
    let (s, d) = dirs();
    if !common::permissions_enforced(d.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(s.path(), "add.bin", b"AAAA"); // readable source add
    common::file(d.path(), "locked_extra.bin", b"BBBB"); // same size ⇒ candidate; unreadable
    common::set_no_perms(d.path(), "locked_extra.bin");

    let r = run_diff(s.path(), d.path(), false);
    common::restore_perms(d.path(), "locked_extra.bin");

    assert!(
        r.issues.iter().any(|i| i.contains("locked_extra.bin") && i.contains("destination:")),
        "labeled destination: {:?}",
        r.issues
    );
    assert!(!r.source_unreadable, "a destination read failure must not suspend deletes");
    assert!(r.removed_paths().contains(&PathBuf::from("locked_extra.bin")));
}

#[cfg(unix)]
#[test]
fn eager_same_size_unreadable_degrades_to_changed() {
    let (s, d) = dirs();
    if !common::permissions_enforced(s.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(s.path(), "f.bin", b"AAAA");
    common::file(d.path(), "f.bin", b"BBBB"); // same size → eager must hash → hash fails
    common::set_no_perms(s.path(), "f.bin");

    let r = run_diff(s.path(), d.path(), true);
    common::restore_perms(s.path(), "f.bin");

    assert_eq!(r.changed_paths(), vec![PathBuf::from("f.bin")], "degrades to changed, not abort");
    assert!(r.issues.iter().any(|i| i.contains("f.bin")), "and is reported: {:?}", r.issues);
}

#[cfg(unix)]
#[test]
fn eager_skips_hashing_when_sizes_already_differ() {
    // Different sizes prove "changed" without reading a byte — so even an UNREADABLE file
    // produces no hash error here. (Also the perf guarantee: no wasted double read.)
    let (s, d) = dirs();
    if !common::permissions_enforced(s.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(s.path(), "f.bin", b"longer-content");
    common::file(d.path(), "f.bin", b"short");
    common::set_no_perms(s.path(), "f.bin");

    let r = run_diff(s.path(), d.path(), true);
    common::restore_perms(s.path(), "f.bin");

    assert_eq!(r.changed_paths(), vec![PathBuf::from("f.bin")]);
    assert!(r.issues.is_empty(), "no hash attempted ⇒ no issue: {:?}", r.issues);
}

// --- the deep pre-overwrite check: mtime drift alone must not destroy a destination version ---

#[test]
fn same_content_with_drifted_mtime_is_touched_not_changed() {
    let (s, d) = dirs();
    common::file(s.path(), "f.bin", b"SAME-BYTES");
    common::file(d.path(), "f.bin", b"SAME-BYTES");
    common::set_mtime(s.path(), "f.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000));
    common::set_mtime(d.path(), "f.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000));

    let r = run_diff(s.path(), d.path(), false);
    assert!(r.changed.is_empty(), "hash-identical file must not be overwritten: {:?}", r.changed);
    assert_eq!(r.touched.len(), 1, "…it gets a metadata refresh instead");
    assert_eq!(r.touched[0].rel, PathBuf::from("f.bin"));
}

#[test]
fn drifted_mtime_with_different_content_is_still_changed() {
    let (s, d) = dirs();
    common::file(s.path(), "f.bin", b"AAAA");
    common::file(d.path(), "f.bin", b"BBBB"); // same size, real difference
    common::set_mtime(s.path(), "f.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000));
    common::set_mtime(d.path(), "f.bin", SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000));

    let r = run_diff(s.path(), d.path(), false);
    assert_eq!(r.changed_paths(), vec![PathBuf::from("f.bin")]);
    assert!(r.touched.is_empty());
}

#[test]
fn eager_flags_the_corruption_signature() {
    // Content differs although size AND mtime match — an mtime-preserving edit, or bit-rot on one
    // side. Only --eager-checksum can see it; it must be re-copied AND called out.
    let (s, d) = dirs();
    common::file(s.path(), "f.bin", b"AAAA");
    common::file(d.path(), "f.bin", b"BBBB");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    common::set_mtime(s.path(), "f.bin", t);
    common::set_mtime(d.path(), "f.bin", t);

    let r = run_diff(s.path(), d.path(), true);
    assert_eq!(r.changed_paths(), vec![PathBuf::from("f.bin")]);
    assert!(
        r.issues.iter().any(|i| i.contains("corruption")),
        "the suspicious signature must be called out: {:?}",
        r.issues
    );
}

// --- --relative-symlinks: the diff compares against the target a copy WOULD write ---

#[cfg(unix)]
#[test]
fn relative_symlinks_diff_treats_rewritten_link_as_unchanged() {
    let (s, d) = dirs();
    if !common::symlinks_supported(s.path()) {
        eprintln!("skipping: no symlink support");
        return;
    }
    common::file(s.path(), "f1/b.txt", b"x");
    common::file(d.path(), "f1/b.txt", b"x");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    common::set_mtime(s.path(), "f1/b.txt", t);
    common::set_mtime(d.path(), "f1/b.txt", t);
    // source: absolute internal link; destination: the rewritten relative form
    std::os::unix::fs::symlink(s.path().join("f1/b.txt"), s.path().join("abs")).unwrap();
    std::os::unix::fs::symlink("f1/b.txt", d.path().join("abs")).unwrap();

    let with_flag = run_diff_flag(s.path(), d.path(), false, true);
    assert!(with_flag.changed.is_empty(), "rewritten link must be unchanged: {:?}", with_flag.changed);
    assert_eq!(with_flag.unchanged, 3, "file + dir + link all unchanged");

    let without_flag = run_diff_flag(s.path(), d.path(), false, false);
    assert_eq!(
        without_flag.changed_paths(),
        vec![PathBuf::from("abs")],
        "verbatim mode still sees the textual difference"
    );
}
