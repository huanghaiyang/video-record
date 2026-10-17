//! Console plumbing for a GUI-subsystem executable.
//!
//! The executable is linked as a Windows GUI binary so that double-clicking it
//! never opens a console window. The side effect is that Windows does not give
//! the process a console at all — not even when it is started from a terminal —
//! which would silence command-line mode. CLI mode therefore re-attaches to the
//! parent console before anything is printed.

/// Re-attaches the process to the console it was started from, when there is
/// one, and rebinds the standard output/error handles to it.
///
/// Must be called before anything is printed: Rust's `stdout`/`stderr` pick up
/// the standard handles lazily, on first use.
///
/// When there is no parent console (a double-click, a service, or a caller that
/// redirected the streams through a pipe) the inherited handles keep working,
/// so this is a no-op.
#[cfg(windows)]
pub fn attach_parent_console() {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    /// Opens a console device, e.g. `CONOUT$`.
    fn open_console(device: &str) -> Option<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .ok()
    }

    // SAFETY: the handles passed to `SetStdHandle` stay valid because the `File`
    // owning them is leaked deliberately, for the lifetime of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }

        if let Some(console) = open_console("CONOUT$") {
            // `RawHandle` is already a `*mut c_void`, which is what HANDLE wraps.
            let handle = HANDLE(console.as_raw_handle());
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
            std::mem::forget(console);
        }
    }
}

/// Non-Windows builds are console applications, so nothing is needed.
#[cfg(not(windows))]
pub fn attach_parent_console() {}

/// True when the process currently has a console window (or is attached to one).
///
/// Used only to decide whether printing is meaningful, e.g. for diagnostics that
/// must not panic when the application was started from Explorer.
#[cfg(windows)]
pub fn has_console() -> bool {
    use windows::Win32::System::Console::GetConsoleWindow;
    !unsafe { GetConsoleWindow() }.is_invalid()
}

#[cfg(not(windows))]
pub fn has_console() -> bool {
    true
}
