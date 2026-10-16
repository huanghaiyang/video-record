//! Persistent application configuration (`%APPDATA%\ScreenRec\config.json`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::CaptureMethod;
use crate::formats::{
    self, AudioCodec, Container, ExportFormat, FormatError, QualityPreset, ScaleOption, VideoCodec,
};
use crate::paths;

pub const SCHEMA_VERSION: u32 = 1;

/// Folder that holds configuration, logs and crash reports.
///
/// Resolution order:
///   1. `SCREENREC_DATA_DIR` — explicit override, handy for automation and for
///      running the recorder from a read-only or portable location.
///   2. `ScreenRec.portable` next to the executable — portable mode.
///   3. `%APPDATA%\ScreenRec`.
pub fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SCREENREC_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let marker = dir.join("ScreenRec.portable");
            if marker.exists() {
                return dir.join("ScreenRecData");
            }
        }
    }

    dirs::config_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ScreenRec")
}

pub fn config_path() -> PathBuf {
    app_data_dir().join("config.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
            Self::System => "跟随系统",
        }
    }
}

/// Where audio comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Master switch for audio in the export.
    pub enabled: bool,
    /// Capture what the machine is playing (WASAPI loopback).
    pub system: bool,
    /// Capture the default recording device.
    pub microphone: bool,
    /// Substring of the microphone device name; empty means "system default".
    pub microphone_device: String,
    /// Gain applied to the loopback signal.
    pub system_volume: f32,
    /// Gain applied to the microphone signal.
    pub microphone_volume: f32,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            system: true,
            microphone: false,
            microphone_device: String::new(),
            system_volume: 1.0,
            microphone_volume: 1.0,
            codec: AudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
        }
    }
}

impl AudioConfig {
    /// True when at least one source is enabled.
    pub fn is_active(&self) -> bool {
        self.enabled && (self.system || self.microphone)
    }

    pub fn has_system(&self) -> bool {
        self.is_active() && self.system
    }

    pub fn has_microphone(&self) -> bool {
        self.is_active() && self.microphone
    }
}

/// The capture source remembered between sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedSource {
    Monitor { index: usize, name: String },
    Window { hwnd: i64, title: String },
}

/// Everything the application persists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub schema_version: u32,

    // ---- output ----
    pub output_dir: PathBuf,
    pub filename_template: String,
    pub open_folder_when_done: bool,

    // ---- video ----
    pub container: Container,
    pub video_codec: VideoCodec,
    pub quality: QualityPreset,
    pub custom_bitrate_kbps: u32,
    pub frame_rate: u32,
    pub scale: ScaleOption,
    /// Extra width ceiling for the output; 0 disables it.
    pub max_output_width: u32,
    pub prefer_hardware_encoder: bool,

    // ---- audio ----
    pub audio: AudioConfig,

    // ---- capture behaviour ----
    pub capture_cursor: bool,
    pub capture_border: bool,
    pub include_secondary_windows: bool,
    /// 0 = derive from the frame rate.
    pub min_update_interval_ms: u32,
    pub countdown_seconds: u32,
    /// Which OS capture API to use.
    pub capture_method: CaptureMethod,
    /// Feed a synthetic test pattern instead of capturing the screen.
    pub test_pattern: bool,

    // ---- hotkeys ----
    pub hotkeys_enabled: bool,
    pub hotkey_start_stop: String,
    pub hotkey_pause: String,
    pub hotkey_screenshot: String,

    // ---- ui ----
    pub theme: ThemeMode,
    pub show_preview: bool,
    pub remember_last_source: bool,
    pub last_source: Option<SavedSource>,

    // ---- external tools ----
    /// Empty means "detect automatically".
    pub ffmpeg_path: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            output_dir: paths::default_output_dir(),
            filename_template: paths::DEFAULT_TEMPLATE.to_string(),
            open_folder_when_done: false,
            container: Container::Mp4,
            video_codec: VideoCodec::H264,
            quality: QualityPreset::Medium,
            custom_bitrate_kbps: 8_000,
            frame_rate: 30,
            scale: ScaleOption::Original,
            max_output_width: 0,
            prefer_hardware_encoder: false,
            audio: AudioConfig::default(),
            capture_cursor: true,
            capture_border: false,
            include_secondary_windows: true,
            min_update_interval_ms: 0,
            countdown_seconds: 0,
            capture_method: CaptureMethod::Auto,
            test_pattern: false,
            hotkeys_enabled: true,
            hotkey_start_stop: "F9".to_string(),
            hotkey_pause: "F10".to_string(),
            hotkey_screenshot: "F8".to_string(),
            theme: ThemeMode::Dark,
            show_preview: true,
            remember_last_source: true,
            last_source: None,
            ffmpeg_path: PathBuf::new(),
        }
    }
}

impl AppConfig {
    /// Loads the configuration, falling back to defaults for anything missing
    /// or unreadable. Never fails: a broken config must not block recording.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(mut config) => {
                    log::info!("loaded configuration from {}", path.display());
                    config.normalize();
                    config
                }
                Err(err) => {
                    log::warn!("configuration at {} is invalid ({err}); using defaults", path.display());
                    let _ = std::fs::rename(&path, path.with_extension("json.broken"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let mut config = self.clone();
        config.normalize();
        let dir = app_data_dir();
        std::fs::create_dir_all(&dir)?;
        let path = config_path();
        let text = serde_json::to_string_pretty(&config)?;
        // Write to a temporary file first so a crash cannot leave a truncated
        // configuration behind.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Clamps user input into ranges the rest of the program can rely on.
    pub fn normalize(&mut self) {
        self.schema_version = SCHEMA_VERSION;
        self.frame_rate = self.frame_rate.clamp(1, 240);
        self.countdown_seconds = self.countdown_seconds.min(60);
        self.custom_bitrate_kbps = self.custom_bitrate_kbps.clamp(200, 200_000);
        self.min_update_interval_ms = self.min_update_interval_ms.min(1_000);
        self.max_output_width = if self.max_output_width == 0 {
            0
        } else {
            self.max_output_width.clamp(160, 7_680) / 2 * 2
        };
        self.filename_template = if self.filename_template.trim().is_empty() {
            paths::DEFAULT_TEMPLATE.to_string()
        } else {
            self.filename_template.clone()
        };
        if self.output_dir.as_os_str().is_empty() {
            self.output_dir = paths::default_output_dir();
        }
        self.audio.system_volume = self.audio.system_volume.clamp(0.0, 4.0);
        self.audio.microphone_volume = self.audio.microphone_volume.clamp(0.0, 4.0);
        self.audio.sample_rate = match self.audio.sample_rate {
            0..=22_050 => 22_050,
            22_051..=32_000 => 32_000,
            32_001..=44_100 => 44_100,
            _ => 48_000,
        };
        self.audio.channels = match self.audio.channels {
            1 => 1,
            _ => 2,
        };
        if !formats::codecs_for(self.container).contains(&self.video_codec) {
            self.video_codec = formats::codecs_for(self.container)[0];
        }
        if self.hotkey_start_stop.trim().is_empty() {
            self.hotkey_start_stop = "F9".to_string();
        }
    }

    /// Resolved export target, honouring the available FFmpeg installation.
    pub fn export_format(&self, ffmpeg_available: bool) -> Result<ExportFormat, FormatError> {
        formats::resolve(self.container, self.video_codec, ffmpeg_available)
    }

    /// Bitrate that will be requested for a given frame size.
    pub fn video_bitrate(&self, width: u32, height: u32) -> u32 {
        formats::bitrate_bps(
            width,
            height,
            self.frame_rate,
            self.quality,
            Some(self.custom_bitrate_kbps),
        )
    }

    /// Minimum update interval handed to Windows Graphics Capture.
    pub fn effective_min_update_interval(&self) -> std::time::Duration {
        let ms = if self.min_update_interval_ms > 0 {
            self.min_update_interval_ms
        } else {
            (1000 / self.frame_rate.max(1)).max(1)
        };
        std::time::Duration::from_millis(ms as u64)
    }

    /// Convenience for the UI: is the configured output folder writable?
    pub fn output_dir_status(&self) -> Result<(), String> {
        paths::is_writable(&self.output_dir)
            .map_err(|reason| format!("录制文件夹不可写（{}）：{reason}", self.output_dir.display()))
    }

    /// Makes sure there is a usable output folder.
    ///
    /// `allow_fallback` should be false when the user explicitly chose a
    /// folder (typing a bad path must be reported, not silently redirected),
    /// and true for a default or remembered value.
    pub fn ensure_output_dir(&mut self, allow_fallback: bool) -> OutputDirOutcome {
        let Err(reason) = paths::is_writable(&self.output_dir) else {
            return OutputDirOutcome::Ok;
        };

        let from = self.output_dir.clone();
        if !allow_fallback {
            return OutputDirOutcome::Failed { dir: from, reason };
        }

        match paths::find_writable_fallback(&from) {
            Some(to) => {
                self.output_dir = to.clone();
                OutputDirOutcome::Relocated { from, to, reason }
            }
            None => OutputDirOutcome::Failed { dir: from, reason },
        }
    }

    /// Remembers the last used source when the feature is enabled.
    pub fn remember_source(&mut self, source: SavedSource) {
        if self.remember_last_source {
            self.last_source = Some(source);
        }
    }

    pub fn set_output_dir(&mut self, dir: impl Into<PathBuf>) {
        self.output_dir = dir.into();
    }

    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }
}

/// What happened when the output folder was validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputDirOutcome {
    /// The configured folder is usable.
    Ok,
    /// The configured folder was unusable and a writable one was selected.
    Relocated {
        from: PathBuf,
        to: PathBuf,
        reason: String,
    },
    /// Nothing writable was found.
    Failed { dir: PathBuf, reason: String },
}

impl OutputDirOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Message to show the user, when there is something to say.
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Ok => None,
            Self::Relocated { from, to, reason } => Some(format!(
                "录制文件夹 {} 不可写（{reason}），已自动改用 {}",
                from.display(),
                to.display()
            )),
            Self::Failed { dir, reason } => Some(format!(
                "找不到可写的录制文件夹：{} 不可写（{reason}），请手动选择一个目录",
                dir.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = AppConfig::default();
        assert!(config.frame_rate >= 1);
        assert!(config.audio.enabled);
        assert!(config.audio.system);
        assert_eq!(config.container, Container::Mp4);
        assert_eq!(config.video_codec, VideoCodec::H264);
        config
            .export_format(false)
            .expect("default format must work without ffmpeg");
    }

    #[test]
    fn round_trips_through_json() {
        let mut config = AppConfig {
            frame_rate: 60,
            container: Container::Gif,
            output_dir: PathBuf::from(r"D:\clips"),
            last_source: Some(SavedSource::Window { hwnd: 12_345, title: "记事本".into() }),
            ..AppConfig::default()
        };
        config.audio.microphone = true;

        let text = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn older_config_files_get_new_fields() {
        // Simulates upgrading from a version that only knew a couple of keys.
        let text = r#"{"frame_rate": 24, "container": "mp4"}"#;
        let config: AppConfig = serde_json::from_str(text).unwrap();
        assert_eq!(config.frame_rate, 24);
        assert_eq!(config.video_codec, VideoCodec::H264);
        assert_eq!(config.audio, AudioConfig::default());
    }

    #[test]
    fn normalize_clamps_out_of_range_values() {
        let mut config = AppConfig {
            frame_rate: 5000,
            countdown_seconds: 900,
            custom_bitrate_kbps: 1,
            filename_template: "   ".to_string(),
            ..AppConfig::default()
        };
        config.audio.sample_rate = 123;
        config.audio.channels = 9;
        config.audio.system_volume = 99.0;
        config.normalize();

        assert_eq!(config.frame_rate, 240);
        assert_eq!(config.countdown_seconds, 60);
        assert_eq!(config.custom_bitrate_kbps, 200);
        assert_eq!(config.audio.sample_rate, 22_050);
        assert_eq!(config.audio.channels, 2);
        assert_eq!(config.audio.system_volume, 4.0);
        assert_eq!(config.filename_template, paths::DEFAULT_TEMPLATE);
    }

    #[test]
    fn codec_is_repaired_for_the_container() {
        let mut config = AppConfig {
            container: Container::Webm,
            video_codec: VideoCodec::H264,
            ..AppConfig::default()
        };
        config.normalize();
        assert_eq!(config.video_codec, VideoCodec::Vp9);
    }

    #[test]
    fn ffmpeg_requirement_propagates_to_the_ui() {
        let config = AppConfig {
            container: Container::Mkv,
            video_codec: VideoCodec::H264,
            ..AppConfig::default()
        };
        assert!(config.export_format(false).is_err());
        assert!(config.export_format(true).is_ok());
    }

    #[test]
    fn min_update_interval_defaults_to_the_frame_rate() {
        let mut config = AppConfig {
            frame_rate: 30,
            min_update_interval_ms: 0,
            ..AppConfig::default()
        };
        assert_eq!(config.effective_min_update_interval().as_millis(), 33);

        config.min_update_interval_ms = 0;
        config.frame_rate = 240;
        assert_eq!(config.effective_min_update_interval().as_millis(), 4);
    }

    #[test]
    fn bitrate_uses_custom_value_when_selected() {
        let config = AppConfig {
            quality: QualityPreset::Custom,
            custom_bitrate_kbps: 15_000,
            ..AppConfig::default()
        };
        assert_eq!(config.video_bitrate(1920, 1080), 15_000_000);
    }

    #[test]
    fn a_writable_output_folder_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("screenrec_cfg_ok_{}", std::process::id()));
        let mut config = AppConfig { output_dir: dir.clone(), ..AppConfig::default() };
        assert_eq!(config.ensure_output_dir(true), OutputDirOutcome::Ok);
        assert_eq!(config.output_dir, dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unusable_default_folder_is_relocated() {
        // A path below a regular file can never become a directory.
        let dir = std::env::temp_dir().join(format!("screenrec_cfg_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();

        let mut config = AppConfig {
            output_dir: file.join("sub"),
            ..AppConfig::default()
        };

        match config.ensure_output_dir(true) {
            OutputDirOutcome::Relocated { from, to, .. } => {
                assert_eq!(from, file.join("sub"));
                assert_ne!(to, from);
                assert_eq!(config.output_dir, to);
                assert!(paths::is_writable(&config.output_dir).is_ok());
            }
            other => panic!("expected relocation, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_explicitly_chosen_bad_folder_is_reported_not_redirected() {
        let dir = std::env::temp_dir().join(format!("screenrec_cfg_explicit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();

        let chosen = file.join("sub");
        let mut config = AppConfig { output_dir: chosen.clone(), ..AppConfig::default() };
        match config.ensure_output_dir(false) {
            OutputDirOutcome::Failed { dir, .. } => assert_eq!(dir, chosen),
            other => panic!("expected failure, got {other:?}"),
        }
        assert_eq!(config.output_dir, chosen, "must not be redirected");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outcome_messages_mention_both_folders() {
        let outcome = OutputDirOutcome::Relocated {
            from: PathBuf::from("C:/a"),
            to: PathBuf::from("D:/b"),
            reason: "拒绝访问".to_string(),
        };
        let message = outcome.message().unwrap();
        assert!(message.contains("C:\\a") || message.contains("C:/a"));
        assert!(message.contains("D:\\b") || message.contains("D:/b"));
        assert!(message.contains("拒绝访问"));
        assert!(OutputDirOutcome::Ok.message().is_none());
    }
}
