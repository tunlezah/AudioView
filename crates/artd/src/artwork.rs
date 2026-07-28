//! Artwork staging on tmpfs.
//!
//! Image bytes never traverse the IPC socket. `artd` writes each image to
//! `ipc.art_dir` and publishes a path; the renderer opens it directly.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

pub struct Stored {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub dimensions: Option<(u32, u32)>,
}

pub struct ArtStore {
    dir: PathBuf,
    retain: usize,
    /// Files written, oldest first. Kept so we can unlink behind the reader.
    written: VecDeque<PathBuf>,
    next_seq: u64,
}

impl ArtStore {
    pub fn new(dir: &Path, retain: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating artwork directory {}", dir.display()))?;
        // A restart leaves stale images behind; the renderer will be told a
        // fresh path immediately, so nothing here is still referenced.
        let mut cleared = 0;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.path()
                    .extension()
                    .is_some_and(|x| x == "jpg" || x == "png")
                {
                    let _ = std::fs::remove_file(e.path());
                    cleared += 1;
                }
            }
        }
        if cleared > 0 {
            tracing::debug!(
                "cleared {cleared} stale artwork file(s) from {}",
                dir.display()
            );
        }
        Ok(ArtStore {
            dir: dir.to_path_buf(),
            retain: retain.max(2),
            written: VecDeque::new(),
            next_seq: 1,
        })
    }

    pub fn store(&mut self, bytes: &[u8]) -> Result<Stored> {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let kind = ImageKind::sniff(bytes);
        let seq = self.next_seq;
        self.next_seq += 1;

        let name = format!("{seq:04}-{}.{}", &sha256[..8], kind.extension());
        let final_path = self.dir.join(&name);
        let tmp_path = self.dir.join(format!(".{name}.tmp"));

        // Write then rename, so a reader never sees a partial image.
        std::fs::write(&tmp_path, bytes)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("renaming into {}", final_path.display()))?;

        self.written.push_back(final_path.clone());
        self.gc();

        Ok(Stored {
            path: final_path,
            sha256,
            bytes: bytes.len() as u64,
            dimensions: kind.dimensions(bytes),
        })
    }

    /// Unlink old revisions, keeping the most recent few.
    ///
    /// The renderer may still be decoding the previous image when the next
    /// arrives, so the retained window is what stops a path being pulled out
    /// from under it mid-read.
    fn gc(&mut self) {
        while self.written.len() > self.retain {
            if let Some(old) = self.written.pop_front() {
                let _ = std::fs::remove_file(old);
            }
        }
    }

    pub fn clear(&mut self) {
        while let Some(p) = self.written.pop_front() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Unknown,
}

impl ImageKind {
    pub fn sniff(b: &[u8]) -> ImageKind {
        if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
            ImageKind::Jpeg
        } else if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
            ImageKind::Png
        } else {
            ImageKind::Unknown
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ImageKind::Jpeg | ImageKind::Unknown => "jpg",
            ImageKind::Png => "png",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            ImageKind::Jpeg | ImageKind::Unknown => "image/jpeg",
            ImageKind::Png => "image/png",
        }
    }

    /// Read dimensions from the header only.
    ///
    /// Full decoding belongs in the renderer, off the hot path; here we only
    /// want enough to report size and, later, to gate an enrichment upgrade
    /// on the candidate actually being bigger.
    pub fn dimensions(self, b: &[u8]) -> Option<(u32, u32)> {
        match self {
            ImageKind::Png => png_dimensions(b),
            ImageKind::Jpeg => jpeg_dimensions(b),
            ImageKind::Unknown => None,
        }
    }
}

fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    // 8-byte signature, 4-byte length, "IHDR", then width and height.
    if b.len() < 24 || &b[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // past SOI
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // Standalone markers carry no length.
        if matches!(marker, 0xD8 | 0xD9 | 0x01) || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        // SOF0..SOF15, excluding the non-frame markers in that range.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("artd-art-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn sniffs_png_and_jpeg() {
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(ImageKind::sniff(&png), ImageKind::Png);
        assert_eq!(ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), ImageKind::Jpeg);
        assert_eq!(ImageKind::sniff(b"not an image"), ImageKind::Unknown);
        assert_eq!(ImageKind::sniff(&[]), ImageKind::Unknown);
    }

    #[test]
    fn reads_png_dimensions() {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&500u32.to_be_bytes());
        b.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(ImageKind::Png.dimensions(&b), Some((500, 480)));
    }

    #[test]
    fn reads_jpeg_dimensions_past_a_leading_segment() {
        let mut b = vec![0xFF, 0xD8];
        // A JFIF APP0 segment first, so we exercise the skip loop.
        b.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        b.extend_from_slice(&[0u8; 14]);
        // SOF0: len, precision, height, width
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&600u16.to_be_bytes());
        b.extend_from_slice(&640u16.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        assert_eq!(ImageKind::Jpeg.dimensions(&b), Some((640, 600)));
    }

    #[test]
    fn truncated_headers_return_none_rather_than_panicking() {
        for n in 0..24 {
            let b = vec![0x89u8; n];
            let _ = ImageKind::Png.dimensions(&b);
            let _ = ImageKind::Jpeg.dimensions(&b);
        }
        assert_eq!(ImageKind::Jpeg.dimensions(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn stores_atomically_and_evicts_old_revisions() {
        let dir = tmpdir("store");
        let mut store = ArtStore::new(&dir, 2).unwrap();

        let mut paths = Vec::new();
        for i in 0..5u8 {
            let s = store.store(&[i; 64]).unwrap();
            assert!(s.path.exists());
            assert_eq!(s.bytes, 64);
            paths.push(s.path);
        }

        // Only the retained window survives; no .tmp files are left behind.
        let live: Vec<_> = paths.iter().filter(|p| p.exists()).collect();
        assert_eq!(live.len(), 2, "retain window not honoured");
        assert!(live.contains(&&paths[4]));
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_clears_stale_images() {
        let dir = tmpdir("restart");
        let mut store = ArtStore::new(&dir, 4).unwrap();
        let p = store.store(&[7; 32]).unwrap().path;
        assert!(p.exists());

        drop(store);
        let _store2 = ArtStore::new(&dir, 4).unwrap();
        assert!(!p.exists(), "stale artwork survived a restart");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
