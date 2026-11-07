//! JustPDF: a minimal, fast PDF reader for Android.
//!
//! Architecture (see CLAUDE.md for the full picture):
//! - `android`: JNI entry points + render thread (EGL, frame loop).
//! - `app`: platform-independent state machine (input, view, tiles, UI).
//! - `worker`: PDFium thread with a priority queue (tiles, search, outline).
//! - `pdfium`: dlopen'ed PDFium bindings.
//! - `gl`/`egl`: OpenGL ES 3 backend; `gfx`: backend-neutral drawing API.

pub mod log;

pub mod app;
pub mod gesture;
pub mod gfx;
pub mod layout;
pub mod pdfium;
pub mod search;
pub mod store;
pub mod text;
pub mod tiles;
pub mod ui;
pub mod worker;

#[cfg(any(target_os = "android", feature = "headless"))]
pub mod egl;
#[cfg(any(target_os = "android", feature = "headless"))]
pub mod gl;

#[cfg(target_os = "android")]
mod android;
