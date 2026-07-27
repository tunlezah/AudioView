//! Fixture capture, packing and replay for shairport-sync metadata.
//!
//! Exposed as a library so the fixture tree can be verified from tests rather
//! than only from the CLI.

pub mod fifo;
pub mod fixture;
pub mod png;
pub mod synth;

pub use fixture::{
    canonicalise, default_art_dir, events, golden_path, pack, render_golden, unpack, GoldenEvent,
};
pub use synth::{all as synth_sessions, Session};

use spmeta::{MetaEvent, ParseError};

/// Render a raw stream to its golden-file text.
pub fn render_events(raw: &[u8]) -> String {
    render_golden(&events(raw)).expect("golden rendering is infallible for these types")
}

/// Render an already-decoded event sequence to the same text, so a
/// differently-chunked decode can be compared against a whole-buffer one.
pub fn render_event_results(results: &[Result<MetaEvent, ParseError>]) -> String {
    let golden: Vec<GoldenEvent> = results.iter().map(fixture::golden_from_result).collect();
    render_golden(&golden).expect("golden rendering is infallible for these types")
}
