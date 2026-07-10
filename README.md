# filesync

Cheaply and reliably mirror one directory onto another — so you don't lose your files when
hardware fails. That's all it does: no more, no less.

It's built for the "back up to an external/USB drive and walk away" case. A large sync can take a
night or a weekend, so filesync runs **unattended — no prompts** — and is **resumable**: if it's
interrupted (drive unplugged, power loss, Ctrl-C), just run it again and it picks up where it left
off. The source is treated **strictly read-only**; every change happens on the destination.

> **Status:** in active development. This README describes the target design and CLI; the rationale
> and the measurements behind it are in [`docs/theory.md`](docs/theory.md).

## What you do with it

Two things:

- **`sync`** — do the job: make the destination match the source.
- **`diff`** — preview the job: see what a sync *would* change, without touching anything. Use this
  when you want to check before acting.

And a cherry on top: **a report**. A normal copy just moves bytes silently; filesync tells you what
actually happened — what was new, changed, or *moved*, and anything that needs your attention
(files it had to skip, filesystem limits it hit, etc.). You get the fine details without having to
sit and approve each file.

## How it decides what to do

filesync compares the two trees and does the minimum necessary — because **writing is the slow,
device-wearing operation, so the goal is to write as little as possible**:

- **Already there and identical?** Skip it.
- **New or changed?** Copy it.
- **Moved** (same content, different path)? **Rename it in place on the destination** instead of
  re-copying — no data transfer, no wear. This is filesync's core trick.
- **Gone from the source?** Delete it from the destination (a true mirror, no stale leftovers).

Identity is by **content** (a blake3 hash), not by name — a renamed file is recognized as the same
file. See [`docs/theory.md`](docs/theory.md) for the move-detection algorithm and the benchmark
data that shaped these choices.

## Usage

```sh
# Preview what a sync would change (touches nothing):
filesync diff --from ~/Documents --to /mnt/backup

# Do the sync, then walk away:
filesync sync --from ~/Documents --to /mnt/backup

# Deep, content-based comparison (blake3) instead of the fast size+mtime check:
filesync diff --from ~/Documents --to /mnt/backup --eager-checksum

# Sync, but keep anything deleted/overwritten in a recovery folder instead of erasing it
# (the folder may live inside the destination — filesync marks it and later runs leave it alone):
filesync sync --from ~/Documents --to /mnt/backup --backup-dir /mnt/backup/.trash-2026-07-09
```

### Commands

| Command | What it does |
|---------|--------------|
| `sync`  | Make `--to` mirror `--from`: copy new/changed, rename moves, delete extras. Resumable, unattended. |
| `diff`  | Report what `sync` would do (new / changed / moved / deleted). Changes nothing. |

### Options

| Option | Applies to | Default | Meaning |
|--------|-----------|---------|---------|
| `--from <DIR>` | both | — (required) | Source directory. **Read-only** — never modified. |
| `--to <DIR>` | both | — (required) | Destination directory. |
| `--eager-checksum` | both | off | Compare by file **content** (blake3) instead of size+mtime. For a thorough check, or to never miss a same-size+same-mtime change. **Re-running with this flag after a completed backup is how you hunt corruption**: a verified mirror that now differs where size and mtime still match means bytes rotted on one side — filesync calls that signature out in the report and re-copies from the source (pair with `--backup-dir` so the destination's old version survives, in case the *source* was the rotten side). |
| `--report <PATH>` | both | see below | Where to write the report. |
| `--no-verify` | `sync` | verify **on** | Skip re-reading each copied file to confirm it landed correctly. |
| `--fsync-each` | `sync` | off | Force every file to disk individually (durable-as-you-go, but ~2–17× slower). Default is one flush at the end. |
| `--backup-dir <DIR>` | `sync` | — | Move files that would be deleted or overwritten here, instead of erasing them. Must be a **fresh** dir (absent or empty) on the **destination's filesystem**, not inside the source — one backup dir per run. It may live inside the destination: filesync marks it with a `.filesync-backup-dir` file, and never mirrors, deletes, or re-backs-up a marked dir. |
| `--relative-symlinks` | both | off | Rewrite symlinks whose fully-resolved target lies **inside the source** (chained links and `..` are seen through; the target need not exist) so they point at the mirrored location, as relative paths — a self-contained backup. Links resolving outside the source are copied verbatim; dangling links are copied and noted. `diff` previews the rewrite the same way. |

Behavior that's always on (no flags): move-detection, mirror/delete-extras, symlinks replicated as
symlinks, **hard-link groups mirrored as hard links** (the content is written once; the other
names are linked at the destination — and re-linked whenever the content is re-copied, so no name
ever serves stale bytes; where the destination can't hold links, they fall back to independent
copies with a note), file **and directory** permissions/mtimes mirrored where the destination
filesystem supports them, a live progress bar on the terminal, one-sync-per-destination locking,
and no confirmation prompts. A same-size file whose mtime drifted is **hash-checked before being
overwritten** — identical content just gets its metadata realigned (`refreshed` in the report), so
nothing is destroyed, and nothing is re-copied, on a shallow signal. If you *don't* want deletion,
a normal copy tool is the right choice — mirror fidelity is the point of filesync. If the
destination fills up mid-run, the remaining copies are skipped with a single clear message instead
of grinding through failures.

Special files (fifos, sockets, device nodes) have no copyable content — they're rendezvous points
for live processes, not data. They're listed under **`skipped`** in the report and do **not** fail
the run; every skip is visible, never silent. Symlinks a destination filesystem can't hold (FAT)
are different: a link *does* carry information, so each one is reported as an issue **with its
target path recorded**, letting you reconstruct them later elsewhere.

**Windows note:** the default end-of-run durability barrier can't persist renames on Windows
(directories can't be flushed there), so filesync refuses to run without `--fsync-each`.

## The report

Every run always writes a report to the current directory:

```
./filesync-report-<source-folder-name>-<YYYY-mm-DD_HHMM>.txt
```

(for example `filesync-report-Documents-2026-07-04_1530.txt`), and prints a summary to the screen.
`--report <PATH>` overrides the location. The report must lie **outside both trees**, and filesync
refuses to start otherwise: inside the source it would write into a read-only tree, and inside the
destination the next run would mirror-delete it. (This includes the default location — running from
a directory inside the source/destination is caught too.) An existing report is never overwritten —
a colliding name gets a `-2`/`-3` suffix. The report is written as the run progresses, so even an
interrupted run leaves a usable record; a completed one ends with a `run completed` line, so a
report cut short by an interruption is recognizable.

## Safety guarantees

- **Source is read-only** — enforced in the code (source and destination are distinct types; only
  the destination can be written or deleted), and guarded at startup: overlapping source/destination
  paths, a backup dir inside the source, and `/` as either end are all rejected.
- **Deletions require a fully-readable source** — if any part of the source can't be read, a file
  hidden behind the error would look "deleted" and its destination twin (possibly the last copy)
  would be mirror-deleted. Instead, **all deletions are suspended for that run** (copies and renames
  still proceed), and the report says so. An entirely empty source is likewise refused — it usually
  means a drive didn't mount.
- **Atomic writes** — each file is written to a temp file and atomically renamed into place, so an
  interruption can never leave a half-written file masquerading as a real one.
- **Resumable** — a re-run re-compares and only redoes what didn't complete; no persistent state to
  corrupt.
- **Verified, and corrected** — by default each copied file is re-read from the device and
  hash-checked against the source (`--no-verify` to skip). A copy that fails the check is reported
  and **removed** — a corrupt copy looks "unchanged" to later runs (same size and mtime), so leaving
  it would hide the damage forever; removed, the next run simply re-copies it. A `--no-verify` run
  can be healed later by re-running with `--eager-checksum`.
- **Recoverable deletes** — with `--backup-dir`, nothing is truly erased.

## Known limitations

- **A silently damaged source file is indistinguishable from an edited one.** filesync keeps no
  persistent hash database (nothing on disk to corrupt), so a source file that still *reads* fine
  but whose bytes rotted looks like an edit and gets mirrored. The layered mitigations — deletion
  suspension when the source isn't fully readable, hash-before-overwrite, and the
  `--eager-checksum` corruption hunt (with `--backup-dir` as the safety net) — are described in
  [`docs/theory.md`](docs/theory.md), "Source trust and reachability".
- **Names beginning with `.filesync_staging.tmp.` are treated as filesync's own scratch.** A user
  file in the source carrying that exact prefix would be silently skipped, and one at the
  destination swept. The prefix is deliberately long and specific to make a real collision
  astronomically unlikely.
- **Root-level backups are rejected** (`--from /` and `--to /`). Scanning `/` would descend into
  every mount — including the destination itself, which would then be mirrored into itself and
  mirror-deleted — plus pseudo-filesystems like `/proc`. Doing this properly needs
  exclude / one-filesystem support, which is deliberately out of scope. Backing up a directory
  that merely *contains* mount points works fine.
- **Windows requires `--fsync-each`.** The default end-of-run durability barrier can't persist
  renames there (directories can't be flushed through the standard library), so filesync refuses
  to run rather than promise durability it can't deliver.
