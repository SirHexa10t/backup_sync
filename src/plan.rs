//! Turn a [`Diff`] into an ordered list of concrete destination [`Action`]s.
//!
//! This is a **pure function** — it performs no I/O, so it's fully unit-testable by asserting the
//! action list. [`crate::apply`] executes the actions.
//!
//! Order encodes "destructive/space-freeing work first, writes last", and delete-before-create so a
//! path whose *kind* changed (file↔dir) is cleared before the new version is placed:
//!   1. **Pre-delete blocked move targets** — if a detected move's target path is currently
//!      occupied by a doomed wrong-kind entry (e.g. a to-be-deleted directory), that subtree is
//!      deleted first, so the rename can't fail with "target is a directory".
//!   2. **Rename** — carry out detected moves (a `mv` *within* the destination; no bytes copied).
//!   3. **Delete** the remaining extras — children before parents, so directories are empty when
//!      removed. Frees space, and clears any wrong-kind entry before step 4/5 recreates that path.
//!   4. **CreateDir** for new directories (parents before children).
//!   5. **Copy** the new/changed files — the space-consuming writes, last.

use std::path::{Path, PathBuf};

use crate::diff::Diff;
use crate::manifest::Kind;

/// A concrete operation on the destination. Paths are relative to the destination root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    CreateDir(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
    Delete(PathBuf),
    /// Reproduce the source entry at this path (copy a file, or recreate a symlink).
    Copy(PathBuf),
    /// Align mtime/permissions with the source — content was hash-verified identical, so no
    /// bytes move (and the next run's quick check will see the file as unchanged).
    RefreshMeta(PathBuf),
    /// Make `name` another name for `leader`'s inode (a source hard-link group mirrored as a
    /// hard link — content written once, via the leader).
    HardLink { leader: PathBuf, name: PathBuf },
}

/// Produce the ordered action list for realizing `diff` on the destination.
pub fn plan(diff: &Diff) -> Vec<Action> {
    let mut actions = Vec::new();

    // 1. Pre-deletes: removed entries sitting at (or under) a move's target path would make the
    //    rename fail — clear exactly those subtrees first, children before parents. They still go
    //    through Delete, so `--backup-dir` moves them aside like any other deletion.
    let mut dels: Vec<PathBuf> = diff.removed.iter().map(|c| c.rel.clone()).collect();
    let blocking: Vec<PathBuf> = {
        let mut v: Vec<PathBuf> = dels
            .iter()
            .filter(|rel| diff.moved.iter().any(|m| rel.starts_with(&m.to)))
            .cloned()
            .collect();
        v.sort();
        v.reverse(); // children before parents
        v
    };
    dels.retain(|rel| !blocking.contains(rel));
    actions.extend(blocking.into_iter().map(Action::Delete));

    // 2. Renames (the detected moves) — before the remaining deletes, so a moved file isn't
    //    deleted/re-copied and it leaves its old directory empty for removal. Already ordered by
    //    destination path.
    for m in &diff.moved {
        actions.push(Action::Rename { from: m.from.clone(), to: m.to.clone() });
    }

    // 3. Deletes — children before parents (reverse sort), so a directory is empty when removed.
    dels.sort();
    dels.reverse();
    actions.extend(dels.into_iter().map(Action::Delete));

    // 4. Added directories, parents before children (ascending sort).
    let mut new_dirs: Vec<PathBuf> =
        diff.added.iter().filter(|c| c.kind == Kind::Dir).map(|c| c.rel.clone()).collect();
    new_dirs.sort();
    actions.extend(new_dirs.into_iter().map(Action::CreateDir));

    // 5. Copies — new non-directory entries plus changed entries. Writes happen last.
    let mut copies: Vec<PathBuf> = diff
        .added
        .iter()
        .filter(|c| c.kind != Kind::Dir)
        .chain(diff.changed.iter())
        .map(|c| c.rel.clone())
        .collect();
    copies.sort();
    actions.extend(copies.into_iter().map(Action::Copy));

    // 6. Metadata refreshes for content-identical files — cheap inode updates, no data writes.
    actions.extend(diff.touched.iter().map(|c| Action::RefreshMeta(c.rel.clone())));

    // 7. Hard links — strictly after the copies, so every leader already exists at the
    //    destination with its FINAL inode (a re-copied leader gets a new inode; linking earlier
    //    would attach followers to the doomed old one).
    actions.extend(
        diff.to_link
            .iter()
            .map(|l| Action::HardLink { leader: l.leader.clone(), name: l.name.clone() }),
    );

    actions
}

/// Total bytes the planned `Copy` actions will write (source sizes; symlinks count as 0) — for
/// the progress bar's length and the suspended-deletions space look-ahead.
pub(crate) fn planned_copy_bytes(actions: &[Action], src_m: &crate::manifest::Manifest) -> u64 {
    let sizes: std::collections::HashMap<&Path, u64> =
        src_m.iter().map(|e| (e.rel.as_path(), e.size)).collect();
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Copy(rel) => sizes.get(rel.as_path()).copied(),
            _ => None,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Entry, Manifest};

    #[test]
    fn planned_copy_bytes_sums_only_copy_actions() {
        let entry = |rel: &str, size: u64| Entry {
            rel: PathBuf::from(rel),
            kind: Kind::File,
            size,
            mtime: None,
            link_target: None,
            link_id: None,
            owner: None,
            mode: None,
        };
        let m = Manifest::from_sorted(vec![entry("a", 100), entry("b", 7)]);
        let actions = vec![
            Action::Copy(PathBuf::from("a")),
            Action::Delete(PathBuf::from("x")),
            Action::Copy(PathBuf::from("b")),
            Action::Copy(PathBuf::from("not-in-manifest")),
        ];
        assert_eq!(planned_copy_bytes(&actions, &m), 107);
    }
}
