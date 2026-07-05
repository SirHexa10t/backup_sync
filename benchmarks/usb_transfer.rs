//! USB / removable-device transfer benchmark for filesync.
//!
//! Produces the data behind two design decisions — how much `fsync`-per-file costs, and how
//! much verify-by-reread costs — plus the three raw measurements: WRITE, CHECKSUM, COPY, each
//! for two profiles: "large" (few big files) and "small" (many tiny files).
//!
//! ─────────────────────────────────────────────────────────────────────────────
//! HOW TO RUN (from the project root). Note the `--bench usb_transfer --` boilerplate:
//! cargo selects the benchmark target, and everything after `--` is passed to it.
//!
//!   # the whole sequence, cold caches — pass your two drives' filesystem LABELS:
//!   cargo bench --bench usb_transfer -- all MY_USB_A MY_USB_B
//!
//!   # or individual phases. A <drive> is a filesystem LABEL or a mount directory
//!   # (a mount directory such as /media/you/DISK is the most reliable form):
//!   cargo bench --bench usb_transfer -- create   MY_USB_A
//!   cargo bench --bench usb_transfer -- checksum MY_USB_A
//!   cargo bench --bench usb_transfer -- copy     MY_USB_A MY_USB_B
//!   cargo bench --bench usb_transfer -- clean    MY_USB_A
//!
//!   # --jobs scaling sweep — WRITE and READ, 1..16 workers, 4 GiB corpus, cold each run.
//!   # NOTE: rewrites the corpus per worker count (~48 GiB of writes); shrink with FILESYNC_BENCH_MIB:
//!   cargo bench --bench usb_transfer -- jobs     MY_USB_A
//!
//!   # AUTHENTIC parallel-copy sweep — copies a real corpus <fastFrom> -> <driveTo> with filesync's
//!   # actual copy path (read+hash+temp+rename) at 1..16 workers. Knobs: FILESYNC_BENCH_SYNC=each|fs
//!   # (durability barrier), FILESYNC_BENCH_REPEAT=N (repeats/point), FILESYNC_BENCH_PROFILE=small|
//!   # large|both, FILESYNC_BENCH_MIB=<per-profile MiB>. -> copy_jobs_results.csv:
//!   cargo bench --bench usb_transfer -- copy-jobs /fast/scratch MY_USB_A
//!
//!   # e.g. a steady-state, small-files-only run (defeats SLC cache), both durability models:
//!   FILESYNC_BENCH_PROFILE=small FILESYNC_BENCH_MIB=16384 cargo bench --bench usb_transfer -- copy-jobs /fast/scratch MY_USB_A
//!
//! Find labels with:  lsblk -o NAME,LABEL,FSTYPE,MOUNTPOINT
//! If you omit the drive args, the LABEL_A / LABEL_B defaults below are used.
//! ─────────────────────────────────────────────────────────────────────────────
//! `all` does the whole dance itself: check free space → create corpus on both drives → remount
//! (drop cache) → checksum both → remount → copy both directions. It resolves/remounts drives via
//! `findmnt`/`udisksctl` (no root; or `drop_caches` as root), so there's no second config file.
//! It refuses to start unless each drive has room (roughly 3× the per-profile size for `all`).
//!
//! SAFETY: only ever creates/deletes inside ONE folder per drive —
//! `<drive>/.filesync_benchmark_scratch/`. A guard refuses to delete anything outside it, so the
//! drive root and your files can't be touched even if a path is misconfigured. `clean` removes it.
//!
//! CACHE: the OS page cache lets a just-written file read back at RAM speed, hiding real device
//! throughput. Unmounting a filesystem invalidates its cache, so remount = cold reads.
//! ─────────────────────────────────────────────────────────────────────────────

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

// ===================== defaults (used only if you omit the CLI drive args) =====================
const LABEL_A: &str = "CHANGE_ME_A";
const LABEL_B: &str = "CHANGE_ME_B";

/// Total size PER PROFILE. Start small to sanity-check, then bump to 20 for a real run.
/// Overridable per-run with the FILESYNC_BENCH_MIB env var (handy for quick smoke-tests).
const GIB_PER_PROFILE: u64 = 1;
/// Per-profile total for the `jobs` sweep (also overridable via FILESYNC_BENCH_MIB).
const JOBS_GIB_PER_PROFILE: u64 = 4;
// ===============================================================================================

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

const LARGE_FILE_BYTES: u64 = 512 * MIB; // "few large": 512 MiB each
const SMALL_FILE_BYTES: u64 = 64 * KIB; //  "many small": 64 KiB each

/// The ONE folder, per drive, this tool is allowed to create and delete within.
/// Deliberately dot-prefixed and unmistakable so it can't collide with real data.
const SCRATCH: &str = ".filesync_benchmark_scratch";
const BUF_BYTES: usize = 4 * MIB as usize;

/// One of the two corpus shapes.
struct Profile {
    name: &'static str,
    file_bytes: u64,
    count: u64,
}

fn profiles() -> Vec<Profile> {
    profiles_of(env_total_bytes(GIB_PER_PROFILE))
}

/// Per-profile total in bytes: `FILESYNC_BENCH_MIB` (in MiB) if set, else `default_gib` GiB.
fn env_total_bytes(default_gib: u64) -> u64 {
    std::env::var("FILESYNC_BENCH_MIB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|mib| mib * MIB)
        .unwrap_or(default_gib * GIB)
}

/// The two corpus shapes for a given per-profile total. File sizes are clamped to the total so tiny
/// smoke-test totals still yield ≥1 file per profile.
fn profiles_of(total: u64) -> Vec<Profile> {
    let large = LARGE_FILE_BYTES.min(total);
    let small = SMALL_FILE_BYTES.min(total);
    vec![
        Profile { name: "large", file_bytes: large, count: (total / large).max(1) },
        Profile { name: "small", file_bytes: small, count: (total / small).max(1) },
    ]
}

/// The profiles to actually run, honoring FILESYNC_BENCH_PROFILE = large | small | both (default).
/// Lets an expensive run focus on the profile that matters (e.g. small files) instead of both.
fn selected_profiles(total: u64) -> Vec<Profile> {
    let want = std::env::var("FILESYNC_BENCH_PROFILE").ok();
    profiles_of(total)
        .into_iter()
        .filter(|p| match want.as_deref() {
            Some("large") => p.name == "large",
            Some("small") => p.name == "small",
            _ => true, // "both" or unset
        })
        .collect()
}

// ── drive resolution: a CLI arg is a filesystem label OR a directory path ─────

/// A resolved drive: where it's mounted and an impersonal name for results.
/// (Remounting looks the device up from the label on demand in `clear_cache`.)
struct Drive {
    path: PathBuf,
    name: String,
}

/// Run a command and capture trimmed stdout; None on failure / non-zero exit / empty output.
fn capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Mount point of a filesystem label. Tries `findmnt --source LABEL=…`, then the
/// `/dev/disk/by-label/…` symlink → device → mount, because some `findmnt` versions don't resolve
/// a bare positional `LABEL=` (which is why passing the mount directory is the more robust form).
fn mountpoint_of_label(label: &str) -> Option<PathBuf> {
    let spec = format!("LABEL={label}");
    if let Some(mp) = capture("findmnt", &["-n", "-o", "TARGET", "--source", &spec]) {
        return Some(PathBuf::from(mp));
    }
    let dev = capture("readlink", &["-f", &format!("/dev/disk/by-label/{label}")])?;
    capture("findmnt", &["-n", "-o", "TARGET", "--source", &dev]).map(PathBuf::from)
}

/// The block device to unmount+remount so a drive's page cache is dropped. Resolves the arg
/// (label or directory) to its mount point, then returns that mount's device — but ONLY when the
/// path is itself a mount root, so we never disturb the enclosing filesystem of a plain test dir.
fn remountable_device(spec: &str) -> Option<String> {
    let path = {
        let p = PathBuf::from(spec);
        if p.is_dir() { p } else { mountpoint_of_label(spec)? }
    };
    let ps = path.to_str()?;
    let target = capture("findmnt", &["-n", "-o", "TARGET", ps])?;
    if Path::new(&target) != path {
        return None; // a subdir of a mount, not a mount root — leave it alone
    }
    capture("findmnt", &["-n", "-o", "SOURCE", ps])
}

/// Resolve a drive argument. If it names an existing directory, use it directly (a plain folder,
/// e.g. for smoke-tests — can't be remounted). Otherwise treat it as a mounted filesystem label.
fn resolve_drive(spec: &str) -> Drive {
    let p = PathBuf::from(spec);
    if p.is_dir() {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| spec.to_string());
        return Drive { path: p, name };
    }
    match mountpoint_of_label(spec) {
        Some(path) => Drive { path, name: spec.to_string() },
        None => {
            eprintln!(
                "{spec:?} is neither an existing directory nor a mounted filesystem label.\n\
                 Plug the drive in, or pass its label / a directory path. Find labels with:\n\
                 \tlsblk -o NAME,LABEL,FSTYPE,MOUNTPOINT"
            );
            std::process::exit(2);
        }
    }
}

fn assert_mounted(path: &Path) {
    if !path.is_dir() {
        eprintln!("resolved drive path {path:?} is not an accessible directory");
        std::process::exit(2);
    }
}

// ── free-space precheck (refuse to start rather than crash mid-write) ─────────

/// Filesystem allocation unit (cluster) for `path`, via `stat -f`. On FAT/exFAT this can be large
/// (128 KiB to several MiB), which is what makes many tiny files consume far more than their
/// nominal size — each file occupies at least one whole cluster. Falls back to 4 KiB.
fn block_size(path: &Path) -> u64 {
    // FILESYNC_BENCH_BLOCK overrides the detected cluster — lets you preview the space needed on a
    // hypothetical big-cluster drive (e.g. exFAT) without having that filesystem to hand.
    if let Some(b) = std::env::var("FILESYNC_BENCH_BLOCK")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&b| b > 0)
    {
        return b;
    }
    path.to_str()
        .and_then(|p| capture("stat", &["-f", "-c", "%S", p]))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&b| b > 0)
        .unwrap_or(4096)
}

/// How much a file of `file_bytes` actually occupies, rounded up to whole allocation units.
fn on_disk(file_bytes: u64, block: u64) -> u64 {
    if block <= 1 {
        file_bytes
    } else {
        (file_bytes + block - 1) / block * block
    }
}

/// On-disk footprint of the full corpus (both profiles) for allocation unit `block`.
fn corpus_footprint(block: u64) -> u64 {
    profiles().iter().map(|p| p.count * on_disk(p.file_bytes, block)).sum()
}

/// On-disk footprint of the largest single profile — the peak a `copy` variant occupies at the
/// destination (variants are written then removed one at a time, so only one exists at once).
fn max_profile_footprint(block: u64) -> u64 {
    profiles().iter().map(|p| p.count * on_disk(p.file_bytes, block)).max().unwrap_or(0)
}

/// Add slack for filesystem overhead and per-file block rounding (~5% + 32 MiB).
fn with_margin(bytes: u64) -> u64 {
    bytes + bytes / 20 + 32 * MIB
}

fn human(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// Available bytes on the filesystem holding `path`, via `df -Pk` (POSIX columns, KiB).
fn available_bytes(path: &Path) -> Option<u64> {
    let out = capture("df", &["-Pk", path.to_str()?])?;
    let kib: u64 = out.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// Refuse to start unless the drive has enough free space. If free space can't be determined
/// (e.g. no `df`), warn and proceed rather than block.
fn ensure_space(path: &Path, needed: u64, drive_name: &str) {
    match available_bytes(path) {
        Some(avail) if avail >= needed => {
            eprintln!("[space] {drive_name}: {} free, need ~{} — ok", human(avail), human(needed));
        }
        Some(avail) => {
            eprintln!(
                "ERROR: not enough space on drive {drive_name:?} ({path:?}): need ~{}, only {} free.\n\
                 Lower GIB_PER_PROFILE (or set FILESYNC_BENCH_MIB), or free up space.",
                human(needed),
                human(avail)
            );
            std::process::exit(1);
        }
        None => eprintln!("[space] warning: couldn't check free space on {path:?}; proceeding."),
    }
}

// ── cache clearing between measured phases ────────────────────────────────────

/// Drop the page cache so the next reads are cold: `drop_caches` if root, else unmount+remount
/// each drive that resolves to a device (invalidates that filesystem's cache; no root needed).
fn clear_cache(specs: &[&str]) {
    let _ = Command::new("sync").status();
    if try_drop_caches() {
        eprintln!("[cache] dropped via /proc/sys/vm/drop_caches (root)");
        return;
    }
    let mut remounted = false;
    for &spec in specs {
        // Capture the device while still mounted, then unmount + mount it back.
        if let Some(dev) = remountable_device(spec) {
            let _ = Command::new("udisksctl").args(["unmount", "-b", &dev]).status();
            let _ = Command::new("udisksctl").args(["mount", "-b", &dev]).status();
            remounted = true;
        }
    }
    if !remounted {
        eprintln!(
            "[cache] could not remount automatically (no root, no udisksctl, or the drive args \
             aren't labels). Replug the drives to guarantee cold reads."
        );
    }
}

/// Best-effort `echo 3 > /proc/sys/vm/drop_caches` (needs root). Returns true on success.
fn try_drop_caches() -> bool {
    File::options()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
        .and_then(|mut f| f.write_all(b"3"))
        .is_ok()
}

// ── scratch layout + the destructive-op guard ────────────────────────────────

fn scratch_root(drive: &Path) -> PathBuf {
    drive.join(SCRATCH)
}
fn corpus_root(drive: &Path) -> PathBuf {
    scratch_root(drive).join("corpus")
}
fn corpus_dir(drive: &Path, profile: &str) -> PathBuf {
    corpus_root(drive).join(profile)
}
fn copy_dir(drive: &Path, profile: &str, variant: &str) -> PathBuf {
    scratch_root(drive).join("copy").join(profile).join(variant)
}

/// The ONLY function allowed to delete. It refuses to remove anything that is not the drive's
/// scratch dir or a descendant — so the drive root and your files are unreachable by construction.
/// Deletes entries bottom-up behind a progress bar, since removing many files (e.g. from a slow
/// exFAT stick) is not instant either.
fn guarded_remove_dir_all(path: &Path, drive: &Path) {
    let scratch = scratch_root(drive);
    assert!(
        scratch != *drive && scratch.starts_with(drive),
        "SAFETY: scratch dir {scratch:?} is not strictly inside drive {drive:?}"
    );
    assert!(
        path.starts_with(&scratch),
        "SAFETY: refusing to delete {path:?} — outside the benchmark scratch dir {scratch:?}"
    );
    if !path.exists() {
        return;
    }

    // Count first (a cheap metadata walk) so the bar has a real total, then remove children before
    // their parents. follow_links is false, so symlinks are unlinked, never followed out of the tree.
    let total = WalkDir::new(path).into_iter().filter_map(Result::ok).count() as u64;
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "  {prefix:<16} [{bar:28}] {percent:>3}%  {pos}/{len} entries  {per_sec}  eta {eta}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    bar.set_prefix("delete");
    bar.enable_steady_tick(Duration::from_millis(200));

    for entry in WalkDir::new(path).contents_first(true).into_iter().filter_map(Result::ok) {
        let _ = if entry.file_type().is_dir() {
            fs::remove_dir(entry.path())
        } else {
            fs::remove_file(entry.path())
        };
        bar.inc(1);
    }
    let _ = fs::remove_dir_all(path); // sweep anything a walk error left behind
    bar.finish_and_clear();
}

// ── fast, incompressible, non-sparse fill (xorshift64, no dependency) ─────────

fn fill(buf: &mut [u8], state: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        let bytes = x.to_le_bytes();
        for (dst, src) in chunk.iter_mut().zip(bytes.iter()) {
            *dst = *src;
        }
    }
}

// ── live progress (indicatif) ────────────────────────────────────────────────
// A per-phase bar on stderr: byte bar + %, live throughput, ETA, and (for the sequential
// write/copy phases) a running file count. Display only — indicatif hides itself when stderr
// isn't a terminal, and the recorded throughput is timed separately, so this doesn't affect the
// numbers.

struct Progress {
    bar: ProgressBar,
    done_files: AtomicU64,
    total_files: u64,
}

impl Progress {
    fn start(label: &str, total_bytes: u64, total_files: u64) -> Progress {
        let bar = ProgressBar::new(total_bytes);
        bar.set_style(
            ProgressStyle::with_template(
                "  {prefix:<16} [{bar:28}] {percent:>3}%  {msg}  {binary_bytes_per_sec}  eta {eta}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        bar.set_prefix(label.to_string());
        bar.set_message(format!("0/{total_files} files"));
        bar.enable_steady_tick(Duration::from_millis(200));
        Progress { bar, done_files: AtomicU64::new(0), total_files }
    }

    fn add_bytes(&self, n: u64) {
        self.bar.inc(n);
    }

    /// Sequential phases only: advance the file counter shown in the bar's message.
    fn inc_file(&self) {
        let done = self.done_files.fetch_add(1, Ordering::Relaxed) + 1;
        self.bar.set_message(format!("{done}/{} files", self.total_files));
    }

    /// Set a static message. Used by the parallel checksum phase, where a per-file message update
    /// would add lock contention that could skew the measurement.
    fn note(&self, msg: String) {
        self.bar.set_message(msg);
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

/// Write one random file durably (fsync'd), bumping `progress` as it goes. Returns the IO error
/// instead of panicking so callers can clean up and report (e.g. on ENOSPC).
fn write_random_file(
    path: &Path,
    bytes: u64,
    buf: &mut [u8],
    state: &mut u64,
    progress: &Progress,
) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    let mut remaining = bytes;
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        fill(&mut buf[..n], state);
        f.write_all(&buf[..n])?;
        remaining -= n as u64;
        progress.add_bytes(n as u64);
    }
    f.sync_all()?; // durable write — the honest cost
    progress.inc_file();
    Ok(())
}

/// Report a write failure clearly (with the FAT/exFAT small-file cluster hint on ENOSPC),
/// remove the partial corpus, and exit — instead of an opaque panic + backtrace.
fn fail_out_of_space(e: &std::io::Error, drive: &Path, drive_name: &str, block: u64, p: &Profile, path: &Path) -> ! {
    eprintln!("\nERROR writing {path:?}: {e}");
    if e.raw_os_error() == Some(28) {
        // ENOSPC
        eprintln!(
            "Out of space on drive {drive_name:?}. On FAT/exFAT with a large allocation unit \
             (this filesystem: {}), each of the {} '{}' files occupies at least one whole cluster — \
             far more than its {} nominal size. Lower FILESYNC_BENCH_MIB / GIB_PER_PROFILE, or use a \
             filesystem with a smaller cluster.",
            human(block), p.count, p.name, human(p.file_bytes)
        );
    }
    guarded_remove_dir_all(&corpus_root(drive), drive); // remove the partial corpus
    std::process::exit(1);
}

// ── phases ───────────────────────────────────────────────────────────────────

/// The full sequence: create on both drives, then cold checksum and cold copy in both directions
/// with a cache-clear before each measured phase.
fn phase_all(a: &str, b: &str) {
    // Fail fast if a drive is missing or short on space, before writing anything. Each drive holds
    // its corpus (both profiles) plus, during the cross-copy, one incoming variant at a time.
    let da = resolve_drive(a);
    let db = resolve_drive(b);
    let (ba, bb) = (block_size(&da.path), block_size(&db.path));
    ensure_space(&da.path, with_margin(corpus_footprint(ba) + max_profile_footprint(ba)), &da.name);
    ensure_space(&db.path, with_margin(corpus_footprint(bb) + max_profile_footprint(bb)), &db.name);

    phase_create(a);
    phase_create(b);

    clear_cache(&[a, b]);
    phase_checksum(a);
    clear_cache(&[a, b]);
    phase_checksum(b);

    clear_cache(&[a, b]);
    phase_copy(a, b);
    clear_cache(&[a, b]);
    phase_copy(b, a);

    eprintln!("\nDone. Results appended to benchmarks/results/results.csv");
    eprintln!("Remove scratch dirs when finished:  cargo bench --bench usb_transfer -- clean {a}  (and {b})");
}

/// WRITE benchmark: create the corpus on `drive`, fsync each file (durable), time it.
fn phase_create(spec: &str) {
    let d = resolve_drive(spec);
    assert_mounted(&d.path);
    guarded_remove_dir_all(&corpus_root(&d.path), &d.path); // fresh corpus; scratch-only
    let block = block_size(&d.path);
    ensure_space(&d.path, with_margin(corpus_footprint(block)), &d.name);

    for p in profiles() {
        let dir = corpus_dir(&d.path, p.name);
        let t = Instant::now();
        write_corpus_profile(&dir, &p, block, &d.path, &d.name);
        let secs = t.elapsed().as_secs_f64();
        record(Row {
            phase: "write",
            from: d.name.clone(),
            to: String::new(),
            profile: p.name,
            fsync: true,
            verify: false,
            files: p.count,
            bytes: p.count * p.file_bytes,
            secs,
        });
    }
}

/// Create `dir` and fill it with `p.count` random, fsync'd files. Shared by the WRITE phase and the
/// jobs-sweep corpus setup. Exits via `fail_out_of_space` on ENOSPC (removing the partial corpus).
fn write_corpus_profile(dir: &Path, p: &Profile, block: u64, drive: &Path, drive_name: &str) {
    fs::create_dir_all(dir).expect("create corpus dir");
    let mut state: u64 = 0x9E3779B97F4A7C15 ^ p.file_bytes; // distinct stream per profile
    let mut buf = vec![0u8; BUF_BYTES];
    let prog = Progress::start(&format!("write {}", p.name), p.count * p.file_bytes, p.count);
    for i in 0..p.count {
        let path = dir.join(format!("f_{i:06}"));
        if let Err(e) = write_random_file(&path, p.file_bytes, &mut buf, &mut state, &prog) {
            prog.finish();
            fail_out_of_space(&e, drive, drive_name, block, p, &path);
        }
    }
    prog.finish();
}

/// CHECKSUM benchmark: blake3-hash the whole corpus on `drive`, in parallel, time it.
fn phase_checksum(spec: &str) {
    let d = resolve_drive(spec);
    assert_mounted(&d.path);

    for p in profiles() {
        let dir = corpus_dir(&d.path, p.name);
        let files = list_files(&dir);
        if files.is_empty() {
            eprintln!("no files in {dir:?}; run `create {spec}` first");
            continue;
        }
        let prog =
            Progress::start(&format!("checksum {}", p.name), files.len() as u64 * p.file_bytes, files.len() as u64);
        prog.note(format!("{} files", files.len()));
        let t = Instant::now();
        let bytes: u64 = files
            .par_iter()
            .map(|f| {
                let n = hash_file(f).0;
                prog.add_bytes(n);
                n
            })
            .sum();
        let secs = t.elapsed().as_secs_f64();
        prog.finish();
        record(Row {
            phase: "checksum",
            from: d.name.clone(),
            to: String::new(),
            profile: p.name,
            fsync: false,
            verify: false,
            files: files.len() as u64,
            bytes,
            secs,
        });
    }
}

/// COPY benchmark: copy corpus from `from` → `to` under 4 variants
/// (plain / +fsync / +verify / +fsync+verify), timing each.
fn phase_copy(from_spec: &str, to_spec: &str) {
    let from = resolve_drive(from_spec);
    let to = resolve_drive(to_spec);
    assert_mounted(&from.path);
    assert_mounted(&to.path);
    let block = block_size(&to.path);
    ensure_space(&to.path, with_margin(max_profile_footprint(block)), &to.name);

    let variants = [
        ("plain", false, false),
        ("fsync", true, false),
        ("verify", false, true),
        ("fsync+verify", true, true),
    ];

    for p in profiles() {
        let src_dir = corpus_dir(&from.path, p.name);
        let files = list_files(&src_dir);
        if files.is_empty() {
            eprintln!("no files in {src_dir:?}; run `create {from_spec}` first");
            continue;
        }
        for (vname, fsync, verify) in variants {
            let dst_dir = copy_dir(&to.path, p.name, vname);
            guarded_remove_dir_all(&dst_dir, &to.path);
            fs::create_dir_all(&dst_dir).expect("create dst dir");

            let prog = Progress::start(
                &format!("copy {}/{}", p.name, vname),
                files.len() as u64 * p.file_bytes,
                files.len() as u64,
            );
            let t = Instant::now();
            let mut bytes = 0u64;
            for src in &files {
                let name = src.file_name().unwrap();
                match copy_one(src, &dst_dir.join(name), fsync, verify, &prog) {
                    Ok(n) => bytes += n,
                    Err(e) => {
                        prog.finish();
                        eprintln!("\nERROR copying to {:?}: {e}", to.name);
                        if e.raw_os_error() == Some(28) {
                            eprintln!("Out of space on {:?}; lower FILESYNC_BENCH_MIB / GIB_PER_PROFILE.", to.path);
                        }
                        guarded_remove_dir_all(&dst_dir, &to.path);
                        std::process::exit(1);
                    }
                }
            }
            let secs = t.elapsed().as_secs_f64();
            prog.finish();
            record(Row {
                phase: "copy",
                from: from.name.clone(),
                to: to.name.clone(),
                profile: p.name,
                fsync,
                verify,
                files: files.len() as u64,
                bytes,
                secs,
            });
            guarded_remove_dir_all(&dst_dir, &to.path); // reclaim space before the next variant
        }
    }
}

/// Remove this tool's scratch dir from a drive (leaves everything else untouched).
fn phase_clean(spec: &str) {
    let d = resolve_drive(spec);
    assert_mounted(&d.path);
    let scratch = scratch_root(&d.path);
    guarded_remove_dir_all(&scratch, &d.path);
    println!("cleaned {scratch:?}");
}

/// JOBS sweep: how does worker count affect throughput on this device — for WRITES (the expensive,
/// flash-wearing operation) and, secondarily, for reads (hashing)?
///
/// `--jobs` currently parallelizes only filesync's hashing (verify + move-detection); copies are
/// sequential. This sweep therefore measures two things at 1/2/4/6/8/16 workers over a 4 GiB corpus
/// per profile, page cache dropped (unmount+remount) before every run so nothing is served from RAM:
///   • WRITE — a parallel-write prototype (buffered writes + one end-of-run `sync`, matching
///     filesync's default durability). This is the number that decides whether parallelizing the
///     copy stage is worth its complexity.
///   • READ  — filesync's own `parallel::map` + `hash_file` (the verify/move-detect path).
///
/// WEAR: the write sweep rewrites the corpus once per worker count, so a full run writes roughly
/// `6 × 2 × 4 GiB ≈ 48 GiB` to the device. Lower it with FILESYNC_BENCH_MIB.
fn phase_jobs(spec: &str) {
    const JOBS: &[usize] = &[1, 2, 4, 6, 8, 16];

    let d0 = resolve_drive(spec);
    assert_mounted(&d0.path);
    let block = block_size(&d0.path);
    let total = env_total_bytes(JOBS_GIB_PER_PROFILE);
    // One profile lives on the drive at a time (written, swept, removed), so we need room for the
    // largest single profile.
    let peak = selected_profiles(total)
        .iter()
        .map(|p| p.count * on_disk(p.file_bytes, block))
        .max()
        .unwrap_or(0);
    ensure_space(&d0.path, with_margin(peak), &d0.name);
    let per_pass: u64 = selected_profiles(total).iter().map(|p| p.count * p.file_bytes).sum();
    eprintln!(
        "[wear] the write sweep rewrites the corpus {} times per profile — about {} of writes total. \
         Lower with FILESYNC_BENCH_MIB.",
        JOBS.len(),
        human(JOBS.len() as u64 * per_pass)
    );

    for p in selected_profiles(total) {
        let file_bytes = p.file_bytes;

        // WRITE sweep — rewrite the corpus at each worker count, timing durable writes.
        for &jobs in JOBS {
            clear_cache(&[spec]);
            let d = resolve_drive(spec); // re-resolve: a remount can change the mountpoint
            assert_mounted(&d.path);
            let dir = corpus_dir(&d.path, p.name);
            guarded_remove_dir_all(&dir, &d.path); // fresh slate (not timed)
            fs::create_dir_all(&dir).expect("create corpus dir");

            let prog =
                Progress::start(&format!("write {}/{}j", p.name, jobs), p.count * file_bytes, p.count);
            let t = Instant::now();
            let results = filesync::parallel::map(jobs, (0..p.count).collect::<Vec<u64>>(), |i| {
                let seed = 0x9E3779B97F4A7C15 ^ file_bytes ^ i.wrapping_mul(0x2545F4914F6CDD1D);
                write_one_buffered(&dir.join(format!("f_{i:06}")), file_bytes, seed, &prog)
            });
            let _ = Command::new("sync").status(); // flush to the device — inside the timed region
            let secs = t.elapsed().as_secs_f64();
            prog.finish();
            if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
                fail_out_of_space(e, &d.path, &d.name, block, &p, &dir);
            }
            record_jobs("jobs_results.csv", "write", &d.name, p.name, jobs, p.count, p.count * file_bytes, secs);
        }

        // READ sweep — hash the corpus just written, cold, at each worker count.
        for &jobs in JOBS {
            clear_cache(&[spec]);
            let d = resolve_drive(spec);
            assert_mounted(&d.path);
            let files = list_files(&corpus_dir(&d.path, p.name));
            if files.is_empty() {
                eprintln!("no files found after remount; aborting read sweep");
                return;
            }
            let prog = Progress::start(
                &format!("hash {}/{}j", p.name, jobs),
                files.len() as u64 * file_bytes,
                files.len() as u64,
            );
            let t = Instant::now();
            let _ = filesync::parallel::map(jobs, files.clone(), |f| {
                let h = filesync::hash::hash_file(&f).expect("hash file");
                prog.add_bytes(file_bytes);
                h
            });
            let secs = t.elapsed().as_secs_f64();
            prog.finish();
            record_jobs("jobs_results.csv", "read", &d.name, p.name, jobs, files.len() as u64, files.len() as u64 * file_bytes, secs);
        }

        // Reclaim the drive before the next profile's corpus.
        let d = resolve_drive(spec);
        guarded_remove_dir_all(&corpus_dir(&d.path, p.name), &d.path);
    }

    eprintln!("\nDone. Results appended to benchmarks/results/jobs_results.csv");
}

/// Write one random file WITHOUT a per-file fsync — durability comes from the caller's end-of-run
/// `sync`, matching filesync's default (buffered copies + one final flush). Parallel-safe: its own
/// buffer and rng state, and it only touches the shared progress via the atomic byte counter (a
/// per-file message update would serialize the workers).
fn write_one_buffered(path: &Path, bytes: u64, mut state: u64, progress: &Progress) -> std::io::Result<()> {
    if state == 0 {
        state = 1; // xorshift must not start at zero
    }
    let mut f = File::create(path)?;
    let cap = (bytes.min(BUF_BYTES as u64) as usize).max(1);
    let mut buf = vec![0u8; cap];
    let mut remaining = bytes;
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        fill(&mut buf[..n], &mut state);
        f.write_all(&buf[..n])?;
        remaining -= n as u64;
        progress.add_bytes(n as u64);
    }
    f.flush()
}

/// Durability-barrier mode for the copy sweep, via FILESYNC_BENCH_SYNC:
///   "each" (default) — one `sync_all` per copied file, sequentially — filesync's *current* barrier.
///   "fs"             — one whole-filesystem flush (`sync`) for the batch — the *documented* plan.
/// Running the sweep under both quantifies the fsync-per-file vs one-fs-sync question directly.
fn sync_mode() -> &'static str {
    match std::env::var("FILESYNC_BENCH_SYNC").as_deref() {
        Ok("fs") => "fs",
        _ => "each",
    }
}

/// Make the just-copied batch durable, the way `sync_mode()` selects.
fn durability_barrier(files: &[PathBuf], mode: &str) {
    match mode {
        "fs" => {
            let _ = Command::new("sync").status();
        }
        _ => {
            for f in files {
                let _ = File::open(f).and_then(|h| h.sync_all());
            }
        }
    }
}

/// COPY sweep — the *authentic* write benchmark. Copies a corpus from `from` to `to` using
/// filesync's real copy path (`copy_one`: read source + blake3-hash + temp file + rename),
/// parallelized at 1/2/4/6/8/16 workers, then applies the durability barrier
/// (`FILESYNC_BENCH_SYNC=each|fs`). Unlike the `jobs` sweep's data-generating prototype, this is
/// what parallelizing filesync's copy stage would actually do.
///
/// `from` should be a *fast* device distinct from `to` (e.g. an internal disk → the USB target),
/// matching a real backup; it is read warm. Repeat each point with FILESYNC_BENCH_REPEAT (default
/// 1) to tame variance; shrink with FILESYNC_BENCH_MIB. Results → copy_jobs_results.csv.
fn phase_copy_jobs(from_spec: &str, to_spec: &str) {
    const JOBS: &[usize] = &[1, 2, 4, 6, 8, 16];
    let repeat: u32 = std::env::var("FILESYNC_BENCH_REPEAT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1);
    let mode = sync_mode();
    let op = if mode == "fs" { "copy-fs" } else { "copy-each" }; // record the durability mode

    let from = resolve_drive(from_spec);
    let to = resolve_drive(to_spec);
    assert_mounted(&from.path);
    assert_mounted(&to.path);
    let src_block = block_size(&from.path);
    let dst_block = block_size(&to.path);
    let total = env_total_bytes(JOBS_GIB_PER_PROFILE);
    let peak = |block: u64| {
        selected_profiles(total).iter().map(|p| p.count * on_disk(p.file_bytes, block)).max().unwrap_or(0)
    };
    ensure_space(&to.path, with_margin(peak(dst_block)), &to.name);
    ensure_space(&from.path, with_margin(peak(src_block)), &from.name);
    eprintln!(
        "[copy-jobs] {} -> {} | durability={mode} | repeat={repeat}. `from` should be a fast device \
         separate from `to`; it is read warm (drop caches as root for a cold source).",
        from.name, to.name
    );
    let per_pass: u64 = selected_profiles(total).iter().map(|p| p.count * p.file_bytes).sum();
    eprintln!(
        "[wear] ~{} written to {} ({} worker counts x 2 profiles x {repeat}). Lower with FILESYNC_BENCH_MIB.",
        human(JOBS.len() as u64 * per_pass * repeat as u64),
        to.name,
        JOBS.len()
    );

    for p in selected_profiles(total) {
        // Source corpus, built once on `from` (off the destination); not timed.
        let src_dir = corpus_dir(&from.path, p.name);
        if list_files(&src_dir).is_empty() {
            guarded_remove_dir_all(&src_dir, &from.path);
            write_corpus_profile(&src_dir, &p, src_block, &from.path, &from.name);
        }
        let src_files = list_files(&src_dir);
        let bytes: u64 = p.count * p.file_bytes;

        for &jobs in JOBS {
            for _rep in 0..repeat {
                clear_cache(&[to_spec]); // drop the destination's page cache (leave the source warm)
                let to_now = resolve_drive(to_spec); // re-resolve: a remount can move the mountpoint
                let dst_dir = copy_dir(&to_now.path, p.name, "parallel");
                guarded_remove_dir_all(&dst_dir, &to_now.path); // fresh slate (not timed)
                fs::create_dir_all(&dst_dir).expect("create dst dir");

                let prog =
                    Progress::start(&format!("copy {}/{}j", p.name, jobs), bytes, src_files.len() as u64);
                let t = Instant::now();
                let results = filesync::parallel::map(jobs, src_files.clone(), |sf| {
                    let name = sf.file_name().expect("source file has a name");
                    copy_one(&sf, &dst_dir.join(name), false, false, &prog)
                });
                let dst_files: Vec<PathBuf> =
                    src_files.iter().map(|sf| dst_dir.join(sf.file_name().unwrap())).collect();
                durability_barrier(&dst_files, mode); // inside the timed region
                let secs = t.elapsed().as_secs_f64();
                prog.finish();
                if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
                    eprintln!("\nERROR copying to {:?}: {e}", to_now.name);
                    guarded_remove_dir_all(&dst_dir, &to_now.path);
                    std::process::exit(1);
                }
                record_jobs("copy_jobs_results.csv", op, &to_now.name, p.name, jobs, src_files.len() as u64, bytes, secs);
                guarded_remove_dir_all(&dst_dir, &to_now.path); // reclaim before the next point
            }
        }
    }

    eprintln!("\nDone -> benchmarks/results/copy_jobs_results.csv");
    eprintln!("Source corpus left on {}; remove with:  cargo bench --bench usb_transfer -- clean {from_spec}", from.name);
}

// ── the copy primitive we're actually measuring ──────────────────────────────
// Prototype of the real copy engine: stream + hash-while-copying, atomic temp+rename,
// optional fsync (file + parent dir), optional verify-by-reread. Source is opened read-only.

fn copy_one(
    src: &Path,
    final_path: &Path,
    fsync: bool,
    verify: bool,
    progress: &Progress,
) -> std::io::Result<u64> {
    let dst_dir = final_path.parent().expect("dst has parent");
    let tmp = dst_dir.join(format!(
        ".bench.tmp.{}",
        final_path.file_name().unwrap().to_string_lossy()
    ));

    let mut reader = File::open(src)?;
    let mut writer = File::create(&tmp)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; BUF_BYTES];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
        total += n as u64;
        progress.add_bytes(n as u64);
    }
    writer.flush()?;
    if fsync {
        writer.sync_all()?;
    }
    drop(writer);

    fs::rename(&tmp, final_path)?;
    if fsync {
        // Persist the directory entry too. Best-effort: some removable FSes no-op/deny this.
        let _ = File::open(dst_dir).and_then(|d| d.sync_all());
    }

    if verify {
        let src_hash = hasher.finalize();
        let (_, dst_hash) = hash_file(final_path);
        assert_eq!(dst_hash, src_hash, "verify mismatch for {final_path:?}");
    }
    progress.inc_file();
    Ok(total)
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn hash_file(path: &Path) -> (u64, blake3::Hash) {
    let mut f = File::open(path).expect("open for hashing");
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; BUF_BYTES];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).expect("read for hashing");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    (total, hasher.finalize())
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| !p.file_name().map_or(false, |n| n.to_string_lossy().starts_with(".bench.tmp.")))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

// ── results recording ─────────────────────────────────────────────────────────

struct Row {
    phase: &'static str,
    from: String,
    to: String,
    profile: &'static str,
    fsync: bool,
    verify: bool,
    files: u64,
    bytes: u64,
    secs: f64,
}

fn record(r: Row) {
    let mib = r.bytes as f64 / MIB as f64;
    let mib_s = if r.secs > 0.0 { mib / r.secs } else { 0.0 };
    let files_s = if r.secs > 0.0 { r.files as f64 / r.secs } else { 0.0 };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!(
        "{:<8} {:>10}->{:<10} {:<6} fsync={:<5} verify={:<5} | {:>7} files  {:>10.1} MiB  {:>8.2} s  =>  {:>7.1} MiB/s  {:>10.1} files/s",
        r.phase, r.from, r.to, r.profile, r.fsync, r.verify, r.files, mib, r.secs, mib_s, files_s
    );

    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/results");
    let _ = fs::create_dir_all(&results_dir);
    let csv = results_dir.join("results.csv");
    let new = !csv.exists();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&csv) {
        if new {
            let _ = writeln!(
                f,
                "unix_ts,phase,from,to,profile,fsync,verify,files,mib,secs,mib_per_s,files_per_s"
            );
        }
        let _ = writeln!(
            f,
            "{},{},{},{},{},{},{},{},{:.1},{:.3},{:.1},{:.1}",
            ts, r.phase, r.from, r.to, r.profile, r.fsync, r.verify, r.files, mib, r.secs, mib_s, files_s
        );
    }
}

/// Append one jobs-sweep measurement to `<file>` under benchmarks/results — kept separate from the
/// WRITE/CHECKSUM/COPY results so its extra `op`/`jobs` dimensions don't disturb that schema. `op`
/// is "write", "read", or "copy".
fn record_jobs(file: &str, op: &str, drive: &str, profile: &str, jobs: usize, files: u64, bytes: u64, secs: f64) {
    let mib = bytes as f64 / MIB as f64;
    let mib_s = if secs > 0.0 { mib / secs } else { 0.0 };
    let files_s = if secs > 0.0 { files as f64 / secs } else { 0.0 };
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    println!(
        "{:<5} jobs={:<2} {:<6} {:>10}  {:>7} files  {:>10.1} MiB  {:>8.2} s  =>  {:>7.1} MiB/s  {:>10.1} files/s",
        op, jobs, profile, drive, files, mib, secs, mib_s, files_s
    );

    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/results");
    let _ = fs::create_dir_all(&results_dir);
    let csv = results_dir.join(file);
    let new = !csv.exists();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&csv) {
        if new {
            let _ = writeln!(f, "unix_ts,op,drive,profile,jobs,files,mib,secs,mib_per_s,files_per_s");
        }
        let _ = writeln!(
            f,
            "{},{},{},{},{},{},{:.1},{:.3},{:.1},{:.1}",
            ts, op, drive, profile, jobs, files, mib, secs, mib_s, files_s
        );
    }
}

// ── entry ─────────────────────────────────────────────────────────────────────

fn main() {
    // Lenient parse: ignore cargo-injected flags like `--bench`; keep positionals.
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    let cmd = args.get(0).map(String::as_str).unwrap_or("");
    let arg = |i: usize, dflt: &'static str| args.get(i).map(String::as_str).unwrap_or(dflt);

    match cmd {
        "all" => phase_all(arg(1, LABEL_A), arg(2, LABEL_B)),
        "create" => phase_create(arg(1, LABEL_A)),
        "checksum" => phase_checksum(arg(1, LABEL_A)),
        "copy" => phase_copy(arg(1, LABEL_A), arg(2, LABEL_B)),
        "jobs" => phase_jobs(arg(1, LABEL_A)),
        "copy-jobs" => phase_copy_jobs(arg(1, LABEL_A), arg(2, LABEL_B)),
        "clean" => phase_clean(arg(1, LABEL_A)),
        _ => {
            eprintln!(
                "usage — a <drive> is a filesystem LABEL (or a directory path):\n\
                 \tcargo bench --bench usb_transfer -- all       <driveA> <driveB>\n\
                 \tcargo bench --bench usb_transfer -- create    <drive>\n\
                 \tcargo bench --bench usb_transfer -- checksum  <drive>\n\
                 \tcargo bench --bench usb_transfer -- copy      <driveFrom> <driveTo>\n\
                 \tcargo bench --bench usb_transfer -- jobs      <drive>\n\
                 \tcargo bench --bench usb_transfer -- copy-jobs <fastFrom> <driveTo>\n\
                 \tcargo bench --bench usb_transfer -- clean     <drive>\n\n\
                 Find labels with:  lsblk -o NAME,LABEL,FSTYPE,MOUNTPOINT"
            );
            std::process::exit(2);
        }
    }
}
