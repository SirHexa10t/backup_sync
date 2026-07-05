//! Shared test helpers: build corpora under a temp dir, probe filesystem capabilities, and
//! snapshot a tree for the source-untouched audit. Everything is built at runtime (not committed),
//! so git's lack of hard-link tracking is irrelevant, and link-dependent tests can skip on
//! filesystems (NTFS/FAT) that don't support them.
#![allow(dead_code)] // not every test uses every helper

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use filesync::manifest::Kind;

/// Create a file with `contents`, making parent dirs.
pub fn file(root: &Path, rel: &str, contents: &[u8]) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    File::create(&p).unwrap().write_all(contents).unwrap();
}

/// Create an (empty) directory.
pub fn dir(root: &Path, rel: &str) {
    fs::create_dir_all(root.join(rel)).unwrap();
}

/// Hard-link `new_rel` to the existing `existing_rel` (asserts success — probe first with
/// [`hardlinks_supported`] if the filesystem might not allow it).
pub fn hardlink(root: &Path, existing_rel: &str, new_rel: &str) {
    let np = root.join(new_rel);
    if let Some(parent) = np.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::hard_link(root.join(existing_rel), np).unwrap();
}

/// Force a file's modified-time (so "unchanged" and eager-checksum tests are deterministic).
pub fn set_mtime(root: &Path, rel: &str, t: SystemTime) {
    File::options().write(true).open(root.join(rel)).unwrap().set_modified(t).unwrap();
}

/// Does this filesystem support hard links? (Probes by trying, then cleans up.)
pub fn hardlinks_supported(dir: &Path) -> bool {
    let (a, b) = (dir.join(".probe_hl_a"), dir.join(".probe_hl_b"));
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    if File::create(&a).is_err() {
        return false;
    }
    let ok = fs::hard_link(&a, &b).is_ok();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    ok
}

/// Does this filesystem support symlinks?
#[cfg(unix)]
pub fn symlinks_supported(dir: &Path) -> bool {
    let l = dir.join(".probe_symlink");
    let _ = fs::remove_file(&l);
    let ok = std::os::unix::fs::symlink("target", &l).is_ok();
    let _ = fs::remove_file(&l);
    ok
}
#[cfg(not(unix))]
pub fn symlinks_supported(_dir: &Path) -> bool {
    false
}

/// A comprehensive corpus: nasty names, nested dirs, empty file/dir, duplicate content, and —
/// best-effort (skipped where unsupported) — a hard-link and relative/broken/to-dir symlinks.
pub fn build_corpus(root: &Path) {
    file(root, "empty_file", b"");
    dir(root, "empty_dir");
    file(root, "f1/a.txt", b"");
    file(root, "f1/b.txt", b"hello world");
    file(root, "f2/a.txt", b"another a");
    file(root, "f2/ with  spaces ", b"spaces");
    file(root, "f2/unicode_\u{30cf}\u{30f3}\u{30d0}\u{30fc}\u{30ac}\u{30fc}_\u{1f363}", b"unicode");
    file(root, "f2/with\nnewline", b"newline");
    file(root, "f2/with\ttab", b"tab");
    file(root, "f3/.hidden", b"hidden");
    file(root, "f3/inner/deep.txt", b"deep");
    file(root, "dup/one.txt", b"DUPLICATE-CONTENT");
    file(root, "dup/two.txt", b"DUPLICATE-CONTENT");

    // hard-link: two paths, one inode (best-effort)
    file(root, "hl/original.txt", b"hard-linked content");
    let _ = fs::hard_link(root.join("hl/original.txt"), root.join("hl/linked.txt"));

    // symlinks (unix, best-effort)
    #[cfg(unix)]
    {
        dir(root, "links");
        let _ = std::os::unix::fs::symlink("../f1/b.txt", root.join("links/rel"));
        let _ = std::os::unix::fs::symlink("nonexistent", root.join("links/broken"));
        let _ = std::os::unix::fs::symlink("../f1", root.join("links/to_dir"));
    }
}

/// (rel path) -> (len, mtime, blake3 bytes) for every regular file under `root`. Used to prove an
/// operation didn't modify a tree (the source-untouched audit).
pub fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>, [u8; 32])> {
    let mut out = BTreeMap::new();
    for e in filesync::scan::scan(root).iter() {
        if e.kind == Kind::File {
            let h = filesync::hash::hash_file(&root.join(&e.rel)).unwrap();
            out.insert(e.rel.clone(), (e.size, e.mtime, *h.as_bytes()));
        }
    }
    out
}

/// Strip all permissions (0o000) from an existing path — used to simulate an unreadable file or
/// directory. Restore with [`restore_perms`] before the tempdir is dropped, or its cleanup leaks.
#[cfg(unix)]
pub fn set_no_perms(root: &Path, rel: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o000)).unwrap();
}

/// Restore owner rwx (0o755) so a previously locked file/dir can be cleaned up. Unix only.
#[cfg(unix)]
pub fn restore_perms(root: &Path, rel: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o755));
}

/// Whether 0-permission entries actually block access here. Returns false when running as root
/// (which bypasses permission bits), so permission tests can skip instead of spuriously failing.
/// Probes by creating a locked file under `dir` and checking whether it can still be opened.
#[cfg(unix)]
pub fn permissions_enforced(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(".probe_perms");
    let _ = fs::remove_file(&p);
    if File::create(&p).is_err() {
        return false;
    }
    fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = File::open(&p).is_err();
    let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o644));
    let _ = fs::remove_file(&p);
    blocked
}
#[cfg(not(unix))]
pub fn permissions_enforced(_dir: &Path) -> bool {
    false
}

/// Create a FIFO (named pipe) at `rel` — a `Kind::Other` special file. Returns false if the
/// filesystem doesn't support it (skip the test then). Unix only.
#[cfg(unix)]
pub fn make_fifo(root: &Path, rel: &str) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    match CString::new(p.as_os_str().as_bytes()) {
        Ok(c) => unsafe { libc::mkfifo(c.as_ptr(), 0o644) == 0 },
        Err(_) => false,
    }
}
