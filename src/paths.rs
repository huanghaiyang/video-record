//! Output locations, file-name templates and collision-free naming.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, SecondsFormat};

/// Values available to a file-name template.
#[derive(Debug, Clone)]
pub struct TemplateContext {
    /// `2026-05-28`
    pub date: String,
    /// `14-05-33`
    pub time: String,
    /// `2026-05-28_14-05-33`
    pub datetime: String,
    /// Human readable source, already sanitised by the caller if needed.
    pub source: String,
    /// Moving picture experts group style frame count, mostly for debugging.
    pub frame_count: u64,
}

impl TemplateContext {
    pub fn now(source: impl Into<String>) -> Self {
        Self::at(Local::now(), source)
    }

    pub fn at(now: DateTime<Local>, source: impl Into<String>) -> Self {
        Self {
            date: now.format("%Y-%m-%d").to_string(),
            time: now.format("%H-%M-%S").to_string(),
            datetime: now.format("%Y-%m-%d_%H-%M-%S").to_string(),
            source: source.into(),
            frame_count: 0,
        }
    }
}

/// Default template used for new installations.
pub const DEFAULT_TEMPLATE: &str = "{date}_{time}_{source}";

/// Replaces every `{token}` in `template`.
///
/// Unknown tokens are left untouched so the user can see the mistake in the
/// resulting file name instead of silently losing text.
pub fn render_template(template: &str, ctx: &TemplateContext) -> String {
    let template = if template.trim().is_empty() { DEFAULT_TEMPLATE } else { template };
    let mut out = template
        .replace("{date}", &ctx.date)
        .replace("{time}", &ctx.time)
        .replace("{datetime}", &ctx.datetime)
        .replace("{source}", &ctx.source)
        .replace("{frames}", &ctx.frame_count.to_string());
    out = sanitize_component(&out);
    if out.trim().is_empty() {
        out = format!("recording_{}", ctx.time);
    }
    out
}

/// Makes a string safe to use as a single Windows path component.
pub fn sanitize_component(input: &str) -> String {
    const ILLEGAL: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let mut out: String = input
        .chars()
        .map(|c| if ILLEGAL.contains(&c) || (c as u32) < 0x20 { '_' } else { c })
        .collect();

    // Windows silently strips trailing dots and spaces, which breaks
    // uniqueness checks, so remove them ourselves.
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    out.truncate(120);
    out
}

/// Reserved device names on Windows.
fn is_reserved(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let upper = name.to_ascii_uppercase();
    RESERVED.contains(&upper.as_str())
}

/// Returns a path that does not exist yet by appending `_1`, `_2`, …
pub fn unique_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let stem = if is_reserved(stem) { format!("{stem}_") } else { stem.to_string() };
    let mut candidate = dir.join(format!("{stem}.{extension}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}_{counter}.{extension}"));
        counter += 1;
        if counter > 10_000 {
            break;
        }
    }
    candidate
}

/// A concrete place the recorder will write to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedOutput {
    /// File path for video/GIF, directory path for image sequences.
    pub path: PathBuf,
    /// True when each frame is written as its own file.
    pub is_sequence: bool,
}

impl PlannedOutput {
    pub fn file(path: PathBuf) -> Self {
        Self { path, is_sequence: false }
    }

    pub fn sequence(dir: PathBuf) -> Self {
        Self { path: dir, is_sequence: true }
    }

    /// Directory that receives the frames (or the parent of the output file).
    pub fn container_dir(&self) -> PathBuf {
        if self.is_sequence {
            self.path.clone()
        } else {
            self.path.parent().map(Path::to_path_buf).unwrap_or_default()
        }
    }

    /// Creates the directory tree the writer needs.
    pub fn prepare(&self) -> std::io::Result<()> {
        if self.is_sequence {
            std::fs::create_dir_all(&self.path)
        } else {
            match self.path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent),
                _ => Ok(()),
            }
        }
    }

    /// Path of the `index`-th frame for image-sequence output.
    pub fn frame_path(&self, index: u64, extension: &str) -> PathBuf {
        self.path.join(format!("frame_{index:06}.{extension}"))
    }

    pub fn display(&self) -> String {
        self.path.display().to_string()
    }

    /// Path handed to the OS file explorer.
    pub fn reveal_target(&self) -> PathBuf {
        self.container_dir()
    }
}

/// Default output folder: `%USERPROFILE%\Videos\ScreenRec`, falling back to
/// the current directory when the shell has no Videos folder.
pub fn default_output_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ScreenRec")
}

/// Path to hand to the file manager: the file itself when it exists, otherwise
/// its folder (so a deleted recording still opens somewhere useful).
pub fn reveal_target(path: &Path) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => path.to_path_buf(),
    }
}

/// Shows a file in the system file manager, selecting it when possible.
///
/// Implemented directly instead of through `opener::reveal`, which is gated
/// behind a feature that drags in a Linux-only D-Bus dependency.
pub fn reveal_in_file_manager(path: &Path) -> std::io::Result<()> {
    let target = reveal_target(path);

    #[cfg(windows)]
    {
        // `explorer /select,<path>` is what the shell itself uses. It reports a
        // non-zero exit code even when it succeeds, so it is spawned and not
        // waited on.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", target.display()))
            .spawn()
            .map(|_| ())
    }

    #[cfg(not(windows))]
    {
        opener::open(&target).map_err(|err| std::io::Error::other(err.to_string()))
    }
}

/// Checks that a folder exists and can actually be written to.
///
/// Creating a directory is not enough: a folder can exist yet be read-only, or
/// sit on a share that refuses new files.
pub fn is_writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;

    let probe = dir.join(".screenrec_write_test");
    std::fs::write(&probe, b"ok").map_err(|err| err.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Output folders to try, most preferred first.
///
/// Used when the configured folder is not writable (locked-down profile,
/// OneDrive-redirected Videos folder, a removed drive): recording should still
/// work instead of failing at the save step.
///
/// The order matters. Per-user locations come first, because both the process
/// and the media encoders can write there. The program directory and the
/// working directory come last: they are often read-only (Program Files) or
/// simply the wrong place to drop recordings.
pub fn output_dir_candidates(configured: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![configured.to_path_buf()];

    let home = dirs::home_dir();
    candidates.push(default_output_dir());
    if let Some(home) = &home {
        candidates.push(home.join("ScreenRec"));
    }
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join("ScreenRec").join("Recordings"));
    }
    candidates.push(std::env::temp_dir().join("ScreenRec"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("Recordings"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("recordings"));
    }

    // Keep the order but drop duplicates and empties.
    let mut unique: Vec<PathBuf> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if candidate.as_os_str().is_empty() || unique.contains(&candidate) {
            continue;
        }
        unique.push(candidate);
    }
    unique
}

/// First writable folder from [`output_dir_candidates`], skipping the first
/// entry (the caller's own choice, which already failed).
pub fn find_writable_fallback(configured: &Path) -> Option<PathBuf> {
    output_dir_candidates(configured)
        .into_iter()
        .skip(1)
        .find(|candidate| is_writable(candidate).is_ok())
}

/// Plans the output for a new recording, creating the folder if needed.
pub fn plan_output(
    output_dir: &Path,
    template: &str,
    source_label: &str,
    extension: &str,
    is_sequence: bool,
) -> std::io::Result<PlannedOutput> {
    std::fs::create_dir_all(output_dir)?;
    let ctx = TemplateContext::now(sanitize_component(source_label));
    let stem = render_template(template, &ctx);

    if is_sequence {
        let mut dir = output_dir.join(&stem);
        let mut counter = 1;
        while dir.exists() {
            dir = output_dir.join(format!("{stem}_{counter}"));
            counter += 1;
        }
        Ok(PlannedOutput::sequence(dir))
    } else {
        Ok(PlannedOutput::file(unique_path(output_dir, &stem, extension)))
    }
}

/// Timestamp string used in log/status messages.
pub fn timestamp_now() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Makes a window invisible to screen capture.
///
/// `WDA_EXCLUDEFROMCAPTURE` (Windows 10 2004 and later) removes the window from
/// Windows Graphics Capture and Desktop Duplication output entirely. Older builds
/// fall back to `WDA_MONITOR`, where the window is captured as a black rectangle
/// instead of leaking its contents into the recording.
#[cfg(windows)]
pub fn exclude_from_capture(hwnd: windows::Win32::Foundation::HWND) -> anyhow::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR,
    };

    // SAFETY: `hwnd` comes from `FindWindowW`/`GetHwnd` of a live window.
    unsafe {
        if SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok() {
            return Ok(());
        }
        SetWindowDisplayAffinity(hwnd, WDA_MONITOR)
            .map_err(|err| anyhow::anyhow!("SetWindowDisplayAffinity 失败：{err}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx() -> TemplateContext {
        let now = Local.with_ymd_and_hms(2026, 5, 28, 14, 5, 33).unwrap();
        TemplateContext::at(now, "Display 1")
    }

    #[test]
    fn template_tokens_are_expanded() {
        let c = ctx();
        assert_eq!(render_template("{date}", &c), "2026-05-28");
        assert_eq!(render_template("{time}", &c), "14-05-33");
        assert_eq!(
            render_template("{date}_{time}_{source}", &c),
            "2026-05-28_14-05-33_Display 1"
        );
    }

    #[test]
    fn empty_template_falls_back_to_default() {
        let out = render_template("   ", &ctx());
        assert!(out.starts_with("2026-05-28_14-05-33_"));
    }

    #[test]
    fn illegal_characters_are_replaced() {
        assert_eq!(sanitize_component("a<b>c:d\"e/f\\g|h?i*j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_component("trailing...   "), "trailing");
        assert!(!sanitize_component("x\n\ty").contains('\n'));
    }

    #[test]
    fn template_output_is_always_a_valid_component() {
        let c = TemplateContext::now("窗口: \"记事本\" / 1");
        let name = render_template("{source}", &c);
        assert!(!name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']));
        assert!(!name.is_empty());
    }

    #[test]
    fn unique_path_avoids_existing_files() {
        let dir = std::env::temp_dir().join(format!("screenrec_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let first = unique_path(&dir, "clip", "mp4");
        assert_eq!(first.file_name().unwrap(), "clip.mp4");
        std::fs::write(&first, b"x").unwrap();

        let second = unique_path(&dir, "clip", "mp4");
        assert_eq!(second.file_name().unwrap(), "clip_1.mp4");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reserved_names_are_escaped() {
        let dir = std::env::temp_dir();
        let p = unique_path(&dir, "CON", "mp4");
        assert_eq!(p.file_name().unwrap(), "CON_.mp4");
    }

    #[test]
    fn sequence_plan_creates_a_directory_target() {
        let dir = std::env::temp_dir().join(format!("screenrec_seq_{}", std::process::id()));
        let plan = plan_output(&dir, "frames", "Display 1", "png", true).unwrap();
        assert!(plan.is_sequence);
        assert_eq!(plan.container_dir(), plan.path);
        assert_eq!(
            plan.frame_path(7, "png").file_name().unwrap(),
            "frame_000007.png"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reveal_target_falls_back_to_the_parent_folder() {
        let dir = std::env::temp_dir().join(format!("screenrec_reveal_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"x").unwrap();

        // An existing file is selected directly.
        assert_eq!(reveal_target(&file), file);
        // A missing file resolves to its folder, so the user still gets there.
        assert_eq!(reveal_target(&dir.join("gone.mp4")), dir);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writability_check_accepts_temp_and_rejects_a_file_path() {        let dir = std::env::temp_dir().join(format!("screenrec_writable_{}", std::process::id()));
        assert!(is_writable(&dir).is_ok());

        // A path whose parent is a regular file can never be a directory.
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(is_writable(&file.join("sub")).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn candidates_start_with_the_configured_folder_and_are_unique() {
        let configured = PathBuf::from(r"Z:\definitely\missing");
        let candidates = output_dir_candidates(&configured);
        assert_eq!(candidates.first(), Some(&configured));

        let mut seen = std::collections::HashSet::new();
        for candidate in &candidates {
            assert!(
                seen.insert(candidate.clone()),
                "duplicate candidate: {}",
                candidate.display()
            );
            assert!(!candidate.as_os_str().is_empty());
        }
    }

    #[test]
    fn a_writable_fallback_is_found_when_the_configured_folder_is_unusable() {
        let dir = std::env::temp_dir().join(format!("screenrec_fallback_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();

        // `file/sub` cannot be created, so a fallback must be proposed.
        let fallback = find_writable_fallback(&file.join("sub"));
        assert!(fallback.is_some(), "expected a writable fallback");
        assert_ne!(fallback.unwrap(), file.join("sub"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
