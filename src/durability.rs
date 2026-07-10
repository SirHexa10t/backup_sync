//! The end-of-run durability barrier — and the home for its platform differences, so
//! platform-specific flushing stays out of the main pipeline.
//!
//! On **Linux** one `syncfs` flushes the destination filesystem's data *and* metadata (including
//! the directory entries behind atomic renames and deletes) in a single call — far cheaper than
//! fsync-per-file for many small files (measured ~1.5×; see `docs/theory.md`).
//!
//! Elsewhere the portable path fsyncs every copied file, then fsyncs each directory an action
//! touched, persisting renames/deletes too. On **Windows** directories can't be opened for
//! syncing through std, so only file contents are flushed — which is why `run_sync` refuses to
//! run there without `--fsync-each`. (Lifting that would take a raw Win32 directory handle via
//! `FILE_FLAG_BACKUP_SEMANTICS` + `FlushFileBuffers`, i.e. a `windows-sys` dependency.)

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::manifest::DstRoot;
use crate::plan::Action;

/// Make the finished run durable. `copied` are the destination-relative paths of copied files.
pub fn flush_destination(dst: &DstRoot, actions: &[Action], copied: &[PathBuf]) {
    #[cfg(target_os = "linux")]
    {
        if syncfs(dst.path()) {
            return;
        }
        // fall through to the portable path if syncfs is unavailable/failed
    }
    portable_flush(dst, actions, copied);
}

/// One whole-filesystem flush. Returns false on failure so the caller can fall back.
#[cfg(target_os = "linux")]
fn syncfs(dst: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    match File::open(dst) {
        Ok(dir) => unsafe { libc::syncfs(dir.as_raw_fd()) == 0 },
        Err(_) => false,
    }
}

/// Portable fallback: fsync copied file contents, then fsync every directory an action touched
/// (rename sources/targets, deletions, creations, copy parents) so the metadata survives too.
fn portable_flush(dst: &DstRoot, actions: &[Action], copied: &[PathBuf]) {
    for rel in copied {
        let _ = File::open(dst.path().join(rel)).and_then(|f| f.sync_all());
    }
    for dir in touched_dirs(dst, actions) {
        sync_dir(&dir);
    }
}

/// Every destination directory whose entries an action changed.
fn touched_dirs(dst: &DstRoot, actions: &[Action]) -> BTreeSet<PathBuf> {
    let mut dirs = BTreeSet::new();
    let mut parent_of = |rel: &Path| {
        let abs = dst.path().join(rel);
        dirs.insert(abs.parent().unwrap_or(dst.path()).to_path_buf());
    };
    for a in actions {
        match a {
            Action::Copy(rel) | Action::Delete(rel) => parent_of(rel),
            Action::CreateDir(rel) => parent_of(rel),
            // metadata lives in the file's own inode; the file itself is in the flushed list
            Action::RefreshMeta(_) => {}
            // a new directory entry for an existing (already-flushed) inode
            Action::HardLink { name, .. } => parent_of(name),
            Action::Rename { from, to } => {
                parent_of(from);
                parent_of(to);
            }
        }
    }
    dirs
}

/// fsync a directory's entries. Directories can be opened and synced on unix; on Windows std
/// can't open a directory handle, so this is a no-op there (see module docs).
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    let _ = File::open(dir).and_then(|f| f.sync_all());
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}
