//! Symlink-target policy: what target a link copied to the destination should carry.
//!
//! Used by *both* sides of the pipeline so they can never disagree: `diff` compares a destination
//! link against the target the copy WOULD write (not against the raw source target), and `apply`
//! writes exactly that target. This is what makes `--relative-symlinks` idempotent — once the
//! destination holds the rewritten link, later runs classify it as unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::SrcRoot;

/// The target a symlink at `link_rel` (relative to the source root) should carry at the
/// destination. Verbatim by default. With `relative_symlinks`, a target that *resolves* inside
/// the source is rewritten as a relative path to the same location in the mirror; anything
/// resolving outside the source stays verbatim.
///
/// Resolution is realpath-style and lenient: chained symlinks and `..` in existing components are
/// followed the way the kernel would, and a missing tail is normalized lexically — whether the
/// target actually exists is deliberately not this function's business.
pub fn desired_target(
    src: &SrcRoot,
    link_rel: &Path,
    raw_target: &Path,
    relative_symlinks: bool,
) -> PathBuf {
    if !relative_symlinks {
        return raw_target.to_path_buf();
    }
    // Absolute targets stand alone; relative ones are anchored at the link's own directory.
    let abs = if raw_target.is_absolute() {
        raw_target.to_path_buf()
    } else {
        let parent = link_rel.parent().unwrap_or_else(|| Path::new(""));
        src.path().join(parent).join(raw_target)
    };
    let resolved = crate::preflight::canonicalize_lenient(&abs);
    let Ok(src_root) = fs::canonicalize(src.path()) else {
        return raw_target.to_path_buf(); // can't resolve the source root — don't rewrite
    };
    match resolved.strip_prefix(&src_root) {
        Ok(inside) => relative_link(link_rel, inside),
        Err(_) => raw_target.to_path_buf(), // resolves outside the source — keep verbatim
    }
}

/// The relative path a symlink at `link_rel` should use to point at `target_rel`, where both are
/// relative to the same root: walk up from the link's own directory to the common ancestor, then
/// down to the target. e.g. `links/rel` → `f1/b.txt` yields `../f1/b.txt`.
pub fn relative_link(link_rel: &Path, target_rel: &Path) -> PathBuf {
    let base: Vec<_> = link_rel.parent().unwrap_or_else(|| Path::new("")).components().collect();
    let target: Vec<_> = target_rel.components().collect();
    let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();

    let mut out = PathBuf::new();
    for _ in common..base.len() {
        out.push("..");
    }
    for c in &target[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push("."); // link points at its own directory
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── relative_link path math ──────────────────────────────────────────────

    #[test]
    fn relative_link_from_root() {
        assert_eq!(relative_link(Path::new("abs"), Path::new("f1/b.txt")), PathBuf::from("f1/b.txt"));
    }

    #[test]
    fn relative_link_preserves_an_internal_relative_link() {
        // links/rel -> f1/b.txt  ⇒  ../f1/b.txt  (so already-relative links are unchanged)
        assert_eq!(
            relative_link(Path::new("links/rel"), Path::new("f1/b.txt")),
            PathBuf::from("../f1/b.txt")
        );
    }

    #[test]
    fn relative_link_with_shared_prefix() {
        // a/b/link -> a/c/x  ⇒  ../c/x
        assert_eq!(relative_link(Path::new("a/b/link"), Path::new("a/c/x")), PathBuf::from("../c/x"));
    }

    #[test]
    fn relative_link_in_same_directory() {
        assert_eq!(relative_link(Path::new("dir/link"), Path::new("dir/tgt")), PathBuf::from("tgt"));
    }

    // ── desired_target policy ────────────────────────────────────────────────

    #[test]
    fn verbatim_when_flag_is_off() {
        let t = tempfile::tempdir().unwrap();
        let src = SrcRoot::new(t.path());
        let abs_inside = t.path().join("f1/b.txt");
        assert_eq!(desired_target(&src, Path::new("l"), &abs_inside, false), abs_inside);
    }

    #[test]
    fn absolute_internal_target_becomes_relative() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("f1")).unwrap();
        std::fs::write(t.path().join("f1/b.txt"), b"x").unwrap();
        let src = SrcRoot::new(t.path());
        assert_eq!(
            desired_target(&src, Path::new("links/l"), &t.path().join("f1/b.txt"), true),
            PathBuf::from("../f1/b.txt")
        );
    }

    #[test]
    fn internal_relative_target_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("links")).unwrap();
        std::fs::create_dir_all(t.path().join("f1")).unwrap();
        let src = SrcRoot::new(t.path());
        assert_eq!(
            desired_target(&src, Path::new("links/l"), Path::new("../f1/b.txt"), true),
            PathBuf::from("../f1/b.txt"),
            "an already-relative internal link must come out unchanged"
        );
    }

    #[test]
    fn external_target_stays_verbatim() {
        let t = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let src = SrcRoot::new(t.path());
        let ext = elsewhere.path().join("ext.txt");
        assert_eq!(desired_target(&src, Path::new("l"), &ext, true), ext);
    }

    #[test]
    fn missing_internal_target_is_still_rewritten() {
        // "may or may not exist — none of our business": a dangling target inside the source is
        // rewritten too; the mirror's link dangles at the mirrored location.
        let t = tempfile::tempdir().unwrap();
        let src = SrcRoot::new(t.path());
        assert_eq!(
            desired_target(&src, Path::new("l"), &t.path().join("not/yet/here"), true),
            PathBuf::from("not/yet/here")
        );
    }

    #[cfg(unix)]
    #[test]
    fn target_reached_through_a_chained_symlink_is_seen_through() {
        // l -> hop (symlink) -> real/data.txt : the desired target is where the chain LANDS.
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("real")).unwrap();
        std::fs::write(t.path().join("real/data.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(t.path().join("real"), t.path().join("hop")).unwrap();
        let src = SrcRoot::new(t.path());
        assert_eq!(
            desired_target(&src, Path::new("l"), &t.path().join("hop/data.txt"), true),
            PathBuf::from("real/data.txt"),
            "resolution must look through intermediate symlinks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn chain_escaping_the_source_stays_verbatim() {
        // The path LOOKS internal but a hop points outside — resolution must catch that.
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), t.path().join("hop")).unwrap();
        let src = SrcRoot::new(t.path());
        let raw = t.path().join("hop/secret.txt");
        assert_eq!(
            desired_target(&src, Path::new("l"), &raw, true),
            raw,
            "a chain that lands outside the source must not be rewritten"
        );
    }
}
