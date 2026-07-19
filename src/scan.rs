//! Walk a directory tree into an in-memory [`Manifest`]. Symlinks are recorded, never followed
//! (so we can't be led out of the tree or into a cycle). The root itself is excluded.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::manifest::{DstRoot, Entry, Kind, Manifest};
use crate::progress_update::ScanProgress;
use crate::runtime::elevation;

/// Everything a scan learned: the manifest, plus what it could NOT include — read errors and
/// backup dirs it deliberately skipped (directories carrying [`crate::artifacts::BACKUP_MARKER`]).
pub struct ScanOutcome {
    pub manifest: Manifest,
    /// Human-readable messages for entries that couldn't be read (permission-denied dirs etc.).
    pub errors: Vec<String>,
    /// Root-relative paths of skipped `--backup-dir` trees (marker file present).
    pub skipped_backup_dirs: Vec<PathBuf>,
    /// Root-relative paths of PERMISSION-class read failures (a structured subset of `errors`) —
    /// the input for the showstoppers file, which turns them into paste-able remedies.
    pub denied: Vec<PathBuf>,
}

/// Scan `root` into a manifest sorted by relative path. Read errors and backup-dir skips are
/// discarded and no progress is shown — use [`scan_with_errors`] when either matters.
pub fn scan(root: &Path) -> Manifest {
    scan_with_errors(root, &mut ScanProgress::hidden()).manifest
}

/// Like [`scan`], but also reports what was left out: read errors (so a run can surface them
/// instead of silently omitting content) and skipped backup dirs (directories containing the
/// [`crate::artifacts::BACKUP_MARKER`] file — filesync's own move-aside storage, which must never be
/// mirrored, deleted, or re-backed-up). Readable, unmarked entries are collected either way.
/// Never modifies the tree — safe for the (read-only) source.
pub fn scan_with_errors(root: &Path, progress: &mut ScanProgress) -> ScanOutcome {
    walk(root, false, progress).0
}

/// Scan the **destination**, additionally sweeping (deleting) any leftover atomic-copy temp files
/// a previous interrupted run left behind — one walk instead of a separate sweep pass. Returns the
/// outcome plus how many temp files were removed. Mutating, hence [`DstRoot`]-only (the type wall).
pub fn scan_destination(dst: &DstRoot, progress: &mut ScanProgress) -> (ScanOutcome, usize) {
    walk(dst.path(), true, progress)
}

/// The shared walk. `sweep_tmp` deletes our `TMP_PREFIX` scratch files instead of just skipping
/// them (destination only — the source is never modified).
fn walk(root: &Path, sweep_tmp: bool, progress: &mut ScanProgress) -> (ScanOutcome, usize) {
    let mut entries: Vec<Entry> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut skipped_backup_dirs: Vec<PathBuf> = Vec::new();
    let mut denied: Vec<PathBuf> = Vec::new();
    let mut swept = 0usize;

    let walker = WalkDir::new(root).follow_links(false).min_depth(1).into_iter();
    // Prune (don't descend into) any directory marked as a filesync backup dir. The root itself is
    // exempt (depth 0) — a scan must never exclude its own root.
    let filtered = walker.filter_entry(|dent| {
        let marked = dent.depth() >= 1
            && dent.file_type().is_dir()
            && dent.path().join(crate::artifacts::BACKUP_MARKER).exists();
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
                if dent.file_name().to_string_lossy().starts_with(crate::artifacts::TMP_PREFIX) =>
            {
                if sweep_tmp && dent.file_type().is_file() {
                    // a stray left by an interrupted ELEVATED run can be root-owned in a
                    // root-owned dir — the same wall-retry applies to sweeping it
                    let p = dent.path();
                    let first = fs::remove_file(p);
                    if elevation::retry_if_permission("sweep temp file", p, first, || {
                        fs::remove_file(p)
                    })
                    .is_ok()
                    {
                        swept += 1;
                    }
                }
                progress.tick(0);
            }
            // the root-level lockfile is the running sync's own artifact, never content
            Ok(dent) if dent.depth() == 1 && dent.file_name() == crate::artifacts::LOCK_FILE => {
                progress.tick(0);
            }
            Ok(dent) => {
                let e = entry_from(root, &dent);
                progress.tick(if e.kind == Kind::File { e.size } else { 0 });
                entries.push(e);
            }
            // An unreadable DIRECTORY with root in reserve: re-walk that subtree elevated. This
            // heals the biggest failure mode outright — a healed source scan is complete, so
            // deletion suspension doesn't trigger at all.
            Err(e)
                if elevation::available()
                    && e.io_error().is_some_and(elevation::is_permission)
                    && e.path().is_some_and(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())) =>
            {
                let dir = e.path().expect("guarded above").to_path_buf();
                if !scan_subtree_elevated(
                    root, &dir, sweep_tmp, &mut entries, &mut errors, &mut swept, progress,
                ) {
                    // escalation failed → honest error, and a showstopper entry
                    errors.push(describe_walk_error(root, &e));
                    push_denied(&mut denied, root, &e);
                }
            }
            Err(e) => {
                errors.push(describe_walk_error(root, &e));
                if e.io_error().is_some_and(elevation::is_permission) {
                    push_denied(&mut denied, root, &e);
                }
            }
        }
    }

    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    (ScanOutcome { manifest: Manifest::from_sorted(entries), errors, skipped_backup_dirs, denied }, swept)
}

/// Record a permission-denied walk error's root-relative path (for the showstoppers file).
fn push_denied(denied: &mut Vec<PathBuf>, root: &Path, e: &walkdir::Error) {
    if let Some(p) = e.path() {
        denied.push(p.strip_prefix(root).unwrap_or(p).to_path_buf());
    }
}

/// Re-walk one unreadable directory's subtree with THIS thread elevated (root in reserve), merging
/// what it finds into the main scan. Returns false if escalation itself failed (caller then
/// records the original error). Entries are stat'ed while elevated, so nested root-owned dirs are
/// covered in the same pass; genuinely broken entries inside (EIO, …) are still recorded as
/// errors — root only opens the permission wall, it doesn't paper over damage. The dir itself was
/// already listed by its (readable) parent, so only `min_depth(1)` contents are added.
fn scan_subtree_elevated(
    root: &Path,
    dir_abs: &Path,
    sweep_tmp: bool,
    entries: &mut Vec<Entry>,
    errors: &mut Vec<String>,
    swept: &mut usize,
    progress: &mut ScanProgress,
) -> bool {
    let Ok(_guard) = elevation::ThreadRoot::acquire() else { return false };
    let mut found = 0usize;
    let walker = WalkDir::new(dir_abs).follow_links(false).min_depth(1).into_iter();
    // same pruning as the main walk: marked backup dirs are never scanned
    let filtered = walker.filter_entry(|dent| {
        !(dent.file_type().is_dir() && dent.path().join(crate::artifacts::BACKUP_MARKER).exists())
    });
    for result in filtered {
        match result {
            Ok(dent)
                if dent.file_name().to_string_lossy().starts_with(crate::artifacts::TMP_PREFIX) =>
            {
                if sweep_tmp && dent.file_type().is_file() && fs::remove_file(dent.path()).is_ok()
                {
                    *swept += 1;
                }
                progress.tick(0);
            }
            Ok(dent) => {
                let e = entry_from(root, &dent);
                progress.tick(if e.kind == Kind::File { e.size } else { 0 });
                entries.push(e);
                found += 1;
            }
            Err(e) => errors.push(describe_walk_error(root, &e)),
        }
    }
    elevation::record(format!(
        "read directory (elevated): {} — {found} entr{} merged into the scan",
        dir_abs.display(),
        if found == 1 { "y" } else { "ies" }
    ));
    true
}

/// Render a walk error as `cannot read <rel-path>: <reason>` (falling back to the raw error). The
/// path is made relative to `root` so scan errors read consistently with the diff's per-file
/// issues; the caller prefixes the side (source/destination).
fn describe_walk_error(root: &Path, e: &walkdir::Error) -> String {
    let rel = e.path().map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string());
    match (rel, e.io_error()) {
        (Some(rel), Some(io)) => format!("cannot read {rel}: {io}"),
        (Some(rel), None) => format!("cannot read {rel}: {e}"),
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
    let (owner, mode) = owner_mode(md.as_ref());
    let mtime = md.and_then(|m| m.modified().ok());
    Entry { rel, kind, size, mtime, link_target, link_id, owner, mode }
}

/// Owner and permission bits from the stat we already did (unix; `(None, None)` elsewhere).
#[cfg(unix)]
fn owner_mode(md: Option<&fs::Metadata>) -> (Option<(u32, u32)>, Option<u32>) {
    use std::os::unix::fs::MetadataExt;
    match md {
        Some(m) => (Some((m.uid(), m.gid())), Some(m.mode())),
        None => (None, None),
    }
}

#[cfg(not(unix))]
fn owner_mode(_md: Option<&fs::Metadata>) -> (Option<(u32, u32)>, Option<u32>) {
    (None, None)
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
