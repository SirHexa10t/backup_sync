use serde::{Deserialize, Deserializer, Serialize};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use base64::Engine as _;
use os_str_bytes::OsStrBytes;
use rayon::prelude::*;
use unicode_width::UnicodeWidthStr;


#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    // lossless path bytes, base64 (relative path bytes as seen by the OS)
    pub encoded_path_b64: String,

    // file/dir/symlink/other
    pub ty: NodeType,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,

    // Store times as ns since UNIX epoch (portable, sortable).
    pub mtime_ns: i128,

    // Optional (Linux): permissions bits (e.g., 0o644). Omit on platforms where it’s awkward.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,

    // Only present when ty == Symlink.
    // Store as JSON string (UTF-8). If you later need lossless non-UTF8 targets on Unix,
    // add link_target_b64 as a parallel field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<PathBuf>,
}


#[derive(Debug, Clone)]
pub struct ManifestEntry {
    /// Relative path used as the primary sort key (human-readable, UTF-8-ish).
    path_key: PathBuf,
    /// Metadata
    record: FileMeta,
}

#[derive(Debug, Clone, Default)]
pub struct Manifest(Vec<ManifestEntry>);



////////////////////////////////////////////////////////////////////////////////////////////////////
///////////////////////////////////////MANIFEST ENTRY///////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////

impl ManifestEntry {

    pub fn from_rel_path(root: &Path, rel: PathBuf) -> Self {
        let full_path = root.join(&rel);

        let md = fs::symlink_metadata(&full_path).unwrap_or_else(|e| panic!("symlink_metadata failed for '{}': {e}", full_path.display()));

        #[cfg(unix)]
        let mode = Some(md.mode() & 0o7777);
        #[cfg(not(unix))]
        let mode = None;

        let ft = md.file_type();
        let ty =
            if ft.is_dir() { NodeType::Dir }
            else if ft.is_file() { NodeType::File }
            else if ft.is_symlink() { NodeType::Symlink }
            else { NodeType::Other };

        fn append_slash_to_path(pb: &PathBuf) -> PathBuf {
            let mut path = PathBuf::from("/some/directory");
            path.push("");  // adds "/". Don't actually add "/", because it'll break. It's built to also support Windows, which uses '\'
            path
        }

        ManifestEntry {
            path_key: if ty == NodeType::Dir { rel.join("") } else { rel.clone() },  // trailing slash for dirs
            record: FileMeta {
                encoded_path_b64: base64::engine::general_purpose::STANDARD_NO_PAD.encode(&*rel.to_raw_bytes()),
                ty,
                size: (ty == NodeType::File).then(|| md.len()),
                mtime_ns: mtime_ns(&md),
                mode,
                link_target: (ty == NodeType::Symlink).then(|| fs::read_link(&full_path).unwrap_or_else(|e| panic!("read_link failed for '{}': {e}", full_path.display()))),
            },
        }
    }

    pub fn deserialize_entry(line: &str) -> Self {
        let mut de = serde_json::Deserializer::from_str(line);

        let path_key: PathBuf = PathBuf::from(String::deserialize(&mut de)
            .unwrap_or_else(|e| panic!("invalid path json: {e}; line={line:?}")));

        let record = FileMeta::deserialize(&mut de)
            .unwrap_or_else(|e| panic!("invalid record json: {e}; line={line:?}"));

        if !de.end().is_ok() { panic!("tracking line has trailing junk; line={line:?}"); }

        ManifestEntry { path_key, record }
    }

    fn serialize_entry(&self) -> (String, String) {
        fn serialize<T>(some_str: &T) -> String where T: ?Sized + Serialize,{
            serde_json::to_string(&some_str).unwrap_or_else(|e| panic!("failed to serialize: {e}"))
        }

        (serialize(&self.path_key.clone()), serialize(&self.record))
    }


    /// Deserialize only the leading JSON string (path_key) from a line (even if there's nothing after)
    pub fn deserialize_path_key(line: &str) -> String {
        let mut it = serde_json::Deserializer::from_str(line).into_iter::<String>();

        it.next()
            .unwrap_or_else(|| panic!("tracking line missing path key; line={line:?}"))
            .unwrap_or_else(|e| panic!("invalid path json: {e}; line={line:?}"))
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////
//////////////////////////////////////////MANIFEST//////////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////

// .collect()
impl FromIterator<ManifestEntry> for Manifest { fn from_iter<I: IntoIterator<Item = ManifestEntry>>(iter: I) -> Self { Manifest(iter.into_iter().collect()) } }

// rayon .par_iter() trait
trait FromParallelIterator<T>: Sized { fn from_par_iter<I>(par_iter: I) -> Self where I: ParallelIterator<Item = T>; }

// rayon .collect()
impl FromParallelIterator<ManifestEntry> for Manifest { fn from_par_iter<I>(par_iter: I) -> Self where I: rayon::iter::ParallelIterator<Item = ManifestEntry>, { Manifest(par_iter.collect()) } }

// into()
impl From<Vec<ManifestEntry>> for Manifest { fn from(v: Vec<ManifestEntry>) -> Self { Manifest(v) } }

// from()
impl From<Manifest> for Vec<ManifestEntry> { fn from(m: Manifest) -> Self { m.0 } }

impl Manifest {

    /// Render entries as aligned lines: `<path_key_json><spaces><record_json>\n`
    /// where `record_json` starts at the same column for all lines.
    pub fn serialize(manifest: Manifest) -> Vec<String> {
        // Parallel map: ManifestEntry -> (key, record)
        let pairs: Vec<(String, String)> = Vec::<ManifestEntry>::from(manifest).par_iter()
            .map(ManifestEntry::serialize_entry)
            .collect();

        fn get_str_visual_width(s: &String) -> usize { UnicodeWidthStr::width(s.as_str()) }

        let pad_to = pairs.par_iter()
            .map(|(k, _)| get_str_visual_width(k))  // align visually, by width of characters, not byte-length
            .max()
            .unwrap_or(0)
            + 2;  // minimum 2 spaces in-between

        // Build final string (sequential join; avoids contention)
        let mut lines: Vec<String> = pairs.into_par_iter()
            .map(|(k, r)| [
                k.as_str(),
                &" ".repeat(pad_to.saturating_sub(get_str_visual_width(&k))),
                r.as_str()
            ].concat())
            .collect();

        lines.par_sort_unstable();
        lines
    }


    pub fn deserialize_manifest(content: &str) -> Manifest {
        let mut entries: Manifest = content.par_lines()
            .filter(|l| !l.is_empty())
            .map(ManifestEntry::deserialize_entry)
            .collect::<Vec<ManifestEntry>>()
            .into();

        entries.sort();
        entries
    }
    
    pub fn sort(&mut self) {
        self.0.par_sort_unstable_by(|a, b| a.path_key.cmp(&b.path_key));
    }

}



////////////////////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////////////////other///////////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////


fn mtime_ns(md: &fs::Metadata) -> i128 {
    let t = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128), // handle pre-epoch if it ever happens
    }
}

