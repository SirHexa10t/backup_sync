//! Walk a directory tree into an in-memory [`Manifest`]. Symlinks are recorded, never followed
//! (so we can't be led out of the tree or into a cycle). The root itself is excluded.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::manifest::{DstRoot, Entry, Kind, Manifest};

/// Everything a scan learned: the manifest, plus what it could NOT include — read errors and
/// backup dirs it deliberately skipped (directories carrying [`crate::apply::BACKUP_MARKER`]).
pub struct ScanOutcome {
    pub manifest: Manifest,
    /// Human-readable messages for entries that couldn't be read (permission-denied dirs etc.).
    pub errors: Vec<String>,
    /// Root-relative paths of skipped `--backup-dir` trees (marker file present).
    pub skipped_backup_dirs: Vec<PathBuf>,
}

/// Scan `root` into a manifest sorted by relative path. Read errors and backup-dir skips are
/// discarded — use [`scan_with_errors`] when they need to be reported.
pub fn scan(root: &Path) -> Manifest {
    scan_with_errors(root).manifest
}

/// Like [`scan`], but also reports what was left out: read errors (so a run can surface them
/// instead of silently omitting content) and skipped backup dirs (directories containing the
/// [`crate::apply::BACKUP_MARKER`] file — filesync's own move-aside storage, which must never be
/// mirrored, deleted, or re-backed-up). Readable, unmarked entries are collected either way.
/// Never modifies the tree — safe for the (read-only) source.
pub fn scan_with_errors(root: &Path) -> ScanOutcome {
    walk(root, false).0
}

/// Scan the **destination**, additionally sweeping (deleting) any leftover atomic-copy temp files
/// a previous interrupted run left behind — one walk instead of a separate sweep pass. Returns the
/// outcome plus how many temp files were removed. Mutating, hence [`DstRoot`]-only (the type wall).
pub fn scan_destination(dst: &DstRoot) -> (ScanOutcome, usize) {
    walk(dst.path(), true)
}

/// The shared walk. `sweep_tmp` deletes our `TMP_PREFIX` scratch files instead of just skipping
/// them (destination only — the source is never modified).
fn walk(root: &Path, sweep_tmp: bool) -> (ScanOutcome, usize) {
    let mut entries: Vec<Entry> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut skipped_backup_dirs: Vec<PathBuf> = Vec::new();
    let mut swept = 0usize;

    let walker = WalkDir::new(root).follow_links(false).min_depth(1).into_iter();
    // Prune (don't descend into) any directory marked as a filesync backup dir. The root itself is
    // exempt (depth 0) — a scan must never exclude its own root.
    let filtered = walker.filter_entry(|dent| {
        let marked = dent.depth() >= 1
            && dent.file_type().is_dir()
            && dent.path().join(crate::apply::BACKUP_MARKER).exists();
        if marked {
            let rel = dent.path().strip_prefix(root).unwrap_or(dent.path()).to_path_buf();
            skipped_backup_dirs.push(rel);
        }
        !marked
    });

    for result in filtered {
        match result {
            // our own atomic-copy temp files are scratch, not content: skip (and sweep at dst)
            Ok(dent)
                if dent.file_name().to_string_lossy().starts_with(crate::apply::TMP_PREFIX) =>
            {
                if sweep_tmp && dent.file_type().is_file() && fs::remove_file(dent.path()).is_ok()
                {
                    swept += 1;
                }
            }
            // the root-level lockfile is the running sync's own artifact, never content
            Ok(dent) if dent.depth() == 1 && dent.file_name() == crate::lock::LOCK_FILE => {}
            Ok(dent) => entries.push(entry_from(root, &dent)),
            Err(e) => errors.push(describe_walk_error(&e)),
        }
    }

    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    (ScanOutcome { manifest: Manifest::from_sorted(entries), errors, skipped_backup_dirs }, swept)
}

/// Render a walk error as `cannot read <path>: <reason>` (falling back to the raw error).
fn describe_walk_error(e: &walkdir::Error) -> String {
    match (e.path(), e.io_error()) {
        (Some(p), Some(io)) => format!("cannot read {}: {io}", p.display()),
        (Some(p), None) => format!("cannot read {}: {e}", p.display()),
        _ => format!("scan error: {e}"),
    }
}

fn entry_from(root: &Path, dent: &walkdir::DirEntry) -> Entry {
    let rel = dent.path().strip_prefix(root).unwrap_or(dent.path()).to_path_buf();
    let ft = dent.file_type(); // with follow_links(false), reflects the entry itself (lstat)
    let md = dent.metadata().ok(); // one stat, reused for size, mtime, and hard-link identity

    let (kind, size, link_target) = if ft.is_symlink() {
        (Kind::Symlink, 0, fs::read_link(dent.path()).ok())
    } else if ft.is_dir() {
        (Kind::Dir, 0, None)
    } else if ft.is_file() {
        (Kind::File, md.as_ref().map(|m| m.len()).unwrap_or(0), None)
    } else {
        (Kind::Other, 0, None)
    };

    // Only multi-name regular files get a link identity (dirs always have nlink > 1 — subdirs).
    let link_id = if kind == Kind::File { md.as_ref().and_then(link_id_of) } else { None };
    let mtime = md.and_then(|m| m.modified().ok());
    Entry { rel, kind, size, mtime, link_target, link_id }
}

/// `(device, inode)` for files that have more than one name. Keyed by device too: inode numbers
/// repeat across filesystems, and a scanned tree may span mounts.
#[cfg(unix)]
fn link_id_of(md: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (md.nlink() > 1).then(|| (md.dev(), md.ino()))
}

#[cfg(not(unix))]
fn link_id_of(_md: &fs::Metadata) -> Option<(u64, u64)> {
    None // no portable inode identity off-unix; hard links degrade to independent copies
}
