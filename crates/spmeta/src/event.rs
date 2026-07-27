//! The typed layer over raw items: [`Decoder`] turns [`MetaItem`]s into
//! [`MetaEvent`]s, which is what `artd`'s state machine consumes.

use crate::codes::{dmap, kind, ssnc};
use crate::dmap::{self as dmapval, Progress, Volume};
use crate::parser::{Limits, MetaItem, ParseError, Parser, Stats};
use crate::FourCc;

/// A track metadata field from a `core` item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreField {
    Title(String),
    Artist(String),
    Album(String),
    AlbumArtist(String),
    Genre(String),
    Composer(String),
    Comment(String),
    Description(String),
    SortName(String),
    Url(String),
    DurationMs(u64),
    TrackNumber(u64),
    PersistentId(u64),
    /// A `core` code we do not model. Kept so captures round-trip and so a
    /// new field shows up in diagnostics rather than vanishing.
    Other {
        code: FourCc,
        raw: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetaEvent {
    // Session lifecycle. See DESIGN §5.1.1 for the abeg/pbeg distinction.
    ActiveBegin,
    ActiveEnd,
    PlayBegin,
    PlayEnd,
    PlayFlush,
    PlayResume,
    FirstFrame,

    // Metadata bundle framing.
    BundleStart,
    BundleEnd,

    // Artwork.
    PictureStart,
    PictureEnd,
    Picture(Vec<u8>),
    /// A zero-length `PICT`: this track explicitly has no artwork.
    PictureCleared,

    // Telemetry.
    Progress(Progress),
    Volume(Volume),
    Stall,

    // Identification.
    ClientName(String),
    UserAgent(String),
    ServerName(String),
    ClientIp(String),
    ServerIp(String),
    ClientConnected(String),
    ClientDisconnected(String),

    Core(CoreField),

    /// A code we do not model. Not an error: shairport-sync emits remote
    /// control plumbing we have no use for, and newer versions add codes.
    Unknown {
        kind: FourCc,
        code: FourCc,
        len: usize,
    },
}

impl MetaEvent {
    /// A short stable label, used for golden files and log lines.
    pub fn label(&self) -> &'static str {
        use MetaEvent::*;
        match self {
            ActiveBegin => "active_begin",
            ActiveEnd => "active_end",
            PlayBegin => "play_begin",
            PlayEnd => "play_end",
            PlayFlush => "play_flush",
            PlayResume => "play_resume",
            FirstFrame => "first_frame",
            BundleStart => "bundle_start",
            BundleEnd => "bundle_end",
            PictureStart => "picture_start",
            PictureEnd => "picture_end",
            Picture(_) => "picture",
            PictureCleared => "picture_cleared",
            Progress(_) => "progress",
            Volume(_) => "volume",
            Stall => "stall",
            ClientName(_) => "client_name",
            UserAgent(_) => "user_agent",
            ServerName(_) => "server_name",
            ClientIp(_) => "client_ip",
            ServerIp(_) => "server_ip",
            ClientConnected(_) => "client_connected",
            ClientDisconnected(_) => "client_disconnected",
            Core(_) => "core",
            Unknown { .. } => "unknown",
        }
    }
}

/// Framing parser plus payload decoding.
pub struct Decoder {
    parser: Parser,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            parser: Parser::new(),
        }
    }

    pub fn with_limits(limits: Limits) -> Self {
        Decoder {
            parser: Parser::with_limits(limits),
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.parser.feed(data);
    }

    pub fn stats(&self) -> Stats {
        self.parser.stats()
    }

    /// Next event, or `None` when more input is needed.
    #[allow(clippy::should_implement_trait)]
    pub fn next_event(&mut self) -> Option<Result<MetaEvent, ParseError>> {
        Some(self.parser.next_item()?.map(decode_item))
    }

    /// Convenience for tests and replay: decode everything currently buffered.
    pub fn drain(&mut self) -> Vec<Result<MetaEvent, ParseError>> {
        let mut out = Vec::new();
        while let Some(ev) = self.next_event() {
            out.push(ev);
        }
        out
    }
}

pub fn decode_item(item: MetaItem) -> MetaEvent {
    match item.kind {
        kind::SSNC => decode_ssnc(item),
        kind::CORE => decode_core(item),
        _ => unknown(item),
    }
}

fn unknown(item: MetaItem) -> MetaEvent {
    MetaEvent::Unknown {
        kind: item.kind,
        code: item.code,
        len: item.payload.len(),
    }
}

fn decode_ssnc(item: MetaItem) -> MetaEvent {
    use MetaEvent as E;
    let p = &item.payload;
    match item.code {
        ssnc::ABEG => E::ActiveBegin,
        ssnc::AEND => E::ActiveEnd,
        ssnc::PBEG => E::PlayBegin,
        ssnc::PEND => E::PlayEnd,
        ssnc::PFLS => E::PlayFlush,
        ssnc::PRSM => E::PlayResume,
        ssnc::PFFR => E::FirstFrame,
        ssnc::MDST => E::BundleStart,
        ssnc::MDEN => E::BundleEnd,
        ssnc::PCST => E::PictureStart,
        ssnc::PCEN => E::PictureEnd,
        ssnc::STAL => E::Stall,
        ssnc::PICT => {
            if p.is_empty() {
                E::PictureCleared
            } else {
                E::Picture(item.payload)
            }
        }
        // A malformed prgr/pvol is reported as Unknown rather than dropped,
        // so it shows up in diagnostics instead of looking like a gap.
        ssnc::PRGR => Progress::parse(p).map_or_else(|| unknown(item.clone()), E::Progress),
        ssnc::PVOL => Volume::parse(p).map_or_else(|| unknown(item.clone()), E::Volume),
        ssnc::SNAM => E::ClientName(dmapval::text(p)),
        ssnc::SNUA => E::UserAgent(dmapval::text(p)),
        ssnc::SVNA => E::ServerName(dmapval::text(p)),
        ssnc::CLIP => E::ClientIp(dmapval::text(p)),
        ssnc::SVIP => E::ServerIp(dmapval::text(p)),
        ssnc::CONN => E::ClientConnected(dmapval::text(p)),
        ssnc::DISC => E::ClientDisconnected(dmapval::text(p)),
        _ => unknown(item),
    }
}

fn decode_core(item: MetaItem) -> MetaEvent {
    use CoreField as F;
    let p = &item.payload;

    let text = |f: fn(String) -> F| Some(f(dmapval::text(p)));
    // A numeric field of an unexpected width falls through to `Other` rather
    // than being coerced; a wrong duration is worse than a missing one.
    let num = |f: fn(u64) -> F| dmapval::be_uint(p).map(f);

    let field = match item.code {
        dmap::MINM => text(F::Title),
        dmap::ASAR => text(F::Artist),
        dmap::ASAL => text(F::Album),
        dmap::ASAA => text(F::AlbumArtist),
        dmap::ASGN => text(F::Genre),
        dmap::ASCP => text(F::Composer),
        dmap::ASCM => text(F::Comment),
        dmap::ASDT => text(F::Description),
        dmap::ASSN => text(F::SortName),
        dmap::ASUL => text(F::Url),
        dmap::ASTM => num(F::DurationMs),
        dmap::ASTN => num(F::TrackNumber),
        dmap::MPER => num(F::PersistentId),
        _ => None,
    };

    MetaEvent::Core(field.unwrap_or(F::Other {
        code: item.code,
        raw: item.payload,
    }))
}
