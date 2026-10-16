//! The frame pipeline shared by every capture backend.
//!
//! A backend only has to produce BGRA frames; this type owns the encoder, the
//! constant-frame-rate pacer, the audio mixer, the live preview and the session
//! state. Both the Windows Graphics Capture callback and the DXGI Desktop
//! Duplication loop drive exactly the same code, so pacing and A/V sync behave
//! identically no matter how frames arrive.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::pacer::Pacer;
use super::session::{PreviewFrame, SessionShared, SessionStatus};
use super::Region;
use crate::audio::AudioMixer;
use crate::encode::{util, FrameData, FrameSink};

/// Builds the encoder once the capture size is known.
///
/// Returns the sink plus the size it expects each frame to have.
pub type SinkFactory =
    Box<dyn FnOnce(u32, u32) -> anyhow::Result<(Box<dyn FrameSink>, u32, u32)> + Send>;

/// Longest edge (in pixels) of the preview image sent to the UI.
pub const PREVIEW_MAX_WIDTH: u32 = 480;
/// Minimum time between two preview images.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(200);

pub struct FramePipeline {
    factory: Option<SinkFactory>,
    sink: Option<Box<dyn FrameSink>>,
    shared: Arc<SessionShared>,
    pacer: Pacer,
    audio: Option<AudioMixer>,
    dst_width: u32,
    dst_height: u32,
    /// Scratch buffers kept between frames to avoid per-frame allocations.
    staging: Vec<u8>,
    scratch: Vec<u8>,
    /// Holds the cropped frame when area recording is enabled.
    crop_scratch: Vec<u8>,
    /// Area of the captured frame to keep, relative to its top-left corner.
    crop: Option<Region>,
    /// Copy of the last frame in output format, used to fill gaps when the
    /// source stops producing frames (a static screen).
    duplicate: Vec<u8>,
    duplicate_valid: bool,
    preview_enabled: bool,
    preview_pixels: Vec<u8>,
    last_preview: Instant,
    pause_started: Option<Instant>,
    failure: Option<String>,
    finished: bool,
}

impl FramePipeline {
    pub fn new(
        factory: SinkFactory,
        shared: Arc<SessionShared>,
        fps: u32,
        audio: Option<AudioMixer>,
        preview: bool,
    ) -> Self {
        Self {
            factory: Some(factory),
            sink: None,
            shared,
            pacer: Pacer::new(fps),
            audio,
            dst_width: 0,
            dst_height: 0,
            staging: Vec::new(),
            scratch: Vec::new(),
            crop_scratch: Vec::new(),
            crop: None,
            duplicate: Vec::new(),
            duplicate_valid: false,
            preview_enabled: preview,
            preview_pixels: Vec::new(),
            last_preview: Instant::now(),
            pause_started: None,
            failure: None,
            finished: false,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Records only this part of every captured frame.
    ///
    /// The crop is applied before the encoder is created, so the output size,
    /// the scaler and the preview all treat the region as their source.
    pub fn set_crop(&mut self, crop: Option<Region>) {
        self.crop = crop;
    }

    pub fn crop(&self) -> Option<Region> {
        self.crop
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn set_failure(&mut self, message: impl Into<String>) {
        self.failure = Some(message.into());
    }

    /// Creates the encoder and starts the timeline. Called on the first frame.
    fn ensure_sink(&mut self, source_width: u32, source_height: u32) -> anyhow::Result<()> {
        let factory = self
            .factory
            .take()
            .ok_or_else(|| anyhow::anyhow!("编码器已经创建"))?;
        let (sink, dst_width, dst_height) = factory(source_width, source_height)?;

        self.shared.set_source_size(source_width, source_height);
        self.shared.set_output_size(dst_width, dst_height);
        self.shared.set_backend(sink.describe());
        self.dst_width = dst_width;
        self.dst_height = dst_height;

        if self.audio.is_some() {
            self.shared.set_audio_summary("已启用");
        }

        self.pacer.restart();
        self.dst_width = dst_width;
        self.dst_height = dst_height;
        self.last_preview = Instant::now() - PREVIEW_INTERVAL;
        self.sink = Some(sink);
        self.duplicate_valid = false;
        log::info!("编码器已创建：输出 {dst_width}x{dst_height}，源 {source_width}x{source_height}");
        Ok(())
    }

    /// Copies the newest preview image out of a source frame.
    fn update_preview(&mut self, pixels: &[u8], width: u32, height: u32) {
        if !self.preview_enabled || self.last_preview.elapsed() < PREVIEW_INTERVAL {
            return;
        }
        let (preview_width, preview_height) = make_preview(
            pixels,
            width,
            height,
            PREVIEW_MAX_WIDTH,
            &mut self.preview_pixels,
        );
        self.last_preview = Instant::now();
        let rgba = std::mem::take(&mut self.preview_pixels);
        self.shared.set_preview(PreviewFrame {
            width: preview_width,
            height: preview_height,
            rgba,
            captured_at: Instant::now(),
        });
    }

    /// Handles pause/resume transitions; returns true when frames must be
    /// skipped for now.
    fn check_pause(&mut self) -> bool {
        if self.shared.is_paused() {
            if self.pause_started.is_none() {
                self.pause_started = Some(Instant::now());
                self.shared.set_status(SessionStatus::Paused);
            }
            return true;
        }

        if let Some(started) = self.pause_started.take() {
            let paused_for = Instant::now().saturating_duration_since(started);
            self.pacer.add_paused(paused_for);
            if let Some(audio) = self.audio.as_mut() {
                audio.request_resync();
            }
            self.shared.set_status(SessionStatus::Recording);
            log::info!("已继续录制（暂停 {:.1} 秒）", paused_for.as_secs_f32());
        }

        false
    }

    /// Feeds one captured BGRA frame (top-down, no row padding).
    pub fn push_bgra(&mut self, pixels: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
        anyhow::ensure!(
            width > 0 && height > 0,
            "捕获到的画面尺寸无效：{width}x{height}"
        );

        // Area recording: cut the region out first so everything downstream —
        // encoder size, scaler, preview — sees the region as its source.
        let mut crop_scratch = std::mem::take(&mut self.crop_scratch);
        let result = match self.crop {
            Some(region) => {
                // Clamp here so a region dragged past the edge is recorded from
                // the part that exists, and publish what was really used.
                let region = region.clamp_to(width, height);
                self.shared.set_region(Some(region));
                util::crop_bgra(pixels, width, height, region, &mut crop_scratch).and_then(|()| {
                    self.push_frame(&crop_scratch, region.width, region.height)
                })
            }
            None => self.push_frame(pixels, width, height),
        };
        self.crop_scratch = crop_scratch;
        result
    }

    /// Runs a ready frame through the sink, the pacing timeline and the preview.
    fn push_frame(&mut self, pixels: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
        if self.sink.is_none() {
            self.ensure_sink(width, height)?;
        }

        if self.check_pause() {
            return Ok(());
        }

        // Take buffers out of `self` so they can be borrowed next to the other
        // fields of the pipeline.
        let mut staging = std::mem::take(&mut self.staging);
        let result = self.push_inner(pixels, width, height, &mut staging);
        self.staging = staging;
        result
    }

    fn push_inner(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        staging: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        let expected = (width as usize) * (height as usize) * 4;
        anyhow::ensure!(
            pixels.len() >= expected,
            "帧数据不完整：{} < {}",
            pixels.len(),
            expected
        );
        let source = &pixels[..expected];

        self.update_preview(source, width, height);

        // Produce the final output-sized frame (a no-op in the common case).
        if source.len() == (self.dst_width as usize) * (self.dst_height as usize) * 4
            && width == self.dst_width
            && height == self.dst_height
        {
            staging.clear();
            staging.extend_from_slice(source);
        } else {
            util::scale_bgra(
                source,
                width,
                height,
                self.dst_width,
                self.dst_height,
                &mut self.scratch,
                staging,
            )?;
        }

        let frame_width = self.dst_width;
        let frame_height = self.dst_height;
        let elapsed = self.pacer.elapsed();
        self.shared.set_elapsed(elapsed);

        let due = self.pacer.frames_due(elapsed);
        while self.pacer.emitted() < due {
            self.emit(staging, frame_width, frame_height)?;
        }

        if matches!(self.shared.status(), SessionStatus::Preparing) {
            self.shared.set_status(SessionStatus::Recording);
        }
        Ok(())
    }

    /// Writes one output frame from already final-sized pixels.
    fn emit(&mut self, pixels: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
        let index = self.pacer.emitted();
        let frame = FrameData {
            pixels,
            width,
            height,
            timestamp_100ns: self.pacer.timestamp_100ns(index),
        };

        let sink = self
            .sink
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("编码器尚未创建"))?;
        sink.push(&frame)?;

        if let Some(audio) = self.audio.as_mut() {
            let fps = self.pacer.fps();
            let chunk = audio.take_for_video_frame(index, fps);
            if !chunk.is_empty() {
                sink.push_audio(chunk)?;
            }
        }

        // Remember this frame so a stalled source can still extend the
        // recording timeline instead of truncating it.
        self.duplicate.clear();
        self.duplicate.extend_from_slice(pixels);
        self.duplicate_valid = true;

        self.pacer.advance();
        self.shared.bump_frames();
        Ok(())
    }

    /// Fills in frames the timeline expects but the source did not deliver.
    ///
    /// Called by polling backends (DXGI) when no new frame is available, so a
    /// static screen still produces a recording of the full duration.
    pub fn tick(&mut self) -> anyhow::Result<()> {
        if self.sink.is_none() || self.finished {
            return Ok(());
        }
        if self.check_pause() {
            return Ok(());
        }

        let elapsed = self.pacer.elapsed();
        self.shared.set_elapsed(elapsed);
        if !self.duplicate_valid || self.pacer.frames_due(elapsed) <= self.pacer.emitted() {
            return Ok(());
        }

        let width = self.dst_width;
        let height = self.dst_height;
        let duplicate = std::mem::take(&mut self.duplicate);
        let result = (|| -> anyhow::Result<()> {
            let due = self.pacer.frames_due(elapsed);
            while self.pacer.emitted() < due {
                self.emit(&duplicate, width, height)?;
            }
            Ok(())
        })();
        self.duplicate = duplicate;
        result
    }

    /// Stops the encoder and publishes the result.
    pub fn finalize(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;

        if !matches!(self.shared.status(), SessionStatus::Failed(_)) {
            self.shared.set_status(SessionStatus::Stopping);
        }

        let sink = self.sink.take();
        let result = match sink {
            Some(sink) => sink.finish(),
            None => Err(anyhow::anyhow!("没有捕获到任何画面，未生成文件")),
        };

        if let Some(message) = self.failure.take() {
            self.shared.fail(message);
            return;
        }

        match result {
            Ok(path) => {
                let file_size = output_size(&path);
                let frames = self.pacer.emitted();
                let duration = self.pacer.elapsed();
                log::info!(
                    "录制完成: {} · {} 帧 · {:.1} 秒 · {} 字节",
                    path.display(),
                    frames,
                    duration.as_secs_f32(),
                    file_size
                );
                self.shared.complete(path, file_size, frames, duration);
            }
            Err(err) => self.shared.fail(format!("{err:#}")),
        }
    }

    /// Frames already handed to the encoder.
    pub fn emitted_frames(&self) -> u64 {
        self.pacer.emitted()
    }
}

/// Size of a finished recording.
///
/// Image sequences finish as a directory, so the frame sizes are summed;
/// reporting a directory's own length would be a confusing "0 B".
fn output_size(path: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.metadata().ok())
                .filter(|meta| meta.is_file())
                .map(|meta| meta.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Nearest-neighbour downscale into RGBA for the live preview.
///
/// Nearest sampling keeps this cheap enough to run on the capture thread a few
/// times per second; the preview shows what is being recorded, it is not a
/// faithful image.
fn make_preview(
    pixels: &[u8],
    width: u32,
    height: u32,
    max_width: u32,
    out: &mut Vec<u8>,
) -> (u32, u32) {
    out.clear();
    if width == 0 || height == 0 {
        return (0, 0);
    }

    let step = width.div_ceil(max_width.max(1)).max(1);
    let preview_width = width.div_ceil(step);
    let preview_height = height.div_ceil(step);
    out.reserve((preview_width * preview_height * 4) as usize);

    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let index = ((y * width + x) * 4) as usize;
            if index + 3 < pixels.len() {
                // BGRA -> RGBA
                out.push(pixels[index + 2]);
                out.push(pixels[index + 1]);
                out.push(pixels[index]);
                out.push(pixels[index + 3]);
            }
            x += step;
        }
        y += step;
    }

    (preview_width, preview_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra_frame(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 200, 255]);
            }
        }
        data
    }

    #[test]
    fn preview_keeps_the_aspect_ratio() {
        let data = bgra_frame(1920, 1080);
        let mut out = Vec::new();
        let (width, height) = make_preview(&data, 1920, 1080, 480, &mut out);
        assert_eq!(width, 480);
        assert_eq!(height, 270);
        assert_eq!(out.len(), (width * height * 4) as usize);
    }

    #[test]
    fn small_frames_are_not_upscaled() {
        let data = bgra_frame(64, 32);
        let mut out = Vec::new();
        let (width, height) = make_preview(&data, 64, 32, 480, &mut out);
        assert_eq!((width, height), (64, 32));
    }

    #[test]
    fn preview_swaps_blue_and_red() {
        let data = vec![10u8, 20, 30, 255];
        let mut out = Vec::new();
        make_preview(&data, 1, 1, 480, &mut out);
        assert_eq!(out, vec![30, 20, 10, 255]);
    }

    #[test]
    fn preview_handles_odd_sizes() {
        let data = bgra_frame(1001, 733);
        let mut out = Vec::new();
        let (width, height) = make_preview(&data, 1001, 733, 480, &mut out);
        assert_eq!(out.len(), (width * height * 4) as usize);
        assert!(width <= 480);
    }

    #[test]
    fn empty_frames_produce_no_preview() {
        let mut out = Vec::new();
        assert_eq!(make_preview(&[], 0, 0, 480, &mut out), (0, 0));
        assert!(out.is_empty());
    }

    #[test]
    fn output_size_sums_sequence_files() {
        let dir = std::env::temp_dir().join(format!("screenrec-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("frame_000001.png"), vec![7u8; 100]).unwrap();
        std::fs::write(dir.join("frame_000002.png"), vec![7u8; 250]).unwrap();

        assert_eq!(output_size(&dir), 350, "directory size must be summed");
        assert_eq!(output_size(&dir.join("frame_000001.png")), 100);
        assert_eq!(output_size(&dir.join("does-not-exist")), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
