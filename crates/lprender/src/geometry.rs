//! Layout maths: where the artwork goes on whatever panel is attached.
//!
//! Pure and heavily tested, because this is where DESIGN §6.3.1's promise
//! lives — the square art area is a *policy*, and every aspect ratio,
//! orientation and connector has to look deliberate rather than broken.

use lpframe_config::{Background, Fit};

/// A rectangle in normalised device coordinates, `-1..1` on both axes with
/// +Y up, matching GL clip space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub cx: f32,
    pub cy: f32,
    pub half_w: f32,
    pub half_h: f32,
}

impl Rect {
    pub const FULL: Rect = Rect {
        cx: 0.0,
        cy: 0.0,
        half_w: 1.0,
        half_h: 1.0,
    };

    /// Width and height in pixels, given the panel it sits on.
    pub fn size_px(&self, panel_w: u32, panel_h: u32) -> (f32, f32) {
        (self.half_w * panel_w as f32, self.half_h * panel_h as f32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Where the artwork is drawn, in the *logical* frame (before rotation).
    pub art: Rect,
    /// Sub-rectangle of the source image to sample, as scale and offset
    /// applied to a 0..1 UV. Only ever crops (`cover`); `contain` shrinks the
    /// quad instead, so UVs stay inside the texture and no border sampling
    /// is needed.
    pub uv_scale: [f32; 2],
    pub uv_offset: [f32; 2],
    /// True when any panel area falls outside the artwork, i.e. the
    /// background will actually be seen.
    pub background_visible: bool,
    /// Rotation applied to the whole output, in degrees.
    pub rotation: u32,
}

impl Layout {
    /// The 2×2 rotation applied to clip-space positions, column-major.
    ///
    /// Rotation is done in the vertex shader rather than via the DRM plane
    /// `rotation` property: the property is not guaranteed present on every
    /// plane, and the shader path behaves identically on the dev backends.
    pub fn rotation_matrix(&self) -> [f32; 4] {
        match self.rotation % 360 {
            90 => [0.0, 1.0, -1.0, 0.0],
            180 => [-1.0, 0.0, 0.0, -1.0],
            270 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutConfig {
    /// Force a 1:1 art area. False fills the panel per `fit`.
    pub square: bool,
    pub fit: Fit,
    pub rotation: u32,
    /// Inset on all sides, as a percentage of the panel, for overscanning TVs.
    pub margin_percent: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        LayoutConfig {
            square: true,
            fit: Fit::Cover,
            rotation: 0,
            margin_percent: 0.0,
        }
    }
}

/// Compute the layout for `image` on `panel`.
///
/// `panel` is the physical framebuffer. For 90°/270° rotation the logical
/// frame is the transpose, and the caller's vertex shader rotates the result
/// back — so all the aspect reasoning below happens in the frame the viewer
/// actually sees.
pub fn layout(
    panel_w: u32,
    panel_h: u32,
    image_w: u32,
    image_h: u32,
    cfg: &LayoutConfig,
) -> Layout {
    let rotation = cfg.rotation % 360;
    let (lw, lh) = if matches!(rotation, 90 | 270) {
        (panel_h.max(1), panel_w.max(1))
    } else {
        (panel_w.max(1), panel_h.max(1))
    };
    let (lw, lh) = (lw as f32, lh as f32);

    let m = (cfg.margin_percent / 100.0).clamp(0.0, 0.45);
    let avail_w = lw * (1.0 - 2.0 * m);
    let avail_h = lh * (1.0 - 2.0 * m);

    // The art area in pixels, before fitting the image into it.
    let (area_w, area_h) = if cfg.square {
        let side = avail_w.min(avail_h);
        (side, side)
    } else {
        (avail_w, avail_h)
    };

    let iw = image_w.max(1) as f32;
    let ih = image_h.max(1) as f32;
    let image_aspect = iw / ih;
    let area_aspect = area_w / area_h;

    let (quad_w, quad_h, uv_scale) = match cfg.fit {
        // Crop to fill: the quad keeps the art area, the UVs take a
        // centred sub-rectangle of the image.
        Fit::Cover => {
            let uv = if image_aspect > area_aspect {
                [area_aspect / image_aspect, 1.0]
            } else {
                [1.0, image_aspect / area_aspect]
            };
            (area_w, area_h, uv)
        }
        // Fit entirely: shrink the quad to the image's aspect and sample the
        // whole texture.
        Fit::Contain => {
            let (w, h) = if image_aspect > area_aspect {
                (area_w, area_w / image_aspect)
            } else {
                (area_h * image_aspect, area_h)
            };
            (w, h, [1.0, 1.0])
        }
    };

    let art = Rect {
        cx: 0.0,
        cy: 0.0,
        half_w: quad_w / lw,
        half_h: quad_h / lh,
    };

    // A hair of tolerance: a 1:1 panel computed in floating point should not
    // be reported as showing a sliver of background.
    const EPS: f32 = 1e-4;
    let background_visible = art.half_w < 1.0 - EPS || art.half_h < 1.0 - EPS;

    Layout {
        art,
        uv_scale,
        uv_offset: [(1.0 - uv_scale[0]) * 0.5, (1.0 - uv_scale[1]) * 0.5],
        background_visible,
        rotation,
    }
}

/// Whether the background pass needs to draw anything more than black.
pub fn background_needs_image(bg: Background) -> bool {
    matches!(
        bg,
        Background::Blur | Background::Dominant | Background::Gradient
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(panel_w: u32, panel_h: u32) -> Layout {
        layout(panel_w, panel_h, 1000, 1000, &LayoutConfig::default())
    }

    /// Round to whole pixels, which is what actually matters visually.
    fn art_px(l: &Layout, panel_w: u32, panel_h: u32) -> (i64, i64) {
        let (w, h) = l.art.size_px(panel_w, panel_h);
        (w.round() as i64, h.round() as i64)
    }

    #[test]
    fn a_square_panel_is_full_bleed() {
        for size in [720u32, 1080, 1920] {
            let l = sq(size, size);
            assert_eq!(art_px(&l, size, size), (size as i64, size as i64));
            assert!(
                !l.background_visible,
                "{size}² reported background; it would never be seen"
            );
        }
    }

    #[test]
    fn the_compatibility_matrix_from_the_design_holds() {
        // DESIGN §6.3.1: square art centred, background filling the rest.
        let cases: &[(u32, u32, i64)] = &[
            (1920, 1080, 1080), // 16:9
            (3840, 2160, 2160), // 4K 16:9
            (1080, 1920, 1080), // portrait 9:16
            (1024, 768, 768),   // 4:3
            (3440, 1440, 1440), // 21:9
            (480, 480, 480),    // small square DSI
        ];
        for &(w, h, side) in cases {
            let l = sq(w, h);
            assert_eq!(art_px(&l, w, h), (side, side), "{w}x{h}");
            assert_eq!(l.background_visible, w != h, "{w}x{h}");
        }
    }

    #[test]
    fn rotation_swaps_the_axes_the_layout_is_computed_against() {
        // A landscape panel rotated 90° is a portrait frame to the viewer, so
        // the square must be sized against the *rotated* dimensions.
        let cfg = LayoutConfig {
            rotation: 90,
            ..Default::default()
        };
        let l = layout(1920, 1080, 1000, 1000, &cfg);
        // Logical frame is 1080x1920; the square is 1080 on a side, which in
        // logical NDC is full width and 1080/1920 of the height.
        assert!((l.art.half_w - 1.0).abs() < 1e-5);
        assert!((l.art.half_h - 1080.0 / 1920.0).abs() < 1e-5);

        // And the rotation matrix maps that back onto the physical panel.
        let m = l.rotation_matrix();
        assert_eq!(m, [0.0, 1.0, -1.0, 0.0]);
    }

    #[test]
    fn rotation_of_180_keeps_the_layout_but_flips_the_matrix() {
        let a = layout(1920, 1080, 1000, 1000, &LayoutConfig::default());
        let b = layout(
            1920,
            1080,
            1000,
            1000,
            &LayoutConfig {
                rotation: 180,
                ..Default::default()
            },
        );
        assert_eq!(a.art, b.art);
        assert_eq!(b.rotation_matrix(), [-1.0, 0.0, 0.0, -1.0]);
    }

    #[test]
    fn cover_crops_the_long_axis_of_a_non_square_image() {
        // A 2:1 image in a square area must be cropped horizontally to half.
        let l = layout(1000, 1000, 2000, 1000, &LayoutConfig::default());
        assert!((l.uv_scale[0] - 0.5).abs() < 1e-5, "{:?}", l.uv_scale);
        assert!((l.uv_scale[1] - 1.0).abs() < 1e-5);
        // Centred, so a quarter is trimmed from each side.
        assert!((l.uv_offset[0] - 0.25).abs() < 1e-5);
        assert!((l.uv_offset[1] - 0.0).abs() < 1e-5);
        // The quad still fills the whole square.
        assert_eq!(art_px(&l, 1000, 1000), (1000, 1000));
    }

    #[test]
    fn cover_crops_the_other_axis_for_a_tall_image() {
        let l = layout(1000, 1000, 1000, 2000, &LayoutConfig::default());
        assert!((l.uv_scale[0] - 1.0).abs() < 1e-5);
        assert!((l.uv_scale[1] - 0.5).abs() < 1e-5);
        assert!((l.uv_offset[1] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn contain_shrinks_the_quad_and_never_samples_outside_the_texture() {
        let cfg = LayoutConfig {
            fit: Fit::Contain,
            ..Default::default()
        };
        let l = layout(1000, 1000, 2000, 1000, &cfg);
        // Whole image visible: UVs untouched.
        assert_eq!(l.uv_scale, [1.0, 1.0]);
        assert_eq!(l.uv_offset, [0.0, 0.0]);
        // The quad is half as tall as it is wide.
        let (w, h) = l.art.size_px(1000, 1000);
        assert!((w - 1000.0).abs() < 0.5, "width {w}");
        assert!((h - 500.0).abs() < 0.5, "height {h}");
        assert!(l.background_visible);
    }

    #[test]
    fn a_square_image_is_identical_under_cover_and_contain() {
        let a = layout(1000, 1000, 800, 800, &LayoutConfig::default());
        let b = layout(
            1000,
            1000,
            800,
            800,
            &LayoutConfig {
                fit: Fit::Contain,
                ..Default::default()
            },
        );
        assert_eq!(a.art, b.art);
        assert_eq!(a.uv_scale, b.uv_scale);
    }

    #[test]
    fn non_square_mode_fills_the_panel() {
        let cfg = LayoutConfig {
            square: false,
            ..Default::default()
        };
        let l = layout(1920, 1080, 1000, 1000, &cfg);
        assert_eq!(art_px(&l, 1920, 1080), (1920, 1080));
        assert!(!l.background_visible);
        // A square image in a 16:9 area under cover crops top and bottom.
        assert!((l.uv_scale[0] - 1.0).abs() < 1e-5);
        assert!(l.uv_scale[1] < 1.0);
    }

    #[test]
    fn margin_insets_the_art_on_every_side() {
        let cfg = LayoutConfig {
            margin_percent: 10.0,
            ..Default::default()
        };
        let l = layout(1000, 1000, 1000, 1000, &cfg);
        // 10% off each side leaves 80%.
        assert_eq!(art_px(&l, 1000, 1000), (800, 800));
        assert!(l.background_visible, "margin must reveal the background");
    }

    #[test]
    fn an_absurd_margin_is_clamped_rather_than_inverting_the_rect() {
        let cfg = LayoutConfig {
            margin_percent: 400.0,
            ..Default::default()
        };
        let l = layout(1000, 1000, 1000, 1000, &cfg);
        let (w, h) = l.art.size_px(1000, 1000);
        assert!(w > 0.0 && h > 0.0, "art rect collapsed or inverted");
    }

    #[test]
    fn degenerate_inputs_do_not_produce_nan() {
        for (pw, ph, iw, ih) in [(0, 0, 0, 0), (1, 0, 0, 1), (0, 1, 1, 0), (1, 1, 1, 1)] {
            let l = layout(pw, ph, iw, ih, &LayoutConfig::default());
            assert!(l.art.half_w.is_finite(), "{pw}x{ph} {iw}x{ih}");
            assert!(l.art.half_h.is_finite());
            assert!(l.uv_scale.iter().all(|v| v.is_finite()));
            assert!(l.uv_offset.iter().all(|v| v.is_finite()));
        }
    }
}
