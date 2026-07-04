# Archived plan — rsync-orchestration approach

**Status: SET ASIDE (2026-07-03).** Kept for reference in case we revisit orchestrating an
external tool. The project has since chosen to implement copying in **pure Rust with no
OS-level dependency**, with external/removable devices as the primary use-case and a strong
emphasis on tests + benchmarks. See the active design notes for the current direction.

## Why this was set aside
- Requirement: a self-contained Rust program that performs the copy itself — no reliance on an
  external `rsync` binary being installed on the host.
- Primary use-case: syncing to detachable/external storage, where we want full control over
  atomicity, interruption safety, and filesystem-quirk handling.

## The goals it was designed for (unchanged, still apply)
1. Copy only files not already at the target, verified by **identity (content), not name**.
2. Flow: git-diff-like report (new / missing / moved) → sync SRC→DST → issues report.
3. Allow quitting mid-work; fast discovery; minimal risk of data loss / incomplete copy /
   corruption / deleting the wrong files.

## Decisions that shaped it (some still valid, some superseded)
- **Priority: safety / data integrity first.** (still valid)
- **Manifest = throwaway comparison artifact**, computed in memory; no persistent tracking file.
  This deletes the old no-truncation corruption bug outright (nothing is written). (still valid)
- **Identity via `blake3` content hash**; `mtime` alone is unreliable across filesystems.
  (still valid)
- **Move-detection for all files, any size, via blake3** — a renamed file is executed as a local
  rename at the destination instead of re-copying. Motivated by the read-vs-write asymmetry of
  storage (reads are typically several times faster than writes; hashing to avoid a re-copy is a
  large win for big files). (still valid)
- **Mirror + hard delete, done early** to free space on a possibly-too-small target; compensate
  the loss of a safety net with a mandatory preview + confirmation gate before any destructive
  op. (still valid)
- **`--checksum` is an opt-in flag**: default fast quick-check (size + mtime with tolerance),
  `--checksum` for a paranoid content-verified pass. (still valid)
- **Copy engine = orchestrate `rsync`.** (SUPERSEDED — now hand-rolled in Rust.)

## The rsync architecture (the superseded part)
"Arch 1" — least reinvention:
1. Walk both trees into in-memory manifests; blake3-hash to pair moves (new-at-source whose
   size+hash matches an extra-at-dest).
2. Execute confirmed renames at the destination early (our code, behind the confirmation gate).
3. Run `rsync -a --delete-before [--checksum] SRC/ DST/` — rsync skips the already-renamed files,
   deletes remaining extras first (frees space), copies new/changed preserving timestamps.
4. Parse rsync's itemized output + exit code into the issues report.

Proposed CLI:
```
filesync diff SRC DST [--checksum] [--prefix P]...
filesync sync SRC DST [--checksum] [--prefix P]... [--dry-run] [--yes]
```

Phases: (0) stabilize/clean warnings, (1) in-memory manifest + hashing, (2) diff + report,
(3) sync via rsync + issues report.

## What carries over to the pure-Rust design
Everything above marked "(still valid)". Only step 3 of the architecture changes: instead of
shelling out to rsync, we implement the copy/delete/rename/verify pipeline ourselves in Rust,
with atomic per-file writes, fsync, timestamp preservation, interruption safety, and explicit
handling of removable-filesystem limitations (FAT/exFAT/NTFS).
