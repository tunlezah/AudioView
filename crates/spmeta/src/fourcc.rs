use std::fmt;

/// A four-character code, as used for the `type` and `code` fields of a
/// metadata item. Stored big-endian, matching the on-the-wire hex encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FourCc(pub [u8; 4]);

impl FourCc {
    pub const fn new(v: &[u8; 4]) -> Self {
        FourCc(*v)
    }

    pub const fn from_u32(v: u32) -> Self {
        FourCc(v.to_be_bytes())
    }

    pub const fn as_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    pub const fn as_bytes(&self) -> &[u8; 4] {
        &self.0
    }

    /// The code as a string, if all four bytes are printable ASCII.
    pub fn as_str(&self) -> Option<&str> {
        if self.0.iter().all(|b| (0x20..0x7f).contains(b)) {
            std::str::from_utf8(&self.0).ok()
        } else {
            None
        }
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            Some(s) => f.write_str(s),
            // Non-printable codes do occur; render them unambiguously rather
            // than lossily, so a log line is enough to identify the sender.
            None => write!(f, "0x{:08x}", self.as_u32()),
        }
    }
}

impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FourCc({self})")
    }
}

impl From<u32> for FourCc {
    fn from(v: u32) -> Self {
        FourCc::from_u32(v)
    }
}

impl From<[u8; 4]> for FourCc {
    fn from(v: [u8; 4]) -> Self {
        FourCc(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_u32() {
        let cc = FourCc::new(b"ssnc");
        assert_eq!(cc.as_u32(), 0x73736e63);
        assert_eq!(FourCc::from_u32(0x73736e63), cc);
    }

    #[test]
    fn displays_printable_and_escapes_the_rest() {
        assert_eq!(FourCc::new(b"PICT").to_string(), "PICT");
        assert_eq!(FourCc::from_u32(0x0001_0203).to_string(), "0x00010203");
    }
}
