//! Enumerating the monitors and windows that can be recorded.

use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

/// A display that can be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// One-based capture index, i.e. the value accepted by
    /// `Monitor::from_index` and by `--monitor`.
    pub index: usize,
    /// Windows display number (`\\.\DISPLAYn`), shown for reference only.
    pub display_index: usize,
    pub name: String,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub is_primary: bool,
}

impl MonitorInfo {
    pub fn label(&self) -> String {
        format!(
            "显示器 {} · {}x{}{} ",
            self.index,
            self.width,
            self.height,
            if self.is_primary { " · 主显示器" } else { "" }
        )
    }

    pub fn describe(&self) -> String {
        let name = if self.name.trim().is_empty() {
            self.device_name.clone()
        } else {
            self.name.clone()
        };
        let mut text = format!(
            "{} — {}x{} @ {}Hz{}",
            name,
            self.width,
            self.height,
            self.refresh_rate,
            if self.is_primary { "（主）" } else { "" }
        );
        if self.display_index != self.index {
            text.push_str(&format!(" · 系统编号 DISPLAY{}", self.display_index));
        }
        text
    }
}

/// A top-level window that can be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: i64,
    pub title: String,
    pub process: String,
    pub pid: u32,
    pub width: i32,
    pub height: i32,
    pub is_foreground: bool,
}

impl WindowInfo {
    pub fn label(&self) -> String {
        format!("{} · {}x{} · {}", self.title, self.width, self.height, self.process)
    }

    /// Case-insensitive match used by the window filter box.
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        self.title.to_lowercase().contains(&needle) || self.process.to_lowercase().contains(&needle)
    }
}

/// Lists all displays, primary first, keeping the OS index for capture.
pub fn list_monitors() -> Vec<MonitorInfo> {
    let monitors = match Monitor::enumerate() {
        Ok(list) => list,
        Err(err) => {
            log::error!("枚举显示器失败: {err}");
            return Vec::new();
        }
    };

    let primary = Monitor::primary().ok();

    let mut infos: Vec<MonitorInfo> = monitors
        .into_iter()
        .enumerate()
        .map(|(position, monitor)| {
            let is_primary = primary.is_some_and(|p| p == monitor);
            MonitorInfo {
                // `Monitor::from_index` is one-based and positional, so the
                // capture index must be the enumeration position, not the
                // Windows display number.
                index: position + 1,
                display_index: monitor.index().unwrap_or(position + 1),
                name: monitor.name().unwrap_or_default(),
                device_name: monitor.device_name().unwrap_or_default(),
                width: monitor.width().unwrap_or(0),
                height: monitor.height().unwrap_or(0),
                refresh_rate: monitor.refresh_rate().unwrap_or(0),
                is_primary,
            }
        })
        .collect();

    // Present the primary monitor first: it is the common choice and the
    // default selection in the UI.
    infos.sort_by_key(|info| (!info.is_primary, info.index));
    infos
}

/// Lists capturable top-level windows, skipping our own process.
pub fn list_windows() -> Vec<WindowInfo> {
    list_windows_excluding(std::process::id())
}

/// Lists capturable top-level windows, skipping `exclude_pid`.
pub fn list_windows_excluding(exclude_pid: u32) -> Vec<WindowInfo> {
    let windows = match Window::enumerate() {
        Ok(list) => list,
        Err(err) => {
            log::error!("枚举窗口失败: {err}");
            return Vec::new();
        }
    };

    let foreground = Window::foreground().ok().map(|w| w.as_raw_hwnd() as i64);

    let mut infos: Vec<WindowInfo> = Vec::new();
    for window in windows {
        let pid = window.process_id().unwrap_or(0);
        if pid == exclude_pid {
            continue;
        }

        let title = window.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }

        let (width, height) = match (window.width(), window.height()) {
            (Ok(w), Ok(h)) => (w, h),
            _ => continue,
        };
        // Skip windows that are collapsed or effectively invisible.
        if width < 16 || height < 16 {
            continue;
        }

        let hwnd = window.as_raw_hwnd() as i64;
        let process = window
            .process_name()
            .unwrap_or_default()
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or_default()
            .to_string();

        infos.push(WindowInfo {
            hwnd,
            title,
            process,
            pid,
            width,
            height,
            is_foreground: foreground == Some(hwnd),
        });
    }

    // Foreground window first, then alphabetical — much easier to scan than the
    // Z-order `EnumWindows` happens to return.
    infos.sort_by(|a, b| {
        b.is_foreground
            .cmp(&a.is_foreground)
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    infos
}

/// Looks a window back up by handle, for "remember last source".
pub fn find_window(hwnd: i64) -> Option<WindowInfo> {
    list_windows().into_iter().find(|info| info.hwnd == hwnd)
}

/// Resolves a capture source against the current desktop.
pub fn resolve_monitor(index: usize) -> Option<MonitorInfo> {
    list_monitors().into_iter().find(|info| info.index == index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_filter_matches_title_and_process() {
        let info = WindowInfo {
            hwnd: 1,
            title: "记事本 - 未命名".into(),
            process: "notepad.exe".into(),
            pid: 42,
            width: 800,
            height: 600,
            is_foreground: false,
        };
        assert!(info.matches(""));
        assert!(info.matches("记事本"));
        assert!(info.matches("NOTEPAD"));
        assert!(!info.matches("计算器"));
    }

    #[test]
    fn monitor_labels_mention_the_primary_display() {
        let info = MonitorInfo {
            index: 1,
            display_index: 2,
            name: "DELL U2720Q".into(),
            device_name: r"\\.\DISPLAY1".into(),
            width: 3840,
            height: 2160,
            refresh_rate: 60,
            is_primary: true,
        };
        assert!(info.label().contains("主显示器"));
        assert!(info.describe().contains("3840x2160"));
        assert!(info.describe().contains("DISPLAY2"));
    }

    #[test]
    fn monitor_describe_hides_a_redundant_display_number() {
        let info = MonitorInfo {
            index: 1,
            display_index: 1,
            name: String::new(),
            device_name: r"\\.\DISPLAY1".into(),
            width: 1920,
            height: 1080,
            refresh_rate: 60,
            is_primary: false,
        };
        assert!(!info.describe().contains("系统编号"));
        assert!(info.describe().contains("DISPLAY1"));
    }
}
