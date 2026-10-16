//! In-process animated GIF writer.
//!
//! GIF has a hard 256-colour limit, so the app caps the frame rate and the
//! width for this container (see [`crate::formats::effective_fps`] and
//! [`crate::formats::effective_max_width`]). Frames are quantised one by one
//! with NeuQuant; the `speed` parameter trades quality for time.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use super::util::bgra_to_rgba;
use super::{FrameData, FrameSink, SinkParams};
use crate::paths::PlannedOutput;

/// NeuQuant sample fraction: lower is slower and better.
const QUANTIZER_SPEED: i32 = 20;

pub struct GifSink {
    encoder: Option<gif::Encoder<BufWriter<File>>>,
    path: PathBuf,
    width: u16,
    height: u16,
    fps: u32,
    rgba: Vec<u8>,
    /// Centiseconds the file says it has so far.
    emitted_centiseconds: u32,
    /// Centiseconds the timeline wants so far (fractional).
    wanted_centiseconds: f64,
    frames: u64,
}

impl GifSink {
    pub fn new(params: SinkParams, output: &PlannedOutput) -> anyhow::Result<Self> {
        anyhow::ensure!(
            params.width > 0 && params.height > 0 && params.width <= u16::MAX as u32,
            "GIF 尺寸超出范围: {}x{}",
            params.width,
            params.height
        );

        output.prepare()?;
        let file = File::create(&output.path)?;
        let mut encoder = gif::Encoder::new(
            BufWriter::new(file),
            params.width as u16,
            params.height as u16,
            &[],
        )?;
        encoder.set_repeat(gif::Repeat::Infinite)?;

        log::info!(
            "GIF 编码器已启动: {}x{} @ {} fps",
            params.width,
            params.height,
            params.fps
        );

        Ok(Self {
            encoder: Some(encoder),
            path: output.path.clone(),
            width: params.width as u16,
            height: params.height as u16,
            fps: params.fps.max(1),
            rgba: Vec::new(),
            emitted_centiseconds: 0,
            wanted_centiseconds: 0.0,
            frames: 0,
        })
    }
}

impl FrameSink for GifSink {
    fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        anyhow::ensure!(
            frame.pixels.len() >= expected,
            "GIF 帧大小不匹配: {} < {}",
            frame.pixels.len(),
            expected
        );

        bgra_to_rgba(&frame.pixels[..expected], &mut self.rgba);

        // GIF stores delays in centiseconds, which rarely divides evenly into
        // the frame rate; carry the remainder so the timeline stays accurate.
        self.wanted_centiseconds += 100.0 / self.fps as f64;
        let wanted = self.wanted_centiseconds.floor() as u32;
        let delay = wanted.saturating_sub(self.emitted_centiseconds).max(1);
        self.emitted_centiseconds += delay;

        let mut gif_frame = gif::Frame::from_rgba_speed(
            self.width,
            self.height,
            &mut self.rgba,
            QUANTIZER_SPEED,
        );
        gif_frame.delay = delay.min(u16::MAX as u32) as u16;
        gif_frame.dispose = gif::DisposalMethod::Background;

        let encoder = self
            .encoder
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("GIF 编码器已经结束"))?;
        encoder.write_frame(&gif_frame)?;
        self.frames += 1;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> anyhow::Result<PathBuf> {
        if let Some(encoder) = self.encoder.take() {
            drop(encoder);
        }
        let size = super::native_video::verify_output(&self.path)?;
        log::info!("GIF 完成: {} 帧, {} 字节", self.frames, size);
        Ok(self.path.clone())
    }

    fn describe(&self) -> String {
        format!("内置 GIF 编码器 · {} 帧", self.frames)
    }
}
