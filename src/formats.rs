//! Export format model.
//!
//! The recorder can write video through the built-in Windows Media Foundation
//! encoder (no external tools), or — when an `ffmpeg` binary is available —
//! through FFmpeg for the containers Media Foundation cannot produce (Matroska,
//! WebM, QuickTime). Animated GIF and image sequences are always produced
//! natively in Rust.
//!
//! Everything in this module is pure data plus small pure functions so the
//! capability matrix can be unit-tested without touching a screen or a codec.

use serde::{Deserialize, Serialize};

/// Output container / file kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    #[default]
    Mp4,
    Mkv,
    Mov,
    Webm,
    Avi,
    Wmv,
    Mpg,
    Gif,
    PngSeq,
    JpegSeq,
}

impl Container {
    pub const ALL: [Self; 10] = [
        Self::Mp4,
        Self::Mkv,
        Self::Mov,
        Self::Webm,
        Self::Avi,
        Self::Wmv,
        Self::Mpg,
        Self::Gif,
        Self::PngSeq,
        Self::JpegSeq,
    ];

    /// Video containers only (no GIF / image sequences).
    pub fn video_containers() -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(|c| c.is_video())
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Mov => "mov",
            Self::Webm => "webm",
            Self::Avi => "avi",
            Self::Wmv => "wmv",
            Self::Mpg => "mpg",
            Self::Gif => "gif",
            Self::PngSeq => "png_seq",
            Self::JpegSeq => "jpeg_seq",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Mov => "mov",
            Self::Webm => "webm",
            Self::Avi => "avi",
            Self::Wmv => "wmv",
            Self::Mpg => "mpg",
            Self::Gif => "gif",
            Self::PngSeq => "png",
            Self::JpegSeq => "jpg",
        }
    }

    /// Human readable name shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4 视频 (.mp4)",
            Self::Mkv => "Matroska (.mkv)",
            Self::Mov => "QuickTime (.mov)",
            Self::Webm => "WebM (.webm)",
            Self::Avi => "AVI (.avi)",
            Self::Wmv => "Windows Media (.wmv)",
            Self::Mpg => "MPEG-2 (.mpg)",
            Self::Gif => "动图 GIF (.gif)",
            Self::PngSeq => "PNG 序列 (每帧一张)",
            Self::JpegSeq => "JPEG 序列 (每帧一张)",
        }
    }

    pub fn is_image_sequence(self) -> bool {
        matches!(self, Self::PngSeq | Self::JpegSeq)
    }

    pub fn is_animated_image(self) -> bool {
        matches!(self, Self::Gif)
    }

    pub fn is_video(self) -> bool {
        !self.is_image_sequence() && !self.is_animated_image()
    }

    /// Containers the built-in Media Foundation encoder can write.
    pub fn native_subtype(self) -> Option<NativeContainer> {
        match self {
            Self::Mp4 => Some(NativeContainer::Mpeg4),
            Self::Avi => Some(NativeContainer::Avi),
            Self::Wmv => Some(NativeContainer::Asf),
            Self::Mpg => Some(NativeContainer::Mpeg2),
            _ => None,
        }
    }

    /// True when this container needs an external FFmpeg binary.
    pub fn requires_ffmpeg(self) -> bool {
        self.native_subtype().is_none() && self.is_video()
    }

    /// Parses a container from its [`Container::id`] or file extension.
    pub fn from_id(id: &str) -> Option<Self> {
        let needle = id.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|candidate| {
            candidate.id() == needle || candidate.extension() == needle
        })
    }
}

/// Containers supported by the built-in encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeContainer {
    Mpeg4,
    Avi,
    Asf,
    Mpeg2,
}

/// Video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    #[default]
    H264,
    Hevc,
    Av1,
    Vp9,
    Vp8,
    Mpeg4,
    Mjpeg,
    Mpeg2,
    Wmv3,
    ProRes,
}

impl VideoCodec {
    pub fn id(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Vp8 => "vp8",
            Self::Mpeg4 => "mpeg4",
            Self::Mjpeg => "mjpeg",
            Self::Mpeg2 => "mpeg2",
            Self::Wmv3 => "wmv3",
            Self::ProRes => "prores",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264 / AVC（兼容性最好）",
            Self::Hevc => "H.265 / HEVC（体积更小）",
            Self::Av1 => "AV1（需要 FFmpeg）",
            Self::Vp9 => "VP9（需要 FFmpeg）",
            Self::Vp8 => "VP8（需要 FFmpeg）",
            Self::Mpeg4 => "MPEG-4 Part 2（需要 FFmpeg）",
            Self::Mjpeg => "Motion JPEG（体积大）",
            Self::Mpeg2 => "MPEG-2（老设备兼容）",
            Self::Wmv3 => "Windows Media Video 9",
            Self::ProRes => "Apple ProRes（需要 FFmpeg）",
        }
    }

    /// `ffmpeg -c:v <name>`
    pub fn ffmpeg_encoder(self, prefer_hardware: bool) -> &'static str {
        match self {
            Self::H264 => {
                if prefer_hardware {
                    "h264_nvenc"
                } else {
                    "libx264"
                }
            }
            Self::Hevc => {
                if prefer_hardware {
                    "hevc_nvenc"
                } else {
                    "libx265"
                }
            }
            Self::Av1 => "libsvtav1",
            Self::Vp9 => "libvpx-vp9",
            Self::Vp8 => "libvpx",
            Self::Mpeg4 => "mpeg4",
            Self::Mjpeg => "mjpeg",
            Self::Mpeg2 => "mpeg2video",
            Self::Wmv3 => "wmv2",
            Self::ProRes => "prores_ks",
        }
    }

    /// Codecs usable without FFmpeg.
    pub fn is_native(self) -> bool {
        matches!(self, Self::H264 | Self::Hevc | Self::Mjpeg | Self::Mpeg2 | Self::Wmv3)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        let needle = id.trim().to_ascii_lowercase();
        [
            Self::H264,
            Self::Hevc,
            Self::Av1,
            Self::Vp9,
            Self::Vp8,
            Self::Mpeg4,
            Self::Mjpeg,
            Self::Mpeg2,
            Self::Wmv3,
            Self::ProRes,
        ]
        .into_iter()
        .find(|candidate| {
            candidate.id() == needle
                || matches!(
                    (candidate, needle.as_str()),
                    (Self::H264, "h264" | "avc" | "x264")
                        | (Self::Hevc, "hevc" | "h265" | "x265")
                        | (Self::Vp9, "vp9")
                        | (Self::Vp8, "vp8")
                )
        })
    }
}

/// Audio codec for the exported file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    #[default]
    Aac,
    Mp3,
    Opus,
    Flac,
    Pcm,
}

impl AudioCodec {
    pub fn id(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
            Self::Flac => "flac",
            Self::Pcm => "pcm",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Aac => "AAC（推荐）",
            Self::Mp3 => "MP3",
            Self::Opus => "Opus（需要 FFmpeg）",
            Self::Flac => "FLAC 无损",
            Self::Pcm => "PCM 无损（体积大）",
        }
    }

    pub fn ffmpeg_encoder(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Mp3 => "libmp3lame",
            Self::Opus => "libopus",
            Self::Flac => "flac",
            Self::Pcm => "pcm_s16le",
        }
    }

    /// True when the built-in Media Foundation encoder can produce it.
    pub fn is_native(self) -> bool {
        !matches!(self, Self::Opus)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "aac" => Some(Self::Aac),
            "mp3" => Some(Self::Mp3),
            "opus" => Some(Self::Opus),
            "flac" => Some(Self::Flac),
            "pcm" | "wav" => Some(Self::Pcm),
            _ => None,
        }
    }
}

/// Quality presets, expressed as bits per pixel per frame before clamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    Low,
    #[default]
    Medium,
    High,
    Ultra,
    Custom,
}

impl QualityPreset {
    pub const ALL: [Self; 5] =
        [Self::Low, Self::Medium, Self::High, Self::Ultra, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "流畅（体积小）",
            Self::Medium => "标准（推荐）",
            Self::High => "高清",
            Self::Ultra => "超清（体积大）",
            Self::Custom => "自定义码率",
        }
    }

    /// Bits per pixel per frame.
    fn bits_per_pixel(self) -> f64 {
        match self {
            Self::Low => 0.05,
            Self::Medium => 0.09,
            Self::High => 0.16,
            Self::Ultra => 0.28,
            Self::Custom => 0.09,
        }
    }

    /// FFmpeg constant-rate-factor equivalent (lower = better).
    pub fn crf(self) -> u32 {
        match self {
            Self::Low => 30,
            Self::Medium => 24,
            Self::High => 20,
            Self::Ultra => 16,
            Self::Custom => 20,
        }
    }

    /// Audio bitrate in bits per second.
    pub fn audio_bitrate(self) -> u32 {
        match self {
            Self::Low => 96_000,
            Self::Medium => 160_000,
            Self::High => 192_000,
            Self::Ultra => 256_000,
            Self::Custom => 192_000,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "low" | "流畅" => Some(Self::Low),
            "medium" | "standard" | "标准" => Some(Self::Medium),
            "high" | "高清" => Some(Self::High),
            "ultra" | "超清" => Some(Self::Ultra),
            "custom" | "自定义" => Some(Self::Custom),
            _ => None,
        }
    }
}

/// Which engine performs the export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Built-in Windows Media Foundation encoder.
    NativeVideo,
    /// External FFmpeg process fed with raw frames.
    Ffmpeg,
    /// In-process animated GIF writer.
    NativeGif,
    /// In-process PNG / JPEG sequence writer.
    NativeImageSequence,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::NativeVideo => "内置编码器（Media Foundation）",
            Self::Ffmpeg => "FFmpeg",
            Self::NativeGif => "内置 GIF 编码器",
            Self::NativeImageSequence => "内置图像序列编码器",
        }
    }

    pub fn needs_ffmpeg(self) -> bool {
        matches!(self, Self::Ffmpeg)
    }
}

/// A fully resolved export target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportFormat {
    pub container: Container,
    pub codec: VideoCodec,
    pub backend: Backend,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        self.container.extension()
    }

    pub fn requires_ffmpeg(self) -> bool {
        self.backend.needs_ffmpeg()
    }

    /// Whether this format can actually be produced right now.
    pub fn is_available(self, ffmpeg_available: bool) -> bool {
        !self.requires_ffmpeg() || ffmpeg_available
    }

    pub fn describe(self) -> String {
        if self.container.is_video() {
            format!(
                "{} · {} · {}",
                self.container.label(),
                self.codec.id().to_uppercase(),
                self.backend.label()
            )
        } else {
            // GIF and image sequences do not use the selected video codec.
            format!("{} · {}", self.container.label(), self.backend.label())
        }
    }
}

/// Codecs that make sense for a container, best first.
pub fn codecs_for(container: Container) -> &'static [VideoCodec] {
    use VideoCodec::*;
    match container {
        Container::Mp4 => &[H264, Hevc, Av1, Mpeg4],
        Container::Mkv => &[H264, Hevc, Av1, Vp9, Mpeg4],
        Container::Mov => &[H264, Hevc, ProRes],
        Container::Webm => &[Vp9, Vp8, Av1],
        Container::Avi => &[Mjpeg, Mpeg2, H264, Mpeg4],
        Container::Wmv => &[Wmv3],
        Container::Mpg => &[Mpeg2],
        Container::Gif | Container::PngSeq | Container::JpegSeq => &[H264],
    }
}

/// (container, codec) pairs the built-in encoder can write.
pub fn is_native_combo(container: Container, codec: VideoCodec) -> bool {
    matches!(
        (container, codec),
        (Container::Mp4, VideoCodec::H264)
            | (Container::Mp4, VideoCodec::Hevc)
            | (Container::Avi, VideoCodec::Mjpeg)
            | (Container::Avi, VideoCodec::Mpeg2)
            | (Container::Wmv, VideoCodec::Wmv3)
            | (Container::Mpg, VideoCodec::Mpeg2)
    )
}

/// Reason a format could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("容器 {container} 不支持编码器 {codec}")]
    Incompatible { container: String, codec: String },
    #[error("容器 {0} 需要安装 FFmpeg 才能导出")]
    NeedsFfmpeg(String),
}

/// Resolve a container + codec pair into a concrete export target.
///
/// `ffmpeg_available` decides whether FFmpeg-backed combinations are allowed.
pub fn resolve(
    container: Container,
    codec: VideoCodec,
    ffmpeg_available: bool,
) -> Result<ExportFormat, FormatError> {
    if container.is_image_sequence() {
        return Ok(ExportFormat {
            container,
            codec: VideoCodec::H264,
            backend: Backend::NativeImageSequence,
        });
    }
    if container.is_animated_image() {
        return Ok(ExportFormat { container, codec: VideoCodec::H264, backend: Backend::NativeGif });
    }

    if !codecs_for(container).contains(&codec) {
        return Err(FormatError::Incompatible {
            container: container.id().to_string(),
            codec: codec.id().to_string(),
        });
    }

    if is_native_combo(container, codec) {
        return Ok(ExportFormat { container, codec, backend: Backend::NativeVideo });
    }

    if container.requires_ffmpeg() || !codec.is_native() {
        if !ffmpeg_available {
            return Err(FormatError::NeedsFfmpeg(container.id().to_string()));
        }
        return Ok(ExportFormat { container, codec, backend: Backend::Ffmpeg });
    }

    // Native container, codec not in the native table: fall back to FFmpeg.
    if !ffmpeg_available {
        return Err(FormatError::NeedsFfmpeg(container.id().to_string()));
    }
    Ok(ExportFormat { container, codec, backend: Backend::Ffmpeg })
}

/// Video bitrate in bits per second.
pub fn bitrate_bps(
    width: u32,
    height: u32,
    fps: u32,
    preset: QualityPreset,
    custom_kbps: Option<u32>,
) -> u32 {
    if preset == QualityPreset::Custom {
        let kbps = custom_kbps.unwrap_or(8_000).clamp(200, 200_000);
        return kbps.saturating_mul(1_000);
    }

    let pixels = (width.max(2) as f64) * (height.max(2) as f64);
    let raw = pixels * fps.max(1) as f64 * preset.bits_per_pixel();
    (raw as u32).clamp(500_000, 200_000_000)
}

/// Output scaling options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScaleOption {
    #[default]
    Original,
    P1080,
    P720,
    P480,
    HalfHeight,
}

impl ScaleOption {
    pub const ALL: [Self; 5] =
        [Self::Original, Self::P1080, Self::P720, Self::P480, Self::HalfHeight];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "原始分辨率",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
            Self::HalfHeight => "按一半高度缩放",
        }
    }

    /// Target size for a source of `(width, height)`; never upscales.
    pub fn apply(self, width: u32, height: u32) -> (u32, u32) {
        let even = |v: u32| (v / 2) * 2;
        match self {
            Self::Original => (even(width.max(2)), even(height.max(2))),
            Self::P1080 => fit(width, height, 1920, 1080),
            Self::P720 => fit(width, height, 1280, 720),
            Self::P480 => fit(width, height, 854, 480),
            Self::HalfHeight => (even((width / 2).max(2)), even((height / 2).max(2))),
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "original" | "原始" => Some(Self::Original),
            "1080" | "1080p" => Some(Self::P1080),
            "720" | "720p" => Some(Self::P720),
            "480" | "480p" => Some(Self::P480),
            "half" | "half_height" | "一半" => Some(Self::HalfHeight),
            _ => None,
        }
    }
}

fn fit(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width <= max_w && height <= max_h {
        return ((width / 2) * 2, (height / 2) * 2);
    }
    let ratio = (max_w as f64 / width as f64).min(max_h as f64 / height as f64);
    let w = ((width as f64 * ratio) as u32 / 2) * 2;
    let h = ((height as f64 * ratio) as u32 / 2) * 2;
    (w.max(2), h.max(2))
}

/// Frame rate actually used for a container.
///
/// Animated GIF is capped because every frame needs its own palette and the
/// format's delays are only precise to 1/100 s.
pub fn effective_fps(container: Container, requested: u32) -> u32 {
    match container {
        Container::Gif => requested.clamp(1, 15),
        _ => requested.clamp(1, 240),
    }
}

/// Width ceiling applied to a container, if any.
pub fn effective_max_width(container: Container) -> Option<u32> {
    match container {
        Container::Gif => Some(640),
        _ => None,
    }
}

/// Output size for a capture of `source` size.
pub fn plan_size(
    container: Container,
    scale: ScaleOption,
    source_width: u32,
    source_height: u32,
) -> (u32, u32) {
    plan_size_with_cap(container, scale, 0, source_width, source_height)
}

/// Output size, additionally bounded by a user supplied width ceiling.
///
/// `max_width` of 0 means "only the container's own ceiling applies".
pub fn plan_size_with_cap(
    container: Container,
    scale: ScaleOption,
    max_width: u32,
    source_width: u32,
    source_height: u32,
) -> (u32, u32) {
    let (mut width, mut height) = scale.apply(source_width, source_height);

    let ceiling = match (effective_max_width(container), max_width) {
        (Some(container_cap), 0) => Some(container_cap),
        (Some(container_cap), user_cap) => Some(container_cap.min(user_cap)),
        (None, 0) => None,
        (None, user_cap) => Some(user_cap),
    };

    if let Some(ceiling) = ceiling {
        if width > ceiling {
            let ratio = ceiling as f64 / width as f64;
            width = ceiling;
            height = ((height as f64 * ratio) as u32).max(2);
        }
    }

    // Encoders (and YUV 4:2:0 in particular) require even dimensions.
    ((width / 2 * 2).max(2), (height / 2 * 2).max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_container_has_at_least_one_codec() {
        for container in Container::ALL {
            assert!(
                !codecs_for(container).is_empty(),
                "container {:?} has no codec",
                container
            );
        }
    }

    #[test]
    fn native_combos_are_advertised_by_their_container() {
        for container in Container::ALL {
            for codec in codecs_for(container) {
                if is_native_combo(container, *codec) {
                    assert!(
                        codecs_for(container).contains(codec),
                        "native combo {:?}/{:?} not in codec list",
                        container,
                        codec
                    );
                }
            }
        }
    }

    #[test]
    fn ffmpeg_only_containers_require_ffmpeg() {
        for container in [Container::Mkv, Container::Mov, Container::Webm] {
            assert!(container.requires_ffmpeg(), "{container:?} should need ffmpeg");
            assert!(
                resolve(container, codecs_for(container)[0], false).is_err(),
                "{container:?} resolved without ffmpeg"
            );
            assert!(resolve(container, codecs_for(container)[0], true).is_ok());
        }
    }

    #[test]
    fn native_formats_work_without_ffmpeg() {
        for (container, codec) in [
            (Container::Mp4, VideoCodec::H264),
            (Container::Mp4, VideoCodec::Hevc),
            (Container::Avi, VideoCodec::Mjpeg),
            (Container::Wmv, VideoCodec::Wmv3),
            (Container::Mpg, VideoCodec::Mpeg2),
        ] {
            let format = resolve(container, codec, false).expect("native format");
            assert_eq!(format.backend, Backend::NativeVideo);
            assert!(!format.requires_ffmpeg());
        }
    }

    #[test]
    fn image_outputs_never_need_ffmpeg() {
        for container in [Container::Gif, Container::PngSeq, Container::JpegSeq] {
            let format = resolve(container, VideoCodec::H264, false).expect("image format");
            assert!(!format.requires_ffmpeg());
        }
    }

    #[test]
    fn incompatible_pairs_are_rejected() {
        // VP9 is not offered for MP4 in this app.
        assert!(resolve(Container::Mp4, VideoCodec::ProRes, true).is_err());
        assert!(resolve(Container::Webm, VideoCodec::Mjpeg, true).is_err());
    }

    #[test]
    fn bitrate_grows_with_quality_and_resolution() {
        let small = bitrate_bps(1280, 720, 30, QualityPreset::Low, None);
        let big = bitrate_bps(1920, 1080, 60, QualityPreset::Ultra, None);
        assert!(big > small);
        for preset in QualityPreset::ALL {
            if preset == QualityPreset::Custom {
                continue;
            }
            let a = bitrate_bps(1920, 1080, 30, preset, None);
            assert!(a >= 500_000, "{preset:?} produced {a}");
        }
    }

    #[test]
    fn custom_bitrate_is_respected() {
        let bps = bitrate_bps(1920, 1080, 30, QualityPreset::Custom, Some(12_345));
        assert_eq!(bps, 12_345_000);
        // Out of range values are clamped instead of overflowing.
        assert_eq!(bitrate_bps(1920, 1080, 30, QualityPreset::Custom, Some(0)), 200_000);
    }

    #[test]
    fn scaling_never_upscales_and_stays_even() {
        let (w, h) = ScaleOption::P720.apply(3840, 2160);
        assert_eq!((w, h), (1280, 720));
        let (w, h) = ScaleOption::P1080.apply(1280, 720);
        assert_eq!((w, h), (1280, 720));
        let (w, h) = ScaleOption::HalfHeight.apply(1001, 733);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn gif_is_rate_limited_and_capped_in_width() {
        assert_eq!(effective_fps(Container::Gif, 60), 15);
        assert_eq!(effective_fps(Container::Mp4, 60), 60);

        let (w, h) = plan_size(Container::Gif, ScaleOption::Original, 1920, 1080);
        assert_eq!(w, 640);
        assert_eq!(h, 360);
    }

    #[test]
    fn plan_size_keeps_odd_sources_even() {
        let (w, h) = plan_size(Container::Mp4, ScaleOption::Original, 1921, 1081);
        assert_eq!((w, h), (1920, 1080));
    }

    #[test]
    fn plan_size_respects_smaller_explicit_scales_for_gif() {
        let (w, h) = plan_size(Container::Gif, ScaleOption::P480, 1920, 1080);
        assert!(w <= 640, "{w}");
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }
}
