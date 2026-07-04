use crate::{does_path_start_with, read_file, read_tracking_file_into_manifest, run, write_tracking_file, write_tracking_file_with_content, ProgramArgs, TRACKING_FILENAME};
use std::{env, io};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::os::unix::fs as unix_fs;  // we're supporting unix filesystem features such as symlinks
use std::process::Command;
use clap::Parser;
use rayon::prelude::*;


use test_case::test_case;
use walkdir::WalkDir;
use crate::structures::{Manifest};


fn run_w_args(args: &[&str]) -> String { run(ProgramArgs::parse_from(args)) }
#[test]
fn run_help() {
    run_w_args(&["filesync", "-h"]);
}

#[test_case("$HOME/Downloads")]  // doesn't work in the IDE; run `cargo test` instead.
fn test_write_tracking_file(dir_spec: &str) {
// #[test]
// fn test_write_tracking_file() {
//     let dir_spec = "$HOME/Downloads";
    let base_dir = expand_home(dir_spec);

    // write empty file
    let (_, mut file) = write_tracking_file(&base_dir);  // if this doesn't panic, we're good

    fn assert_file_empty(file: &fs::File) { assert_eq!(file.metadata().expect("can't check file metadata!").len(), 0) }
    fn assert_file_non_empty(file: &fs::File) { assert!(file.metadata().expect("can't check file metadata!!").len() > 0) }


    file.set_len(0).expect(""); // clears content
    assert_file_empty(&file);

    let our_string = "AAAAAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBB\n";

    // make sure that writing the file doesn't overwrite it, if it exists
    file.write_all(our_string.as_bytes()).expect("failed to write to file");
    file.flush().expect("failed to flush"); // harmless for File; required if buffered somewhere
    assert_file_non_empty(&file);

    // check no-overwrite on write_tracking_file() call
    let (same_path, same_file) = write_tracking_file(&base_dir);  // writing again
    assert_file_non_empty(&same_file);  // checking file wasn't overwritten
    assert!(read_file(&same_path).contains(our_string));

    // check that there's overwrite on write_tracking_file_with_content() call
    let filled_file_path = write_tracking_file_with_content(&base_dir, None);  // rewrite file contents
    assert!(!read_file(&filled_file_path).contains(our_string));  // make sure previous string is overwritten

    let _ = fs::remove_file(&filled_file_path);  // cleanup - remove tracking-file
}

#[test]
fn tracking_file_compare_with_shell_command() {
    let tracker_content = create_tree_and_tracker_and_read_paths("S", None);
    let baseline_out = find_escaped_output(&define_tmp_dir("S"));

    assert_eq!(tracker_content, baseline_out);
}

#[test]
fn check_serialized_deserialization_is_same() {
    let file_content = create_tree_and_tracker_and_read_manifest("serialization_test", None);

    // deserialize from String, then serialize into String
    let dereserailized = Manifest::deserialize_into_manifest(Manifest::serialize(&file_content).join("\n").as_str());

    assert_eq!(file_content, dereserailized);
}


#[test]
fn test_args_cli_track() {

    let cli_path = "CLI";
    let pre1 = "f1";
    let pre2 = "f4";
    let pre3 = "aaaaaaaaaaaaaaaaaqqqqqqqqqqqqqq";  // nothing starts with that

    let root = creates_complicated_testing_tree(cli_path, None);

    let result_of_track_w_args = |args: &[&str]| -> Manifest {
        let prefix_args = std::iter::repeat("-p").zip(args.iter()).flat_map(|(p, s)| [p, *s]);  // -p pre1 -p pre2 ...
        let full_cmd = &["filesync", "--track", &root.to_str().unwrap()].into_iter()
            .chain(prefix_args).collect::<Vec<&str>>();

        let tracker = run_w_args(full_cmd);
        // Manifest::serialize(read_tracking_file_into_manifest(tracker.as_ref()))
        read_tracking_file_into_manifest(tracker.as_ref())
    };

    fn all_start_with(content: &Manifest, prefix: &str) -> bool {
        content.iter().all(|e| e.get_path_stringified().starts_with(prefix))
    }

    let content = result_of_track_w_args(&[pre1]);
    assert!(all_start_with(&content, pre1));

    let content_again = result_of_track_w_args(&[pre1]);
    assert_eq!(content, content_again);  // make sure same-entries don't add up

    let content2 = result_of_track_w_args(&[pre2]);
    assert!(!all_start_with(&content2, pre2), "failed test, with content:\n\t{}", content2.serialize().join("\n\t"));  // check that it's not overwrite
    assert!(content2.iter().all(|s| s.get_path_stringified().starts_with(pre1) || s.get_path_stringified().starts_with(pre2)));  // check that it's additive

    let _ = remove_entries_with_prefix(&root, format!("{pre1}/a").as_str());  // removing ONE entry from disk
    let content3 = result_of_track_w_args(&[pre1, pre2]);  // smaller footprint (one less entry)
    assert_eq!(content2.filepaths().len(), content3.filepaths().len()+1,
               "failed expected the following to be one item:\n\t{}\n--------------\n\t{}", content2.serialize().join("\n\t"), content3.serialize().join("\n\t"));


    // assert_eq!(content2.len(), content3.len(), "failed expected the following to be one item:\n\t{}", items_in_first_only(content2, content3).join("\n\t"));

    // let _ = remove_entries_with_prefix(&root, "f");  // delete all files relevant to pre1 AND pre2
    // let content_empty = result_of_track_w_args(&[pre1, pre2]);
    // assert!(content_empty.is_empty());   // check it's indeed empty

    //
    //
    //
    // let _ = remove_entries_with_prefix(&root, "f-");
    //
    // // TODO - test a prefix that catches nothing
    //
    // // check that this is indeed what's happening here directly in the tests and program
    // let tracking_file = write_tracking_file_with_content(root, None);
    // read_tracking_file_into_filepaths(&tracking_file);
}

/// tracking with specific prefixes (rather than all files)
#[test]
fn test_picked_track_scans() {
    // TODO - pick a subdir, check that only those got "walked"
    // TODO - pick another subdir, check that those got walked and added (not replacing previous)
    // TODO - move a file, rescan, and see that the tracking file got updated (erasing relevant previous entries)
    // TODO - check unicode prefixes
}


#[test]
fn detect_differences_between_filetrees() {
    // need to read only paths because the creation date would be different
    let tracker_content_a = create_tree_and_tracker_and_read_entries("A", None);
    let extras: Vec<String> = vec!["EXTRA/".into(), "EXTRA/x.txt".into(), "EXTRA/y.txt".into()];
    let tracker_content_b = create_tree_and_tracker_and_read_entries("B", Some(&extras));  // B has "extra" files

    assert_eq!(items_in_first_only(tracker_content_b, tracker_content_a), extras);
}


/// returns the path of the newly created tracking file
fn create_tree_and_tracker(subdir: &str, extra: Option<&[String]>) -> PathBuf {
    let new_dir = creates_complicated_testing_tree(subdir, extra);
    write_tracking_file_with_content(&new_dir, None)
}

/// returns the newly made and listed files within the new tracking file
fn create_tree_and_tracker_and_read_entries(subdir: &str, extra: Option<&[String]>) -> Vec<String> {
    let tracker_filepath = create_tree_and_tracker(subdir, extra);
    entry_strs_from_tracking_file(&tracker_filepath)
}

fn create_tree_and_tracker_and_read_manifest(subdir: &str, extra: Option<&[String]>) -> Manifest {
    let tracker_filepath = create_tree_and_tracker(subdir, extra);
    read_tracking_file_into_manifest(&tracker_filepath)
}

fn create_tree_and_tracker_and_read_paths(subdir: &str, extra: Option<&[String]>) -> Vec<String> {
    create_tree_and_tracker_and_read_manifest(subdir, extra).filepaths()
}

fn define_tmp_dir(subdir: &str) -> PathBuf {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    project_root.join("testing").join(subdir)
}

fn creates_complicated_testing_tree(subdir: &str, extra: Option<&[String]>) -> PathBuf {
    let root = define_tmp_dir(subdir);

    let _ = fs::remove_dir_all(&root);

    // simple
    create_entry(&root, "f1/a.txt", b"");
    create_entry(&root, "f1/b.txt", b"hello world");

    // problematic chars
    create_entry(&root, "f2/a.txt", b"another a");
    create_entry(&root, "f2/ with    space", b"space");
    create_entry(&root, "f2/special!@#$%^&*()-+`\"\'", b"specials");
    create_entry(&root, "f2/with\nnewline", b"newline");
    create_entry(&root, "f2/with\ttab", b"tab");
    create_entry(&root, "f2/unicode_ハンバーガー_🍣", b"unicode");
    create_entry(&root, "f2/unicode_ハハハハハハハハハ", b"unicode");
    create_entry(&root, "f2/unicode_🍣🍣🍣🍣🍣🍣🍣🍣", b"unicode");
    create_entry(&root, "f2/emojis_🇺🇸🇺🇸🇺🇸🇺🇸🇺🇸🇺🇸🇺🇸", b"unicode");
    create_entry(&root, "f2/escaped_\\\'\"\'\'\\\\\t\\\'", b"unicode");
    create_entry(&root, "ハwハwハ", b"unicode");

    // hierarchy/hidden
    create_entry(&root, "f-3/inner1", b"");
    create_entry(&root, "f-3/f4/inner2", b"inner2");
    create_entry(&root, "f-3/.hidden", b"hidden");

    // empty
    create_entry(&root, "empty_dir/", b"");  // empty dir
    create_entry(&root, "empty_file", b"");  // empty file in root

    // extra instances
    create_entry(&root, "f-4/inner2", b"another inner2");  // "duplicate" file
    create_entry(&root, &format!("f4/{TRACKING_FILENAME}"), b"another inner2");  // "duplicate" file

    // links
    create_symlink(&root, "f5/sl1", "../f1/b.txt");
    create_symlink(&root, "f5/sl2", "sl1");  // f5/sl2 -> f5/sl1
    create_symlink(&root, "f5/f6/sl3", "../..");  // f5/f6/sl3 -> f5/
    create_symlink(&root, "f5/f6/sl4", expand_home("$HOME/Downloads").to_str().unwrap());  // link outside of project
    create_symlink(&root, "f5/f6/broken", "../file_that_doesnt_exist");  // f5/f6/sl3 -> f5/

    // extras (optional)
    if let Some(extra) = extra {
        for rel in extra {
            create_entry(&root, rel, b"");
        }
    }

    root
}


fn expand_home(s: &str) -> PathBuf {
    if s.starts_with("$HOME/") || s == "$HOME" {
        let home = env::var("HOME").expect("HOME is not set");
        let rest = s.strip_prefix("$HOME").unwrap();
        return PathBuf::from(home).join(rest.trim_start_matches('/'));
    }
    PathBuf::from(s)
}

fn create_entry(root: &Path, rel: &str, contents: &[u8]) -> PathBuf {
    // Expect paths relative to `root` (e.g., "f1/a.txt" or "empty_dir/")
    let rel = rel.strip_prefix("./").unwrap_or(rel);

    if rel.ends_with('/') {
        let dir_path = root.join(rel.trim_end_matches('/'));
        fs::create_dir_all(&dir_path).unwrap();
        return dir_path;
    }

    let file_path = root.join(rel);
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, contents).unwrap();
    file_path
}

/// Scans `dir` and removes all files and directories whose names start with `prefix`.
// pub fn remove_entries_with_prefix<P: AsRef<Path>>(root: P, prefix: &str,) -> io::Result<()> {
//     for entry in fs::read_dir(&root)? {
//         let entry = entry?;
//         let full_path = entry.path();
//
//
//         if does_path_start_with(full_path.as_path(), root.as_ref(), prefix) {
//             dbg!("removing: {:?} (with prefix: '{}')", &full_path, &prefix);
//             let file_type = entry.file_type()?;
//
//             if file_type.is_dir() {
//                 fs::remove_dir_all(&full_path)?;
//             } else {
//                 fs::remove_file(&full_path)?;
//             }
//         }
//     }
//
//     Ok(())
// }
pub fn remove_entries_with_prefix<P: AsRef<Path>>(root: P, prefix: &str,) -> io::Result<()> {
    let root_path = root.as_ref();

    for (path, filetype) in WalkDir::new(root_path)
        .min_depth(1) // skip root itself
        .into_iter()
        .filter_map(|entry| entry.ok())
        .map(|entry| (entry.path().to_path_buf(), entry.file_type()))
        .filter(|(path, _)| { does_path_start_with(&path, root_path, &[prefix.to_string()]) })
    {
        // Delete files immediately
        if filetype.is_file() { fs::remove_file(path)?; }
        // Delete directories bottom-up
        else if filetype.is_dir() { fs::remove_dir_all(path)?; }
    }

    Ok(())
}

fn create_symlink(root: &Path, link_rel: &str, target: &str) -> PathBuf {
    let link_rel = link_rel.strip_prefix("./").unwrap_or(link_rel);
    let link_path = root.join(link_rel);

    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }

    // Replace if it already exists (file/dir/symlink)
    let _ = fs::remove_file(&link_path);
    let _ = fs::remove_dir_all(&link_path);

    unix_fs::symlink(target, &link_path).unwrap();
    link_path
}


/// Runs:
///   find . -mindepth 1 -printf '%P\0'
/// Then escapes the output and sorts it.
///
/// Notes:
/// - Uses `sh -lc` because of the pipe.
fn find_escaped_output(dir: &std::path::Path) -> Vec<String> {
    let mut lines: Vec<String> = Command::new("sh")  // run "find" command
        .arg("-lc")
        // .arg(r"find . -mindepth 1 -printf '%P\0'")
        .arg(r"find . -mindepth 1 \( -type d -printf '%P/\0' -o -type f -printf '%P\0' -o -type l -printf '%P\0' \)")
        .current_dir(dir)
        .output()       // shell command output
        .inspect_err(|e| panic!("failed to run shell command in '{}': {e}", dir.display()))
        .inspect( |out| assert!(out.status.success(), "find failed in '{}': exit={:?}, stderr={}", dir.display(), out.status.code(), String::from_utf8_lossy(&out.stderr),) )
        .unwrap().stdout.par_split(|&b| b == 0)  // split stdout on \0
        .filter(|s| !s.is_empty())  // sanitize
        .filter(|s| *s != TRACKING_FILENAME.as_bytes())  // sanitize
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();

    lines.par_sort_unstable();  // no need to preserve equals' order; run it a bit faster
    lines
}

fn items_in_first_only(first: Vec<String>, second: Vec<String>) -> Vec<String> {
    let second_set: HashSet<String> = second.into_iter().collect();
    first.into_iter().filter(|item| !second_set.contains(item)).collect()
}



pub fn entry_strs_from_tracking_file(tracking_file: &std::path::Path) -> Vec<String> {
    read_tracking_file_into_manifest(&tracking_file).iter()  // back and forth necessary for filenames with newlines within
        .map(|m| m.serialize_entry())
        .map(|(s1, s2)| format!("{s1} {s2}"))
        .collect::<Vec<_>>()
}

