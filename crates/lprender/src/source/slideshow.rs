//! A directory of images, cycled on a timer.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::source::{Source, Wanted};

/// A directory of images, cycled on a timer. The milestone-3 deliverable:
/// the whole render path with no `artd` involved.
#[derive(Debug)]
pub struct Slideshow {
    files: Vec<PathBuf>,
    interval_ms: u64,
    index: usize,
    next_at_ms: u64,
    started: bool,
}

impl Slideshow {
    pub fn new(dir: &Path, interval_ms: u64) -> Result<Slideshow> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
                    matches!(x.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")
                })
            })
            .collect();
        files.sort();
        if files.is_empty() {
            anyhow::bail!("no .jpg or .png files in {}", dir.display());
        }
        Ok(Slideshow {
            files,
            interval_ms: interval_ms.max(100),
            index: 0,
            next_at_ms: 0,
            started: false,
        })
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl Source for Slideshow {
    fn poll(&mut self, now_ms: u64) -> Option<Wanted> {
        if self.started && now_ms < self.next_at_ms {
            return None;
        }
        if self.started {
            self.index = (self.index + 1) % self.files.len();
        }
        self.started = true;
        self.next_at_ms = now_ms + self.interval_ms;
        Some(Wanted {
            // Monotonic across wraps, so revisiting an image still counts as
            // a change and crossfades rather than silently doing nothing.
            id: self.index as u64 + 1,
            path: self.files[self.index].clone(),
            is_upgrade: false,
        })
    }

    fn next_wakeup_ms(&self, _now_ms: u64) -> Option<u64> {
        Some(self.next_at_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lprender-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_png(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]))
            .save(&p)
            .unwrap();
        p
    }

    #[test]
    fn a_slideshow_lists_only_images_in_sorted_order() {
        let d = tmpdir("list");
        write_png(&d, "b.png");
        write_png(&d, "a.png");
        std::fs::write(d.join("notes.txt"), b"ignore me").unwrap();
        std::fs::write(d.join("cover.JPG"), b"not really a jpeg").unwrap();

        let s = Slideshow::new(&d, 1000).unwrap();
        // Extension matching is case-insensitive; content is the decoder's
        // problem, not the lister's.
        assert_eq!(s.len(), 3);
        assert!(s.files[0].ends_with("a.png"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_directory_is_an_error_rather_than_a_blank_screen() {
        let d = tmpdir("empty");
        let err = Slideshow::new(&d, 1000).unwrap_err();
        assert!(err.to_string().contains("no .jpg or .png"), "{err}");
        assert!(Slideshow::new(&d.join("nope"), 1000).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_slideshow_advances_on_its_interval_and_wraps() {
        let d = tmpdir("advance");
        write_png(&d, "a.png");
        write_png(&d, "b.png");
        let mut s = Slideshow::new(&d, 1000).unwrap();

        let first = s.poll(0).expect("first image immediately");
        assert!(first.path.ends_with("a.png"));
        assert_eq!(s.poll(500), None, "advanced before the interval elapsed");

        let second = s.poll(1000).expect("second image");
        assert!(second.path.ends_with("b.png"));
        assert_ne!(second.id, first.id);

        let third = s.poll(2000).expect("wrapped back around");
        assert!(third.path.ends_with("a.png"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_slideshow_always_asks_to_be_woken_in_the_future() {
        let d = tmpdir("wake");
        write_png(&d, "a.png");
        let mut s = Slideshow::new(&d, 1000).unwrap();
        s.poll(0);
        assert_eq!(s.next_wakeup_ms(0), Some(1000));
        let _ = std::fs::remove_dir_all(&d);
    }
}
