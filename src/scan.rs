//! Walk a directory tree into an in-memory [`Manifest`]. Symlinks are recorded, never followed
//! (so we can't be led out of the tree or into a cycle). The root itself is excluded.

use std::fs;
use std::path::Path;

use walkdir::WalkDir;

use crate::manifest::{Entry, Kind, Manifest};

/// Scan `root` into a manifest sorted by relative path. Read errors are discarded — use
/// [`scan_with_errors`] when you need to report files/directories that couldn't be read.
pub fn scan(root: &Path) -> Manifest {
    scan_with_errors(root).0
}

/// Like [`scan`], but also returns human-readable messages for entries that couldn't be read
/// (e.g. a permission-denied directory), so a run can surface them instead of silently omitting
/// their contents. Readable entries are still collected either way.
pub fn scan_with_errors(root: &Path) -> (Manifest, Vec<String>) {
    let mut entries: Vec<Entry> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for result in WalkDir::new(root).follow_links(false).min_depth(1) {
        match result {
            // ignore our own atomic-copy temp files — they're scratch, not real content
            Ok(dent)
                if dent.file_name().to_string_lossy().starts_with(crate::apply::TMP_PREFIX) => {}
            Ok(dent) => entries.push(entry_from(root, &dent)),
            Err(e) => errors.push(describe_walk_error(&e)),
        }
    }

    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    (Manifest::from_sorted(entries), errors)
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

    let (kind, size, link_target) = if ft.is_symlink() {
        (Kind::Symlink, 0, fs::read_link(dent.path()).ok())
    } else if ft.is_dir() {
        (Kind::Dir, 0, None)
    } else if ft.is_file() {
        let size = dent.metadata().map(|m| m.len()).unwrap_or(0);
        (Kind::File, size, None)
    } else {
        (Kind::Other, 0, None)
    };

    let mtime = dent.metadata().ok().and_then(|m| m.modified().ok());
    Entry { rel, kind, size, mtime, link_target }
}
