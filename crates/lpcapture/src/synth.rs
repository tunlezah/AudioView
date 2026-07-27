//! Synthetic sessions.
//!
//! These are not a substitute for real captures — the whole point of
//! `lpcapture tee` is to record actual Apple Music sessions, and the
//! semantics of a few codes stay unverified until we have them (DESIGN §4.3).
//! What they are is a deterministic, committable baseline so the parser and
//! `artd`'s state machine have material to test against today, including
//! failure shapes that are awkward to provoke on demand with a real phone.

use spmeta::codes::{dmap, kind, ssnc};
use spmeta::{encode_item, FourCc};

use crate::png;

pub struct Session {
    pub name: &'static str,
    pub description: &'static str,
    pub bytes: Vec<u8>,
}

struct Builder {
    out: Vec<u8>,
}

impl Builder {
    fn new() -> Self {
        Builder { out: Vec::new() }
    }

    fn ssnc(&mut self, code: FourCc) -> &mut Self {
        self.out.extend(encode_item(kind::SSNC, code, b""));
        self
    }

    fn ssnc_data(&mut self, code: FourCc, payload: &[u8]) -> &mut Self {
        self.out.extend(encode_item(kind::SSNC, code, payload));
        self
    }

    fn core(&mut self, code: FourCc, payload: &[u8]) -> &mut Self {
        self.out.extend(encode_item(kind::CORE, code, payload));
        self
    }

    fn connect(&mut self, client: &str, agent: &str) -> &mut Self {
        self.ssnc_data(ssnc::SNAM, client.as_bytes())
            .ssnc_data(ssnc::SNUA, agent.as_bytes())
            .ssnc_data(ssnc::CLIP, b"192.168.1.42")
            .ssnc_data(ssnc::SVIP, b"192.168.1.10")
    }

    /// A metadata bundle: `mdst`, the core fields, then `mden`.
    #[allow(clippy::too_many_arguments)]
    fn bundle(
        &mut self,
        artist: &str,
        album: &str,
        title: &str,
        track_no: u16,
        duration_ms: u32,
        persistent_id: u64,
    ) -> &mut Self {
        self.ssnc(ssnc::MDST)
            .core(dmap::ASAR, artist.as_bytes())
            .core(dmap::ASAL, album.as_bytes())
            .core(dmap::ASAA, artist.as_bytes())
            .core(dmap::MINM, title.as_bytes())
            .core(dmap::ASGN, b"Electronica")
            .core(dmap::ASTN, &track_no.to_be_bytes())
            .core(dmap::ASTM, &duration_ms.to_be_bytes())
            .core(dmap::MPER, &persistent_id.to_be_bytes())
            .ssnc(ssnc::MDEN)
    }

    /// Artwork, framed by `pcst`/`pcen` as shairport-sync emits it.
    fn artwork(&mut self, hue: u8) -> &mut Self {
        let art = png::fake_cover(128, hue);
        self.ssnc_data(ssnc::PCST, art.len().to_string().as_bytes())
            .ssnc_data(ssnc::PICT, &art)
            .ssnc(ssnc::PCEN)
    }

    fn no_artwork(&mut self) -> &mut Self {
        self.ssnc_data(ssnc::PCST, b"0")
            .ssnc_data(ssnc::PICT, b"")
            .ssnc(ssnc::PCEN)
    }

    fn progress(&mut self, position_s: u32, duration_s: u32) -> &mut Self {
        let hz = spmeta::dmap::RTP_HZ as u32;
        let start = 1u32;
        let payload = format!(
            "{}/{}/{}",
            start,
            start + position_s * hz,
            start + duration_s * hz
        );
        self.ssnc_data(ssnc::PRGR, payload.as_bytes())
    }

    fn volume(&mut self, db: f32) -> &mut Self {
        let payload = format!("{db:.6},0.000000,-30.000000,0.000000");
        self.ssnc_data(ssnc::PVOL, payload.as_bytes())
    }

    fn done(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }
}

const TRACKS: &[(&str, &str, &str, u16, u32, u64, u8)] = &[
    (
        "Massive Attack",
        "Mezzanine",
        "Angel",
        1,
        379_000,
        0x0102_0304_0506_0701,
        30,
    ),
    (
        "Massive Attack",
        "Mezzanine",
        "Risingson",
        2,
        298_000,
        0x0102_0304_0506_0702,
        90,
    ),
    (
        "Massive Attack",
        "Mezzanine",
        "Teardrop",
        3,
        330_000,
        0x0102_0304_0506_0703,
        170,
    ),
];

/// A normal album play: one active session containing several tracks.
///
/// The important shape here is that `pbeg`/`pend` cycle *per track* inside a
/// single `abeg`…`aend` envelope (DESIGN §5.1.1).
fn album() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Ben's iPhone", "AirPlay/845.5.1");
    b.ssnc(ssnc::ABEG);
    for (i, (artist, alb, title, no, dur, mper, hue)) in TRACKS.iter().enumerate() {
        b.ssnc(ssnc::PBEG);
        b.bundle(artist, alb, title, *no, *dur, *mper);
        b.artwork(*hue);
        if i == 0 {
            b.volume(-14.5);
        }
        b.ssnc(ssnc::PFFR);
        b.progress(0, dur / 1000);
        b.progress(dur / 2000, dur / 1000);
        b.ssnc(ssnc::PEND);
    }
    b.ssnc(ssnc::AEND);
    b.done()
}

/// A track with no cover art: `PICT` arrives with zero length.
fn no_artwork() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Kitchen iPad", "AirPlay/845.5.1");
    b.ssnc(ssnc::ABEG).ssnc(ssnc::PBEG);
    b.bundle(
        "Unknown Artist",
        "Field Recordings",
        "Untitled",
        1,
        60_000,
        0x11,
    );
    b.no_artwork();
    b.ssnc(ssnc::PFFR).progress(0, 60);
    b.ssnc(ssnc::PEND).ssnc(ssnc::AEND);
    b.done()
}

/// Pause and resume mid-track.
fn pause_resume() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Ben's iPhone", "AirPlay/845.5.1");
    b.ssnc(ssnc::ABEG).ssnc(ssnc::PBEG);
    b.bundle(
        "Boards of Canada",
        "Music Has the Right to Children",
        "Roygbiv",
        8,
        151_000,
        0x21,
    );
    b.artwork(200);
    b.ssnc(ssnc::PFFR).progress(0, 151);
    b.progress(30, 151);
    b.ssnc(ssnc::PFLS);
    b.ssnc(ssnc::PRSM);
    b.progress(31, 151);
    b.ssnc(ssnc::PEND).ssnc(ssnc::AEND);
    b.done()
}

/// Skipping quickly through tracks: several short play streams with no gap.
/// This is the shape that makes a naive amp trigger chatter.
fn track_skip() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Ben's iPhone", "AirPlay/845.5.1");
    b.ssnc(ssnc::ABEG);
    for (i, (artist, alb, title, no, dur, mper, hue)) in TRACKS.iter().enumerate() {
        b.ssnc(ssnc::PBEG);
        b.bundle(artist, alb, title, *no, *dur, *mper);
        b.artwork(*hue);
        b.ssnc(ssnc::PFFR);
        if i < TRACKS.len() - 1 {
            b.ssnc(ssnc::PFLS);
        }
        b.ssnc(ssnc::PEND);
    }
    b.ssnc(ssnc::AEND);
    b.done()
}

/// The sender vanishes: no `pend`, no `aend`, and the stream simply stops
/// mid-item. Drives the stall/session watchdog (DESIGN §5.1).
fn abrupt_disconnect() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Ben's iPhone", "AirPlay/845.5.1");
    b.ssnc(ssnc::ABEG).ssnc(ssnc::PBEG);
    b.bundle(
        "Aphex Twin",
        "Selected Ambient Works 85-92",
        "Xtal",
        1,
        293_000,
        0x31,
    );
    b.artwork(60);
    b.ssnc(ssnc::PFFR).progress(0, 293);
    b.progress(12, 293);
    b.ssnc(ssnc::STAL);
    let mut out = b.done();
    // Truncate mid-item, as a killed writer would leave it.
    out.extend_from_slice(b"<item><type>73736e63</type><code>7072");
    out
}

/// An older sender that never emits `abeg`/`aend`; `pbeg` has to imply the
/// session, and the session timeout has to close it.
fn airplay1_no_active() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Old iTunes", "iTunes/12.9");
    b.ssnc(ssnc::PBEG);
    b.bundle("Portishead", "Dummy", "Roads", 8, 322_000, 0x41);
    b.artwork(120);
    b.ssnc(ssnc::PFFR).progress(0, 322);
    b.ssnc(ssnc::PEND);
    b.done()
}

/// Two senders in succession, the second taking over without a clean end
/// from the first.
fn client_handover() -> Vec<u8> {
    let mut b = Builder::new();
    b.connect("Ben's iPhone", "AirPlay/845.5.1");
    b.ssnc(ssnc::ABEG).ssnc(ssnc::PBEG);
    b.bundle("Burial", "Untrue", "Archangel", 2, 231_000, 0x51);
    b.artwork(15);
    b.ssnc(ssnc::PFFR);
    // No pend/aend: the new client just arrives.
    b.connect("Living Room Mac", "AirPlay/860.7.1");
    b.ssnc(ssnc::ABEG).ssnc(ssnc::PBEG);
    b.bundle("Four Tet", "Rounds", "She Moves She", 3, 254_000, 0x52);
    b.artwork(220);
    b.ssnc(ssnc::PFFR);
    b.ssnc(ssnc::PEND).ssnc(ssnc::AEND);
    b.done()
}

/// Codes we deliberately do not model, mixed into a normal session. Asserts
/// that unknown codes are inert rather than disruptive.
fn unknown_codes() -> Vec<u8> {
    let mut b = Builder::new();
    b.ssnc(ssnc::ABEG);
    b.ssnc_data(FourCc::new(b"copl"), b"<plist>...</plist>");
    b.ssnc_data(FourCc::new(b"acre"), b"1234567890");
    b.ssnc_data(FourCc::new(b"dapo"), b"3689");
    b.ssnc(ssnc::PBEG);
    b.bundle(
        "Jon Hopkins",
        "Immunity",
        "Open Eye Signal",
        3,
        477_000,
        0x61,
    );
    b.core(FourCc::new(b"asdk"), &[0u8]);
    b.core(FourCc::new(b"aeSP"), &[1u8]);
    b.artwork(80);
    b.ssnc(ssnc::PFFR);
    b.ssnc_data(FourCc::new(b"phbt"), b"0/0");
    b.ssnc(ssnc::PEND).ssnc(ssnc::AEND);
    b.done()
}

pub fn all() -> Vec<Session> {
    vec![
        Session {
            name: "album",
            description: "Three-track album: pbeg/pend cycles inside one abeg/aend envelope",
            bytes: album(),
        },
        Session {
            name: "no-artwork",
            description: "Track with a zero-length PICT",
            bytes: no_artwork(),
        },
        Session {
            name: "pause-resume",
            description: "pfls then prsm mid-track",
            bytes: pause_resume(),
        },
        Session {
            name: "track-skip",
            description: "Rapid skipping: several short play streams in one session",
            bytes: track_skip(),
        },
        Session {
            name: "abrupt-disconnect",
            description: "Sender vanishes; no pend/aend and the stream stops mid-item",
            bytes: abrupt_disconnect(),
        },
        Session {
            name: "airplay1-no-active",
            description: "Sender that never emits abeg/aend",
            bytes: airplay1_no_active(),
        },
        Session {
            name: "client-handover",
            description: "Second sender takes over with no clean end from the first",
            bytes: client_handover(),
        },
        Session {
            name: "unknown-codes",
            description: "Unmodelled ssnc and core codes mixed into a normal session",
            bytes: unknown_codes(),
        },
    ]
}
