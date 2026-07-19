//! Everything the `diff` command reports: the four output files AND the terminal summary. The
//! prints are reporting too — the compact counts, the suspension preview, where each file went —
//! so they live here with the files, not in the command driver. (Live progress is the exception:
//! updates, not reports — `crate::progress_update`.)

use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use crate::diff::Diff;
use crate::manifest::{DstRoot, SrcRoot};
use crate::scan::ScanOutcome;

use super::{conclusions, errors, findings, showstoppers, write_diag, OutputPaths};

/// Write the diff's output files into `out_dir` and print the terminal summary. `audit` is the
/// drained root-assist trail; `elevation_available` gates the sudo hint (and showstopper
/// predictions). The diff is a preview — it never fails the run, so nothing is returned.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit(
    out_dir: &Path,
    src: &SrcRoot,
    src_scan: &ScanOutcome,
    dst: &DstRoot,
    dst_scan: &ScanOutcome,
    d: &Diff,
    audit: &[String],
    elevation_available: bool,
) {
    let (src_m, dst_m) = (&src_scan.manifest, &dst_scan.manifest);

    // Everything that needs attention, each line naming its side — bound for the errors file.
    // The two trees fail very differently: a source read gap risks your data, a destination one
    // usually doesn't.
    let mut issues: Vec<String> = Vec::new();
    for e in &src_scan.errors {
        issues.push(format!("source: {e}"));
    }
    for e in &dst_scan.errors {
        issues.push(format!("destination: {e}"));
    }
    for p in &src_scan.skipped_backup_dirs {
        issues.push(format!(
            "source: ignoring backup dir (has {}): {}",
            crate::artifacts::BACKUP_MARKER,
            p.display()
        ));
    }
    for p in &dst_scan.skipped_backup_dirs {
        issues.push(format!(
            "destination: ignoring backup dir (has {}): {}",
            crate::artifacts::BACKUP_MARKER,
            p.display()
        ));
    }
    issues.extend(d.issues.iter().cloned());

    // Preview honestly: a sync would refuse the destructive parts of this diff while the source
    // view is incomplete — an unreadable directory (scan) or an unreadable file caught during
    // classification (`d.source_unreadable`). Renames onto occupied targets are also deferred by
    // a sync (their target-clearing deletes are suspended, and a raw rename would erase the
    // occupant) — count them so the preview matches what run_sync would actually do. High-signal,
    // so it goes on the terminal, not just the file.
    let src_scan_incomplete =
        !src_scan.errors.is_empty() || !src_scan.skipped_backup_dirs.is_empty();
    let occupied_renames = if src_scan_incomplete || d.source_unreadable {
        let dst_paths: HashSet<&Path> = dst_m.iter().map(|e| e.rel.as_path()).collect();
        d.moved.iter().filter(|m| dst_paths.contains(m.to.as_path())).count()
    } else {
        0
    };
    let suspend_note = ((src_scan_incomplete || d.source_unreadable)
        && (!d.removed.is_empty() || !d.to_link.is_empty() || occupied_renames > 0))
    .then(|| {
        format!(
            "note: a sync would SUSPEND the {} deletion(s) and defer the {} hard-link update(s) \
             and {} occupied-target rename(s) listed — the source was not fully readable",
            d.removed.len(),
            d.to_link.len(),
            occupied_renames
        )
    });

    // The output files, on one de-duplicated stem: full findings and conclusions always; errors
    // and showstoppers only when there is something to show.
    let paths = OutputPaths::build(out_dir, "diff", src.path(), SystemTime::now());
    let (src_disp, dst_disp) = (src.path().display().to_string(), dst.path().display().to_string());

    let wrote_report =
        findings::write_diff(&paths.report, &src_disp, &dst_disp, &d.render(true), audit);

    let conclusions_body = conclusions::analyze(d, src_m, dst_m).render(&src_disp, &dst_disp);
    let wrote_conclusions = write_diag(&paths.conclusions, &conclusions_body, "conclusions");

    let wrote_errors = !issues.is_empty() && errors::write_diff_errors(&paths.errors, &issues);

    let stoppers = showstoppers::analyze(
        src,
        src_m,
        &src_scan.denied,
        dst,
        dst_m,
        &dst_scan.denied,
        d,
        elevation_available,
    );
    let wrote_stoppers =
        !stoppers.is_empty() && write_diag(&paths.showstoppers, &stoppers.render(), "showstoppers");

    // Terminal: the compact count summary, the suspension preview, and where the detail went —
    // never the full dump.
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
    if !elevation_available && issues.iter().any(|i| i.contains("Permission denied")) {
        println!(
            "hint: permission-denied issues — run under `sudo filesync` to let it handle \
             restricted-access files (root is used only at those walls, and every use is \
             recorded), or fix them manually."
        );
    }
}
