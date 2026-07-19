//! Filesystem/device facts: which device a path lives on, whether two paths share one, and how
//! much space is free. Consumed by preflight validation (same-filesystem rule for `--backup-dir`),
//! the parallel-scan decision (different devices → independent I/O paths), and the suspended-
//! deletions space look-ahead. Everything degrades safely off-unix (no device introspection).

use std::fs;
use std::path::Path;

/// Free bytes available to unprivileged writes on the filesystem holding `path` (unix `statvfs`);
/// `None` when it can't be determined (then the caller proceeds without a space check).
#[cfg(unix)]
pub(crate) fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub(crate) fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether `a` and `b` live on the same filesystem (device). Off-unix, device introspection isn't
/// portable, so the check is skipped (returns `true`).
#[cfg(unix)]
pub(crate) fn same_filesystem(a: &Path, b: &Path) -> std::io::Result<bool> {
    Ok(fs_device(a)? == fs_device(b)?)
}

#[cfg(not(unix))]
pub(crate) fn same_filesystem(_a: &Path, _b: &Path) -> std::io::Result<bool> {
    Ok(true)
}

/// Whether `a` and `b` sit on **different** devices — the cue that scanning or hashing them
/// concurrently overlaps independent I/O instead of contending for one disk head. Unknown or
/// off-unix → `false` (stay sequential; never risk thrashing a single spindle). Caveat: two
/// partitions of one physical disk look "different" here — the real win is separate drives.
#[cfg(unix)]
pub(crate) fn different_devices(a: &Path, b: &Path) -> bool {
    matches!((fs_device(a), fs_device(b)), (Ok(da), Ok(db)) if da != db)
}

#[cfg(not(unix))]
pub(crate) fn different_devices(_a: &Path, _b: &Path) -> bool {
    false
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn different_devices_is_false_within_one_filesystem() {
        // Two directories on the same filesystem must NOT be judged different devices — so paired
        // scans stay sequential rather than thrashing one disk head.
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert!(!different_devices(&a, &b));
    }

    #[cfg(unix)]
    #[test]
    fn same_filesystem_judges_missing_paths_by_their_ancestor() {
        let t = tempfile::tempdir().unwrap();
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        // backup dir doesn't exist yet → judged by its nearest existing ancestor (the tempdir)
        assert!(same_filesystem(&t.path().join("backup"), &dst).unwrap());
    }
}
