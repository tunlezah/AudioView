//! Dominant-colour similarity, the second half of the perceptual gate.
//!
//! **Why a histogram and not the k-means palette.** [`crate::color::palette`]
//! already extracts the two colours a viewer would name, and comparing those
//! with [`crate::color::oklab_distance`] was the obvious first choice. It is
//! the wrong tool here. k-means reduces a cover to two or three points, so
//! the comparison is a handful of distances and the accept/reject decision
//! turns on whether two clusterings happened to converge the same way — which
//! for a cover with three roughly equal colours they routinely do not, even
//! for two encodings of the identical image. The failure is bimodal: almost
//! exact, or wildly wrong, with nothing in between to set a threshold against.
//!
//! A coarse histogram degrades gracefully instead. Every pixel contributes,
//! there is no clustering step to be unstable, and the cosine of two bin
//! vectors moves smoothly as the images diverge — so 0.85 means something and
//! the number shown on the diagnostics page is interpretable.
//!
//! Two further properties matter for this specific job. Cosine similarity
//! ignores magnitude, so images of different sizes compare directly. And the
//! histogram discards spatial layout entirely, so a candidate that is the
//! same artwork with a different crop, or with a "Parental Advisory" sticker
//! in the corner, still matches — the difference hash is what covers layout,
//! and the two gates are deliberately sensitive to different things.

use crate::color::{linear_to_oklab, srgb_to_linear};

/// Bins per axis. Coarse on purpose: fine bins put JPEG requantisation noise
/// either side of a boundary and turn a match into a near-miss.
const BINS: usize = 4;

/// OKLab chroma range covered by the a and b axes. The sRGB gamut reaches
/// roughly ±0.3 on each; anything outside is clamped into the end bin.
const CHROMA: f32 = 0.35;

/// A 4×4×4 OKLab occupancy histogram, normalised to unit length.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Histogram([f32; BINS * BINS * BINS]);

impl Histogram {
    /// The raw bins, for diagnostics.
    pub fn bins(&self) -> &[f32] {
        &self.0
    }

    /// Whether any pixel contributed. An empty histogram matches nothing.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|v| *v == 0.0)
    }
}

/// Build a histogram from RGBA8 pixel data.
///
/// Contributions are spread trilinearly across the eight surrounding bins
/// rather than dropped into the nearest one. Hard binning makes the result
/// discontinuous — a cover whose dominant colour sits on a bin boundary
/// scores differently depending on which side a single re-encode pushed it —
/// and a gate that flips on a rounding difference is not a gate.
pub fn histogram(rgba: &[u8], width: u32, height: u32) -> Histogram {
    let mut bins = [0.0f32; BINS * BINS * BINS];
    let n = (width as usize).saturating_mul(height as usize);
    if n == 0 || rgba.len() < n * 4 {
        return Histogram(bins);
    }

    for p in rgba[..n * 4].chunks_exact(4) {
        // Fully transparent pixels carry no colour information; a cover with
        // an alpha border would otherwise be dominated by whatever the
        // encoder left in the RGB channels underneath it.
        if p[3] < 8 {
            continue;
        }
        let lab = linear_to_oklab(
            srgb_to_linear(p[0] as f32 / 255.0),
            srgb_to_linear(p[1] as f32 / 255.0),
            srgb_to_linear(p[2] as f32 / 255.0),
        );
        let coords = [
            axis(lab[0], 0.0, 1.0),
            axis(lab[1], -CHROMA, CHROMA),
            axis(lab[2], -CHROMA, CHROMA),
        ];
        let weight = p[3] as f32 / 255.0;

        for (li, lw) in split(coords[0]) {
            for (ai, aw) in split(coords[1]) {
                for (bi, bw) in split(coords[2]) {
                    let w = lw * aw * bw * weight;
                    if w > 0.0 {
                        bins[(li * BINS + ai) * BINS + bi] += w;
                    }
                }
            }
        }
    }

    let norm = bins.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut bins {
            *v /= norm;
        }
    }
    Histogram(bins)
}

/// Cosine similarity of two histograms, in `0..=1`.
///
/// Both vectors are non-negative and unit length, so this is just their dot
/// product; 1.0 is an identical colour distribution and 0.0 shares no bins.
pub fn histogram_similarity(a: &Histogram, b: &Histogram) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.0.iter()
        .zip(b.0.iter())
        .map(|(x, y)| x * y)
        .sum::<f32>()
        .clamp(0.0, 1.0)
}

/// Map a value onto a continuous bin coordinate in `0..=BINS-1`.
fn axis(v: f32, lo: f32, hi: f32) -> f32 {
    (((v - lo) / (hi - lo)) * (BINS - 1) as f32).clamp(0.0, (BINS - 1) as f32)
}

/// The two bins a coordinate falls between, with their weights.
fn split(c: f32) -> [(usize, f32); 2] {
    let low = c.floor();
    let frac = c - low;
    let i = low as usize;
    [(i, 1.0 - frac), ((i + 1).min(BINS - 1), frac)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect()
    }

    fn halves(w: u32, h: u32, top: [u8; 3], bottom: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let c = if i / w < h / 2 { top } else { bottom };
                [c[0], c[1], c[2], 255]
            })
            .collect()
    }

    #[test]
    fn an_image_is_identical_to_itself() {
        let px = halves(32, 32, [200, 30, 40], [20, 40, 190]);
        let h = histogram(&px, 32, 32);
        assert!((histogram_similarity(&h, &h) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn layout_does_not_matter_but_palette_does() {
        // The same two colours in the same proportion, rearranged: the
        // histogram must not care. That is the difference hash's job.
        let a = histogram(&halves(32, 32, [200, 30, 40], [20, 40, 190]), 32, 32);
        let b = histogram(&halves(32, 32, [20, 40, 190], [200, 30, 40]), 32, 32);
        assert!(histogram_similarity(&a, &b) > 0.99);

        let c = histogram(&halves(32, 32, [30, 200, 40], [190, 190, 20]), 32, 32);
        assert!(
            histogram_similarity(&a, &c) < 0.85,
            "unrelated palettes scored {}",
            histogram_similarity(&a, &c)
        );
    }

    #[test]
    fn the_same_image_at_two_sizes_scores_near_one() {
        let big = solid(600, 600, [140, 90, 200]);
        let small = solid(90, 90, [140, 90, 200]);
        let s = histogram_similarity(&histogram(&big, 600, 600), &histogram(&small, 90, 90));
        assert!(s > 0.999, "size changed the score: {s}");
    }

    #[test]
    fn a_small_colour_shift_stays_inside_the_gate() {
        // A different master of the same cover, or a different JPEG quality:
        // the score must degrade smoothly rather than fall off a cliff.
        let a = histogram(&halves(32, 32, [200, 30, 40], [20, 40, 190]), 32, 32);
        let b = histogram(&halves(32, 32, [208, 38, 34], [26, 34, 198]), 32, 32);
        let s = histogram_similarity(&a, &b);
        assert!(s > 0.85, "a mild recolour scored {s}");
    }

    #[test]
    fn black_and_white_are_distinguishable() {
        // Both are achromatic and only differ on the L axis, which is the
        // case a chroma-only histogram would get wrong.
        let black = histogram(&solid(16, 16, [0, 0, 0]), 16, 16);
        let white = histogram(&solid(16, 16, [255, 255, 255]), 16, 16);
        assert!(histogram_similarity(&black, &white) < 0.1);
    }

    #[test]
    fn degenerate_input_matches_nothing() {
        let empty = histogram(&[], 0, 0);
        assert!(empty.is_empty());
        assert_eq!(histogram_similarity(&empty, &empty), 0.0);
        assert!(histogram(&[1, 2, 3], 100, 100).is_empty());

        let real = histogram(&solid(8, 8, [10, 20, 30]), 8, 8);
        assert_eq!(histogram_similarity(&real, &empty), 0.0);
    }

    #[test]
    fn histograms_are_deterministic() {
        let px = halves(64, 64, [17, 200, 99], [240, 12, 200]);
        let first = histogram(&px, 64, 64);
        for _ in 0..5 {
            assert_eq!(histogram(&px, 64, 64), first);
        }
    }
}
