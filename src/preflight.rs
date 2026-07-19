//! Preflight validation: refuse bad configurations before touching a single file. Everything here
//! runs against *canonical* paths (see [`canonicalize_lenient`]), so no overlap or placement rule
//! can be dodged with a symlink, `..`, or a not-yet-created tail.

use std::fs;
use std::path::{Path, PathBuf};

use crate::device::same_filesystem;
use crate::manifest::{DstRoot, SrcRoot};

/// Resolve the directory this run writes its output files into, and refuse to place it inside either
/// tree: inside the read-only source it can't be written (and would be backed up as data); inside
/// the destination the next sync would mirror-delete the files as extras. With no `--report`, output
/// goes to the current directory; a `--report` argument must be an existing directory.
pub(crate) fn resolve_output_dir(
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
pub(crate) fn validate_roots(from: &Path, to: &Path) -> Result<(), String> {
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
/// (Also used by `crate::links` to resolve symlink chains whose targets may not exist.)
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
///    (A backup dir *inside* the destination is fine: it gets a [`crate::artifacts::BACKUP_MARKER`]
///    on first use, and scans skip marked dirs, so later runs never mirror, delete, or re-back-up
///    it.)
/// 3. **Fresh** — absent or an empty directory. Reusing a dir (marked from a previous run, or one
///    holding unrelated data) invites silent collisions: `rename` would overwrite same-named
///    entries in it. One run, one backup dir.
/// 4. **Same filesystem as the destination** — move-aside uses `rename`, which can't cross devices.
pub(crate) fn validate_backup_dir(bdir: &Path, src: &SrcRoot, dst: &DstRoot) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
}
