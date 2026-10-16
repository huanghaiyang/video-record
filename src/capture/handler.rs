//! Windows Graphics Capture callback.
//!
//! This is a thin adapter: frame extraction happens here (that is all the
//! Graphics Capture API offers), everything else lives in
//! [`super::pipeline::FramePipeline`].

use std::sync::Arc;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;

use super::pipeline::FramePipeline;
use super::session::SessionShared;

pub use super::pipeline::{SinkFactory, PREVIEW_MAX_WIDTH};

/// State handed to the capture thread.
pub struct HandlerFlags {
    pub pipeline: FramePipeline,
    pub shared: Arc<SessionShared>,
}

pub struct RecorderHandler {
    pipeline: FramePipeline,
    shared: Arc<SessionShared>,
    /// Scratch buffer for the raw (possibly row-padded) frame copy.
    raw: Vec<u8>,
}

impl GraphicsCaptureApiHandler for RecorderHandler {
    type Flags = HandlerFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let flags = ctx.flags;
        log::info!("图形捕获会话已建立，等待第一帧…");
        Ok(Self {
            pipeline: flags.pipeline,
            shared: flags.shared,
            raw: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.pipeline.is_finished() {
            capture_control.stop();
            return Ok(());
        }

        if self.shared.is_stop_requested() {
            self.pipeline.finalize();
            capture_control.stop();
            return Ok(());
        }

        if let Err(err) = self.handle_frame(frame) {
            let message = format!("{err:#}");
            log::error!("录制过程中出错: {message}");
            self.pipeline.set_failure(message);
            self.pipeline.finalize();
            capture_control.stop();
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::info!("捕获目标已关闭，正在结束录制");
        self.shared.request_stop();
        self.pipeline.finalize();
        Ok(())
    }
}

impl RecorderHandler {
    fn handle_frame(&mut self, frame: &mut Frame) -> anyhow::Result<()> {
        let width = frame.width();
        let height = frame.height();

        // `as_nopadding_buffer` borrows from both the frame buffer and the
        // destination, so the buffer must outlive the pixel slice.
        let mut raw = std::mem::take(&mut self.raw);
        let result = (|| -> anyhow::Result<()> {
            let frame_buffer = frame.buffer()?;
            let pixels = frame_buffer.as_nopadding_buffer(&mut raw);
            self.pipeline.push_bgra(pixels, width, height)
        })();
        self.raw = raw;
        result
    }
}
