//! Where images come from.
//!
//! The renderer is dumb by design (DESIGN §3.1): it is told "show this file"
//! and "go to sleep", and every policy decision — which track, which
//! artwork, when to blank — lives on the other side of this trait. That is
//! what lets the whole render path run from a directory of images with no
//! daemon at all, and what keeps `artd` restartable underneath a running
//! renderer.

pub mod artd;
pub mod slideshow;

pub use artd::ArtdSource;
pub use slideshow::Slideshow;

use std::path::PathBuf;

use lpframe_proto::DisplayPower;

/// Where images come from.
pub trait Source {
    /// The next image to show, if it has changed. `None` means no change.
    fn poll(&mut self, now_ms: u64) -> Option<Wanted>;
    /// The requested display power state.
    fn power(&self) -> DisplayPower {
        DisplayPower::On
    }
    /// When the source next wants attention, in monotonic milliseconds.
    fn next_wakeup_ms(&self, _now_ms: u64) -> Option<u64> {
        None
    }
}

/// An image the source would like shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// Stable identity. A repeat of the current id is ignored.
    pub id: u64,
    pub path: PathBuf,
    /// Use the shorter crossfade: the same album, just sharper.
    pub is_upgrade: bool,
}
