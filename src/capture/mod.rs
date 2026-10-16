//! Screen capture: source discovery, frame pacing and the recording session.
//!
//! Windows-only by construction: everything here is built on the Windows
//! Graphics Capture API through `windows-capture`.

pub mod dxgi;
pub mod handler;
pub mod pacer;
pub mod pipeline;
pub mod screenshot;
pub mod session;
pub mod sources;

pub use session::{
    PreviewFrame, Recorder, RecordingRequest, SessionShared, SessionSnapshot, SessionStatus,
};
pub use sources::{MonitorInfo, WindowInfo};

use serde::{Deserialize, Serialize};

/// Which OS capture API to use.
///
/// Graphics Capture can record single windows and is the default; Desktop
/// Duplication is a fallback for whole monitors when Graphics Capture is
/// unavailable (remote sessions, exclusive full-screen apps, restricted
/// tokens, older drivers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    /// Try Graphics Capture, fall back to DXGI for monitors.
    #[default]
    Auto,
    /// Windows Graphics Capture only.
    Wgc,
    /// DXGI Desktop Duplication only (monitors).
    Dxgi,
}

impl CaptureMethod {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Wgc, Self::Dxgi];

    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Wgc => "wgc",
            Self::Dxgi => "dxgi",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（优先图形捕获）",
            Self::Wgc => "图形捕获 WGC（可录窗口）",
            Self::Dxgi => "DXGI 桌面复制（仅整屏）",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "优先使用 Windows 图形捕获；被系统拒绝时自动改用 DXGI 桌面复制（仅整屏）",
            Self::Wgc => "Windows Graphics Capture，支持整屏与单个窗口",
            Self::Dxgi => "DXGI Desktop Duplication，兼容性更好，但只能录制整块屏幕",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "wgc" | "graphics" | "graphicscapture" => Some(Self::Wgc),
            "dxgi" | "duplication" => Some(Self::Dxgi),
            _ => None,
        }
    }

    /// True when the method cannot capture a single window.
    pub fn monitors_only(self) -> bool {
        matches!(self, Self::Dxgi)
    }
}

/// What to record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSelection {
    /// A whole display.
    Monitor { index: usize, name: String },
    /// A single top-level window.
    Window { hwnd: i64, title: String, process: String },
    /// Ask the user through the system capture picker.
    Picker,
}

impl SourceSelection {
    pub fn monitor(index: usize, name: impl Into<String>) -> Self {
        Self::Monitor { index, name: name.into() }
    }

    pub fn window(hwnd: i64, title: impl Into<String>, process: impl Into<String>) -> Self {
        Self::Window { hwnd, title: title.into(), process: process.into() }
    }

    /// Human readable description.
    pub fn label(&self) -> String {
        match self {
            Self::Monitor { index, name } => {
                if name.is_empty() {
                    format!("显示器 {index}")
                } else {
                    format!("显示器 {index}：{name}")
                }
            }
            Self::Window { title, process, .. } => {
                if process.is_empty() {
                    format!("窗口：{title}")
                } else {
                    format!("窗口：{title} （{process}）")
                }
            }
            Self::Picker => "由系统选择器选择".to_string(),
        }
    }

    /// Short label used in file names.
    pub fn file_label(&self) -> String {
        match self {
            Self::Monitor { index, .. } => format!("Display{index}"),
            Self::Window { title, process, .. } => {
                if title.trim().is_empty() {
                    process.clone()
                } else {
                    title.clone()
                }
            }
            Self::Picker => "PickedItem".to_string(),
        }
    }

    pub fn is_monitor(&self) -> bool {
        matches!(self, Self::Monitor { .. })
    }
}

// Re-exported so capture code can keep using `capture::Region`.
pub use crate::region::Region;
