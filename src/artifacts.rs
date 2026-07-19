//! The names of filesync's OWN on-disk artifacts — the things it writes that are never user data.
//!
//! They live together because one rule binds them: **scans must treat them as scratch, not
//! content** — never mirrored, never mirror-deleted, never backed up. Keeping the names (and that
//! rule) in one neutral module also keeps the read layer (`scan`) from depending on the write
//! layer (`apply`/`lock`) just to learn a filename.

/// Prefix for the temp files that atomic copies write before renaming into place. Deliberately
/// long and specific: scans silently ignore names with this prefix (they're our scratch), so the
/// odds of colliding with real user data must stay astronomically small. A destination scan also
/// sweeps strays left by an interrupted run.
pub const TMP_PREFIX: &str = ".filesync_staging.tmp.";

/// Marker file dropped inside a `--backup-dir` on first use. A directory containing this file is
/// filesync's own move-aside storage: scans exclude it, so a backup dir living inside the
/// destination is never mirrored, deleted, or re-backed-up by later runs — and a used backup dir
/// is recognizable, so it can't be accidentally reused.
pub const BACKUP_MARKER: &str = ".filesync-backup-dir";

/// The destination-root lockfile (PID-bearing) enforcing one sync per destination. The running
/// sync's own artifact: released on exit, and never treated as destination content.
pub const LOCK_FILE: &str = ".filesync.lock";
