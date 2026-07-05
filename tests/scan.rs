//! Scan behavior: nasty names, symlinks (recorded, not followed), hard-links, and that scanning
//! never modifies the tree.

mod common;

use std::path::{Path, PathBuf};

use filesync::manifest::{Kind, Manifest};
use filesync::scan::scan;

fn rels(m: &Manifest) -> Vec<String> {
    m.iter().map(|e| e.rel.to_string_lossy().into_owned()).collect()
}

#[test]
fn scans_files_dirs_and_nasty_names() {
    let tmp = tempfile::tempdir().unwrap();
    common::build_corpus(tmp.path());
    let m = scan(tmp.path());
    let r = rels(&m);

    for expected in [
        "f1/b.txt",
        "f2/ with  spaces ",
        "f2/with\nnewline",
        "f2/with\ttab",
        "f3/.hidden",
        "f3/inner/deep.txt",
        "empty_dir",
        "empty_file",
    ] {
        assert!(r.iter().any(|p| p == expected), "missing {expected:?} in {r:?}");
    }
    // the root itself is excluded
    assert!(!r.iter().any(|p| p.is_empty()));
}

#[cfg(unix)]
#[test]
fn symlinks_are_recorded_but_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    if !common::symlinks_supported(tmp.path()) {
        eprintln!("skipping: filesystem lacks symlink support");
        return;
    }
    common::build_corpus(tmp.path());
    let m = scan(tmp.path());

    let rel = m.iter().find(|e| e.rel == PathBuf::from("links/rel")).expect("relative symlink");
    assert_eq!(rel.kind, Kind::Symlink);
    assert_eq!(rel.link_target.as_deref(), Some(Path::new("../f1/b.txt")));

    // a broken symlink is still recorded (not followed → no error)
    assert!(m
        .iter()
        .any(|e| e.rel == PathBuf::from("links/broken") && e.kind == Kind::Symlink));

    // a symlink to a directory is a Symlink and is NOT descended into
    let to_dir = m.iter().find(|e| e.rel == PathBuf::from("links/to_dir")).unwrap();
    assert_eq!(to_dir.kind, Kind::Symlink);
    // nothing was descended *into* it (the symlink entry itself doesn't count)
    assert!(!m
        .iter()
        .any(|e| e.rel != Path::new("links/to_dir") && e.rel.starts_with("links/to_dir")));
}

#[test]
fn hardlinks_appear_as_two_regular_files_with_equal_content() {
    let tmp = tempfile::tempdir().unwrap();
    if !common::hardlinks_supported(tmp.path()) {
        eprintln!("skipping: filesystem lacks hard-link support");
        return;
    }
    common::build_corpus(tmp.path());
    let m = scan(tmp.path());

    let orig = m.iter().find(|e| e.rel == PathBuf::from("hl/original.txt")).unwrap();
    let link = m.iter().find(|e| e.rel == PathBuf::from("hl/linked.txt")).unwrap();
    assert_eq!(orig.kind, Kind::File);
    assert_eq!(link.kind, Kind::File);
    assert_eq!(orig.size, link.size);

    let ho = filesync::hash::hash_file(&tmp.path().join(&orig.rel)).unwrap();
    let hl = filesync::hash::hash_file(&tmp.path().join(&link.rel)).unwrap();
    assert_eq!(ho, hl);
}

#[test]
fn scan_ignores_filesync_temp_files() {
    let tmp = tempfile::tempdir().unwrap();
    common::file(tmp.path(), "real.txt", b"x");
    common::file(tmp.path(), ".filesync.tmp.123.foo", b"scratch"); // our own temp
    let r = rels(&scan(tmp.path()));
    assert!(r.iter().any(|p| p == "real.txt"));
    assert!(!r.iter().any(|p| p.starts_with(".filesync.tmp.")), "temp file must be ignored: {r:?}");
}

#[test]
fn scanning_does_not_modify_the_tree() {
    let tmp = tempfile::tempdir().unwrap();
    common::build_corpus(tmp.path());
    let before = common::snapshot_files(tmp.path());
    let _ = scan(tmp.path());
    let after = common::snapshot_files(tmp.path());
    assert_eq!(before, after);
}

#[cfg(unix)]
#[test]
fn scan_reports_an_unreadable_directory() {
    let tmp = tempfile::tempdir().unwrap();
    if !common::permissions_enforced(tmp.path()) {
        eprintln!("skipping: permission bits not enforced (running as root?)");
        return;
    }
    common::file(tmp.path(), "readable.txt", b"ok");
    common::file(tmp.path(), "locked/secret.txt", b"hidden");
    common::set_no_perms(tmp.path(), "locked"); // can't list the directory's contents

    let (m, errors) = filesync::scan::scan_with_errors(tmp.path());
    common::restore_perms(tmp.path(), "locked"); // let the tempdir clean itself up

    // readable siblings are still scanned
    assert!(rels(&m).iter().any(|p| p == "readable.txt"));
    // the unreadable directory produced a reported error naming it (not a silent omission)
    assert!(
        errors.iter().any(|e| e.contains("locked")),
        "expected a scan error mentioning 'locked': {errors:?}"
    );
    // and its hidden contents were NOT silently pulled in
    assert!(!rels(&m).iter().any(|p| p == "locked/secret.txt"));
}
