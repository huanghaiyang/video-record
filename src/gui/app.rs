//! The desktop application.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::RichText;

use super::theme::colors;
use super::widgets::{self, action_button, badge, hint, key_value, section};
use crate::capture::sources::{self, MonitorInfo, WindowInfo};
use crate::capture::{
    CaptureMethod, PreviewFrame, Recorder, RecordingRequest, SessionShared, SessionSnapshot,
    SessionStatus, SourceSelection,
};
use crate::config::{AppConfig, OutputDirOutcome, SavedSource, ThemeMode};
use crate::encode::capability::CapabilityMap;
use crate::ffmpeg::{self, FfmpegInfo};
use crate::history::HistoryStore;
use crate::region::Region;
use crate::formats::{self, AudioCodec, Container, QualityPreset, ScaleOption};
use crate::hotkeys::{HotkeyAction, HotkeyManager};
use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Record,
    History,
    Settings,
    About,
}

/// What the history tab wants to do with the selected recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryAction {
    Open(u64),
    Reveal(u64),
    CopyPath(u64),
    Remove(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Monitor,
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageKind {
    Info,
    Success,
    Error,
}

struct Message {
    text: String,
    kind: MessageKind,
}

pub struct ScreenRecApp {
    config: AppConfig,
    config_dirty: bool,
    last_config_change: Instant,
    tab: Tab,

    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    source_kind: SourceKind,
    window_filter: String,
    last_source_refresh: Instant,

    source: Option<SourceSelection>,

    /// True once the user edited or picked the output folder themselves.
    output_dir_explicit: bool,

    ffmpeg: Option<FfmpegInfo>,

    recorder: Option<Recorder>,
    shared: Option<Arc<SessionShared>>,
    snapshot: SessionSnapshot,
    preview_texture: Option<egui::TextureHandle>,

    message: Option<Message>,
    pending_start: Option<Instant>,

    hotkeys: Option<HotkeyManager>,
    hotkey_status: String,
    hotkey_registered: usize,

    /// Result of the runtime encoder probe, when it has finished.
    capabilities: Option<CapabilityMap>,
    capability_rx: Option<std::sync::mpsc::Receiver<CapabilityMap>>,

    /// Recording history shown in the "记录" tab.
    history: HistoryStore,
    selected_entry: Option<u64>,
    confirm_clear_history: bool,

    /// Area-recording controls. The fields keep their values while the feature
    /// is switched off so toggling it back restores what was entered.
    region_enabled: bool,
    region_fields: Region,
    /// First corner picked with "取左上角", in virtual-desktop pixels.
    region_corner: Option<(i32, i32)>,

    /// System tray icon; `None` when it could not be created (the window still
    /// works, so this is never fatal).
    tray: Option<super::tray::TrayHandle>,
    /// Floating control bar shown while recording, excluded from capture.
    float_panel: super::float_panel::FloatPanel,
}

impl ScreenRecApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        super::theme::install_fonts(&cc.egui_ctx);

        let config = AppConfig::load();
        super::theme::apply_style(&cc.egui_ctx, config.theme);

        let ffmpeg = ffmpeg::detect(&config.ffmpeg_path);
        // Read before `config` is moved into the struct below.
        let saved_region = config.region;

        let mut app = Self {
            config,
            config_dirty: false,
            last_config_change: Instant::now(),
            tab: Tab::Record,
            monitors: Vec::new(),
            windows: Vec::new(),
            source_kind: SourceKind::Monitor,
            window_filter: String::new(),
            last_source_refresh: Instant::now(),
            source: None,
            output_dir_explicit: false,
            ffmpeg,
            recorder: None,
            shared: None,
            snapshot: SessionSnapshot::default(),
            preview_texture: None,
            message: None,
            pending_start: None,
            hotkeys: None,
            hotkey_status: String::new(),
            hotkey_registered: 0,
            capabilities: None,
            capability_rx: None,
            history: HistoryStore::load(),
            selected_entry: None,
            confirm_clear_history: false,
            region_enabled: saved_region.is_some(),
            region_fields: saved_region.unwrap_or(Region::new(0, 0, 640, 360)),
            region_corner: None,
            tray: None,
            float_panel: super::float_panel::FloatPanel::new(),
        };

        // The tray is what makes "exit" reachable while the window is hidden, so
        // it is created before anything else can hide the window.
        match super::tray::TrayHandle::spawn() {
            Ok(tray) => {
                log::info!("系统托盘已创建");
                app.tray = Some(tray);
            }
            Err(err) => log::warn!("系统托盘创建失败（其余功能不受影响）：{err}"),
        }

        app.refresh_sources();
        app.restore_last_source();
        app.restart_hotkeys();
        // Encoder availability is machine specific (Media Foundation
        // transforms), so probe it in the background while the UI is usable.
        app.start_capability_probe();

        // Pick a usable recording folder before the user needs it: the default
        // lives under the profile, which can be read-only or redirected.
        app.ensure_output_dir(true);

        if app.ffmpeg.is_none() {
            app.info("未找到 FFmpeg：仍可导出 MP4 / AVI / WMV / MPEG-2 / GIF / 图像序列");
        }
        app
    }

    // ------------------------------------------------------------ helpers ----

    /// Handles the menu picks from the system tray.
    fn poll_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = self.tray.as_ref() else {
            return;
        };
        tray.set_recording(self.is_recording());

        // Collect first: acting on events needs `&mut self` while `tray` borrows it.
        let mut events = Vec::new();
        while let Some(event) = tray.try_recv() {
            events.push(event);
        }

        for event in events {
            match event {
                super::tray::TrayEvent::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                }
                super::tray::TrayEvent::Hide => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                super::tray::TrayEvent::Start => self.request_start(),
                super::tray::TrayEvent::Stop => self.stop_recording(),
                super::tray::TrayEvent::Exit => {
                    // Quitting goes through the normal close path so the
                    // recording is finalized and the config is saved.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// Draws the floating control bar while a recording is running.
    fn show_float_panel(&mut self, ctx: &egui::Context) {
        let shared = self.shared.clone();
        let snapshot = self.snapshot.clone();
        if let Some(action) = self.float_panel.show(ctx, &snapshot, shared.as_ref()) {
            match action {
                super::float_panel::FloatAction::Stop => self.stop_recording(),
            }
        }
    }

    fn is_recording(&self) -> bool {
        self.shared.as_ref().is_some_and(|shared| shared.is_active())
    }

    fn info(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), kind: MessageKind::Info });
    }

    fn success(&mut self, text: impl Into<String>) {
        self.message = Some(Message { text: text.into(), kind: MessageKind::Success });
    }

    fn error(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::error!("{text}");
        self.message = Some(Message { text, kind: MessageKind::Error });
    }

    fn touch_config(&mut self) {
        // The region lives in two places while editing: the checkbox plus the
        // numeric fields here, and `Some(region)` in the config. Keeping them in
        // step in one place avoids a mismatch when the recording starts.
        self.config.region = self.effective_region();
        self.config_dirty = true;
        self.last_config_change = Instant::now();
    }

    fn refresh_sources(&mut self) {
        self.monitors = sources::list_monitors();
        self.windows = sources::list_windows();
        self.last_source_refresh = Instant::now();

        // Keep the selection valid: fall back to the primary display when the
        // remembered monitor disappeared.
        let still_valid = match &self.source {
            Some(SourceSelection::Monitor { index, .. }) => {
                self.monitors.iter().any(|monitor| monitor.index == *index)
            }
            Some(SourceSelection::Window { hwnd, .. }) => {
                self.windows.iter().any(|window| window.hwnd == *hwnd)
            }
            _ => false,
        };

        if !still_valid {
            self.source = self
                .monitors
                .iter()
                .find(|monitor| monitor.is_primary)
                .or_else(|| self.monitors.first())
                .map(|monitor| SourceSelection::monitor(monitor.index, monitor.name.clone()));
        }
    }

    fn restore_last_source(&mut self) {
        if !self.config.remember_last_source {
            return;
        }
        match &self.config.last_source {
            Some(SavedSource::Monitor { index, name }) => {
                if self.monitors.iter().any(|monitor| monitor.index == *index) {
                    self.source = Some(SourceSelection::monitor(*index, name.clone()));
                    self.source_kind = SourceKind::Monitor;
                }
            }
            Some(SavedSource::Window { hwnd, title }) => {
                if let Some(window) = self.windows.iter().find(|window| window.hwnd == *hwnd) {
                    self.source = Some(SourceSelection::window(
                        window.hwnd,
                        window.title.clone(),
                        window.process.clone(),
                    ));
                    self.source_kind = SourceKind::Window;
                } else if !title.is_empty() {
                    log::info!("上次使用的窗口“{title}”已不存在");
                }
            }
            None => {}
        }
    }

    fn restart_hotkeys(&mut self) {
        self.hotkeys = None;
        self.hotkey_registered = 0;

        if !self.config.hotkeys_enabled {
            self.hotkey_status = "已关闭".to_string();
            return;
        }

        let bindings = vec![
            (HotkeyAction::StartStop, self.config.hotkey_start_stop.clone()),
            (HotkeyAction::PauseResume, self.config.hotkey_pause.clone()),
            (HotkeyAction::Screenshot, self.config.hotkey_screenshot.clone()),
        ];

        match HotkeyManager::start(&bindings) {
            Some(manager) => {
                self.hotkey_registered = manager.registered_count();
                self.hotkey_status = format!("已注册 {} 个全局快捷键", self.hotkey_registered);
                self.hotkeys = Some(manager);
            }
            None => {
                self.hotkey_status = "注册失败（可能被其他程序占用）".to_string();
            }
        }
    }

    // ------------------------------------------------------------ actions ----

    fn request_start(&mut self) {
        if self.is_recording() || self.pending_start.is_some() {
            return;
        }
        if self.config.countdown_seconds > 0 {
            self.pending_start =
                Some(Instant::now() + Duration::from_secs(self.config.countdown_seconds as u64));
            self.info(format!("{} 秒后开始录制…", self.config.countdown_seconds));
        } else {
            self.begin_recording();
        }
    }

    fn begin_recording(&mut self) {
        let Some(source) = self.source.clone() else {
            self.error("请先在左侧选择一个录制源");
            return;
        };

        // Last chance to end up somewhere writable rather than losing the take.
        self.ensure_output_dir(true);
        if let Err(message) = self.config.output_dir_status() {
            self.error(message);
            return;
        }

        self.config.remember_source(match &source {
            SourceSelection::Monitor { index, name } => {
                SavedSource::Monitor { index: *index, name: name.clone() }
            }
            SourceSelection::Window { hwnd, title, .. } => {
                SavedSource::Window { hwnd: *hwnd, title: title.clone() }
            }
            SourceSelection::Picker => SavedSource::Monitor { index: 0, name: String::new() },
        });
        let _ = self.config.save();

        let request = RecordingRequest {
            source,
            config: self.config.clone(),
            ffmpeg: self.ffmpeg.as_ref().map(|info| info.path.clone()),
        };

        match Recorder::start(request) {
            Ok(recorder) => {
                self.shared = Some(recorder.shared());
                self.recorder = Some(recorder);
                self.preview_texture = None;
                self.snapshot = SessionSnapshot::default();
                self.info("已开始录制");
            }
            Err(err) => self.error(format!("{err:#}")),
        }
    }

    fn stop_recording(&mut self) {
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        let reveal = recorder.output().reveal_target();

        match recorder.stop() {
            Ok(path) => {
                self.success(format!("录制已保存：{}", path.display()));
                if self.config.open_folder_when_done {
                    if let Err(err) = opener::open(&reveal) {
                        log::warn!("打开录制文件夹失败: {err}");
                    }
                }
            }
            Err(err) => self.error(format!("{err:#}")),
        }

        self.shared = None;
        self.preview_texture = None;
        self.snapshot = SessionSnapshot::default();
        // The recorder appended the entry as it closed the file.
        self.reload_history();
    }

    /// Called when the capture thread stopped on its own (source closed, error).
    fn finish_session(&mut self) {
        let Some(recorder) = self.recorder.take() else {
            self.shared = None;
            return;
        };

        match recorder.wait_for(Duration::from_secs(20)) {
            SessionStatus::Completed => {
                let path = recorder_path_hint(&self.snapshot);
                self.success(format!("录制已保存：{path}"));
                if self.config.open_folder_when_done {
                    if let Some(output) = &self.snapshot.output {
                        let dir = output.parent().map(PathBuf::from);
                        if let Some(dir) = dir {
                            let _ = opener::open(dir);
                        }
                    }
                }
            }
            SessionStatus::Failed(message) => self.error(message),
            other => self.info(format!("录制已结束：{}", other.label())),
        }

        self.shared = None;
        self.preview_texture = None;
        self.reload_history();
    }

    fn toggle_pause(&mut self) {
        let Some(shared) = self.shared.clone() else { return };
        let paused = !shared.is_paused();
        shared.set_paused(paused);
        self.info(if paused { "已暂停" } else { "已继续" });
    }

    fn take_screenshot(&mut self) {
        let index = match &self.source {
            Some(SourceSelection::Monitor { index, .. }) => *index,
            _ => self
                .monitors
                .iter()
                .find(|monitor| monitor.is_primary)
                .map(|monitor| monitor.index)
                .unwrap_or(0),
        };

        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let dir = self.config.output_dir.clone();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            self.error(format!("无法创建录制文件夹：{err}"));
            return;
        }
        let path = paths::unique_path(&dir, &format!("screenshot_{stamp}"), "png");

        match crate::capture::screenshot::capture_monitor_to_file(index, &path) {
            Ok(size) => self.success(format!(
                "截图已保存：{}（{}）",
                path.display(),
                widgets::format_size(size)
            )),
            Err(err) => self.error(format!("{err:#}")),
        }
    }

    fn handle_hotkey(&mut self, action: HotkeyAction) {
        match action {
            HotkeyAction::StartStop => {
                if self.is_recording() {
                    self.stop_recording();
                } else {
                    self.request_start();
                }
            }
            HotkeyAction::PauseResume => self.toggle_pause(),
            HotkeyAction::Screenshot => self.take_screenshot(),
        }
    }

    /// Validates the output folder, optionally relocating to a writable one.
    fn ensure_output_dir(&mut self, allow_fallback: bool) {
        let explicit = self.output_dir_explicit;
        match self.config.ensure_output_dir(allow_fallback && !explicit) {
            OutputDirOutcome::Ok => {}
            outcome @ OutputDirOutcome::Relocated { .. } => {
                self.output_dir_explicit = false;
                self.touch_config();
                if let Some(message) = outcome.message() {
                    log::warn!("{message}");
                    self.info(message);
                }
            }
            outcome @ OutputDirOutcome::Failed { .. } => {
                if let Some(message) = outcome.message() {
                    self.error(message);
                }
            }
        }
    }

    fn redetect_ffmpeg(&mut self) {
        self.ffmpeg = ffmpeg::detect(&self.config.ffmpeg_path);
        match &self.ffmpeg {
            Some(info) => self.success(format!("已找到 FFmpeg：{}", info.summary())),
            None => self.info("仍未找到 FFmpeg，仅内置编码器可用"),
        }
    }

    /// Starts (or restarts) the background encoder probe.
    fn start_capability_probe(&mut self) {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("screenrec-capability".to_string())
            .spawn(move || {
                let map = crate::encode::capability::probe_all();
                let _ = sender.send(map);
            })
            .ok();
        self.capabilities = None;
        self.capability_rx = Some(receiver);
    }

    /// Whether a container/codec pair can actually be produced right now.
    ///
    /// While the probe is still running, native combinations are assumed to
    /// work so the UI does not flicker; once it finishes, missing Media
    /// Foundation transforms are hidden instead of failing at save time.
    fn codec_available(&self, container: Container, codec: crate::formats::VideoCodec) -> bool {
        match formats::resolve(container, codec, self.ffmpeg.is_some()) {
            Ok(format) => {
                if format.backend == crate::formats::Backend::NativeVideo {
                    self.capabilities
                        .as_ref()
                        .and_then(|map| map.is_supported(container, codec))
                        .unwrap_or(true)
                } else {
                    true
                }
            }
            Err(_) => false,
        }
    }

    fn codec_unavailable_reason(
        &self,
        container: Container,
        codec: crate::formats::VideoCodec,
    ) -> Option<String> {
        let map = self.capabilities.as_ref()?;
        if map.is_supported(container, codec) == Some(false) {
            Some(
                map.reason(container, codec)
                    .unwrap_or("本机不支持该编码组合")
                    .to_string(),
            )
        } else {
            None
        }
    }

    /// A container is offered when at least one of its codecs can be produced.
    fn container_available(&self, container: Container) -> bool {
        formats::codecs_for(container)
            .iter()
            .any(|codec| self.codec_available(container, *codec))
    }

    // --------------------------------------------------------------- frame ----

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(shared) = self.shared.clone() {
            if let Some(preview) = shared.take_preview() {
                self.upload_preview(ctx, preview);
            }
            let snapshot = shared.snapshot();
            let ended = !snapshot.status.is_active();
            self.snapshot = snapshot;

            if ended {
                self.finish_session();
            } else {
                ctx.request_repaint_after(Duration::from_millis(120));
            }
        }

        if let Some(deadline) = self.pending_start {
            if Instant::now() >= deadline {
                self.pending_start = None;
                self.begin_recording();
            } else {
                ctx.request_repaint_after(Duration::from_millis(80));
            }
        }

        if let Some(hotkeys) = self.hotkeys.as_ref() {
            for action in hotkeys.poll() {
                self.handle_hotkey(action);
            }
        }

        if let Some(receiver) = self.capability_rx.take() {
            match receiver.try_recv() {
                Ok(map) => {
                    log::info!("编码器检测完成：{}", map.summary());
                    self.capabilities = Some(map);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.capability_rx = Some(receiver);
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }

        if self.config_dirty && self.last_config_change.elapsed() > Duration::from_secs(2) {
            self.config_dirty = false;
            if let Err(err) = self.config.save() {
                log::warn!("保存配置失败: {err}");
            }
        }

        if !self.is_recording() && self.last_source_refresh.elapsed() > Duration::from_secs(5) {
            self.refresh_sources();
        }
    }

    fn upload_preview(&mut self, ctx: &egui::Context, preview: PreviewFrame) {
        if preview.width == 0 || preview.height == 0 {
            return;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [preview.width as usize, preview.height as usize],
            &preview.rgba,
        );
        match self.preview_texture.as_mut() {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.preview_texture = Some(ctx.load_texture(
                    "screenrec-preview",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
    }

    // ---------------------------------------------------------------- views ----

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("ScreenRec").size(20.0).strong());
            ui.label(
                RichText::new(format!("v{}", crate::VERSION))
                    .color(colors::MUTED)
                    .small(),
            );
            ui.separator();

            ui.selectable_value(&mut self.tab, Tab::Record, "录制");
            let history_label = if self.history.is_empty() {
                "记录".to_string()
            } else {
                format!("记录 ({})", self.history.len())
            };
            ui.selectable_value(&mut self.tab, Tab::History, history_label);
            ui.selectable_value(&mut self.tab, Tab::Settings, "设置");
            ui.selectable_value(&mut self.tab, Tab::About, "关于");

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("打开录制文件夹").clicked() {
                    let dir = self.config.output_dir.clone();
                    if let Err(err) = std::fs::create_dir_all(&dir) {
                        self.error(format!("无法创建录制文件夹：{err}"));
                    } else if let Err(err) = opener::open(&dir) {
                        self.error(format!("无法打开文件夹：{err}"));
                    }
                }
                if ui.button("刷新录制源").clicked() {
                    self.refresh_sources();
                }

                match &self.ffmpeg {
                    Some(_) => badge(ui, "FFmpeg 已就绪", colors::OK),
                    None => badge(ui, "FFmpeg 未安装", colors::WARN),
                };
            });
        });
    }

    fn source_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(RichText::new("录制源").strong().size(15.0));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.source_kind, SourceKind::Monitor, "整块屏幕");
            ui.selectable_value(&mut self.source_kind, SourceKind::Window, "单个窗口");
        });
        ui.add_space(4.0);

        match self.source_kind {
            SourceKind::Monitor => self.monitor_list(ui),
            SourceKind::Window => self.window_list(ui),
        }

        ui.add_space(6.0);
        if ui
            .button("使用系统选择器…")
            .on_hover_text("打开 Windows 自带的捕获选择器，可选择窗口或屏幕")
            .clicked()
        {
            self.source = Some(SourceSelection::Picker);
            self.info("已选择“系统选择器”，点击开始录制后会弹出选择框");
        }

        self.region_controls(ui);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new("当前选择").strong());
        match &self.source {
            Some(source) => {
                ui.label(source.label());
                if let Some((width, height)) = source_resolution(source, &self.monitors, &self.windows)
                {
                    hint(ui, format!("分辨率 {width}×{height}"));
                }
            }
            None => hint(ui, "尚未选择"),
        }
        if let Some(region) = self.effective_region() {
            ui.label(
                RichText::new(format!("只录制区域 {}", region.label()))
                    .color(colors::ACCENT)
                    .small(),
            );
        }
    }

    /// Area-recording controls: numeric fields plus picking the two corners
    /// with the mouse, which is pixel-exact because the cursor is read in
    /// physical screen coordinates.
    fn region_controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.region_enabled, "只录制一块区域")
                .on_hover_text("只编码所选矩形内的画面，坐标相对于所录显示器/窗口的左上角")
                .changed()
            {
                self.region_corner = None;
                self.touch_config();
            }
        });

        if !self.region_enabled {
            return;
        }

        let mut fields = self.region_fields;
        let mut changed = false;

        egui::Grid::new("region_fields")
            .num_columns(4)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                ui.label("X");
                changed |= ui.add(egui::DragValue::new(&mut fields.x).range(0..=32_768)).changed();
                ui.label("Y");
                changed |= ui.add(egui::DragValue::new(&mut fields.y).range(0..=32_768)).changed();
                ui.end_row();

                ui.label("宽");
                changed |= ui
                    .add(egui::DragValue::new(&mut fields.width).range(Region::MIN_SIZE..=32_768))
                    .changed();
                ui.label("高");
                changed |= ui
                    .add(egui::DragValue::new(&mut fields.height).range(Region::MIN_SIZE..=32_768))
                    .changed();
                ui.end_row();
            });

        if changed {
            self.region_fields = fields;
            self.touch_config();
        }

        ui.horizontal(|ui| {
            let has_corner = self.region_corner.is_some();
            if ui
                .button("取第一角")
                .on_hover_text("把鼠标移到区域的一个角，然后点这里")
                .clicked()
            {
                match crate::capture::sources::cursor_position() {
                    Some(point) => {
                        self.region_corner = Some(point);
                        self.info(format!("已记录第一个角：({}, {})，请把鼠标移到对角再点“取对角”", point.0, point.1));
                    }
                    None => self.error("无法读取鼠标位置"),
                }
            }
            if ui
                .add_enabled(has_corner, egui::Button::new("取对角"))
                .on_hover_text("把鼠标移到区域的另一个角，然后点这里")
                .clicked()
            {
                self.finish_region_pick();
            }
            if ui.button("重置为全屏").clicked() {
                self.region_enabled = false;
                self.region_corner = None;
                self.touch_config();
            }
        });

        if self.region_corner.is_some() {
            hint(ui, "已记住第一个角；移动鼠标后点“取对角”完成选择（Esc 可取消）");
        }

        // Warn when the region reaches past the source, since capture clamps it.
        if let Some((width, height)) = self.current_source_size() {
            if self.region_fields.right() > width || self.region_fields.bottom() > height {
                ui.label(
                    RichText::new(format!(
                        "⚠ 区域超出画面 {width}×{height}，录制时会被裁剪到边界内"
                    ))
                    .color(colors::WARN)
                    .small(),
                );
            }
        }
    }

    /// Turns the remembered corner plus the current cursor into a region.
    fn finish_region_pick(&mut self) {
        let Some(from) = self.region_corner.take() else {
            return;
        };
        let Some(to) = crate::capture::sources::cursor_position() else {
            self.error("无法读取鼠标位置");
            return;
        };

        // The region is relative to the monitor the first corner sits on, which
        // is the monitor the user was pointing at.
        let Some(monitor) = crate::capture::sources::monitor_at(from) else {
            self.error("找不到鼠标所在的显示器");
            return;
        };

        let region = monitor.region_from_corners(from, to);
        if !region.is_valid() {
            self.error(format!(
                "选中的区域太小（{}），宽高至少 {} 像素",
                region.label(),
                Region::MIN_SIZE
            ));
            return;
        }

        self.region_fields = region;
        self.region_enabled = true;
        self.touch_config();
        self.success(format!("已选择区域 {}", region.label()));
    }

    /// The region that will actually be recorded.
    fn effective_region(&self) -> Option<Region> {
        self.region_enabled.then_some(self.region_fields)
    }

    /// Size of the currently selected capture source, when known.
    fn current_source_size(&self) -> Option<(u32, u32)> {
        self.source
            .as_ref()
            .and_then(|source| source_resolution(source, &self.monitors, &self.windows))
    }

    fn monitor_list(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .max_height(240.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.monitors.is_empty() {
                    hint(ui, "没有检测到显示器");
                }
                for monitor in self.monitors.clone() {
                    let selected = matches!(
                        &self.source,
                        Some(SourceSelection::Monitor { index, .. }) if *index == monitor.index
                    );
                    let label = monitor.describe();
                    if ui.selectable_label(selected, label).clicked() {
                        self.source = Some(SourceSelection::monitor(
                            monitor.index,
                            monitor.name.clone(),
                        ));
                    }
                }
            });
    }

    fn window_list(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.window_filter)
                    .hint_text("按标题或进程名筛选")
                    .desired_width(ui.available_width() - 8.0),
            );
        });

        let filtered: Vec<WindowInfo> = self
            .windows
            .iter()
            .filter(|window| window.matches(&self.window_filter))
            .cloned()
            .collect();

        hint(ui, format!("共 {} 个可录制窗口", filtered.len()));

        egui::ScrollArea::vertical()
            .max_height(360.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if filtered.is_empty() {
                    hint(ui, "没有匹配的窗口");
                }
                for window in filtered {
                    let selected = matches!(
                        &self.source,
                        Some(SourceSelection::Window { hwnd, .. }) if *hwnd == window.hwnd
                    );
                    let label = format!(
                        "{}{}",
                        if window.is_foreground { "● " } else { "" },
                        window.label()
                    );
                    let response = ui.selectable_label(selected, label);
                    if response.clicked() {
                        self.source = Some(SourceSelection::window(
                            window.hwnd,
                            window.title.clone(),
                            window.process.clone(),
                        ));
                    }
                }
            });
    }

    fn record_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.control_card(ui);
                self.preview_card(ui);
                self.format_card(ui);
                self.audio_card(ui);
                self.watermark_card(ui);
                self.output_card(ui);
            });
    }

    fn control_card(&mut self, ui: &mut egui::Ui) {
        section(ui, "录制控制", |ui| {
            let recording = self.is_recording();
            let snapshot = self.snapshot.clone();

            ui.horizontal(|ui| {
                if recording {
                    let paused = self
                        .shared
                        .as_ref()
                        .is_some_and(|shared| shared.is_paused());

                    if action_button(
                        ui,
                        if paused { "继续录制" } else { "暂停" },
                        colors::PAUSE,
                        true,
                    )
                    .clicked()
                    {
                        self.toggle_pause();
                    }

                    if action_button(ui, "停止并保存", colors::RECORD, true).clicked() {
                        self.stop_recording();
                    }
                } else if let Some(deadline) = self.pending_start {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if action_button(
                        ui,
                        &format!("取消（{:.0} 秒后开始）", remaining.as_secs_f32().ceil()),
                        colors::MUTED,
                        true,
                    )
                    .clicked()
                    {
                        self.pending_start = None;
                        self.info("已取消");
                    }
                } else {
                    if action_button(ui, "开始录制", colors::RECORD, self.source.is_some()).clicked()
                    {
                        self.request_start();
                    }
                    if self.source.is_none() {
                        hint(ui, "请先选择录制源");
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("截图").clicked() {
                        self.take_screenshot();
                    }
                    ui.label(format!("倒计时 {} 秒", self.config.countdown_seconds));
                });
            });

            if recording {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(widgets::format_duration(snapshot.elapsed))
                            .size(26.0)
                            .strong()
                            .monospace(),
                    );
                    ui.add_space(8.0);
                    match &snapshot.status {
                        SessionStatus::Recording => badge(ui, "录制中", colors::RECORD),
                        SessionStatus::Paused => badge(ui, "已暂停", colors::PAUSE),
                        SessionStatus::Preparing => badge(ui, "准备中", colors::ACCENT),
                        SessionStatus::Stopping => badge(ui, "保存中", colors::ACCENT),
                        other => badge(ui, &other.label(), colors::MUTED),
                    };
                });
                ui.horizontal(|ui| {
                    key_value(ui, "帧数", snapshot.frames.to_string());
                    key_value(ui, "文件大小", widgets::format_size(snapshot.file_size));
                    if let Some((width, height)) = snapshot.output_size {
                        key_value(ui, "输出", format!("{width}×{height}"));
                    }
                });
                if !snapshot.backend.is_empty() {
                    hint(ui, format!("编码器：{}", snapshot.backend));
                }
                if !snapshot.audio.is_empty() {
                    hint(ui, format!("音频：{}", snapshot.audio));
                }
                if let Some(path) = &snapshot.planned_output {
                    hint(ui, format!("输出文件：{}", path.display()));
                }
            }
        });
    }

    fn preview_card(&mut self, ui: &mut egui::Ui) {
        if !self.config.show_preview {
            return;
        }
        section(ui, "实时预览", |ui| match &self.preview_texture {
            Some(texture) => {
                let size = texture.size_vec2();
                let available = ui.available_width().max(120.0);
                let scale = (available / size.x).min(1.0);
                let target = size * scale;
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::new(texture.id(), target))
                        .corner_radius(4.0),
                );
            }
            None => {
                hint(ui, "开始录制后这里会显示实时画面");
            }
        });
    }

    fn format_card(&mut self, ui: &mut egui::Ui) {
        let ffmpeg_available = self.ffmpeg.is_some();

        section(ui, "导出格式", |ui| {
            egui::Grid::new("format_grid")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label("容器格式");
                    egui::ComboBox::from_id_salt("container")
                        .selected_text(self.config.container.label())
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for container in Container::ALL {
                                let needs_ffmpeg = container.requires_ffmpeg();
                                let available = self.container_available(container);
                                let text = if !available && needs_ffmpeg && !ffmpeg_available {
                                    format!("{}（需要 FFmpeg）", container.label())
                                } else if !available {
                                    format!("{}（本机不支持）", container.label())
                                } else {
                                    container.label().to_string()
                                };
                                if ui
                                    .add_enabled_ui(available, |ui| {
                                        ui.selectable_value(
                                            &mut self.config.container,
                                            container,
                                            text,
                                        )
                                    })
                                    .inner
                                    .clicked()
                                {
                                    let codecs = formats::codecs_for(self.config.container);
                                    if !codecs.contains(&self.config.video_codec) {
                                        self.config.video_codec = codecs[0];
                                    }
                                    self.touch_config();
                                }
                            }
                        });
                    ui.end_row();

                    if self.config.container.is_video() {
                        ui.label("视频编码");
                        egui::ComboBox::from_id_salt("codec")
                            .selected_text(self.config.video_codec.label())
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                for codec in formats::codecs_for(self.config.container) {
                                    let available = self.codec_available(self.config.container, *codec);
                                    let text = if available {
                                        codec.label().to_string()
                                    } else if let Some(reason) =
                                        self.codec_unavailable_reason(self.config.container, *codec)
                                    {
                                        format!("{}（本机不支持：{}）", codec.label(), reason)
                                    } else {
                                        format!("{}（需要 FFmpeg）", codec.label())
                                    };
                                    if ui
                                        .add_enabled_ui(available, |ui| {
                                            ui.selectable_value(
                                                &mut self.config.video_codec,
                                                *codec,
                                                text,
                                            )
                                        })
                                        .inner
                                        .clicked()
                                    {
                                        self.touch_config();
                                    }
                                }
                            });
                        ui.end_row();
                    }

                    ui.label("画质");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("quality")
                            .selected_text(self.config.quality.label())
                            .width(180.0)
                            .show_ui(ui, |ui| {
                                for preset in QualityPreset::ALL {
                                    if ui
                                        .selectable_value(
                                            &mut self.config.quality,
                                            preset,
                                            preset.label(),
                                        )
                                        .clicked()
                                    {
                                        self.touch_config();
                                    }
                                }
                            });
                        if self.config.quality == QualityPreset::Custom {
                            let mut kbps = self.config.custom_bitrate_kbps;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut kbps)
                                        .range(200..=200_000)
                                        .suffix(" kbps"),
                                )
                                .changed()
                            {
                                self.config.custom_bitrate_kbps = kbps;
                                self.touch_config();
                            }
                        }
                    });
                    ui.end_row();

                    ui.label("帧率");
                    ui.horizontal(|ui| {
                        let mut fps = self.config.frame_rate;
                        if ui
                            .add(egui::Slider::new(&mut fps, 1..=120).suffix(" fps"))
                            .changed()
                        {
                            self.config.frame_rate = fps;
                            self.touch_config();
                        }
                        if self.config.container == Container::Gif {
                            hint(ui, "GIF 会被限制为 15 fps / 640 像素宽");
                        }
                    });
                    ui.end_row();

                    ui.label("缩放");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("scale")
                            .selected_text(self.config.scale.label())
                            .width(160.0)
                            .show_ui(ui, |ui| {
                                for option in ScaleOption::ALL {
                                    if ui
                                        .selectable_value(
                                            &mut self.config.scale,
                                            option,
                                            option.label(),
                                        )
                                        .clicked()
                                    {
                                        self.touch_config();
                                    }
                                }
                            });
                        let mut width = self.config.max_output_width;
                        if ui
                            .add(
                                egui::DragValue::new(&mut width)
                                    .range(0..=7680)
                                    .prefix("宽度上限 ")
                                    .suffix(" px"),
                            )
                            .changed()
                        {
                            self.config.max_output_width = width;
                            self.touch_config();
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(6.0);
            let fps = formats::effective_fps(self.config.container, self.config.frame_rate);
            let source_size = self
                .source
                .as_ref()
                .and_then(|source| source_resolution(source, &self.monitors, &self.windows))
                .unwrap_or((1920, 1080));
            let (output_width, output_height) = formats::plan_size_with_cap(
                self.config.container,
                self.config.scale,
                self.config.max_output_width,
                source_size.0,
                source_size.1,
            );
            let bitrate = self.config.video_bitrate(output_width, output_height);

            key_value(
                ui,
                "预计输出",
                format!(
                    "{}×{} · {} fps · {} Mbps",
                    output_width,
                    output_height,
                    fps,
                    bitrate / 1_000_000
                ),
            );
            match self.config.export_format(ffmpeg_available) {
                Ok(format) => key_value(ui, "编码后端", format.describe()),
                Err(err) => {
                    ui.label(RichText::new(format!("⚠ {err}")).color(colors::WARN));
                }
            }
        });
    }

    fn audio_card(&mut self, ui: &mut egui::Ui) {
        section(ui, "声音", |ui| {
            if ui
                .checkbox(&mut self.config.audio.enabled, "录制声音")
                .changed()
            {
                self.touch_config();
            }

            ui.add_enabled_ui(self.config.audio.enabled, |ui| {
                if ui
                    .checkbox(&mut self.config.audio.system, "系统声音（扬声器回环）")
                    .changed()
                {
                    self.touch_config();
                }
                if ui
                    .checkbox(&mut self.config.audio.microphone, "麦克风")
                    .changed()
                {
                    self.touch_config();
                }

                ui.horizontal(|ui| {
                    ui.label("系统音量");
                    let mut volume = self.config.audio.system_volume;
                    if ui
                        .add(egui::Slider::new(&mut volume, 0.0..=2.0).fixed_decimals(2))
                        .changed()
                    {
                        self.config.audio.system_volume = volume;
                        self.touch_config();
                    }
                    ui.label("麦克风音量");
                    let mut mic = self.config.audio.microphone_volume;
                    if ui
                        .add(egui::Slider::new(&mut mic, 0.0..=2.0).fixed_decimals(2))
                        .changed()
                    {
                        self.config.audio.microphone_volume = mic;
                        self.touch_config();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("音频编码");
                    egui::ComboBox::from_id_salt("audio_codec")
                        .selected_text(self.config.audio.codec.label())
                        .width(180.0)
                        .show_ui(ui, |ui| {
                            for codec in [AudioCodec::Aac, AudioCodec::Mp3, AudioCodec::Flac, AudioCodec::Pcm, AudioCodec::Opus] {
                                let enabled = codec.is_native() || self.ffmpeg.is_some();
                                if ui
                                    .add_enabled_ui(enabled, |ui| {
                                        ui.selectable_value(
                                            &mut self.config.audio.codec,
                                            codec,
                                            codec.label(),
                                        )
                                    })
                                    .inner
                                    .clicked()
                                {
                                    self.touch_config();
                                }
                            }
                        });
                });

                hint(
                    ui,
                    "系统声音使用 WASAPI 回环；若设备不支持，录制仍会继续但没有声音。",
                );
            });
        });
    }

    /// Watermark controls: text, pattern, motion and repeat mode.
    fn watermark_card(&mut self, ui: &mut egui::Ui) {
        use crate::watermark::{
            WatermarkAnchor, WatermarkMotion, WatermarkPattern, WatermarkRepeat,
        };

        section(ui, "水印", |ui| {
            let mut changed = false;
            changed |= ui
                .checkbox(&mut self.config.watermark.enabled, "启用水印（写入录制画面）")
                .changed();

            ui.add_enabled_ui(self.config.watermark.enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("文字");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut self.config.watermark.text)
                                .hint_text("例如：内部资料 请勿外传")
                                .desired_width(240.0),
                        )
                        .changed();
                });

                ui.horizontal(|ui| {
                    ui.label("图案");
                    egui::ComboBox::from_id_salt("wm_pattern")
                        .selected_text(self.config.watermark.pattern.label())
                        .width(96.0)
                        .show_ui(ui, |ui| {
                            for value in WatermarkPattern::ALL {
                                changed |= ui
                                    .selectable_value(
                                        &mut self.config.watermark.pattern,
                                        value,
                                        value.label(),
                                    )
                                    .changed();
                            }
                        });

                    ui.label("位置");
                    egui::ComboBox::from_id_salt("wm_anchor")
                        .selected_text(self.config.watermark.anchor.label())
                        .width(72.0)
                        .show_ui(ui, |ui| {
                            for value in WatermarkAnchor::ALL {
                                changed |= ui
                                    .selectable_value(
                                        &mut self.config.watermark.anchor,
                                        value,
                                        value.label(),
                                    )
                                    .changed();
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("运动");
                    egui::ComboBox::from_id_salt("wm_motion")
                        .selected_text(self.config.watermark.motion.label())
                        .width(96.0)
                        .show_ui(ui, |ui| {
                            for value in WatermarkMotion::ALL {
                                changed |= ui
                                    .selectable_value(
                                        &mut self.config.watermark.motion,
                                        value,
                                        value.label(),
                                    )
                                    .changed();
                            }
                        });

                    ui.label("重复");
                    egui::ComboBox::from_id_salt("wm_repeat")
                        .selected_text(self.config.watermark.repeat.label())
                        .width(84.0)
                        .show_ui(ui, |ui| {
                            for value in WatermarkRepeat::ALL {
                                changed |= ui
                                    .selectable_value(
                                        &mut self.config.watermark.repeat,
                                        value,
                                        value.label(),
                                    )
                                    .changed();
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("字号");
                    let mut size = self.config.watermark.font_size;
                    if ui
                        .add(egui::Slider::new(&mut size, 12..=160).suffix(" px"))
                        .changed()
                    {
                        self.config.watermark.font_size = size;
                        changed = true;
                    }
                    ui.label("不透明度");
                    let mut opacity = self.config.watermark.opacity;
                    if ui
                        .add(egui::Slider::new(&mut opacity, 0.05..=1.0).fixed_decimals(2))
                        .changed()
                    {
                        self.config.watermark.opacity = opacity;
                        changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("颜色");
                    let mut colour = egui::Color32::from_rgb(
                        self.config.watermark.color[0],
                        self.config.watermark.color[1],
                        self.config.watermark.color[2],
                    );
                    if ui.color_edit_button_srgba(&mut colour).changed() {
                        self.config.watermark.color = [colour.r(), colour.g(), colour.b()];
                        changed = true;
                    }
                    ui.label("速度");
                    let mut speed = self.config.watermark.speed;
                    if ui
                        .add(egui::Slider::new(&mut speed, 4.0..=400.0).suffix(" px/s"))
                        .changed()
                    {
                        self.config.watermark.speed = speed;
                        changed = true;
                    }
                    ui.label("边距");
                    let mut margin = self.config.watermark.margin;
                    if ui
                        .add(egui::Slider::new(&mut margin, 0..=200).suffix(" px"))
                        .changed()
                    {
                        self.config.watermark.margin = margin;
                        changed = true;
                    }
                });
            });

            if changed {
                self.touch_config();
            }
            hint(
                ui,
                "水印在缩放之后叠加到输出画面；运动与平铺都是时间的纯函数，暂停恢复不会跳变。",
            );
        });
    }

    fn output_card(&mut self, ui: &mut egui::Ui) {
        section(ui, "保存位置", |ui| {
            let mut dir = self.config.output_dir.clone();
            if widgets::folder_row(ui, "录制文件夹", &mut dir) {
                self.config.output_dir = dir;
                self.output_dir_explicit = true;
                self.touch_config();
            }

            ui.horizontal(|ui| {
                ui.label("文件名模板");
                let mut template = self.config.filename_template.clone();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut template)
                            .desired_width(ui.available_width() - 8.0),
                    )
                    .changed()
                {
                    self.config.filename_template = template;
                    self.touch_config();
                }
            });
            hint(ui, "可用变量：{date} {time} {datetime} {source}");

            let context = paths::TemplateContext::now("Display1");
            let stem = paths::render_template(&self.config.filename_template, &context);
            key_value(
                ui,
                "文件名示例",
                format!("{stem}.{}", self.config.container.extension()),
            );

            if ui
                .checkbox(&mut self.config.open_folder_when_done, "保存后打开文件夹")
                .changed()
            {
                self.touch_config();
            }

            match self.config.output_dir_status() {
                Ok(()) => {
                    ui.label(RichText::new("✔ 文件夹可写").color(colors::OK).small());
                }
                Err(err) => {
                    ui.label(RichText::new(format!("⚠ {err}")).color(colors::ERROR));
                    if ui
                        .button("自动选择可用文件夹")
                        .on_hover_text("在用户目录、程序目录和临时目录中寻找第一个可写的位置")
                        .clicked()
                    {
                        // Ignore the "explicit" flag: the user asked for this.
                        self.output_dir_explicit = false;
                        self.ensure_output_dir(true);
                    }
                }
            }
        });
    }

    /// "记录" tab: past recordings, with actions for the selected one.
    ///
    /// Recordings are appended by [`crate::capture::Recorder`] as soon as they
    /// finish, so this list also covers recordings started from the CLI.
    fn history_tab(&mut self, ui: &mut egui::Ui) {
        let count = self.history.len();
        let total_bytes = self.history.total_bytes();
        let missing = count - self.history.existing_count();
        let output_dir = self.config.output_dir.clone();

        // The UI closures only touch these locals; actions are applied below so
        // that `self` is never borrowed twice.
        let mut select: Option<u64> = None;
        let mut action: Option<HistoryAction> = None;
        let mut open_output_dir = false;
        let mut refresh = false;
        let mut clear = false;

        ui.horizontal(|ui| {
            ui.heading("录制记录");
            ui.label(
                RichText::new(format!(
                    "共 {count} 条 · 合计 {}",
                    crate::units::format_size(total_bytes)
                ))
                .color(colors::MUTED)
                .small(),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.confirm_clear_history {
                    if ui
                        .button(RichText::new("确定清空").color(colors::ERROR))
                        .on_hover_text("只删除记录，不会删除录制文件")
                        .clicked()
                    {
                        clear = true;
                    }
                    if ui.button("取消").clicked() {
                        self.confirm_clear_history = false;
                    }
                } else if ui
                    .add_enabled(!self.history.is_empty(), egui::Button::new("清空记录"))
                    .on_hover_text("只删除记录，不会删除录制文件")
                    .clicked()
                {
                    self.confirm_clear_history = true;
                }

                if ui.button("刷新").clicked() {
                    refresh = true;
                }
                if ui.button("打开录制文件夹").clicked() {
                    open_output_dir = true;
                }
            });
        });

        if missing > 0 {
            ui.label(
                RichText::new(format!(
                    "⚠ 有 {missing} 条记录对应的文件已不在原位置（可能是被移动或删除）"
                ))
                .color(colors::WARN)
                .small(),
            );
        }

        ui.separator();

        if count == 0 {
            widgets::hint(ui, "还没有录制记录。完成一次录制后会自动出现在这里。");
            widgets::hint(ui, "双击某一行可以直接打开对应的录制文件。");
            return;
        }

        let entries = self.history.entries().to_vec();
        let selected_id = self.selected_entry;

        egui::ScrollArea::vertical()
            .max_height(ui.available_height() - 150.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("history_table")
                    .num_columns(6)
                    .striped(true)
                    .spacing([14.0, 6.0])
                    .show(ui, |ui| {
                        for header in ["时间", "来源", "格式", "时长", "大小", "状态"] {
                            ui.label(RichText::new(header).strong().small());
                        }
                        ui.end_row();

                        for entry in &entries {
                            let selected = selected_id == Some(entry.id);
                            let response = ui
                                .selectable_label(selected, entry.created_text())
                                .on_hover_text(entry.path.display().to_string());
                            if response.clicked() {
                                select = Some(entry.id);
                            }
                            if response.double_clicked() {
                                select = Some(entry.id);
                                action = Some(HistoryAction::Open(entry.id));
                            }

                            ui.label(&entry.source_label);
                            ui.label(entry.container.to_uppercase());
                            ui.label(crate::units::format_duration(entry.duration()));
                            ui.label(crate::units::format_size(entry.bytes));

                            if entry.exists {
                                ui.label(RichText::new("正常").color(colors::OK).small());
                            } else {
                                ui.label(RichText::new("文件丢失").color(colors::ERROR).small());
                            }
                            ui.end_row();
                        }
                    });
            });

        let selected = selected_id
            .or_else(|| entries.first().map(|entry| entry.id))
            .and_then(|id| entries.iter().find(|entry| entry.id == id).cloned());

        if let Some(entry) = selected {
            ui.add_space(6.0);
            widgets::section(ui, "详情", |ui| {
                widgets::key_value(ui, "文件", entry.file_name());
                widgets::key_value(ui, "完整路径", entry.path.display().to_string());
                widgets::key_value(ui, "分辨率", entry.resolution_text());
                if let Some(region) = entry.region_text() {
                    widgets::key_value(ui, "录制区域", region);
                }
                widgets::key_value(ui, "声音", entry.audio.clone());
                widgets::key_value(ui, "编码器", entry.backend.clone());
                widgets::key_value(ui, "大小", crate::units::format_size(entry.bytes));
            });

            ui.horizontal(|ui| {
                if ui
                    .add_enabled(entry.exists, egui::Button::new("打开文件"))
                    .clicked()
                {
                    action = Some(HistoryAction::Open(entry.id));
                }
                if ui
                    .add_enabled(entry.exists, egui::Button::new("在文件夹中显示"))
                    .clicked()
                {
                    action = Some(HistoryAction::Reveal(entry.id));
                }
                if ui.button("复制路径").clicked() {
                    action = Some(HistoryAction::CopyPath(entry.id));
                }
                if ui
                    .button("从记录中删除")
                    .on_hover_text("只删除记录，不会删除录制文件")
                    .clicked()
                {
                    action = Some(HistoryAction::Remove(entry.id));
                }
            });
        }

        // ---- apply the collected intents ----
        if refresh {
            self.history.refresh_existence();
            self.info("已重新检查录制文件");
        }
        if open_output_dir {
            if let Err(err) = std::fs::create_dir_all(&output_dir) {
                self.error(format!("无法创建录制文件夹：{err}"));
            } else if let Err(err) = opener::open(&output_dir) {
                self.error(format!("无法打开文件夹：{err}"));
            }
        }
        if clear {
            match self.history.clear() {
                Ok(()) => {
                    self.selected_entry = None;
                    self.confirm_clear_history = false;
                    self.info("已清空录制记录（录制文件未被删除）");
                }
                Err(err) => self.error(format!("清空记录失败：{err}")),
            }
        }
        if let Some(id) = select {
            self.selected_entry = Some(id);
        }
        match action {
            Some(HistoryAction::Open(id)) => self.open_history_entry(id),
            Some(HistoryAction::Reveal(id)) => self.reveal_history_entry(id),
            Some(HistoryAction::CopyPath(id)) => {
                if let Some(entry) = self.history.find(id) {
                    let path = entry.path.display().to_string();
                    ui.ctx().copy_text(path.clone());
                    self.info(format!("已复制路径：{path}"));
                }
            }
            Some(HistoryAction::Remove(id)) => {
                let name = self.history.find(id).map(|entry| entry.file_name());
                match self.history.remove(id) {
                    Ok(true) => {
                        if self.selected_entry == Some(id) {
                            self.selected_entry = None;
                        }
                        self.info(format!(
                            "已从记录中删除 {}（录制文件保留）",
                            name.unwrap_or_default()
                        ));
                    }
                    Ok(false) => self.info("该记录已不存在"),
                    Err(err) => self.error(format!("删除记录失败：{err}")),
                }
            }
            None => {}
        }
    }

    fn open_history_entry(&mut self, id: u64) {
        let Some(entry) = self.history.find(id).cloned() else {
            return;
        };
        if !entry.path.exists() {
            self.error(format!("录制文件已不存在：{}", entry.path.display()));
            return;
        }
        match opener::open(&entry.path) {
            Ok(()) => self.info(format!("已打开 {}", entry.file_name())),
            Err(err) => self.error(format!("无法打开文件：{err}")),
        }
    }

    fn reveal_history_entry(&mut self, id: u64) {
        let Some(entry) = self.history.find(id).cloned() else {
            return;
        };
        if let Err(err) = crate::paths::reveal_in_file_manager(&entry.path) {
            self.error(format!("无法在文件夹中显示：{err}"));
        }
    }

    /// Reloads the history from disk, e.g. after a recording finished.
    fn reload_history(&mut self) {
        self.history = HistoryStore::load();
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section(ui, "捕获行为", |ui| {
                    if ui
                        .checkbox(&mut self.config.capture_cursor, "录制鼠标指针")
                        .changed()
                    {
                        self.touch_config();
                    }
                    if ui
                        .checkbox(&mut self.config.capture_border, "显示捕获边框")
                        .changed()
                    {
                        self.touch_config();
                    }
                    if ui
                        .checkbox(
                            &mut self.config.include_secondary_windows,
                            "包含同一进程的其他窗口（如弹出菜单）",
                        )
                        .changed()
                    {
                        self.touch_config();
                    }
                    if ui
                        .checkbox(&mut self.config.remember_last_source, "记住上次的录制源")
                        .changed()
                    {
                        self.touch_config();
                    }

                    ui.horizontal(|ui| {
                        ui.label("开始前倒计时");
                        let mut seconds = self.config.countdown_seconds;
                        if ui
                            .add(egui::Slider::new(&mut seconds, 0..=30).suffix(" 秒"))
                            .changed()
                        {
                            self.config.countdown_seconds = seconds;
                            self.touch_config();
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.label("最小刷新间隔");
                        let mut interval = self.config.min_update_interval_ms;
                        if ui
                            .add(
                                egui::DragValue::new(&mut interval)
                                    .range(0..=1000)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            self.config.min_update_interval_ms = interval;
                            self.touch_config();
                        }
                        hint(ui, "0 = 按帧率自动计算");
                    });

                    if ui
                        .checkbox(&mut self.config.show_preview, "显示实时预览")
                        .changed()
                    {
                        self.touch_config();
                    }

                    ui.horizontal(|ui| {
                        ui.label("采集方式");
                        egui::ComboBox::from_id_salt("capture_method")
                            .selected_text(self.config.capture_method.label())
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                for method in CaptureMethod::ALL {
                                    if ui
                                        .selectable_value(
                                            &mut self.config.capture_method,
                                            method,
                                            method.label(),
                                        )
                                        .clicked()
                                    {
                                        self.touch_config();
                                    }
                                }
                            });
                    });
                    hint(ui, self.config.capture_method.description());

                    if ui
                        .checkbox(
                            &mut self.config.test_pattern,
                            "使用测试图案代替屏幕画面（用于验证编码设置）",
                        )
                        .changed()
                    {
                        self.touch_config();
                    }
                    if ui
                        .checkbox(
                            &mut self.config.prefer_hardware_encoder,
                            "优先使用硬件编码器（FFmpeg 后端）",
                        )
                        .changed()
                    {
                        self.touch_config();
                    }
                });

                section(ui, "全局快捷键", |ui| {
                    if ui
                        .checkbox(&mut self.config.hotkeys_enabled, "启用全局快捷键")
                        .changed()
                    {
                        self.touch_config();
                        self.restart_hotkeys();
                    }

                    let mut changed = false;
                    let hotkeys_enabled = self.config.hotkeys_enabled;
                    ui.add_enabled_ui(hotkeys_enabled, |ui| {
                        egui::Grid::new("hotkey_grid")
                            .num_columns(2)
                            .spacing([16.0, 6.0])
                            .show(ui, |ui| {
                                let rows: [(&str, &mut String); 3] = [
                                    ("开始 / 停止", &mut self.config.hotkey_start_stop),
                                    ("暂停 / 继续", &mut self.config.hotkey_pause),
                                    ("截图", &mut self.config.hotkey_screenshot),
                                ];
                                for (label, value) in rows {
                                    ui.label(label);
                                    let mut text = value.clone();
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut text)
                                                .desired_width(140.0)
                                                .hint_text("例如 F9 / Ctrl+Shift+R"),
                                        )
                                        .changed()
                                    {
                                        *value = text;
                                        changed = true;
                                    }
                                    ui.end_row();
                                }
                            });
                    });

                    if changed {
                        self.touch_config();
                    }

                    ui.horizontal(|ui| {
                        if ui.button("重新注册快捷键").clicked() {
                            self.restart_hotkeys();
                        }
                        let status = self.hotkey_status.clone();
                        hint(ui, status);
                    });
                    hint(ui, "窗口未获得焦点时快捷键同样有效。");
                });

                section(ui, "外部工具与外观", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("FFmpeg 路径");
                        let mut path = self.config.ffmpeg_path.display().to_string();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut path)
                                    .desired_width(ui.available_width() - 90.0)
                                    .hint_text("留空表示自动查找"),
                            )
                            .changed()
                        {
                            self.config.ffmpeg_path = PathBuf::from(path);
                            self.touch_config();
                        }
                        if ui.button("检测").clicked() {
                            self.redetect_ffmpeg();
                        }
                    });
                    match &self.ffmpeg {
                        Some(info) => hint(ui, info.summary()),
                        None => hint(ui, "未找到 FFmpeg：MKV / WebM / MOV 与 AV1 / VP9 / ProRes 不可用"),
                    }

                    ui.horizontal(|ui| {
                        ui.label("界面主题");
                        for mode in ThemeMode::ALL {
                            if ui
                                .selectable_label(self.config.theme == mode, mode.label())
                                .clicked()
                            {
                                self.config.theme = mode;
                                self.touch_config();
                            }
                        }
                    });
                });

                section(ui, "编码器检测", |ui| {
                    match &self.capabilities {
                        Some(map) => {
                            key_value(ui, "本机可用", map.summary());
                            let unsupported: Vec<String> = map
                                .entries()
                                .into_iter()
                                .filter_map(|(container, codec, result)| match result {
                                    Ok(_) => None,
                                    Err(reason) => Some(format!(
                                        "{} / {}：{reason}",
                                        container.id(),
                                        codec.id()
                                    )),
                                })
                                .collect();
                            if unsupported.is_empty() {
                                hint(ui, "全部内置编码组合均可用");
                            } else {
                                hint(ui, "以下组合不可用（已在下拉框中禁用）：");
                                for line in unsupported {
                                    ui.label(RichText::new(format!("• {line}")).small().color(colors::WARN));
                                }
                            }
                        }
                        None => hint(ui, "正在检测本机可用的编码器…"),
                    }
                    if ui.button("重新检测编码器").clicked() {
                        self.start_capability_probe();
                    }
                });

                section(ui, "配置", |ui| {
                    ui.horizontal(|ui| {
                        key_value(ui, "配置文件", crate::config::config_path().display().to_string());
                    });
                    ui.horizontal(|ui| {
                        key_value(
                            ui,
                            "日志文件",
                            crate::logging::current_log_path().display().to_string(),
                        );
                    });
                    ui.horizontal(|ui| {
                        if ui.button("保存配置").clicked() {
                            match self.config.save() {
                                Ok(()) => self.success("配置已保存"),
                                Err(err) => self.error(format!("保存配置失败：{err}")),
                            }
                        }
                        if ui.button("打开配置文件夹").clicked() {
                            let _ = opener::open(crate::config::app_data_dir());
                        }
                        if ui.button("恢复默认设置").clicked() {
                            self.config = AppConfig::default();
                            self.touch_config();
                            super::theme::apply_style(ui.ctx(), self.config.theme);
                            self.restart_hotkeys();
                            self.info("已恢复默认设置");
                        }
                    });
                });
            });
    }

    /// Self-check panel: shows the state of everything the recorder depends on,
    /// with the exact error when something is unavailable.
    ///
    /// This exists because "nothing works" reports are otherwise impossible to
    /// diagnose: the app itself needs no administrator rights, so the cause is
    /// always one of the paths, the encoders, or the desktop session.
    fn diagnostics_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "自检", |ui| {
            #[cfg(windows)]
            {
                let elevated = unsafe { windows::Win32::UI::Shell::IsUserAnAdmin() }.as_bool();
                key_value(
                    ui,
                    "权限",
                    if elevated {
                        "管理员（不需要，但也能用）"
                    } else {
                        "标准用户（推荐，功能完整）"
                    },
                );
            }

            key_value(
                ui,
                "系统托盘",
                match self.tray.is_some() {
                    true => "已创建",
                    false => "不可用（任务栏/资源管理器未运行？其余功能不受影响）",
                },
            );

            let data_dir = crate::config::app_data_dir();
            key_value(
                ui,
                "配置目录",
                match crate::paths::is_writable(&data_dir) {
                    Ok(()) => format!("{}（可写）", data_dir.display()),
                    Err(reason) => format!("{}（不可写：{reason}）", data_dir.display()),
                },
            );

            let output_dir = self.config.output_dir.clone();
            key_value(
                ui,
                "录制文件夹",
                match crate::paths::is_writable(&output_dir) {
                    Ok(()) => format!("{}（可写）", output_dir.display()),
                    Err(reason) => {
                        format!("{}（不可写：{reason}，开始时自动换一个）", output_dir.display())
                    }
                },
            );

            key_value(
                ui,
                "采集源",
                format!("{} 个显示器，{} 个窗口", self.monitors.len(), self.windows.len()),
            );

            key_value(
                ui,
                "FFmpeg",
                match &self.ffmpeg {
                    Some(info) => format!("{}（MKV/WebM/MOV 可用）", info.path.display()),
                    None => "未找到（仅内置格式可用）".to_string(),
                },
            );

            match &self.capabilities {
                Some(map) => {
                    key_value(
                        ui,
                        "编码器",
                        format!("{}/{} 组合可用", map.supported_count(), map.total()),
                    );
                    // Only the failures are interesting; the supported ones are
                    // listed in the table below.
                    let mut failures: Vec<String> = map
                        .entries()
                        .into_iter()
                        .filter_map(|(container, codec, result)| match result {
                            Err(reason) => Some(format!(
                                "{} {}：{reason}",
                                container.label(),
                                codec.label()
                            )),
                            Ok(_) => None,
                        })
                        .collect();
                    if failures.is_empty() {
                        hint(ui, "所有组合均可用");
                    } else {
                        failures.truncate(4);
                        for failure in failures {
                            hint(ui, failure);
                        }
                    }
                }
                None => {
                    key_value(ui, "编码器", "检测中…");
                }
            }

            hint(
                ui,
                "提示：本程序不需要管理员权限。若 Windows 会话是远程桌面（RDP）或没有可用的桌面输出，\
                 采集会失败；此时上面的采集源为 0。",
            );
        });
    }

    fn about_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section(ui, "关于 ScreenRec", |ui| {
                    key_value(ui, "版本", crate::VERSION);
                    ui.label(
                        "使用 Windows Graphics Capture 录制整块屏幕或任意单一窗口，支持多种导出格式与自定义保存位置。",
                    );
                    hint(
                        ui,
                        "内置编码器基于 Windows Media Foundation；安装 FFmpeg 后可导出 MKV / WebM / MOV 等更多格式。",
                    );
                });

                self.diagnostics_section(ui);

                section(ui, "支持的导出格式", |ui| {
                    egui::Grid::new("formats_grid")
                        .num_columns(3)
                        .striped(true)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(RichText::new("容器").strong());
                            ui.label(RichText::new("编码器").strong());
                            ui.label(RichText::new("后端").strong());
                            ui.end_row();

                            for container in Container::ALL {
                                for (position, codec) in
                                    formats::codecs_for(container).iter().enumerate()
                                {
                                    let resolved =
                                        formats::resolve(container, *codec, self.ffmpeg.is_some());
                                    let (backend, colour) = match &resolved {
                                        Ok(format) => {
                                            let colour = if format.requires_ffmpeg() {
                                                colors::WARN
                                            } else {
                                                colors::OK
                                            };
                                            (format.backend.label().to_string(), colour)
                                        }
                                        Err(err) => (err.to_string(), colors::MUTED),
                                    };
                                    ui.label(if position == 0 {
                                        container.label()
                                    } else {
                                        ""
                                    });
                                    ui.label(codec.label());
                                    ui.label(RichText::new(backend.as_str()).color(colour).small());
                                    ui.end_row();
                                }
                            }
                        });
                });
            });
    }
}

/// Resolution of a selection, when it can be determined.
fn source_resolution(
    source: &SourceSelection,
    monitors: &[MonitorInfo],
    windows: &[WindowInfo],
) -> Option<(u32, u32)> {
    match source {
        SourceSelection::Monitor { index, .. } => monitors
            .iter()
            .find(|monitor| monitor.index == *index)
            .map(|monitor| (monitor.width, monitor.height)),
        SourceSelection::Window { hwnd, .. } => windows
            .iter()
            .find(|window| window.hwnd == *hwnd)
            .map(|window| (window.width.max(0) as u32, window.height.max(0) as u32)),
        SourceSelection::Picker => None,
    }
}

fn recorder_path_hint(snapshot: &SessionSnapshot) -> String {
    snapshot
        .output
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "（未知路径）".to_string())
}

impl eframe::App for ScreenRecApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.poll_tray(&ctx);
        self.show_float_panel(&ctx);

        egui::Panel::top("header")
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 8)))
            .show(ui, |ui| self.header(ui));

        egui::Panel::bottom("status")
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 6)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    match &self.message {
                        Some(message) => {
                            let colour = match message.kind {
                                MessageKind::Info => colors::MUTED,
                                MessageKind::Success => colors::OK,
                                MessageKind::Error => colors::ERROR,
                            };
                            ui.label(RichText::new(&message.text).color(colour));
                        }
                        None => {
                            hint(ui, format!("日志：{}", crate::logging::current_log_path().display()));
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        hint(ui, format!("日志：{}", crate::logging::current_log_path().display()));
                    });
                });
            });

        egui::Panel::left("sources")
            .resizable(true)
            .default_size(330.0)
            .size_range(260.0..=460.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.source_panel(ui));
            });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Record => self.record_tab(ui),
            Tab::History => self.history_tab(ui),
            Tab::Settings => self.settings_tab(ui),
            Tab::About => self.about_tab(ui),
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(recorder) = self.recorder.take() {
            recorder.request_stop();
            let _ = recorder.wait_for(Duration::from_secs(15));
        }
        if let Err(err) = self.config.save() {
            log::warn!("退出时保存配置失败: {err}");
        }
        if let Some(mut hotkeys) = self.hotkeys.take() {
            hotkeys.stop();
        }
        log::info!("界面已退出");
    }
}
