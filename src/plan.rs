//! Turn a [`Diff`] into an ordered list of concrete destination [`Action`]s.
//!
//! This is a **pure function** — it performs no I/O, so it's fully unit-testable by asserting the
//! action list. [`crate::apply`] executes the actions.
//!
//! Order encodes "destructive/space-freeing work first, writes last", and delete-before-create so a
//! path whose *kind* changed (file↔dir) is cleared before the new version is placed:
//!   1. **Rename** — carry out detected moves (a `mv` *within* the destination; no bytes copied).
//!   2. **Delete** the extras — children before parents, so directories are empty when removed.
//!      Frees space, and clears any wrong-kind entry before step 3/4 recreates that path.
//!   3. **CreateDir** for new directories (parents before children).
//!   4. **Copy** the new/changed files — the space-consuming writes, last.

use std::path::PathBuf;

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
}

/// Produce the ordered action list for realizing `diff` on the destination.
pub fn plan(diff: &Diff) -> Vec<Action> {
    let mut actions = Vec::new();

    // 1. Renames (the detected moves) — first, so a moved file isn't deleted/re-copied and it
    //    leaves its old directory empty for removal. Already ordered by destination path.
    for m in &diff.moved {
        actions.push(Action::Rename { from: m.from.clone(), to: m.to.clone() });
    }

    // 2. Deletes — children before parents (reverse sort), so a directory is empty when removed.
    let mut dels: Vec<PathBuf> = diff.removed.iter().map(|c| c.rel.clone()).collect();
    dels.sort();
    dels.reverse();
    actions.extend(dels.into_iter().map(Action::Delete));

    // 3. Added directories, parents before children (ascending sort).
    let mut new_dirs: Vec<PathBuf> =
        diff.added.iter().filter(|c| c.kind == Kind::Dir).map(|c| c.rel.clone()).collect();
    new_dirs.sort();
    actions.extend(new_dirs.into_iter().map(Action::CreateDir));

    // 4. Copies — new non-directory entries plus changed entries. Writes happen last.
    let mut copies: Vec<PathBuf> = diff
        .added
        .iter()
        .filter(|c| c.kind != Kind::Dir)
        .chain(diff.changed.iter())
        .map(|c| c.rel.clone())
        .collect();
    copies.sort();
    actions.extend(copies.into_iter().map(Action::Copy));

    actions
}
