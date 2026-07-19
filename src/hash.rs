//! Content hashing (blake3). Reading every byte to hash is also our content-equality check —
//! a hash match means identical content (collision odds ~2⁻²⁵⁶), not just an equal name.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// blake3 hash of a file's contents. A permission wall (EACCES/EPERM) is retried with root when
/// it's in reserve (see [`crate::elevation`]) — reading changes nothing about the file.
pub fn hash_file(path: &Path) -> std::io::Result<blake3::Hash> {
    let first = hash_once(path);
    crate::elevation::retry_if_permission("read for hashing", path, first, || hash_once(path))
}

fn hash_once(path: &Path) -> std::io::Result<blake3::Hash> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}
