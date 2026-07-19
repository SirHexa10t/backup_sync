//! Execute a plan against the destination.
//!
//! **Only the destination is mutated** — every function that writes/deletes takes a [`DstRoot`], so
//! a source path can't reach them (the type wall). Copies are **atomic** (write to a temp file,
//! then `rename` into place), so an interruption never leaves a half-written real file. Durability
//! is one end-of-run barrier by default ([`crate::durability`]) or per-file with `fsync_each`, and
//! each copied file is **verified** (re-read + hash-compared to the source) unless disabled — a
//! copy that fails verification is removed so a re-run redoes it.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::hash;
use crate::links;
use crate::manifest::{DstRoot, Kind, Manifest, SrcRoot};
use crate::plan::Action;
use crate::progress::Progress;
use crate::reports::Report;

/// Prefix for the temp files that atomic copies write before renaming into place. Deliberately
/// long and specific: scans silently ignore names with this prefix (they're our scratch), so the
/// odds of colliding with real user data must stay astronomically small.
pub const TMP_PREFIX: &str = ".filesync_staging.tmp.";

/// Marker file dropped inside a `--backup-dir` on first use. A directory containing this file is
/// filesync's own move-aside storage: scans exclude it, so a backup dir living inside the
/// destination is never mirrored, deleted, or re-backed-up by later runs — and a used backup dir
/// is recognizable, so it can't be accidentally reused.
pub const BACKUP_MARKER: &str = ".filesync-backup-dir";
const BUF: usize = 1 << 20;

pub struct Options {
    pub verify: bool,
    pub fsync_each: bool,
    pub backup_dir: Option<PathBuf>,
    /// Rewrite into-source symlink targets to relative in-mirror paths ([`crate::links`]).
    pub relative_symlinks: bool,
}

/// Run the plan. Failures are collected into `report.issues`, not propagated — one bad file
/// shouldn't abort the whole backup. Benign skips (special files — nothing to copy by nature) go
/// to `report.skipped` instead, which never affects the exit code. The one deliberate exception
/// to keep-going: once the destination reports **out of space**, the remaining copies are skipped
/// (each would only churn and fail) and a single summary issue says so.
///
/// A set `stop` flag (raised by a signal — see [`crate::interrupt`]) ends the loop **between**
/// actions, so the in-flight file always completes; the finalize (directory metadata, the
/// durability barrier, verify) still runs over the work done, and the report is marked incomplete.
pub fn apply(
    src: &SrcRoot,
    dst: &DstRoot,
    src_m: &Manifest,
    actions: &[Action],
    opts: &Options,
    report: &mut Report,
    progress: &Progress,
    stop: &AtomicBool,
) {
    // copied files: rel path + byte count + source content hash, for the verify stage
    let mut copied: Vec<(PathBuf, u64, blake3::Hash)> = Vec::new();
    let mut refreshed: Vec<PathBuf> = Vec::new();
    let mut disk_full = false;
    let mut skipped_full = 0usize;

    let mut performed = 0usize;
    for action in actions {
        // Graceful stop: checked BETWEEN actions, so the in-flight file always finishes — we just
        // don't start the next one. The finalize steps below still run over the work done.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match action {
            Action::CreateDir(rel) => {
                let path = dst.path().join(rel);
                let first = fs::create_dir_all(&path);
                let made = crate::elevation::retry_if_permission("create directory", &path, first, || {
                    create_dirs_user(&path)
                });
                if let Err(e) = made {
                    report.issue(rel.clone(), &e);
                }
            }
            Action::Rename { from, to } => match do_rename(src, dst, from, to) {
                Ok(()) => report.moved += 1,
                Err(e) => report.issue(to.clone(), &e),
            },
            Action::Delete(rel) => match do_delete(dst, rel, opts) {
                Ok(()) => report.deleted += 1,
                Err(e) => report.issue(rel.clone(), &e),
            },
            Action::RefreshMeta(rel) => {
                match stamp_metadata(&src.path().join(rel), &dst.path().join(rel)) {
                    Ok(()) => report.refreshed += 1,
                    Err(e) => report.issue(rel.clone(), &e),
                }
                refreshed.push(rel.clone());
            }
            Action::HardLink { leader, name } => match do_hard_link(dst, leader, name, opts) {
                Ok(()) => report.linked += 1,
                Err(link_err) => {
                    // Content first: if the destination can't hold another name for the inode
                    // (FAT, or a mount boundary inside the destination), copy independently.
                    match copy_entry(src, dst, name, opts, progress) {
                        Ok(Copied::File { bytes, hash }) => {
                            report.copied += 1;
                            report.bytes_copied += bytes;
                            copied.push((name.clone(), bytes, hash));
                            report.skip_msg(format!(
                                "{}: hard link not possible ({link_err}); copied as an \
                                 independent file",
                                name.display()
                            ));
                        }
                        Ok(_) => report.issue(name.clone(), &link_err),
                        Err(copy_err) => report.issue_msg(format!(
                            "{}: hard link failed ({link_err}) and the fallback copy failed \
                             ({copy_err})",
                            name.display()
                        )),
                    }
                }
            },
            Action::Copy(rel) if disk_full => skipped_full += 1,
            Action::Copy(rel) => match copy_entry(src, dst, rel, opts, progress) {
                Ok(Copied::File { bytes, hash }) => {
                    report.copied += 1;
                    report.bytes_copied += bytes;
                    copied.push((rel.clone(), bytes, hash));
                }
                Ok(Copied::Symlink { broken: false }) => report.copied += 1,
                Ok(Copied::Symlink { broken: true }) => {
                    report.copied += 1;
                    report.issue_msg(format!(
                        "{}: symlink target does not exist (link copied anyway)",
                        rel.display()
                    ));
                }
                Ok(Copied::NoContent(what)) => {
                    report.skip_msg(format!("{}: {what} — no content to copy", rel.display()))
                }
                Ok(Copied::Failed(why)) => report.issue_msg(format!("{}: {why}", rel.display())),
                Err(e) => {
                    if is_disk_full(&e) {
                        disk_full = true;
                    }
                    report.issue(rel.clone(), &e);
                }
            },
        }
        progress.action_done();
        performed += 1;
    }
    if skipped_full > 0 {
        report.issue_msg(format!(
            "destination is full — skipped the remaining {skipped_full} copies; free some space \
             (or use a larger target) and re-run"
        ));
    }

    // Align directory metadata (mtime/permissions) with the source — after all writes, since
    // copying into a directory bumps its mtime. Cheap, and only touches what actually differs.
    mirror_dir_metadata(src, dst, src_m);

    // Durability barrier: unless we fsync'd each file as we went, make the work durable now — data
    // and metadata (renames/deletes) alike. On a graceful stop, only the actions we performed are
    // flushed. See crate::durability.
    if !opts.fsync_each && performed > 0 {
        let mut flushed: Vec<PathBuf> = copied.iter().map(|(rel, _, _)| rel.clone()).collect();
        flushed.extend(refreshed);
        crate::durability::flush_destination(dst, &actions[..performed], &flushed);
    }

    // Verify + correct: re-read each copied file and confirm it matches the source content.
    if opts.verify {
        verify_copied(dst, &copied, report, progress);
    }

    // If a graceful stop cut the loop short, record it — the mirror is incomplete (exit non-zero).
    if performed < actions.len() {
        report.mark_stopped_early(performed, actions.len());
    }
}

/// Make `name` another name for `leader`'s inode at the destination. An existing entry at `name`
/// (a stale name still pointing at a pre-rewrite inode, or an old independent copy) is cleared
/// through the normal delete path first — so `--backup-dir` semantics hold for diverged content
/// (and a stale hard-linked name moved into the backup shares its inode there: zero extra space).
fn do_hard_link(dst: &DstRoot, leader: &Path, name: &Path, opts: &Options) -> io::Result<()> {
    let np = dst.path().join(name);
    if stat_entry(&np).is_ok() {
        do_delete(dst, name, opts)?; // elevated retry lives inside do_delete
    }
    if let Some(parent) = np.parent() {
        let first = fs::create_dir_all(parent);
        crate::elevation::retry_if_permission("create directory", parent, first, || {
            create_dirs_user(parent)
        })?;
    }
    let lp = dst.path().join(leader);
    let first = fs::hard_link(&lp, &np);
    // no chown: a hard link is a new NAME for an existing inode — its ownership is the inode's
    crate::elevation::retry_if_permission("hard link", &np, first, || fs::hard_link(&lp, &np))
}

/// Copy the source entry's mtime and (unix) permissions onto the destination entry. A permission
/// wall (e.g. the destination file is root-owned from an earlier elevated copy or an old sudo cp)
/// is retried with root in reserve — it stamps the same source-derived metadata either way.
fn stamp_metadata(src_abs: &Path, dst_abs: &Path) -> io::Result<()> {
    let first = stamp_once(src_abs, dst_abs);
    crate::elevation::retry_if_permission("refresh metadata", dst_abs, first, || {
        stamp_once(src_abs, dst_abs)
    })
}

fn stamp_once(src_abs: &Path, dst_abs: &Path) -> io::Result<()> {
    let md = fs::metadata(src_abs)?;
    if let Ok(mtime) = md.modified() {
        File::options().write(true).open(dst_abs).and_then(|f| f.set_modified(mtime))?;
    }
    #[cfg(unix)]
    fs::set_permissions(dst_abs, md.permissions())?;
    Ok(())
}

/// Bring every mirrored directory's mtime and (unix) permissions in line with the source. The
/// quick diff never classifies directories as changed (their mtimes churn with content edits), so
/// this pass is what propagates directory metadata. Writes only where something differs — an
/// aligned tree costs two stats per directory and zero writes. Best-effort: filesystems without
/// permissions (FAT) simply won't take them.
fn mirror_dir_metadata(src: &SrcRoot, dst: &DstRoot, src_m: &Manifest) {
    for e in src_m.iter().filter(|e| e.kind == Kind::Dir) {
        let (sp, dp) = (src.path().join(&e.rel), dst.path().join(&e.rel));
        // source stat gets the traversal-wall retry (a dir nested under a root-owned 0700 parent);
        // the destination side is ours
        let (Ok(smd), Ok(dmd)) = (stat_follow(&sp), fs::metadata(&dp)) else {
            continue; // vanished or unreadable — the action loop already reported real problems
        };
        if let (Ok(sm), Ok(dm)) = (smd.modified(), dmd.modified()) {
            let drift = sm.duration_since(dm).or_else(|_| dm.duration_since(sm));
            if drift.map(|d| d > std::time::Duration::from_secs(2)).unwrap_or(true) {
                // dirs can't be opened for writing; set via the path (utimensat under the hood)
                let _ = File::open(&dp).and_then(|f| f.set_modified(sm));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if smd.permissions().mode() & 0o7777 != dmd.permissions().mode() & 0o7777 {
                let _ = fs::set_permissions(&dp, smd.permissions());
            }
        }
    }
}

/// The verify (+ correct) stage: cold-re-read every copied file and hash-compare it against the
/// content read from the source during the copy. A mismatch means the copy is corrupt — it is
/// reported AND **removed from the destination**: the bad file carries the source's size and
/// mtime, so if left in place every later quick-check run would call it "unchanged" forever.
/// Removed, the next run sees the file as missing and simply re-copies it. (Corruption that a
/// `--no-verify` run let through can likewise be healed later by re-running with
/// `--eager-checksum`, which compares content instead of size+mtime.) A copy that can't be
/// re-read is reported but kept — unreadable is not proof of corruption.
fn verify_copied(
    dst: &DstRoot,
    copied: &[(PathBuf, u64, blake3::Hash)],
    report: &mut Report,
    progress: &Progress,
) {
    progress.start_verify(copied.iter().map(|(_, bytes, _)| *bytes).sum());
    for (rel, bytes, want) in copied {
        let path = dst.path().join(rel);
        drop_cache(&path); // verify against the device, not the page cache
        match verify_matches(&path, want) {
            Ok(true) => {}
            Ok(false) => report.issue_msg(match fs::remove_file(&path) {
                Ok(()) => format!(
                    "{}: verify failed — corrupt copy removed from the destination; re-run \
                     filesync to copy it again",
                    rel.display()
                ),
                Err(e) => format!(
                    "{}: verify failed — content mismatch, and removing the corrupt copy also \
                     failed: {e}",
                    rel.display()
                ),
            }),
            Err(e) => report.issue_msg(format!("{}: {e}", rel.display())),
        }
        progress.add_bytes(*bytes);
    }
}

/// True iff the file at `path` hashes to `want`. (The verify check, exposed for testing.)
pub fn verify_matches(path: &Path, want: &blake3::Hash) -> io::Result<bool> {
    Ok(&hash::hash_file(path)? == want)
}

/// Does this error mean the destination filesystem is out of space?
fn is_disk_full(e: &io::Error) -> bool {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ENOSPC) {
        return true;
    }
    e.kind() == io::ErrorKind::StorageFull
}

/// `symlink_metadata` with the elevation retry: when the path sits under a directory this process
/// can't search (e.g. a root-owned 0700 dir), the WALL IS THE STAT ITSELF — it fails EACCES before
/// any open/delete is even attempted, so it needs the same root assist as the operation proper.
fn stat_entry(path: &Path) -> io::Result<fs::Metadata> {
    let first = fs::symlink_metadata(path);
    crate::elevation::retry_if_permission("stat", path, first, || fs::symlink_metadata(path))
}

/// `metadata` (follows symlinks) with the same traversal-wall retry as [`stat_entry`].
fn stat_follow(path: &Path) -> io::Result<fs::Metadata> {
    let first = fs::metadata(path);
    crate::elevation::retry_if_permission("stat", path, first, || fs::metadata(path))
}

/// `create_dir_all` that hands every directory it actually CREATES to the invoking user (matters
/// when running elevated — a root-owned dir at the destination would wall off future unprivileged
/// runs). Unprivileged, the chown is a no-op on our own fresh dirs.
fn create_dirs_user(path: &Path) -> io::Result<()> {
    let mut missing: Vec<&Path> = Vec::new();
    let mut cur = path;
    while !cur.exists() {
        missing.push(cur);
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    for dir in missing.iter().rev() {
        match fs::create_dir(dir) {
            Ok(()) => crate::elevation::chown_to_user(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn do_rename(src: &SrcRoot, dst: &DstRoot, from: &Path, to: &Path) -> io::Result<()> {
    let (fp, tp) = (dst.path().join(from), dst.path().join(to));
    if let Some(parent) = tp.parent() {
        let first = fs::create_dir_all(parent);
        crate::elevation::retry_if_permission("create directory", parent, first, || {
            create_dirs_user(parent)
        })?;
    }
    let first = fs::rename(&fp, &tp);
    crate::elevation::retry_if_permission("rename", &tp, first, || fs::rename(&fp, &tp))?;
    // The renamed file kept the OLD destination mtime. Refresh mtime (+ unix permissions) from
    // the source so the next run's size+mtime quick check sees the moved file as unchanged —
    // otherwise the very file the rename saved would be re-copied next run. Best-effort: a miss
    // only costs a future re-copy, never correctness.
    let _ = stamp_metadata(&src.path().join(to), &tp);
    Ok(())
}

fn do_delete(dst: &DstRoot, rel: &Path, opts: &Options) -> io::Result<()> {
    let path = dst.path().join(rel);
    let md = match stat_entry(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // already gone
        Err(e) => return Err(e),
    };
    if md.is_dir() {
        // empty by delete-order; dirs carry no data
        let first = fs::remove_dir(&path);
        crate::elevation::retry_if_permission("delete directory", &path, first, || {
            fs::remove_dir(&path)
        })
    } else {
        // file or symlink
        match &opts.backup_dir {
            Some(bdir) => move_to_backup(&path, rel, bdir), // move aside instead of erasing
            None => {
                let first = fs::remove_file(&path);
                crate::elevation::retry_if_permission("delete file", &path, first, || {
                    fs::remove_file(&path)
                })
            }
        }
    }
}

/// Move the destination entry at `abs_path` into `bdir`, mirroring its `rel` layout — the shared
/// mechanism behind `--backup-dir` for both deleted and overwritten files. Uses `rename`, so
/// `bdir` must be on the same filesystem as the destination. The marker is written before the
/// first move; if it can't be written, the move fails too (the original stays in place) — a
/// backup dir must never exist unmarked. An elevated retry keeps the moved entry's ownership
/// untouched (that's the point of a backup) but hands the dirs/marker WE created to the user.
fn move_to_backup(abs_path: &Path, rel: &Path, bdir: &Path) -> io::Result<()> {
    let attempt = |elevated: bool| -> io::Result<()> {
        let fresh_marker = !bdir.join(BACKUP_MARKER).exists();
        ensure_backup_marker(bdir)?;
        if elevated && fresh_marker {
            crate::elevation::chown_to_user(bdir);
            crate::elevation::chown_to_user(&bdir.join(BACKUP_MARKER));
        }
        let target = bdir.join(rel);
        if let Some(parent) = target.parent() {
            if elevated {
                create_dirs_user(parent)?;
            } else {
                fs::create_dir_all(parent)?;
            }
        }
        fs::rename(abs_path, &target)
    };
    let first = attempt(false);
    crate::elevation::retry_if_permission("move to backup dir", abs_path, first, || attempt(true))
}

/// Create `bdir` (if needed) and drop [`BACKUP_MARKER`] into it, once.
fn ensure_backup_marker(bdir: &Path) -> io::Result<()> {
    let marker = bdir.join(BACKUP_MARKER);
    if !marker.exists() {
        fs::create_dir_all(bdir)?;
        fs::write(
            &marker,
            "This directory holds files that filesync moved aside (--backup-dir).\n\
             filesync ignores directories containing this marker, so the saved files are never\n\
             mirrored, deleted, or backed up again by later runs. Each run must use a fresh\n\
             backup dir. Delete this file to make filesync treat the directory as normal data.\n",
        )?;
    }
    Ok(())
}

enum Copied {
    File { bytes: u64, hash: blake3::Hash },
    Symlink { broken: bool },
    /// Nothing to copy by nature (special files) → `report.skipped`, exit code unaffected.
    NoContent(&'static str),
    /// Real content that couldn't be reproduced → `report.issues` (needs attention).
    Failed(String),
}

fn copy_entry(
    src: &SrcRoot,
    dst: &DstRoot,
    rel: &Path,
    opts: &Options,
    progress: &Progress,
) -> io::Result<Copied> {
    let sp = src.path().join(rel);
    let ft = stat_entry(&sp)?.file_type(); // traversal itself can be the permission wall

    if ft.is_symlink() {
        let first = fs::read_link(&sp);
        let raw =
            crate::elevation::retry_if_permission("read symlink", &sp, first, || fs::read_link(&sp))?;
        // The target the mirror should carry (rewritten under --relative-symlinks; see links.rs).
        let target = links::desired_target(src, rel, &raw, opts.relative_symlinks);
        match recreate_symlink(&dst.path().join(rel), &target) {
            Ok(()) => {
                // Only meaningful under --relative-symlinks, where targets were resolved anyway:
                // note links whose chain doesn't land on anything (stat follows the chain).
                let broken = opts.relative_symlinks && stat_follow(&sp).is_err();
                Ok(Copied::Symlink { broken })
            }
            // An issue (not a benign skip): a symlink DOES carry information — its target. Record
            // the target in the report so the user can reconstruct the link elsewhere.
            Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(Copied::Failed(format!(
                "symlink (-> {}) not supported on the destination filesystem; recorded here so it \
                 can be reconstructed manually",
                target.display()
            ))),
            Err(e) => Err(e),
        }
    } else if ft.is_file() {
        let (bytes, hash) = copy_file_atomic(&sp, dst, rel, opts, progress)?;
        Ok(Copied::File { bytes, hash })
    } else {
        Ok(Copied::NoContent("special file (fifo/socket/device)"))
    }
}

/// Stream a file to a temp sibling, preserve mtime/perms, then atomically rename into place.
fn copy_file_atomic(
    sp: &Path,
    dst: &DstRoot,
    rel: &Path,
    opts: &Options,
    progress: &Progress,
) -> io::Result<(u64, blake3::Hash)> {
    let final_path = dst.path().join(rel);
    let parent = final_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination path has no parent"))?;
    let mkdirs = fs::create_dir_all(parent);
    crate::elevation::retry_if_permission("create directory", parent, mkdirs, || {
        create_dirs_user(parent)
    })?;

    let fname = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = parent.join(format!("{TMP_PREFIX}{}.{fname}", std::process::id()));

    let src_meta = stat_follow(sp)?; // the stat itself is walled under an unsearchable parent
    // A permission wall here is at open/create time (the source opens before the temp is
    // created), so an elevated retry restarts from zero bytes — no double-counted progress, and
    // the temp is re-created from scratch. A temp created while elevated is handed to the user.
    let first = stream_to_tmp(sp, &tmp, opts.fsync_each, progress);
    let streamed = crate::elevation::retry_if_permission("copy", sp, first, || {
        let r = stream_to_tmp(sp, &tmp, opts.fsync_each, progress);
        if r.is_ok() {
            crate::elevation::chown_to_user(&tmp);
        }
        r
    });
    let (total, hash) = match streamed {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };

    // preserve mtime and (unix) permissions on the temp before it becomes the real file
    if let Ok(mtime) = src_meta.modified() {
        let _ = File::options().write(true).open(&tmp).and_then(|f| f.set_modified(mtime));
    }
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&tmp, src_meta.permissions());
    }

    // If we're about to overwrite an existing entry and a backup dir is set, move the old version
    // aside first — the "overwritten" half of --backup-dir (deletes are handled in do_delete).
    if let Some(bdir) = &opts.backup_dir {
        if fs::symlink_metadata(&final_path).is_ok() {
            if let Err(e) = move_to_backup(&final_path, rel, bdir) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }

    let renamed = fs::rename(&tmp, &final_path);
    if let Err(e) = crate::elevation::retry_if_permission("rename into place", &final_path, renamed, || {
        fs::rename(&tmp, &final_path)
    }) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if opts.fsync_each {
        let _ = File::open(parent).and_then(|f| f.sync_all()); // persist the rename
    }
    Ok((total, hash))
}

/// Stream `sp` into a fresh temp file, hashing as it goes. The hasher lives inside so a retry
/// (e.g. elevated, after a permission wall) starts from a clean slate — never double-fed.
fn stream_to_tmp(
    sp: &Path,
    tmp: &Path,
    fsync: bool,
    progress: &Progress,
) -> io::Result<(u64, blake3::Hash)> {
    let mut hasher = blake3::Hasher::new();
    let mut reader = File::open(sp)?;
    let mut writer = File::create(tmp)?;
    let mut buf = vec![0u8; BUF];
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
    Ok((total, hasher.finalize()))
}

#[cfg(unix)]
fn recreate_symlink(link_path: &Path, target: &Path) -> io::Result<()> {
    let once = || -> io::Result<()> {
        if let Some(parent) = link_path.parent() {
            create_dirs_user(parent)?;
        }
        // replace any existing entry — "nothing there" is fine; a permission failure must
        // surface (it's the retryable wall) instead of decaying into a confusing EEXIST
        match fs::remove_file(link_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::os::unix::fs::symlink(target, link_path)
    };
    let first = once();
    crate::elevation::retry_if_permission("create symlink", link_path, first, || {
        once()?;
        crate::elevation::chown_to_user(link_path); // created while elevated → user-owned
        Ok(())
    })
}

#[cfg(not(unix))]
fn recreate_symlink(_link_path: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "symlinks not supported on this platform"))
}

/// Advise the kernel to drop this file's cached pages, so the following read hits the device
/// instead of the page cache — otherwise verify would just re-read the (correct) in-RAM copy and
/// miss device-level corruption. Best-effort and advisory; only affects already-synced pages.
#[cfg(unix)]
fn drop_cache(path: &Path) {
    use std::os::unix::io::AsRawFd;
    if let Ok(f) = File::open(path) {
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
}

#[cfg(not(unix))]
fn drop_cache(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    // ── verify + correct ─────────────────────────────────────────────────────

    #[test]
    fn verify_removes_a_corrupt_copy_and_reports_it() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("f.txt"), b"what actually landed on the device").unwrap();
        // the hash captured while reading the source — deliberately different content
        let copied =
            vec![(PathBuf::from("f.txt"), 34u64, blake3::hash(b"what the source contained"))];

        let mut report = Report::new();
        verify_copied(&DstRoot::new(t.path()), &copied, &mut report, &Progress::hidden());

        assert!(
            !t.path().join("f.txt").exists(),
            "a corrupt copy must not remain in the mirror — it would look 'unchanged' forever"
        );
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].contains("removed"), "issue must say it was removed: {:?}", report.issues);
    }

    #[test]
    fn verify_keeps_a_correct_copy() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("f.txt"), b"identical content").unwrap();
        let copied = vec![(PathBuf::from("f.txt"), 17u64, blake3::hash(b"identical content"))];

        let mut report = Report::new();
        verify_copied(&DstRoot::new(t.path()), &copied, &mut report, &Progress::hidden());

        assert!(t.path().join("f.txt").is_file(), "a good copy stays");
        assert!(report.issues.is_empty(), "no issues for a good copy: {:?}", report.issues);
    }

    #[test]
    fn verify_reports_an_unreadable_copy_without_pretending_it_is_corrupt() {
        let t = tempfile::tempdir().unwrap();
        // the copied file has vanished (e.g. pulled drive) — an error, but not proof of corruption
        let copied = vec![(PathBuf::from("gone.txt"), 8u64, blake3::hash(b"anything"))];

        let mut report = Report::new();
        verify_copied(&DstRoot::new(t.path()), &copied, &mut report, &Progress::hidden());

        assert_eq!(report.issues.len(), 1);
        assert!(!report.issues[0].contains("removed"), "read errors must not claim removal");
    }
}
