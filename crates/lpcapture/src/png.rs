//! A minimal PNG writer, used to give synthetic fixtures real decodable
//! artwork rather than random bytes.
//!
//! Deliberately dependency-free and uncompressed (deflate "stored" blocks):
//! fixtures are small, and a hand-rolled encoder with no image dependency
//! keeps the fixture tool buildable anywhere.

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c = table[((c ^ *b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut crc_input = Vec::with_capacity(4 + body.len());
    crc_input.extend_from_slice(tag);
    crc_input.extend_from_slice(body);
    out.extend_from_slice(&crc_input);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Zlib stream using stored (uncompressed) deflate blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    for (i, block) in raw.chunks(65_535).enumerate() {
        let last = (i + 1) * 65_535 >= raw.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// An 8-bit RGB PNG of `w`×`h`, filled by `px(x, y) -> [r, g, b]`.
pub fn rgb(w: u32, h: u32, px: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((h * (1 + w * 3)) as usize);
    for y in 0..h {
        raw.push(0); // filter type: none
        for x in 0..w {
            raw.extend_from_slice(&px(x, y));
        }
    }

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // depth 8, truecolour, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", b"");
    out
}

/// Fake cover art: a two-tone diagonal split with a corner block, so that
/// different tracks produce visibly and perceptually distinct images.
pub fn fake_cover(size: u32, hue: u8) -> Vec<u8> {
    let a = [hue, hue.wrapping_mul(3), 255u8.wrapping_sub(hue)];
    let b = [
        255u8.wrapping_sub(hue),
        hue.wrapping_mul(7),
        hue.wrapping_add(64),
    ];
    rgb(size, size, move |x, y| {
        if x < size / 5 && y < size / 5 {
            [255, 255, 255]
        } else if x + y < size {
            a
        } else {
            b
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_structurally_valid_png() {
        let png = fake_cover(64, 40);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &64u32.to_be_bytes());
        assert!(png.ends_with(&[0xae, 0x42, 0x60, 0x82])); // IEND crc
    }

    #[test]
    fn chunk_crcs_verify() {
        let png = fake_cover(32, 7);
        let mut i = 8;
        let mut seen = Vec::new();
        while i + 8 <= png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let tag = &png[i + 4..i + 8];
            let body_end = i + 8 + len;
            let expect = u32::from_be_bytes(png[body_end..body_end + 4].try_into().unwrap());
            assert_eq!(crc32(&png[i + 4..body_end]), expect, "crc for {tag:?}");
            seen.push(String::from_utf8_lossy(tag).to_string());
            i = body_end + 4;
        }
        assert_eq!(seen, ["IHDR", "IDAT", "IEND"]);
        assert_eq!(i, png.len());
    }

    #[test]
    fn multi_block_images_stay_valid() {
        // Larger than one 65535-byte stored block, exercising the loop.
        let png = fake_cover(200, 3);
        assert!(png.len() > 65_535);
        assert_eq!(&png[12..16], b"IHDR");
    }
}
