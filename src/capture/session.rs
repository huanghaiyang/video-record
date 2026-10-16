//! Recording session: preparation, backend selection and observable state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_capture::capture::GraphicsCaptureApiHandler;
use windows_capture::graphics_capture_picker::GraphicsCapturePicker;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use super::dxgi;
use super::handler::{HandlerFlags, RecorderHandler, SinkFactory};
use super::pipeline::FramePipeline;
use super::sources;
use super::{CaptureMethod, SourceSelection};
use crate::audio::{AudioCapture, AudioMixer};
use crate::config::AppConfig;
use crate::encode::{self, AudioParams, SinkParams};
use crate::formats::{self, ExportFormat};
use crate::history::{EntrySource, HistoryStore, RecordingEntry};
use crate::paths::{self, PlannedOutput};

/// Lifecycle of one recording.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionStatus {
    #[default]
    Idle,
    /// Resolving the source and starting the encoder.
    Preparing,
    Recording,
    Paused,
    /// Flushing the encoder and closing the file.
    Stopping,
    Completed,
    Failed(String),
}

impl SessionStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Recording | Self::Paused | Self::Stopping
        )
    }

    pub fn label(&self) -> String {
        match self {
            Self::Idle => "空闲".to_string(),
            Self::Preparing => "准备中…".to_string(),
            Self::Recording => "录制中".to_string(),
            Self::Paused => "已暂停".to_string(),
            Self::Stopping => "正在保存…".to_string(),
            Self::Completed => "已完成".to_string(),
            Self::Failed(message) => format!("失败：{message}"),
        }
    }
}

/// The most recent preview image.
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA, tightly packed.
    pub rgba: Vec<u8>,
    pub captured_at: Instant,
}

/// A cheap snapshot of the session for the UI.
#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    pub status: SessionStatus,
    pub frames: u64,
    pub elapsed: Duration,
    pub output: Option<PathBuf>,
    pub planned_output: Option<PathBuf>,
    pub file_size: u64,
    pub source_size: Option<(u32, u32)>,
    pub output_size: Option<(u32, u32)>,
    pub backend: String,
    pub audio: String,
}

/// State shared between the UI thread, the capture thread and the encoder.
#[derive(Debug)]
pub struct SessionShared {
    stop: AtomicBool,
    paused: AtomicBool,
    frames: AtomicU64,
    elapsed_ms: AtomicU64,
    file_size: AtomicU64,
    duration_ms: AtomicU64,
    status: Mutex<SessionStatus>,
    output: Mutex<Option<PathBuf>>,
    planned_output: Mutex<Option<PathBuf>>,
    source_size: Mutex<Option<(u32, u32)>>,
    output_size: Mutex<Option<(u32, u32)>>,
    backend: Mutex<String>,
    audio: Mutex<String>,
    preview: Mutex<Option<PreviewFrame>>,
}

impl Default for SessionShared {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionShared {
    pub fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            elapsed_ms: AtomicU64::new(0),
            file_size: AtomicU64::new(0),
            duration_ms: AtomicU64::new(0),
            status: Mutex::new(SessionStatus::Idle),
            output: Mutex::new(None),
            planned_output: Mutex::new(None),
            source_size: Mutex::new(None),
            output_size: Mutex::new(None),
            backend: Mutex::new(String::new()),
            audio: Mutex::new(String::new()),
            preview: Mutex::new(None),
        }
    }

    pub fn status(&self) -> SessionStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_status(&self, status: SessionStatus) {
        if let Ok(mut current) = self.status.lock() {
            *current = status;
        }
    }

    pub fn is_active(&self) -> bool {
        self.status().is_active()
    }

    pub fn is_stop_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Records that one more frame was handed to the encoder.
    pub fn bump_frames(&self) -> u64 {
        self.frames.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn set_elapsed(&self, elapsed: Duration) {
        self.elapsed_ms
            .store(elapsed.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn set_backend(&self, backend: impl Into<String>) {
        if let Ok(mut value) = self.backend.lock() {
            *value = backend.into();
        }
    }

    pub fn set_audio_summary(&self, summary: impl Into<String>) {
        if let Ok(mut value) = self.audio.lock() {
            *value = summary.into();
        }
    }

    pub fn set_source_size(&self, width: u32, height: u32) {
        if let Ok(mut value) = self.source_size.lock() {
            *value = Some((width, height));
        }
    }

    pub fn set_output_size(&self, width: u32, height: u32) {
        if let Ok(mut value) = self.output_size.lock() {
            *value = Some((width, height));
        }
    }

    pub fn set_planned_output(&self, path: &PlannedOutput) {
        if let Ok(mut value) = self.planned_output.lock() {
            *value = Some(path.path.clone());
        }
    }

    pub fn set_preview(&self, frame: PreviewFrame) {
        if let Ok(mut value) = self.preview.lock() {
            *value = Some(frame);
        }
    }

    pub fn take_preview(&self) -> Option<PreviewFrame> {
        self.preview.lock().ok().and_then(|mut value| value.take())
    }

    /// Marks the session as finished successfully.
    pub fn complete(&self, path: PathBuf, file_size: u64, frames: u64, duration: Duration) {
        if let Ok(mut value) = self.output.lock() {
            *value = Some(path);
        }
        self.file_size.store(file_size, Ordering::Relaxed);
        self.duration_ms
            .store(duration.as_millis() as u64, Ordering::Relaxed);
        self.set_elapsed(duration);
        let _ = frames;
        self.set_status(SessionStatus::Completed);
    }

    /// Marks the session as failed with a user-facing message.
    pub fn fail(&self, message: impl Into<String>) {
        let message = message.into();
        log::error!("录制失败: {message}");
        self.set_status(SessionStatus::Failed(message));
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            status: self.status(),
            frames: self.frames(),
            elapsed: Duration::from_millis(self.elapsed_ms.load(Ordering::Relaxed)),
            output: self.output.lock().ok().and_then(|v| v.clone()),
            planned_output: self.planned_output.lock().ok().and_then(|v| v.clone()),
            file_size: self.file_size.load(Ordering::Relaxed),
            source_size: *self.source_size.lock().unwrap_or_else(|e| e.into_inner()),
            output_size: *self.output_size.lock().unwrap_or_else(|e| e.into_inner()),
            backend: self.backend.lock().map(|v| v.clone()).unwrap_or_default(),
            audio: self.audio.lock().map(|v| v.clone()).unwrap_or_default(),
        }
    }
}

/// Everything needed to start a recording.
#[derive(Debug, Clone)]
pub struct RecordingRequest {
    pub source: SourceSelection,
    pub config: AppConfig,
    /// Path of a usable `ffmpeg` binary, when one was found.
    pub ffmpeg: Option<PathBuf>,
}

/// Controls one running recording.
pub struct Recorder {
    shared: Arc<SessionShared>,
    handle: Option<JoinHandle<()>>,
    audio: Option<AudioCapture>,
    output: PlannedOutput,
    format: ExportFormat,
    /// Details copied into the recording history once the file is written.
    source_template: EntrySource,
    history_written: bool,
}

impl Recorder {
    /// Validates the request, prepares the encoder pipeline and starts capturing.
    pub fn start(request: RecordingRequest) -> anyhow::Result<Self> {
        let mut config = request.config.clone();
        config.normalize();

        config.output_dir_status().map_err(|err| anyhow::anyhow!(err))?;

        let format = config
            .export_format(request.ffmpeg.is_some())
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;

        let fps = formats::effective_fps(format.container, config.frame_rate);
        let output = paths::plan_output(
            &config.output_dir,
            &config.filename_template,
            &request.source.file_label(),
            format.extension(),
            format.container.is_image_sequence(),
        )?;
        output.prepare()?;

        // Audio starts before the first video frame so both timelines line up.
        let audio_capture = if config.audio.is_active() {
            Some(AudioCapture::start(&config.audio)?)
        } else {
            None
        };
        let audio_ready = audio_capture.as_ref().is_some_and(|capture| !capture.is_empty());
        let audio_params = audio_capture
            .as_ref()
            .filter(|_| audio_ready)
            .map(|capture| AudioParams {
                sample_rate: capture.format().sample_rate,
                channels: capture.format().channels,
                codec: config.audio.codec,
                bitrate: config.quality.audio_bitrate(),
            });
        let mixer = audio_capture
            .as_ref()
            .filter(|_| audio_ready)
            .map(|capture| AudioMixer::from_streams(capture.format(), capture.streams()));

        let shared = Arc::new(SessionShared::new());
        shared.set_status(SessionStatus::Preparing);
        shared.set_planned_output(&output);
        shared.set_backend(format.describe());
        if let Some(capture) = audio_capture.as_ref() {
            let names = capture.names().join(" + ");
            shared.set_audio_summary(if names.is_empty() {
                "无可用音频源".to_string()
            } else {
                names
            });
        } else {
            shared.set_audio_summary("未启用".to_string());
        }

        let factory = make_factory(
            config.clone(),
            format,
            output.clone(),
            request.ffmpeg.clone(),
            audio_params,
            fps,
        );

        let job = CaptureJob {
            factory,
            shared: Arc::clone(&shared),
            fps,
            audio: mixer,
            preview: config.show_preview,
            options: CaptureOptions::from_config(&config),
            method: config.capture_method,
            test_pattern: config.test_pattern,
        };
        let source = request.source.clone();
        let thread_shared = Arc::clone(&shared);

        log::info!(
            "开始录制: {} → {} ({} fps, {}, {})",
            source.label(),
            output.path.display(),
            fps,
            format.describe(),
            config.capture_method.label()
        );

        let handle = std::thread::Builder::new()
            .name("screenrec-capture".to_string())
            .spawn(move || {
                if let Err(err) = run_capture(source, job) {
                    log::error!("捕获线程结束: {err:#}");
                    thread_shared.fail(format!("{err:#}"));
                }
            })
            .map_err(|err| anyhow::anyhow!("无法创建采集线程: {err}"))?;

        Ok(Self {
            shared,
            handle: Some(handle),
            audio: audio_capture,
            output,
            format,
            source_template: EntrySource {
                kind: match &request.source {
                    SourceSelection::Monitor { .. } => "monitor",
                    SourceSelection::Window { .. } => "window",
                    SourceSelection::Picker => "picker",
                }
                .to_string(),
                label: request.source.label(),
                container: format.container.id().to_string(),
                codec: format.codec.id().to_string(),
                frame_rate: fps,
                // GIF and image sequences have no audio track.
                audio_capable: format.container.is_video(),
            },
            history_written: false,
        })
    }

    pub fn shared(&self) -> Arc<SessionShared> {
        Arc::clone(&self.shared)
    }

    pub fn output(&self) -> &PlannedOutput {
        &self.output
    }

    pub fn format(&self) -> ExportFormat {
        self.format
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.set_paused(paused);
    }

    pub fn request_stop(&self) {
        self.shared.request_stop();
    }

    /// Blocks until the capture thread has finished and the file is closed.
    pub fn wait(mut self) -> SessionStatus {
        self.join(None);
        self.shared.status()
    }

    /// Like [`Recorder::wait`], but gives up after `grace`.
    ///
    /// A capture thread can in principle never observe the stop request (for
    /// example if the source stops producing frames entirely), and neither the
    /// CLI nor the UI may hang because of that. The recorder is still asked to
    /// stop; the returned status is simply whatever state it reached.
    pub fn wait_for(mut self, grace: Duration) -> SessionStatus {
        self.join(Some(grace));
        self.shared.status()
    }

    /// Stops the recording and returns the path of the written file.
    pub fn stop(self) -> anyhow::Result<PathBuf> {
        self.shared.request_stop();
        let shared = Arc::clone(&self.shared);
        let status = self.wait_for(Duration::from_secs(30));
        match status {
            SessionStatus::Completed => shared
                .snapshot()
                .output
                .ok_or_else(|| anyhow::anyhow!("录制已结束，但没有输出文件")),
            SessionStatus::Failed(message) => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("录制意外结束或超时：{}", other.label())),
        }
    }

    fn join(&mut self, grace: Option<Duration>) {
        if let Some(handle) = self.handle.take() {
            match grace {
                None => {
                    let _ = handle.join();
                }
                Some(grace) => {
                    let deadline = Instant::now() + grace;
                    while !handle.is_finished() {
                        if Instant::now() >= deadline {
                            log::error!(
                                "采集线程在 {:.0} 秒内没有结束；文件可能不完整",
                                grace.as_secs_f32()
                            );
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    if handle.is_finished() {
                        let _ = handle.join();
                    }
                }
            }
        }
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        self.write_history();
    }

    /// Appends the finished recording to the history file.
    ///
    /// Runs after the sink has closed, so the file size and duration are final.
    /// Failures here are logged only: a recording that was written is never
    /// reported as failed just because the history could not be updated.
    fn write_history(&mut self) {
        if self.history_written {
            return;
        }
        self.history_written = true;

        if self.shared.status() != SessionStatus::Completed {
            return;
        }

        let snapshot = self.shared.snapshot();
        let Some(entry) = RecordingEntry::from_session(&self.source_template, &snapshot) else {
            return;
        };

        let mut store = HistoryStore::load();
        match store.add(entry) {
            Ok(id) => log::info!(
                "已写入录制记录 #{id}（共 {} 条，文件：{}）",
                store.len(),
                snapshot
                    .output
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            ),
            Err(err) => log::warn!("写入录制记录失败: {err}"),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.shared.request_stop();
        }
        self.join(Some(Duration::from_secs(10)));
    }
}

/// Capture options that map onto Graphics Capture settings.
#[derive(Debug, Clone, Copy)]
struct CaptureOptions {
    cursor: CursorCaptureSettings,
    border: DrawBorderSettings,
    secondary: SecondaryWindowSettings,
    min_update_interval: MinimumUpdateIntervalSettings,
    dirty_region: DirtyRegionSettings,
}

impl CaptureOptions {
    fn from_config(config: &AppConfig) -> Self {
        Self {
            cursor: if config.capture_cursor {
                CursorCaptureSettings::WithCursor
            } else {
                CursorCaptureSettings::WithoutCursor
            },
            border: if config.capture_border {
                DrawBorderSettings::WithBorder
            } else {
                DrawBorderSettings::WithoutBorder
            },
            secondary: if config.include_secondary_windows {
                SecondaryWindowSettings::Include
            } else {
                SecondaryWindowSettings::Exclude
            },
            min_update_interval: MinimumUpdateIntervalSettings::Custom(
                config.effective_min_update_interval(),
            ),
            dirty_region: DirtyRegionSettings::Default,
        }
    }
}

/// Everything the capture thread needs.
struct CaptureJob {
    factory: SinkFactory,
    shared: Arc<SessionShared>,
    fps: u32,
    audio: Option<AudioMixer>,
    preview: bool,
    options: CaptureOptions,
    method: CaptureMethod,
    /// Feed synthetic frames instead of capturing the screen.
    test_pattern: bool,
}

/// Size of the synthetic test pattern.
const TEST_PATTERN_SIZE: (u32, u32) = (1280, 720);

/// Drives the real pipeline with generated frames.
///
/// Used by `--test-pattern` to verify encoder settings, pacing, audio and file
/// naming on a machine where screen capture itself is unavailable or blocked.
fn run_test_pattern(
    factory: SinkFactory,
    shared: Arc<SessionShared>,
    fps: u32,
    audio: Option<AudioMixer>,
    preview: bool,
) -> anyhow::Result<()> {
    let (width, height) = TEST_PATTERN_SIZE;
    let mut pipeline = FramePipeline::new(factory, Arc::clone(&shared), fps, audio, preview);
    let period = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];

    log::info!("使用内置测试图案（{width}x{height} @ {fps} fps）");

    let start = Instant::now();
    let mut index: u64 = 0;
    let mut outcome = Ok(());

    while !shared.is_stop_requested() {
        draw_test_pattern(&mut pixels, width, height, index);
        if let Err(err) = pipeline.push_bgra(&pixels, width, height) {
            outcome = Err(err);
            break;
        }
        index += 1;

        // Keep the requested cadence; when we fall behind, `tick` fills in the
        // frames the timeline expects instead of silently dropping them.
        let target = start + period * index as u32;
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        } else if let Err(err) = pipeline.tick() {
            outcome = Err(err);
            break;
        }
    }

    if let Err(err) = &outcome {
        pipeline.set_failure(format!("{err:#}"));
    }
    pipeline.finalize();
    outcome
}

/// Draws a moving, high-contrast pattern so frames differ and stay readable.
fn draw_test_pattern(pixels: &mut [u8], width: u32, height: u32, index: u64) {
    let shift = (index as u32) * 7;
    let bar = (index as u32 * 13) % width;

    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * 4) as usize;
            let checker = ((x / 48) + (y / 48)) % 2 == 0;
            let base: u8 = if checker { 40 } else { 96 };

            let (r, g, b) = if x.abs_diff(bar) < 24 {
                // Sweeping vertical bar.
                (255, 40, 40)
            } else if y < height / 3 {
                // Top band: horizontal gradient that scrolls with time.
                (((x + shift) % 256) as u8, 80, 160)
            } else if y < (height * 2) / 3 {
                // Middle band: colour ramp, changes every frame.
                (60, ((y + shift) % 256) as u8, 200)
            } else {
                // Bottom band: checkerboard with a brightness step per frame.
                let level = base.saturating_add((index % 8) as u8 * 8);
                (level, level, level)
            };

            pixels[offset] = b;
            pixels[offset + 1] = g;
            pixels[offset + 2] = r;
            pixels[offset + 3] = 255;
        }
    }
}

/// Runs the capture. Graphics Capture takes over the calling thread; the DXGI
/// loop uses it as a normal worker thread.
fn run_capture(source: SourceSelection, job: CaptureJob) -> anyhow::Result<()> {
    let CaptureJob { factory, shared, fps, audio, preview, options, method, test_pattern } = job;

    // A synthetic source keeps the whole pipeline testable without touching the
    // screen: `screenrec record --test-pattern`.
    if test_pattern {
        return run_test_pattern(factory, shared, fps, audio, preview);
    }

    let pipeline = FramePipeline::new(factory, Arc::clone(&shared), fps, audio, preview);

    match source {
        SourceSelection::Monitor { index, .. } => match method {
            CaptureMethod::Dxgi => dxgi::run(index, pipeline, shared),
            CaptureMethod::Wgc => {
                let item = monitor_item(index)?;
                start_graphics_capture(item, pipeline, shared, options)
            }
            CaptureMethod::Auto => match monitor_item(index) {
                Ok(item) => start_graphics_capture(item, pipeline, shared, options),
                Err(err) => {
                    log::warn!("图形捕获不可用（{err:#}），自动改用 DXGI 桌面复制");
                    dxgi::run(index, pipeline, shared)
                }
            },
        },
        SourceSelection::Window { hwnd, title, .. } => {
            if method.monitors_only() {
                log::warn!("DXGI 桌面复制无法录制单个窗口，已改用图形捕获");
            }
            let item = window_item(hwnd, &title)?;
            start_graphics_capture(item, pipeline, shared, options)
        }
        SourceSelection::Picker => {
            if method.monitors_only() {
                log::warn!("DXGI 桌面复制无法录制单个窗口，已改用图形捕获");
            }
            log::info!("等待用户在系统选择器中选择捕获目标…");
            let picked = GraphicsCapturePicker::pick_item()
                .map_err(|err| anyhow::anyhow!("系统选择器失败：{err}"))?
                .ok_or_else(|| anyhow::anyhow!("未选择任何画面"))?;
            let item = convert_item(picked, "所选画面")?;
            start_graphics_capture(item, pipeline, shared, options)
        }
    }
}

/// Starts the Windows Graphics Capture session on the current thread.
fn start_graphics_capture(
    item: GraphicsCaptureItemType,
    pipeline: FramePipeline,
    shared: Arc<SessionShared>,
    options: CaptureOptions,
) -> anyhow::Result<()> {
    let settings = Settings::new(
        item,
        options.cursor,
        options.border,
        options.secondary,
        options.min_update_interval,
        options.dirty_region,
        ColorFormat::Bgra8,
        HandlerFlags { pipeline, shared },
    );

    RecorderHandler::start(settings).map_err(|err| anyhow::anyhow!("采集失败：{err}"))
}

fn monitor_item(index: usize) -> anyhow::Result<GraphicsCaptureItemType> {
    let monitor = Monitor::from_index(index)
        .map_err(|err| anyhow::anyhow!("无法打开显示器 {index}：{err}"))?;
    convert_item(monitor, &format!("显示器 {index}"))
}

fn window_item(hwnd: i64, title: &str) -> anyhow::Result<GraphicsCaptureItemType> {
    let window = Window::from_raw_hwnd(hwnd as *mut std::ffi::c_void);
    anyhow::ensure!(window.is_valid(), "窗口“{title}”已经关闭");
    convert_item(window, &format!("窗口“{title}”"))
}

/// Converts a capture source into a WinRT capture item, keeping the real error.
///
/// `GraphicsCaptureApiHandler::start` discards the HRESULT, which turns
/// "access denied" into a useless "failed to convert item".
fn convert_item<T>(item: T, description: &str) -> anyhow::Result<GraphicsCaptureItemType>
where
    T: TryInto<GraphicsCaptureItemType>,
    <T as TryInto<GraphicsCaptureItemType>>::Error: std::fmt::Display,
{
    item.try_into().map_err(|err| {
        anyhow::anyhow!(
            "无法为{description}创建捕获目标：{err}。\
             请确认系统支持 Windows 图形捕获（Windows 10 1903 及以上）。"
        )
    })
}

/// Builds the closure that creates the encoder on the first frame.
fn make_factory(
    config: AppConfig,
    format: ExportFormat,
    output: PlannedOutput,
    ffmpeg: Option<PathBuf>,
    audio: Option<AudioParams>,
    fps: u32,
) -> SinkFactory {
    Box::new(move |source_width: u32, source_height: u32| {
        let (width, height) = formats::plan_size_with_cap(
            format.container,
            config.scale,
            config.max_output_width,
            source_width,
            source_height,
        );
        let bitrate = config.video_bitrate(width, height);

        let params = SinkParams { width, height, fps, bitrate, audio };

        log::info!(
            "编码器参数: {}x{} (源 {}x{}), {} fps, {} bps",
            width,
            height,
            source_width,
            source_height,
            fps,
            bitrate
        );

        let sink = encode::create_sink(encode::SinkRequest {
            format,
            params,
            output: &output,
            ffmpeg: ffmpeg.as_deref(),
            quality: config.quality,
            prefer_hardware: config.prefer_hardware_encoder,
        })?;
        Ok((sink, width, height))
    })
}

/// Helper used by the CLI to describe a source before recording.
pub fn describe_source(source: &SourceSelection) -> String {
    match source {
        SourceSelection::Monitor { index, .. } => sources::resolve_monitor(*index)
            .map(|monitor| monitor.describe())
            .unwrap_or_else(|| source.label()),
        SourceSelection::Window { hwnd, title, .. } => sources::find_window(*hwnd)
            .map(|window| window.label())
            .unwrap_or_else(|| format!("窗口“{title}”")),
        SourceSelection::Picker => "系统选择器".to_string(),
    }
}
