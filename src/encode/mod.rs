//! Frame sinks — everything that can turn captured frames into an output file.
//!
//! A recording session owns exactly one sink. The capture handler is
//! responsible for producing a *constant* frame rate (see
//! [`crate::capture::pacer`]), so sinks can assume frames arrive in order with
//! monotonic timestamps.

pub mod capability;
pub mod ffmpeg_sink;
pub mod gif_sink;
pub mod image_sink;
pub mod native_video;
pub mod util;

use std::path::PathBuf;

use crate::formats::{AudioCodec, Container, ExportFormat, VideoCodec};

/// Row order of the pixels handed to a sink.
///
/// Media Foundation reads RGB buffers bottom-up, everything else here wants
/// top-down. Letting the sink declare it means the pipeline can *produce* the
/// requested order — flipping while it copies a frame anyway — instead of making
/// the sink flip a second full-frame copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowOrder {
    /// Row 0 is the top of the image (the capture format).
    #[default]
    TopDown,
    /// Row 0 is the bottom of the image; Media Foundation's expectation.
    BottomUp,
}

/// One captured frame in the format produced by the capture backend.
#[derive(Debug, Clone, Copy)]
pub struct FrameData<'a> {
    /// BGRA, no row padding, `width * height * 4` bytes.
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// Presentation time in 100 ns units, relative to the first frame.
    pub timestamp_100ns: i64,
    /// Row order of `pixels`, matching [`FrameSink::row_order`].
    pub row_order: RowOrder,
}

/// Audio format the sink must accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    pub codec: AudioCodec,
    pub bitrate: u32,
}

/// Everything a sink needs to know before the first frame.
#[derive(Debug, Clone)]
pub struct SinkParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub audio: Option<AudioParams>,
}

/// Destination of a recording.
pub trait FrameSink: Send {
    /// Writes one frame.
    fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()>;

    /// Row order the sink wants frames in; the pipeline produces this order.
    fn row_order(&self) -> RowOrder {
        RowOrder::TopDown
    }

    /// Writes interleaved 16-bit PCM matching [`SinkParams::audio`].
    fn push_audio(&mut self, _pcm: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    /// Flushes and closes the output, returning what was written.
    fn finish(self: Box<Self>) -> anyhow::Result<PathBuf>;

    /// Short description for logs and the UI.
    fn describe(&self) -> String;

    /// True when the sink actually consumes audio.
    fn accepts_audio(&self) -> bool {
        false
    }
}

/// Everything needed to build a sink.
pub struct SinkRequest<'a> {
    pub format: ExportFormat,
    pub params: SinkParams,
    pub output: &'a crate::paths::PlannedOutput,
    /// Path of a usable `ffmpeg` binary, required by [`crate::formats::Backend::Ffmpeg`].
    pub ffmpeg: Option<&'a std::path::Path>,
    pub quality: crate::formats::QualityPreset,
    pub prefer_hardware: bool,
}

/// Creates the sink that matches a resolved [`ExportFormat`].
pub fn create_sink(request: SinkRequest<'_>) -> anyhow::Result<Box<dyn FrameSink>> {
    use crate::formats::Backend;

    let SinkRequest { format, params, output, ffmpeg, quality, prefer_hardware } = request;

    match format.backend {
        Backend::NativeVideo => Ok(Box::new(native_video::NativeVideoSink::new(
            format.container, format.codec, params, output,
        )?)),
        Backend::Ffmpeg => {
            let ffmpeg = ffmpeg.ok_or_else(|| {
                anyhow::anyhow!(
                    "导出 {} 需要 FFmpeg，但未找到可用的 ffmpeg 可执行文件",
                    format.container.id()
                )
            })?;
            Ok(Box::new(ffmpeg_sink::FfmpegSink::new(
                ffmpeg,
                format.container,
                format.codec,
                params,
                output,
                quality,
                prefer_hardware,
            )?))
        }
        Backend::NativeGif => Ok(Box::new(gif_sink::GifSink::new(params, output)?)),
        Backend::NativeImageSequence => Ok(Box::new(image_sink::ImageSequenceSink::new(
            format.container, params, output,
        )?)),
    }
}

/// Video codec that will end up in the file, for status messages.
pub fn codec_description(container: Container, codec: VideoCodec) -> String {
    if container.is_animated_image() {
        "GIF (256 色调色板)".to_string()
    } else if container.is_image_sequence() {
        format!("{} 序列", container.extension().to_uppercase())
    } else {
        codec.id().to_uppercase()
    }
}
