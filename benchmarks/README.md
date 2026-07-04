# filesync benchmarks

Reproducible throughput measurements on real removable media. Results are appended to
[`results/results.csv`](results/) so anyone can compare their hardware and audit the numbers
behind filesync's design decisions (durability / verify costs).

## What it measures

For two corpus shapes — **large** (few 512 MiB files) and **small** (many 64 KiB files):

| Phase | Question it answers |
|-------|--------------------|
| `write`    | How fast can we durably (`fsync`'d) write to the device? |
| `checksum` | How fast can we blake3-hash the data back (cold)? |
| `copy`     | Device-to-device copy speed, under 4 variants: `plain`, `+fsync`, `+verify`, `+fsync+verify` |

The `copy` variants are the point: they isolate the cost of `fsync`-per-file and of
verify-by-reread, so the durability/verify defaults are chosen from data, not guesswork.

## Setup

Pass each drive on the command line as **either its mount directory** (e.g. `/media/you/MYDISK`)
**or its filesystem label** (e.g. `MYDISK`). The mount directory is the most reliable — label
lookup relies on `findmnt` resolving `LABEL=`, which some setups don't. Find both with
`lsblk -o NAME,LABEL,FSTYPE,MOUNTPOINT`. The last path component (or the label) — both impersonal,
no username — is what gets recorded in the CSV. No file editing needed.

Optional knobs at the top of [`usb_transfer.rs`](usb_transfer.rs):

- `LABEL_A` / `LABEL_B` — defaults used only when you omit the drive arguments.
- `GIB_PER_PROFILE` — total size per profile; start at `2`, bump to `20` for a real run
  (or override per-run with the `FILESYNC_BENCH_MIB` env var for a quick smoke-test).

To smoke-test without any USB drive, pass two ordinary directory paths as the drive arguments.

## Disk space

The tool checks free space and **refuses to start** rather than crash mid-write. With
`GIB_PER_PROFILE = G` (so a ~`2G` total corpus), rough free-space needs per drive:

- `create` — `2G` (both profiles).
- `copy` — `~G` at the destination (variants are written one at a time; the corpus is already there).
- `all` — `~3G` on **each** drive (its corpus plus one incoming copy).

If a drive is short, it prints needed-vs-free and exits; lower `GIB_PER_PROFILE` or free up space.
If a write still hits ENOSPC (e.g. the estimate was under-read), it reports it and cleans up the
partial corpus instead of panicking.

**FAT/exFAT caveat.** Every file occupies at least one whole allocation unit (cluster), which on
exFAT/FAT can be 128 KiB–several MiB. So the many-small-files profile (64 KiB files) can occupy
*many times* its nominal size — e.g. with a 1 MiB cluster, 32k × 64 KiB files eat ~32 GiB, not 2 GiB.
The estimate reads the real cluster size (`stat -f`) and accounts for this, so the "need ~X" figure
can be far larger than `2G`/`3G` on such drives. Lower `GIB_PER_PROFILE` accordingly, or use a
smaller-cluster filesystem for the small-files test. To preview the requirement for a hypothetical
cluster size without that drive, set `FILESYNC_BENCH_BLOCK=<bytes>` (e.g. `1048576` for 1 MiB).

## Safety

The benchmark only ever creates and deletes files inside **one** folder per drive —
`<drive>/.filesync_benchmark_scratch/`. A guard (`guarded_remove_dir_all`) refuses to delete
anything outside it, so the drive root and your own files can't be touched even if a path is
misconfigured. Remove the scratch dir anytime with `clean`:

```sh
cargo bench --bench usb_transfer -- clean MY_USB_A   # (and MY_USB_B)
```

## Running

### One command (recommended)

Plug in both drives and pass their mount directories (or labels — see Setup):

```sh
cargo bench --bench usb_transfer -- all MY_USB_A MY_USB_B
```

`all` runs the whole sequence itself — create corpus on both drives → drop cache → checksum →
drop cache → copy both directions — clearing the page cache between measured phases by
unmounting+remounting each drive (no root needed via `udisksctl`, or `drop_caches` as root).

Each phase shows a **live progress bar** on stderr (bar, %, files done, live MiB/s, ETA) so long
runs aren't silent. It's display-only (doesn't affect the recorded throughput) and appears only on
a terminal — piped or redirected runs stay quiet and just record the summary.

### Manual phases (mind the cache!)

To step through it, run the phases separately and clear the cache between them by
**unmounting + remounting (or unplugging + replugging) both drives** — otherwise reads hit the
page cache and look unrealistically fast:

```sh
cargo bench --bench usb_transfer -- create   MY_USB_A            # write corpus to A
#   → replug both drives ←
cargo bench --bench usb_transfer -- checksum MY_USB_A            # cold hash of A
cargo bench --bench usb_transfer -- copy     MY_USB_A MY_USB_B   # cold copy A → B
#   → replug ←
cargo bench --bench usb_transfer -- copy     MY_USB_B MY_USB_A   # cold copy B → A
```

## Fidelity notes

- Data is random (xorshift), non-sparse, incompressible — no filesystem can cheat with
  zero-detection or compression.
- Device-to-device copy can't use reflink/copy-on-write, so byte movement is real.
- `write` and `copy --fsync` call `fsync` (file + best-effort parent dir), so the numbers
  reflect *durable* writes, not just cache hits.

## Results

`results/results.csv` columns:
`unix_ts, phase, from, to, profile, fsync, verify, files, mib, secs, mib_per_s, files_per_s`.

When sharing results, please add a note of the drive models, connection (USB 2/3, etc.) and
filesystem — that's what makes the numbers meaningful to someone else.
