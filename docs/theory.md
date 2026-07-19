# filesync — theory, findings, and design rationale

This is the "why" behind filesync: what we're optimizing for, the measurements that grounded the
design, and how those measurements lead to the plan we settled on. It reads as research notes, not
just a checklist — the raw data lives in
[`../benchmarks/results/1GiB_USB_results.csv`](../benchmarks/results/1GiB_USB_results.csv) (the
WRITE/CHECKSUM/COPY study) and
[`../benchmarks/results/4GiB_parallel_results.csv`](../benchmarks/results/4GiB_parallel_results.csv)
(the `--jobs` parallelism study), and
[`../benchmarks/results/3GiB_copy_parallel_results.csv`](../benchmarks/results/3GiB_copy_parallel_results.csv)
(the authentic parallel-copy + durability study).

## Aim

filesync copies a source directory to a destination — typically **removable / external storage**
— for backups. Its priorities, in order:

1. **Reliable end-result.** When it reports "done," the destination is a correct, complete copy of
   the source: every file present, byte-for-byte intact, with no silent corruption, no missing
   files, and no stale/extra files masquerading as current.
2. **Robust procedure.** The process itself is safe: the source is never modified; an interruption
   (unplugged drive, power loss, Ctrl-C) can never corrupt data or leave the destination worse
   than it started; the run is resumable; deletions never touch the wrong thing.
3. **Fast runtime — a second priority, but still high.** Backups to slow removable media can take
   hours. Among designs that satisfy (1) and (2), we pick the fastest, and we work hard to avoid
   unnecessary work. Speed never overrides reliability or robustness.

**Usage model.** A large sync can run for a night or a weekend, so filesync runs **unattended — no
interactive prompts** — and is resumable. The user does one of two things: run `sync` (do the job)
or run `diff` (preview the job / find discrepancies). To "check before acting," run `diff` first;
there is deliberately no confirmation prompt in `sync`.

### Why writes are the thing to avoid

On the target media — USB flash, spinning and SATA drives — **writing is the slow, expensive
operation, and on flash it physically wears the device.** If writes were free, the simplest
correct backup would be to wipe the destination and re-copy everything every time. They aren't, so
the core job is to **write as little as possible**: copy only what is genuinely new or changed,
turn moves into renames, and never re-copy what is already correct. This principle threads through
every decision below.

## The experiment

`benchmarks/usb_transfer.rs` measured three operations — **WRITE**, **CHECKSUM**, **COPY** —
cold-cache (drives unmounted+remounted between phases to defeat the page cache), at **1 GiB per
profile**, for two corpus shapes:

- **large**: a few 512 MiB files (throughput-dominated).
- **small**: 16,384 × 64 KiB files (per-file-overhead-dominated).

COPY was run under four variants: `plain`, `+fsync-each`, `+verify`, `+fsync-each+verify`. Two real
drives were used:

- **USB-stick-exFAT** — a USB flash stick, exFAT.
- **USB-SATA-BTRFS** — a larger SATA drive, Btrfs.

## Key data

COPY throughput, MiB/s (from the CSV):

| copy → destination | large plain | large +fsync-each | small plain | small +fsync-each |
|---|---:|---:|---:|---:|
| → USB-SATA-BTRFS   | 89.9 | 41.6 | **34.1** | **2.0** |
| → USB-stick-exFAT  | 46.8 | 24.6 | 0.5 | 0.4 |

Supporting rows:

- **Durable small-file write** (fsync'd) of 1 GiB: **2169 s (~36 min)** on the stick, 154 s on the
  SATA drive. The same data **read back**: 17 s and 32 s. Reads are ~50–120× faster than durable
  small-file writes.
- **verify** (re-read + hash-compare) cost: ~3–14% typically, ≈0 when the copy is already
  write-bound, worst case ~47% (large files written to the slow stick). Contrast fsync-each's up
  to **17×**.
- Same small-file copy: **34 MiB/s** to the SATA drive vs **0.5 MiB/s** to the exFAT stick — the
  **destination device dominates**, and its ~133 ms/small-file write rate is a hardware floor.

## Findings

1. **Per-file `fsync` is catastrophic for small files (2–17× slower) and multiplies flash wear.**
   It forces a flush-and-wait to the device once per file, so nothing batches — and each forced
   commit is a physical program/erase cycle on flash.
2. **`verify` is cheap** — it's a read, reads are fast, and it costs ≈nothing exactly when writes
   are the bottleneck. It is also the *only* operation that actually checks correctness.
3. **Writes ≫ reads; the destination dominates; small files are the enemy.** The expensive,
   device-wearing operation is the small-file write to the destination.
4. **Avoiding a write always wins.** Turning a moved file (same content, new path) into a local
   rename instead of a re-copy can save the entire copy time — e.g. ~36 min for 1 GiB of small
   files on the stick.

## From findings to plan

### `fsync` vs `verify` are different questions

- **`fsync`** answers *"are the bytes durably on the device?"* — `write()` only puts data in the
  OS RAM cache; `fsync` forces it to the device and waits.
- **`verify`** answers *"are the bytes that landed actually correct?"* — a cold re-read + blake3
  compare against the source, catching silent corruption (dying flash, bad cable, bit-rot).

They are orthogonal. `fsync-each` gives durability *as you go* but is slow, wearing, and **never
checks correctness**.

### The plan: a staged "rough copy → sync → verify → correct"

This is the architecture the findings point to, and it satisfies all three aims at once:

1. **Bulk copy** — stream each needed file source→dest, hashing the source while reading it (free),
   **without per-file fsync**. Fast; the OS batches writes. *(serves aim 3, and aim 1 by hashing.)*
2. **Durability barrier** — one filesystem `fsync` at the end, before we announce completion.
   *(serves aim 2: everything is physically on the device when we say "done".)*
3. **Verify** — cold re-read every copied file, hash, compare to source; build a mismatch list.
   *(serves aim 1: byte-perfect result, actually checked.)*
4. **Correct** — a copy that fails verification is reported and **removed from the destination**.
   Crucially, a corrupt copy carries the source's size *and* mtime, so if left in place every
   later quick-check run would call it "unchanged" forever; removed, the next run sees it as
   missing and simply re-copies it — so a plain re-run always heals. (Corruption that a
   `--no-verify` run let through is likewise healed by re-running with `--eager-checksum`, which
   compares content instead of size+mtime — the initial run's flags never limit later repair.)
   *(serves aim 1 again: the mirror never retains a file that looks synced but isn't.)*

This delivers a reliable result (verify) **and** robust durability (end-sync) at low time and low
flash wear — strictly better than `fsync-each`, which is slower, wears more, and doesn't verify.

**Defaults:** no per-file fsync (single end-of-run sync); `verify` **on**. Escapes: `--fsync-each`
for someone who wants every-file-durable-as-it-goes; `--no-verify` to skip the check.

### Minimize writes: skip-identical + move-as-rename (from findings 3 & 4, and the aim)

- Copy only files that are **missing or changed**, deciding identity by **content hash, not name**.
- Detect **moves** (same content, different path) and perform a **local rename** at the destination
  instead of re-copying — the single biggest speed and wear win.
- Mirror **hard-link groups as hard links**: names sharing an inode at the source (detected for
  free — the scan's stat already carries `dev`/`ino`/`nlink`) are written **once** via the group's
  leader; the other names become destination hard links (metadata only). The one trap is handled
  explicitly: a re-copied leader lands via atomic temp+rename, which creates a *new* inode — so a
  leader rewrite forces every follower to be **relinked in the same run**, or they'd silently keep
  serving the old bytes. Followers of a verified leader are verified by construction (same inode).
- Overwrite only on **proven** difference: a same-size file whose mtime drifted is hash-compared
  first; identical content gets a metadata refresh instead of a re-copy.
- These are why writing is minimized to exactly what's necessary — the raison d'être of the tool.

#### Move detection — the algorithm (core, always on — not a flag)

Terms: an **add** is a file Source has that Destination lacks (would be copied); an **extra** is a
file Destination has that Source lacks (mirror would delete it). A **move** is an `add` whose
identical content already exists as an `extra` — so instead of copy-then-delete we **`rename` it in
place on the Destination**: no transfer, no write, no flash wear. Steps 1–4 are the `diff`/plan;
`sync` executes step 5.

1. **Scan** Source and Destination into in-memory manifests (relative path, size, mtime, type).
2. **Classify by path:** in both & same (skip) / in both & differ (update) / Source-only (**add**) /
   Destination-only (**extra**).
3. **Cross-reference `add` × `extra` by content:**
   - group `extra` files by **size** — a cheap pre-filter, since only equal-size files can be
     identical, so we hash *only genuine candidates, never the whole tree*;
   - for each `add` with a same-size `extra`, blake3-hash both. Computing the hash reads every byte,
     so a match is a true content check (collision odds ~2⁻²⁵⁶), not a name check;
   - match → **move**: plan `rename(dest/old → dest/new)` and drop the pair from `add`/`extra`;
   - size collision but different content → leave as a normal `add` + `extra`.
4. **Plan** = moves (renames) + remaining extras (deletes) + adds (copies) + changed (updates).
5. **Execute** destructive/space-freeing first (renames + deletes), then copies/updates, then verify.
   Moves are verified-by-construction (content was confirmed in step 3).

Cost/benefit: we spend a Destination read + a Source read on each size-matched candidate to avoid a
full Destination **write** — and writes are the slow, wearing operation (findings 1 & 3). The extra
work is bounded to size-collision candidates, never the whole tree.

### Source trust and reachability (aim 1 & 2)

Two assumptions about the source are load-bearing, so they are stated (and enforced) explicitly:

1. **The source must be fully readable.** Mirroring decides deletions by *absence*: a file the scan
   couldn't see looks identical to a file the user deleted — and its destination twin might be the
   last surviving copy. The scan itself is the reachability check (it visits every directory), so
   no separate pre-pass is needed. Two kinds of source read failure trigger the safety valve:
   an unreadable **directory** (its contents vanish from the scan), and a listable-but-unreadable
   **file** caught during classification — because a would-be move whose source can't be hashed
   degrades to copy+delete, so a to-be-deleted destination file might actually be that file's
   content under a new name. **On either — or a skipped marked backup dir — every deletion is
   suspended for that run** — copies and renames still proceed (they are additive/content-
   preserving), and the report says what was suspended and why. One exception: a rename whose
   target path is currently occupied at the destination is deferred along with the deletions —
   its target-clearing delete was suspended, and a raw rename would silently replace the occupant
   (which might itself be the last copy of something the unreadable source is hiding). (Every
   read-failure message names its side, so "my source won't back up" is never confused with "a
   destination extra".)
   Because deletions normally free space *before* the copies run, a suspended run also does a
   **space look-ahead**: if the destination can't fit all planned copies, the copies are skipped
   too rather than churning into a full disk.
2. **The source is authoritative — filesync cannot tell a damaged file from an edited one.**
   *Unreadable* damage (I/O errors) is caught by rule 1. *Silent* damage (a file that reads fine
   but whose bytes rotted) is indistinguishable from a legitimate edit without prior knowledge —
   i.e. a persisted known-good hash, which filesync deliberately doesn't keep (no tracking file).
   The mitigations that DO exist, layered:
   - **Never destroy on a shallow signal.** A same-size file whose mtime drifted is hash-compared
     *before* its destination version is overwritten. Identical content ⇒ a metadata refresh
     (`refreshed` in the report) — no write, nothing destroyed, and the next run is quiet again.
   - **Hunt corruption with an `--eager-checksum` re-run.** After a completed (verified) backup,
     the two trees are known-identical — so a later eager run finding "same size + same mtime +
     different content" means bytes rotted on one side (or an mtime-preserving edit). That
     signature is explicitly called out in the report, and the file is re-copied from the source.
     It cannot say **which side** rotted — pair with `--backup-dir` so the destination's previous
     version survives even if the source was the damaged side.

### The backup dir is self-marking (`--backup-dir`)

The natural place for a recovery folder is *inside the destination* (it must share the
destination's filesystem, since move-aside uses `rename`). But a naive mirror would then destroy
it on the next run: the backup dir isn't in the source, so its contents look like extras. So:

- On first use, filesync drops a **marker file** (`.filesync-backup-dir`) into the backup dir.
  **Scans skip marked directories entirely** — they are never mirrored, deleted, or re-backed-up.
  (If a marked dir is found in the *source*, its subtree is skipped and reported — filesync's own
  trash-cans aren't data to mirror; delete the marker to override.)
- **One run, one backup dir**: a backup dir must be fresh (absent or empty). Reuse is refused —
  `rename` into an already-populated backup dir would silently overwrite same-named entries from
  the earlier run.
- The backup dir must **not overlap the source** (it receives writes; the source is read-only —
  and the compile-time type wall can't see a raw `--backup-dir` path, so this is checked at
  startup), and must not *be* the destination itself.

### Robust procedure (aim 2)

- **Source is strictly read-only** — enforced at compile time via distinct `SrcRoot`/`DstRoot`
  path types (destructive functions accept only `DstRoot`), plus a before/after source-hash audit
  in tests. All mutations happen on the destination.
- **Atomic writes** — copy to a temp file, then atomic `rename` into place, so an interruption can
  never leave a half-written file masquerading as real.
- **Interruption-safe & resumable** — because each file is atomic and the manifest is recomputed,
  a re-run simply re-copies whatever didn't land; stray temp files are swept.
- **Mirror with early deletes** — the destination mirrors the source (no stale extras); deletions
  run *early* to free space on tight targets, and never against the source. There is **no
  interactive prompt** (filesync runs unattended) — preview with the `diff` command, and
  `--backup-dir` makes deletions/overwrites recoverable. Deletions require a **complete** source
  scan (see "Source trust and reachability" above) — an unreadable source suspends them all.
- **`--eager-checksum`** opt-in to compare by content hash instead of the default size+mtime
  quick-check (with tolerance for coarse FAT/exFAT timestamps) — for a thorough check, or to never
  miss a same-size+mtime change.

### Reliable end-result (aim 1)

The verify stage guarantees byte-perfect copies; the end-sync guarantees durability at "done"; and
the pre-flight **diff report** (new / changed / moved / deleted) plus the post-run **issues report**
give the operator complete visibility into what happened and what needs attention.

## Parallelism (`--jobs`): a second experiment

A later benchmark asked whether worker parallelism helps on the target media. `--jobs` currently
parallelizes only filesync's **hashing** (verify + move-detection); copies are sequential. The
sweep ran WRITE and READ at 1/2/4/6/8/16 workers over **4 GiB per profile**, cold before each run,
on **USB-SATA-BTRFS** — raw data in
[`../benchmarks/results/4GiB_parallel_results.csv`](../benchmarks/results/4GiB_parallel_results.csv).

Throughput vs workers (MiB/s):

| op / profile   | 1 | 2 | 4 | 6 | 8 | 16 |
|---|--:|--:|--:|--:|--:|--:|
| write / large  | 249 | 185 | 216 | 195 | 168 | 184 |
| write / small  |  82 | 105 | **129** | 109 | 121 | 128 |
| read  / large  | 357 | 279 | 290 | 271 | 274 | 355 |
| read  / small  | 106 | 146 | 149 | 143 | 127 | 128 |

Tentative reading:

- **Large files: parallelism buys nothing**, read or write — one stream already saturates the
  device; jobs=1 is as fast as anything.
- **Small-file writes: ~1.5× from ~4 workers** (82 → 129 MiB/s), plateauing by jobs≈4 — per-file
  overhead (create/close/metadata) overlaps across workers.
- **Reads don't scale** meaningfully on this device.

### Caveat: this is a hint, not a verdict

Two things make these numbers directional only:

1. **The write prototype is not filesync's copy.** It generates data in RAM (no **source read**),
   does no **blake3 hashing**, skips the **temp-file + rename + mtime/perms**, and — most important
   — flushes with **one global `sync`** where filesync does **one `sync_all` per file** at the end
   (`apply.rs`). For the small profile that is 65,536 fsyncs of real cost the benchmark never pays,
   so it measures an *idealized* filesync; the small-file numbers in particular overstate what the
   real program would achieve.
2. **The read numbers aren't repeatable.** Each is a single cold sample, taken right after tens of
   GiB of writes, over a corpus written by 16 parallel writers (a nondeterministic Btrfs layout).
   The large-file reads swung ~25% between two runs; treat the read *shape* as noise, not signal.

Also: only one device (Btrfs SATA) was measured this round.

### Authentic copy sweep — the verdict

The `jobs` sweep above was a data-*generating* prototype. To settle it, a second sweep copies a real
corpus (internal disk → USB) through filesync's **actual copy path** (`copy_one`: read source +
blake3-hash + temp file + rename) at 1–16 workers, under both durability barriers — `each` (one
`sync_all` per file, filesync's *current* behavior) and `fs` (one filesystem `sync`). 3 GiB of small
files (49,152 × 64 KiB) → USB-SATA-BTRFS; raw data in
[`../benchmarks/results/3GiB_copy_parallel_results.csv`](../benchmarks/results/3GiB_copy_parallel_results.csv).

Small-file copy throughput, MiB/s:

| durability | 1 | 2 | 4 | 6 | 8 | 16 |
|---|--:|--:|--:|--:|--:|--:|
| `each` (per-file fsync) | 48 | 49 | 32 | 44 | 33 | 38 |
| `fs` (one fs-sync)      | **72** | 64 | 71 | 45 | 54 | 40 |

Two firm conclusions:

1. **Parallelizing copies does not help — it hurts.** In *both* barriers the best result is at
   **jobs=1**; more workers are flat-to-worse. The earlier prototype's "~1.5× for small writes" was
   an artifact of omitting fsync. **Decision: copies stay sequential, and the `--jobs` flag (which
   only parallelized hashing) was removed** — which also validates the existing design.
2. **`fsync` is the real lever.** At the setting filesync actually runs (sequential, jobs=1), one
   fs-sync is **~1.5× faster** than per-file fsync (72 vs 48 MiB/s; up to ~2× at some worker counts).
   The barrier is 49,152 *serialized* device flushes — a large fixed cost that dominates a small-file
   backup and swamps everything else.

### Privilege model: root in reserve, never in charge

Launched under sudo, filesync drops to the invoking user immediately (keeping saved-uid 0) and
re-escalates **per operation, per thread**, only when an operation fails with `EACCES`/`EPERM` at
one of the enumerated walls (list / read / delete / rename / create / stamp). The class test is
deliberate: an unforeseen error must never be rammed through with privilege — `EIO` stays loud
(dying disk), `EROFS` root can't fix, and anything unrecognized is reported, not overridden. Root
expands *capability*, never *policy*: it performs only actions the plan already contained, existing
files' ownership/permissions are never modified, elevated-created artifacts are chowned back to the
user, and every assist is recorded in the report. `--unelevated` drops privileges permanently
instead; a bare root login is refused (no `$SUDO_UID` to own the run).

### A different axis: scanning two devices at once

The verdict above concerns parallel work on **one** device, where a single stream already saturates
it. Reading the **source and destination concurrently** is the opposite situation: in the normal
backup case they're on *different* devices (internal disk ↔ external drive), so the two scans — and,
in the classification phase, the two move-detection hash passes (source candidates vs destination
candidates) — travel independent I/O paths. Overlapping them roughly halves that phase's wall-clock
for free (the CPU is never the bottleneck). filesync does this automatically, gated on a device-id
check (`stat().dev()` on the two roots): different devices → concurrent (`std::thread::scope`, no new
dependency); same device → sequential, since parallel reads there would only thrash one head. This
doesn't contradict the `--jobs` verdict — it's a distinct axis (two devices, not N workers on one).
(Caveat: two partitions of one physical disk look "different" to the dev-id check; the real win is
genuinely separate drives.)

### The fix: one fs-sync, not N per-file fsyncs

This resolves the discrepancy the plan already implied: the barrier looped `sync_all` per copied
file, but the plan specifies **one filesystem sync at the end**. It is now **implemented** in
`apply.rs` — `syncfs` on the destination, with a portable fsync-per-file + per-directory fallback —
the highest-value change for small-file backups, because it is both:

- **~1.5× faster** (data above): it replaces tens of thousands of serialized device flushes with a
  single `syncfs`; and
- **more correct** — `syncfs` also flushes the directory entries that make the atomic `rename`s
  durable, which the current per-file `sync_all` loop never does (a latent durability gap in the
  default path — and reliability is aim #1).

Caveats on the numbers: single cold samples (noisy — e.g. `fs` jobs=6 dipped), one device (Btrfs
SATA SSD), and 3 GiB may be partly SLC-assisted (less so for `each`, whose fsyncs defeat the write
cache). The **jobs=1 `each`-vs-`fs` gap is the cleanest, most decision-relevant point**.

## Decisions locked

- Copy engine: **pure Rust**, no external tool.
- Durability: **staged copy → one `syncfs` → verify → correct**; no per-file fsync by default
  (`--fsync-each` escape).
- Correctness: **verify on by default** (`--no-verify` escape).
- Efficiency: **skip-identical + move-as-rename**, identity by blake3 content hash.
- Safety: **source read-only** (type-enforced), **atomic temp+rename**, resumable, mirror with
  early deletes (no prompts — preview via `diff`; `--backup-dir` for recoverable deletes),
  `--eager-checksum` opt-in.
- Manifest: **in-memory**, recomputed per run (no persisted tracking file).
- Copies **sequential**, hashing too — parallelism gives no benefit on the target media (measured,
  both durability modes; jobs=1 is best). The `--jobs` flag was **removed**; `parallel.rs` is kept
  only for the benchmark.

## Next

The engine is implemented (scan + manifest + hashing, diff + report, staged copy/verify/correct,
move-as-rename, mirror-with-backup), each with tests.

1. **Durability barrier — done.** The per-file `sync_all` loop is replaced by a single `syncfs` on
   the destination (with a portable fsync-per-file + per-directory fallback), keeping `--fsync-each`
   as the escape. ~1.5× on small-file backups, and it closes the rename-durability gap.
2. **Parallelism — settled and removed.** The authentic copy sweep showed no benefit (it hurts);
   copies stay sequential and the `--jobs` flag is gone. `parallel.rs`/rayon are retained only for
   the benchmark's worker-count sweeps.
