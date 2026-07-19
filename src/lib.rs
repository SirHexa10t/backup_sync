//! filesync — cheaply and reliably mirror one directory onto another.
//!
//! See `README.md` for the CLI/UX and `docs/theory.md` for the design rationale and the
//! benchmark data behind it.
//!
//! Pipeline: scan both trees → `diff` (classify + move-detect) → `plan` (ordered actions) →
//! `apply` (renames/deletes/atomic copies → end-sync → verify) → `report`.

pub mod apply;
pub mod cli;
pub mod diff;
pub mod durability;
pub mod elevation;
pub mod hash;
pub mod interrupt;
pub mod links;
pub mod lock;
pub mod manifest;
pub mod plan;
pub mod progress;
pub mod reports;
pub mod scan;
pub mod target;

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
    if let Err(msg) = elevation::init(common.unelevated) {
        eprintln!("filesync: {msg}");
        return 1;
    }

    if let Err(msg) = validate_roots(&common.from, &common.to) {
        eprintln!("filesync: {msg}");
        return 1;
    }

    let src = SrcRoot::new(&common.from);
    let dst = DstRoot::new(&common.to);

    match &cli.command {
        Command::Diff(a) => {
            // Resolve the output directory first and refuse to place it inside either tree (same
            // rule as sync) — including the DEFAULT (current) directory.
            let out_dir = match resolve_output_dir(&a.common.report, &src, &dst) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("filesync diff: {msg}");
                    return 1;
                }
            };

            // Scan both trees. On different devices, do it concurrently — the two reads use
            // independent I/O paths and the CPU isn't the bottleneck; on one device, sequentially
            // (parallel reads would only fight over the head).
            let parallel = different_devices(src.path(), dst.path());
            let (src_scan, dst_scan) = if parallel {
                let (_group, mut sp, mut dp) = progress::scan_pair(src.path(), dst.path());
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
                let mut sp = progress::ScanProgress::start(src.path());
                let so = scan::scan_with_errors(src.path(), &mut sp);
                sp.finish();
                let mut dp = progress::ScanProgress::start(dst.path());
                let d_out = scan::scan_with_errors(dst.path(), &mut dp);
                dp.finish();
                (so, d_out)
            };

            let src_scan_incomplete =
                !src_scan.errors.is_empty() || !src_scan.skipped_backup_dirs.is_empty();
            let (src_m, dst_m) = (src_scan.manifest, dst_scan.manifest);
            let dopts = diff::DiffOptions {
                eager: a.common.eager_checksum,
                relative_symlinks: a.common.relative_symlinks,
                include_same: a.include_same,
                parallel,
            };
            let cp = progress::CompareProgress::start();
            let d = diff::diff(&src, &src_m, &dst, &dst_m, &dopts, &cp);
            cp.finish();

            // Everything that needs attention, each line naming its side — bound for the errors
            // file. The two trees fail very differently: a source read gap risks your data, a
            // destination one usually doesn't.
            let mut issues: Vec<String> = Vec::new();
            for e in &src_scan.errors {
                issues.push(format!("source: {e}"));
            }
            for e in &dst_scan.errors {
                issues.push(format!("destination: {e}"));
            }
            for p in &src_scan.skipped_backup_dirs {
                issues.push(format!("source: ignoring backup dir (has {}): {}", apply::BACKUP_MARKER, p.display()));
            }
            for p in &dst_scan.skipped_backup_dirs {
                issues.push(format!("destination: ignoring backup dir (has {}): {}", apply::BACKUP_MARKER, p.display()));
            }
            for issue in &d.issues {
                issues.push(issue.clone());
            }

            // Preview honestly: a sync would refuse the destructive parts of this diff while the
            // source view is incomplete — an unreadable directory (scan) or an unreadable file
            // caught during classification (`d.source_unreadable`). High-signal, so it goes on the
            // terminal, not just the file.
            // Renames onto occupied targets are also deferred by a sync (their target-clearing
            // deletes are suspended, and a raw rename would erase the occupant) — count them so
            // the preview matches what run_sync would actually do.
            let occupied_renames = if src_scan_incomplete || d.source_unreadable {
                let dst_paths: std::collections::HashSet<&Path> =
                    dst_m.iter().map(|e| e.rel.as_path()).collect();
                d.moved.iter().filter(|m| dst_paths.contains(m.to.as_path())).count()
            } else {
                0
            };
            let suspend_note = ((src_scan_incomplete || d.source_unreadable)
                && (!d.removed.is_empty() || !d.to_link.is_empty() || occupied_renames > 0))
            .then(|| {
                format!(
                    "note: a sync would SUSPEND the {} deletion(s) and defer the {} hard-link \
                     update(s) and {} occupied-target rename(s) listed — the source was not \
                     fully readable",
                    d.removed.len(),
                    d.to_link.len(),
                    occupied_renames
                )
            });

            // Build the (de-duplicated) output paths inside the chosen directory, then write the
            // three files: the full findings, the conclusions (always), and the issues (only if
            // any). A same-minute re-run gets a `-N` stem, so nothing is ever clobbered.
            let paths = reports::OutputPaths::build(&out_dir, "diff", src.path(), SystemTime::now());
            let (src_disp, dst_disp) =
                (src.path().display().to_string(), dst.path().display().to_string());

            // Root-assisted operations (scan heals, elevated hashing) — recorded in the findings.
            let audit = elevation::drain_audit();

            let wrote_report = reports::findings::write_diff(
                &paths.report,
                &src_disp,
                &dst_disp,
                &d.render(true),
                &audit,
            );

            let conclusions =
                reports::conclusions::analyze(&d, &src_m, &dst_m).render(&src_disp, &dst_disp);
            let wrote_conclusions =
                reports::write_diag(&paths.conclusions, &conclusions, "conclusions");

            let wrote_errors =
                !issues.is_empty() && reports::errors::write_diff_errors(&paths.errors, &issues);

            // The showstoppers file: what blocks a faithful mirror, as paste-able remedies.
            // Written by both commands, only when there is something to show.
            let stoppers = reports::showstoppers::analyze(
                &src,
                &src_m,
                &src_scan.denied,
                &dst,
                &dst_m,
                &dst_scan.denied,
                &d,
                elevation::available(),
            );
            let wrote_stoppers = !stoppers.is_empty()
                && reports::write_diag(&paths.showstoppers, &stoppers.render(), "showstoppers");

            // Terminal: the compact count summary, the suspension preview, and where the detail
            // went — never the full dump.
            print!("{}", d.render(false));
            if let Some(note) = &suspend_note {
                println!("{note}");
            }
            if wrote_report {
                println!("{:<12} {}", "findings:", paths.report.display());
            }
            if wrote_conclusions {
                println!("{:<12} {}", "conclusions:", paths.conclusions.display());
            }
            if wrote_stoppers {
                println!(
                    "{:<12} {}  ({} item(s) — paste-able remedies inside)",
                    "showstoppers:",
                    paths.showstoppers.display(),
                    stoppers.total()
                );
            }
            if !issues.is_empty() {
                if wrote_errors {
                    println!("{:<12} {}  ({} issue(s))", "issues:", paths.errors.display(), issues.len());
                } else {
                    println!("issues: {}", issues.len());
                    for i in &issues {
                        println!("  ! {i}");
                    }
                }
            }
            if !audit.is_empty() {
                println!("root-assisted: {} operation(s) — recorded in the findings file", audit.len());
            }
            if !elevation::available() && issues.iter().any(|i| i.contains("Permission denied")) {
                println!(
                    "hint: permission-denied issues — run under `sudo filesync` to let it handle \
                     restricted-access files (root is used only at those walls, and every use is \
                     recorded), or fix them manually."
                );
            }
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
    let out_dir = match resolve_output_dir(&a.common.report, src, dst) {
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
    let _lock = match lock::Lock::acquire(dst) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("filesync sync: {e}");
            return 1;
        }
    };

    // Validate the backup dir before mutating anything (see validate_backup_dir for the rules).
    if let Some(bdir) = &a.backup_dir {
        if let Err(msg) = validate_backup_dir(bdir, src, dst) {
            eprintln!("filesync sync: {msg}");
            return 1;
        }
    }

    // Scan both trees — concurrently when they're on different devices (independent I/O paths),
    // sequentially on one device. The destination scan also sweeps temp files a previous,
    // interrupted run left behind.
    let parallel = different_devices(src.path(), dst.path());
    let (src_scan, (dst_scan, swept)) = if parallel {
        let (_group, mut sp, mut dp) = progress::scan_pair(src.path(), dst.path());
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
        let mut sp = progress::ScanProgress::start(src.path());
        let so = scan::scan_with_errors(src.path(), &mut sp);
        sp.finish();
        let mut dp = progress::ScanProgress::start(dst.path());
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
    let cp = progress::CompareProgress::start();
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

    // The showstoppers forecast: permission walls this run will hit (or, under sudo, the ones
    // that already resisted root). Written before apply, so even an interrupted run leaves it.
    let stoppers = reports::showstoppers::analyze(
        src,
        &src_m,
        &src_scan.denied,
        dst,
        &dst_m,
        &dst_scan.denied,
        &d,
        elevation::available(),
    );
    let stoppers_path = (!stoppers.is_empty()
        && reports::write_diag(&paths.showstoppers, &stoppers.render(), "showstoppers"))
    .then(|| paths.showstoppers.clone());

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
            apply::BACKUP_MARKER
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
        let needed = planned_copy_bytes(&actions, &src_m);
        let needed_with_margin = needed + needed / 20 + 32 * 1024 * 1024; // ~5% + slack
        if let Some(avail) = available_bytes(dst.path()) {
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
    interrupt::arm();

    // Live progress for the long parts (bar = bytes to copy; auto-hidden off-terminal).
    let prog = progress::Progress::for_sync(planned_copy_bytes(&actions, &src_m), actions.len() as u64);
    apply::apply(src, dst, &src_m, &actions, &opts, &mut report, &prog, interrupt::global());
    prog.finish();

    // The accountability trail: every operation root helped with (scan heals included).
    for m in elevation::drain_audit() {
        report.root_op(m);
    }

    report.finish();

    print!("{}", report.render());
    if report.has_file() {
        println!("report: {}", paths.report.display());
    }
    if let Some(p) = &stoppers_path {
        println!(
            "showstoppers: {}  ({} item(s) — paste-able remedies inside)",
            p.display(),
            stoppers.total()
        );
    }
    // Surface issues: point at the errors file if one was written, else inline (in-memory report,
    // or the errors file couldn't be opened) so they're never lost.
    if !report.issues.is_empty() {
        match report.errors_file() {
            Some(p) => println!("issues: {} — see {}", report.issues.len(), p.display()),
            None => {
                for i in &report.issues {
                    println!("  ! {i}");
                }
            }
        }
    }
    // Permission walls with no root in reserve: say, once, how to let filesync handle them.
    if !elevation::available() && report.issues.iter().any(|i| i.contains("Permission denied")) {
        println!(
            "hint: permission-denied issues above — run under `sudo filesync` to let it handle \
             restricted-access files (it drops to your user and uses root only at those walls, \
             recording every use), or fix them manually."
        );
    }

    // A requested early stop leaves the mirror incomplete → exit non-zero, like issues, so a script
    // never mistakes it for a finished backup.
    if report.issues.is_empty() && !report.was_stopped_early() {
        0
    } else {
        1
    }
}

/// Resolve the directory this run writes its output files into, and refuse to place it inside either
/// tree: inside the read-only source it can't be written (and would be backed up as data); inside
/// the destination the next sync would mirror-delete the files as extras. With no `--report`, output
/// goes to the current directory; a `--report` argument must be an existing directory.
fn resolve_output_dir(
    report: &Option<PathBuf>,
    src: &SrcRoot,
    dst: &DstRoot,
) -> Result<PathBuf, String> {
    let dir = match report {
        Some(p) => {
            if !p.is_dir() {
                return Err(format!(
                    "--report must be an existing directory to write the output files into: {}",
                    p.display()
                ));
            }
            p.clone()
        }
        None => std::env::current_dir()
            .map_err(|e| format!("cannot determine the current directory for output files: {e}"))?,
    };
    let cdir = canonicalize_lenient(&dir);
    if fs::canonicalize(src.path()).is_ok_and(|cf| cdir.starts_with(cf)) {
        return Err(format!(
            "the output directory ({}) is inside the source, which is read-only — run from a \
             different directory or pass --report <dir outside it>",
            dir.display()
        ));
    }
    if cdir.starts_with(canonicalize_lenient(dst.path())) {
        return Err(format!(
            "the output directory ({}) is inside the destination — the next sync would delete the \
             files as extras; pass --report <dir outside it>",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Validate the source/destination pair before doing anything. Rejects a non-directory source and
/// any overlap between the two roots — identical, or one nested inside the other. Comparison is on
/// *canonical* paths, so an overlap can't be hidden behind a symlink, `..`, or a trailing-slash
/// alias.
fn validate_roots(from: &Path, to: &Path) -> Result<(), String> {
    if !from.is_dir() {
        return Err(format!("source is not a directory: {}", from.display()));
    }
    let cf = fs::canonicalize(from)
        .map_err(|e| format!("cannot resolve --from {}: {e}", from.display()))?;
    let ct = canonicalize_lenient(to);

    // Neither end may be the filesystem root. Mirroring FROM `/` would scan every mount —
    // including the destination itself (self-nesting copies, mirror-deleting the previous backup)
    // and pseudo-filesystems like /proc; mirroring ONTO `/` would mirror-delete everything
    // outside the source.
    if cf.parent().is_none() {
        return Err("--from must not be the filesystem root (scanning / would descend into every \
                    mount, including the destination itself)"
            .to_string());
    }
    if ct.parent().is_none() {
        return Err("--to must not be the filesystem root".to_string());
    }
    if cf == ct {
        Err("--from and --to are the same directory".to_string())
    } else if ct.starts_with(&cf) {
        Err(format!(
            "--to is inside --from — that would copy the tree into itself (from={}, to={})",
            cf.display(),
            ct.display()
        ))
    } else if cf.starts_with(&ct) {
        Err(format!(
            "--from is inside --to — mirror-delete could erase the source (from={}, to={})",
            cf.display(),
            ct.display()
        ))
    } else {
        Ok(())
    }
}

/// Canonicalize `path`, tolerating a not-yet-created tail. Relative paths are resolved against the
/// current directory first, so a relative argument can never dodge an overlap check. Then walk the
/// path component by component: while components exist, resolve them for real (following symlinks,
/// so `..` crosses a symlink the same way the kernel would); once a component doesn't exist, the
/// rest is handled lexically (`.` dropped, `..` pops) — which matches what `create_dir_all` +
/// path resolution will later do, because freshly created directories are never symlinks.
pub(crate) fn canonicalize_lenient(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };

    let mut out = PathBuf::new();
    let mut exists = true; // flips off at the first component that can't be resolved
    for comp in abs.components() {
        use std::path::Component;
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            // `out` is fully resolved up to here (canonicalized per component), so a lexical pop
            // is the physical parent; in the nonexistent tail it matches future create_dir_all.
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => {
                out.push(c);
                if exists {
                    match fs::canonicalize(&out) {
                        Ok(resolved) => out = resolved,
                        Err(_) => exists = false,
                    }
                }
            }
        }
    }
    out
}

/// Validate `--backup-dir` before anything is mutated. The rules, and why:
/// 1. **Not overlapping the source** (either direction) — the backup dir receives *writes*; the
///    source is strictly read-only, and the type wall can't see a raw backup path.
/// 2. **Not the destination itself** — its marker would make the whole mirror invisible to scans.
///    (A backup dir *inside* the destination is fine: it gets a [`apply::BACKUP_MARKER`] on first
///    use, and scans skip marked dirs, so later runs never mirror, delete, or re-back-up it.)
/// 3. **Fresh** — absent or an empty directory. Reusing a dir (marked from a previous run, or one
///    holding unrelated data) invites silent collisions: `rename` would overwrite same-named
///    entries in it. One run, one backup dir.
/// 4. **Same filesystem as the destination** — move-aside uses `rename`, which can't cross devices.
fn validate_backup_dir(bdir: &Path, src: &SrcRoot, dst: &DstRoot) -> Result<(), String> {
    let cb = canonicalize_lenient(bdir);
    let cf = fs::canonicalize(src.path())
        .map_err(|e| format!("cannot resolve source {}: {e}", src.path().display()))?;
    let cd = canonicalize_lenient(dst.path());

    if cb.starts_with(&cf) || cf.starts_with(&cb) {
        return Err(format!(
            "--backup-dir must not overlap the source (backup-dir={}, source={})",
            cb.display(),
            cf.display()
        ));
    }
    if cb == cd {
        return Err("--backup-dir must not be the destination itself".to_string());
    }
    match fs::symlink_metadata(&cb) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // fresh — will be created on use
        Err(e) => return Err(format!("cannot inspect --backup-dir {}: {e}", cb.display())),
        Ok(md) if !md.is_dir() => {
            return Err(format!("--backup-dir {} exists and is not a directory", cb.display()))
        }
        Ok(_) => match fs::read_dir(&cb) {
            Err(e) => return Err(format!("cannot read --backup-dir {}: {e}", cb.display())),
            Ok(mut rd) => {
                if rd.next().is_some() {
                    return Err(format!(
                        "--backup-dir {} is not empty — each run needs a fresh backup dir \
                         (a previous run's backups stay untouched; pick a new directory)",
                        cb.display()
                    ));
                }
            }
        },
    }
    match same_filesystem(&cb, dst.path()) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "--backup-dir must be on the same filesystem as the destination \
             (backup-dir={}, destination={})",
            cb.display(),
            dst.path().display()
        )),
        Err(e) => Err(format!("cannot check --backup-dir location: {e}")),
    }
}

/// Total bytes the planned `Copy` actions will write (source sizes; symlinks count as 0).
fn planned_copy_bytes(actions: &[plan::Action], src_m: &manifest::Manifest) -> u64 {
    let sizes: std::collections::HashMap<&Path, u64> =
        src_m.iter().map(|e| (e.rel.as_path(), e.size)).collect();
    actions
        .iter()
        .filter_map(|a| match a {
            plan::Action::Copy(rel) => sizes.get(rel.as_path()).copied(),
            _ => None,
        })
        .sum()
}

/// Free bytes available to unprivileged writes on the filesystem holding `path` (unix `statvfs`);
/// `None` when it can't be determined (then the caller proceeds without a space check).
#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Whether `a` and `b` live on the same filesystem (device). Off-unix, device introspection isn't
/// portable, so the check is skipped (returns `true`).
#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> std::io::Result<bool> {
    Ok(fs_device(a)? == fs_device(b)?)
}

/// Whether `a` and `b` sit on **different** devices — the cue that scanning or hashing them
/// concurrently overlaps independent I/O instead of contending for one disk head. Unknown or
/// off-unix → `false` (stay sequential; never risk thrashing a single spindle). Caveat: two
/// partitions of one physical disk look "different" here — the real win is separate drives.
#[cfg(unix)]
fn different_devices(a: &Path, b: &Path) -> bool {
    matches!((fs_device(a), fs_device(b)), (Ok(da), Ok(db)) if da != db)
}

#[cfg(not(unix))]
fn different_devices(_a: &Path, _b: &Path) -> bool {
    false
}

/// Device id of the filesystem holding `path`, or — if `path` doesn't exist yet — of its nearest
/// existing ancestor (so a not-yet-created backup dir is judged by where it *would* be created).
#[cfg(unix)]
fn fs_device(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut cur = path;
    loop {
        if let Ok(m) = fs::metadata(cur) {
            return Ok(m.dev());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("cannot resolve {}", path.display()),
                ))
            }
        }
    }
}

#[cfg(not(unix))]
fn same_filesystem(_a: &Path, _b: &Path) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn different_devices_is_false_within_one_filesystem() {
        // Two directories on the same filesystem must NOT be judged different devices — so paired
        // scans stay sequential rather than thrashing one disk head.
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert!(!different_devices(&a, &b));
    }

    #[cfg(unix)]
    #[test]
    fn backup_on_same_filesystem_is_allowed() {
        let t = tempfile::tempdir().unwrap();
        let dst = t.path().join("dst");
        fs::create_dir(&dst).unwrap();
        // backup dir doesn't exist yet → judged by its nearest existing ancestor (the tempdir)
        assert!(same_filesystem(&t.path().join("backup"), &dst).unwrap());
    }

    #[test]
    fn validate_rejects_nonexistent_source() {
        let t = tempfile::tempdir().unwrap();
        let err = validate_roots(&t.path().join("nope"), &t.path().join("dst")).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn validate_rejects_file_source() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("f");
        fs::write(&f, b"x").unwrap();
        assert!(validate_roots(&f, &t.path().join("dst")).unwrap_err().contains("not a directory"));
    }

    #[test]
    fn validate_rejects_identical_roots() {
        let t = tempfile::tempdir().unwrap();
        assert!(validate_roots(t.path(), t.path()).unwrap_err().contains("same directory"));
    }

    #[test]
    fn validate_rejects_destination_root() {
        let t = tempfile::tempdir().unwrap();
        let err = validate_roots(t.path(), Path::new("/")).unwrap_err();
        assert!(err.contains("filesystem root"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn validate_rejects_source_root() {
        let t = tempfile::tempdir().unwrap();
        let err = validate_roots(Path::new("/"), t.path()).unwrap_err();
        assert!(err.contains("--from must not be the filesystem root"), "{err}");
    }

    #[test]
    fn validate_rejects_destination_inside_source() {
        let t = tempfile::tempdir().unwrap();
        // destination need not exist yet — canonicalize_lenient resolves its existing prefix
        let err = validate_roots(t.path(), &t.path().join("backup")).unwrap_err();
        assert!(err.contains("--to is inside --from"), "{err}");
    }

    #[test]
    fn validate_rejects_source_inside_destination() {
        let t = tempfile::tempdir().unwrap();
        let sub = t.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let err = validate_roots(&sub, t.path()).unwrap_err();
        assert!(err.contains("--from is inside --to"), "{err}");
    }

    #[test]
    fn validate_accepts_siblings_with_shared_name_prefix() {
        // `foo` must not count as "inside" `foobar` (component-wise, not string prefix)
        let t = tempfile::tempdir().unwrap();
        let foo = t.path().join("foo");
        let foobar = t.path().join("foobar");
        fs::create_dir(&foo).unwrap();
        fs::create_dir(&foobar).unwrap();
        assert!(validate_roots(&foo, &foobar).is_ok());
        assert!(validate_roots(&foobar, &foo).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn validate_detects_overlap_through_symlink() {
        let t = tempfile::tempdir().unwrap();
        let inside = t.path().join("inside");
        fs::create_dir(&inside).unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&inside, &link).unwrap();
        // --to is a symlink resolving to a dir inside --from → must be caught
        let err = validate_roots(t.path(), &link).unwrap_err();
        assert!(err.contains("--to is inside --from"), "{err}");
    }

    #[test]
    fn lenient_canonicalize_extends_existing_prefix() {
        let t = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(t.path()).unwrap();
        assert_eq!(
            canonicalize_lenient(&t.path().join("nope/deep")),
            base.join("nope").join("deep")
        );
    }

    #[test]
    fn lenient_canonicalize_equals_canonicalize_when_present() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(canonicalize_lenient(t.path()), fs::canonicalize(t.path()).unwrap());
    }

    #[test]
    fn lenient_canonicalize_resolves_relative_paths_against_cwd() {
        // A relative, nonexistent path must not stay relative — that would dodge every
        // starts_with overlap check (the `--to backup` bypass).
        let out = canonicalize_lenient(Path::new("filesync-nonexistent-xyz/sub"));
        let cwd = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert!(out.is_absolute(), "must be absolutized: {out:?}");
        assert_eq!(out, cwd.join("filesync-nonexistent-xyz").join("sub"));
    }

    #[test]
    fn lenient_canonicalize_normalizes_dot_and_dotdot_in_missing_tail() {
        let t = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(t.path()).unwrap();
        assert_eq!(
            canonicalize_lenient(&t.path().join("nope/../other/./x")),
            base.join("other").join("x"),
            "`..` and `.` in a not-yet-existing tail must be resolved lexically"
        );
    }

    #[test]
    fn backup_dir_inside_source_is_rejected() {
        let t = tempfile::tempdir().unwrap();
        let (s, d) = (t.path().join("src"), t.path().join("dst"));
        fs::create_dir_all(&s).unwrap();
        fs::create_dir_all(&d).unwrap();
        let err = validate_backup_dir(&s.join("bk"), &SrcRoot::new(&s), &DstRoot::new(&d))
            .unwrap_err();
        assert!(err.contains("overlap the source"), "{err}");
    }

    #[test]
    fn backup_dir_must_be_fresh() {
        let t = tempfile::tempdir().unwrap();
        let (s, d, bk) = (t.path().join("src"), t.path().join("dst"), t.path().join("bk"));
        fs::create_dir_all(&s).unwrap();
        fs::create_dir_all(&d).unwrap();
        let (sr, dr) = (SrcRoot::new(&s), DstRoot::new(&d));

        // absent → ok; empty → ok; non-empty → rejected
        assert!(validate_backup_dir(&bk, &sr, &dr).is_ok(), "absent backup dir is fresh");
        fs::create_dir(&bk).unwrap();
        assert!(validate_backup_dir(&bk, &sr, &dr).is_ok(), "empty backup dir is fresh");
        fs::write(bk.join("leftover"), b"x").unwrap();
        let err = validate_backup_dir(&bk, &sr, &dr).unwrap_err();
        assert!(err.contains("not empty"), "{err}");
    }

    #[test]
    fn backup_dir_may_be_inside_destination_but_not_the_destination() {
        let t = tempfile::tempdir().unwrap();
        let (s, d) = (t.path().join("src"), t.path().join("dst"));
        fs::create_dir_all(&s).unwrap();
        fs::create_dir_all(&d).unwrap();
        let (sr, dr) = (SrcRoot::new(&s), DstRoot::new(&d));
        assert!(validate_backup_dir(&d.join(".trash"), &sr, &dr).is_ok(), "inside dst is fine");
        let err = validate_backup_dir(&d, &sr, &dr).unwrap_err();
        assert!(err.contains("destination itself"), "{err}");
    }

    #[test]
    fn planned_copy_bytes_sums_only_copy_actions() {
        use manifest::{Entry, Kind, Manifest};
        let entry = |rel: &str, size: u64| Entry {
            rel: PathBuf::from(rel),
            kind: Kind::File,
            size,
            mtime: None,
            link_target: None,
            link_id: None,
            owner: None,
            mode: None,
        };
        let m = Manifest::from_sorted(vec![entry("a", 100), entry("b", 7)]);
        let actions = vec![
            plan::Action::Copy(PathBuf::from("a")),
            plan::Action::Delete(PathBuf::from("x")),
            plan::Action::Copy(PathBuf::from("b")),
            plan::Action::Copy(PathBuf::from("not-in-manifest")),
        ];
        assert_eq!(planned_copy_bytes(&actions, &m), 107);
    }
}
