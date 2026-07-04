//! Execute a plan against the destination.
//!
//! **Only the destination is mutated** — every function that writes/deletes takes a [`DstRoot`], so
//! a source path can't reach them (the type wall). Copies are **atomic** (write to a temp file,
//! then `rename` into place), so an interruption never leaves a half-written real file. Durability
//! is one end-of-run sync by default (or per-file with `fsync_each`), and each copied file is
//! **verified** (re-read + hash-compared to the source) unless disabled.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::hash;
use crate::manifest::{DstRoot, SrcRoot};
use crate::plan::Action;
use crate::report::Report;

/// Prefix for the temp files that atomic copies write before renaming into place.
pub const TMP_PREFIX: &str = ".filesync.tmp.";
const BUF: usize = 1 << 20;

pub struct Options {
    pub verify: bool,
    pub fsync_each: bool,
    pub backup_dir: Option<PathBuf>,
    /// Worker threads for the verify hashing (1 = sequential).
    pub jobs: usize,
}

/// Remove any leftover atomic-copy temp files under the destination (from a prior interrupted
/// run). Safe to call before a sync; returns how many were removed.
pub fn sweep_temp_files(dst: &DstRoot) -> usize {
    let mut removed = 0;
    for entry in WalkDir::new(dst.path()).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file()
            && entry.file_name().to_string_lossy().starts_with(TMP_PREFIX)
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Run the plan. Failures are collected into `report.issues`, not propagated — one bad file
/// shouldn't abort the whole backup.
pub fn apply(src: &SrcRoot, dst: &DstRoot, actions: &[Action], opts: &Options, report: &mut Report) {
    // copied files + their source content hash, for the verify stage
    let mut copied: Vec<(PathBuf, blake3::Hash)> = Vec::new();

    for action in actions {
        match action {
            Action::CreateDir(rel) => {
                if let Err(e) = fs::create_dir_all(dst.path().join(rel)) {
                    report.issue(rel.clone(), &e);
                }
            }
            Action::Rename { from, to } => match do_rename(dst, from, to) {
                Ok(()) => report.moved += 1,
                Err(e) => report.issue(to.clone(), &e),
            },
            Action::Delete(rel) => match do_delete(dst, rel, opts) {
                Ok(()) => report.deleted += 1,
                Err(e) => report.issue(rel.clone(), &e),
            },
            Action::Copy(rel) => match copy_entry(src, dst, rel, opts) {
                Ok(Copied::File { bytes, hash }) => {
                    report.copied += 1;
                    report.bytes_copied += bytes;
                    copied.push((rel.clone(), hash));
                }
                Ok(Copied::Symlink) => report.copied += 1,
                Ok(Copied::Skipped(why)) => report.issue_msg(format!("{}: {why}", rel.display())),
                Err(e) => report.issue(rel.clone(), &e),
            },
        }
    }

    // Durability barrier: if we didn't fsync each file as we went, flush the copies now.
    if !opts.fsync_each {
        for (rel, _) in &copied {
            let _ = File::open(dst.path().join(rel)).and_then(|f| f.sync_all());
        }
    }

    // Verify: re-read each copied file and confirm it matches the source content.
    if opts.verify {
        // Parallelizable read work (sequential unless --jobs > 1). Collect problems, then record
        // them — keeping the report update single-threaded.
        let idx: Vec<usize> = (0..copied.len()).collect();
        let problems = crate::parallel::map(opts.jobs, idx, |i| {
            let (rel, want) = &copied[i];
            let path = dst.path().join(rel);
            drop_cache(&path); // verify against the device, not the page cache
            match verify_matches(&path, want) {
                Ok(true) => None,
                Ok(false) => Some(format!("{}: verify failed — content mismatch after copy", rel.display())),
                Err(e) => Some(format!("{}: {e}", rel.display())),
            }
        });
        for p in problems.into_iter().flatten() {
            report.issue_msg(p);
        }
    }
}

/// True iff the file at `path` hashes to `want`. (The verify check, exposed for testing.)
pub fn verify_matches(path: &Path, want: &blake3::Hash) -> io::Result<bool> {
    Ok(&hash::hash_file(path)? == want)
}

fn do_rename(dst: &DstRoot, from: &Path, to: &Path) -> io::Result<()> {
    let (fp, tp) = (dst.path().join(from), dst.path().join(to));
    if let Some(parent) = tp.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&fp, &tp)
}

fn do_delete(dst: &DstRoot, rel: &Path, opts: &Options) -> io::Result<()> {
    let path = dst.path().join(rel);
    let md = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()), // already gone
        Err(e) => return Err(e),
    };
    if md.is_dir() {
        fs::remove_dir(&path) // empty by delete-order; dirs carry no data
    } else {
        // file or symlink
        match &opts.backup_dir {
            Some(bdir) => {
                let target = bdir.join(rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&path, &target) // move aside instead of erasing
            }
            None => fs::remove_file(&path),
        }
    }
}

enum Copied {
    File { bytes: u64, hash: blake3::Hash },
    Symlink,
    Skipped(String),
}

fn copy_entry(src: &SrcRoot, dst: &DstRoot, rel: &Path, opts: &Options) -> io::Result<Copied> {
    let sp = src.path().join(rel);
    let ft = fs::symlink_metadata(&sp)?.file_type();

    if ft.is_symlink() {
        let target = fs::read_link(&sp)?;
        match recreate_symlink(&dst.path().join(rel), &target) {
            Ok(()) => Ok(Copied::Symlink),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                Ok(Copied::Skipped(format!("symlink unsupported on destination ({e})")))
            }
            Err(e) => Err(e),
        }
    } else if ft.is_file() {
        let (bytes, hash) = copy_file_atomic(&sp, dst, rel, opts)?;
        Ok(Copied::File { bytes, hash })
    } else {
        Ok(Copied::Skipped("unsupported file type (fifo/socket/device)".into()))
    }
}

/// Stream a file to a temp sibling, preserve mtime/perms, then atomically rename into place.
fn copy_file_atomic(
    sp: &Path,
    dst: &DstRoot,
    rel: &Path,
    opts: &Options,
) -> io::Result<(u64, blake3::Hash)> {
    let final_path = dst.path().join(rel);
    let parent = final_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination path has no parent"))?;
    fs::create_dir_all(parent)?;

    let fname = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = parent.join(format!("{TMP_PREFIX}{}.{fname}", std::process::id()));

    let src_meta = fs::metadata(sp)?;
    let mut hasher = blake3::Hasher::new();
    let total = match stream_to_tmp(sp, &tmp, &mut hasher, opts.fsync_each) {
        Ok(n) => n,
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

    if let Err(e) = fs::rename(&tmp, &final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if opts.fsync_each {
        let _ = File::open(parent).and_then(|f| f.sync_all()); // persist the rename
    }
    Ok((total, hasher.finalize()))
}

fn stream_to_tmp(sp: &Path, tmp: &Path, hasher: &mut blake3::Hasher, fsync: bool) -> io::Result<u64> {
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
    }
    writer.flush()?;
    if fsync {
        writer.sync_all()?;
    }
    Ok(total)
}

#[cfg(unix)]
fn recreate_symlink(link_path: &Path, target: &Path) -> io::Result<()> {
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(link_path); // replace any existing entry
    std::os::unix::fs::symlink(target, link_path)
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
