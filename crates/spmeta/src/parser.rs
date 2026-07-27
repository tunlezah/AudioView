//! Incremental framing parser for the shairport-sync metadata pipe.
//!
//! The pipe is a byte stream, not a document: there is no root element, items
//! arrive back to back, and a reader routinely starts mid-item (every
//! reconnect). The parser is therefore a resynchronising scanner rather than
//! an XML parser.

use base64::engine::general_purpose::GeneralPurposeConfig;
use base64::engine::{DecodePaddingMode, GeneralPurpose};
use base64::{alphabet, Engine};

use crate::FourCc;

const ITEM_OPEN: &[u8] = b"<item>";

/// Tolerant base64: shairport-sync line-wraps its payloads, and we would
/// rather decode a slightly malformed payload than drop cover art.
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// One item read off the pipe.
#[derive(Clone, PartialEq, Eq)]
pub struct MetaItem {
    pub kind: FourCc,
    pub code: FourCc,
    /// Decoded payload. Empty for zero-length items.
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for MetaItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Payloads are routinely megabytes of JPEG; never print them.
        f.debug_struct("MetaItem")
            .field("kind", &self.kind)
            .field("code", &self.code)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("malformed item at byte {offset}: {reason}")]
    Malformed { offset: u64, reason: &'static str },

    #[error("{len} bytes of junk before an item at byte {offset}")]
    Junk { offset: u64, len: usize },

    #[error("item at byte {offset} declares {len} bytes, over the {max} limit")]
    TooLarge { offset: u64, len: usize, max: usize },

    #[error("no item found within {max} buffered bytes; resynchronising")]
    BufferOverflow { max: usize },

    #[error("base64 payload at byte {offset} did not decode")]
    BadBase64 { offset: u64 },
}

/// Resource limits. The pipe is reachable by anything on the LAN that can
/// AirPlay to us, so declared lengths are never trusted.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest accepted single payload. Covers any plausible cover art.
    pub max_item_bytes: usize,
    /// Largest amount of unparsed data we will hold before resynchronising.
    pub max_buffer_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_item_bytes: 8 * 1024 * 1024,
            max_buffer_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Counters worth surfacing as metrics; all of these are "shouldn't happen,
/// but does" conditions that we want visible rather than silently swallowed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub items: u64,
    pub errors: u64,
    pub junk_bytes: u64,
    /// Items whose declared `length` disagreed with the decoded payload.
    pub length_mismatches: u64,
}

/// Incremental parser. Feed bytes, drain items.
///
/// ```
/// # use spmeta::Parser;
/// let mut p = Parser::new();
/// p.feed(b"<item><type>73736e63</type><code>70626567</code><length>0</length></item>");
/// let item = p.next_item().unwrap().unwrap();
/// assert_eq!(item.code.to_string(), "pbeg");
/// ```
pub struct Parser {
    buf: Vec<u8>,
    /// Read cursor into `buf`.
    pos: usize,
    /// Total bytes consumed before `buf[0]`, for absolute error offsets.
    base: u64,
    /// Bytes after `pos` already known not to begin the payload terminator.
    /// Lets a partially received payload resume its scan instead of
    /// restarting it on every call. Relative to `pos`, so compaction (which
    /// shifts both) leaves it valid.
    scan_hint: usize,
    limits: Limits,
    stats: Stats,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of attempting to parse one item at a fixed offset.
enum Attempt {
    /// Need more bytes; nothing consumed.
    Incomplete,
    Bad(Bad),
}

enum Bad {
    Reason(&'static str),
    Base64,
    TooLarge(usize),
}

impl Parser {
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    pub fn with_limits(limits: Limits) -> Self {
        Parser {
            buf: Vec::new(),
            pos: 0,
            base: 0,
            scan_hint: 0,
            limits,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Append bytes read from the pipe.
    pub fn feed(&mut self, data: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(data);
    }

    /// Buffered bytes not yet consumed: a partially received item, if any.
    ///
    /// Live readers can ignore this — the bytes will be consumed once the
    /// rest arrives. It exists for tools that reframe a finite stream and
    /// must not silently discard a truncated tail, which is exactly the
    /// interesting part of a capture that ends in a crash.
    pub fn remainder(&self) -> &[u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }

    /// Drop the already-consumed prefix so the buffer does not grow without
    /// bound over a long session.
    fn compact(&mut self) {
        if self.pos == 0 {
            return;
        }
        // Only pay for the memmove once the prefix is worth reclaiming.
        if self.pos >= 64 * 1024 || self.pos == self.buf.len() {
            self.buf.drain(..self.pos);
            self.base += self.pos as u64;
            self.pos = 0;
        }
    }

    fn offset_of(&self, idx: usize) -> u64 {
        self.base + idx as u64
    }

    /// Next item, or `None` when more input is needed.
    ///
    /// Errors are recoverable: the parser skips the offending byte and
    /// resynchronises on the following `<item>`, so a caller should log and
    /// keep polling rather than tear down the stream.
    #[allow(clippy::should_implement_trait)]
    pub fn next_item(&mut self) -> Option<Result<MetaItem, ParseError>> {
        // Find the next item start, treating anything before it as junk.
        let Some(rel) = find(&self.buf[self.pos..], ITEM_OPEN) else {
            // Retain a short tail: `<item>` may be split across feeds.
            let keep = ITEM_OPEN.len() - 1;
            let unparsed = self.buf.len() - self.pos;
            let skip = unparsed.saturating_sub(keep);
            if skip > 0 {
                let offset = self.offset_of(self.pos);
                let junk = &self.buf[self.pos..self.pos + skip];
                let significant = junk.iter().any(|b| !b.is_ascii_whitespace());
                self.pos += skip;
                self.scan_hint = 0;
                self.stats.junk_bytes += skip as u64;
                if significant {
                    self.stats.errors += 1;
                    return Some(Err(ParseError::Junk { offset, len: skip }));
                }
            }
            self.compact();
            return None;
        };

        if rel > 0 {
            let offset = self.offset_of(self.pos);
            let junk = &self.buf[self.pos..self.pos + rel];
            let significant = junk.iter().any(|b| !b.is_ascii_whitespace());
            self.pos += rel;
            self.scan_hint = 0;
            self.stats.junk_bytes += rel as u64;
            if significant {
                self.stats.errors += 1;
                return Some(Err(ParseError::Junk { offset, len: rel }));
            }
        }

        let start = self.pos;
        let mut hint = self.scan_hint;
        let attempt = parse_item_at(&self.buf, start, &self.limits, &mut hint);
        match attempt {
            Ok((item, consumed, mismatch)) => {
                self.pos = consumed;
                self.scan_hint = 0;
                self.stats.items += 1;
                if mismatch {
                    self.stats.length_mismatches += 1;
                }
                Some(Ok(item))
            }
            Err(Attempt::Incomplete) => {
                self.scan_hint = hint;
                let buffered = self.buf.len() - start;
                if buffered > self.limits.max_buffer_bytes {
                    // A declared length we can never satisfy, or a writer
                    // emitting something that is not this protocol at all.
                    self.pos = start + 1;
                    self.scan_hint = 0;
                    self.stats.errors += 1;
                    return Some(Err(ParseError::BufferOverflow {
                        max: self.limits.max_buffer_bytes,
                    }));
                }
                None
            }
            Err(Attempt::Bad(bad)) => {
                let offset = self.offset_of(start);
                // Step one byte so the next `<item>` search cannot latch
                // onto the same bad frame forever.
                self.pos = start + 1;
                self.scan_hint = 0;
                self.stats.errors += 1;
                Some(Err(match bad {
                    Bad::TooLarge(len) => ParseError::TooLarge {
                        offset,
                        len,
                        max: self.limits.max_item_bytes,
                    },
                    Bad::Base64 => ParseError::BadBase64 { offset },
                    Bad::Reason(reason) => ParseError::Malformed { offset, reason },
                }))
            }
        }
    }
}

/// Parse one item starting at `start`. Returns the item, the absolute index
/// just past it, and whether the declared length disagreed with reality.
fn parse_item_at(
    buf: &[u8],
    start: usize,
    limits: &Limits,
    hint: &mut usize,
) -> Result<(MetaItem, usize, bool), Attempt> {
    let mut c = Cursor { b: buf, i: start };

    c.tag(ITEM_OPEN)?;
    c.ws();
    let kind = FourCc::from_u32(c.hex_field(b"<type>", b"</type>")?);
    c.ws();
    let code = FourCc::from_u32(c.hex_field(b"<code>", b"</code>")?);
    c.ws();
    let declared = c.dec_field(b"<length>", b"</length>")?;
    if declared > limits.max_item_bytes {
        return Err(Attempt::Bad(Bad::TooLarge(declared)));
    }
    c.ws();

    // Zero-length items close immediately; anything else carries a data
    // block. This fork has to distinguish "not a close tag" from "the close
    // tag is still arriving", or every feed that lands here mid-tag looks
    // like corruption.
    const CLOSE: &[u8] = b"</item>";
    const DATA: &[u8] = b"<data";
    if c.choose(&[CLOSE, DATA])? == 0 {
        c.tag(CLOSE)?;
        let mismatch = declared != 0;
        return Ok((
            MetaItem {
                kind,
                code,
                payload: Vec::new(),
            },
            c.i,
            mismatch,
        ));
    }

    c.tag(DATA)?;
    // Skip attributes rather than matching them exactly: the encoding
    // attribute has been spelled more than one way across versions, and the
    // payload is unambiguously base64 either way.
    c.until(b">")?;
    c.tag(b">")?;

    // Resume the terminator scan where the last attempt gave up. Payloads
    // are the only unbounded field, so this is the only scan that matters.
    let raw = c.until_hinted(b"</data>", start, hint)?;
    c.tag(b"</data>")?;
    c.ws();
    c.tag(b"</item>")?;

    // Strip the line wrapping before decoding.
    let mut packed = Vec::with_capacity(raw.len());
    packed.extend(raw.iter().copied().filter(|b| !b.is_ascii_whitespace()));

    let payload = B64.decode(&packed).map_err(|_| Attempt::Bad(Bad::Base64))?;
    if payload.len() > limits.max_item_bytes {
        return Err(Attempt::Bad(Bad::TooLarge(payload.len())));
    }

    let mismatch = payload.len() != declared;
    Ok((
        MetaItem {
            kind,
            code,
            payload,
        },
        c.i,
        mismatch,
    ))
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a [u8] {
        &self.b[self.i.min(self.b.len())..]
    }

    /// Index of whichever alternative the input matches.
    ///
    /// Returns `Incomplete` when the input so far is a proper prefix of any
    /// alternative — that is a short read, not a syntax error.
    fn choose(&self, alts: &[&[u8]]) -> Result<usize, Attempt> {
        let rest = self.rest();
        if let Some(i) = alts.iter().position(|a| rest.starts_with(a)) {
            return Ok(i);
        }
        if alts.iter().any(|a| a.starts_with(rest)) {
            return Err(Attempt::Incomplete);
        }
        Err(Attempt::Bad(Bad::Reason("expected </item> or <data")))
    }

    fn tag(&mut self, tag: &[u8]) -> Result<(), Attempt> {
        let rest = self.rest();
        if rest.starts_with(tag) {
            self.i += tag.len();
            Ok(())
        } else if tag.starts_with(rest) {
            // A prefix match at end-of-buffer means the tag is split across
            // feeds, not that it is wrong.
            Err(Attempt::Incomplete)
        } else {
            Err(Attempt::Bad(Bad::Reason("unexpected tag")))
        }
    }

    fn ws(&mut self) {
        while let Some(b) = self.b.get(self.i) {
            if b.is_ascii_whitespace() {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// Consume up to (not including) `end`.
    fn until(&mut self, end: &[u8]) -> Result<&'a [u8], Attempt> {
        let rest = self.rest();
        match find(rest, end) {
            Some(n) => {
                let out = &rest[..n];
                self.i += n;
                Ok(out)
            }
            None => Err(Attempt::Incomplete),
        }
    }

    /// Like [`Cursor::until`], but skips re-scanning a prefix already known
    /// not to contain the terminator. `hint` is relative to `origin` and is
    /// advanced on failure.
    fn until_hinted(
        &mut self,
        end: &[u8],
        origin: usize,
        hint: &mut usize,
    ) -> Result<&'a [u8], Attempt> {
        // Back off by needle-1 so a terminator straddling the previous end
        // of buffer is still found.
        let resume = (origin + *hint).saturating_sub(end.len() - 1).max(self.i);
        let from = resume.min(self.b.len());
        match find(&self.b[from..], end) {
            Some(n) => {
                let abs = from + n;
                let out = &self.b[self.i..abs];
                self.i = abs;
                Ok(out)
            }
            None => {
                *hint = self.b.len().saturating_sub(origin);
                Err(Attempt::Incomplete)
            }
        }
    }

    fn hex_field(&mut self, open: &[u8], close: &[u8]) -> Result<u32, Attempt> {
        self.tag(open)?;
        let body = self.until(close)?;
        self.tag(close)?;
        let s = std::str::from_utf8(body)
            .map_err(|_| Attempt::Bad(Bad::Reason("non-utf8 hex field")))?;
        u32::from_str_radix(s.trim(), 16).map_err(|_| Attempt::Bad(Bad::Reason("bad hex field")))
    }

    fn dec_field(&mut self, open: &[u8], close: &[u8]) -> Result<usize, Attempt> {
        self.tag(open)?;
        let body = self.until(close)?;
        self.tag(close)?;
        let s =
            std::str::from_utf8(body).map_err(|_| Attempt::Bad(Bad::Reason("non-utf8 length")))?;
        s.trim()
            .parse::<usize>()
            .map_err(|_| Attempt::Bad(Bad::Reason("bad length")))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
