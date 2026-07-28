//! Image decoding, off the render thread.
//!
//! Decoding happens on a worker; the render thread only ever uploads an
//! already-decoded buffer. The CPU downscale before upload is not an
//! optimisation detail — a 3000² texture on a 720² panel costs memory that
//! comes out of CMA on a Pi 4, and mipmapping it wastes more.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result};
use image::imageops::FilterType;

/// A decoded image ready for upload.
pub struct Decoded {
    pub id: u64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Dimensions before downscaling, for diagnostics.
    pub source_size: (u32, u32),
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoded")
            .field("id", &self.id)
            .field("size", &(self.width, self.height))
            .field("source_size", &self.source_size)
            .finish()
    }
}

/// Decode and downscale to at most `max_edge` on the longer side.
pub fn decode(path: &Path, id: u64, max_edge: u32) -> Result<Decoded> {
    let img = image::open(path).with_context(|| format!("decoding {}", path.display()))?;
    let source_size = (img.width(), img.height());

    let longest = source_size.0.max(source_size.1);
    let img = if max_edge > 0 && longest > max_edge {
        // Lanczos3 rather than a box filter: this is the step that decides
        // how sharp a 3000² cover looks at 1920², and it happens once per
        // track rather than per frame.
        img.resize(max_edge, max_edge, FilterType::Lanczos3)
    } else {
        img
    };

    let rgba = img.to_rgba8();
    Ok(Decoded {
        id,
        path: path.to_path_buf(),
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
        source_size,
    })
}

/// A background decoder thread.
///
/// One worker is enough: images arrive at most once per track, and a queue
/// keeps the render thread from ever blocking on libjpeg.
pub struct Decoder {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<Result<Decoded>>,
}

struct Request {
    path: PathBuf,
    id: u64,
    max_edge: u32,
}

impl Decoder {
    pub fn spawn() -> Decoder {
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, res_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("lprender-decode".into())
            .spawn(move || {
                while let Ok(req) = req_rx.recv() {
                    let out = decode(&req.path, req.id, req.max_edge);
                    if res_tx.send(out).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning the decode thread");
        Decoder {
            requests: req_tx,
            results: res_rx,
        }
    }

    pub fn request(&self, path: &Path, id: u64, max_edge: u32) {
        let _ = self.requests.send(Request {
            path: path.to_path_buf(),
            id,
            max_edge,
        });
    }

    /// Collect whatever has finished, without blocking.
    pub fn poll(&self) -> Vec<Result<Decoded>> {
        self.results.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let path = dir.join(name);
        let mut buf = image::RgbaImage::new(w, h);
        for (x, y, p) in buf.enumerate_pixels_mut() {
            *p = image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        buf.save(&path).unwrap();
        path
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lprender-decode-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn decodes_and_reports_true_dimensions() {
        let dir = tmpdir("basic");
        let p = write_png(&dir, "a.png", 64, 48);
        let d = decode(&p, 1, 0).unwrap();
        assert_eq!((d.width, d.height), (64, 48));
        assert_eq!(d.source_size, (64, 48));
        assert_eq!(d.rgba.len(), 64 * 48 * 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downscales_only_when_larger_than_the_cap() {
        let dir = tmpdir("scale");
        let big = write_png(&dir, "big.png", 400, 200);

        let d = decode(&big, 1, 100).unwrap();
        assert_eq!(d.width.max(d.height), 100, "not capped");
        // Aspect preserved.
        assert_eq!((d.width, d.height), (100, 50));
        assert_eq!(d.source_size, (400, 200));

        // Below the cap: untouched, no needless resample.
        let d = decode(&big, 1, 4000).unwrap();
        assert_eq!((d.width, d.height), (400, 200));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_panic() {
        let dir = tmpdir("corrupt");
        let p = dir.join("bad.png");
        std::fs::write(&p, b"this is not a png").unwrap();
        assert!(decode(&p, 1, 0).is_err());
        assert!(decode(&dir.join("missing.png"), 1, 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_worker_returns_results_without_blocking_the_caller() {
        let dir = tmpdir("worker");
        let a = write_png(&dir, "a.png", 32, 32);
        let b = write_png(&dir, "b.png", 16, 16);

        let d = Decoder::spawn();
        d.request(&a, 1, 0);
        d.request(&b, 2, 0);

        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(d.poll().into_iter().filter_map(|r| r.ok()));
            if got.len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(got.len(), 2, "decode worker did not deliver");
        let mut ids: Vec<u64> = got.iter().map(|d| d.id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_decode_is_reported_rather_than_dropped() {
        let dir = tmpdir("worker-err");
        let bad = dir.join("bad.png");
        std::fs::write(&bad, b"nope").unwrap();

        let d = Decoder::spawn();
        d.request(&bad, 9, 0);
        let mut saw_error = false;
        for _ in 0..200 {
            if d.poll().iter().any(|r| r.is_err()) {
                saw_error = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_error, "a decode failure vanished silently");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
