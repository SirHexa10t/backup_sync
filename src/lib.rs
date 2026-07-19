//! filesync — cheaply and reliably mirror one directory onto another.
//!
//! See `README.md` for the CLI/UX and `docs/theory.md` for the design rationale and the
//! benchmark data behind it.
//!
//! Pipeline: scan both trees → `diff` (classify + move-detect) → `plan` (ordered actions) →
//! `apply` (renames/deletes/atomic copies → end-sync → verify) → `report`.

pub mod apply;
pub mod artifacts;
pub mod cli;
mod device;
pub mod diff;
pub mod durability;
pub mod hash;
pub mod links;
pub mod manifest;
pub mod plan;
mod preflight;
pub mod progress_update;
pub mod reports;
pub mod runtime;
pub mod scan;
pub mod target;
mod units;

pub use cli::{Cli, Command};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use manifest::{DstRoot, Kind, SrcRoot};

/// Program entry point, called from `main` — and by embedders (e.g. a shell-tool wrapper), which
/// is why it returns a plain exit code (`0` = success) rather than the opaque `process::ExitCode`.
pub fn run(cli: Cli) -> u8 {
    let common = cli.command.common();

    // FIRST, before any filesystem access: settle the privilege model. Under sudo this drops to
    // the invoking user (root kept in reserve unless --unelevated); as plain root it refuses.
    if let Err(msg) = runtime::elevation::init(common.unelevated) {
        eprintln!("filesync: {msg}");
        return 1;
    }

    if let Err(msg) = preflight::validate_roots(&common.from, &common.to) {
        eprintln!("filesync: {msg}");
        return 1;
    }

    let src = SrcRoot::new(&common.from);
    let dst = DstRoot::new(&common.to);

    match &cli.command {
        Command::Diff(a) => {
            // Resolve the output directory first and refuse to place it inside either tree (same
            // rule as sync) — including the DEFAULT (current) directory.
            let out_dir = match preflight::resolve_output_dir(&a.common.report, &src, &dst) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("filesync diff: {msg}");
                    return 1;
                }
            };

            // Scan both trees. On different devices, do it concurrently — the two reads use
            // independent I/O paths and the CPU isn't the bottleneck; on one device, sequentially
            // (parallel reads would only fight over the head).
            let parallel = device::different_devices(src.path(), dst.path());
            let (src_scan, dst_scan) = if parallel {
                let (_group, mut sp, mut dp) = progress_update::scan_pair(src.path(), dst.path());
                let dst_path = dst.path();
                std::thread::scope(|s| {
                    let dh = s.spawn(move || {
                        let o = scan::scan_with_errors(dst_path, &mut dp);
                        dp.finish();
                        o
                    });
                    let so = scan::scan_with_errors(src.path(), &mut sp);
                    sp.finish();
                    (so, dh.join().expect("destination scan thread panicked"))
                })
            } else {
                let mut sp = progress_update::ScanProgress::start(src.path());
                let so = scan::scan_with_errors(src.path(), &mut sp);
                sp.finish();
                let mut dp = progress_update::ScanProgress::start(dst.path());
                let d_out = scan::scan_with_errors(dst.path(), &mut dp);
                dp.finish();
                (so, d_out)
            };

            let dopts = diff::DiffOptions {
                eager: a.common.eager_checksum,
                relative_symlinks: a.common.relative_symlinks,
                include_same: a.include_same,
                parallel,
            };
            let cp = progress_update::CompareProgress::start();
            let d = diff::diff(&src, &src_scan.manifest, &dst, &dst_scan.manifest, &dopts, &cp);
            cp.finish();

            // All reporting — the four output files AND the terminal summary — is reports/'
            // business (crate::reports::diff_cmd). A diff is a preview; it always exits 0.
            let audit = runtime::elevation::drain_audit();
            reports::diff_cmd::emit(
                &out_dir,
                &src,
                &src_scan,
                &dst,
                &dst_scan,
                &d,
                &audit,
                runtime::elevation::available(),
            );
            0
        }
        Command::Sync(a) => run_sync(&src, &dst, a),
    }
}

fn run_sync(src: &SrcRoot, dst: &DstRoot, a: &cli::SyncArgs) -> u8 {
    // Windows can't flush directory entries through std, so the default end-of-run barrier cannot
    // make renames durable there — refuse rather than silently promise less (docs: durability.rs).
    #[cfg(windows)]
    if !a.fsync_each {
        eprintln!(
            "filesync sync: on Windows the default end-of-run durability barrier cannot persist \
             renames — run with --fsync-each"
        );
        return 1;
    }

    // Resolve the output directory first and refuse to place it inside either tree (see
    // resolve_output_dir) — including the DEFAULT (current) directory.
    let out_dir = match preflight::resolve_output_dir(&a.common.report, src, dst) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("filesync sync: {msg}");
            return 1;
        }
    };

    if let Err(e) = fs::create_dir_all(dst.path()) {
        eprintln!("filesync sync: cannot create destination {}: {e}", dst.path().display());
        return 1;
    }

    // One sync per destination: concurrent runs would sweep each other's staging files and plan
    // from snapshots the other invalidates. Held (and auto-released) for the rest of this run.
    let _lock = match runtime::lock::Lock::acquire(dst) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("filesync sync: {e}");
            return 1;
        }
    };

    // Validate the backup dir before mutating anything (see validate_backup_dir for the rules).
    if let Some(bdir) = &a.backup_dir {
        if let Err(msg) = preflight::validate_backup_dir(bdir, src, dst) {
            eprintln!("filesync sync: {msg}");
            return 1;
        }
    }

    // Scan both trees — concurrently when they're on different devices (independent I/O paths),
    // sequentially on one device. The destination scan also sweeps temp files a previous,
    // interrupted run left behind.
    let parallel = device::different_devices(src.path(), dst.path());
    let (src_scan, (dst_scan, swept)) = if parallel {
        let (_group, mut sp, mut dp) = progress_update::scan_pair(src.path(), dst.path());
        std::thread::scope(|s| {
            let dh = s.spawn(move || {
                let r = scan::scan_destination(dst, &mut dp);
                dp.finish();
                r
            });
            let so = scan::scan_with_errors(src.path(), &mut sp);
            sp.finish();
            (so, dh.join().expect("destination scan thread panicked"))
        })
    } else {
        let mut sp = progress_update::ScanProgress::start(src.path());
        let so = scan::scan_with_errors(src.path(), &mut sp);
        sp.finish();
        let mut dp = progress_update::ScanProgress::start(dst.path());
        let dr = scan::scan_destination(dst, &mut dp);
        dp.finish();
        (so, dr)
    };
    if src_scan.manifest.is_empty() {
        eprintln!(
            "filesync sync: source {} is empty — refusing to mirror, which would delete everything \
             in the destination. If the source drive simply isn't mounted, mount it and retry; to \
             deliberately empty the destination, remove it yourself.",
            src.path().display()
        );
        return 1;
    }
    if swept > 0 {
        eprintln!("filesync: removed {swept} leftover temp file(s) from a previous run");
    }
    for p in &dst_scan.skipped_backup_dirs {
        eprintln!("filesync: ignoring backup dir at destination: {}", p.display());
    }
    let (src_m, dst_m) = (src_scan.manifest, dst_scan.manifest);

    // include_same is a diff-only findings toggle; the sync planner never needs the unchanged list.
    let dopts = diff::DiffOptions {
        eager: a.common.eager_checksum,
        relative_symlinks: a.common.relative_symlinks,
        include_same: false,
        parallel,
    };
    let cp = progress_update::CompareProgress::start();
    let d = diff::diff(src, &src_m, dst, &dst_m, &dopts, &cp);
    cp.finish();

    let opts = apply::Options {
        verify: !a.no_verify,
        fsync_each: a.fsync_each,
        backup_dir: a.backup_dir.clone(),
        relative_symlinks: a.common.relative_symlinks,
    };

    // Open the (streamed) report — never truncating a previous one (the stem is de-duplicated) —
    // and fall back to in-memory if the file can't be created. The errors file (companion, opened
    // lazily on the first issue) shares the stem.
    let paths = reports::OutputPaths::build(&out_dir, "sync", src.path(), SystemTime::now());
    let context = format!("sync {} -> {}", src.path().display(), dst.path().display());
    let mut report = reports::Report::create(&paths.report, &paths.errors, &context).unwrap_or_else(|e| {
        eprintln!("filesync sync: cannot open report {} ({e}); continuing without a report file", paths.report.display());
        reports::Report::new()
    });

    // The showstoppers forecast — written before apply, so even an interrupted run leaves it.
    let stoppers = reports::sync_cmd::write_showstoppers(
        &paths,
        src,
        &src_m,
        &src_scan.denied,
        dst,
        &dst_m,
        &dst_scan.denied,
        &d,
        runtime::elevation::available(),
    );

    // Anything the diff couldn't examine as intended (it degraded safely instead of aborting).
    for issue in &d.issues {
        report.issue_msg(issue.clone());
    }

    // Record anything we couldn't read while scanning, up front, so an interrupted run still
    // shows what was missed (its contents were omitted from the mirror). Labeled by side.
    for e in &src_scan.errors {
        report.issue_msg(format!("source: {e}"));
    }
    for e in &dst_scan.errors {
        report.issue_msg(format!("destination: {e}"));
    }
    for p in &src_scan.skipped_backup_dirs {
        report.issue_msg(format!(
            "source contains a filesync backup dir; its subtree is not mirrored: {} \
             (delete its {} marker to include it)",
            p.display(),
            artifacts::BACKUP_MARKER
        ));
    }

    let mut actions = plan::plan(&d);

    // SAFETY VALVE: deletions are only trustworthy when the source was read COMPLETELY. A file
    // invisible behind an unreadable *directory* would be classified "extra at destination" and
    // deleted, destroying the (possibly last) copy. A source *file* that's listable but couldn't
    // be *read* during classification (`d.source_unreadable`) is just as dangerous: its would-be
    // move degrades to copy+delete, so a to-be-deleted destination file might be its content under
    // a new name. Either way, suspend every deletion; copies and renames still run.
    let src_view_incomplete = !src_scan.errors.is_empty()
        || !src_scan.skipped_backup_dirs.is_empty()
        || d.source_unreadable;
    if src_view_incomplete {
        let deletes = actions.iter().filter(|x| matches!(x, plan::Action::Delete(_))).count();
        // Hard-link updates can clear an existing destination name (a delete in disguise), so
        // they're deferred too — the next fully-readable run performs them.
        let links = actions.iter().filter(|x| matches!(x, plan::Action::HardLink { .. })).count();
        // A rename lands via a raw fs::rename, which silently REPLACES a file/symlink already at
        // the target. Normally the plan clears a wrong-kind occupant first (a Delete — moved
        // aside under --backup-dir); with those deletes suspended, the rename itself would erase
        // the occupant with no record and no backup. Defer exactly the renames whose target is
        // currently occupied — the occupant might be the last copy of data the unreadable part
        // of the source is hiding.
        let dst_paths: std::collections::HashSet<&Path> =
            dst_m.iter().map(|e| e.rel.as_path()).collect();
        let mut deferred_renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        actions.retain(|x| match x {
            plan::Action::Delete(_) | plan::Action::HardLink { .. } => false,
            plan::Action::Rename { from, to } if dst_paths.contains(to.as_path()) => {
                deferred_renames.push((from.clone(), to.clone()));
                false
            }
            _ => true,
        });
        if deletes > 0 {
            report.issue_msg(format!(
                "source was not fully readable — {deletes} deletion(s) suspended; nothing was \
                 deleted this run. Fix the source (permissions/mount) and re-run."
            ));
        }
        if links > 0 {
            report.issue_msg(format!(
                "{links} hard-link update(s) deferred until the source is fully readable"
            ));
        }
        if !deferred_renames.is_empty() {
            report.issue_msg(format!(
                "{} rename(s) deferred until the source is fully readable — each target path is \
                 occupied at the destination, its clearing delete is suspended, and a raw rename \
                 would silently erase the occupant",
                deferred_renames.len()
            ));
            for (from, to) in &deferred_renames {
                report.issue_msg(format!(
                    "deferred rename: {} -> {} (target occupied)",
                    from.display(),
                    to.display()
                ));
            }
        }

        // Deletes normally free space before the copies run; with deletions suspended, look ahead
        // instead of churning into a full disk: if the planned copies can't all fit, skip them too.
        let needed = plan::planned_copy_bytes(&actions, &src_m);
        let needed_with_margin = needed + needed / 20 + 32 * 1024 * 1024; // ~5% + slack
        if let Some(avail) = device::available_bytes(dst.path()) {
            if avail < needed_with_margin {
                let copies = actions.iter().filter(|x| matches!(x, plan::Action::Copy(_))).count();
                actions.retain(|x| !matches!(x, plan::Action::Copy(_)));
                report.issue_msg(format!(
                    "insufficient free space for the {copies} planned copies while deletions are \
                     suspended (need ~{} MiB, have {} MiB) — copies skipped this run",
                    needed_with_margin / (1 << 20),
                    avail / (1 << 20)
                ));
            }
        }
    }

    // Warn up front about destination limitations that will force skips/fallbacks.
    let caps = target::probe(dst);
    if !caps.symlinks {
        let n = src_m.iter().filter(|e| e.kind == Kind::Symlink).count();
        if n > 0 {
            report.issue_msg(format!("destination cannot store symlinks; {n} will be skipped"));
        }
    }
    if !caps.hardlinks && !d.to_link.is_empty() {
        // Content still lands (the apply stage falls back to independent copies) — the linkage
        // is what's lost, so this is a note, not a failure.
        report.skip_msg(format!(
            "destination cannot hold hard links; {} linked name(s) will be copied as independent \
             files",
            d.to_link.len()
        ));
    }

    // Arm the graceful-stop handlers just before the mutating phase: the first Ctrl+C (or a
    // SIGTERM) stops after the current file and finalizes cleanly; a second aborts. The scanning
    // above is read-only, so a stop there is just a plain (safe) abort — nothing to finalize.
    runtime::interrupt::arm();

    // Live progress for the long parts (bar = bytes to copy; auto-hidden off-terminal).
    let prog = progress_update::Progress::for_sync(plan::planned_copy_bytes(&actions, &src_m), actions.len() as u64);
    apply::apply(src, dst, &src_m, &actions, &opts, &mut report, &prog, runtime::interrupt::global());
    prog.finish();

    // The accountability trail: every operation root helped with (scan heals included).
    for m in runtime::elevation::drain_audit() {
        report.root_op(m);
    }

    report.finish();

    // The end-of-run terminal summary is reports/' business (crate::reports::sync_cmd).
    reports::sync_cmd::print_summary(&report, &paths, &stoppers, runtime::elevation::available());

    // A requested early stop leaves the mirror incomplete → exit non-zero, like issues, so a script
    // never mistakes it for a finished backup.
    if report.issues.is_empty() && !report.was_stopped_early() {
        0
    } else {
        1
    }
}
