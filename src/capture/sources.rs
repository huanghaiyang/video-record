//! Enumerating the monitors and windows that can be recorded.

use windows_capture::monitor::Monitor;
use windows_capture::window::Window;

use crate::region::Region;

/// A monitor as GDI sees it: its position on the virtual desktop and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorRect {
    /// Device name, e.g. `\\.\DISPLAY1`.
    pub device_name: String,
    /// Left edge on the virtual desktop, in physical pixels (may be negative).
    pub x: i32,
    /// Top edge on the virtual desktop, in physical pixels (may be negative).
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

impl MonitorRect {
    /// Converts two virtual-desktop corners into a region relative to this
    /// monitor, clamped so the result never leaves it.
    pub fn region_from_corners(&self, from: (i32, i32), to: (i32, i32)) -> Region {
        if self.width == 0 || self.height == 0 {
            return Region::new(0, 0, 0, 0);
        }

        // Clamp the corners first: clamping only the origin afterwards would
        // let a drag that starts outside the monitor produce an oversized
        // region.
        let clamp_x = |value: i32| value.clamp(self.x, self.x + self.width as i32);
        let clamp_y = |value: i32| value.clamp(self.y, self.y + self.height as i32);

        let (ax, ay) = (clamp_x(from.0), clamp_y(from.1));
        let (bx, by) = (clamp_x(to.0), clamp_y(to.1));

        Region {
            x: (ax.min(bx) - self.x) as u32,
            y: (ay.min(by) - self.y) as u32,
            width: (ax - bx).unsigned_abs(),
            height: (ay - by).unsigned_abs(),
        }
    }

    /// True when a virtual-desktop point lies inside this monitor.
    pub fn contains(&self, point: (i32, i32)) -> bool {
        point.0 >= self.x
            && point.0 < self.x + self.width as i32
            && point.1 >= self.y
            && point.1 < self.y + self.height as i32
    }
}

/// Current cursor position in virtual-desktop physical pixels.
///
/// Physical pixels are what `Region` needs, so reading the cursor directly
/// avoids the logical/DPI conversion a UI toolkit would apply.
pub fn cursor_position() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    match unsafe { GetCursorPos(&mut point) } {
        Ok(()) => Some((point.x, point.y)),
        Err(err) => {
            log::warn!("读取鼠标位置失败: {err}");
            None
        }
    }
}

/// Monitor containing a virtual-desktop point, falling back to the first one.
pub fn monitor_at(point: (i32, i32)) -> Option<MonitorRect> {
    let rects = monitor_rects();
    rects
        .iter()
        .find(|rect| rect.contains(point))
        .or_else(|| rects.first())
        .cloned()
}

/// Lists monitors through GDI, which is the only source of their positions.
///
/// `windows-capture` reports sizes and names but not where a monitor sits, and
/// area recording needs the origin to translate screen coordinates.
pub fn monitor_rects() -> Vec<MonitorRect> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };

    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<MonitorRect>) };
        // GetMonitorInfoW fills an EX structure when cbSize says so, which is
        // the only way to also learn the device name.
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            let end = info.szDevice.iter().position(|c| *c == 0).unwrap_or(info.szDevice.len());
            let rect = info.monitorInfo.rcMonitor;
            list.push(MonitorRect {
                device_name: String::from_utf16_lossy(&info.szDevice[..end]),
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
                is_primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
        TRUE
    }

    let mut list: Vec<MonitorRect> = Vec::new();
    // SAFETY: the callback only pushes into `list`, which outlives the call.
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM((&mut list as *mut Vec<MonitorRect>) as isize),
        )
    };

    if !ok.as_bool() {
        log::warn!("枚举显示器位置失败");
    }
    list.sort_by_key(|monitor| (!monitor.is_primary, monitor.x, monitor.y));
    list
}

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
    /// Top-left corner on the virtual desktop, in physical pixels. `(0, 0)`
    /// when GDI could not be queried.
    pub position: (i32, i32),
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
            "{} — {}x{} @ {}Hz{} · 桌面位置 ({}, {})",
            name,
            self.width,
            self.height,
            self.refresh_rate,
            if self.is_primary { "（主）" } else { "" },
            self.position.0,
            self.position.1
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
                position: (0, 0),
            }
        })
        .collect();

    // Present the primary monitor first: it is the common choice and the
    // default selection in the UI.
    infos.sort_by_key(|info| (!info.is_primary, info.index));

    // Attach the desktop position from GDI: `windows-capture` reports sizes and
    // names but not where a monitor sits, and area recording needs the origin.
    let rects = monitor_rects();
    for info in &mut infos {
        let matched = rects
            .iter()
            .find(|rect| !rect.device_name.is_empty() && rect.device_name == info.device_name)
            .or_else(|| {
                rects
                    .iter()
                    .find(|rect| rect.width == info.width && rect.height == info.height)
            });
        if let Some(rect) = matched {
            info.position = (rect.x, rect.y);
        }
    }

    // The primary flag from GDI is authoritative when names did not match.
    if let Some(primary) = rects.iter().find(|rect| rect.is_primary) {
        if let Some(info) = infos
            .iter_mut()
            .find(|info| (info.width, info.height) == (primary.width, primary.height))
        {
            info.is_primary = true;
        }
    }

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
            position: (0, 0),
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
            position: (0, 0),
        };
        assert!(!info.describe().contains("系统编号"));
        assert!(info.describe().contains("DISPLAY1"));
    }

    #[test]
    fn monitor_positions_are_readable() {
        // Every Windows session reports at least one monitor; the point is that
        // the GDI call succeeds and returns sane geometry.
        let rects = monitor_rects();
        if rects.is_empty() {
            eprintln!("no monitors reported (acceptable in a headless session)");
            return;
        }
        assert!(
            rects.iter().all(|rect| rect.width > 0 && rect.height > 0),
            "every monitor must have a non-zero size"
        );
        assert_eq!(
            rects.iter().filter(|rect| rect.is_primary).count(),
            1,
            "exactly one monitor is primary"
        );
    }

    #[test]
    fn corners_become_a_region_relative_to_the_monitor() {
        let monitor = MonitorRect {
            device_name: r"\\.\DISPLAY2".to_string(),
            x: 1920,
            y: -200,
            width: 2560,
            height: 1440,
            is_primary: false,
        };

        // Dragged left-to-right, top-to-bottom.
        assert_eq!(
            monitor.region_from_corners((2020, 100), (2500, 500)),
            Region::new(100, 300, 480, 400)
        );
        // Dragged the other way round gives the same region.
        assert_eq!(
            monitor.region_from_corners((2500, 500), (2020, 100)),
            Region::new(100, 300, 480, 400)
        );
        // A drag starting left/above the monitor is clamped into it.
        assert_eq!(
            monitor.region_from_corners((1800, -400), (2000, 0)),
            Region::new(0, 0, 80, 200)
        );

        assert!(monitor.contains((1920, -200)));
        assert!(monitor.contains((4479, 1239)));
        assert!(!monitor.contains((1919, 0)));
        assert!(!monitor.contains((1920, 1241)));
    }
}
