//! Diagnostics for a `diff`: distil a large classification into the few conclusions a human should
//! actually look at — loudest where data could be lost.
//!
//! The motivating danger: a backup mirror deletes whatever the source no longer has, so a folder
//! that vanished from the source (by accident, a bad mount, a moved drive) quietly takes its only
//! surviving copy with it. There is far too much in a multi-terabyte diff to eyeball, so this module
//! surfaces the catastrophic-looking parts up front.
//!
//! Everything is computed from data already in memory (the two manifests + the [`Diff`]), so there
//! is no extra scan and negligible memory even on a huge tree. [`analyze`] returns a [`Conclusions`]
//! of plain numbers (tests assert on values); [`Conclusions::render`] formats the `…conclusions.txt`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::diff::Diff;
use crate::manifest::{Kind, Manifest};

/// Path components that are pure noise — tallied separately so backing up someone's trash, or a
/// recycle folder about to be mirror-deleted, doesn't hide inside the totals. `.Trash-*` is matched
/// by prefix; the rest by exact component name.
const JUNK_NAMES: &[&str] = &[
    "$RECYCLE.BIN",
    "System Volume Information",
    ".DS_Store",
    "@eaDir",
    "lost+found",
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
];

/// How many rows a bounded list shows before it summarizes "… and N more".
const TOP_N: usize = 20;

/// A deleted destination subtree — a directory absent from the source, with everything under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtree {
    pub root: PathBuf,
    pub files: u64,
    pub bytes: u64,
}

/// A file with its size, for the "largest" and "orphan deletion" lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub rel: PathBuf,
    pub bytes: u64,
}

/// One top-level folder's share of the change (files only; directories are covered by [`Subtree`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderDelta {
    pub folder: String,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub moved_in: usize,
    pub net_files: i64,
    pub net_bytes: i64,
}

/// A junk/system path's presence on each side of the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JunkTally {
    pub pattern: String,
    pub deleting_from_dest: u64,
    pub copying_from_source: u64,
}

/// The distilled conclusions. Fields are public so callers (and tests) read the numbers directly.
#[derive(Debug, Default, Clone)]
pub struct Conclusions {
    // Headline deletion volume, framed against the destination's size.
    pub deleted_files: u64,
    pub deleted_bytes: u64,
    pub dest_files: u64,
    pub dest_bytes: u64,

    // A — data-loss watch.
    /// Whole destination directories absent from the source (deepest-savings first).
    pub deleted_subtrees: Vec<Subtree>,
    /// Deletions with no same base-name anywhere in the source — not a move/rename, so this content
    /// simply goes away (largest first).
    pub orphan_deletes: Vec<FileEntry>,
    pub orphan_bytes: u64,
    /// Of the orphans, how many also have no same-*size* file anywhere in the source — the ones
    /// least likely to be a move the detector failed to pair, i.e. most likely genuinely unique.
    pub orphan_unique_size: u64,

    // B — overview.
    pub added: usize,
    pub added_bytes: u64,
    pub moved: usize,
    pub changed: usize,
    pub refreshed: usize,
    pub linked: usize,
    pub unchanged: usize,

    /// Empty (0-byte) files among the adds and deletes. Move-detection skips size 0 (an empty
    /// "move" is meaningless), so these are plain add/delete; the render notes that when both sides
    /// have some — the case where they might otherwise have looked like moves.
    pub empty_adds: u64,
    pub empty_removes: u64,

    // C — per top-level folder (biggest byte losses first).
    pub folders: Vec<FolderDelta>,

    // D — junk / system paths present in the job.
    pub junk: Vec<JunkTally>,

    // E — extremes.
    pub largest_adds: Vec<FileEntry>,
    pub largest_deletes: Vec<FileEntry>,
    /// Deleted-file extensions by count (lowercased; "(none)" for extensionless), most first.
    pub delete_ext_histogram: Vec<(String, u64)>,
}

/// Compute the conclusions from a classified diff and the two manifests it came from.
pub fn analyze(d: &Diff, src_m: &Manifest, dst_m: &Manifest) -> Conclusions {
    let mut c = Conclusions::default();

    // Size lookups for files on each side.
    let dst_size: HashMap<&Path, u64> =
        dst_m.iter().filter(|e| e.kind == Kind::File).map(|e| (e.rel.as_path(), e.size)).collect();
    let src_size: HashMap<&Path, u64> =
        src_m.iter().filter(|e| e.kind == Kind::File).map(|e| (e.rel.as_path(), e.size)).collect();

    c.dest_files = dst_size.len() as u64;
    c.dest_bytes = dst_size.values().sum();

    // --- overview ---
    c.added = d.added.len();
    c.added_bytes = d.added.iter().filter_map(|ch| src_size.get(ch.rel.as_path())).sum();
    c.moved = d.moved.len();
    c.changed = d.changed.len();
    c.refreshed = d.touched.len();
    c.linked = d.to_link.len();
    c.unchanged = d.unchanged;

    // --- deletions (files only; directories are accounted for as subtrees) ---
    let deleted_files: Vec<FileEntry> = d
        .removed
        .iter()
        .filter(|ch| ch.kind == Kind::File)
        .map(|ch| FileEntry {
            rel: ch.rel.clone(),
            bytes: dst_size.get(ch.rel.as_path()).copied().unwrap_or(0),
        })
        .collect();
    c.deleted_files = deleted_files.len() as u64;
    c.deleted_bytes = deleted_files.iter().map(|f| f.bytes).sum();

    // Empty (0-byte) files on each side — excluded from move-detection (see `diff::detect_moves`).
    c.empty_adds = d
        .added
        .iter()
        .filter(|ch| ch.kind == Kind::File && src_size.get(ch.rel.as_path()).copied().unwrap_or(0) == 0)
        .count() as u64;
    c.empty_removes = deleted_files.iter().filter(|f| f.bytes == 0).count() as u64;

    // --- A1: whole deleted subtrees = shallowest removed dirs (no removed-dir ancestor) ---
    let removed_dir_set: HashSet<&Path> =
        d.removed.iter().filter(|ch| ch.kind == Kind::Dir).map(|ch| ch.rel.as_path()).collect();
    for dir in d.removed.iter().filter(|ch| ch.kind == Kind::Dir).map(|ch| &ch.rel) {
        if dir.ancestors().skip(1).any(|a| removed_dir_set.contains(a)) {
            continue; // nested under another deleted dir — the ancestor already accounts for it
        }
        let mut sub = Subtree { root: dir.clone(), files: 0, bytes: 0 };
        for f in deleted_files.iter().filter(|f| f.rel.starts_with(dir)) {
            sub.files += 1;
            sub.bytes += f.bytes;
        }
        c.deleted_subtrees.push(sub);
    }
    c.deleted_subtrees.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.root.cmp(&b.root)));

    // --- A2: orphan deletions — no same base-name anywhere in the source ---
    let src_basenames: HashSet<String> = src_m.iter().map(|e| basename_lower(&e.rel)).collect();
    let src_sizes: HashSet<u64> = src_size.values().copied().collect();
    for f in &deleted_files {
        if !src_basenames.contains(&basename_lower(&f.rel)) {
            c.orphan_bytes += f.bytes;
            if !src_sizes.contains(&f.bytes) {
                c.orphan_unique_size += 1;
            }
            c.orphan_deletes.push(f.clone());
        }
    }
    c.orphan_deletes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.rel.cmp(&b.rel)));

    // --- C: per top-level folder (files) ---
    let mut folders: HashMap<String, FolderDelta> = HashMap::new();
    for ch in d.added.iter().filter(|ch| ch.kind == Kind::File) {
        let size = src_size.get(ch.rel.as_path()).copied().unwrap_or(0);
        let e = folder_entry(&mut folders, top_level(&ch.rel));
        e.added += 1;
        e.net_files += 1;
        e.net_bytes += size as i64;
    }
    for f in &deleted_files {
        let e = folder_entry(&mut folders, top_level(&f.rel));
        e.removed += 1;
        e.net_files -= 1;
        e.net_bytes -= f.bytes as i64;
    }
    for ch in d.changed.iter().filter(|ch| ch.kind == Kind::File) {
        let before = dst_size.get(ch.rel.as_path()).copied().unwrap_or(0) as i64;
        let after = src_size.get(ch.rel.as_path()).copied().unwrap_or(0) as i64;
        let e = folder_entry(&mut folders, top_level(&ch.rel));
        e.changed += 1;
        e.net_bytes += after - before; // an in-place update only shifts bytes, not the file count
    }
    for m in &d.moved {
        let size = src_size.get(m.to.as_path()).copied().unwrap_or(0) as i64;
        let to = folder_entry(&mut folders, top_level(&m.to));
        to.moved_in += 1;
        to.net_files += 1;
        to.net_bytes += size;
        let from = folder_entry(&mut folders, top_level(&m.from));
        from.net_files -= 1;
        from.net_bytes -= size; // a within-folder move nets to zero; a cross-folder one shows as a shift
    }
    c.folders = folders.into_values().collect();
    c.folders.sort_by(|a, b| a.net_bytes.cmp(&b.net_bytes).then_with(|| a.folder.cmp(&b.folder)));

    // --- D: junk / system paths ---
    let mut junk: HashMap<&'static str, JunkTally> = HashMap::new();
    for ch in &d.removed {
        if let Some(p) = junk_pattern(&ch.rel) {
            junk.entry(p).or_insert_with(|| tally(p)).deleting_from_dest += 1;
        }
    }
    for ch in &d.added {
        if let Some(p) = junk_pattern(&ch.rel) {
            junk.entry(p).or_insert_with(|| tally(p)).copying_from_source += 1;
        }
    }
    c.junk = junk.into_values().collect();
    c.junk.sort_by(|a, b| {
        (b.deleting_from_dest + b.copying_from_source)
            .cmp(&(a.deleting_from_dest + a.copying_from_source))
            .then_with(|| a.pattern.cmp(&b.pattern))
    });

    // --- E: extremes ---
    c.largest_adds = d
        .added
        .iter()
        .filter(|ch| ch.kind == Kind::File)
        .map(|ch| FileEntry { rel: ch.rel.clone(), bytes: src_size.get(ch.rel.as_path()).copied().unwrap_or(0) })
        .collect();
    c.largest_adds.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.rel.cmp(&b.rel)));
    c.largest_adds.truncate(TOP_N);

    c.largest_deletes = deleted_files.clone();
    c.largest_deletes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.rel.cmp(&b.rel)));
    c.largest_deletes.truncate(TOP_N);

    let mut ext: HashMap<String, u64> = HashMap::new();
    for f in &deleted_files {
        *ext.entry(extension_lower(&f.rel)).or_insert(0) += 1;
    }
    c.delete_ext_histogram = ext.into_iter().collect();
    c.delete_ext_histogram
        .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    c.delete_ext_histogram.truncate(TOP_N);

    c
}

impl Conclusions {
    /// The deletion is alarming enough to shout about: it removes a large share of the destination,
    /// or wipes a whole top-level folder. Drives the banner at the top of the rendered file.
    pub fn data_loss_alarm(&self) -> bool {
        let tenth_of_files = self.deleted_files.saturating_mul(10) >= self.dest_files && self.dest_files > 0;
        let tenth_of_bytes = self.deleted_bytes.saturating_mul(10) >= self.dest_bytes && self.dest_bytes > 0;
        let whole_top_folder =
            self.deleted_subtrees.iter().any(|s| s.root.components().count() == 1);
        tenth_of_files || tenth_of_bytes || whole_top_folder
    }

    /// Format the conclusions for the `…conclusions.txt` file. `src`/`dst` are the paths compared,
    /// for the header.
    pub fn render(&self, src: &str, dst: &str) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "filesync diff — conclusions");
        let _ = writeln!(s, "comparing {src}  ->  {dst}");
        let _ = writeln!(s);

        // A — data-loss watch.
        let pct_files = percent(self.deleted_files, self.dest_files);
        let pct_bytes = percent(self.deleted_bytes, self.dest_bytes);
        if self.deleted_files == 0 && self.deleted_subtrees.is_empty() {
            let _ = writeln!(s, "DATA-LOSS WATCH: nothing would be deleted this run.");
        } else {
            if self.data_loss_alarm() {
                let _ = writeln!(s, "‼‼ DATA-LOSS WATCH — review before syncing ‼‼");
            } else {
                let _ = writeln!(s, "DATA-LOSS WATCH");
            }
            let _ = writeln!(
                s,
                "  Deleting {} file(s), {} — {pct_files}% of the destination's files, {pct_bytes}% of its bytes.",
                self.deleted_files,
                human_bytes(self.deleted_bytes)
            );
            if !self.deleted_subtrees.is_empty() {
                let _ = writeln!(
                    s,
                    "  {} destination folder(s) would be deleted ENTIRELY (absent from the source):",
                    self.deleted_subtrees.len()
                );
                write_subtrees(&mut s, &self.deleted_subtrees);
            }
            if self.orphan_deletes.is_empty() {
                let _ = writeln!(
                    s,
                    "  Every deletion shares a name with something in the source (all likely moves/renames)."
                );
            } else {
                let _ = writeln!(
                    s,
                    "  {} deletion(s) ({}) have NO same-named file in the source — not a move; this content \
                     would be gone. {} of them also have no same-size file anywhere in the source.",
                    self.orphan_deletes.len(),
                    human_bytes(self.orphan_bytes),
                    self.orphan_unique_size
                );
                let _ = writeln!(s, "  Largest such deletions:");
                write_files(&mut s, &self.orphan_deletes, "    ");
            }
        }
        let _ = writeln!(s);

        // B — overview.
        let _ = writeln!(s, "OVERVIEW");
        let _ = writeln!(
            s,
            "  copy {} ({}) · move {} · delete {} ({}) · update {} · refresh {} · link {} · unchanged {}",
            self.added,
            human_bytes(self.added_bytes),
            self.moved,
            self.deleted_files,
            human_bytes(self.deleted_bytes),
            self.changed,
            self.refreshed,
            self.linked,
            self.unchanged
        );
        let _ = writeln!(s);

        // Empty files: distinct note when both sides have some (where they might otherwise have
        // looked like moves) — move-detection deliberately skips size 0.
        if self.empty_adds > 0 && self.empty_removes > 0 {
            let _ = writeln!(
                s,
                "NOTE: {} empty (0-byte) file(s) to copy and {} to delete are handled as plain \
                 add/delete — NO move was attempted, because a size of 0 has no content to match.",
                self.empty_adds, self.empty_removes
            );
            let _ = writeln!(s);
        }

        // C — per top-level folder.
        if !self.folders.is_empty() {
            let _ = writeln!(s, "CHANGES BY TOP-LEVEL FOLDER (biggest byte losses first)");
            let _ = writeln!(
                s,
                "  {:<28} {:>5} {:>6} {:>5} {:>6}  {:>10}  {:>12}",
                "folder", "+add", "-del", "*chg", "~move", "net files", "net bytes"
            );
            for f in &self.folders {
                let _ = writeln!(
                    s,
                    "  {:<28} {:>5} {:>6} {:>5} {:>6}  {:>+10} {:>13}",
                    truncate(&f.folder, 28),
                    f.added,
                    f.removed,
                    f.changed,
                    f.moved_in,
                    f.net_files,
                    signed_bytes(f.net_bytes)
                );
            }
            let _ = writeln!(s);
        }

        // D — junk / system paths.
        if !self.junk.is_empty() {
            let _ = writeln!(s, "JUNK / SYSTEM PATHS");
            for j in &self.junk {
                let mut parts = Vec::new();
                if j.deleting_from_dest > 0 {
                    parts.push(format!("{} to delete from destination", j.deleting_from_dest));
                }
                if j.copying_from_source > 0 {
                    parts.push(format!("{} to copy from source", j.copying_from_source));
                }
                let _ = writeln!(s, "  {:<26} {}", j.pattern, parts.join(", "));
            }
            let _ = writeln!(s);
        }

        // E — extremes.
        if !self.largest_deletes.is_empty() {
            let _ = writeln!(s, "LARGEST DELETIONS");
            write_files(&mut s, &self.largest_deletes, "  ");
            let _ = writeln!(s);
        }
        if !self.largest_adds.is_empty() {
            let _ = writeln!(s, "LARGEST ADDITIONS");
            write_files(&mut s, &self.largest_adds, "  ");
            let _ = writeln!(s);
        }
        if !self.delete_ext_histogram.is_empty() {
            let _ = writeln!(s, "DELETIONS BY EXTENSION");
            for (extn, n) in &self.delete_ext_histogram {
                let _ = writeln!(s, "  {n:>8}  {extn}");
            }
            let _ = writeln!(s);
        }

        // Full orphan list last — it can be long, so it lives at the bottom.
        if !self.orphan_deletes.is_empty() {
            let _ = writeln!(s, "FULL LIST — deletions with no same-named file in the source ({})", self.orphan_deletes.len());
            for f in &self.orphan_deletes {
                let _ = writeln!(s, "  {}", f.rel.display());
            }
        }

        s
    }
}

/// Write up to [`TOP_N`] file rows (`indent path  size`), then summarize any remainder.
fn write_files(s: &mut String, files: &[FileEntry], indent: &str) {
    use std::fmt::Write;
    for f in files.iter().take(TOP_N) {
        let _ = writeln!(s, "{indent}{}  ({})", f.rel.display(), human_bytes(f.bytes));
    }
    if files.len() > TOP_N {
        let rest: u64 = files.iter().skip(TOP_N).map(|f| f.bytes).sum();
        let _ = writeln!(s, "{indent}… and {} more ({})", files.len() - TOP_N, human_bytes(rest));
    }
}

fn write_subtrees(s: &mut String, subs: &[Subtree]) {
    use std::fmt::Write;
    for sub in subs.iter().take(TOP_N) {
        let _ = writeln!(
            s,
            "    {}   {} file(s), {}",
            sub.root.display(),
            sub.files,
            human_bytes(sub.bytes)
        );
    }
    if subs.len() > TOP_N {
        let _ = writeln!(s, "    … and {} more folder(s)", subs.len() - TOP_N);
    }
}

fn folder_entry<'a>(m: &'a mut HashMap<String, FolderDelta>, key: String) -> &'a mut FolderDelta {
    m.entry(key.clone()).or_insert_with(|| FolderDelta {
        folder: key,
        added: 0,
        removed: 0,
        changed: 0,
        moved_in: 0,
        net_files: 0,
        net_bytes: 0,
    })
}

fn tally(pattern: &'static str) -> JunkTally {
    JunkTally { pattern: pattern.to_string(), deleting_from_dest: 0, copying_from_source: 0 }
}

/// The first path component if the entry lives under a folder; a shared bucket for entries that sit
/// directly at the root (so a diff of loose top-level files doesn't spray one row each).
fn top_level(rel: &Path) -> String {
    let mut comps = rel.components();
    match (comps.next(), comps.next()) {
        (Some(first), Some(_)) => first.as_os_str().to_string_lossy().into_owned(),
        _ => "(top-level files)".to_string(),
    }
}

/// The junk pattern this path matches, if any (`.Trash-*` by prefix, the rest by exact component).
fn junk_pattern(rel: &Path) -> Option<&'static str> {
    for comp in rel.components() {
        let s = comp.as_os_str().to_string_lossy();
        if s == ".Trash" || s.starts_with(".Trash-") {
            return Some(".Trash*");
        }
        for name in JUNK_NAMES {
            if s == *name {
                return Some(name);
            }
        }
    }
    None
}

fn basename_lower(rel: &Path) -> String {
    rel.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn extension_lower(rel: &Path) -> String {
    rel.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_else(|| "(none)".to_string())
}

fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        0
    } else {
        (part.saturating_mul(100)) / whole
    }
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let (mut v, mut i) = (n as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn signed_bytes(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "+" };
    format!("{sign}{}", human_bytes(n.unsigned_abs()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Change;
    use crate::manifest::{Entry, Manifest};

    fn file(rel: &str, size: u64) -> Entry {
        Entry {
            rel: PathBuf::from(rel),
            kind: Kind::File,
            size,
            mtime: None,
            link_target: None,
            link_id: None,
        }
    }
    fn dir(rel: &str) -> Entry {
        Entry { kind: Kind::Dir, ..file(rel, 0) }
    }
    fn removed(rel: &str, kind: Kind) -> Change {
        Change { rel: PathBuf::from(rel), kind }
    }

    #[test]
    fn orphan_deletions_are_those_with_no_source_basename() {
        // dst has two extras; source still has a file named keep.txt (elsewhere) but nothing named
        // vanished.bin → only vanished.bin is an orphan (true content loss).
        let src = Manifest::from_sorted(vec![file("archive/keep.txt", 10)]);
        let dst = Manifest::from_sorted(vec![file("old/keep.txt", 10), file("vanished.bin", 999)]);
        let d = Diff {
            removed: vec![removed("old/keep.txt", Kind::File), removed("vanished.bin", Kind::File)],
            ..Diff::default()
        };
        let c = analyze(&d, &src, &dst);
        assert_eq!(c.orphan_deletes.len(), 1, "only the unmatched name is an orphan");
        assert_eq!(c.orphan_deletes[0].rel, PathBuf::from("vanished.bin"));
        assert_eq!(c.orphan_bytes, 999);
        // 999 has no same-size file in the source (only a 10-byte file) → counted as unique-size
        assert_eq!(c.orphan_unique_size, 1);
    }

    #[test]
    fn whole_deleted_folder_is_one_subtree_and_alarms() {
        // an entire top-level folder (dir + nested dir + files) is being removed
        let src = Manifest::from_sorted(vec![file("stillhere.txt", 1)]);
        let dst = Manifest::from_sorted(vec![
            file("stillhere.txt", 1),
            dir("Photos"),
            dir("Photos/2019"),
            file("Photos/2019/a.jpg", 100),
            file("Photos/2019/b.jpg", 200),
        ]);
        let d = Diff {
            removed: vec![
                removed("Photos", Kind::Dir),
                removed("Photos/2019", Kind::Dir),
                removed("Photos/2019/a.jpg", Kind::File),
                removed("Photos/2019/b.jpg", Kind::File),
            ],
            ..Diff::default()
        };
        let c = analyze(&d, &src, &dst);
        assert_eq!(c.deleted_subtrees.len(), 1, "nested dirs collapse into one subtree root");
        assert_eq!(c.deleted_subtrees[0].root, PathBuf::from("Photos"));
        assert_eq!(c.deleted_subtrees[0].files, 2);
        assert_eq!(c.deleted_subtrees[0].bytes, 300);
        assert!(c.data_loss_alarm(), "a whole top-level folder deletion must raise the alarm");
    }

    #[test]
    fn folder_table_nets_and_junk_are_tallied() {
        let src = Manifest::from_sorted(vec![file("Docs/new.txt", 50)]);
        let dst = Manifest::from_sorted(vec![
            file("Docs/old.txt", 20),
            file(".Trash-1000/junk.bin", 5),
        ]);
        let d = Diff {
            added: vec![Change { rel: PathBuf::from("Docs/new.txt"), kind: Kind::File }],
            removed: vec![
                removed("Docs/old.txt", Kind::File),
                removed(".Trash-1000/junk.bin", Kind::File),
            ],
            ..Diff::default()
        };
        let c = analyze(&d, &src, &dst);

        let docs = c.folders.iter().find(|f| f.folder == "Docs").expect("Docs row");
        assert_eq!((docs.added, docs.removed), (1, 1));
        assert_eq!(docs.net_files, 0, "one in, one out");
        assert_eq!(docs.net_bytes, 30, "+50 added, -20 removed");

        let trash = c.junk.iter().find(|j| j.pattern == ".Trash*").expect("trash tally");
        assert_eq!(trash.deleting_from_dest, 1);
        assert_eq!(trash.copying_from_source, 0);
    }

    #[test]
    fn render_leads_with_the_alarm_and_lists_orphans() {
        let src = Manifest::from_sorted(vec![file("keep.txt", 1)]);
        let dst = Manifest::from_sorted(vec![file("keep.txt", 1), file("Backups/tax2019.pdf", 4096)]);
        let d = Diff {
            removed: vec![removed("Backups", Kind::Dir), removed("Backups/tax2019.pdf", Kind::File)],
            ..Diff::default()
        };
        let out = analyze(&d, &src, &dst).render("/src", "/dst");
        assert!(out.contains("DATA-LOSS WATCH"), "{out}");
        assert!(out.contains("deleted ENTIRELY"), "names the whole-folder deletion:\n{out}");
        assert!(out.contains("tax2019.pdf"), "orphan listed:\n{out}");
    }

    #[test]
    fn empty_files_get_the_distinct_no_move_note() {
        // an empty file added at one path and an empty file deleted at another — same (zero) size,
        // so under the old behavior they'd have been paired as a move; now they're add/delete.
        let src = Manifest::from_sorted(vec![file("keep.txt", 5), file("new_empty", 0)]);
        let dst = Manifest::from_sorted(vec![file("keep.txt", 5), file("old_empty", 0)]);
        let d = Diff {
            added: vec![Change { rel: PathBuf::from("new_empty"), kind: Kind::File }],
            removed: vec![removed("old_empty", Kind::File)],
            ..Diff::default()
        };
        let c = analyze(&d, &src, &dst);
        assert_eq!((c.empty_adds, c.empty_removes), (1, 1));
        let out = c.render("/s", "/d");
        assert!(
            out.contains("NO move was attempted") && out.contains("0-byte"),
            "the distinct size-0 note must appear:\n{out}"
        );
    }

    #[test]
    fn no_deletions_reads_calm() {
        let src = Manifest::from_sorted(vec![file("a.txt", 1)]);
        let dst = Manifest::from_sorted(vec![file("a.txt", 1)]);
        let out = analyze(&Diff::default(), &src, &dst).render("/s", "/d");
        assert!(out.contains("nothing would be deleted"), "{out}");
    }
}
