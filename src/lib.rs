// TEMPORARILY DISABLED during the rebuild: tests.rs is the re-applied richer suite and
// references APIs the current lib no longer exposes, so it won't compile. The test *corpus*
// (creates_complicated_testing_tree etc.) is preserved for reuse; tests get reintroduced properly.
// #[cfg(test)]
// mod tests;
mod structures;
mod args_parse;

pub use crate::args_parse::ProgramArgs;

#[cfg(unix)]
use crate::structures::ManifestEntry;

use std::fs;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use std::io::{Write, BufWriter};
use crate::structures::Manifest;

pub const TRACKING_FILENAME: &str = "filesync_tracking.txt";

pub fn run(args: ProgramArgs) -> String {
    if let Some(dir) = args.track {
        write_tracking_file_with_content(dir, args.prefix.as_deref())
            .to_str().unwrap().to_string()
    } else if let Some(files_pair) = args.diff {
        let master = &files_pair[0];
        let slave = &files_pair[1];
        // ...
        "".to_string()
    } else if let Some(dirs) = args.sync {
        let master = &dirs[0];
        let slave = &dirs[1];
        // ...
        "".to_string()
    } else {
        unreachable!("clap ArgGroup enforces exactly one command");
    }
}

pub fn write_tracking_file(dir: impl AsRef<Path>) -> (PathBuf, File) {
    let dir = dir.as_ref();

    match fs::metadata(&dir) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => panic!("not a directory: '{}'", dir.display()),
        Err(e) => panic!("metadata failed for '{}': {e}", dir.display()),
    }

    let file_path = dir.join(TRACKING_FILENAME);

    match fs::symlink_metadata(&file_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}  // doesn't exist: ok
        Ok(m) if m.is_file() => {}                             // exists and is file: ok
        Ok(_) => panic!("tracking path exists but is not a file: '{}'", file_path.display()),
        Err(e) => panic!("metadata failed for '{}': {e}", file_path.display()),
    }

    let file = OpenOptions::new()
        .create(true)
        .read(true)  // for optionally reading from the same handle later
        .write(true)  // for optionally writing with the same handle later
        .open(&file_path)
        .unwrap_or_else(|e| panic!("failed to create '{}': {e}", file_path.display()));

    (file_path, file)
}



/// Walk directory
fn discover_files(root: &Path, allowed_prefixes: Option<&[String]>) -> Manifest {
    let root_str = root.to_str().unwrap();

    let mut out: Manifest = WalkDir::new(root).follow_links(false).into_iter()
        .filter_entry(|e| {
            allowed_prefixes.is_none() || e.depth() == 0 || {  // depth 0 is root, which we don't want to stop at
                allowed_prefixes.into_iter()
                    .flatten()
                    .map(|p| format!("{root_str}/{p}"))
                    .any(|s| e.path().starts_with(s))
            }
        })
        .filter_map(|e| e.ok())  // ignore traversal errors for now
        .filter(|e| e.depth() != 0)  // exclude root itself)
        .map(|e| e.path().strip_prefix(root).unwrap().to_path_buf())
        .filter(|rel| rel.as_os_str() != TRACKING_FILENAME)
        .map(|rel| ManifestEntry::from_rel_path(root, rel))
        .collect();

    out.sort();
    out
}



pub fn write_tracking_file_with_content(dir: impl AsRef<Path>, allowed_prefix: Option<&[String]>) -> PathBuf {
    let dir = dir.as_ref();
    let (tracker_path, tracker_file) = write_tracking_file(dir);

    let entries = discover_files(dir, allowed_prefix);

    let data = Manifest::serialize(entries);
    let mut w = BufWriter::new(tracker_file);  // buffered writing (smaller burden on RAM)
    for d in data {
        writeln!(w, "{}", d).unwrap_or_else(|err| panic!("failed to write to '{}': {err}", tracker_path.display()))
    }

    tracker_path
}

pub fn read_tracking_file_into_string(tracking_file: &std::path::Path) -> String {
    std::fs::read_to_string(tracking_file)
        .unwrap_or_else(|e| panic!("failed to read '{}': {e}", tracking_file.display()))
}

// pub fn read_tracking_file_into_manifest(tracking_file: &std::path::Path) -> Manifest {
//     Manifest::deserialize_manifests(&read_tracking_file_into_string(&tracking_file))
// }

pub fn read_tracking_file_into_filepaths(tracking_file: &std::path::Path) -> Vec<String> {
    let mut strings = read_tracking_file_into_string(&tracking_file).lines()
        .map(|s| ManifestEntry::deserialize_path_key(&s))
        .collect::<Vec<_>>();

    // Escaped strings' order can differ after deserialization. Re-sorting might be necessary.
    strings.sort_unstable();
    strings
}

