//! Deterministic box downscaling.
//!
//! Written out here rather than taken from an image library because the
//! enrichment gate's thresholds are calibrated against *this* filter. A
//! library's "high quality" resampler is free to change between releases, and
//! a silent change to the filter would move every dHash and every histogram
//! at once — a whole class of covers would start being accepted or rejected
//! with nothing in the changelog to explain it.
//!
//! A box filter is the right choice for the gate specifically: it is an
//! unweighted average of the source pixels covering each destination pixel,
//! so it is exactly "what colour is this region", which is the question both
//! the hash and the histogram are asking.

/// Downscale RGBA8 to `out_w` × `out_h` by averaging source pixels.
///
/// Averaging happens in linear light, not on gamma-encoded bytes: averaging
/// sRGB values darkens edges, and on a cover with fine white text on black
/// that is enough to shift the difference hash.
///
/// Returns an empty vector for degenerate input rather than panicking —
/// enrichment runs against bytes fetched from the internet, and a truncated
/// or lying image header must not be able to take the daemon down.
pub fn resize_box(rgba: &[u8], width: u32, height: u32, out_w: u32, out_h: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let (ow, oh) = (out_w as usize, out_h as usize);
    if w == 0 || h == 0 || ow == 0 || oh == 0 || rgba.len() < w * h * 4 {
        return Vec::new();
    }

    let mut out = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        // Half-open source spans, at least one pixel wide even when
        // upscaling, so every destination pixel has something to average.
        let y0 = oy * h / oh;
        let y1 = (((oy + 1) * h).div_ceil(oh)).max(y0 + 1).min(h);
        for ox in 0..ow {
            let x0 = ox * w / ow;
            let x1 = (((ox + 1) * w).div_ceil(ow)).max(x0 + 1).min(w);

            let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            let mut n = 0.0f32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    r += crate::color::srgb_to_linear(rgba[i] as f32 / 255.0);
                    g += crate::color::srgb_to_linear(rgba[i + 1] as f32 / 255.0);
                    b += crate::color::srgb_to_linear(rgba[i + 2] as f32 / 255.0);
                    a += rgba[i + 3] as f32 / 255.0;
                    n += 1.0;
                }
            }

            let o = (oy * ow + ox) * 4;
            out[o] = encode(r / n);
            out[o + 1] = encode(g / n);
            out[o + 2] = encode(b / n);
            out[o + 3] = (a / n * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn encode(linear: f32) -> u8 {
    (crate::color::linear_to_srgb(linear) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_solid_image_survives_downscaling_unchanged() {
        let px: Vec<u8> = (0..64 * 64).flat_map(|_| [180u8, 40, 90, 255]).collect();
        let out = resize_box(&px, 64, 64, 32, 32);
        assert_eq!(out.len(), 32 * 32 * 4);
        for chunk in out.chunks(4) {
            assert_eq!(chunk[0..3], [180, 40, 90], "solid colour drifted");
        }
    }

    #[test]
    fn averaging_happens_in_linear_light() {
        // Half black, half white. The sRGB midpoint of 0 and 255 is 128; the
        // perceptually correct answer — the average of the *light* — is 188.
        let px: Vec<u8> = (0..2)
            .flat_map(|i| if i == 0 { [0, 0, 0, 255] } else { [255; 4] })
            .collect();
        let out = resize_box(&px, 2, 1, 1, 1);
        assert!(
            (out[0] as i32 - 188).abs() <= 1,
            "expected a linear-light average, got {}",
            out[0]
        );
    }

    #[test]
    fn resizing_is_deterministic_and_size_invariant() {
        // The gate compares a 500px AirPlay image against a 3000px candidate,
        // so the same picture at two sizes must reduce to nearly the same
        // 32x32 — this is the property the whole perceptual gate rests on.
        let big = gradient(600, 600);
        let small = gradient(150, 150);
        let a = resize_box(&big, 600, 600, 32, 32);
        let b = resize_box(&small, 150, 150, 32, 32);
        assert_eq!(a, resize_box(&big, 600, 600, 32, 32), "not deterministic");

        let worst = a
            .chunks(4)
            .zip(b.chunks(4))
            .flat_map(|(p, q)| (0..3).map(move |i| (p[i] as i32 - q[i] as i32).abs()))
            .max()
            .unwrap();
        assert!(worst <= 4, "same image at two sizes diverged by {worst}");
    }

    #[test]
    fn degenerate_input_yields_nothing_rather_than_panicking() {
        assert!(resize_box(&[], 0, 0, 32, 32).is_empty());
        assert!(resize_box(&[1, 2, 3], 100, 100, 32, 32).is_empty());
        assert!(resize_box(&[1, 2, 3, 4], 1, 1, 0, 0).is_empty());
        // Upscaling is not what this is for, but it must still be safe.
        assert_eq!(resize_box(&[1, 2, 3, 4], 1, 1, 4, 4).len(), 64);
    }

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    (x * 255 / w.max(1)) as u8,
                    (y * 255 / h.max(1)) as u8,
                    128,
                    255,
                ]
            })
            .collect()
    }
}
