//! The render loop, independent of which backend supplies the framebuffer.
//!
//! Frame pacing is the important part. The loop only draws when [`Scene`]
//! says something is animating; a static image means no draw and no page
//! flip, and the scanout hardware keeps showing the last framebuffer at no
//! cost (DESIGN §6.3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use lpframe_config::{Background, Config};
use lpframe_proto::DisplayPower;

use crate::color::{palette, Palette};
use crate::decode::{Decoded, Decoder};
use crate::geometry::{layout, LayoutConfig};
use crate::gl::{DrawCall, Renderer, Texture};
use crate::scene::{Scene, Timings};

/// Where images come from.
pub trait Source {
    /// The next image to show, if it has changed. `None` means no change.
    fn poll(&mut self, now_ms: u64) -> Option<Wanted>;
    /// The requested display power state.
    fn power(&self) -> DisplayPower {
        DisplayPower::On
    }
    /// When the source next wants attention, in monotonic milliseconds.
    fn next_wakeup_ms(&self, _now_ms: u64) -> Option<u64> {
        None
    }
}

/// An image the source would like shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// Stable identity. A repeat of the current id is ignored.
    pub id: u64,
    pub path: PathBuf,
    /// Use the shorter crossfade: the same album, just sharper.
    pub is_upgrade: bool,
}

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

/// Resident images, keyed by the id the scene refers to.
struct Images {
    map: HashMap<u64, Texture>,
    palettes: HashMap<u64, Palette>,
}

impl Images {
    fn new() -> Images {
        Images {
            map: HashMap::new(),
            palettes: HashMap::new(),
        }
    }

    /// Drop everything the scene no longer references.
    fn retain(&mut self, renderer: &Renderer, keep: &[Option<u64>]) {
        let live: Vec<u64> = keep.iter().flatten().copied().collect();
        let dead: Vec<u64> = self
            .map
            .keys()
            .copied()
            .filter(|k| !live.contains(k))
            .collect();
        for id in dead {
            if let Some(t) = self.map.remove(&id) {
                renderer.delete_texture(t);
            }
            self.palettes.remove(&id);
        }
    }
}

pub struct App {
    pub scene: Scene,
    cfg: Config,
    decoder: Decoder,
    images: Images,
    /// Requested but not yet decoded.
    pending: Option<Wanted>,
    started: Instant,
    max_edge: u32,
}

impl App {
    pub fn new(cfg: Config, panel: (u32, u32)) -> App {
        let timings = Timings {
            crossfade_ms: cfg.render.crossfade.as_millis(),
            enrichment_crossfade_ms: cfg.render.enrichment_crossfade.as_millis(),
            fade_out_ms: cfg.power.display.fade_out.as_millis(),
            fade_in_ms: 400,
            ken_burns: cfg.render.ken_burns,
            ken_burns_period_ms: cfg.render.ken_burns_period.as_millis(),
        };
        // No value in a 3000² texture on a 720² panel, and on a Pi 4 that
        // memory comes out of CMA.
        let max_edge = (panel.0.max(panel.1) * 2).clamp(512, 3000);
        App {
            scene: Scene::new(timings, 0),
            cfg,
            decoder: Decoder::spawn(),
            images: Images::new(),
            pending: None,
            started: Instant::now(),
            max_edge,
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Advance by one iteration. Returns true if a frame was drawn.
    pub fn step(&mut self, renderer: &mut Renderer, source: &mut dyn Source) -> bool {
        let now = self.now_ms();
        self.step_at(renderer, source, now)
    }

    /// As [`App::step`], but at an explicit time. Used by the headless dump
    /// so a sequence of frames is reproducible rather than depending on how
    /// fast the machine happens to be.
    pub fn step_at(&mut self, renderer: &mut Renderer, source: &mut dyn Source, now: u64) -> bool {
        self.update_at(renderer, source, now);
        if !self.scene.needs_frame(now) {
            return false;
        }
        self.draw(renderer, now);
        true
    }

    /// Poll the source and admit anything that finished decoding, without
    /// drawing.
    ///
    /// Separate from the draw so a caller that always wants a frame — the
    /// headless dump — does not have to poll the source itself and
    /// accidentally consume the update this call was going to see.
    pub fn update_at(&mut self, renderer: &mut Renderer, source: &mut dyn Source, now: u64) {
        if let Some(wanted) = source.poll(now) {
            if Some(wanted.id) != self.scene.current() {
                self.decoder.request(&wanted.path, wanted.id, self.max_edge);
                self.pending = Some(wanted);
            }
        }
        self.scene.set_power(source.power(), now);

        // Uploads happen here, and the scene is only told about an image once
        // it is fully resident — a crossfade against a texture that is still
        // uploading is how frames get dropped.
        for result in self.decoder.poll() {
            match result {
                Ok(decoded) => self.admit(renderer, decoded, now),
                Err(e) => {
                    tracing::warn!("{e:#}");
                    self.pending = None;
                }
            }
        }
    }

    /// Block until every requested image has been decoded and uploaded.
    ///
    /// Only for the headless dump, which simulates time and would otherwise
    /// race the decode thread and emit black frames.
    pub fn settle(&mut self, renderer: &mut Renderer, timeout_ms: u64, now: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while self.pending.is_some() && std::time::Instant::now() < deadline {
            for result in self.decoder.poll() {
                match result {
                    Ok(decoded) => self.admit(renderer, decoded, now),
                    Err(e) => {
                        tracing::warn!("{e:#}");
                        self.pending = None;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn admit(&mut self, renderer: &mut Renderer, decoded: Decoded, now: u64) {
        let pal = palette(&decoded.rgba, decoded.width, decoded.height);
        match renderer.upload(decoded.width, decoded.height, &decoded.rgba) {
            Ok(tex) => {
                tracing::debug!(
                    "resident: {} {}x{} (source {}x{})",
                    decoded.path.display(),
                    decoded.width,
                    decoded.height,
                    decoded.source_size.0,
                    decoded.source_size.1
                );
                self.images.map.insert(decoded.id, tex);
                self.images.palettes.insert(decoded.id, pal);
                let is_upgrade = self
                    .pending
                    .as_ref()
                    .is_some_and(|w| w.id == decoded.id && w.is_upgrade);
                self.scene.present(decoded.id, is_upgrade, now);
                self.pending = None;
                self.images
                    .retain(renderer, &[self.scene.current(), self.scene.previous()]);
            }
            Err(e) => tracing::warn!("upload failed: {e:#}"),
        }
    }

    /// Draw the current frame into whatever framebuffer is bound.
    pub fn draw(&mut self, renderer: &mut Renderer, now_ms: u64) {
        let frame = self.scene.frame(now_ms);
        let panel = renderer.panel();

        let current = self.scene.current().and_then(|id| self.images.map.get(&id));
        let previous = self
            .scene
            .previous()
            .and_then(|id| self.images.map.get(&id));
        let sized = current.or(previous);
        let (iw, ih) = sized.map_or((1, 1), |t| (t.width, t.height));

        let layout = layout(
            panel.0,
            panel.1,
            iw,
            ih,
            &LayoutConfig {
                square: self.cfg.render.square,
                fit: self.cfg.render.fit,
                rotation: self.cfg.display.rotation,
                margin_percent: self.cfg.display.margin_percent,
            },
        );

        let pal = self
            .scene
            .current()
            .and_then(|id| self.images.palettes.get(&id))
            .copied()
            .unwrap_or_default();

        let ambient_dim =
            (self.scene.power() == DisplayPower::Ambient).then_some(self.cfg.render.ambient_dim);
        // Ambient is the blurred, dimmed last artwork, whatever the
        // configured background happens to be.
        let background = if self.scene.power() == DisplayPower::Ambient {
            Background::Blur
        } else {
            self.cfg.render.background
        };

        renderer.draw(&DrawCall {
            layout,
            frame,
            current,
            previous,
            background,
            background_dim: self.cfg.render.background_dim,
            palette: pal,
            ambient_dim,
        });
    }

    /// How long the caller may sleep before the next iteration.
    pub fn next_wakeup_ms(&self, source: &dyn Source) -> Option<u64> {
        let now = self.now_ms();
        let a = self.scene.next_frame_at_ms(now);
        let b = source.next_wakeup_ms(now);
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// True once a fade-out has completed, so a backend may power the panel
    /// down rather than keep scanning out black.
    pub fn blanked(&mut self) -> bool {
        let now = self.now_ms();
        self.scene.frame(now).blanked
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
