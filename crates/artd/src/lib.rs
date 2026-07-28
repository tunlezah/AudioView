//! `artd` — metadata state machine, artwork staging, and IPC.
//!
//! Exposed as a library so the state machine can be replayed against fixtures
//! from integration tests (DESIGN §9).

// `deny` rather than `forbid`: the FIFO reader needs two libc calls, each
// individually opted in with a SAFETY note. Everything else stays safe.
#![deny(unsafe_code)]

pub mod artwork;
pub mod enrich;
pub mod hub;
pub mod ipc;
pub mod machine;
pub mod pipe;
pub mod runtime;
pub mod web;

use time::format_description::well_known::Rfc3339;

/// Wall-clock timestamp for published messages.
///
/// Only ever used for display and logging. Every ordering and timeout
/// decision uses the monotonic clock instead, so a clock step — an NTP
/// correction shortly after boot, say — cannot reorder state or fire a timer
/// early.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}
