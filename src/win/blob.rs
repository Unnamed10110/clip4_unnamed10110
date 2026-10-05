//! Content-addressed sidecar files for large payloads (spec 8.3).
//!
//! File layout (ours): `"CBL1" u16:headLen head[headLen] DPAPI(payload)`.
//! `head` is a cleartext copy of a DIB *header* only (so an image row can show its
//! dimensions at load without decrypting megabytes of pixels); it is empty for every
//! other format. Files are DPAPI-encrypted individually (spec 20.2).
use super::{dpapi, sha1::sha1, util::hex};
use crate::model::{BlobSource, FormatKey, CF_DIB, CF_DIBV5};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const MAGIC: &[u8; 4] = b"CBL1";
/// Unreferenced blobs younger than this are never collected (a capture may be in flight).
const GC_GRACE: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct BlobStore {
    dir: PathBuf,
}

fn head_len_for(key: &FormatKey, data: &[u8]) -> usize {
    if (key.is_std(CF_DIB) || key.is_std(CF_DIBV5)) && data.len() >= 4 {
        let sz = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        sz.clamp(12, 124).min(data.len())
    } else {
        0
    }
}

impl BlobStore {
    pub fn new(dir: PathBuf) -> BlobStore {
        BlobStore { dir }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, sha: &[u8; 20]) -> PathBuf {
        self.dir.join(hex(sha))
    }

    /// Writes `data` (once; identical payloads share a file) and returns its SHA-1.
    pub fn put(&self, key: &FormatKey, data: &[u8]) -> Option<[u8; 20]> {
        let sha = sha1(data)?;
        let path = self.path(&sha);
        if path.exists() {
            // Refresh mtime so the GC grace period protects a re-captured payload.
            if let Ok(f) = fs::OpenOptions::new().write(true).open(&path) {
                let _ = f.set_modified(SystemTime::now());
            }
            return Some(sha);
        }
        fs::create_dir_all(&self.dir).ok()?;
        let enc = dpapi::protect(data, "clip4 blob")?;
        let hl = head_len_for(key, data);
        let mut buf = Vec::with_capacity(6 + hl + enc.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(hl as u16).to_le_bytes());
        buf.extend_from_slice(&data[..hl]);
        buf.extend_from_slice(&enc);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &buf).ok()?;
        fs::rename(&tmp, &path).ok()?;
        Some(sha)
    }

    pub fn get(&self, sha: &[u8; 20]) -> Option<Vec<u8>> {
        let buf = fs::read(self.path(sha)).ok()?;
        if buf.len() < 6 || &buf[..4] != MAGIC {
            return None;
        }
        let hl = u16::from_le_bytes([buf[4], buf[5]]) as usize;
        dpapi::unprotect(buf.get(6 + hl..)?)
    }

    pub fn head(&self, sha: &[u8; 20]) -> Option<Vec<u8>> {
        let mut f = File::open(self.path(sha)).ok()?;
        let mut h = [0u8; 6];
        f.read_exact(&mut h).ok()?;
        if &h[..4] != MAGIC {
            return None;
        }
        let hl = u16::from_le_bytes([h[4], h[5]]) as usize;
        let mut head = vec![0u8; hl];
        f.read_exact(&mut head).ok()?;
        Some(head)
    }

    /// Deletes blob files not in `keep` (and older than the grace period).
    pub fn gc(&self, keep: &HashSet<[u8; 20]>) -> usize {
        let Ok(rd) = fs::read_dir(&self.dir) else { return 0 };
        let mut removed = 0;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let referenced = super::util::unhex20(&name).is_some_and(|s| keep.contains(&s));
            if referenced {
                continue;
            }
            let old_enough = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > GC_GRACE);
            if old_enough && fs::remove_file(e.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// Removes every blob (Clear history).
    pub fn clear(&self) {
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

impl BlobSource for BlobStore {
    fn read_all(&self, sha1: &[u8; 20]) -> Option<Vec<u8>> {
        self.get(sha1)
    }
    fn read_head(&self, sha1: &[u8; 20]) -> Option<Vec<u8>> {
        self.head(sha1)
    }
}

/// clip2's blob directory: raw, unencrypted files named `<sha1-hex>` (spec 8.6).
pub struct Clip2Blobs(pub PathBuf);

impl BlobSource for Clip2Blobs {
    fn read_all(&self, sha1: &[u8; 20]) -> Option<Vec<u8>> {
        fs::read(self.0.join(hex(sha1))).ok()
    }
    fn read_head(&self, sha1: &[u8; 20]) -> Option<Vec<u8>> {
        let mut f = File::open(self.0.join(hex(sha1))).ok()?;
        let mut v = vec![0u8; 124];
        let n = f.read(&mut v).ok()?;
        v.truncate(n);
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BLOB_THRESHOLD;

    #[test]
    fn put_get_head_gc() {
        let dir = std::env::temp_dir().join(format!("clip4-blob-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bs = BlobStore::new(dir.clone());
        let mut dib = vec![0u8; BLOB_THRESHOLD + 10];
        dib[0] = 40; // biSize
        let sha = bs.put(&FormatKey::Standard(CF_DIB), &dib).expect("put");
        assert_eq!(bs.put(&FormatKey::Standard(CF_DIB), &dib), Some(sha), "idempotent");
        assert_eq!(bs.get(&sha).expect("get"), dib);
        assert_eq!(bs.head(&sha).expect("head").len(), 40);
        let txt = vec![b'a'; BLOB_THRESHOLD];
        let sha2 = bs.put(&FormatKey::Standard(1), &txt).expect("put2");
        assert_eq!(bs.head(&sha2).expect("head2").len(), 0);
        // Fresh files are protected by the grace period.
        assert_eq!(bs.gc(&HashSet::new()), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
