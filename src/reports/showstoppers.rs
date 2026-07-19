//! The showstoppers file: everything that blocks a faithful mirror, with paste-able remedies.
//!
//! Conclusions answer "what would a sync change?"; **showstoppers answer "what prevents the mirror
//! from converging, and how do I unblock it?"** — unreadable source data (can't be backed up),
//! undeletable destination extras (mirror-delete will fail), unwritable destination directories
//! (copies into them will fail).
//!
//! The rendered file is deliberately **not a runnable script**. Each section is a bash *array* of
//! absolute paths (verbose, distinct name) followed by the loop that applies that section's remedy
//! to the array — the user pastes the array, reviews it, then pastes the loop. Destructive actions
//! require that active, conscious step. Paths are emitted as bash literals (`'…'`, or ANSI-C
//! `$'…'` when the name carries quotes/control characters — newlines included), so bash itself
//! reconstructs the exact raw filename when the array is defined; the loops expand `"${arr[@]}"`
//! quoted and pass `--` before paths, so any filename survives the round-trip.
//!
//! Detection is two-tier: **confirmed** items actually failed during this run (scan/classify);
//! **predicted** items come from permission arithmetic over the owner/mode bits the scan already
//! collected (zero extra syscalls) — ACLs can overrule those bits either way, hence the marker.
//! When root is in reserve (a sudo launch), predictions are skipped entirely: those walls will be
//! handled, so only what actually still failed is a showstopper.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::Diff;
use crate::manifest::{DstRoot, Entry, Kind, Manifest, SrcRoot};

/// One blocked path, with what the remedy commands need to know about it.
#[derive(Debug, Clone)]
pub struct Item {
    pub abs: PathBuf,
    pub owner: Option<(u32, u32)>,
    /// Raw `st_mode`; rendered as the familiar 4-digit octal of the permission bits.
    pub mode: Option<u32>,
    /// True when an operation actually failed here this run; false for permission-math predictions.
    pub confirmed: bool,
}

/// The blocked paths, grouped by the remedy they need. A path can appear in several groups
/// (e.g. both unreadable and undeletable) — each group is an independent fix.
#[derive(Debug, Default)]
pub struct Showstoppers {
    pub source_unreadable_dirs: Vec<Item>,
    pub source_unreadable_files: Vec<Item>,
    pub destination_unreadable_dirs: Vec<Item>,
    pub destination_unreadable_files: Vec<Item>,
    pub destination_undeletable_files: Vec<Item>,
    pub destination_undeletable_dirs: Vec<Item>,
    pub destination_unwritable_dirs: Vec<Item>,
}

impl Showstoppers {
    fn sections(&self) -> [&Vec<Item>; 7] {
        [
            &self.source_unreadable_dirs,
            &self.source_unreadable_files,
            &self.destination_unreadable_dirs,
            &self.destination_unreadable_files,
            &self.destination_undeletable_files,
            &self.destination_undeletable_dirs,
            &self.destination_unwritable_dirs,
        ]
    }

    pub fn is_empty(&self) -> bool {
        self.sections().iter().all(|s| s.is_empty())
    }

    pub fn total(&self) -> usize {
        self.sections().iter().map(|s| s.len()).sum()
    }
}

/// Compute the showstoppers from what the run saw. `src_denied`/`dst_denied` are the scans'
/// permission failures; the diff carries the classify-time ones; predictions come from the
/// manifests' owner/mode bits (skipped when `elevation_available` — root will handle those walls).
pub fn analyze(
    src: &SrcRoot,
    src_m: &Manifest,
    src_denied: &[PathBuf],
    dst: &DstRoot,
    dst_m: &Manifest,
    dst_denied: &[PathBuf],
    d: &Diff,
    elevation_available: bool,
) -> Showstoppers {
    let mut s = Showstoppers::default();
    // one seen-set per section, so a confirmed item suppresses its own prediction
    let mut seen: [HashSet<PathBuf>; 7] = Default::default();

    // Absolute paths throughout — the file's whole point is paste-into-a-shell handling, which
    // must not depend on the CWD the user happens to be in.
    let src_abs = absolutize(src.path());
    let dst_abs = absolutize(dst.path());
    let (src, dst) = (&SrcRoot::new(&src_abs), &DstRoot::new(&dst_abs));

    let src_by_path: std::collections::HashMap<&Path, &Entry> =
        src_m.iter().map(|e| (e.rel.as_path(), e)).collect();
    let dst_by_path: std::collections::HashMap<&Path, &Entry> =
        dst_m.iter().map(|e| (e.rel.as_path(), e)).collect();

    // ---- confirmed: scan-time permission failures (walkdir read_dir → directories) ----
    for (denied, root, dirs_ix, files_ix) in
        [(src_denied, src.path(), 0usize, 1usize), (dst_denied, dst.path(), 2, 3)]
    {
        for rel in denied {
            let abs = root.join(rel);
            let md = fs::symlink_metadata(&abs).ok();
            let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(true); // read_dir errors are dirs
            let item = Item { abs, owner: owner_of(md.as_ref()), mode: mode_of(md.as_ref()), confirmed: true };
            push(&mut s, &mut seen, if is_dir { dirs_ix } else { files_ix }, item);
        }
    }

    // ---- confirmed: classify-time permission failures (hashing → files) ----
    for (denied, root, by_path, ix) in [
        (&d.denied_source, src.path(), &src_by_path, 1usize),
        (&d.denied_dest, dst.path(), &dst_by_path, 3),
    ] {
        for rel in denied {
            let e = by_path.get(rel.as_path());
            let item = Item {
                abs: root.join(rel),
                owner: e.and_then(|e| e.owner),
                mode: e.and_then(|e| e.mode),
                confirmed: true,
            };
            push(&mut s, &mut seen, ix, item);
        }
    }

    // ---- predicted: permission arithmetic over the scanned owner/mode bits ----
    if !elevation_available {
        if let Some(me) = Me::current() {
            // Source entries this process can't read = data that cannot be backed up.
            for e in src_m.iter() {
                match e.kind {
                    Kind::Dir if !me.allows(e, 0o4) || !me.allows(e, 0o1) => {
                        push(&mut s, &mut seen, 0, predicted(src.path(), e));
                    }
                    Kind::File if !me.allows(e, 0o4) => {
                        push(&mut s, &mut seen, 1, predicted(src.path(), e));
                    }
                    _ => {}
                }
            }
            // Destination extras whose PARENT directory this process can't write = mirror-delete
            // will fail (unlink/rmdir permission lives in the parent).
            for ch in &d.removed {
                let parent_writable = match ch.rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                    Some(parent) => dst_by_path
                        .get(parent)
                        .map(|pe| me.allows(pe, 0o2) && me.allows(pe, 0o1))
                        .unwrap_or(true), // parent itself scheduled away / unknown — don't guess
                    None => true, // destination root: filesync validated it's workable
                };
                if !parent_writable {
                    let e = dst_by_path.get(ch.rel.as_path());
                    let item = Item {
                        abs: dst.path().join(&ch.rel),
                        owner: e.and_then(|e| e.owner),
                        mode: e.and_then(|e| e.mode),
                        confirmed: false,
                    };
                    let ix = if ch.kind == Kind::Dir { 5 } else { 4 };
                    push(&mut s, &mut seen, ix, item);
                }
            }
            // Destination directories that must RECEIVE copies but aren't writable by us.
            let mut receiving: HashSet<&Path> = HashSet::new();
            for rel in d.added.iter().map(|c| &c.rel).chain(d.changed.iter().map(|c| &c.rel)) {
                if let Some(p) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
                    receiving.insert(p);
                }
            }
            for m in &d.moved {
                if let Some(p) = m.to.parent().filter(|p| !p.as_os_str().is_empty()) {
                    receiving.insert(p);
                }
            }
            for parent in receiving {
                if let Some(pe) = dst_by_path.get(parent) {
                    if pe.kind == Kind::Dir && !(me.allows(pe, 0o2) && me.allows(pe, 0o1)) {
                        push(&mut s, &mut seen, 6, predicted(dst.path(), pe));
                    }
                }
            }
        }
    }

    for section in [
        &mut s.source_unreadable_dirs,
        &mut s.source_unreadable_files,
        &mut s.destination_unreadable_dirs,
        &mut s.destination_unreadable_files,
        &mut s.destination_undeletable_files,
        &mut s.destination_undeletable_dirs,
        &mut s.destination_unwritable_dirs,
    ] {
        section.sort_by(|a, b| a.abs.cmp(&b.abs));
    }
    s
}

fn push(s: &mut Showstoppers, seen: &mut [HashSet<PathBuf>; 7], ix: usize, item: Item) {
    if !seen[ix].insert(item.abs.clone()) {
        return;
    }
    let section = match ix {
        0 => &mut s.source_unreadable_dirs,
        1 => &mut s.source_unreadable_files,
        2 => &mut s.destination_unreadable_dirs,
        3 => &mut s.destination_unreadable_files,
        4 => &mut s.destination_undeletable_files,
        5 => &mut s.destination_undeletable_dirs,
        _ => &mut s.destination_unwritable_dirs,
    };
    section.push(item);
}

fn predicted(root: &Path, e: &Entry) -> Item {
    Item { abs: root.join(&e.rel), owner: e.owner, mode: e.mode, confirmed: false }
}

/// Resolve to an absolute path: canonical when the path exists (it does — we scanned it), else
/// CWD-joined as a fallback.
fn absolutize(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| {
        std::env::current_dir().map(|cwd| cwd.join(p)).unwrap_or_else(|_| p.to_path_buf())
    })
}

fn owner_of(md: Option<&fs::Metadata>) -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        md.map(|m| (m.uid(), m.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        None
    }
}

fn mode_of(md: Option<&fs::Metadata>) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        md.map(|m| m.mode())
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        None
    }
}

/// This process's identity, for "would that operation be allowed?" arithmetic.
struct Me {
    #[cfg(unix)]
    euid: u32,
    #[cfg(unix)]
    groups: Vec<u32>,
}

impl Me {
    #[cfg(unix)]
    fn current() -> Option<Self> {
        let euid = unsafe { libc::geteuid() };
        let mut groups = vec![unsafe { libc::getegid() }];
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if n > 0 {
            let mut buf = vec![0 as libc::gid_t; n as usize];
            let got = unsafe { libc::getgroups(n, buf.as_mut_ptr()) };
            if got > 0 {
                groups.extend(buf.into_iter().take(got as usize));
            }
        }
        Some(Self { euid, groups })
    }

    #[cfg(not(unix))]
    fn current() -> Option<Self> {
        None // no owner/mode bits collected off-unix — predictions don't apply
    }

    /// Classic DAC check: does this process hold permission bit `want` (4=r, 2=w, 1=x) on `e`?
    /// Root passes everything (moot in practice: bare root is refused, and with root in reserve
    /// predictions are skipped).
    #[cfg(unix)]
    fn allows(&self, e: &Entry, want: u32) -> bool {
        let (Some((uid, gid)), Some(mode)) = (e.owner, e.mode) else {
            return true; // unknown bits — never invent a problem out of missing data
        };
        if self.euid == 0 {
            return true;
        }
        let perms = mode & 0o777;
        let shift = if self.euid == uid {
            6
        } else if self.groups.contains(&gid) {
            3
        } else {
            0
        };
        (perms >> shift) & want == want
    }

    #[cfg(not(unix))]
    fn allows(&self, _e: &Entry, _want: u32) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

impl Showstoppers {
    /// Format the `…showstoppers.txt` file: header, then one array + remedy loop per non-empty
    /// section.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# filesync showstoppers — what prevents a faithful mirror, with paste-able remedies.\n\
             #\n\
             # Each section below is a bash ARRAY of absolute paths, followed by the LOOP that\n\
             # applies that section's fix to the array. Paste the array into your shell, review\n\
             # it, then paste the loop. Nothing in this file runs on its own — that's deliberate.\n\
             # Quoting is bash-exact (including $'…' for names with special characters), so any\n\
             # filename round-trips correctly.\n\
             #\n\
             # Per-line comment: owner  permission-bits  confidence.\n\
             #   confirmed = an operation actually failed there during this run\n\
             #   predicted = judged from owner/mode arithmetic (an ACL may say otherwise)\n\
             #\n\
             # Alternative to all of this: run filesync under sudo — it uses root only at these\n\
             # walls and records every use (see README, \"Restricted-access files\").\n",
        );

        section(
            &mut out,
            "SOURCE: directories your user cannot read — their contents CANNOT BE BACKED UP",
            "While any of these exist, a sync also suspends ALL deletions (incomplete source view).",
            "filesync_source_unreadable_dirs",
            "sudo setfacl -R -m \"u:$(id -un):rX\" -- \"$f\"",
            &["# (or take ownership instead: sudo chown -R \"$(id -un):\" -- each path)"],
            &self.source_unreadable_dirs,
        );
        section(
            &mut out,
            "SOURCE: files your user cannot read — they CANNOT BE BACKED UP",
            "Reading them is required to copy, hash, and verify.",
            "filesync_source_unreadable_files",
            "sudo setfacl -m \"u:$(id -un):r\" -- \"$f\"",
            &[],
            &self.source_unreadable_files,
        );
        section(
            &mut out,
            "DESTINATION: directories your user cannot read",
            "Their contents can't be verified or mirror-managed.",
            "filesync_destination_unreadable_dirs",
            "sudo setfacl -R -m \"u:$(id -un):rX\" -- \"$f\"",
            &[],
            &self.destination_unreadable_dirs,
        );
        section(
            &mut out,
            "DESTINATION: files your user cannot read",
            "They can't be hash-compared (move-detection, corruption checks).",
            "filesync_destination_unreadable_files",
            "sudo setfacl -m \"u:$(id -un):r\" -- \"$f\"",
            &[],
            &self.destination_unreadable_files,
        );
        section(
            &mut out,
            "DESTINATION: extra files the mirror wants to DELETE, but their parent dir is not writable by you",
            "Mirror semantics already doom these (absent from the source) — review before pasting.",
            "filesync_destination_undeletable_files",
            "sudo rm -- \"$f\"",
            &[],
            &self.destination_undeletable_files,
        );
        section(
            &mut out,
            "DESTINATION: extra directories the mirror wants to DELETE, but their parent dir is not writable by you",
            "rmdir only removes EMPTY dirs — clear their contents first (files list above).",
            "filesync_destination_undeletable_dirs",
            "sudo rmdir -- \"$f\"",
            &[],
            &self.destination_undeletable_dirs,
        );
        section(
            &mut out,
            "DESTINATION: directories that must RECEIVE copies, but are not writable by you",
            "Planned copies/moves into them will fail.",
            "filesync_destination_unwritable_dirs",
            "sudo setfacl -m \"u:$(id -un):rwX\" -- \"$f\"",
            &[],
            &self.destination_unwritable_dirs,
        );
        out
    }
}

fn section(
    out: &mut String,
    title: &str,
    why: &str,
    array_name: &str,
    remedy: &str,
    extra_comments: &[&str],
    items: &[Item],
) {
    use std::fmt::Write;
    if items.is_empty() {
        return;
    }
    let _ = write!(out, "\n# {} {title}\n# {why}\n", "\u{2500}\u{2500}");
    for c in extra_comments {
        let _ = writeln!(out, "{c}");
    }
    let _ = writeln!(out, "{array_name}=(");
    for it in items {
        let owner = match it.owner {
            Some((uid, gid)) => format!("{}:{}", user_name(uid), group_name(gid)),
            None => "?:?".to_string(),
        };
        let mode = it.mode.map(|m| format!("{:04o}", m & 0o7777)).unwrap_or_else(|| "????".into());
        let mark = if it.confirmed { "confirmed" } else { "predicted" };
        let _ = writeln!(out, "  {}  # {owner} {mode} {mark}", bash_quote(&it.abs));
    }
    let _ = writeln!(out, ")");
    let _ = writeln!(out, "for f in \"${{{array_name}[@]}}\"; do\n  {remedy}\ndone");
}

/// Quote a path as a bash literal. Plain `'…'` when every byte is printable ASCII without `'`;
/// otherwise ANSI-C `$'…'` with `\'`, `\\`, `\n`, `\t`, `\r`, and `\xNN` for other control bytes
/// (non-ASCII UTF-8 bytes pass through — `$'…'` keeps them literal). Bash re-parses either form
/// back to the exact original bytes when the array is defined.
pub fn bash_quote(path: &Path) -> String {
    let bytes = path_bytes(path);
    let simple = bytes.iter().all(|b| (0x20..=0x7e).contains(b) && *b != b'\'');
    if simple {
        let s: String = bytes.iter().map(|&b| b as char).collect();
        return format!("'{s}'");
    }
    let mut out = String::from("$'");
    for &b in bytes.iter() {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            0x80..=0xff => out.push_str(&format!("\\x{b:02x}")), // byte-exact, encoding-agnostic
            _ => out.push_str(&format!("\\x{b:02x}")),           // other control bytes
        }
    }
    out.push('\'');
    out
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// uid → login name, numeric fallback (thread-safe getpwuid_r).
#[cfg(unix)]
fn user_name(uid: u32) -> String {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result)
    };
    if rc == 0 && !result.is_null() {
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        if let Ok(s) = name.to_str() {
            return s.to_string();
        }
    }
    uid.to_string()
}

/// gid → group name, numeric fallback (thread-safe getgrgid_r).
#[cfg(unix)]
fn group_name(gid: u32) -> String {
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result)
    };
    if rc == 0 && !result.is_null() {
        let name = unsafe { std::ffi::CStr::from_ptr(grp.gr_name) };
        if let Ok(s) = name.to_str() {
            return s.to_string();
        }
    }
    gid.to_string()
}

#[cfg(not(unix))]
fn user_name(uid: u32) -> String {
    uid.to_string()
}

#[cfg(not(unix))]
fn group_name(gid: u32) -> String {
    gid.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_get_single_quotes() {
        assert_eq!(bash_quote(Path::new("/a b/c.txt")), "'/a b/c.txt'");
        assert_eq!(bash_quote(Path::new("/plain")), "'/plain'");
    }

    #[test]
    fn special_characters_get_ansi_c_quoting() {
        // newline in the name — the case that breaks naive one-path-per-line formats
        assert_eq!(bash_quote(Path::new("/a\nb")), "$'/a\\nb'");
        // an apostrophe
        assert_eq!(bash_quote(Path::new("/it's")), "$'/it\\'s'");
        // tab + backslash
        assert_eq!(bash_quote(Path::new("/a\tb\\c")), "$'/a\\tb\\\\c'");
    }

    #[cfg(unix)]
    #[test]
    fn non_ascii_bytes_are_escaped_byte_exact() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let p = Path::new(OsStr::from_bytes(b"/caf\xc3\xa9"));
        assert_eq!(bash_quote(p), "$'/caf\\xc3\\xa9'");
    }

    #[test]
    fn render_emits_array_loop_and_metadata_comment() {
        let s = Showstoppers {
            source_unreadable_files: vec![Item {
                abs: PathBuf::from("/data/secret.bin"),
                owner: Some((0, 0)),
                mode: Some(0o100600),
                confirmed: true,
            }],
            ..Showstoppers::default()
        };
        let out = s.render();
        assert!(out.contains("filesync_source_unreadable_files=("), "{out}");
        assert!(out.contains("'/data/secret.bin'"), "{out}");
        assert!(out.contains("0600 confirmed"), "{out}");
        assert!(
            out.contains("for f in \"${filesync_source_unreadable_files[@]}\"; do"),
            "the remedy loop must reference the verbose array name:\n{out}"
        );
        assert!(out.contains("setfacl"), "{out}");
        // empty sections are omitted entirely
        assert!(!out.contains("filesync_destination_undeletable_files"), "{out}");
    }

    #[test]
    fn empty_showstoppers_is_empty() {
        assert!(Showstoppers::default().is_empty());
        assert_eq!(Showstoppers::default().total(), 0);
    }
}
