//! Optional root assistance for restricted-access files.
//!
//! Launched as `sudo filesync …`, the process **immediately drops to the invoking user**
//! (`$SUDO_UID`/`$SUDO_GID`, keeping saved-uid 0) and runs everything normally. Root is held in
//! reserve and used only to retry an operation that failed with a **permission error**
//! (EACCES/EPERM) — and only at the known walls: directory listing, opening for read
//! (hash/copy/verify), deleting a planned extra, renaming, creating/writing at the destination,
//! and metadata stamping. Any other error (EIO, ENOENT, ENOSPC, EROFS, …) is **never** retried
//! with root: those aren't permission walls, and an unforeseen failure must stay loud rather than
//! be rammed through with privilege. Privilege expands *capability*, never *policy* — root does
//! nothing the plan didn't already call for.
//!
//! Guarantees:
//! - Existing files' **ownership and permissions are never modified** by elevation (root reads
//!   them as-is); anything filesync *creates* while elevated is chowned back to the invoking user.
//! - Every root-assisted operation is recorded (see [`drain_audit`]) and lands in the report.
//! - `--unelevated` (or launching without sudo) forgoes all of this: privileges are dropped
//!   permanently and restricted files are reported instead of handled.
//! - A bare root login (no `$SUDO_UID`) is refused — filesync must know which user owns the run.
//!
//! Escalation is **per-thread** on Linux (raw `setresuid` syscall affects only the calling
//! thread, unlike glibc's process-wide broadcast), so a scan thread getting past a wall never
//! makes a concurrent thread run as root. Elsewhere on unix it falls back to a mutex-serialized
//! process-wide `seteuid` window.

#[cfg(unix)]
pub use imp::*;

#[cfg(not(unix))]
pub use stub::*;

#[cfg(unix)]
mod imp {
    use std::io;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    /// The identity to work as (and to hand created files to) while root waits in reserve.
    #[derive(Clone, Copy)]
    struct Reserve {
        uid: libc::uid_t,
        gid: libc::gid_t,
    }

    /// `Some(reserve)` = privileges dropped, re-escalation available. `None` = no elevation.
    static RESERVE: OnceLock<Option<Reserve>> = OnceLock::new();
    static AUDIT: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn reserve() -> Option<Reserve> {
        RESERVE.get().copied().flatten()
    }

    /// Whether a root retry is possible (launched under sudo, not `--unelevated`).
    pub fn available() -> bool {
        reserve().is_some()
    }

    /// Set up the privilege model. Call FIRST, before any filesystem access. Not root → nothing to
    /// do. Root via sudo → drop to `$SUDO_UID`; with `unelevated` the drop is permanent (saved-uid
    /// too), otherwise saved-uid stays 0 so single operations can re-escalate. Idempotent.
    pub fn init(unelevated: bool) -> Result<(), String> {
        if RESERVE.get().is_some() {
            return Ok(()); // already initialized (tests call run() repeatedly)
        }
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            let _ = RESERVE.set(None);
            return Ok(());
        }

        let uid: libc::uid_t = std::env::var("SUDO_UID").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let gid: libc::gid_t = std::env::var("SUDO_GID").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        if uid == 0 {
            return Err("running as plain root — launch via `sudo filesync …` from your user \
                        account instead, so filesync knows which user should own the mirror \
                        (it drops privileges and uses root only past permission walls)"
                .to_string());
        }

        // Supplementary groups first (needs root), then gid, then uid — order matters.
        if let Ok(user) = std::env::var("SUDO_USER") {
            if let Ok(cuser) = std::ffi::CString::new(user) {
                unsafe { libc::initgroups(cuser.as_ptr(), gid) };
            }
        }
        let saved_gid = if unelevated { gid } else { 0 };
        let saved_uid = if unelevated { uid } else { 0 };
        let ok = unsafe {
            libc::setresgid(gid, gid, saved_gid) == 0 && libc::setresuid(uid, uid, saved_uid) == 0
        };
        if !ok || unsafe { libc::geteuid() } != uid {
            return Err("failed to drop root privileges to the invoking user — refusing to run \
                        with full root"
                .to_string());
        }

        if unelevated {
            eprintln!(
                "filesync: --unelevated — root discarded permanently; restricted-access files \
                 will be reported (see the errors file), not handled"
            );
            let _ = RESERVE.set(None);
        } else {
            eprintln!(
                "filesync: running as your user with root in reserve — it is used only to get \
                 past permission walls (EACCES/EPERM) at known operations, never to change \
                 existing files' ownership or permissions; every use is recorded in the report. \
                 Pass --unelevated to forbid root use entirely."
            );
            let _ = RESERVE.set(Some(Reserve { uid, gid }));
        }
        Ok(())
    }

    /// Is this a permission wall (EACCES/EPERM) — the only class of error root may retry?
    pub fn is_permission(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(c) if c == libc::EACCES || c == libc::EPERM)
            || e.kind() == io::ErrorKind::PermissionDenied
    }

    /// This thread runs as root while the guard lives; restored on drop. If restoring ever fails,
    /// the process aborts — continuing to run arbitrary later work as root is never acceptable.
    pub struct ThreadRoot {
        restore: libc::uid_t,
        #[cfg(not(target_os = "linux"))]
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ThreadRoot {
        pub(crate) fn acquire() -> io::Result<Self> {
            let r = reserve()
                .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "no root reserve"))?;
            #[cfg(target_os = "linux")]
            {
                set_thread_euid(0)?;
                Ok(Self { restore: r.uid })
            }
            #[cfg(not(target_os = "linux"))]
            {
                static LOCK: Mutex<()> = Mutex::new(());
                let lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
                if unsafe { libc::seteuid(0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Self { restore: r.uid, _lock: lock })
            }
        }
    }

    impl Drop for ThreadRoot {
        fn drop(&mut self) {
            #[cfg(target_os = "linux")]
            let ok = set_thread_euid(self.restore).is_ok();
            #[cfg(not(target_os = "linux"))]
            let ok = unsafe { libc::seteuid(self.restore) } == 0;
            if !ok || unsafe { libc::geteuid() } != self.restore {
                eprintln!("filesync: FATAL: could not drop root after an elevated operation");
                std::process::abort();
            }
        }
    }

    /// Change only the CALLING THREAD's effective uid (raw syscall — glibc's wrapper broadcasts
    /// to every thread, which would make unrelated concurrent work run as root).
    #[cfg(target_os = "linux")]
    fn set_thread_euid(euid: libc::uid_t) -> io::Result<()> {
        let keep = -1 as libc::c_long;
        let r = unsafe { libc::syscall(libc::SYS_setresuid, keep, euid as libc::c_long, keep) };
        if r == 0 && unsafe { libc::geteuid() } == euid {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// The elevation gate. If `first` failed with EACCES/EPERM and root is in reserve, run `op`
    /// once with this thread elevated and record the assist on success. Anything else passes
    /// through untouched — unknown errors are never retried with root.
    pub fn retry_if_permission<T>(
        what: &str,
        path: &Path,
        first: io::Result<T>,
        op: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let Err(e) = &first else { return first };
        if !is_permission(e) || !available() {
            return first;
        }
        let Ok(_guard) = ThreadRoot::acquire() else { return first };
        match op() {
            Ok(v) => {
                record(format!("{what}: {}", path.display()));
                Ok(v)
            }
            // The elevated error is the truer one (e.g. EROFS behind the permission wall).
            Err(e2) => Err(e2),
        }
    }

    /// Hand a file/dir/symlink CREATED while elevated to the invoking user (no-op without a
    /// reserve; never applied to pre-existing files). Best-effort.
    pub fn chown_to_user(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        if let Some(r) = reserve() {
            if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
                unsafe { libc::lchown(c.as_ptr(), r.uid, r.gid) };
            }
        }
    }

    /// Record one root-assisted operation (drained into the report at the end of the run).
    pub fn record(msg: String) {
        AUDIT.lock().unwrap_or_else(|p| p.into_inner()).push(msg);
    }

    /// Take (and clear) the audit trail of root-assisted operations.
    pub fn drain_audit() -> Vec<String> {
        std::mem::take(&mut *AUDIT.lock().unwrap_or_else(|p| p.into_inner()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn init_without_root_leaves_elevation_off() {
            if unsafe { libc::geteuid() } == 0 {
                eprintln!("skipping: running as root");
                return;
            }
            assert!(init(false).is_ok());
            assert!(!available(), "no sudo launch → no reserve");
        }

        #[test]
        fn permission_errors_are_the_only_retry_class() {
            for (code, expect) in [(libc::EACCES, true), (libc::EPERM, true), (libc::EIO, false), (libc::ENOENT, false), (libc::ENOSPC, false), (libc::EROFS, false)] {
                let e = io::Error::from_raw_os_error(code);
                assert_eq!(is_permission(&e), expect, "errno {code}");
            }
        }

        #[test]
        fn retry_passes_through_untouched_when_no_reserve() {
            if unsafe { libc::geteuid() } == 0 {
                eprintln!("skipping: running as root");
                return;
            }
            let _ = init(false);
            // success passes through, op untouched
            let r = retry_if_permission("t", Path::new("/x"), Ok(7), || panic!("must not run"));
            assert_eq!(r.unwrap(), 7);
            // permission error without a reserve: original error returned, op untouched
            let first: io::Result<u8> = Err(io::Error::from_raw_os_error(libc::EACCES));
            let r = retry_if_permission("t", Path::new("/x"), first, || panic!("must not run"));
            assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EACCES));
            // non-permission error: same, and it must never be considered for retry
            let first: io::Result<u8> = Err(io::Error::from_raw_os_error(libc::EIO));
            let r = retry_if_permission("t", Path::new("/x"), first, || panic!("must not run"));
            assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EIO));
        }

        #[test]
        fn audit_records_and_drains() {
            record("test op: /somewhere".into());
            let a = drain_audit();
            assert!(a.iter().any(|m| m.contains("test op")));
            assert!(drain_audit().is_empty(), "drain clears");
        }
    }
}

#[cfg(not(unix))]
mod stub {
    use std::io;
    use std::path::Path;

    /// Never constructible off-unix — `acquire` always fails (and `available()` is false, so no
    /// caller reaches it).
    pub struct ThreadRoot {}
    impl ThreadRoot {
        pub(crate) fn acquire() -> io::Result<Self> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "no elevation on this platform"))
        }
    }

    pub fn init(_unelevated: bool) -> Result<(), String> {
        Ok(()) // no unix privilege model here; elevation is simply unavailable
    }
    pub fn available() -> bool {
        false
    }
    pub fn is_permission(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::PermissionDenied
    }
    pub fn retry_if_permission<T>(
        _what: &str,
        _path: &Path,
        first: io::Result<T>,
        _op: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        first
    }
    pub fn chown_to_user(_path: &Path) {}
    pub fn record(_msg: String) {}
    pub fn drain_audit() -> Vec<String> {
        Vec::new()
    }
}
