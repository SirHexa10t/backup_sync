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
sit and approve each file — and `diff` adds a **conclusions** file that flags the alarming parts up
front, like a whole folder about to be deleted because it's no longer in the source.

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
| `--report <DIR>` | both | current dir | Existing directory to write this run's output files into (see [The report](#the-report)). Must be outside both trees. |
| `--include-same` | `diff` | off | Also list content-identical files (needing neither copy nor move) in the findings. Off by default — the list can be enormous. |
| `--no-verify` | `sync` | verify **on** | Skip re-reading each copied file to confirm it landed correctly. |
| `--fsync-each` | `sync` | off | Force every file to disk individually (durable-as-you-go, but ~2–17× slower). Default is one flush at the end. |
| `--backup-dir <DIR>` | `sync` | — | Move files that would be deleted or overwritten here, instead of erasing them. Must be a **fresh** dir (absent or empty) on the **destination's filesystem**, not inside the source — one backup dir per run. It may live inside the destination: filesync marks it with a `.filesync-backup-dir` file, and never mirrors, deletes, or re-backs-up a marked dir. |
| `--relative-symlinks` | both | off | Rewrite symlinks whose fully-resolved target lies **inside the source** (chained links and `..` are seen through; the target need not exist) so they point at the mirrored location, as relative paths — a self-contained backup. Links resolving outside the source are copied verbatim; dangling links are copied and noted. `diff` previews the rewrite the same way. |

Behavior that's always on (no flags): move-detection, mirror/delete-extras, symlinks replicated as
symlinks, **hard-link groups mirrored as hard links** (the content is written once; the other
names are linked at the destination — and re-linked whenever the content is re-copied, so no name
ever serves stale bytes; where the destination can't hold links, they fall back to independent
copies with a note), file **and directory** permissions/mtimes mirrored where the destination
filesystem supports them, **concurrent scanning of source and destination when they're on different
devices** (they use independent I/O paths and the CPU isn't the bottleneck; same-device stays
sequential), live progress on the terminal (per-device scan counters with entries + bytes, then a
compare spinner while it hashes for move-detection, then a copy/verify bar), one-sync-per-destination
locking, and no confirmation prompts.
When output is redirected (cron logs), the live displays are replaced by occasional plain
heartbeat lines and per-scan summaries — an overnight run stays visibly alive in its log. A same-size file whose mtime drifted is **hash-checked before being
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

Every run writes its output to files in the current directory, so a huge listing never scrolls off
the screen and live progress never bleeds into a redirected file. filesync routes its kinds of
output by *meaning* — which shell redirection can't, since errors and progress would otherwise share
stderr — into files plus the terminal. The files share one timestamped stem:

```
./filesync-<command>-<source>-<YYYY-mm-DD_HHMM>.findings.txt      # what would change / did change
./filesync-<command>-<source>-<YYYY-mm-DD_HHMM>.errors.txt        # issues — only if any
./filesync-<command>-<source>-<YYYY-mm-DD_HHMM>.conclusions.txt   # diagnostics (diff only)
```

(for example `filesync-sync-Documents-2026-07-04_1530.findings.txt`). `<command>` is `sync` or `diff`:

- The **findings** file is the report. `sync` records what it did (the counts, plus any benign
  skips); `diff` writes the full new/changed/moved/deleted listing — however large — and prints only
  a compact count summary to the screen. By default `diff` omits content-identical files (they're
  counted, not listed); `--include-same` lists them too.
- The **`.errors.txt`** companion holds anything needing your attention, one issue per line, each
  labeled with its side (`source:` / `destination:`). It's created **only if there's at least one
  issue** — so *no errors file means a clean run*.
- The **`.conclusions.txt`** file (written by `diff`) distils the diff into the few things worth
  looking at — see [Conclusions](#conclusions) below.
- **Live progress** (`… scanned …`) stays on the terminal only; it is never written to any file.

`--report <DIR>` chooses the directory to write these files into (they keep their generated names);
it must be an existing directory. The output directory must lie **outside both trees**, and filesync
refuses to start otherwise: inside the source it would write into a read-only tree, and inside the
destination the next run would mirror-delete the files. (This includes the default — running from a
directory inside the source or destination is caught too; run from elsewhere, or pass `--report`.)
An existing report is never overwritten — a same-minute re-run's stem gets a `-2`/`-3` suffix.
`sync` writes its report as the run progresses, so even an interrupted run leaves a usable record; a
completed one ends with a `run completed` line, so a report cut short by an interruption is
recognizable.

### Conclusions

A full diff of a large tree is too much to read line by line — and the dangerous part (a backup
folder that silently vanished from the source, about to be mirror-deleted) is easy to miss in it. So
`diff` also writes a **conclusions** file that surfaces the meaningful few, loudest where data could
be lost:

- **Data-loss watch** — whole destination folders that are *entirely* absent from the source;
  deletions whose name appears **nowhere** in the source (not a move — content that would simply be
  gone); and the total deletion volume as a share of the destination, with a banner when it's large.
- **Overview** — the counts with byte totals.
- **Changes by top-level folder** — a table of adds/deletes/changes/moves and the net file and byte
  change per folder, biggest losses first.
- **Junk & system paths** — how many `.Trash-*`, `$RECYCLE.BIN`, `.DS_Store`, etc. entries are being
  deleted from the destination or copied from the source.
- **Extremes** — the largest single additions and deletions, and a breakdown of deletions by file
  extension.

## Stopping a run early

A `sync` can take a long time. To stop one **gracefully** — without abandoning it mid-write — signal it:

- **`Ctrl+C`** (interactive), or **`kill <pid>`** (from another terminal / a script; the PID is in
  the destination's `.filesync.lock`).

On the first signal filesync **finishes the file it's currently writing, then stops before the next
one** — it doesn't start any further copies, renames, or deletes. It still runs the finalize over
what it did: the durability flush and the verify pass, and a report that ends with
`run stopped early by request — N of M planned action(s) done`. The run exits **non-zero**, because
the mirror is incomplete — just re-run to finish (only the unfinished work is redone).

Press **`Ctrl+C` again** (or signal again) to **abort immediately**, if you don't want to wait for a
large in-flight file. That's a hard stop — safe too (writes are atomic and the run resumes), it just
skips the clean finalize. On Windows, `Ctrl+C` is always this immediate stop (Unix signals aren't
available there).

## Safety guarantees

- **Source is read-only** — enforced in the code (source and destination are distinct types; only
  the destination can be written or deleted), and guarded at startup: overlapping source/destination
  paths, a backup dir inside the source, and `/` as either end are all rejected.
- **Deletions require a fully-readable source** — if any part of the source can't be read, a file
  hidden behind the error would look "deleted" and its destination twin (possibly the last copy)
  would be mirror-deleted. Instead, **all deletions are suspended for that run** (copies and renames
  still proceed), and the errors file says so. An entirely empty source is likewise refused — it
  usually means a drive didn't mount.
- **Atomic writes** — each file is written to a temp file and atomically renamed into place, so an
  interruption can never leave a half-written file masquerading as a real one.
- **Resumable** — a re-run re-compares and only redoes what didn't complete; no persistent state to
  corrupt.
- **Stoppable** — a long `sync` can be ended early on demand (see [Stopping a run early](#stopping-a-run-early)); it finishes the in-flight file, flushes and verifies what it did, writes an honest report, and exits non-zero so nothing mistakes the partial mirror for a finished one.
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
