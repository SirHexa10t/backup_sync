//! Walk a directory tree into an in-memory [`Manifest`]. Symlinks are recorded, never followed
//! (so we can't be led out of the tree or into a cycle). The root itself is excluded.

use std::fs;
use std::path::Path;

use walkdir::WalkDir;

use crate::manifest::{Entry, Kind, Manifest};

/// Scan `root` into a manifest sorted by relative path.
pub fn scan(root: &Path) -> Manifest {
    let mut entries: Vec<Entry> = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1) // exclude the root itself
        .into_iter()
        .filter_map(Result::ok) // TODO: collect walk errors into the run report (report.rs)
        // ignore our own atomic-copy temp files — they're scratch, not real content
        .filter(|dent| !dent.file_name().to_string_lossy().starts_with(crate::apply::TMP_PREFIX))
        .map(|dent| entry_from(root, &dent))
        .collect();
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    Manifest::from_sorted(entries)
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
