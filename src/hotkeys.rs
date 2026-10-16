//! Global hotkeys via `RegisterHotKey`.
//!
//! A dedicated thread owns the registrations and pumps its own message queue;
//! the UI thread polls [`HotkeyManager::poll`] each frame, so no callback ever
//! touches application state from a foreign thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY};

/// Actions the application reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    StartStop,
    PauseResume,
    Screenshot,
}

impl HotkeyAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::StartStop => "开始 / 停止录制",
            Self::PauseResume => "暂停 / 继续",
            Self::Screenshot => "抓取截图",
        }
    }
}

/// A parsed hotkey combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub virtual_key: u32,
}

impl Hotkey {
    /// Parses strings such as `F9`, `Ctrl+Shift+R` or `Alt+F1`.
    pub fn parse(spec: &str) -> Option<Self> {
        let parts: Vec<&str> = spec
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }

        let mut modifiers = MOD_NOREPEAT.0;
        let mut key: Option<u32> = None;

        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
                "alt" => modifiers |= MOD_ALT.0,
                "shift" => modifiers |= MOD_SHIFT.0,
                "win" | "super" | "meta" => modifiers |= MOD_WIN.0,
                other => {
                    // Only one non-modifier key is allowed.
                    if key.is_some() {
                        return None;
                    }
                    key = Some(virtual_key(other)?);
                }
            }
        }

        Some(Self { modifiers, virtual_key: key? })
    }

    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.modifiers & MOD_CONTROL.0 != 0 {
            parts.push("Ctrl".to_string());
        }
        if self.modifiers & MOD_ALT.0 != 0 {
            parts.push("Alt".to_string());
        }
        if self.modifiers & MOD_SHIFT.0 != 0 {
            parts.push("Shift".to_string());
        }
        if self.modifiers & MOD_WIN.0 != 0 {
            parts.push("Win".to_string());
        }
        parts.push(key_name(self.virtual_key));
        parts.join("+")
    }
}

fn virtual_key(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();

    if let Some(rest) = lower.strip_prefix('f') {
        if let Ok(number) = rest.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0x70 + number - 1);
            }
        }
    }

    if lower.len() == 1 {
        let byte = lower.as_bytes()[0];
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            return Some(byte.to_ascii_uppercase() as u32);
        }
    }

    match lower.as_str() {
        "space" => Some(0x20),
        "enter" | "return" => Some(0x0D),
        "esc" | "escape" => Some(0x1B),
        "tab" => Some(0x09),
        "backspace" => Some(0x08),
        "insert" => Some(0x2D),
        "delete" | "del" => Some(0x2E),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "left" => Some(0x25),
        "up" => Some(0x26),
        "right" => Some(0x27),
        "down" => Some(0x28),
        "printscreen" => Some(0x2C),
        "pause" => Some(0x13),
        _ => None,
    }
}

fn key_name(virtual_key: u32) -> String {
    if (0x70..=0x87).contains(&virtual_key) {
        return format!("F{}", virtual_key - 0x70 + 1);
    }
    if (b'A' as u32..=b'Z' as u32).contains(&virtual_key) {
        return (virtual_key as u8 as char).to_string();
    }
    if (b'0' as u32..=b'9' as u32).contains(&virtual_key) {
        return (virtual_key as u8 as char).to_string();
    }
    match virtual_key {
        0x20 => "Space".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Esc".to_string(),
        0x09 => "Tab".to_string(),
        0x08 => "Backspace".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x24 => "Home".to_string(),
        0x23 => "End".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2C => "PrintScreen".to_string(),
        0x13 => "Pause".to_string(),
        other => format!("0x{other:02X}"),
    }
}

/// Owns the hotkey registrations.
pub struct HotkeyManager {
    registered: Vec<(i32, HotkeyAction)>,
    pending: Arc<Mutex<Vec<HotkeyAction>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HotkeyManager {
    /// Registers the requested bindings. Returns `None` when nothing could be
    /// registered (or the feature is disabled), which is not an error — the UI
    /// simply works without global shortcuts.
    pub fn start(bindings: &[(HotkeyAction, String)]) -> Option<Self> {
        let mut parsed: Vec<(i32, HotkeyAction, Hotkey)> = Vec::new();
        for (index, (action, spec)) in bindings.iter().enumerate() {
            if spec.trim().is_empty() {
                continue;
            }
            match Hotkey::parse(spec) {
                Some(hotkey) => parsed.push((index as i32 + 1, *action, hotkey)),
                None => log::warn!("无法解析快捷键“{spec}”（{action:?}）"),
            }
        }
        if parsed.is_empty() {
            return None;
        }

        let pending: Arc<Mutex<Vec<HotkeyAction>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Vec<(i32, HotkeyAction)>>();

        let thread_pending = Arc::clone(&pending);
        let thread_stop = Arc::clone(&stop);

        let handle = std::thread::Builder::new()
            .name("screenrec-hotkeys".to_string())
            .spawn(move || {
                let registered = register_all(&parsed);
                let _ = ready_tx.send(registered.clone());

                let mut message = MSG::default();
                while !thread_stop.load(Ordering::Relaxed) {
                    // Peek instead of Get so the stop flag is noticed promptly.
                    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                        if message.message == WM_HOTKEY {
                            let id = message.wParam.0 as i32;
                            if let Some((_, action)) =
                                registered.iter().find(|(registered_id, _)| *registered_id == id)
                            {
                                if let Ok(mut queue) = thread_pending.lock() {
                                    queue.push(*action);
                                }
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }

                for (id, _) in &registered {
                    let _ = unsafe { UnregisterHotKey(None, *id) };
                }
            })
            .ok()?;

        let registered = match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(list) => list,
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return None;
            }
        };

        if registered.is_empty() {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
            return None;
        }

        for (id, action) in &registered {
            log::info!("已注册全局快捷键 {id}: {action:?}");
        }

        Some(Self {
            registered,
            pending,
            stop,
            handle: Some(handle),
        })
    }

    /// Actions triggered since the last poll.
    pub fn poll(&self) -> Vec<HotkeyAction> {
        self.pending
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }

    pub fn registered_count(&self) -> usize {
        self.registered.len()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        self.stop();
    }
}

fn register_all(parsed: &[(i32, HotkeyAction, Hotkey)]) -> Vec<(i32, HotkeyAction)> {
    let mut registered = Vec::new();
    for (id, action, hotkey) in parsed {
        let result = unsafe {
            RegisterHotKey(
                None,
                *id,
                windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS(hotkey.modifiers),
                hotkey.virtual_key,
            )
        };
        match result {
            Ok(()) => registered.push((*id, *action)),
            Err(err) => log::warn!(
                "注册快捷键 {} 失败：{err}（可能已被其他程序占用）",
                hotkey.describe()
            ),
        }
    }
    registered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_keys_are_parsed() {
        let hotkey = Hotkey::parse("F9").unwrap();
        assert_eq!(hotkey.virtual_key, 0x78);
        assert_eq!(hotkey.modifiers & MOD_CONTROL.0, 0);
        assert_eq!(hotkey.describe(), "F9");
    }

    #[test]
    fn modifier_combinations_round_trip() {
        let hotkey = Hotkey::parse("ctrl+shift+r").unwrap();
        assert_eq!(hotkey.virtual_key, 'R' as u32);
        assert_ne!(hotkey.modifiers & MOD_CONTROL.0, 0);
        assert_ne!(hotkey.modifiers & MOD_SHIFT.0, 0);
        assert_eq!(hotkey.describe(), "Ctrl+Shift+R");
    }

    #[test]
    fn alt_and_win_modifiers_work() {
        let hotkey = Hotkey::parse("Win+Alt+F5").unwrap();
        assert_eq!(hotkey.describe(), "Alt+Win+F5");
    }

    #[test]
    fn named_keys_are_supported() {
        assert_eq!(Hotkey::parse("ctrl+space").unwrap().virtual_key, 0x20);
        assert_eq!(Hotkey::parse("pageup").unwrap().describe(), "PageUp");
        assert_eq!(Hotkey::parse("3").unwrap().virtual_key, '3' as u32);
    }

    #[test]
    fn invalid_specs_are_rejected() {
        assert!(Hotkey::parse("").is_none());
        assert!(Hotkey::parse("ctrl").is_none());
        assert!(Hotkey::parse("F99").is_none());
        assert!(Hotkey::parse("ctrl+a+b").is_none());
        assert!(Hotkey::parse("nosuchkey").is_none());
    }

    #[test]
    fn norepeat_is_always_set() {
        let hotkey = Hotkey::parse("F8").unwrap();
        assert_ne!(hotkey.modifiers & MOD_NOREPEAT.0, 0);
    }
}
