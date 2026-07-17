//! Compare a source and destination tree into actions: added / removed / changed / moved /
//! unchanged. Move-detection pairs content-identical add/remove files (see `docs/theory.md`).
//!
//! Efficiency: only files whose size appears on *both* the add and remove sides ("contested") can
//! be moves, so nothing else is ever hashed. Contested candidates are hashed each at most once,
//! then matched by content via a hash→queue map (which also handles duplicate content).
//!
//! **Infallible by design**: one unreadable file must not abort the whole run (the same principle
//! `apply` follows). Hash failures degrade safely — a move-candidate falls back to a plain
//! copy/delete, an eager comparison falls back to "changed" (the copy will re-read and verify) —
//! and every degradation is recorded in [`Diff::issues`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::hash;
use crate::links;
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

/// A hard link to create at the destination: `name` becomes another name for `leader`'s inode —
/// the content is written once (via the leader) and never copied for `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub leader: PathBuf,
    pub name: PathBuf,
}

/// The classified difference between a source and a destination tree.
#[derive(Debug, Default)]
pub struct Diff {
    pub added: Vec<Change>,   // in source, missing at dest → copy
    pub removed: Vec<Change>, // extra at dest, not in source → delete
    pub changed: Vec<Change>, // present on both sides but differing → update
    pub moved: Vec<Move>,     // content-identical relocation → rename at dest
    /// Content-identical (hash-verified) but metadata drifted (mtime beyond tolerance) — realized
    /// as a metadata refresh at the destination, never a re-copy.
    pub touched: Vec<Change>,
    /// Hard links to (re)create: follower names of source hard-link groups whose destination
    /// linkage is missing, wrong, or invalidated by a leader re-copy.
    pub to_link: Vec<Link>,
    pub unchanged: usize,
    /// Relative paths of content-identical entries (needing neither a copy nor a move). Populated
    /// only when the caller sets `include_same` — otherwise just `unchanged` is counted, since on a
    /// large tree this list can dwarf every other category.
    pub unchanged_paths: Vec<PathBuf>,
    /// Files that couldn't be examined as intended (hash errors) — the classification degraded
    /// safely instead of aborting the run. Each message names its side. Callers should surface them.
    pub issues: Vec<String>,
    /// True if a **source** file couldn't be read while classifying (a move-candidate or an
    /// eager comparison). This means the source view is incomplete in a way that can endanger
    /// deletions — a would-be move degrades to copy+delete, so a to-be-deleted destination file
    /// might actually be the unreadable source file's content under a new name. Callers treat this
    /// like an unreadable source directory: suspend deletions. (Destination-side read failures
    /// don't set it — they can't cause a wrong deletion.)
    pub source_unreadable: bool,
}

/// Which tree a read failure occurred on — for message labeling and the source-unreadable signal.
#[derive(Clone, Copy)]
enum Side {
    Source,
    Destination,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Side::Source => "source",
            Side::Destination => "destination",
        }
    }
}

/// Classify `src` vs `dst`. `eager` compares files by content hash instead of size+mtime.
/// `relative_symlinks` compares each destination link against the target a copy WOULD write
/// ([`links::desired_target`]) — so a link already rewritten by a previous run is "unchanged".
pub fn diff(
    src: &SrcRoot,
    src_m: &Manifest,
    dst: &DstRoot,
    dst_m: &Manifest,
    eager: bool,
    relative_symlinks: bool,
    include_same: bool,
) -> Diff {
    let dst_by_path: HashMap<&Path, &Entry> = dst_m.iter().map(|e| (e.rel.as_path(), e)).collect();
    let src_by_path: HashMap<&Path, &Entry> = src_m.iter().map(|e| (e.rel.as_path(), e)).collect();

    let mut d = Diff::default();
    let mut add_files: Vec<&Entry> = Vec::new(); // file-only adds — candidates for move-detection
    let mut rm_files: Vec<&Entry> = Vec::new(); // file-only removes — the other side of a move

    // Hard-link groups: the first name (path order) is the LEADER and carries the content through
    // the normal copy/skip/move machinery below; the rest are FOLLOWERS, which are never copied —
    // the linkage pass at the end decides which of them need a (re)link at the destination.
    let groups = src_m.hardlink_groups();
    let followers: HashSet<&Path> =
        groups.iter().flat_map(|g| g[1..].iter().map(|e| e.rel.as_path())).collect();

    for se in src_m.iter() {
        // A follower's content is its leader's business. Here we only make sure a wrong-kind
        // occupant at the follower's destination path gets cleared; whether a link is needed is
        // decided by the linkage pass.
        if se.kind == Kind::File && followers.contains(se.rel.as_path()) {
            if let Some(de) = dst_by_path.get(se.rel.as_path()) {
                if de.kind != Kind::File {
                    d.removed.push(Change { rel: de.rel.clone(), kind: de.kind });
                }
            }
            continue;
        }
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
            Some(de) => match compare_entries(
                src,
                se,
                dst,
                de,
                eager,
                relative_symlinks,
                &mut d.issues,
                &mut d.source_unreadable,
            ) {
                Verdict::Unchanged => {
                    d.unchanged += 1;
                    if include_same {
                        d.unchanged_paths.push(se.rel.clone());
                    }
                }
                Verdict::Touched => d.touched.push(Change { rel: se.rel.clone(), kind: se.kind }),
                Verdict::Changed => d.changed.push(Change { rel: se.rel.clone(), kind: se.kind }),
            },
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

    detect_moves(src, &add_files, dst, &rm_files, &mut d);

    // Linkage pass. THE trap this must never fall into: a re-copied leader lands via atomic
    // temp+rename, which creates a NEW destination inode — existing links at follower names keep
    // pointing at the OLD inode and would silently serve stale bytes. So a follower needs a
    // (re)link when its leader's destination inode will be new this run (added/changed/moved-in),
    // OR when the destination doesn't currently hold both names on one inode.
    let rewritten: HashSet<&Path> = d
        .added
        .iter()
        .chain(d.changed.iter())
        .map(|c| c.rel.as_path())
        .chain(d.moved.iter().map(|m| m.to.as_path()))
        .collect();
    for group in &groups {
        let leader = group[0];
        let leader_rewritten = rewritten.contains(leader.rel.as_path());
        let dst_leader_id = dst_by_path.get(leader.rel.as_path()).and_then(|e| e.link_id);
        for f in &group[1..] {
            let properly_linked = !leader_rewritten
                && match (dst_by_path.get(f.rel.as_path()), dst_leader_id) {
                    (Some(df), Some(lid)) => df.kind == Kind::File && df.link_id == Some(lid),
                    _ => false,
                };
            if properly_linked {
                d.unchanged += 1;
                if include_same {
                    d.unchanged_paths.push(f.rel.clone());
                }
            } else {
                d.to_link.push(Link { leader: leader.rel.clone(), name: f.rel.clone() });
            }
        }
    }

    d.added.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.removed.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.changed.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.touched.sort_by(|a, b| a.rel.cmp(&b.rel));
    d.moved.sort_by(|a, b| a.to.cmp(&b.to));
    d.to_link.sort_by(|a, b| a.name.cmp(&b.name));
    d.unchanged_paths.sort();
    d
}

enum Verdict {
    Unchanged,
    /// Content identical (hash-verified), metadata drifted — refresh, don't re-copy.
    Touched,
    Changed,
}

/// Same path, same kind: has the entry changed? Two safety properties baked in:
/// - **Never destroy on a shallow signal.** Before a same-size file is declared changed on mtime
///   alone (and its destination version overwritten), both sides are hashed. Identical content ⇒
///   [`Verdict::Touched`] (metadata refresh, no write, nothing destroyed).
/// - **Degrade, don't abort.** When content can't be compared (hash error), the verdict is
///   Changed — copying is the safe direction — and the failure is recorded in `issues`.
#[allow(clippy::too_many_arguments)]
fn compare_entries(
    src: &SrcRoot,
    se: &Entry,
    dst: &DstRoot,
    de: &Entry,
    eager: bool,
    relative_symlinks: bool,
    issues: &mut Vec<String>,
    source_unreadable: &mut bool,
) -> Verdict {
    if se.kind != de.kind {
        return Verdict::Changed;
    }
    match se.kind {
        Kind::Dir => Verdict::Unchanged, // dir metadata is aligned by apply, not classified here
        Kind::Symlink => match (&se.link_target, &de.link_target) {
            // compare against what a copy would WRITE, so rewritten links register as unchanged
            (Some(st), Some(dt)) => {
                if &links::desired_target(src, &se.rel, st, relative_symlinks) == dt {
                    Verdict::Unchanged
                } else {
                    Verdict::Changed
                }
            }
            (None, None) => Verdict::Unchanged,
            _ => Verdict::Changed,
        },
        // Specials carry no copyable content — same kind at the same path means nothing to do.
        Kind::Other => Verdict::Unchanged,
        Kind::File => {
            if se.size != de.size {
                return Verdict::Changed; // different sizes can't be identical — no reads needed
            }
            let same_mtime = mtime_close(se.mtime, de.mtime);
            if !eager && same_mtime {
                return Verdict::Unchanged; // the fast path: size + mtime agree
            }
            // Same size but mtime drifted (or eager mode): decide by content.
            let (sh, dh) = (
                hash::hash_file(&src.path().join(&se.rel)),
                hash::hash_file(&dst.path().join(&de.rel)),
            );
            match (sh, dh) {
                (Ok(a), Ok(b)) if a == b => {
                    if same_mtime {
                        Verdict::Unchanged
                    } else {
                        Verdict::Touched // identical bytes; only the mtime needs aligning
                    }
                }
                (Ok(_), Ok(_)) => {
                    if same_mtime {
                        // Only reachable in eager mode: content differs although size AND mtime
                        // match — an mtime-preserving edit, or corruption on one side.
                        issues.push(format!(
                            "{}: content differs although size and mtime match — possible \
                             corruption on one side (or an mtime-preserving edit); will re-copy \
                             from the source",
                            se.rel.display()
                        ));
                    }
                    Verdict::Changed
                }
                // Source read failure is the consequential one — it marks the view incomplete
                // (see Diff::source_unreadable). Check it first so it wins if both sides fail.
                (Err(e), _) => {
                    *source_unreadable = true;
                    issues.push(format!(
                        "source: {}: cannot read to compare content ({e}); treating as changed",
                        se.rel.display()
                    ));
                    Verdict::Changed
                }
                (_, Err(e)) => {
                    issues.push(format!(
                        "destination: {}: cannot read to compare content ({e}); treating as changed",
                        se.rel.display()
                    ));
                    Verdict::Changed
                }
            }
        }
    }
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
/// hashing of only the contested candidates + hash→queue matching (dup-content safe). A candidate
/// that can't be hashed simply isn't a move (falls back to plain copy/delete, with an issue).
fn detect_moves(
    src: &SrcRoot,
    add_files: &[&Entry],
    dst: &DstRoot,
    rm_files: &[&Entry],
    d: &mut Diff,
) {
    let add_sizes: HashSet<u64> = add_files.iter().map(|e| e.size).collect();
    let rem_sizes: HashSet<u64> = rm_files.iter().map(|e| e.size).collect();
    let contested: HashSet<u64> = add_sizes.intersection(&rem_sizes).copied().collect();

    // Indices of files worth hashing: only those whose size appears on both sides.
    let add_cand: Vec<usize> =
        (0..add_files.len()).filter(|&i| contested.contains(&add_files[i].size)).collect();
    let rem_cand: Vec<usize> =
        (0..rm_files.len()).filter(|&i| contested.contains(&rm_files[i].size)).collect();

    let add_hashes =
        hash_candidates(src.path(), Side::Source, add_cand, add_files, &mut d.issues, &mut d.source_unreadable);
    let rem_hashes =
        hash_candidates(dst.path(), Side::Destination, rem_cand, rm_files, &mut d.issues, &mut d.source_unreadable);

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
}

/// Hash the candidate files (by index into `files`) under `root` on `side`. Failures don't
/// propagate: the candidate is dropped from move-matching (→ plain copy/delete), the error is
/// recorded (labeled with its side), and a source-side failure sets `source_unreadable` — a
/// would-be move degrading to copy+delete could otherwise delete a destination file that is
/// actually this unreadable source file's content under a new name.
fn hash_candidates(
    root: &Path,
    side: Side,
    cand: Vec<usize>,
    files: &[&Entry],
    issues: &mut Vec<String>,
    source_unreadable: &mut bool,
) -> Vec<(usize, [u8; 32])> {
    let mut out = Vec::with_capacity(cand.len());
    for i in cand {
        match hash::hash_file(&root.join(&files[i].rel)) {
            Ok(h) => out.push((i, *h.as_bytes())),
            Err(e) => {
                if matches!(side, Side::Source) {
                    *source_unreadable = true;
                }
                issues.push(format!(
                    "{}: {}: cannot hash for move-detection ({e}); treating as a plain copy/delete",
                    side.label(),
                    files[i].rel.display()
                ));
            }
        }
    }
    out
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

    /// A git-diff-like textual summary. `detail` controls whether the per-file lines are included:
    /// the findings file gets the full listing (`true`); the terminal gets only the count lines
    /// (`false`), so a diff of a huge tree never floods the screen — the detail is in the file.
    pub fn render(&self, detail: bool) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "moved:     {}", self.moved.len());
        if detail {
            for m in &self.moved {
                let _ = writeln!(s, "    ~ {}  ->  {}", m.from.display(), m.to.display());
            }
        }
        let _ = writeln!(s, "to copy:   {}", self.added.len());
        if detail {
            for c in &self.added {
                if c.kind == Kind::Other {
                    let _ = writeln!(s, "    + {} (special file — no content; will be skipped)", c.rel.display());
                } else {
                    let _ = writeln!(s, "    + {}", c.rel.display());
                }
            }
        }
        let _ = writeln!(s, "to delete: {}", self.removed.len());
        if detail {
            for c in &self.removed {
                let _ = writeln!(s, "    - {}", c.rel.display());
            }
        }
        let _ = writeln!(s, "to update: {}", self.changed.len());
        if detail {
            for c in &self.changed {
                let _ = writeln!(s, "    * {}", c.rel.display());
            }
        }
        let _ = writeln!(s, "to link:   {} (hard links — content written once via the leader)", self.to_link.len());
        if detail {
            for l in &self.to_link {
                let _ = writeln!(s, "    & {}  ->  {}", l.name.display(), l.leader.display());
            }
        }
        let _ = writeln!(s, "to refresh (content identical, metadata drift): {}", self.touched.len());
        if detail {
            for c in &self.touched {
                let _ = writeln!(s, "    ≈ {}", c.rel.display());
            }
        }
        let _ = writeln!(s, "unchanged: {}", self.unchanged);
        if detail {
            // Only ever populated under --include-same; listed here so the exhaustive findings
            // account for every entry, not just the ones that change.
            for rel in &self.unchanged_paths {
                let _ = writeln!(s, "    = {}", rel.display());
            }
        }
        s
    }
}
