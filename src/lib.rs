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
pub mod hash;
pub mod manifest;
pub mod parallel;
pub mod plan;
pub mod report;
pub mod scan;
pub mod target;

pub use cli::{Cli, Command};

use std::fs;
use std::process::ExitCode;
use std::time::SystemTime;

use manifest::{DstRoot, Kind, SrcRoot};

/// Program entry point, called from `main`.
pub fn run(cli: Cli) -> ExitCode {
    let common = cli.command.common();

    if !common.from.is_dir() {
        eprintln!("filesync: source is not a directory: {}", common.from.display());
        return ExitCode::FAILURE;
    }
    if common.from == common.to {
        eprintln!("filesync: --from and --to are the same directory");
        return ExitCode::FAILURE;
    }

    let src = SrcRoot::new(&common.from);
    let dst = DstRoot::new(&common.to);

    match &cli.command {
        Command::Diff(a) => {
            let src_m = scan::scan(src.path());
            let dst_m = scan::scan(dst.path());
            match diff::diff(&src, &src_m, &dst, &dst_m, a.common.eager_checksum, a.common.jobs) {
                Ok(d) => {
                    print!("{}", d.render());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("filesync diff: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Sync(a) => run_sync(&src, &dst, a),
    }
}

fn run_sync(src: &SrcRoot, dst: &DstRoot, a: &cli::SyncArgs) -> ExitCode {
    if let Err(e) = fs::create_dir_all(dst.path()) {
        eprintln!("filesync sync: cannot create destination {}: {e}", dst.path().display());
        return ExitCode::FAILURE;
    }

    // Clean up any temp files a previous, interrupted run left behind.
    let swept = apply::sweep_temp_files(dst);
    if swept > 0 {
        eprintln!("filesync: removed {swept} leftover temp file(s) from a previous run");
    }

    let src_m = scan::scan(src.path());
    let dst_m = scan::scan(dst.path());
    let d = match diff::diff(src, &src_m, dst, &dst_m, a.common.eager_checksum, a.common.jobs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("filesync sync: {e}");
            return ExitCode::FAILURE;
        }
    };

    let actions = plan::plan(&d);
    let opts = apply::Options {
        verify: !a.no_verify,
        fsync_each: a.fsync_each,
        backup_dir: a.backup_dir.clone(),
        jobs: a.common.jobs,
    };

    // Open the (streamed) report; fall back to in-memory if the file can't be created.
    let report_path = a
        .common
        .report
        .clone()
        .unwrap_or_else(|| report::default_report_path(src.path(), SystemTime::now()));
    let mut report = report::Report::create(&report_path).unwrap_or_else(|e| {
        eprintln!("filesync sync: cannot open report {} ({e}); continuing without a report file", report_path.display());
        report::Report::new()
    });

    // Warn up front about destination limitations that will force skips.
    let caps = target::probe(dst);
    if !caps.symlinks {
        let n = src_m.iter().filter(|e| e.kind == Kind::Symlink).count();
        if n > 0 {
            report.issue_msg(format!("destination cannot store symlinks; {n} will be skipped"));
        }
    }

    apply::apply(src, dst, &actions, &opts, &mut report);
    report.finish();

    print!("{}", report.render());
    println!("report: {}", report_path.display());

    if report.issues.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
