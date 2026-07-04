# filesync — theory, findings, and design rationale

This is the "why" behind filesync: what we're optimizing for, the measurements that grounded the
design, and how those measurements lead to the plan we settled on. It reads as research notes, not
just a checklist — the raw data lives in
[`../benchmarks/results/1GiB_USB_results.csv`](../benchmarks/results/1GiB_USB_results.csv).

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
4. **Correct** — re-copy (and re-verify) any mismatches; repeat until clean or flagged.
   *(serves aim 1 again: the result is guaranteed, not assumed.)*

This delivers a reliable result (verify) **and** robust durability (end-sync) at low time and low
flash wear — strictly better than `fsync-each`, which is slower, wears more, and doesn't verify.

**Defaults:** no per-file fsync (single end-of-run sync); `verify` **on**. Escapes: `--fsync-each`
for someone who wants every-file-durable-as-it-goes; `--no-verify` to skip the check.

### Minimize writes: skip-identical + move-as-rename (from findings 3 & 4, and the aim)

- Copy only files that are **missing or changed**, deciding identity by **content hash, not name**.
- Detect **moves** (same content, different path) and perform a **local rename** at the destination
  instead of re-copying — the single biggest speed and wear win.
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
  `--backup-dir` makes deletions/overwrites recoverable.
- **`--eager-checksum`** opt-in to compare by content hash instead of the default size+mtime
  quick-check (with tolerance for coarse FAT/exFAT timestamps) — for a thorough check, or to never
  miss a same-size+mtime change.

### Reliable end-result (aim 1)

The verify stage guarantees byte-perfect copies; the end-sync guarantees durability at "done"; and
the pre-flight **diff report** (new / changed / moved / deleted) plus the post-run **issues report**
give the operator complete visibility into what happened and what needs attention.

## Decisions locked

- Copy engine: **pure Rust**, no external tool.
- Durability: **staged copy → end-sync → verify → correct**; no per-file fsync by default
  (`--fsync-each` escape).
- Correctness: **verify on by default** (`--no-verify` escape).
- Efficiency: **skip-identical + move-as-rename**, identity by blake3 content hash.
- Safety: **source read-only** (type-enforced), **atomic temp+rename**, resumable, mirror with
  early deletes (no prompts — preview via `diff`; `--backup-dir` for recoverable deletes),
  `--eager-checksum` opt-in.
- Manifest: **in-memory**, recomputed per run (no persisted tracking file).

## Next

With durability (staged, fsync-off default) and verify (on) settled, and the read-only enforcement
approach confirmed, the remaining work is implementation — Phase 1 onward: in-memory scan + manifest
+ hashing, then diff + report, then the staged copy/verify/correct engine, each with its tests.
