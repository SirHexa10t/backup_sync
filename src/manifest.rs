//! In-memory representation of a directory tree, plus the source/destination type wall.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A source root. Nothing in the program writes through this type — the read-only invariant is
/// enforced *by construction*: the destructive operations (copy / delete / rename, added in a later
/// phase) accept only [`DstRoot`], so a source path can never reach them.
#[derive(Debug, Clone)]
pub struct SrcRoot(PathBuf);

/// A destination root — the only place mutations are allowed.
#[derive(Debug, Clone)]
pub struct DstRoot(PathBuf);

impl SrcRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl DstRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// What a filesystem entry is. `Other` covers fifos, sockets, devices, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry in a scanned tree, identified by its path relative to the scanned root.
///
/// Content hashes are intentionally *not* here — they're computed lazily by the diff/move stage,
/// and only for the candidates that need them (never the whole tree).
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path relative to the scanned root.
    pub rel: PathBuf,
    pub kind: Kind,
    /// File size in bytes (0 for non-files).
    pub size: u64,
    /// Last-modified time, if the platform/filesystem reports one.
    pub mtime: Option<SystemTime>,
    /// Target of a symlink (only set when `kind == Symlink`).
    pub link_target: Option<PathBuf>,
    /// Hard-link identity `(device, inode)` — set only for regular files with more than one name
    /// (`nlink > 1`, unix). Entries sharing this value are the same file under different names.
    /// Free to collect: it comes from the stat the scan already performs.
    pub link_id: Option<(u64, u64)>,
    /// Owner `(uid, gid)` and raw `st_mode` bits (unix; `None` elsewhere or when the stat failed).
    /// Also free from the scan's existing stat — showstopper predictions ("can this process read/
    /// write/delete that?") are pure arithmetic over these, no extra syscalls.
    pub owner: Option<(u32, u32)>,
    pub mode: Option<u32>,
}

/// A scanned tree: entries sorted by relative path.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    entries: Vec<Entry>,
}

impl Manifest {
    /// Build from entries that are already sorted by `rel`.
    pub fn from_sorted(entries: Vec<Entry>) -> Self {
        Self { entries }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Hard-link groups: files sharing an inode, grouped, each group holding ≥2 members in
    /// manifest (path-sorted) order. The first member of a group is its **leader** — the one
    /// name whose content gets copied; the rest are followers, realized as hard links.
    pub fn hardlink_groups(&self) -> Vec<Vec<&Entry>> {
        let mut by_id: std::collections::HashMap<(u64, u64), Vec<&Entry>> =
            std::collections::HashMap::new();
        for e in self.entries.iter() {
            if let Some(id) = e.link_id {
                by_id.entry(id).or_default().push(e); // entries are sorted ⇒ groups are sorted
            }
        }
        let mut groups: Vec<Vec<&Entry>> = by_id.into_values().filter(|g| g.len() >= 2).collect();
        groups.sort_by(|a, b| a[0].rel.cmp(&b[0].rel));
        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_expose_their_path() {
        assert_eq!(SrcRoot::new("/x").path(), Path::new("/x"));
        assert_eq!(DstRoot::new("/y").path(), Path::new("/y"));
    }
}
