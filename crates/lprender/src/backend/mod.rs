//! Output backends.
//!
//! Each one supplies a GLES3 context and a framebuffer; the drawing itself is
//! identical across all of them, which is what makes the headless golden
//! images representative of what the Pi renders.

pub mod headless;

#[cfg(feature = "backend-drm")]
pub mod drm;

#[cfg(feature = "backend-sdl2")]
pub mod sdl;
