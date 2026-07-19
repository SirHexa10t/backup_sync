//! Forbid concurrent syncs onto one destination.
//!
//! Two filesync processes interleaving on the same destination make a mess: each would sweep the
//! other's live staging files, and both plan from snapshots the other is invalidating. An
//! exclusive lockfile at the destination root turns that into a clear refusal. The lock is
//! advisory (a cooperating-filesync protocol, not an OS lock): it holds the owning process id,
//! and a leftover lock whose process is provably dead (unix) is reclaimed automatically — a crash
//! must not wedge unattended backups forever.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use crate::manifest::DstRoot;

use crate::artifacts::LOCK_FILE;

/// A held lock — released (deleted) on drop, including early returns.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Take the destination lock, or explain why not. `create_new` makes creation atomic — of two
    /// racing processes exactly one wins. A pre-existing lock is reclaimed only when its recorded
    /// process is provably gone.
    pub fn acquire(dst: &DstRoot) -> io::Result<Lock> {
        let path = dst.path().join(LOCK_FILE);
        for attempt in 0..2 {
            match File::create_new(&path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                    if holder_is_dead(&path) {
                        let _ = fs::remove_file(&path); // stale (crashed run) — reclaim and retry
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "another filesync is already syncing this destination (lock: {}). \
                             If you are certain none is running, delete the lock file and retry.",
                            path.display()
                        ),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("second attempt either creates the lock or returns an error");
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Is the process recorded in the lockfile provably dead? Unreadable/garbled contents count as
/// alive (conservative: never steal a lock we can't judge). Off-unix there's no portable liveness
/// probe, so a leftover lock always needs manual removal there.
#[cfg(unix)]
fn holder_is_dead(path: &std::path::Path) -> bool {
    let Some(pid) = fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<i32>().ok())
    else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // kill(pid, 0) delivers nothing; it only checks existence. ESRCH = no such process.
    unsafe { libc::kill(pid, 0) == -1 && *libc::__errno_location() == libc::ESRCH }
}

#[cfg(not(unix))]
fn holder_is_dead(_path: &std::path::Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let t = tempfile::tempdir().unwrap();
        let dst = DstRoot::new(t.path());

        let lock = Lock::acquire(&dst).expect("first acquire succeeds");
        let second_err = Lock::acquire(&dst).err().expect("a live lock must refuse a second run");
        assert!(second_err.to_string().contains("another filesync"));

        drop(lock);
        assert!(!t.path().join(LOCK_FILE).exists(), "released on drop");
        Lock::acquire(&dst).expect("acquirable again after release");
    }

    #[cfg(unix)]
    #[test]
    fn stale_lock_from_a_dead_process_is_reclaimed() {
        let t = tempfile::tempdir().unwrap();
        let dst = DstRoot::new(t.path());

        // a process id that certainly belonged to a dead process: a reaped child of ours
        let dead_pid = std::process::Command::new("true")
            .status()
            .map(|_| {
                let child = std::process::Command::new("true").spawn().unwrap();
                let pid = child.id();
                let _ = child.wait_with_output(); // reaped ⇒ pid is free/dead
                pid
            })
            .unwrap();
        fs::write(t.path().join(LOCK_FILE), format!("{dead_pid}\n")).unwrap();

        Lock::acquire(&dst).expect("a provably-dead holder's lock is reclaimed");
    }

    #[test]
    fn garbled_lock_is_not_stolen() {
        let t = tempfile::tempdir().unwrap();
        let dst = DstRoot::new(t.path());
        fs::write(t.path().join(LOCK_FILE), b"not a pid").unwrap();
        assert!(Lock::acquire(&dst).is_err(), "unjudgable locks are treated as held");
    }
}
