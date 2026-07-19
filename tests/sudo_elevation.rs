//! REAL-root elevation tests — these exercise the actual privilege-drop / per-operation
//! re-escalation path, which needs to START as root. They are `#[ignore]`d so a normal
//! `cargo test` skips them; run them via `sudo ./TEST_SUDO.sh` (which builds as your user and
//! executes only this suite as root).
//!
//! Design: the TEST process stays root the whole time (it never calls `elevation::init`) so it can
//! freely build root-owned fixtures; the code under test is the real `filesync` binary, spawned as
//! a subprocess — it inherits `SUDO_UID`/`SUDO_GID` from the sudo environment and performs its own
//! drop + reserve, exactly as in real use.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Running as root, launched via sudo (so the binary under test has a user to drop to)?
fn sudo_context() -> Option<(u32, u32)> {
    let euid: u32 = String::from_utf8(Command::new("id").arg("-u").output().ok()?.stdout)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let uid: u32 = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid: u32 = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    (euid == 0 && uid != 0).then_some((uid, gid))
}

fn skip() -> Option<(u32, u32)> {
    let ctx = sudo_context();
    if ctx.is_none() {
        eprintln!("skipping: not root-via-sudo — run these through `sudo ./TEST_SUDO.sh`");
    }
    ctx
}

/// chown a path to uid:gid (we're root; direct syscall via the `chown` binary keeps this dep-free).
fn chown(path: &Path, spec: &str, recursive: bool) {
    let mut c = Command::new("chown");
    if recursive {
        c.arg("-R");
    }
    let out = c.arg(spec).arg("--").arg(path).output().expect("run chown");
    assert!(out.status.success(), "chown {spec} {path:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn chmod(path: &Path, mode: u32) {
    let mut p = fs::metadata(path).expect("stat for chmod").permissions();
    p.set_mode(mode);
    fs::set_permissions(path, p).expect("chmod");
}

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Run the real filesync binary (inheriting the sudo env) and return (exit_ok, stdout, stderr).
fn filesync(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_filesync"))
        .args(args)
        .output()
        .expect("run the filesync binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn find_output(dir: &Path, suffix: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(suffix)))
}

fn read_output(dir: &Path, suffix: &str) -> String {
    fs::read_to_string(find_output(dir, suffix).unwrap_or_else(|| panic!("no {suffix} in {dir:?}")))
        .unwrap()
}

/// A tempdir whose tree the DEMOTED filesync can use: everything user-owned; the test then
/// re-roots specific fixtures.
fn user_tree(uid: u32, gid: u32) -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    chown(t.path(), &format!("{uid}:{gid}"), true);
    t
}

/// Core promise: a root-owned, user-unreadable SOURCE file gets backed up anyway — arriving
/// user-owned at the destination — while the source file's ownership/mode stay untouched, and the
/// assist is recorded.
#[test]
#[ignore]
fn sudo_backs_up_root_owned_unreadable_source_file() {
    let Some((uid, gid)) = skip() else { return };
    let base = user_tree(uid, gid);
    let (src, dst, out) = (base.path().join("src"), base.path().join("dst"), base.path().join("out"));
    for d in [&src, &dst, &out] {
        fs::create_dir(d).unwrap();
        chown(d, &format!("{uid}:{gid}"), false);
    }
    write(&src.join("normal.txt"), b"plain");
    chown(&src.join("normal.txt"), &format!("{uid}:{gid}"), false);
    write(&src.join("secret.conf"), b"TOP-SECRET-PAYLOAD");
    chown(&src.join("secret.conf"), "0:0", false);
    chmod(&src.join("secret.conf"), 0o600);

    let (ok, _, err) = filesync(&[
        "sync", "--from", src.to_str().unwrap(), "--to", dst.to_str().unwrap(),
        "--report", out.to_str().unwrap(),
    ]);
    assert!(ok, "elevated sync must succeed cleanly:\n{err}");
    assert!(err.contains("root in reserve"), "startup notice expected:\n{err}");

    // the restricted file was backed up, and the COPY belongs to the user
    let copy = dst.join("secret.conf");
    assert_eq!(fs::read(&copy).unwrap(), b"TOP-SECRET-PAYLOAD");
    let md = fs::metadata(&copy).unwrap();
    assert_eq!(md.uid(), uid, "elevated-created files must be handed to the user");
    // the ORIGINAL is untouched: still root's, still 0600
    let smd = fs::metadata(src.join("secret.conf")).unwrap();
    assert_eq!(smd.uid(), 0, "source ownership must never be modified");
    assert_eq!(smd.mode() & 0o777, 0o600, "source mode must never be modified");
    // the assist is on the record, and the report file itself belongs to the user
    let rep = read_output(&out, ".findings.txt");
    assert!(rep.contains("root-assisted:"), "audit count expected:\n{rep}");
    assert!(rep.contains("% root:"), "per-op audit line expected:\n{rep}");
    let rep_md = fs::metadata(find_output(&out, ".findings.txt").unwrap()).unwrap();
    assert_eq!(rep_md.uid(), uid, "report written while demoted → user-owned");
}

/// An unreadable source DIRECTORY is healed by an elevated re-walk: the scan is complete, so
/// deletion suspension must NOT trigger — genuine extras are deleted in the same run.
#[test]
#[ignore]
fn sudo_heals_unreadable_source_dir_so_deletions_proceed() {
    let Some((uid, gid)) = skip() else { return };
    let base = user_tree(uid, gid);
    let (src, dst, out) = (base.path().join("src"), base.path().join("dst"), base.path().join("out"));
    for d in [&src, &dst, &out] {
        fs::create_dir(d).unwrap();
        chown(d, &format!("{uid}:{gid}"), false);
    }
    write(&src.join("ok.txt"), b"fine");
    chown(&src.join("ok.txt"), &format!("{uid}:{gid}"), false);
    write(&src.join("vault/deep.txt"), b"walled-in content");
    chown(&src.join("vault"), "0:0", true);
    chmod(&src.join("vault"), 0o700);
    write(&dst.join("stale_extra.txt"), b"gone from source");
    chown(&dst.join("stale_extra.txt"), &format!("{uid}:{gid}"), false);

    let (ok, _, err) = filesync(&[
        "sync", "--from", src.to_str().unwrap(), "--to", dst.to_str().unwrap(),
        "--report", out.to_str().unwrap(),
    ]);
    assert!(ok, "healed run must succeed:\n{err}");

    assert_eq!(fs::read(dst.join("vault/deep.txt")).unwrap(), b"walled-in content");
    assert_eq!(fs::metadata(dst.join("vault/deep.txt")).unwrap().uid(), uid);
    assert!(!dst.join("stale_extra.txt").exists(), "scan healed ⇒ suspension must NOT trigger");
    let rep = read_output(&out, ".findings.txt");
    assert!(rep.contains("deleted: 1"), "{rep}");
    assert!(!rep.contains("suspended"), "no suspension in a healed run:\n{rep}");
    assert!(rep.contains("read directory (elevated)"), "the heal is audited:\n{rep}");
}

/// A destination extra locked behind a root-owned parent dir gets deleted through the wall
/// (mirror semantics already doomed it — root only opens the door).
#[test]
#[ignore]
fn sudo_deletes_extra_behind_root_owned_parent() {
    let Some((uid, gid)) = skip() else { return };
    let base = user_tree(uid, gid);
    let (src, dst, out) = (base.path().join("src"), base.path().join("dst"), base.path().join("out"));
    for d in [&src, &dst, &out] {
        fs::create_dir(d).unwrap();
        chown(d, &format!("{uid}:{gid}"), false);
    }
    write(&src.join("keep.txt"), b"k");
    chown(&src.join("keep.txt"), &format!("{uid}:{gid}"), false);
    write(&dst.join("guard/junk.bin"), b"undeletable without root");
    chown(&dst.join("guard"), "0:0", true); // root-owned parent: unlink inside requires root

    let (ok, _, err) = filesync(&[
        "sync", "--from", src.to_str().unwrap(), "--to", dst.to_str().unwrap(),
        "--report", out.to_str().unwrap(),
    ]);
    assert!(ok, "walled deletes must be handled:\n{err}");
    assert!(!dst.join("guard").exists(), "extra dir and its walled content are gone");
    let rep = read_output(&out, ".findings.txt");
    assert!(rep.contains("delete file"), "the elevated delete is audited:\n{rep}");
}

/// `--unelevated` under sudo: privileges dropped permanently — restricted files are reported (and
/// land in showstoppers), never handled; nothing is copied through the wall.
#[test]
#[ignore]
fn sudo_unelevated_reports_instead_of_healing() {
    let Some((uid, gid)) = skip() else { return };
    let base = user_tree(uid, gid);
    let (src, dst, out) = (base.path().join("src"), base.path().join("dst"), base.path().join("out"));
    for d in [&src, &dst, &out] {
        fs::create_dir(d).unwrap();
        chown(d, &format!("{uid}:{gid}"), false);
    }
    write(&src.join("normal.txt"), b"plain");
    chown(&src.join("normal.txt"), &format!("{uid}:{gid}"), false);
    write(&src.join("secret.conf"), b"TOP-SECRET-PAYLOAD");
    chown(&src.join("secret.conf"), "0:0", false);
    chmod(&src.join("secret.conf"), 0o600);

    let (ok, _, err) = filesync(&[
        "sync", "--from", src.to_str().unwrap(), "--to", dst.to_str().unwrap(),
        "--report", out.to_str().unwrap(), "--unelevated",
    ]);
    assert!(!ok, "the walled copy must fail → non-zero exit");
    assert!(err.contains("--unelevated"), "permanent-drop notice expected:\n{err}");
    assert!(!dst.join("secret.conf").exists(), "nothing may pass the wall unelevated");
    assert_eq!(fs::read(dst.join("normal.txt")).unwrap(), b"plain", "the rest still syncs");
    let errors = read_output(&out, ".errors.txt");
    assert!(errors.contains("Permission denied"), "{errors}");
    let rep = read_output(&out, ".findings.txt");
    assert!(!rep.contains("root-assisted"), "no root may have been used:\n{rep}");
    let stoppers = read_output(&out, ".showstoppers.txt");
    assert!(
        stoppers.contains("filesync_source_unreadable_files=("),
        "the wall lands in showstoppers with a remedy:\n{stoppers}"
    );
}
