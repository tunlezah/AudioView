//! Four-character codes seen on the shairport-sync metadata pipe.
//!
//! See `docs/DESIGN.md` §4.3 for which of these we act on and which are
//! deliberately ignored.

use crate::FourCc;

/// The `type` field of an item: which subsystem produced it.
pub mod kind {
    use super::FourCc;

    /// Emitted by shairport-sync itself: session lifecycle, artwork, progress.
    pub const SSNC: FourCc = FourCc::new(b"ssnc");
    /// Passed through from the sender: DMAP/DAAP track metadata.
    pub const CORE: FourCc = FourCc::new(b"core");
}

/// Codes carried under [`kind::SSNC`].
pub mod ssnc {
    use super::FourCc;

    // --- session lifecycle ------------------------------------------------
    /// Active mode entered. Wraps a whole listening session; see DESIGN §5.1.1.
    pub const ABEG: FourCc = FourCc::new(b"abeg");
    /// Active mode exited, after `active_state_timeout`.
    pub const AEND: FourCc = FourCc::new(b"aend");
    /// Play stream begin. Fires per *track* under AirPlay 2.
    pub const PBEG: FourCc = FourCc::new(b"pbeg");
    /// Play stream end. Fires per *track* under AirPlay 2.
    pub const PEND: FourCc = FourCc::new(b"pend");
    /// Play stream flush (seek or pause).
    pub const PFLS: FourCc = FourCc::new(b"pfls");
    /// Play stream resume.
    pub const PRSM: FourCc = FourCc::new(b"prsm");
    /// First frame received and validly timed: audio is genuinely flowing.
    pub const PFFR: FourCc = FourCc::new(b"pffr");

    // --- metadata bundle framing ------------------------------------------
    pub const MDST: FourCc = FourCc::new(b"mdst");
    pub const MDEN: FourCc = FourCc::new(b"mden");

    // --- artwork ----------------------------------------------------------
    pub const PCST: FourCc = FourCc::new(b"pcst");
    pub const PCEN: FourCc = FourCc::new(b"pcen");
    /// Cover art payload (JPEG or PNG). Zero length means "no artwork".
    pub const PICT: FourCc = FourCc::new(b"PICT");

    // --- telemetry --------------------------------------------------------
    /// Progress: `"start/current/end"` in RTP timestamps.
    pub const PRGR: FourCc = FourCc::new(b"prgr");
    /// Volume: `"airplay,current,lowest,highest"` in dB.
    pub const PVOL: FourCc = FourCc::new(b"pvol");
    /// Metadata reception stalled; a watchdog input.
    pub const STAL: FourCc = FourCc::new(b"stal");

    // --- identification ---------------------------------------------------
    pub const SNAM: FourCc = FourCc::new(b"snam");
    pub const SNUA: FourCc = FourCc::new(b"snua");
    pub const SVNA: FourCc = FourCc::new(b"svna");
    pub const CLIP: FourCc = FourCc::new(b"clip");
    pub const SVIP: FourCc = FourCc::new(b"svip");
    pub const CONN: FourCc = FourCc::new(b"conn");
    pub const DISC: FourCc = FourCc::new(b"disc");
}

/// Codes carried under [`kind::CORE`] (DMAP/DAAP).
pub mod dmap {
    use super::FourCc;

    /// Item name: the track title.
    pub const MINM: FourCc = FourCc::new(b"minm");
    pub const ASAR: FourCc = FourCc::new(b"asar"); // artist
    pub const ASAL: FourCc = FourCc::new(b"asal"); // album
    pub const ASAA: FourCc = FourCc::new(b"asaa"); // album artist
    pub const ASGN: FourCc = FourCc::new(b"asgn"); // genre
    pub const ASCP: FourCc = FourCc::new(b"ascp"); // composer
    pub const ASCM: FourCc = FourCc::new(b"ascm"); // comment
    pub const ASDT: FourCc = FourCc::new(b"asdt"); // description
    pub const ASUL: FourCc = FourCc::new(b"asul"); // url
    pub const ASSN: FourCc = FourCc::new(b"assn"); // sort name
    pub const ASTM: FourCc = FourCc::new(b"astm"); // duration, ms
    pub const ASTN: FourCc = FourCc::new(b"astn"); // track number
    pub const ASDK: FourCc = FourCc::new(b"asdk"); // data kind
    pub const MPER: FourCc = FourCc::new(b"mper"); // persistent id
}
