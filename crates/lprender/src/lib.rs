//! `lprender` — the LP Frame fullscreen artwork renderer.
//!
//! `deny` rather than `forbid`: the GL and EGL bindings are unsafe by
//! nature, and each call site carries a SAFETY note. Everything above the
//! graphics layer — layout, animation, colour, decoding — is safe Rust and
//! is where the behaviour is actually tested.
#![deny(unsafe_code)]

pub mod app;
pub mod backend;
pub mod color;
pub mod decode;
pub mod geometry;
pub mod gl;
pub mod scene;
