//! The size and perceptual gates (DESIGN §5.4 ④ and ⑤).
//!
//! The text gate picks a candidate; these two decide whether it is worth
//! swapping and whether it is actually the same picture. Both produce a
//! score, and every score reaches the diagnostics page — a rejection nobody
//! can explain is a bug report we cannot answer.

use anyhow::{bail, Context, Result};
use lpframe_image::{dhash, hamming, histogram, histogram_similarity, resize_box, Histogram};

/// Working resolution for the comparison (DESIGN §5.4 ⑤).
const GRID: u32 = 32;

/// Largest dHash distance still considered the same picture, out of 64.
pub const HAMMING_MAX: u32 = 12;

/// Smallest dominant-colour similarity still considered the same picture.
pub const SIMILARITY_MIN: f32 = 0.85;

/// How much bigger a candidate must be before a swap is worth the crossfade.
pub const SIZE_RATIO: f64 = 1.5;

/// Ceiling on decoded dimensions.
///
/// These bytes came off the internet. `max_dimension` is 3000 and Apple's
/// documented ceiling is the same, so anything past 8192 on a side is either
/// a mistake or an attempt to make us allocate a gigabyte.
const MAX_SIDE: u32 = 8192;

/// What the gate needs to know about an image, without keeping the pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fingerprint {
    pub width: u32,
    pub height: u32,
    pub dhash: u64,
    histogram: Histogram,
}

impl Fingerprint {
    /// The smaller side. Cover art is nominally square but not always, and an
    /// upgrade that is only wider is not sharper.
    pub fn min_side(&self) -> u32 {
        self.width.min(self.height)
    }
}

/// Decode an image and reduce it to a fingerprint.
///
/// The reduction to 32×32 happens once, here, and both halves of the
/// perceptual gate are computed from that same reduction — so the two scores
/// can never disagree about which pixels they were looking at.
pub fn fingerprint(bytes: &[u8]) -> Result<Fingerprint> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .context("sniffing image format")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    reader.limits(limits);

    let decoded = reader.decode().context("decoding image")?;
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        bail!("image has a zero dimension");
    }

    let small = resize_box(rgba.as_raw(), width, height, GRID, GRID);
    Ok(Fingerprint {
        width,
        height,
        dhash: dhash(&small, GRID, GRID),
        histogram: histogram(&small, GRID, GRID),
    })
}

/// The result of comparing two fingerprints, in the terms the design uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Visual {
    pub hamming: u32,
    pub similarity: f32,
}

impl Visual {
    pub fn passes(&self) -> bool {
        self.hamming <= HAMMING_MAX && self.similarity >= SIMILARITY_MIN
    }
}

impl std::fmt::Display for Visual {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hamming {}/64 similarity {:.2}",
            self.hamming, self.similarity
        )
    }
}

pub fn compare(current: &Fingerprint, candidate: &Fingerprint) -> Visual {
    Visual {
        hamming: hamming(current.dhash, candidate.dhash),
        similarity: histogram_similarity(&current.histogram, &candidate.histogram),
    }
}

/// Whether a candidate is enough sharper to justify swapping (DESIGN §5.4 ④).
pub fn size_gate(current_min_side: u32, candidate_min_side: u32) -> bool {
    f64::from(candidate_min_side) >= f64::from(current_min_side) * SIZE_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic cover: coloured quadrants with a diagonal band, rendered at
    /// whatever size is asked for. Scaling it is the "same album, bigger file"
    /// case the whole gate exists to recognise.
    fn cover(size: u32, hue_shift: i16) -> Vec<u8> {
        let mut px = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let (fx, fy) = (x as f32 / size as f32, y as f32 / size as f32);
                let base: [i16; 3] = if (fx + fy) > 0.9 && (fx + fy) < 1.1 {
                    [240, 230, 60]
                } else if fx < 0.5 && fy < 0.5 {
                    [200, 40, 40]
                } else if fx >= 0.5 && fy < 0.5 {
                    [30, 60, 180]
                } else if fx < 0.5 {
                    [20, 20, 20]
                } else {
                    [230, 230, 220]
                };
                px.extend([
                    (base[0] + hue_shift).clamp(0, 255) as u8,
                    (base[1] + hue_shift / 2).clamp(0, 255) as u8,
                    (base[2] - hue_shift).clamp(0, 255) as u8,
                    255,
                ]);
            }
        }
        px
    }

    /// A different record entirely: horizontal bands, cool palette.
    fn other_cover(size: u32) -> Vec<u8> {
        (0..size * size)
            .flat_map(|i| {
                let y = i / size;
                let v = if (y * 8 / size) % 2 == 0 { 40u8 } else { 110 };
                [v / 2, v, (255 - v).min(200), 255]
            })
            .collect()
    }

    fn png(px: &[u8], size: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_raw(size, size, px.to_vec()).unwrap();
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn the_same_cover_at_two_sizes_passes_the_perceptual_gate() {
        let small = fingerprint(&png(&cover(500, 0), 500)).unwrap();
        let large = fingerprint(&png(&cover(1500, 0), 1500)).unwrap();
        let v = compare(&small, &large);
        assert!(v.passes(), "same cover rejected: {v}");
        assert_eq!(small.min_side(), 500);
        assert_eq!(large.min_side(), 1500);
    }

    #[test]
    fn an_unrelated_cover_is_rejected_by_the_perceptual_gate() {
        let a = fingerprint(&png(&cover(500, 0), 500)).unwrap();
        let b = fingerprint(&png(&other_cover(1500), 1500)).unwrap();
        let v = compare(&a, &b);
        assert!(!v.passes(), "an unrelated cover was accepted: {v}");
    }

    #[test]
    fn a_recoloured_variant_fails_the_gate_and_needs_the_text_bypass() {
        // The regional-variant case DESIGN §5.4 ⑤ calls out: the same layout
        // in a different palette. The perceptual gate is meant to say no —
        // the near-exact text bypass is what lets it through.
        let a = fingerprint(&png(&cover(500, 0), 500)).unwrap();
        let b = fingerprint(&png(&cover(1500, 150), 1500)).unwrap();
        let v = compare(&a, &b);
        assert!(
            v.hamming <= HAMMING_MAX,
            "layout is unchanged, so the hash should still match: {v}"
        );
        assert!(!v.passes(), "a recoloured cover slipped through: {v}");
    }

    #[test]
    fn jpeg_re_encoding_does_not_move_the_scores_far() {
        // The realistic comparison is a JPEG thumbnail against a JPEG
        // original, not two lossless copies.
        let raw = cover(600, 0);
        let img = image::RgbaImage::from_raw(600, 600, raw.clone()).unwrap();
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .to_rgb8()
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();

        let a = fingerprint(&png(&raw, 600)).unwrap();
        let b = fingerprint(&jpeg.into_inner()).unwrap();
        let v = compare(&a, &b);
        assert!(v.passes(), "lossy re-encoding broke the gate: {v}");
    }

    #[test]
    fn the_size_gate_rejects_a_marginal_upgrade() {
        // The example from the design: 600 over 500 is not worth a swap.
        assert!(!size_gate(500, 600));
        assert!(!size_gate(500, 749));
        assert!(size_gate(500, 750));
        assert!(size_gate(500, 3000));
        // A candidate that is smaller is never an upgrade.
        assert!(!size_gate(1200, 600));
    }

    #[test]
    fn undecodable_bytes_are_an_error_rather_than_a_panic() {
        assert!(fingerprint(b"").is_err());
        assert!(fingerprint(b"not an image at all").is_err());
        // A truncated PNG: valid signature, no pixels.
        let mut truncated = png(&cover(32, 0), 32);
        truncated.truncate(40);
        assert!(fingerprint(&truncated).is_err());
    }

    #[test]
    fn an_absurdly_large_declared_size_is_refused_before_allocating() {
        // A PNG header claiming 60000x60000 must not become 14 GB of RGBA.
        let mut header = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        header.extend_from_slice(&13u32.to_be_bytes());
        header.extend_from_slice(b"IHDR");
        header.extend_from_slice(&60000u32.to_be_bytes());
        header.extend_from_slice(&60000u32.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        header.extend_from_slice(&[0u8; 4]);
        assert!(fingerprint(&header).is_err());
    }
}
