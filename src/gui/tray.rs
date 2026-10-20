//! System tray icon and its menu.
//!
//! The tray runs on its own thread with a message-only window and its own
//! message pump, because egui/eframe owns the main event loop and does not
//! expose it. Menu clicks are forwarded to the GUI as [`TrayEvent`]s through a
//! channel that the app polls every frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// What the user picked in the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Bring the main window back.
    Show,
    /// Hide the main window (keep recording in the background).
    Hide,
    /// Start a recording with the current settings.
    Start,
    /// Stop the running recording.
    Stop,
    /// Quit the application.
    Exit,
}

/// Handle to the tray thread.
pub struct TrayHandle {
    events: Receiver<TrayEvent>,
    /// True while a recording is running, so the menu can enable the right item.
    recording: Arc<AtomicBool>,
    /// Set to ask the tray thread to leave its message loop.
    stop: Arc<AtomicBool>,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TrayHandle {
    /// Creates the tray icon.
    ///
    /// A failure here must not stop the application: the window still works
    /// without a tray, so the caller only logs the error.
    pub fn spawn() -> anyhow::Result<Self> {
        let (sender, events) = channel();
        let recording = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = channel::<anyhow::Result<u32>>();

        let thread_recording = Arc::clone(&recording);
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("screenrec-tray".to_string())
            .spawn(move || {
                if let Err(err) = run_tray(sender, thread_recording, thread_stop, ready_tx.clone()) {
                    let _ = ready_tx.send(Err(err));
                }
            })?;

        // Wait for the window (and its thread id) to exist before returning, so
        // the caller can be sure the icon is clickable.
        let thread_id = match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(err)) => {
                let _ = thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(anyhow::anyhow!("托盘线程启动超时"));
            }
        };

        Ok(Self {
            events,
            recording,
            stop,
            thread_id,
            thread: Some(thread),
        })
    }

    /// Returns the next pending tray event, if any.
    pub fn try_recv(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }

    /// Tells the tray whether a recording is running, so its menu can grey out
    /// "start" or "stop" accordingly.
    pub fn set_recording(&self, recording: bool) {
        self.recording.store(recording, Ordering::Relaxed);
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the message loop so it notices the flag.
        #[cfg(windows)]
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_TRAY_WAKE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// --------------------------------------------------------------- internals ----

#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// Posted to the tray thread to make it re-check the stop flag.
#[cfg(windows)]
const WM_TRAY_WAKE: u32 = WM_APP + 10;
/// Shell callback message for the icon.
#[cfg(windows)]
const WM_TRAY_CALLBACK: u32 = WM_APP + 11;

/// Menu command ids; they must be stable because Windows returns them directly.
#[cfg(windows)]
mod command {
    pub const SHOW: usize = 1;
    pub const HIDE: usize = 2;
    pub const START: usize = 3;
    pub const STOP: usize = 4;
    pub const EXIT: usize = 5;
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn run_tray(
    sender: Sender<TrayEvent>,
    recording: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ready: Sender<anyhow::Result<u32>>,
) -> anyhow::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
        NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NIF_SHOWTIP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DestroyWindow, DispatchMessageW, GetMessageW, GetWindowLongPtrW, LoadIconW, PostQuitMessage,
        RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage,
        UnregisterClassW, GWLP_USERDATA, IDI_APPLICATION,
        MF_SEPARATOR, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
        WINDOW_STYLE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP,
        WNDCLASSEXW,
    };

    /// Data the window procedure needs; stored in GWLP_USERDATA.
    struct TrayState {
        sender: Sender<TrayEvent>,
        recording: Arc<AtomicBool>,
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: the pointer was stored by this module (see WM_CREATE handling
        // below) and lives until the window is destroyed.
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayState;

        if message == WM_TRAY_CALLBACK && !state.is_null() {
            let state = unsafe { &*state };
            // NOTIFYICON_VERSION_4 packs the message into the low word of lparam.
            let notification = (lparam.0 & 0xFFFF) as u32;
            match notification {
                WM_LBUTTONDBLCLK => {
                    let _ = state.sender.send(TrayEvent::Show);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    unsafe { show_menu(hwnd, state) };
                }
                _ => {}
            }
            return LRESULT(0);
        }

        match message {
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    /// Builds and shows the context menu, forwarding the chosen item.
    unsafe fn show_menu(hwnd: HWND, state: &TrayState) {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let is_recording = state.recording.load(Ordering::Relaxed);

        let items: [(&str, usize, bool); 5] = [
            ("显示主窗口", command::SHOW, true),
            ("隐藏主窗口", command::HIDE, true),
            ("开始录制", command::START, !is_recording),
            ("停止录制", command::STOP, is_recording),
            ("退出", command::EXIT, true),
        ];
        for (index, (label, id, enabled)) in items.iter().enumerate() {
            if index == 4 {
                let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) };
            }
            let mut flags = MF_STRING;
            if !enabled {
                flags |= windows::Win32::UI::WindowsAndMessaging::MF_GRAYED;
            }
            let text = wide(label);
            let _ = unsafe { AppendMenuW(menu, flags, *id, PCWSTR(text.as_ptr())) };
        }

        // The classic dance: without setting the foreground window first, the
        // menu does not close when the user clicks elsewhere.
        let mut point = POINT::default();
        let _ = unsafe { GetCursorPos(&mut point) };
        let _ = unsafe { SetForegroundWindow(hwnd) };
        let chosen = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            )
        };
        // Dismiss the menu properly.
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
        let _ = unsafe { DestroyMenu(menu) };

        let event = match chosen.0 as usize {
            command::SHOW => Some(TrayEvent::Show),
            command::HIDE => Some(TrayEvent::Hide),
            command::START => Some(TrayEvent::Start),
            command::STOP => Some(TrayEvent::Stop),
            command::EXIT => Some(TrayEvent::Exit),
            _ => None,
        };
        if let Some(event) = event {
            let _ = state.sender.send(event);
        }
    }

    let instance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)? };
    let class_name = wide("ScreenRecTrayWindow");

    let window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&window_class) };

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("ScreenRec 托盘").as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            // Message-only window: never shown, never in the taskbar.
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?
    };

    let state = Box::into_raw(Box::new(TrayState {
        sender: sender.clone(),
        recording: Arc::clone(&recording),
    }));
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };

    // Our own icon, embedded in the executable as IDI_APPLICATION by build.rs.
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();

    let tooltip = wide("ScreenRec — 屏幕录制");
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    for (slot, ch) in data.szTip.iter_mut().zip(tooltip.iter()) {
        *slot = *ch;
    }

    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        // Version 4 gives the richer callback packing used above.
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) };
    } else {
        let os_error = std::io::Error::last_os_error();
        let _ = unsafe { DestroyWindow(hwnd) };
        // Typically "the taskbar/Explorer is not running in this session", which
        // is also why the sandboxed test environment cannot show a tray icon.
        return Err(anyhow::anyhow!(
            "Shell_NotifyIcon 添加托盘图标失败（{os_error}）；             如果没有运行资源管理器/任务栏，托盘图标不可用"
        ));
    }

    let _ = ready.send(Ok(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }));

    // Message loop.
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        if message.message == WM_TRAY_WAKE && stop.load(Ordering::Relaxed) {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // Clean up.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
    unsafe { drop(Box::from_raw(state)) };
    let _ = unsafe { DestroyWindow(hwnd) };
    let _ = unsafe { UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(instance.into())) };
    Ok(())
}
