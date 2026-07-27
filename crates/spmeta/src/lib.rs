//! Parser for the shairport-sync metadata pipe.
//!
//! Two layers:
//!
//! * [`Parser`] handles framing — it turns a byte stream into [`MetaItem`]s
//!   and resynchronises after damage.
//! * [`Decoder`] adds payload decoding, producing [`MetaEvent`]s.
//!
//! Both are incremental and make no assumptions about chunk boundaries, which
//! matters because a FIFO delivers whatever the writer happened to flush.
//!
//! ```
//! use spmeta::{Decoder, MetaEvent};
//!
//! let mut d = Decoder::new();
//! d.feed(b"<item><type>73736e63</type><code>61626567</code><length>0</length></item>");
//! assert_eq!(d.next_event().unwrap().unwrap(), MetaEvent::ActiveBegin);
//! ```

#![forbid(unsafe_code)]

pub mod codes;
pub mod dmap;
mod event;
mod fourcc;
mod parser;

pub use event::{decode_item, CoreField, Decoder, MetaEvent};
pub use fourcc::FourCc;
pub use parser::{Limits, MetaItem, ParseError, Parser, Stats};

use base64::Engine;

/// Serialise an item in shairport-sync's wire format.
///
/// Used to build fixtures and by `lpcapture` when rehydrating externalised
/// artwork. Kept in the library so that encode/parse round-tripping is a
/// property we can test rather than an assumption.
pub fn encode_item(kind: FourCc, code: FourCc, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() * 4 / 3 + 128);
    out.extend_from_slice(
        format!(
            "<item><type>{:08x}</type><code>{:08x}</code><length>{}</length>",
            kind.as_u32(),
            code.as_u32(),
            payload.len()
        )
        .as_bytes(),
    );
    if payload.is_empty() {
        out.extend_from_slice(b"</item>\n");
        return out;
    }
    out.extend_from_slice(b"\n<data encoding=\"base64\">\n");

    let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
    // shairport-sync line-wraps; matching that keeps fixtures realistic and
    // exercises the parser's whitespace handling.
    for chunk in b64.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.push(b'\n');
    }
    out.extend_from_slice(b"</data></item>\n");
    out
}
