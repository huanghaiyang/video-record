//! Minimal file-based logger.
//!
//! A GUI application has no console, so logging goes to
//! `%APPDATA%\ScreenRec\logs\screenrec.log` (falling back to the executable's
//! directory). When a console *is* attached — the CLI mode, or a debug run —
//! records are mirrored to stderr as well.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Metadata, Record};

struct FileLogger {
    sink: Mutex<Option<File>>,
    mirror_stderr: bool,
}

impl log::Log for FileLogger {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        let line = format!(
            "{} [{:<5}] {} — {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );

        if self.mirror_stderr {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }

        if let Ok(mut guard) = self.sink.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.sink.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

static LOGGER: OnceLock<FileLogger> = OnceLock::new();

/// Directory that holds log files.
pub fn log_dir() -> PathBuf {
    crate::config::app_data_dir().join("logs")
}

/// Installs the global logger.
///
/// Failure to open the log file is never fatal: the recorder must still work
/// from a read-only or restricted location, so records then go to stderr only.
pub fn init(level: LevelFilter, mirror_stderr: bool) -> anyhow::Result<PathBuf> {
    let dir = log_dir();
    let path = dir.join("screenrec.log");

    let file = match open_log_file(&dir, &path) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!("警告：无法写入日志文件 {}（{err}），仅输出到控制台", path.display());
            None
        }
    };

    match LOGGER.get() {
        Some(logger) => {
            if let Ok(mut guard) = logger.sink.lock() {
                *guard = file;
            }
        }
        None => {
            let logger = FileLogger { sink: Mutex::new(file), mirror_stderr };
            let _ = LOGGER.set(logger);
            if let Some(logger) = LOGGER.get() {
                let _ = log::set_logger(logger);
            }
        }
    }

    log::set_max_level(level);
    Ok(path)
}

/// Creates the log directory and opens (rotating) the log file.
fn open_log_file(dir: &Path, path: &Path) -> std::io::Result<File> {
    fs::create_dir_all(dir)?;

    // Keep the previous run's log around; it is usually the interesting one.
    if path.exists() {
        let _ = fs::rename(path, dir.join("screenrec.prev.log"));
    }

    OpenOptions::new().create(true).append(true).open(path)
}

/// Trims the log file when it grows past a few megabytes.
pub fn rotate_if_large(max_bytes: u64) {
    let path = log_dir().join("screenrec.log");
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > max_bytes {
            let _ = fs::rename(&path, log_dir().join("screenrec.prev.log"));
        }
    }
}

/// Convenience for reporting a failure without panicking.
pub fn log_error(context: &str, error: &dyn std::fmt::Display) {
    log::error!("{context}: {error}");
}

/// True when the process owns a visible console window.
pub fn has_console() -> bool {
    use windows::Win32::System::Console::GetConsoleWindow;
    let window = unsafe { GetConsoleWindow() };
    !window.0.is_null()
}

/// Sets up logging for a GUI run (stderr mirror only when a console exists).
pub fn init_for_gui() -> Option<PathBuf> {
    let mirror = has_console();
    match init(LevelFilter::Debug, mirror) {
        Ok(path) => {
            log::info!("=== ScreenRec started (log: {}) ===", path.display());
            Some(path)
        }
        Err(_) => None,
    }
}

/// Path of the log file currently in use.
pub fn current_log_path() -> PathBuf {
    log_dir().join("screenrec.log")
}

/// Ensures the parent directory of `path` exists.
pub fn ensure_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir),
        _ => Ok(()),
    }
}
