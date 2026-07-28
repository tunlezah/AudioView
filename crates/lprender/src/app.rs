//! The render loop, independent of which backend supplies the framebuffer.
//!
//! Frame pacing is the important part. The loop only draws when [`Scene`]
//! says something is animating; a static image means no draw and no page
//! flip, and the scanout hardware keeps showing the last framebuffer at no
//! cost (DESIGN §6.3).

use std::collections::HashMap;
use std::time::Instant;

use lpframe_config::{Background, Config};
use lpframe_proto::DisplayPower;

use crate::color::{palette, Palette};
use crate::decode::{Decoded, Decoder};
use crate::geometry::{layout, LayoutConfig};
use crate::gl::{DrawCall, Renderer, Texture};
use crate::scene::{Scene, Timings};
use crate::source::{Source, Wanted};

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
