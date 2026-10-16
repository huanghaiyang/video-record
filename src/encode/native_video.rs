//! Built-in video encoder (Windows Media Foundation, via `windows-capture`).
//!
//! This path needs no external tools and uses the hardware encoders exposed by
//! the GPU driver when the container/codec combination maps onto a Media
//! Foundation media type.

use std::path::{Path, PathBuf};

use windows_capture::encoder::{
    AudioSettingsBuilder, AudioSettingsSubType, ContainerSettingsBuilder, ContainerSettingsSubType,
    VideoEncoder, VideoSettingsBuilder, VideoSettingsSubType,
};

use super::{AudioParams, FrameData, FrameSink, SinkParams};
use crate::encode::util;
use crate::formats::{AudioCodec, Container, VideoCodec};
use crate::paths::PlannedOutput;

/// Maps an app container onto a Media Foundation container subtype.
pub fn container_subtype(container: Container) -> anyhow::Result<ContainerSettingsSubType> {
    Ok(match container {
        Container::Mp4 => ContainerSettingsSubType::MPEG4,
        Container::Avi => ContainerSettingsSubType::AVI,
        Container::Wmv => ContainerSettingsSubType::ASF,
        Container::Mpg => ContainerSettingsSubType::MPEG2,
        other => anyhow::bail!("容器 {} 无法用内置编码器写出", other.id()),
    })
}

/// Maps an app codec onto a Media Foundation video subtype.
pub fn video_subtype(codec: VideoCodec) -> anyhow::Result<VideoSettingsSubType> {
    Ok(match codec {
        VideoCodec::H264 => VideoSettingsSubType::H264,
        VideoCodec::Hevc => VideoSettingsSubType::HEVC,
        VideoCodec::Mjpeg => VideoSettingsSubType::MJPG,
        VideoCodec::Mpeg2 => VideoSettingsSubType::MPEG2,
        VideoCodec::Wmv3 => VideoSettingsSubType::WMV3,
        other => anyhow::bail!("编码器 {} 需要 FFmpeg", other.id()),
    })
}

/// Maps an app audio codec onto a Media Foundation audio subtype.
pub fn audio_subtype(codec: AudioCodec) -> anyhow::Result<AudioSettingsSubType> {
    Ok(match codec {
        AudioCodec::Aac => AudioSettingsSubType::AAC,
        AudioCodec::Mp3 => AudioSettingsSubType::MP3,
        AudioCodec::Flac => AudioSettingsSubType::FLAC,
        AudioCodec::Pcm => AudioSettingsSubType::PCM,
        AudioCodec::Opus => anyhow::bail!("Opus 需要 FFmpeg"),
    })
}

pub struct NativeVideoSink {
    encoder: Option<VideoEncoder>,
    path: PathBuf,
    label: String,
    audio: Option<AudioParams>,
    width: u32,
    height: u32,
    /// Reused buffer holding the bottom-up copy Media Foundation expects.
    mf_frame: Vec<u8>,
}

impl NativeVideoSink {
    pub fn new(
        container: Container,
        codec: VideoCodec,
        params: SinkParams,
        output: &PlannedOutput,
    ) -> anyhow::Result<Self> {
        let path = output.path.clone();
        Self::create(container, codec, params, path)
    }

    /// Creates a sink writing to an explicit path (used by the format probe).
    pub fn create(
        container: Container,
        codec: VideoCodec,
        params: SinkParams,
        path: PathBuf,
    ) -> anyhow::Result<Self> {
        let video_settings = VideoSettingsBuilder::new(params.width, params.height)
            .bitrate(params.bitrate)
            .frame_rate(params.fps)
            .sub_type(video_subtype(codec)?);

        let audio_settings = match params.audio {
            Some(audio) => AudioSettingsBuilder::new()
                .bitrate(audio.bitrate)
                .sample_rate(audio.sample_rate)
                .channel_count(audio.channels as u32)
                .bit_per_sample(16)
                .sub_type(audio_subtype(audio.codec)?),
            None => AudioSettingsBuilder::default().disabled(true),
        };

        let container_settings =
            ContainerSettingsBuilder::new().sub_type(container_subtype(container)?);

        let encoder = VideoEncoder::new(video_settings, audio_settings, container_settings, &path)
            .map_err(|err| encoder_creation_error(&path, &err.to_string()))?;
        log::info!(
            "内置编码器已启动: {} · {} · {}x{}@{} · {} bps",
            container.id(),
            codec.id(),
            params.width,
            params.height,
            params.fps,
            params.bitrate
        );

        Ok(Self {
            encoder: Some(encoder),
            path,
            label: format!("内置编码器 {}", codec.id().to_uppercase()),
            audio: params.audio,
            width: params.width,
            height: params.height,
            mf_frame: Vec::new(),
        })
    }

    fn encoder(&mut self) -> anyhow::Result<&mut VideoEncoder> {
        self.encoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("编码器已经结束"))
    }
}

impl FrameSink for NativeVideoSink {
    fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        anyhow::ensure!(
            frame.width == self.width && frame.height == self.height,
            "帧尺寸 {}x{} 与编码器设定的 {}x{} 不一致",
            frame.width,
            frame.height,
            self.width,
            self.height
        );
        anyhow::ensure!(
            frame.pixels.len() >= expected,
            "帧大小不匹配: 收到 {} 字节，期望 {expected} 字节",
            frame.pixels.len()
        );

        let mut mf_frame = std::mem::take(&mut self.mf_frame);
        prepare_mf_frame(
            &frame.pixels[..expected],
            self.width,
            self.height,
            &mut mf_frame,
        );
        let result = self
            .encoder()?
            .send_frame_buffer(&mf_frame, frame.timestamp_100ns);
        self.mf_frame = mf_frame;
        result.map_err(Into::into)
    }

    fn push_audio(&mut self, pcm: &[u8]) -> anyhow::Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        self.encoder()?.send_audio_buffer(pcm, 0)?;
        Ok(())
    }

    fn accepts_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn finish(mut self: Box<Self>) -> anyhow::Result<PathBuf> {
        if let Some(encoder) = self.encoder.take() {
            encoder.finish()?;
        }
        verify_output(&self.path)?;
        Ok(self.path.clone())
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

/// Reorders a captured (top-down) frame for Media Foundation.
///
/// Media Foundation interprets RGB media types as **bottom-up** unless
/// `MF_MT_DEFAULT_STRIDE` is negative, and the encoder wrapper does not set that
/// attribute. Feeding top-down pixels straight into `send_frame_buffer` makes
/// the recorded video come out vertically mirrored, so the rows are reversed
/// here. The Direct3D-surface path (`send_frame`) is unaffected.
///
/// Verified by decoding a recording and comparing it against the source frames;
/// see `screenrec record --test-pattern`.
fn prepare_mf_frame(pixels: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    util::flip_vertical(pixels, width, height, out);
}

/// Fails loudly when an encoder claimed success but produced nothing usable.
pub fn verify_output(path: &Path) -> anyhow::Result<u64> {
    let meta = std::fs::metadata(path)
        .map_err(|err| anyhow::anyhow!("输出文件 {} 不存在: {err}", path.display()))?;
    if meta.len() == 0 {
        anyhow::bail!(
            "编码器没有写入任何数据（{}）。该容器/编码器组合可能不被系统支持。",
            path.display()
        );
    }
    Ok(meta.len())
}

/// Turns an encoder startup failure into something the user can act on.
///
/// The Media Foundation sink writer reports an access failure with the generic
/// "this item is not in a location the application can access" text, which says
/// nothing about *which* folder or what to do about it.
pub fn encoder_creation_error(path: &Path, message: &str) -> anyhow::Error {
    let denied = message.contains("0x80070005")
        || message.contains("拒绝访问")
        || message.contains("Access is denied");

    if denied {
        anyhow::anyhow!(
            "无法在 {} 创建视频文件：{message}\n\
             提示：该文件夹不允许系统编码器写入（受保护目录、只读卷或在受限进程中运行时常见）。\
             请在「保存位置」中改选其它文件夹，或用 --output-dir 指定一个用户目录。",
            path.display()
        )
    } else {
        anyhow::anyhow!("无法创建视频编码器（{}）：{message}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_reversed_for_media_foundation() {
        // 2x2 BGRA: row 0 is 1s, row 1 is 2s (8 bytes each).
        let mut source = Vec::new();
        source.extend_from_slice(&[1u8; 8]);
        source.extend_from_slice(&[2u8; 8]);

        let mut prepared = Vec::new();
        prepare_mf_frame(&source, 2, 2, &mut prepared);

        // The last source row must come first, otherwise MF renders the video
        // upside down.
        assert_eq!(prepared.len(), 16);
        assert_eq!(&prepared[0..8], &[2u8; 8]);
        assert_eq!(&prepared[8..16], &[1u8; 8]);
    }

    #[test]
    fn preparing_preserves_every_byte() {
        let width = 7u32;
        let height = 5u32;
        let mut source = Vec::new();
        for index in 0..(width * height) {
            source.extend_from_slice(&[index as u8, (index * 3) as u8, (index * 7) as u8, 255]);
        }

        let mut prepared = Vec::new();
        prepare_mf_frame(&source, width, height, &mut prepared);
        assert_eq!(prepared.len(), source.len());

        let mut sorted_source = source.clone();
        sorted_source.sort_unstable();
        let mut sorted_prepared = prepared.clone();
        sorted_prepared.sort_unstable();
        assert_eq!(sorted_source, sorted_prepared, "flip must only reorder rows");
    }

    #[test]
    fn access_denied_errors_explain_what_to_do() {
        let error = encoder_creation_error(
            Path::new(r"D:\clips\a.mp4"),
            "Windows API error: 拒绝访问。 (0x80070005)",
        );
        let text = error.to_string();
        assert!(text.contains(r"D:\clips\a.mp4"), "{text}");
        assert!(text.contains("保存位置") || text.contains("--output-dir"), "{text}");
    }

    #[test]
    fn other_failures_keep_the_original_message() {
        let error = encoder_creation_error(
            Path::new("a.mp4"),
            "找不到接受该用户首选输出类型的编码器 MFT。 (0xC00DA412)",
        );
        let text = error.to_string();
        assert!(text.contains("0xC00DA412"), "{text}");
        assert!(!text.contains("保存位置"), "{text}");
    }

    #[test]
    fn verify_output_rejects_missing_and_empty_files() {
        let dir = std::env::temp_dir().join(format!("screenrec_verify_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("missing.mp4");
        assert!(verify_output(&missing).is_err());

        let empty = dir.join("empty.mp4");
        std::fs::write(&empty, b"").unwrap();
        assert!(verify_output(&empty).is_err());

        let good = dir.join("good.mp4");
        std::fs::write(&good, b"data").unwrap();
        assert_eq!(verify_output(&good).unwrap(), 4);

        std::fs::remove_dir_all(&dir).ok();
    }
}
