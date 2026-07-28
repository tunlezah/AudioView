//! What to draw right now: crossfade progress, fade to and from black,
//! Ken Burns phase.
//!
//! Pure, with time passed in. The single most important property here is
//! [`Scene::needs_frame`]: when nothing is animating it must return false, so
//! the event loop blocks and the device does zero work while showing a static
//! image (DESIGN §6.3). A renderer that quietly spins at 60fps on a still
//! picture costs several watts continuously.

use lpframe_proto::DisplayPower;

/// Cubic ease-in-out over `0..1`.
pub fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = -2.0 * t + 2.0;
        1.0 - f * f * f / 2.0
    }
}

/// A linear ramp between two values over a fixed duration.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ramp {
    from: f32,
    to: f32,
    start_ms: u64,
    duration_ms: u64,
}

impl Ramp {
    fn held(value: f32) -> Ramp {
        Ramp {
            from: value,
            to: value,
            start_ms: 0,
            duration_ms: 0,
        }
    }

    fn value(&self, now_ms: u64) -> f32 {
        if self.duration_ms == 0 {
            return self.to;
        }
        let elapsed = now_ms.saturating_sub(self.start_ms) as f32;
        let t = (elapsed / self.duration_ms as f32).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * t
    }

    fn done(&self, now_ms: u64) -> bool {
        self.duration_ms == 0 || now_ms.saturating_sub(self.start_ms) >= self.duration_ms
    }

    fn ends_at(&self) -> u64 {
        self.start_ms.saturating_add(self.duration_ms)
    }
}

/// Timings, from config.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub crossfade_ms: u64,
    /// Shorter: the same album getting sharper is not a new record.
    pub enrichment_crossfade_ms: u64,
    pub fade_out_ms: u64,
    pub fade_in_ms: u64,
    pub ken_burns: bool,
    pub ken_burns_period_ms: u64,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            crossfade_ms: 600,
            enrichment_crossfade_ms: 250,
            fade_out_ms: 1000,
            fade_in_ms: 400,
            ken_burns: false,
            ken_burns_period_ms: 180_000,
        }
    }
}

/// What the renderer should draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// Blend between `previous` (0) and `current` (1), already eased.
    pub mix: f32,
    /// Overall brightness, 0 = black. Applied in the fragment shader rather
    /// than via the CRTC gamma ramp, which is inconsistent across
    /// BCM2711/2712 (DESIGN §6.1).
    pub global_fade: f32,
    /// Ken Burns zoom factor, 1.0 when disabled.
    pub zoom: f32,
    /// Whether a further frame is needed after this one.
    pub animating: bool,
    /// True once a fade-out has completed, so the caller can turn the CRTC
    /// off. Only meaningful when the requested power state is `Off`.
    pub blanked: bool,
}

pub struct Scene {
    timings: Timings,
    /// Identity of the image being shown, and the one fading out.
    current: Option<u64>,
    previous: Option<u64>,
    crossfade: Ramp,
    fade: Ramp,
    power: DisplayPower,
    ken_burns_origin_ms: u64,
    /// Set while `power` is `Off` and the fade has finished.
    blanked: bool,
}

impl Scene {
    pub fn new(timings: Timings, now_ms: u64) -> Scene {
        Scene {
            timings,
            current: None,
            previous: None,
            crossfade: Ramp::held(1.0),
            // Start black: the first image fades in rather than snapping on.
            fade: Ramp::held(0.0),
            power: DisplayPower::Off,
            ken_burns_origin_ms: now_ms,
            blanked: true,
        }
    }

    pub fn current(&self) -> Option<u64> {
        self.current
    }

    pub fn previous(&self) -> Option<u64> {
        self.previous
    }

    pub fn timings(&self) -> &Timings {
        &self.timings
    }

    pub fn set_timings(&mut self, timings: Timings) {
        self.timings = timings;
    }

    /// Show an image that is **already decoded and resident**.
    ///
    /// Called only once the upload has completed, never when it is requested:
    /// starting a crossfade against a texture that is still uploading is
    /// exactly how frames get dropped (DESIGN §6.3).
    pub fn present(&mut self, id: u64, is_upgrade: bool, now_ms: u64) {
        if self.current == Some(id) {
            return;
        }
        let duration = if is_upgrade {
            self.timings.enrichment_crossfade_ms
        } else {
            self.timings.crossfade_ms
        };

        // Mid-crossfade, the outgoing image is whatever is currently most
        // visible. Keeping the *original* previous would make the new fade
        // start from a frame the viewer has already stopped seeing.
        if !self.crossfade.done(now_ms) && self.crossfade.value(now_ms) < 0.5 {
            // Still mostly showing `previous`; keep it as the outgoing image.
        } else {
            self.previous = self.current;
        }
        self.current = Some(id);

        self.crossfade = if self.previous.is_none() {
            // Nothing to fade from; the global fade handles the appearance.
            Ramp::held(1.0)
        } else {
            Ramp {
                from: 0.0,
                to: 1.0,
                start_ms: now_ms,
                duration_ms: duration,
            }
        };
        self.ken_burns_origin_ms = now_ms;
    }

    /// Drop all imagery, e.g. the track has no artwork.
    pub fn clear(&mut self, now_ms: u64) {
        self.previous = self.current;
        self.current = None;
        self.crossfade = Ramp {
            from: 0.0,
            to: 1.0,
            start_ms: now_ms,
            duration_ms: self.timings.crossfade_ms,
        };
    }

    /// Request a display power state. Idempotent.
    pub fn set_power(&mut self, power: DisplayPower, now_ms: u64) {
        if self.power == power {
            return;
        }
        let from = self.fade.value(now_ms);
        self.power = power;
        let (to, duration) = match power {
            DisplayPower::Off => (0.0, self.timings.fade_out_ms),
            // Ambient is dimmed rather than dark; the blur itself is applied
            // by the renderer, not here.
            DisplayPower::Ambient | DisplayPower::On => (1.0, self.timings.fade_in_ms),
        };
        if to > 0.0 {
            self.blanked = false;
        }
        self.fade = Ramp {
            from,
            to,
            start_ms: now_ms,
            duration_ms: duration,
        };
    }

    pub fn power(&self) -> DisplayPower {
        self.power
    }

    /// Evaluate the frame at `now_ms`.
    ///
    /// Driven by the monotonic clock rather than a frame counter, so a
    /// dropped frame shortens the transition instead of stretching it.
    pub fn frame(&mut self, now_ms: u64) -> Frame {
        let mix = ease_in_out(self.crossfade.value(now_ms));
        let global_fade = self.fade.value(now_ms);

        let crossfade_done = self.crossfade.done(now_ms);
        if crossfade_done && self.previous.is_some() {
            // Release the outgoing image so its texture can be reclaimed.
            self.previous = None;
        }

        let fade_done = self.fade.done(now_ms);
        if fade_done && self.power == DisplayPower::Off && global_fade <= 0.0 {
            self.blanked = true;
        }

        let zoom = self.zoom(now_ms);
        // Ken Burns never stops on its own, but it is pointless while black.
        let ken_burns_running =
            self.timings.ken_burns && global_fade > 0.0 && self.current.is_some();

        Frame {
            mix,
            global_fade,
            zoom,
            animating: !crossfade_done || !fade_done || ken_burns_running,
            blanked: self.blanked,
        }
    }

    fn zoom(&self, now_ms: u64) -> f32 {
        if !self.timings.ken_burns || self.timings.ken_burns_period_ms == 0 {
            return 1.0;
        }
        let period = self.timings.ken_burns_period_ms;
        let phase =
            (now_ms.saturating_sub(self.ken_burns_origin_ms) % (period * 2)) as f32 / period as f32;
        // Ping-pong 0→1→0 so the zoom reverses rather than snapping back.
        let t = if phase <= 1.0 { phase } else { 2.0 - phase };
        1.0 + 0.04 * ease_in_out(t)
    }

    /// Whether another frame is required. False means the event loop can
    /// block indefinitely and the scanout hardware keeps showing the last
    /// framebuffer at no cost.
    pub fn needs_frame(&self, now_ms: u64) -> bool {
        if !self.crossfade.done(now_ms) || !self.fade.done(now_ms) {
            return true;
        }
        self.timings.ken_burns && self.fade.value(now_ms) > 0.0 && self.current.is_some()
    }

    /// When the next frame is due, for a loop that sleeps rather than polls.
    pub fn next_frame_at_ms(&self, now_ms: u64) -> Option<u64> {
        if self.needs_frame(now_ms) {
            let ends = [self.crossfade.ends_at(), self.fade.ends_at()];
            let soonest = ends.into_iter().filter(|t| *t > now_ms).min();
            // Ken Burns wants a steady cadence; transitions want vsync.
            Some(soonest.unwrap_or(now_ms + 16))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        let mut s = Scene::new(Timings::default(), 0);
        s.set_power(DisplayPower::On, 0);
        s
    }

    #[test]
    fn easing_is_smooth_and_anchored() {
        assert!((ease_in_out(0.0)).abs() < 1e-6);
        assert!((ease_in_out(1.0) - 1.0).abs() < 1e-6);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        // Monotonic.
        let mut prev = -1.0;
        for i in 0..=100 {
            let v = ease_in_out(i as f32 / 100.0);
            assert!(v >= prev, "not monotonic at {i}");
            prev = v;
        }
        // Out-of-range input is clamped rather than extrapolated.
        assert_eq!(ease_in_out(-5.0), 0.0);
        assert_eq!(ease_in_out(5.0), 1.0);
    }

    #[test]
    fn a_static_image_needs_no_further_frames() {
        // The property the whole idle-power story rests on.
        let mut s = scene();
        s.present(1, false, 0);
        // Let the fade-in finish.
        let _ = s.frame(10_000);
        assert!(!s.needs_frame(10_000), "a still image is still animating");
        assert_eq!(s.next_frame_at_ms(10_000), None);
    }

    #[test]
    fn a_crossfade_runs_for_its_configured_duration_then_stops() {
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(5_000);

        s.present(2, false, 5_000);
        assert_eq!(s.previous(), Some(1));
        assert_eq!(s.current(), Some(2));

        assert!(s.frame(5_000).mix < 0.01, "crossfade did not start at 0");
        let mid = s.frame(5_300).mix;
        assert!((0.3..0.7).contains(&mid), "midpoint was {mid}");
        assert!(s.needs_frame(5_300));

        let end = s.frame(5_600);
        assert!((end.mix - 1.0).abs() < 1e-5);
        assert!(!s.needs_frame(5_600));
        // The outgoing image is released so its texture can be reclaimed.
        assert_eq!(s.previous(), None);
    }

    #[test]
    fn an_enrichment_upgrade_uses_the_shorter_crossfade() {
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(5_000);

        s.present(2, true, 5_000);
        // 250ms, so it is finished well before the 600ms track crossfade.
        assert!(!s.needs_frame(5_250 + 1));
        let f = s.frame(5_251);
        assert!((f.mix - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_dropped_frame_shortens_the_fade_rather_than_stretching_it() {
        // Time-based, not frame-counted: jumping past the end lands at 1.0
        // rather than resuming mid-transition.
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(1_000);
        s.present(2, false, 1_000);
        let f = s.frame(9_999);
        assert!((f.mix - 1.0).abs() < 1e-5);
        assert!(!f.animating);
    }

    #[test]
    fn presenting_the_same_image_twice_does_not_restart_the_crossfade() {
        // artd suppresses duplicate artwork, but a resent revision must be
        // inert here too rather than causing a visible re-fade.
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(5_000);
        s.present(1, false, 5_000);
        assert!(!s.needs_frame(5_000));
        assert_eq!(s.previous(), None);
    }

    #[test]
    fn the_first_image_has_nothing_to_fade_from() {
        let mut s = scene();
        s.present(7, false, 0);
        assert_eq!(s.previous(), None);
        assert!((s.frame(0).mix - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fading_to_black_reports_blanked_only_once_it_is_actually_black() {
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(1_000);

        s.set_power(DisplayPower::Off, 1_000);
        let mid = s.frame(1_500);
        assert!(mid.global_fade > 0.0 && mid.global_fade < 1.0);
        assert!(!mid.blanked, "blanked before the fade finished");
        assert!(mid.animating);

        let end = s.frame(2_000);
        assert_eq!(end.global_fade, 0.0);
        assert!(end.blanked, "never reported blanked");
        assert!(!s.needs_frame(2_000));
    }

    #[test]
    fn waking_from_black_fades_in_and_clears_blanked() {
        let mut s = scene();
        s.present(1, false, 0);
        s.set_power(DisplayPower::Off, 0);
        let _ = s.frame(5_000);
        assert!(s.frame(5_000).blanked);

        s.set_power(DisplayPower::On, 5_000);
        let f = s.frame(5_000);
        assert!(!f.blanked, "still blanked after wake");
        assert!(f.animating);
        let done = s.frame(6_000);
        assert!((done.global_fade - 1.0).abs() < 1e-5);
    }

    #[test]
    fn interrupting_a_fade_out_resumes_from_the_current_brightness() {
        // Waking mid-fade must not flash: the ramp starts from where the
        // screen actually is, not from 0.
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(1_000);
        s.set_power(DisplayPower::Off, 1_000);
        let mid = s.frame(1_500);
        let brightness = mid.global_fade;
        assert!(brightness > 0.2 && brightness < 0.8);

        s.set_power(DisplayPower::On, 1_500);
        let resumed = s.frame(1_500);
        assert!(
            (resumed.global_fade - brightness).abs() < 1e-4,
            "brightness jumped from {brightness} to {}",
            resumed.global_fade
        );
    }

    #[test]
    fn ken_burns_keeps_requesting_frames_but_only_while_lit() {
        let timings = Timings {
            ken_burns: true,
            ken_burns_period_ms: 1_000,
            ..Default::default()
        };
        let mut s = Scene::new(timings, 0);
        s.set_power(DisplayPower::On, 0);
        s.present(1, false, 0);
        let _ = s.frame(2_000);
        assert!(s.needs_frame(2_000), "ken burns stopped requesting frames");

        let a = s.frame(2_000).zoom;
        let b = s.frame(2_400).zoom;
        assert!((a - b).abs() > 1e-4, "zoom did not move");
        assert!((1.0..=1.05).contains(&a) && (1.0..=1.05).contains(&b));

        // Black screen: pointless work, so it must stop.
        s.set_power(DisplayPower::Off, 2_400);
        let _ = s.frame(5_000);
        assert!(
            !s.needs_frame(5_000),
            "ken burns ran against a black screen"
        );
    }

    #[test]
    fn ken_burns_reverses_rather_than_snapping_back() {
        let timings = Timings {
            ken_burns: true,
            ken_burns_period_ms: 1_000,
            ..Default::default()
        };
        let mut s = Scene::new(timings, 0);
        s.set_power(DisplayPower::On, 0);
        s.present(1, false, 0);
        let peak = s.frame(1_000).zoom;
        let after = s.frame(1_500).zoom;
        let back = s.frame(2_000).zoom;
        assert!(peak > after && after > back, "{peak} {after} {back}");
        assert!((back - 1.0).abs() < 1e-3, "did not return to 1.0: {back}");
    }

    #[test]
    fn clearing_artwork_fades_out_the_last_image() {
        let mut s = scene();
        s.present(1, false, 0);
        let _ = s.frame(1_000);
        s.clear(1_000);
        assert_eq!(s.current(), None);
        assert_eq!(s.previous(), Some(1));
        assert!(s.needs_frame(1_000));
    }

    #[test]
    fn next_frame_at_is_always_in_the_future_while_animating() {
        // A deadline in the past would make the caller's sleep zero-length
        // and spin, the same bug artd's timers had.
        let mut s = scene();
        s.present(1, false, 0);
        s.present(2, false, 100);
        for now in [100u64, 200, 400, 650, 5_000] {
            if let Some(next) = s.next_frame_at_ms(now) {
                assert!(next > now, "next_frame_at {next} <= now {now}");
            }
        }
    }
}
