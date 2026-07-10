//! Destination filesystem capability probing.
//!
//! Removable-media filesystems differ in what they can store — notably exFAT/FAT can't hold
//! symlinks or unix permissions. `apply` already degrades gracefully per-file, but probing the
//! destination up front lets us warn *before* a long run (e.g. "these symlinks won't be kept").

use std::fs;
use std::path::Path;

use crate::manifest::DstRoot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Can the destination store symbolic links?
    pub symlinks: bool,
    /// Can the destination hold multiple names for one inode (hard links)?
    pub hardlinks: bool,
}

/// Probe the destination by trying (then cleaning up), rather than guessing from the FS type.
pub fn probe(dst: &DstRoot) -> Capabilities {
    Capabilities {
        symlinks: symlinks_supported(dst.path()),
        hardlinks: hardlinks_supported(dst.path()),
    }
}

/// Probe hard-link support: create a scratch file, try to give it a second name, clean up.
fn hardlinks_supported(dir: &Path) -> bool {
    let (a, b) = (dir.join(".filesync-probe-hl-a"), dir.join(".filesync-probe-hl-b"));
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    if fs::write(&a, b"probe").is_err() {
        return false;
    }
    let ok = fs::hard_link(&a, &b).is_ok();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    ok
}

#[cfg(unix)]
fn symlinks_supported(dir: &Path) -> bool {
    let probe = dir.join(".filesync-probe-symlink");
    let _ = fs::remove_file(&probe);
    let ok = std::os::unix::fs::symlink("probe-target", &probe).is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

#[cfg(not(unix))]
fn symlinks_supported(_dir: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_a_normal_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let caps = probe(&DstRoot::new(tmp.path()));
        // the test filesystem (tmpfs/ext4) supports both link kinds
        #[cfg(unix)]
        {
            assert!(caps.symlinks);
            assert!(caps.hardlinks);
        }
        let _ = caps;
        // and probing leaves nothing behind
        assert!(!tmp.path().join(".filesync-probe-symlink").exists());
        assert!(!tmp.path().join(".filesync-probe-hl-a").exists());
        assert!(!tmp.path().join(".filesync-probe-hl-b").exists());
    }
}
