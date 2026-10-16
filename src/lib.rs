//! ScreenRec — Windows screen recorder.
//!
//! * full-screen (per display) or single-window capture through the Windows
//!   Graphics Capture API,
//! * constant-frame-rate output with optional system audio / microphone,
//! * multi-format export: MP4/AVI/WMV/MPEG-2 with the built-in Media Foundation
//!   encoder, Matroska/WebM/QuickTime through FFmpeg when it is installed, and
//!   animated GIF plus PNG/JPEG image sequences entirely in-process,
//! * a configurable recording folder and file-name template.
//!
//! The binary has two front ends that share this library: an `egui` desktop
//! application (default) and a command line interface for automation.

pub mod audio;
pub mod capture;
pub mod cli;
pub mod config;
pub mod encode;
pub mod ffmpeg;
pub mod formats;
pub mod gui;
pub mod history;
pub mod hotkeys;
pub mod logging;
pub mod paths;
pub mod units;

pub use capture::sources::WindowInfo;

/// Version string reported by the UI and `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
