//! The `sync` command's reporting that isn't already streamed through [`super::Report`]: the
//! showstoppers forecast file, and the end-of-run terminal summary (counts, file pointers, issue
//! surfacing, the sudo hint). The prints are reporting too — they live here with the files.

use std::path::PathBuf;

use crate::diff::Diff;
use crate::manifest::{DstRoot, Manifest, SrcRoot};

use super::{showstoppers, write_diag, OutputPaths, Report};

/// Write the showstoppers forecast (permission walls this run will hit — or, under sudo, the ones
/// that already resisted root). Written before apply, so even an interrupted run leaves it; only
/// when there is something to show. Returns the written path and item count.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_showstoppers(
    paths: &OutputPaths,
    src: &SrcRoot,
    src_m: &Manifest,
    src_denied: &[PathBuf],
    dst: &DstRoot,
    dst_m: &Manifest,
    dst_denied: &[PathBuf],
    d: &Diff,
    elevation_available: bool,
) -> Option<(PathBuf, usize)> {
    let stoppers = showstoppers::analyze(
        src,
        src_m,
        src_denied,
        dst,
        dst_m,
        dst_denied,
        d,
        elevation_available,
    );
    (!stoppers.is_empty()
        && write_diag(&paths.showstoppers, &stoppers.render(), "showstoppers"))
    .then(|| (paths.showstoppers.clone(), stoppers.total()))
}

/// The end-of-run terminal summary: the report's counts (+ skips, root assists), where the report
/// and showstoppers files went, the issues (pointer to the errors file, or inline when no file
/// could back them), and — when permission walls were hit without root in reserve — the sudo hint.
pub(crate) fn print_summary(
    report: &Report,
    paths: &OutputPaths,
    stoppers: &Option<(PathBuf, usize)>,
    elevation_available: bool,
) {
    print!("{}", report.render());
    if report.has_file() {
        println!("report: {}", paths.report.display());
    }
    if let Some((path, total)) = stoppers {
        println!("showstoppers: {}  ({total} item(s) — paste-able remedies inside)", path.display());
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
    if !elevation_available && report.issues.iter().any(|i| i.contains("Permission denied")) {
        println!(
            "hint: permission-denied issues above — run under `sudo filesync` to let it handle \
             restricted-access files (it drops to your user and uses root only at those walls, \
             recording every use), or fix them manually."
        );
    }
}
