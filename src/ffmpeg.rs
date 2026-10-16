//! Locating a usable FFmpeg installation.
//!
//! FFmpeg is optional: the built-in Media Foundation encoder covers MP4, AVI,
//! WMV and MPEG-2, and GIF / image sequences are produced in-process. FFmpeg
//! unlocks Matroska, WebM, QuickTime and the software codecs.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegInfo {
    pub path: PathBuf,
    /// First line of `ffmpeg -version`, e.g. `ffmpeg version 7.1 ...`.
    pub version: String,
}

impl FfmpegInfo {
    pub fn summary(&self) -> String {
        format!("{} ({})", self.version.trim(), self.path.display())
    }
}

/// File name of the FFmpeg executable on this platform.
pub fn executable_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

/// Locations searched, in priority order.
pub fn candidates(configured: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let exe_name = executable_name();

    if !configured.as_os_str().is_empty() {
        paths.push(configured.to_path_buf());
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Alongside the binary, and in the conventional sub-folders a user
            // gets when unzipping a static build.
            paths.push(dir.join(exe_name));
            paths.push(dir.join("ffmpeg").join("bin").join(exe_name));
            paths.push(dir.join("tools").join("ffmpeg").join("bin").join(exe_name));

            // While developing, the binary lives in target/<profile>, so walk up
            // looking for a repository-local `.tools` folder.
            let mut current = Some(dir);
            for _ in 0..4 {
                let Some(base) = current else { break };
                paths.push(base.join(".tools").join("ffmpeg").join("bin").join(exe_name));
                paths.push(base.join("tools").join("ffmpeg").join("bin").join(exe_name));
                current = base.parent();
            }
        }
    }

    paths.push(PathBuf::from(exe_name));
    paths
}

/// Runs `ffmpeg -version` to confirm the binary works.
pub fn probe(path: &Path) -> Option<FfmpegInfo> {
    let mut command = Command::new(path);
    command.arg("-hide_banner").arg("-version");
    hide_console(&mut command);

    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .lines()
        .find(|line| line.to_lowercase().contains("ffmpeg version"))
        .unwrap_or_else(|| text.lines().next().unwrap_or("ffmpeg"))
        .to_string();

    Some(FfmpegInfo { path: path.to_path_buf(), version })
}

/// Returns the first candidate that runs, if any.
pub fn detect(configured: &Path) -> Option<FfmpegInfo> {
    for candidate in candidates(configured) {
        if let Some(info) = probe(&candidate) {
            log::info!("找到 FFmpeg: {}", info.summary());
            return Some(info);
        }
    }
    log::info!("未找到 FFmpeg，将只提供内置编码器支持的格式");
    None
}

/// List of encoders the loaded FFmpeg build provides.
pub fn list_encoders(info: &FfmpegInfo) -> Vec<String> {
    let mut command = Command::new(&info.path);
    command.args(["-hide_banner", "-loglevel", "error", "-encoders"]);
    hide_console(&mut command);

    let Ok(output) = command.output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let flags = parts.next()?;
            let name = parts.next()?;
            if flags.len() == 6 && flags.starts_with('V') {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_include_the_configured_path_first() {
        let configured = PathBuf::from(r"C:\tools\ffmpeg\bin\ffmpeg.exe");
        let list = candidates(&configured);
        assert_eq!(list.first(), Some(&configured));
        assert!(list.len() > 1);
    }

    #[test]
    fn candidates_always_end_with_the_plain_command() {
        let list = candidates(Path::new(""));
        assert!(list.iter().any(|p| p == Path::new(executable_name())));
    }

    #[test]
    fn probing_a_missing_binary_is_none() {
        assert!(probe(Path::new("definitely-not-ffmpeg-12345")).is_none());
    }
}
