//! Colour conversion and dominant-colour extraction.
//!
//! Re-exported from `lpframe-image` rather than implemented here: `artd`'s
//! enrichment gate needs the identical OKLab conversion to decide whether a
//! candidate cover is the same picture, and it cannot depend on the renderer.
//! The tests for all of this live with the implementation.

pub use lpframe_image::color::{
    linear_to_oklab, linear_to_srgb, oklab_distance, oklab_to_linear, palette, srgb_to_linear,
    Palette,
};
