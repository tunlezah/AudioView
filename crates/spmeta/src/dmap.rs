//! Payload decoding for DMAP/DAAP `core` items.
//!
//! DMAP numeric values are big-endian integers of varying width; strings are
//! UTF-8 and occasionally NUL-padded. Nothing here trusts the sender.

/// Big-endian unsigned integer of 1, 2, 4 or 8 bytes.
///
/// Other widths return `None` rather than guessing — a `u64` silently derived
/// from a 3-byte field would be a plausible-looking wrong duration.
pub fn be_uint(b: &[u8]) -> Option<u64> {
    match b.len() {
        1 => Some(b[0] as u64),
        2 => Some(u16::from_be_bytes([b[0], b[1]]) as u64),
        4 => Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64),
        8 => Some(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ])),
        _ => None,
    }
}

/// Lossy UTF-8, with trailing NULs and surrounding whitespace removed.
pub fn text(b: &[u8]) -> String {
    let end = b.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// `prgr` payload: `"start/current/end"` in RTP timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub start: u32,
    pub current: u32,
    pub end: u32,
}

/// AirPlay's RTP clock. Both AirPlay 1 and the AirPlay 2 realtime path run
/// at 44.1 kHz regardless of the source material's sample rate.
pub const RTP_HZ: u64 = 44_100;

impl Progress {
    pub fn parse(b: &[u8]) -> Option<Progress> {
        let s = std::str::from_utf8(b).ok()?;
        let mut it = s.trim().split('/');
        let start = it.next()?.trim().parse().ok()?;
        let current = it.next()?.trim().parse().ok()?;
        let end = it.next()?.trim().parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(Progress {
            start,
            current,
            end,
        })
    }

    /// Elapsed playback position. Saturates rather than wrapping: the RTP
    /// timestamp is a u32 that does wrap (roughly every 27 hours), and a
    /// wrapped subtraction here would produce a wildly wrong position.
    pub fn position_ms(&self) -> u64 {
        (self.current.saturating_sub(self.start)) as u64 * 1000 / RTP_HZ
    }

    pub fn duration_ms(&self) -> u64 {
        (self.end.saturating_sub(self.start)) as u64 * 1000 / RTP_HZ
    }
}

/// `pvol` payload: `"airplay,current,lowest,highest"` in dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volume {
    /// The sender's requested volume. -144.0 means muted.
    pub airplay_db: f32,
    pub current_db: f32,
    pub lowest_db: f32,
    pub highest_db: f32,
}

impl Volume {
    pub fn parse(b: &[u8]) -> Option<Volume> {
        let s = std::str::from_utf8(b).ok()?;
        let mut it = s.trim().split(',');
        let mut next = || -> Option<f32> { it.next()?.trim().parse().ok() };
        let v = Volume {
            airplay_db: next()?,
            current_db: next()?,
            lowest_db: next()?,
            highest_db: next()?,
        };
        if it.next().is_some() {
            return None;
        }
        Some(v)
    }

    pub fn is_muted(&self) -> bool {
        self.airplay_db <= -144.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_integer_widths() {
        assert_eq!(be_uint(&[0x01]), Some(1));
        assert_eq!(be_uint(&[0x01, 0x00]), Some(256));
        assert_eq!(be_uint(&[0, 0, 0x01, 0x00]), Some(256));
        assert_eq!(be_uint(&[0, 0, 0, 0, 0, 0, 0x01, 0x00]), Some(256));
        assert_eq!(be_uint(&[0x01, 0x02, 0x03]), None);
        assert_eq!(be_uint(&[]), None);
    }

    #[test]
    fn trims_nul_padding_from_text() {
        assert_eq!(text(b"Mezzanine\0\0\0"), "Mezzanine");
        assert_eq!(text(b"  Teardrop  "), "Teardrop");
        assert_eq!(text(b""), "");
    }

    #[test]
    fn parses_progress_and_derives_position() {
        let p = Progress::parse(b"1/441001/13230001").unwrap();
        assert_eq!(p.start, 1);
        assert_eq!(p.position_ms(), 10_000);
        assert_eq!(p.duration_ms(), 300_000);
    }

    #[test]
    fn progress_saturates_on_wrapped_timestamps() {
        // current < start happens across a u32 wrap; must not underflow.
        let p = Progress {
            start: 100,
            current: 50,
            end: 200,
        };
        assert_eq!(p.position_ms(), 0);
    }

    #[test]
    fn rejects_malformed_progress() {
        assert!(Progress::parse(b"1/2").is_none());
        assert!(Progress::parse(b"1/2/3/4").is_none());
        assert!(Progress::parse(b"a/b/c").is_none());
    }

    #[test]
    fn parses_volume_and_detects_mute() {
        let v = Volume::parse(b"-14.500000,0.000000,-30.000000,0.000000").unwrap();
        assert_eq!(v.airplay_db, -14.5);
        assert!(!v.is_muted());
        assert!(Volume::parse(b"-144.000000,0.0,-30.0,0.0")
            .unwrap()
            .is_muted());
        assert!(Volume::parse(b"-14.5,0.0").is_none());
    }
}
