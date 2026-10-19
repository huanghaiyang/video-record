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
use crate::encode::{util, FrameData, FrameSink, RowOrder};

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
    /// Row order of what currently sits in `staging`/`duplicate`, which is
    /// whatever the sink asked for.
    row_order: RowOrder,
    preview_enabled: bool,
    preview_pixels: Vec<u8>,
    last_preview: Instant,
    pause_started: Option<Instant>,
    failure: Option<String>,
    finished: bool,
    stats: Stats,
}

/// Per-frame counters, reported periodically so a stuttering recording can be
/// diagnosed from the log instead of guessed at.
///
/// The numbers answer the three questions that matter when the output looks like
/// a slideshow: how fast frames actually arrive, how many output frames are
/// repeats, and how long the per-frame work (including the encoder) takes.
#[derive(Debug, Default)]
struct Stats {
    /// Frames handed to the pipeline.
    pushed: u64,
    /// Frames dropped before any processing because the timeline did not need
    /// them (the source runs faster than the target frame rate).
    skipped_not_due: u64,
    /// Frames that went through crop/scale/encode.
    processed: u64,
    /// Output frames written, and how many of them repeat the previous frame.
    ///
    /// Repeats come from two places and both must be counted, otherwise a
    /// recording that duplicates frames to hold the frame rate looks healthy:
    /// * catch-up inside one push (the timeline was behind by several frames),
    /// * gap filling from [`FramePipeline::tick`] (the source stopped delivering).
    emitted: u64,
    repeated: u64,
    /// Time spent in the per-frame work, and inside the encoder specifically.
    work: Duration,
    encode: Duration,
    max_encode: Duration,
    /// Snapshot of the counters at the previous report.
    reported_at: Option<Instant>,
    reported: StatsSnapshot,
}

/// Counters as of the last report, so the periodic line can show rates.
#[derive(Debug, Default, Clone, Copy)]
struct StatsSnapshot {
    pushed: u64,
    skipped_not_due: u64,
    processed: u64,
    emitted: u64,
    repeated: u64,
    work: Duration,
    encode: Duration,
}

impl Stats {
    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            pushed: self.pushed,
            skipped_not_due: self.skipped_not_due,
            processed: self.processed,
            emitted: self.emitted,
            repeated: self.repeated,
            work: self.work,
            encode: self.encode,
        }
    }
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
            row_order: RowOrder::TopDown,
            preview_enabled: preview,
            preview_pixels: Vec::new(),
            last_preview: Instant::now(),
            pause_started: None,
            failure: None,
            finished: false,
            stats: Stats::default(),
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
        let expected = (width as usize) * (height as usize) * 4;
        anyhow::ensure!(
            pixels.len() >= expected,
            "帧数据不完整：{} < {}",
            pixels.len(),
            expected
        );
        let source = &pixels[..expected];

        // Area recording: the region is resolved first because it also decides
        // the encoder's size. Clamping here means a region dragged past the edge
        // still records the part that exists.
        let region = self.crop.map(|region| {
            let clamped = region.clamp_to(width, height);
            self.shared.set_region(Some(clamped));
            clamped
        });
        let (source_width, source_height) =
            region.map_or((width, height), |region| (region.width, region.height));

        if self.sink.is_none() {
            self.ensure_sink(source_width, source_height)?;
        }

        if self.check_pause() {
            return Ok(());
        }

        // Drop frames the timeline does not need *before* transforming them: a
        // 60 Hz source recording at 30 fps would otherwise crop, scale and
        // convert every single frame only to throw half of them away.
        let elapsed = self.pacer.elapsed();
        self.shared.set_elapsed(elapsed);
        self.stats.pushed += 1;
        if self.pacer.frames_due(elapsed) <= self.pacer.emitted() {
            self.stats.skipped_not_due += 1;
            self.report_stats(false);
            return Ok(());
        }

        let started = Instant::now();
        let mut crop_scratch = std::mem::take(&mut self.crop_scratch);
        let result = match region {
            Some(region) => util::crop_bgra(source, width, height, region, &mut crop_scratch)
                .and_then(|()| {
                    self.publish(&crop_scratch, region.width, region.height)
                }),
            None => self.publish(source, width, height),
        };
        self.crop_scratch = crop_scratch;
        if result.is_ok() {
            self.stats.processed += 1;
            self.stats.work += started.elapsed();
        }
        self.report_stats(false);
        result
    }

    /// Logs a periodic summary and warns when the source cannot keep up.
    ///
    /// A recording that looks like a slideshow is almost always gap filling:
    /// fewer distinct frames arrive than the target frame rate needs, so the
    /// timeline is padded with repeats. These counters make that visible.
    fn report_stats(&mut self, final_report: bool) {
        const INTERVAL: Duration = Duration::from_secs(2);

        let now = Instant::now();
        let Some(previous_time) = self.stats.reported_at else {
            // First call only starts the reporting window.
            self.stats.reported_at = Some(now);
            self.stats.reported = self.stats.snapshot();
            return;
        };
        let elapsed = now.saturating_duration_since(previous_time);
        if elapsed.is_zero() || (!final_report && elapsed < INTERVAL) {
            return;
        }

        let before = self.stats.reported;
        let after = self.stats.snapshot();
        let seconds = elapsed.as_secs_f64();
        let delta = |now: u64, then: u64| now.saturating_sub(then);
        let emitted = delta(after.emitted, before.emitted);
        let repeated = delta(after.repeated, before.repeated);
        let processed = delta(after.processed, before.processed);
        let pushed = delta(after.pushed, before.pushed);

        let per_frame = |total: Duration, count: u64| {
            if count == 0 {
                0.0
            } else {
                total.as_secs_f64() * 1_000.0 / count as f64
            }
        };
        let work_ms = per_frame(after.work.saturating_sub(before.work), processed);
        let encode_ms = per_frame(after.encode.saturating_sub(before.encode), emitted);

        let fps_of = |count: u64| count as f64 / seconds.max(0.001);
        log::info!(
            "录制统计：采集 {:.1} fps（{} 帧，跳过 {} 帧）· 输出 {:.1} fps（{} 帧，其中重复 {}）· \
             单帧处理 {:.1} ms · 编码 {:.1} ms（峰值 {:.1} ms）",
            fps_of(pushed),
            pushed,
            delta(after.skipped_not_due, before.skipped_not_due),
            fps_of(emitted),
            emitted,
            repeated,
            work_ms,
            encode_ms,
            self.stats.max_encode.as_secs_f64() * 1_000.0,
        );

        // More than a third of the output being repeats means the pipeline (or
        // the source) cannot sustain the requested frame rate.
        if emitted > 0 && repeated * 3 > emitted {
            let percent = repeated as f64 / emitted as f64 * 100.0;
            log::warn!(
                "采集跟不上目标帧率：约 {percent:.0}% 的输出帧是重复帧，画面会像幻灯片。\
                 建议：降低帧率、把输出缩放到 1080p、关闭显示器 HDR，或改用硬件编码器。"
            );
        }

        self.stats.reported = after;
        self.stats.reported_at = Some(now);
        self.stats.max_encode = Duration::ZERO;
    }

    /// Scales a ready frame, emits everything the timeline owes and updates the
    /// preview.
    fn publish(&mut self, pixels: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
        self.update_preview(pixels, width, height);

        // Take the buffer out of `self` so it can be borrowed next to the other
        // fields of the pipeline.
        let mut staging = std::mem::take(&mut self.staging);
        let result = self.publish_inner(pixels, width, height, &mut staging);
        self.staging = staging;
        result
    }

    fn publish_inner(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        staging: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        // Produce the final output-sized frame in the order the sink wants.
        // When the sink needs bottom-up rows (Media Foundation) the flip is fused
        // into this copy instead of costing a second full-frame pass.
        let bottom_up = self
            .sink
            .as_ref()
            .map(|sink| sink.row_order() == RowOrder::BottomUp)
            .unwrap_or(false);
        if width == self.dst_width && height == self.dst_height {
            if bottom_up {
                util::flip_vertical(pixels, width, height, staging);
            } else {
                staging.clear();
                staging.extend_from_slice(pixels);
            }
            self.row_order = if bottom_up {
                RowOrder::BottomUp
            } else {
                RowOrder::TopDown
            };
        } else {
            util::scale_bgra(
                pixels,
                width,
                height,
                self.dst_width,
                self.dst_height,
                &mut self.scratch,
                staging,
            )?;
            // The scaler always writes top-down; the sink flips if it needs to.
            self.row_order = RowOrder::TopDown;
        }

        let frame_width = self.dst_width;
        let frame_height = self.dst_height;
        let due = self.pacer.frames_due(self.pacer.elapsed());
        let before = self.pacer.emitted();
        while self.pacer.emitted() < due {
            self.emit(staging, frame_width, frame_height)?;
        }
        let emitted_now = self.pacer.emitted();
        if emitted_now > before {
            // One push can owe several output frames when the timeline fell
            // behind; everything past the first is a repeat of this frame.
            self.stats.repeated += emitted_now - before - 1;
        }

        if emitted_now != before {
            // Keep the finished frame for gap filling by *handing over* the
            // buffer instead of copying it again; `staging` gets the previous
            // frame's allocation back and reuses it next time.
            std::mem::swap(&mut self.duplicate, staging);
            self.duplicate_valid = true;
        }

        if matches!(self.shared.status(), SessionStatus::Preparing) {
            self.shared.set_status(SessionStatus::Recording);
        }
        Ok(())
    }

    /// Writes one output frame from already final-sized pixels.
    ///
    /// Relies on data that is already owned by the pipeline (`staging` or
    /// `duplicate`); every sink copies what it needs before returning.
    fn emit(&mut self, pixels: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
        let index = self.pacer.emitted();
        let frame = FrameData {
            pixels,
            width,
            height,
            timestamp_100ns: self.pacer.timestamp_100ns(index),
            row_order: self.row_order,
        };

        let sink = self
            .sink
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("编码器尚未创建"))?;

        // Time the encoder separately: it is the only part of the per-frame work
        // that happens outside this crate's control, so a slideshow can be told
        // apart from a slow encoder.
        let encode_started = Instant::now();
        sink.push(&frame)?;
        let encode_time = encode_started.elapsed();
        self.stats.encode += encode_time;
        self.stats.max_encode = self.stats.max_encode.max(encode_time);

        if let Some(audio) = self.audio.as_mut() {
            let fps = self.pacer.fps();
            let chunk = audio.take_for_video_frame(index, fps);
            if !chunk.is_empty() {
                sink.push_audio(chunk)?;
            }
        }

        // Remembering the frame for gap filling is the caller's job
        // (`publish_inner` hands the buffer over; `tick` already owns it), so
        // nothing is copied here.

        self.stats.emitted += 1;
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
                // These pixels already are the remembered frame, so nothing is
                // copied: the timeline is simply extended with them.
                self.emit(&duplicate, width, height)?;
                self.stats.repeated += 1;
            }
            Ok(())
        })();
        self.duplicate = duplicate;
        self.report_stats(false);
        result
    }

    /// Stops the encoder and publishes the result.
    pub fn finalize(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.report_stats(true);

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
    use crate::region::Region;
    use std::sync::Mutex;

    /// One frame as the sink saw it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Captured {
        width: u32,
        height: u32,
        timestamp: i64,
        /// First byte of the frame, used to tell frames apart.
        marker: u8,
    }

    struct CaptureSink {
        frames: Arc<Mutex<Vec<Captured>>>,
    }

    impl FrameSink for CaptureSink {
        fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()> {
            self.frames.lock().unwrap().push(Captured {
                width: frame.width,
                height: frame.height,
                timestamp: frame.timestamp_100ns,
                marker: frame.pixels.first().copied().unwrap_or(0),
            });
            Ok(())
        }

        fn finish(self: Box<Self>) -> anyhow::Result<std::path::PathBuf> {
            Ok(std::path::PathBuf::from("test"))
        }

        fn describe(&self) -> String {
            "测试编码器".to_string()
        }
    }

    fn capture_factory(
        target: Option<(u32, u32)>,
        frames: Arc<Mutex<Vec<Captured>>>,
    ) -> SinkFactory {
        Box::new(move |width: u32, height: u32| {
            let (out_width, out_height) = target.unwrap_or((width, height));
            Ok((
                Box::new(CaptureSink { frames }) as Box<dyn FrameSink>,
                out_width,
                out_height,
            ))
        })
    }

    fn frame(marker: u8, width: u32, height: u32) -> Vec<u8> {
        vec![marker; (width * height * 4) as usize]
    }

    /// Builds a pipeline plus the list its sink writes into.
    fn pipeline(
        fps: u32,
        crop: Option<Region>,
        target: Option<(u32, u32)>,
    ) -> (FramePipeline, Arc<Mutex<Vec<Captured>>>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(SessionShared::new());
        let mut pipeline = FramePipeline::new(
            capture_factory(target, Arc::clone(&frames)),
            shared,
            fps,
            None,
            false,
        );
        pipeline.set_crop(crop);
        (pipeline, frames)
    }

    #[test]
    fn frames_that_are_not_due_yet_are_dropped() {
        // 20 fps means one frame every 50 ms; pushing in a tight loop must not
        // produce frames, which is what makes the early-out worth having.
        let (mut pipeline, frames) = pipeline(20, None, None);
        for _ in 0..50 {
            pipeline.push_bgra(&frame(0xAA, 32, 32), 32, 32).unwrap();
        }
        assert_eq!(pipeline.emitted_frames(), 0);
        assert!(frames.lock().unwrap().is_empty());

        // Once the timeline is due, the next frame fills in the gaps.
        std::thread::sleep(Duration::from_millis(120));
        pipeline.push_bgra(&frame(0xAA, 32, 32), 32, 32).unwrap();

        let captured = frames.lock().unwrap().clone();
        assert!(
            (2..=3).contains(&captured.len()),
            "expected the owed frames, got {}",
            captured.len()
        );
        // All of them carry the frame that was just pushed.
        assert!(captured.iter().all(|item| item.marker == 0xAA));
        // Timestamps follow the timeline, not the arrival order of pushes.
        for (index, item) in captured.iter().enumerate() {
            assert_eq!(item.timestamp, index as i64 * 10_000_000 / 20);
        }
    }

    #[test]
    fn a_stalled_source_is_filled_with_the_last_frame() {
        let (mut pipeline, frames) = pipeline(20, None, None);

        // The first frame starts the timeline, so nothing is due yet; a second
        // frame after a pause produces at least one output frame.
        pipeline.push_bgra(&frame(0xBB, 32, 32), 32, 32).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        pipeline.push_bgra(&frame(0xBB, 32, 32), 32, 32).unwrap();
        let after_push = pipeline.emitted_frames();
        assert!(after_push >= 1, "the second frame should be emitted");

        // Then silence: `tick` has to extend the timeline.
        std::thread::sleep(Duration::from_millis(150));
        pipeline.tick().unwrap();

        let captured = frames.lock().unwrap().clone();
        assert!(
            captured.len() as u64 > after_push,
            "tick must add frames for the stall"
        );
        // Every filler must repeat the remembered frame — and in particular not
        // contain leftovers of a recycled buffer.
        assert!(
            captured.iter().all(|item| item.marker == 0xBB),
            "filler frames must repeat the last frame: {captured:?}"
        );
    }

    #[test]
    fn the_remembered_frame_follows_the_newest_content() {
        // Guards the buffer hand-over: the frame kept for gap filling must be
        // the newest one, not the buffer that was swapped out.
        let (mut pipeline, frames) = pipeline(20, None, None);

        std::thread::sleep(Duration::from_millis(60));
        pipeline.push_bgra(&frame(0x11, 32, 32), 32, 32).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        pipeline.push_bgra(&frame(0x22, 32, 32), 32, 32).unwrap();

        frames.lock().unwrap().clear();
        std::thread::sleep(Duration::from_millis(120));
        pipeline.tick().unwrap();

        let captured = frames.lock().unwrap().clone();
        assert!(!captured.is_empty(), "tick must extend the timeline");
        assert!(
            captured.iter().all(|item| item.marker == 0x22),
            "gap filling must use the newest frame: {captured:?}"
        );
    }

    #[test]
    fn cropping_sizes_the_output_and_keeps_the_region_content() {
        // 64x64 frame where every pixel encodes its own column, cropped to
        // columns 8..24 and rows 8..24.
        let (mut pipeline, frames) = pipeline(20, Some(Region::new(8, 8, 16, 16)), None);

        let mut source = vec![0u8; 64 * 64 * 4];
        for y in 0..64usize {
            for x in 0..64usize {
                let offset = (y * 64 + x) * 4;
                source[offset] = x as u8;
                source[offset + 1] = y as u8;
                source[offset + 3] = 255;
            }
        }

        std::thread::sleep(Duration::from_millis(60));
        pipeline.push_bgra(&source, 64, 64).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        pipeline.push_bgra(&source, 64, 64).unwrap();

        let captured = frames.lock().unwrap().clone();
        assert!(!captured.is_empty());
        assert_eq!((captured[0].width, captured[0].height), (16, 16));
        // The sink only sees the region, so its first pixel is the region's
        // top-left corner: column 8, row 8.
        assert_eq!(captured[0].marker, 8);
    }

    #[test]
    fn scaling_uses_the_sink_size() {
        let (mut pipeline, frames) = pipeline(20, None, Some((16, 16)));
        pipeline.push_bgra(&frame(0x33, 64, 64), 64, 64).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        pipeline.push_bgra(&frame(0x33, 64, 64), 64, 64).unwrap();

        let captured = frames.lock().unwrap().clone();
        assert!(!captured.is_empty());
        assert_eq!((captured[0].width, captured[0].height), (16, 16));
        assert_eq!(captured[0].marker, 0x33);
    }

    #[test]
    fn an_incomplete_frame_is_rejected_before_any_work() {
        let (mut pipeline, frames) = pipeline(20, None, None);
        assert!(pipeline.push_bgra(&[], 32, 32).is_err());
        assert!(pipeline.push_bgra(&[0u8; 16], 32, 32).is_err());
        assert!(frames.lock().unwrap().is_empty());
    }

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
