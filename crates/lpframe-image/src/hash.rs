//! Difference hashing, for "is this the same picture?".
//!
//! dHash rather than aHash or pHash. aHash thresholds against the mean and
//! collapses on low-contrast covers; pHash needs a DCT and is sensitive to
//! the exact resampling that produced its input. dHash asks only whether each
//! pixel is lighter than its right-hand neighbour, which survives rescaling,
//! JPEG requantisation and a modest brightness or gamma difference between
//! two masters of the same cover — precisely the differences we expect
//! between a 500px AirPlay thumbnail and a 3000px catalogue original.

use crate::color::{linear_to_oklab, srgb_to_linear};
use crate::resize::resize_box;

/// 64-bit difference hash of RGBA8 pixel data.
///
/// The image is reduced to a 9×8 grid and each row yields eight
/// lighter-than-its-neighbour comparisons. Lightness is OKLab's L rather than
/// a weighted sRGB sum, so "lighter" means what a person would say it means:
/// saturated red against mid grey is the case where a luma approximation
/// picks the wrong one.
///
/// Bit 0 is the leftmost comparison of the top row. Degenerate input hashes
/// to zero, which the caller must treat as "no information" rather than as a
/// match — [`crate::histogram`] is the second gate for exactly that reason.
pub fn dhash(rgba: &[u8], width: u32, height: u32) -> u64 {
    let small = resize_box(rgba, width, height, 9, 8);
    if small.is_empty() {
        return 0;
    }

    let lightness: Vec<f32> = small
        .chunks_exact(4)
        .map(|p| {
            let r = srgb_to_linear(p[0] as f32 / 255.0);
            let g = srgb_to_linear(p[1] as f32 / 255.0);
            let b = srgb_to_linear(p[2] as f32 / 255.0);
            linear_to_oklab(r, g, b)[0]
        })
        .collect();

    let mut bits = 0u64;
    let mut n = 0;
    for row in 0..8 {
        for col in 0..8 {
            let i = row * 9 + col;
            if lightness[i] > lightness[i + 1] {
                bits |= 1 << n;
            }
            n += 1;
        }
    }
    bits
}

/// Number of differing bits between two hashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    ((x + y) * 127 / (w + h)) as u8,
                    255,
                ]
            })
            .collect()
    }

    /// Concentric bands, unrelated to `gradient` in both layout and palette.
    fn rings(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let dx = x as i32 - w as i32 / 2;
                let dy = y as i32 - h as i32 / 2;
                let d = ((dx * dx + dy * dy) as f32).sqrt() as u32;
                let v = if (d / 5) % 2 == 0 { 20u8 } else { 235 };
                [v, 255 - v, v / 2, 255]
            })
            .collect()
    }

    #[test]
    fn the_same_image_at_two_sizes_hashes_almost_identically() {
        // The property the perceptual gate depends on.
        let a = dhash(&gradient(500, 500), 500, 500);
        let b = dhash(&gradient(3000, 3000), 3000, 3000);
        assert!(hamming(a, b) <= 2, "distance {}", hamming(a, b));
    }

    #[test]
    fn unrelated_images_are_far_apart() {
        let a = dhash(&gradient(256, 256), 256, 256);
        let b = dhash(&rings(256, 256), 256, 256);
        assert!(
            hamming(a, b) > 12,
            "distance {} is inside the gate threshold",
            hamming(a, b)
        );
    }

    #[test]
    fn hashing_is_deterministic() {
        let px = gradient(128, 128);
        let first = dhash(&px, 128, 128);
        for _ in 0..5 {
            assert_eq!(dhash(&px, 128, 128), first);
        }
    }

    #[test]
    fn a_flat_image_hashes_to_zero_and_does_not_panic() {
        let flat: Vec<u8> = (0..64 * 64).flat_map(|_| [77u8, 77, 77, 255]).collect();
        assert_eq!(dhash(&flat, 64, 64), 0);
        assert_eq!(dhash(&[], 0, 0), 0);
        assert_eq!(dhash(&[1, 2, 3], 500, 500), 0);
    }

    #[test]
    fn hamming_counts_differing_bits() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1011, 0b0011), 1);
    }
}
