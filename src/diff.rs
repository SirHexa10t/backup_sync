//! Compare a source and destination tree into actions: added / removed / changed / moved /
//! unchanged. Move-detection pairs content-identical add/remove files (see `docs/theory.md`).
//!
//! Efficiency: only files whose size appears on *both* the add and remove sides ("contested") can
//! be moves, so nothing else is ever hashed. Contested candidates are hashed in parallel, each at
//! most once, then matched by content via a hash→queue map (which also handles duplicate content).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::hash;
use crate::manifest::{DstRoot, Entry, Kind, Manifest, SrcRoot};

/// A single-path difference, carrying the entry kind so the planner knows how to realize it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub rel: PathBuf,
    pub kind: Kind,
}

/// A content-identical file currently at `from` on the destination that should live at `to`
/// (its path in the source) — executed as a rename, not a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// The classified difference between a source and a destination tree.
#[derive(Debug, Default)]
pub struct Diff {
    pub added: Vec<Change>,   // in source, missing at dest → copy
    pub removed: Vec<Change>, // extra at dest, not in source → delete
    pub changed: Vec<Change>, // present on both sides but differing → update
    pub moved: Vec<Move>,     // content-identical relocation → rename at dest
    pub unchanged: usize,
}

/// Classify `src` vs `dst`. `eager` compares files by content hash instead of size+mtime.
pub fn diff(
    src: &SrcRoot,
    src_m: &Manifest,
    dst: &DstRoot,
    dst_m: &Manifest,
    eager: bool,
    jobs: usize,
) -> std::io::Result<Diff> {
    let dst_by_path: HashMap<&Path, &Entry> = dst_m.iter().map(|e| (e.rel.as_path(), e)).collect();
    let src_by_path: HashMap<&Path, &Entry> = src_m.iter().map(|e| (e.rel.as_path(), e)).collect();

    let mut d = Diff::default();
    let mut add_files: Vec<&Entry> = Vec::new(); // file-only adds — candidates for move-detection
    let mut rm_files: Vec<&Entry> = Vec::new(); // file-only removes — the other side of a move

    for se in src_m.iter() {
        match dst_by_path.get(se.rel.as_path()) {
            // Same path, different kind (file↔dir↔symlink): not an in-place update. Treat it as
            // "delete the destination's version + add the source's" — the plan's delete-before-
            // create ordering then resolves it with no special case.
            Some(de) if se.kind != de.kind => {
                if se.kind == Kind::File {
                    add_files.push(se);
                } else {
                    d.added.push(Change { rel: se.rel.clone(), kind: se.kind });
                }
                if de.kind == Kind::File {
                    rm_files.push(de);
                } else {
                    d.removed.push(Change { rel: de.rel.clone(), kind: de.kind });
                }
            }
            Some(de) => {
                if entries_equal(src, se, dst, de, eager)? {
                    d.unchanged += 1;
                } else {
                    d.changed.push(Change { rel: se.rel.clone(), kind: se.kind });
                }
            }
            None if se.kind == Kind::File => add_files.push(se),
            None => d.added.push(Change { rel: se.rel.clone(), kind: se.kind }),
        }
    }
    for de in dst_m.iter() {
        if !src_by_path.contains_key(de.rel.as_path()) {
            if de.kind == Kind::File {
                rm_files.push(de);
            } else {
                d.removed.push(Change { rel: de.rel.clone(), kind: de.kind });
            }
        }
    }

    detect_moves(src, &add_files, dst, &rm_files, &mut d, jobs)?;

    d.added.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.removed.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.changed.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.moved.sort_by(|a, b| a.to.cmp(&b.to));
    Ok(d)
}

fn entries_equal(
    src: &SrcRoot,
    se: &Entry,
    dst: &DstRoot,
    de: &Entry,
    eager: bool,
) -> std::io::Result<bool> {
    if se.kind != de.kind {
        return Ok(false);
    }
    Ok(match se.kind {
        Kind::Dir => true, // same path, both dirs — dir mtime churns, so don't call it a change
        Kind::Symlink => se.link_target == de.link_target,
        Kind::Other => false, // can't meaningfully compare specials → flag for attention
        Kind::File => {
            if eager {
                hash::hash_file(&src.path().join(&se.rel))?
                    == hash::hash_file(&dst.path().join(&de.rel))?
            } else {
                se.size == de.size && mtime_close(se.mtime, de.mtime)
            }
        }
    })
}

/// Treat mtimes within 2s as equal (tolerates coarse FAT/exFAT timestamps). If either side lacks
/// an mtime, don't use it to flag a change (fall back to size alone).
fn mtime_close(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => {
            let delta = if x >= y { x.duration_since(y) } else { y.duration_since(x) };
            delta.map(|d| d <= Duration::from_secs(2)).unwrap_or(false)
        }
        _ => true,
    }
}

/// Pair content-identical adds (source) with removes (dest) — those are moves. Size pre-filter +
/// parallel hashing of only the contested candidates + hash→queue matching (dup-content safe).
fn detect_moves(
    src: &SrcRoot,
    add_files: &[&Entry],
    dst: &DstRoot,
    rm_files: &[&Entry],
    d: &mut Diff,
    jobs: usize,
) -> std::io::Result<()> {
    let add_sizes: HashSet<u64> = add_files.iter().map(|e| e.size).collect();
    let rem_sizes: HashSet<u64> = rm_files.iter().map(|e| e.size).collect();
    let contested: HashSet<u64> = add_sizes.intersection(&rem_sizes).copied().collect();

    // Indices of files worth hashing: only those whose size appears on both sides.
    let add_cand: Vec<usize> =
        (0..add_files.len()).filter(|&i| contested.contains(&add_files[i].size)).collect();
    let rem_cand: Vec<usize> =
        (0..rm_files.len()).filter(|&i| contested.contains(&rm_files[i].size)).collect();

    let add_hashes = par_hash(src.path(), add_cand, add_files, jobs)?;
    let rem_hashes = par_hash(dst.path(), rem_cand, rm_files, jobs)?;

    // content hash → queue of remove indices with that content (queue handles duplicate content)
    let mut by_hash: HashMap<[u8; 32], VecDeque<usize>> = HashMap::new();
    for (i, h) in rem_hashes {
        by_hash.entry(h).or_default().push_back(i);
    }

    let mut moved_add = vec![false; add_files.len()];
    let mut moved_rem = vec![false; rm_files.len()];
    for (ai, h) in add_hashes {
        if let Some(q) = by_hash.get_mut(&h) {
            if let Some(ri) = q.pop_front() {
                d.moved.push(Move { from: rm_files[ri].rel.clone(), to: add_files[ai].rel.clone() });
                moved_add[ai] = true;
                moved_rem[ri] = true;
            }
        }
    }

    for (i, e) in add_files.iter().enumerate() {
        if !moved_add[i] {
            d.added.push(Change { rel: e.rel.clone(), kind: Kind::File });
        }
    }
    for (i, e) in rm_files.iter().enumerate() {
        if !moved_rem[i] {
            d.removed.push(Change { rel: e.rel.clone(), kind: Kind::File });
        }
    }
    Ok(())
}

/// Hash the given candidate files (by index into `files`) under `root`, using `jobs` threads.
fn par_hash(
    root: &Path,
    cand: Vec<usize>,
    files: &[&Entry],
    jobs: usize,
) -> std::io::Result<Vec<(usize, [u8; 32])>> {
    crate::parallel::map(jobs, cand, |i| {
        hash::hash_file(&root.join(&files[i].rel)).map(|h| (i, *h.as_bytes()))
    })
    .into_iter()
    .collect()
}

impl Diff {
    pub fn added_paths(&self) -> Vec<PathBuf> {
        self.added.iter().map(|c| c.rel.clone()).collect()
    }
    pub fn removed_paths(&self) -> Vec<PathBuf> {
        self.removed.iter().map(|c| c.rel.clone()).collect()
    }
    pub fn changed_paths(&self) -> Vec<PathBuf> {
        self.changed.iter().map(|c| c.rel.clone()).collect()
    }

    /// A git-diff-like textual summary (used by the `diff` command and, later, the report).
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "moved:     {}", self.moved.len());
        for m in &self.moved {
            let _ = writeln!(s, "    ~ {}  ->  {}", m.from.display(), m.to.display());
        }
        let _ = writeln!(s, "to copy:   {}", self.added.len());
        for c in &self.added {
            let _ = writeln!(s, "    + {}", c.rel.display());
        }
        let _ = writeln!(s, "to delete: {}", self.removed.len());
        for c in &self.removed {
            let _ = writeln!(s, "    - {}", c.rel.display());
        }
        let _ = writeln!(s, "to update: {}", self.changed.len());
        for c in &self.changed {
            let _ = writeln!(s, "    * {}", c.rel.display());
        }
        let _ = writeln!(s, "unchanged: {}", self.unchanged);
        s
    }
}
