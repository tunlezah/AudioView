//! Fixture packing.
//!
//! A raw capture of a real session is mostly base64 cover art: a handful of
//! tracks runs to tens of megabytes, which is not something to put in git.
//! `pack` externalises every artwork payload into a content-addressed file
//! and leaves a reference behind; `unpack` reverses it exactly.
//!
//! The transform is lossless and tested as such — a fixture that did not
//! round-trip byte-for-byte would quietly weaken every test built on it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use spmeta::codes::{kind, ssnc};
use spmeta::{encode_item, Decoder, MetaEvent, Parser};

/// Encoding marker used in place of `base64` for externalised payloads.
const REF_ENCODING: &str = "lpcapture-ref";

pub fn art_digest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Rewrite `input` with artwork payloads replaced by references, writing the
/// payloads into `art_dir`. Returns the packed bytes and the digests used.
pub fn pack(input: &[u8], art_dir: &Path) -> Result<(Vec<u8>, BTreeSet<String>)> {
    let mut out = Vec::with_capacity(input.len() / 4);
    let mut digests = BTreeSet::new();
    let mut parser = Parser::new();
    parser.feed(input);

    while let Some(result) = parser.next_item() {
        let item = match result {
            Ok(item) => item,
            // Damage in a capture is data, not a failure: a fixture that
            // reproduces a real parser error is worth keeping. Preserving it
            // exactly is impossible once reframed, so refuse rather than
            // silently produce a fixture that differs from what was captured.
            Err(e) => bail!("capture contains unparseable data ({e}); pack it as raw instead"),
        };

        let is_art = item.kind == kind::SSNC && item.code == ssnc::PICT && !item.payload.is_empty();
        if !is_art {
            out.extend_from_slice(&encode_item(item.kind, item.code, &item.payload));
            continue;
        }

        let digest = art_digest(&item.payload);
        let path = art_dir.join(format!("{digest}.bin"));
        if !path.exists() {
            std::fs::create_dir_all(art_dir)
                .with_context(|| format!("creating {}", art_dir.display()))?;
            std::fs::write(&path, &item.payload)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        out.extend_from_slice(
            format!(
                "<item><type>{:08x}</type><code>{:08x}</code><length>{}</length>\n\
                 <data encoding=\"{REF_ENCODING}\">\n{digest}</data></item>\n",
                item.kind.as_u32(),
                item.code.as_u32(),
                item.payload.len()
            )
            .as_bytes(),
        );
        digests.insert(digest);
    }

    // A capture that ends mid-item is the whole point of some fixtures;
    // reframing must not quietly tidy the truncation away.
    out.extend_from_slice(trailing_partial_item(&parser));

    Ok((out, digests))
}

/// Reverse [`pack`], reading payloads back out of `art_dir`.
pub fn unpack(packed: &[u8], art_dir: &Path) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(packed.len() * 4);
    let mut rest = packed;

    // References are rewritten textually; everything else passes through
    // untouched, so a packed file that contains no references is a no-op.
    let open = format!("<data encoding=\"{REF_ENCODING}\">").into_bytes();
    while let Some(at) = find(rest, &open) {
        // Rewind to the enclosing <item> so the whole record can be re-encoded.
        let item_start = rfind(&rest[..at], b"<item>")
            .context("found an artwork reference with no enclosing <item>")?;
        let item_end = find(&rest[at..], b"</item>")
            .map(|n| at + n + b"</item>".len())
            .context("found an unterminated artwork reference")?;

        out.extend_from_slice(&rest[..item_start]);

        let body_start = at + open.len();
        let body_end = find(&rest[body_start..], b"</data>")
            .map(|n| body_start + n)
            .context("artwork reference is missing </data>")?;
        let digest = std::str::from_utf8(&rest[body_start..body_end])
            .context("artwork reference is not utf-8")?
            .trim()
            .to_string();
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("artwork reference {digest:?} is not a sha256 digest");
        }

        let path = art_dir.join(format!("{digest}.bin"));
        let payload = std::fs::read(&path)
            .with_context(|| format!("reading referenced artwork {}", path.display()))?;
        let actual = art_digest(&payload);
        if actual != digest {
            bail!("{} has digest {actual}, expected {digest}", path.display());
        }

        out.extend_from_slice(&encode_item(kind::SSNC, ssnc::PICT, &payload));
        rest = &rest[item_end..];
        // Drop the newline the packer wrote after the reference item.
        if rest.first() == Some(&b'\n') {
            rest = &rest[1..];
        }
    }
    out.extend_from_slice(rest);
    Ok(out)
}

/// Reframe a stream by parsing and re-encoding every item.
///
/// `pack` normalises framing as a side effect, so round-trip verification
/// compares against this rather than the original bytes — otherwise a capture
/// whose whitespace differs from our encoder would look lossy when it is not.
pub fn canonicalise(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut parser = Parser::new();
    parser.feed(input);
    while let Some(result) = parser.next_item() {
        let item = result.map_err(|e| anyhow::anyhow!("capture contains unparseable data: {e}"))?;
        out.extend_from_slice(&encode_item(item.kind, item.code, &item.payload));
    }
    out.extend_from_slice(trailing_partial_item(&parser));
    Ok(out)
}

/// The unconsumed tail, minus separator whitespace.
///
/// `Parser::remainder` is literal, so after a clean stream it still holds the
/// newline following the last item. `encode_item` emits that separator
/// itself, so copying the remainder verbatim appends one extra newline per
/// pack/unpack cycle — which compounds silently until a round-trip check
/// catches it.
fn trailing_partial_item(parser: &Parser) -> &[u8] {
    let rest = parser.remainder();
    let start = rest
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(rest.len());
    &rest[start..]
}

/// Load a fixture, rehydrating artwork references if present.
pub fn load(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let art_dir = default_art_dir(path);
    unpack(&raw, &art_dir)
}

/// Fixtures live in `fixtures/sessions/`, artwork in a sibling `fixtures/art/`.
pub fn default_art_dir(fixture: &Path) -> PathBuf {
    fixture
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| Path::new("."))
        .join("art")
}

/// Decode a fixture into its event sequence, the golden-file representation.
pub fn events(bytes: &[u8]) -> Vec<GoldenEvent> {
    let mut d = Decoder::new();
    d.feed(bytes);
    d.drain().iter().map(golden_from_result).collect()
}

/// Render one decode result as a golden line.
pub fn golden_from_result(result: &Result<MetaEvent, spmeta::ParseError>) -> GoldenEvent {
    match result {
        Ok(ev) => GoldenEvent::from_event(ev),
        Err(e) => GoldenEvent {
            event: "parse_error".into(),
            detail: Some(e.to_string()),
        },
    }
}

/// One line of a golden file.
///
/// Deliberately a flattened string form rather than a serialisation of
/// `MetaEvent`: golden files are read by humans during review, and a diff
/// should show "the album changed" rather than a restructured enum.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoldenEvent {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl GoldenEvent {
    pub fn from_event(ev: &MetaEvent) -> GoldenEvent {
        use spmeta::CoreField as F;
        use MetaEvent as E;
        let detail = match ev {
            // Artwork is summarised by size and digest: the bytes live in
            // fixtures/art/ and a golden file full of base64 is unreviewable.
            E::Picture(p) => Some(format!("{} bytes sha256:{}", p.len(), &art_digest(p)[..16])),
            E::Progress(p) => Some(format!("{}ms/{}ms", p.position_ms(), p.duration_ms())),
            E::Volume(v) => Some(format!("{:.1}dB", v.airplay_db)),
            E::ClientName(s)
            | E::UserAgent(s)
            | E::ServerName(s)
            | E::ClientIp(s)
            | E::ServerIp(s)
            | E::ClientConnected(s)
            | E::ClientDisconnected(s) => Some(s.clone()),
            E::Unknown { kind, code, len } => Some(format!("{kind}/{code} {len} bytes")),
            E::Core(f) => Some(match f {
                F::Title(s) => format!("title={s}"),
                F::Artist(s) => format!("artist={s}"),
                F::Album(s) => format!("album={s}"),
                F::AlbumArtist(s) => format!("album_artist={s}"),
                F::Genre(s) => format!("genre={s}"),
                F::Composer(s) => format!("composer={s}"),
                F::Comment(s) => format!("comment={s}"),
                F::Description(s) => format!("description={s}"),
                F::SortName(s) => format!("sort_name={s}"),
                F::Url(s) => format!("url={s}"),
                F::DurationMs(v) => format!("duration_ms={v}"),
                F::TrackNumber(v) => format!("track_number={v}"),
                F::PersistentId(v) => format!("persistent_id={v:016x}"),
                F::Other { code, raw } => format!("{code} ({} bytes)", raw.len()),
            }),
            _ => None,
        };
        GoldenEvent {
            event: ev.label().to_string(),
            detail,
        }
    }
}

pub fn golden_path(fixture: &Path) -> PathBuf {
    fixture.with_extension("events.json")
}

pub fn render_golden(events: &[GoldenEvent]) -> Result<String> {
    let mut s = serde_json::to_string_pretty(events)?;
    s.push('\n');
    Ok(s)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).rposition(|w| w == needle)
}
