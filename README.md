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

# Sync, but keep anything deleted/overwritten in a recovery folder instead of erasing it:
filesync sync --from ~/Documents --to /mnt/backup --backup-dir /mnt/backup/.filesync-trash
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
| `--eager-checksum` | both | off | Compare by file **content** (blake3) instead of size+mtime. For a thorough check, or to never miss a same-size+same-mtime change. |
| `--report <PATH>` | both | see below | Where to write the report. |
| `--no-verify` | `sync` | verify **on** | Skip re-reading each copied file to confirm it landed correctly. |
| `--fsync-each` | `sync` | off | Force every file to disk individually (durable-as-you-go, but ~2–17× slower). Default is one flush at the end. |
| `--backup-dir <DIR>` | `sync` | — | Move files that would be deleted or overwritten here, instead of erasing them. |

Behavior that's always on (no flags): move-detection, mirror/delete-extras, symlinks replicated as
symlinks, permissions and mtimes preserved where the destination filesystem supports them, and no
confirmation prompts. If you *don't* want deletion, a normal copy tool is the right choice — mirror
fidelity is the point of filesync.

## The report

Every run always writes a report to the current directory:

```
./filesync-report-<source-folder-name>-<YYYY-mm-DD_HHMM>.txt
```

(for example `filesync-report-Documents-2026-07-04_1530.txt`), and prints a summary to the screen.
`--report <PATH>` overrides the location. The report is written to the **current directory** — never
into the source (which is read-only) or the destination (which would pollute the mirror) — and it's
written as the run progresses, so even an interrupted run leaves a usable report.

## Safety guarantees

- **Source is read-only** — enforced in the code (source and destination are distinct types; only
  the destination can be written or deleted).
- **Atomic writes** — each file is written to a temp file and atomically renamed into place, so an
  interruption can never leave a half-written file masquerading as a real one.
- **Resumable** — a re-run re-compares and only redoes what didn't complete; no persistent state to
  corrupt.
- **Verified** — by default each copied file is re-read from the device and hash-checked against the
  source (`--no-verify` to skip).
- **Recoverable deletes** — with `--backup-dir`, nothing is truly erased.
