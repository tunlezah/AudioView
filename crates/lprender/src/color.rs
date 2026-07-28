//! Dominant-colour extraction, for the `dominant` and `gradient` background
//! modes.
//!
//! Clustering happens in OKLab rather than sRGB. Euclidean distance in sRGB
//! does not correspond to perceived difference, so a naive average of a
//! two-tone cover tends to produce mud; OKLab is close enough to perceptually
//! uniform that a cheap k-means gives colours a person would actually name.
//!
//! The same conversion is reused at milestone 5 for the enrichment
//! perceptual gate, which is why it lives here rather than inside the
//! renderer.

/// Two colours describing an image.
///
/// Display-encoded sRGB in `0..1`, **not** linear: the shader writes straight
/// into an 8-bit framebuffer with no `GL_FRAMEBUFFER_SRGB`, so a linear value
/// handed over here comes out visibly too dark.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub primary: [f32; 3],
    pub secondary: [f32; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            primary: [0.0; 3],
            secondary: [0.0; 3],
        }
    }
}

/// sRGB (0..1, gamma encoded) to linear.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear to sRGB.
pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Linear RGB to OKLab.
///
/// Coefficients are Björn Ottosson's published matrices, kept at full
/// precision so they read as the reference values rather than as numbers
/// someone rounded by hand.
#[allow(clippy::excessive_precision)]
pub fn linear_to_oklab(r: f32, g: f32, b: f32) -> [f32; 3] {
    let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;
    let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

/// OKLab back to linear RGB.
#[allow(clippy::excessive_precision)]
pub fn oklab_to_linear(lab: [f32; 3]) -> [f32; 3] {
    let l_ = lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2];
    let m_ = lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2];
    let s_ = lab[0] - 0.0894841775 * lab[1] - 1.2914855480 * lab[2];
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s).clamp(0.0, 1.0),
        (-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s).clamp(0.0, 1.0),
        (-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s).clamp(0.0, 1.0),
    ]
}

/// Perceptual distance between two OKLab colours.
pub fn oklab_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// Extract a two-colour palette from RGBA8 pixel data.
///
/// Deterministic: seeding is by even spacing through the sample list rather
/// than randomly, so the same artwork always yields the same background and
/// golden images stay stable.
pub fn palette(rgba: &[u8], width: u32, height: u32) -> Palette {
    let samples = sample_oklab(rgba, width, height, 32);
    if samples.is_empty() {
        return Palette::default();
    }

    let clusters = kmeans(&samples, 3, 12);
    // Sort by population: the biggest cluster is the one a viewer would call
    // "the colour of the cover".
    let mut ranked: Vec<_> = clusters.into_iter().filter(|c| c.count > 0).collect();
    ranked.sort_by(|a, b| b.count.cmp(&a.count));

    let encode = |lab: [f32; 3]| {
        let lin = oklab_to_linear(lab);
        [
            linear_to_srgb(lin[0]),
            linear_to_srgb(lin[1]),
            linear_to_srgb(lin[2]),
        ]
    };
    let primary = ranked.first().map(|c| encode(c.centre)).unwrap_or([0.0; 3]);
    let secondary = ranked.get(1).map(|c| encode(c.centre)).unwrap_or(primary);

    Palette { primary, secondary }
}

/// Downsample to at most `grid`×`grid` samples, converted to OKLab.
fn sample_oklab(rgba: &[u8], width: u32, height: u32, grid: u32) -> Vec<[f32; 3]> {
    if width == 0 || height == 0 || rgba.len() < (width as usize * height as usize * 4) {
        return Vec::new();
    }
    let step_x = (width / grid).max(1);
    let step_y = (height / grid).max(1);
    let mut out = Vec::with_capacity((grid * grid) as usize);
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let i = ((y as usize * width as usize) + x as usize) * 4;
            let r = srgb_to_linear(rgba[i] as f32 / 255.0);
            let g = srgb_to_linear(rgba[i + 1] as f32 / 255.0);
            let b = srgb_to_linear(rgba[i + 2] as f32 / 255.0);
            out.push(linear_to_oklab(r, g, b));
            x += step_x;
        }
        y += step_y;
    }
    out
}

struct Cluster {
    centre: [f32; 3],
    count: usize,
}

fn kmeans(samples: &[[f32; 3]], k: usize, iterations: usize) -> Vec<Cluster> {
    let k = k.min(samples.len()).max(1);
    let mut centres: Vec<[f32; 3]> = (0..k).map(|i| samples[i * samples.len() / k]).collect();

    let mut counts = vec![0usize; k];
    for _ in 0..iterations {
        let mut sums = vec![[0.0f32; 3]; k];
        counts.iter_mut().for_each(|c| *c = 0);

        for s in samples {
            let mut best = 0;
            let mut best_d = f32::MAX;
            for (i, c) in centres.iter().enumerate() {
                let d = oklab_distance(*s, *c);
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            for axis in 0..3 {
                sums[best][axis] += s[axis];
            }
            counts[best] += 1;
        }

        for i in 0..k {
            if counts[i] > 0 {
                for axis in 0..3 {
                    centres[i][axis] = sums[i][axis] / counts[i] as f32;
                }
            }
        }
    }

    centres
        .into_iter()
        .zip(counts)
        .map(|(centre, count)| Cluster { centre, count })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect()
    }

    /// The palette is already display-encoded, so this is just a scale.
    fn to_srgb8(c: [f32; 3]) -> [u8; 3] {
        [
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        ]
    }

    #[test]
    fn srgb_round_trips() {
        for v in [0u32, 1, 55, 128, 200, 255] {
            let f = v as f32 / 255.0;
            let back = linear_to_srgb(srgb_to_linear(f));
            assert!((back - f).abs() < 1e-4, "{v} -> {back}");
        }
    }

    #[test]
    fn oklab_round_trips() {
        for rgb in [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.2, 0.7, 0.4],
            [0.9, 0.1, 0.05],
        ] {
            let lab = linear_to_oklab(rgb[0], rgb[1], rgb[2]);
            let back = oklab_to_linear(lab);
            for i in 0..3 {
                assert!((back[i] - rgb[i]).abs() < 1e-3, "{rgb:?} -> {back:?}");
            }
        }
    }

    #[test]
    fn white_and_black_sit_at_the_ends_of_the_lightness_axis() {
        assert!(linear_to_oklab(0.0, 0.0, 0.0)[0].abs() < 1e-4);
        assert!((linear_to_oklab(1.0, 1.0, 1.0)[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_solid_image_yields_that_colour() {
        let px = solid(64, 64, [200, 40, 60]);
        let p = palette(&px, 64, 64);
        let got = to_srgb8(p.primary);
        for i in 0..3 {
            assert!((got[i] as i32 - [200, 40, 60][i]).abs() <= 2, "got {got:?}");
        }
        // With only one colour present, both entries agree.
        assert_eq!(to_srgb8(p.secondary), got);
    }

    #[test]
    fn a_two_tone_image_yields_both_colours_with_the_larger_first() {
        // Three quarters red, one quarter blue.
        let (w, h) = (64u32, 64u32);
        let mut px = Vec::new();
        for y in 0..h {
            for _ in 0..w {
                if y < h * 3 / 4 {
                    px.extend([220, 30, 30, 255]);
                } else {
                    px.extend([30, 30, 220, 255]);
                }
            }
        }
        let p = palette(&px, w, h);
        let a = to_srgb8(p.primary);
        let b = to_srgb8(p.secondary);
        assert!(a[0] > 150 && a[2] < 80, "primary should be red: {a:?}");
        assert!(b[2] > 150 && b[0] < 80, "secondary should be blue: {b:?}");
    }

    #[test]
    fn extraction_is_deterministic() {
        // Golden images depend on this: the same cover must always produce
        // the same background.
        let (w, h) = (48u32, 48u32);
        let px: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let v = (i % 251) as u8;
                [v, v.wrapping_mul(3), 255 - v, 255]
            })
            .collect();
        let first = palette(&px, w, h);
        for _ in 0..5 {
            assert_eq!(palette(&px, w, h), first);
        }
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        assert_eq!(palette(&[], 0, 0), Palette::default());
        assert_eq!(palette(&[1, 2, 3], 100, 100), Palette::default());
        let one = palette(&[10, 20, 30, 255], 1, 1);
        assert!(one.primary.iter().all(|v| v.is_finite()));
    }
}
