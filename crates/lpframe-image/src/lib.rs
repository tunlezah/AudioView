//! Image maths shared by the renderer and the enrichment gate.
//!
//! Two consumers with nothing else in common: `lprender` needs OKLab to pick
//! a background colour, and `artd` needs OKLab, a difference hash and a
//! colour histogram to decide whether a candidate cover is the same picture
//! as the AirPlay art. `artd` must not depend on `lprender` — the daemon has
//! no business linking EGL — so the maths lives in its own crate rather than
//! being duplicated or inverted.
//!
//! **Everything here is deterministic.** The renderer's golden images and the
//! enrichment gate's accept/reject decisions both depend on identical output
//! for identical input, on every platform: no randomness, no floating-point
//! reductions whose order varies, no dependence on a resampling filter that
//! might change between releases of an image library.

#![forbid(unsafe_code)]

pub mod color;
pub mod hash;
pub mod histogram;
pub mod resize;

pub use color::{
    linear_to_oklab, linear_to_srgb, oklab_distance, oklab_to_linear, palette, srgb_to_linear,
    Palette,
};
pub use hash::{dhash, hamming};
pub use histogram::{histogram, histogram_similarity, Histogram};
pub use resize::resize_box;
