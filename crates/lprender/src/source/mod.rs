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
    /// What changed since the last call. `None` means nothing did.
    fn poll(&mut self, now_ms: u64) -> Option<Update>;
    /// The requested display power state.
    fn power(&self) -> DisplayPower {
        DisplayPower::On
    }
    /// When the source next wants attention, in monotonic milliseconds.
    fn next_wakeup_ms(&self, _now_ms: u64) -> Option<u64> {
        None
    }

    /// A file descriptor that becomes readable when this source has
    /// something new, so a render loop can block on it instead of polling.
    ///
    /// `None` means the source is purely clock-driven and
    /// [`Source::next_wakeup_ms`] is the only scheduling signal. Providing
    /// one is what lets an idle device do genuinely zero work rather than
    /// waking ten times a second to find nothing changed (DESIGN §6.3).
    ///
    /// The caller must only poll for readability; draining is the source's
    /// business and happens inside [`Source::poll`].
    fn wakeup_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    /// Whether this source is driven by the outside world rather than by the
    /// clock the caller passes in.
    ///
    /// The headless dump simulates time so a slideshow renders reproducibly.
    /// Against a live source that is wrong: it races through every frame in a
    /// fraction of a real second and dumps black, because nothing has arrived
    /// yet. A live source makes the caller pace in real time instead.
    fn is_live(&self) -> bool {
        false
    }
}

/// What the source wants on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Show this image.
    Show(Wanted),
    /// The track explicitly has no artwork — a zero-length `PICT`.
    ///
    /// Distinct from "nothing changed": leaving the previous album's cover up
    /// is worse than showing nothing, because it is confidently wrong.
    Clear,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A source with no fd must still schedule itself, and one with an fd
    /// must not ask to be polled — otherwise an idle device wakes on a timer
    /// for nothing, which is the behaviour the wakeup pipe exists to remove.
    #[test]
    fn a_source_either_provides_an_fd_or_a_wakeup_time() {
        struct Clockless;
        impl Source for Clockless {
            fn poll(&mut self, _now_ms: u64) -> Option<Update> {
                None
            }
        }
        let s = Clockless;
        assert_eq!(s.wakeup_fd(), None);
        assert_eq!(s.next_wakeup_ms(0), None);
        assert!(!s.is_live());
    }
}
