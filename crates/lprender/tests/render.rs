//! Rendering tests against real GLES 3.0.
//!
//! These run the actual shaders through surfaceless EGL — llvmpipe in CI,
//! v3d on the Pi — so a shader or layout regression is caught without a panel
//! attached (DESIGN §6.4).
//!
//! Two layers: pixel assertions that state a property in words ("the corners
//! are black"), and golden images that catch any change at all. The property
//! tests are the ones that say *why* something is wrong; the goldens say
//! *that* something changed.

use lpframe_config::{Background, Fit};
use lprender::backend::headless::Headless;
use lprender::color::{palette, Palette};
use lprender::geometry::{layout, LayoutConfig};
use lprender::gl::{DrawCall, Texture};
use lprender::scene::Frame;

/// Per-channel tolerance when comparing against a golden.
///
/// Not zero: llvmpipe and the Pi's v3d differ slightly in filtering and
/// rounding, and a golden suite that only passes on one rasteriser is worse
/// than useless — it would fail on the actual target hardware.
const GOLDEN_MEAN_TOLERANCE: f64 = 2.0;
const GOLDEN_MAX_TOLERANCE: u8 = 24;

struct Rgba {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Rgba {
    fn at(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 4) as usize;
        [self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]]
    }
    fn centre(&self) -> [u8; 4] {
        self.at(self.w / 2, self.h / 2)
    }
}

/// A synthetic cover: solid quadrants, so cropping and rotation are visible.
fn cover(size: u32, tint: [u8; 3]) -> (u32, u32, Vec<u8>) {
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let left = x < size / 2;
            let top = y < size / 2;
            let c = match (left, top) {
                (true, true) => tint,
                (false, true) => [tint[1], tint[2], tint[0]],
                (true, false) => [tint[2], tint[0], tint[1]],
                (false, false) => [255 - tint[0], 255 - tint[1], 255 - tint[2]],
            };
            px.extend([c[0], c[1], c[2], 255]);
        }
    }
    (size, size, px)
}

fn solid(size: u32, c: [u8; 3]) -> (u32, u32, Vec<u8>) {
    (
        size,
        size,
        (0..size * size)
            .flat_map(|_| [c[0], c[1], c[2], 255])
            .collect(),
    )
}

struct Harness {
    hl: Headless,
}

impl Harness {
    fn new(w: u32, h: u32) -> Option<Harness> {
        match Headless::new(w, h) {
            Ok(hl) => Some(Harness { hl }),
            Err(e) => {
                // Loud, not silent: a CI runner that quietly stopped testing
                // the shaders would be a hole we never noticed.
                eprintln!("SKIPPING GPU TEST: no EGL context available: {e:#}");
                None
            }
        }
    }

    fn upload(&mut self, img: &(u32, u32, Vec<u8>)) -> Texture {
        self.hl
            .renderer()
            .upload(img.0, img.1, &img.2)
            .expect("upload")
    }

    fn draw(&mut self, call: &DrawCall<'_>) -> Rgba {
        self.hl.bind();
        self.hl.renderer().draw(call);
        let (w, h) = self.hl.size();
        Rgba {
            w,
            h,
            px: self.hl.read(),
        }
    }
}

fn frame(mix: f32, fade: f32) -> Frame {
    Frame {
        mix,
        global_fade: fade,
        zoom: 1.0,
        animating: false,
        blanked: false,
    }
}

fn call<'a>(
    layout: lprender::geometry::Layout,
    frame: Frame,
    current: Option<&'a Texture>,
    previous: Option<&'a Texture>,
    background: Background,
) -> DrawCall<'a> {
    DrawCall {
        layout,
        frame,
        current,
        previous,
        background,
        background_dim: 1.0,
        palette: Palette::default(),
        ambient_dim: None,
    }
}

fn near(got: [u8; 4], want: [u8; 3], tol: i32, what: &str) {
    for i in 0..3 {
        assert!(
            (got[i] as i32 - want[i] as i32).abs() <= tol,
            "{what}: got {got:?}, wanted about {want:?}"
        );
    }
}

// --- property tests -------------------------------------------------------

#[test]
fn a_square_panel_is_filled_edge_to_edge() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let img = solid(32, [220, 40, 40]);
    let tex = h.upload(&img);
    let l = layout(64, 64, 32, 32, &LayoutConfig::default());
    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));

    for (x, y, what) in [
        (0, 0, "top-left"),
        (63, 0, "top-right"),
        (0, 63, "bottom-left"),
        (63, 63, "bottom-right"),
        (32, 32, "centre"),
    ] {
        near(out.at(x, y), [220, 40, 40], 3, what);
    }
}

#[test]
fn a_widescreen_panel_letterboxes_to_black() {
    let Some(mut h) = Harness::new(128, 72) else {
        return;
    };
    let img = solid(32, [40, 200, 90]);
    let tex = h.upload(&img);
    let l = layout(128, 72, 32, 32, &LayoutConfig::default());
    assert!(l.background_visible);

    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));
    near(out.centre(), [40, 200, 90], 3, "centre");
    // The square is 72 wide, centred: x < 28 is outside it.
    near(out.at(2, 36), [0, 0, 0], 2, "left bar");
    near(out.at(125, 36), [0, 0, 0], 2, "right bar");
}

#[test]
fn a_portrait_panel_pillarboxes_the_other_way() {
    let Some(mut h) = Harness::new(72, 128) else {
        return;
    };
    let img = solid(32, [90, 90, 230]);
    let tex = h.upload(&img);
    let l = layout(72, 128, 32, 32, &LayoutConfig::default());
    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));
    near(out.centre(), [90, 90, 230], 3, "centre");
    near(out.at(36, 2), [0, 0, 0], 2, "top bar");
    near(out.at(36, 125), [0, 0, 0], 2, "bottom bar");
}

#[test]
fn the_image_is_not_flipped_or_mirrored() {
    // GL puts texture v=0 at the bottom while an uploaded image's first row
    // is its top. Every earlier pixel test used solid or symmetric images and
    // so could not see that every cover was rendering vertically mirrored.
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    // Four distinguishable quadrants, in source order.
    const TL: [u8; 3] = [255, 0, 0];
    const TR: [u8; 3] = [0, 255, 0];
    const BL: [u8; 3] = [0, 0, 255];
    const BR: [u8; 3] = [255, 255, 0];
    let size = 32u32;
    let mut px = Vec::new();
    for y in 0..size {
        for x in 0..size {
            let c = match (x < size / 2, y < size / 2) {
                (true, true) => TL,
                (false, true) => TR,
                (true, false) => BL,
                (false, false) => BR,
            };
            px.extend([c[0], c[1], c[2], 255]);
        }
    }
    let tex = h.upload(&(size, size, px));
    let l = layout(64, 64, size, size, &LayoutConfig::default());
    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));

    near(out.at(16, 16), TL, 4, "top-left quadrant");
    near(out.at(48, 16), TR, 4, "top-right quadrant");
    near(out.at(16, 48), BL, 4, "bottom-left quadrant");
    near(out.at(48, 48), BR, 4, "bottom-right quadrant");
}

#[test]
fn the_global_fade_darkens_everything_toward_black() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let img = solid(32, [200, 200, 200]);
    let tex = h.upload(&img);
    let l = layout(64, 64, 32, 32, &LayoutConfig::default());

    let full = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));
    let half = h.draw(&call(
        l,
        frame(1.0, 0.5),
        Some(&tex),
        None,
        Background::Black,
    ));
    let none = h.draw(&call(
        l,
        frame(1.0, 0.0),
        Some(&tex),
        None,
        Background::Black,
    ));

    near(full.centre(), [200, 200, 200], 3, "full");
    near(half.centre(), [100, 100, 100], 4, "half");
    near(none.centre(), [0, 0, 0], 1, "black");
}

#[test]
fn a_crossfade_interpolates_between_the_two_images() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let a = solid(32, [255, 0, 0]);
    let b = solid(32, [0, 0, 255]);
    let ta = h.upload(&a);
    let tb = h.upload(&b);
    let l = layout(64, 64, 32, 32, &LayoutConfig::default());

    let start = h.draw(&call(
        l,
        frame(0.0, 1.0),
        Some(&tb),
        Some(&ta),
        Background::Black,
    ));
    near(
        start.centre(),
        [255, 0, 0],
        3,
        "mix=0 should be the previous image",
    );

    let mid = h.draw(&call(
        l,
        frame(0.5, 1.0),
        Some(&tb),
        Some(&ta),
        Background::Black,
    ));
    near(mid.centre(), [128, 0, 128], 4, "mix=0.5");

    let end = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tb),
        Some(&ta),
        Background::Black,
    ));
    near(
        end.centre(),
        [0, 0, 255],
        3,
        "mix=1 should be the current image",
    );
}

#[test]
fn a_crossfade_with_no_previous_image_shows_the_current_one() {
    // Guards the uHasPrev path: sampling an unbound unit would give noise.
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let img = solid(32, [10, 240, 10]);
    let tex = h.upload(&img);
    let l = layout(64, 64, 32, 32, &LayoutConfig::default());
    for mix in [0.0, 0.5, 1.0] {
        let out = h.draw(&call(
            l,
            frame(mix, 1.0),
            Some(&tex),
            None,
            Background::Black,
        ));
        near(out.centre(), [10, 240, 10], 3, &format!("mix={mix}"));
    }
}

#[test]
fn rotation_moves_the_image_onto_the_rotated_axes() {
    let Some(mut h) = Harness::new(128, 72) else {
        return;
    };
    let img = solid(32, [255, 128, 0]);
    let tex = h.upload(&img);

    // Rotated 90°, the logical frame is portrait 72x128, so the square is 72
    // on a side — the same size, but now the bars are top and bottom of the
    // logical frame, which maps to left and right physically.
    let l = layout(
        128,
        72,
        32,
        32,
        &LayoutConfig {
            rotation: 90,
            ..Default::default()
        },
    );
    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));
    near(out.centre(), [255, 128, 0], 3, "centre");
    near(out.at(2, 36), [0, 0, 0], 2, "left bar after rotation");
}

#[test]
fn cover_crops_a_wide_image_rather_than_squashing_it() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    // A 2:1 image: left half red, right half blue. Cover into a square keeps
    // the middle, so both halves stay visible and the vertical seam is at
    // the centre.
    let (w, hgt) = (64u32, 32u32);
    let mut px = Vec::new();
    for _ in 0..hgt {
        for x in 0..w {
            if x < w / 2 {
                px.extend([255u8, 0, 0, 255]);
            } else {
                px.extend([0u8, 0, 255, 255]);
            }
        }
    }
    let tex = h.upload(&(w, hgt, px));
    let l = layout(64, 64, w, hgt, &LayoutConfig::default());
    assert!((l.uv_scale[0] - 0.5).abs() < 1e-5);

    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));
    near(out.at(16, 32), [255, 0, 0], 4, "left of the seam");
    near(out.at(48, 32), [0, 0, 255], 4, "right of the seam");
    // Cropped to fill, so the corners are image, not background.
    near(out.at(1, 1), [255, 0, 0], 4, "top-left is image");
}

#[test]
fn contain_shows_the_whole_image_over_the_background() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let (w, hgt) = (64u32, 32u32);
    let px: Vec<u8> = (0..w * hgt).flat_map(|_| [200u8, 200, 0, 255]).collect();
    let tex = h.upload(&(w, hgt, px));
    let l = layout(
        64,
        64,
        w,
        hgt,
        &LayoutConfig {
            fit: Fit::Contain,
            ..Default::default()
        },
    );
    let out = h.draw(&call(
        l,
        frame(1.0, 1.0),
        Some(&tex),
        None,
        Background::Black,
    ));
    near(out.centre(), [200, 200, 0], 3, "centre");
    // Half as tall, so the top and bottom quarters are background.
    near(out.at(32, 2), [0, 0, 0], 2, "top band");
    near(out.at(32, 61), [0, 0, 0], 2, "bottom band");
}

#[test]
fn the_dominant_background_picks_up_the_artwork_colour() {
    let Some(mut h) = Harness::new(128, 72) else {
        return;
    };
    let img = solid(32, [180, 60, 20]);
    let tex = h.upload(&img);
    let l = layout(128, 72, 32, 32, &LayoutConfig::default());
    let mut c = call(l, frame(1.0, 1.0), Some(&tex), None, Background::Dominant);
    c.palette = palette(&img.2, img.0, img.1);
    c.background_dim = 1.0;
    let out = h.draw(&c);

    // The bars carry the cover's colour rather than black.
    near(out.at(2, 36), [180, 60, 20], 6, "left bar");
    near(out.centre(), [180, 60, 20], 3, "centre");
}

#[test]
fn the_blurred_background_is_neither_black_nor_the_sharp_image() {
    let Some(mut h) = Harness::new(128, 72) else {
        return;
    };
    let img = cover(64, [230, 40, 40]);
    let tex = h.upload(&img);
    let l = layout(128, 72, 64, 64, &LayoutConfig::default());
    let mut c = call(l, frame(1.0, 1.0), Some(&tex), None, Background::Blur);
    c.background_dim = 0.9;
    let out = h.draw(&c);

    let bar = out.at(3, 36);
    let luminance = bar[0] as u32 + bar[1] as u32 + bar[2] as u32;
    assert!(luminance > 30, "blurred background came out black: {bar:?}");

    // Blur smooths the quadrant seam: two nearby bar pixels either side of
    // the image's horizontal split should be close together.
    let above = out.at(3, 30);
    let below = out.at(3, 42);
    let delta: i32 = (0..3)
        .map(|i| (above[i] as i32 - below[i] as i32).abs())
        .sum();
    assert!(
        delta < 240,
        "background does not look blurred: {above:?} vs {below:?}"
    );
}

#[test]
fn ambient_dimming_darkens_without_going_black() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let img = solid(32, [200, 200, 200]);
    let tex = h.upload(&img);
    let l = layout(64, 64, 32, 32, &LayoutConfig::default());
    let mut c = call(l, frame(1.0, 1.0), Some(&tex), None, Background::Black);
    c.ambient_dim = Some(0.12);
    let out = h.draw(&c);
    near(out.centre(), [24, 24, 24], 4, "ambient");
}

#[test]
fn drawing_with_no_artwork_produces_a_black_frame() {
    let Some(mut h) = Harness::new(64, 64) else {
        return;
    };
    let l = layout(64, 64, 1, 1, &LayoutConfig::default());
    let out = h.draw(&call(l, frame(1.0, 1.0), None, None, Background::Black));
    near(out.centre(), [0, 0, 0], 1, "no artwork");
    near(out.at(0, 0), [0, 0, 0], 1, "no artwork corner");
}

#[test]
fn an_upload_with_the_wrong_buffer_size_is_rejected() {
    let Some(mut h) = Harness::new(16, 16) else {
        return;
    };
    let err = h.hl.renderer().upload(8, 8, &[0u8; 10]).unwrap_err();
    assert!(err.to_string().contains("expected"), "{err}");
}

// --- golden images --------------------------------------------------------

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn compare_golden(name: &str, img: &Rgba) -> Result<(), String> {
    let path = golden_dir().join(format!("{name}.png"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        image::RgbaImage::from_raw(img.w, img.h, img.px.clone())
            .unwrap()
            .save(&path)
            .unwrap();
        return Ok(());
    }

    let want = image::open(&path)
        .map_err(|e| format!("{}: {e}; rerun with UPDATE_GOLDEN=1", path.display()))?
        .to_rgba8();
    if want.dimensions() != (img.w, img.h) {
        return Err(format!(
            "{name}: size {:?} != golden {:?}",
            (img.w, img.h),
            want.dimensions()
        ));
    }

    let mut total = 0f64;
    let mut worst = 0u8;
    for (a, b) in img.px.chunks(4).zip(want.as_raw().chunks(4)) {
        for i in 0..3 {
            let d = a[i].abs_diff(b[i]);
            total += d as f64;
            worst = worst.max(d);
        }
    }
    let mean = total / (img.px.len() as f64 / 4.0 * 3.0);
    if mean > GOLDEN_MEAN_TOLERANCE || worst > GOLDEN_MAX_TOLERANCE {
        return Err(format!(
            "{name}: mean diff {mean:.2} (limit {GOLDEN_MEAN_TOLERANCE}), \
             worst {worst} (limit {GOLDEN_MAX_TOLERANCE})"
        ));
    }
    Ok(())
}

#[test]
fn golden_images_across_the_panel_matrix() {
    // One scenario per row of the DESIGN §6.3.1 compatibility matrix, plus
    // the transitions. A layout regression on 16:9 is caught here without
    // owning a 16:9 panel.
    let scenarios: &[(&str, u32, u32, LayoutConfig, Background, f32, f32)] = &[
        (
            "square-1to1",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "wide-16to9-black",
            128,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "wide-16to9-blur",
            128,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Blur,
            1.0,
            1.0,
        ),
        (
            "wide-16to9-gradient",
            128,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Gradient,
            1.0,
            1.0,
        ),
        (
            "portrait-9to16",
            72,
            128,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Blur,
            1.0,
            1.0,
        ),
        (
            "ultrawide-21to9",
            168,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Dominant,
            1.0,
            1.0,
        ),
        (
            "four-to-three",
            96,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "rotated-90",
            128,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 90,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "rotated-180",
            128,
            72,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 180,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "margin-10pc",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 10.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "contain",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Contain,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "fill-panel",
            128,
            72,
            LayoutConfig {
                square: false,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            1.0,
            1.0,
        ),
        (
            "crossfade-25",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            0.25,
            1.0,
        ),
        (
            "crossfade-50",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            0.5,
            1.0,
        ),
        (
            "crossfade-75",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Black,
            0.75,
            1.0,
        ),
        (
            "faded-40pc",
            96,
            96,
            LayoutConfig {
                square: true,
                fit: Fit::Cover,
                rotation: 0,
                margin_percent: 0.0,
            },
            Background::Blur,
            1.0,
            0.4,
        ),
    ];

    let mut failures = Vec::new();
    let mut ran = 0;
    for (name, pw, ph, cfg, bg, mix, fade) in scenarios {
        let Some(mut h) = Harness::new(*pw, *ph) else {
            return;
        };
        ran += 1;
        let a = cover(48, [40, 90, 200]);
        let b = cover(48, [220, 120, 20]);
        let ta = h.upload(&a);
        let tb = h.upload(&b);
        let l = layout(*pw, *ph, a.0, a.1, cfg);
        let mut c = call(l, frame(*mix, *fade), Some(&tb), Some(&ta), *bg);
        c.palette = palette(&b.2, b.0, b.1);
        c.background_dim = 0.6;
        let out = h.draw(&c);
        if let Err(e) = compare_golden(name, &out) {
            failures.push(e);
        }
    }

    assert!(ran > 0, "no scenarios ran");
    assert!(
        failures.is_empty(),
        "golden mismatches ({} of {ran}):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
