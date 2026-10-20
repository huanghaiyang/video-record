//! Command line interface: headless recording, diagnostics and probing.
//!
//! The GUI is the default entry point; every subcommand here is also what the
//! automated tests exercise, so the two share exactly the same pipeline.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};

use crate::capture::{self, CaptureMethod, Recorder, RecordingRequest, SessionStatus, SourceSelection};
use crate::config::{AppConfig, OutputDirOutcome};
use crate::encode;
use crate::ffmpeg;
use crate::formats::{self, Container, QualityPreset, ScaleOption, VideoCodec};
use crate::history::HistoryStore;
use crate::logging;
use crate::region::Region;

#[derive(Parser, Debug)]
#[command(
    name = "screenrec",
    version,
    about = "Windows 屏幕录制工具：全屏 / 指定窗口录制，多格式导出",
    long_about = None,
    arg_required_else_help = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
// The argument structs are built once at startup; boxing them would only make
// the call sites noisier.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// 录制显示器或窗口
    Record(RecordArgs),
    /// 查看录制记录
    History(HistoryArgs),
    /// 列出可录制的显示器与窗口
    Sources(SourcesArgs),
    /// 列出支持的导出格式与对应后端
    Formats,
    /// 检测本机可用的编码器（逐个写出小文件验证）
    Probe(ProbeArgs),
    /// 抓取单帧画面保存为图片
    Frame(FrameArgs),
    /// 显示环境、路径与配置信息
    Doctor,
}

#[derive(Args, Debug)]
pub struct RecordArgs {
    /// 显示器序号（见 `screenrec sources`）
    #[arg(long, conflicts_with_all = ["window", "picker"])]
    pub monitor: Option<usize>,

    /// 窗口：十六进制/十进制句柄，或标题关键字
    #[arg(long, conflicts_with_all = ["monitor", "picker"])]
    pub window: Option<String>,

    /// 使用系统选择器选择要录制的画面
    #[arg(long, conflicts_with_all = ["monitor", "window"])]
    pub picker: bool,

    /// 录制时长（秒）；0 表示一直录制到 Ctrl+C
    #[arg(long, default_value_t = 10)]
    pub duration: u32,

    /// 容器：mp4 / mkv / mov / webm / avi / wmv / mpg / gif / png_seq / jpeg_seq
    #[arg(long)]
    pub format: Option<String>,

    /// 视频编码器：h264 / hevc / av1 / vp9 / vp8 / mpeg4 / mjpeg / mpeg2 / wmv3 / prores
    #[arg(long)]
    pub codec: Option<String>,

    /// 帧率
    #[arg(long)]
    pub fps: Option<u32>,

    /// 画质：low / medium / high / ultra / custom
    #[arg(long)]
    pub quality: Option<String>,

    /// 自定义码率（kbps），配合 --quality custom
    #[arg(long)]
    pub bitrate_kbps: Option<u32>,

    /// 输出目录（默认使用配置中的目录）
    #[arg(long)]
    pub output_dir: Option<PathBuf>,

    /// 文件名模板，支持 {date} {time} {datetime} {source}
    #[arg(long)]
    pub filename: Option<String>,

    /// 缩放：original / 1080p / 720p / 480p / half
    #[arg(long)]
    pub scale: Option<String>,

    /// 输出宽度上限（像素）
    #[arg(long)]
    pub max_width: Option<u32>,

    /// 水印文字（支持中文）；与 --watermark-pattern 可同时使用
    #[arg(long)]
    pub watermark_text: Option<String>,

    /// 水印基础图案：none / dots / grid / logo
    #[arg(long)]
    pub watermark_pattern: Option<String>,

    /// 水印运动：none / scroll-left / scroll-right / scroll-up / scroll-down / diagonal / bounce
    #[arg(long)]
    pub watermark_motion: Option<String>,

    /// 水印重复：off / tile / ticker
    #[arg(long)]
    pub watermark_repeat: Option<String>,

    /// 水印不透明度 0..1
    #[arg(long)]
    pub watermark_opacity: Option<f32>,

    /// 水印字号（像素）
    #[arg(long)]
    pub watermark_size: Option<u32>,

    /// 水印颜色，RRGGBB
    #[arg(long)]
    pub watermark_color: Option<String>,

    /// 水印静态位置：top-left / top-right / bottom-left / bottom-right / center
    #[arg(long)]
    pub watermark_anchor: Option<String>,

    /// 水印移动速度（像素/秒）
    #[arg(long)]
    pub watermark_speed: Option<f32>,

    /// 不录制鼠标指针
    #[arg(long)]
    pub no_cursor: bool,

    /// 录制目标窗口时显示捕获边框
    #[arg(long)]
    pub border: bool,

    /// 录制系统声音（默认取配置值）
    #[arg(long)]
    pub system_audio: bool,

    /// 不录制任何声音
    #[arg(long)]
    pub no_audio: bool,

    /// 录制麦克风
    #[arg(long)]
    pub mic: bool,

    /// 指定 ffmpeg 可执行文件
    #[arg(long)]
    pub ffmpeg: Option<PathBuf>,

    /// 录制前的倒计时秒数
    #[arg(long, default_value_t = 0)]
    pub countdown: u32,

    /// 采集方式：auto / wgc / dxgi
    #[arg(long)]
    pub capture: Option<String>,

    /// 只录制所选源的一块区域，如 100,200,800,600 或 800x600+100+200
    /// （坐标相对于所录显示器/窗口的左上角，单位为物理像素）
    #[arg(long)]
    pub region: Option<String>,

    /// 不采集屏幕，改用内置测试图案验证编码与封装
    #[arg(long)]
    pub test_pattern: bool,

    /// 以 JSON 输出结果
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct SourcesArgs {
    /// 以 JSON 输出
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    /// 只显示最近 N 条（0 表示全部）
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// 以 JSON 输出，便于脚本处理
    #[arg(long)]
    pub json: bool,

    /// 清空全部记录（只删除记录，不删除录制文件）
    #[arg(long)]
    pub clear: bool,

    /// 打开指定 id 的录制文件
    #[arg(long)]
    pub open: Option<u64>,

    /// 删除指定 id 的记录（不删除录制文件）
    #[arg(long)]
    pub remove: Option<u64>,

    /// 重新检查录制文件是否仍然存在
    #[arg(long)]
    pub verify: bool,
}

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// 把探测文件写到该目录（默认写到临时目录，探测完删除）
    #[arg(long)]
    pub output_dir: Option<PathBuf>,

    /// 有不可用组合时以非零退出码结束（用于 CI）
    #[arg(long)]
    pub strict: bool,
}

#[derive(Args, Debug)]
pub struct FrameArgs {
    /// 显示器序号
    #[arg(long, default_value_t = 0)]
    pub monitor: usize,

    /// 输出文件（.png / .jpg / .bmp）
    #[arg(long)]
    pub out: PathBuf,
}

use crate::WindowInfo;

/// Runs a subcommand.
pub fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Record(args) => record(args),
        Command::History(args) => history(args),
        Command::Sources(args) => list_sources(args),
        Command::Formats => list_formats(),
        Command::Probe(args) => probe(args),
        Command::Frame(args) => capture_frame(args),
        Command::Doctor => doctor(),
    }
}

/// Runs the command line interface and returns the process exit code.
///
/// Shared by both binaries:
///
/// * `screenrec.exe` is linked as a Windows GUI binary, so launching the
///   application never opens a console window. A shell does not wait for such a
///   process, which makes this path fine for ad-hoc use but not for scripts.
/// * `screenrec-cli.exe` is a console binary: the shell blocks until it exits and
///   its output is captured normally. That is the one to call from scripts.
pub fn main_entry() -> i32 {
    use clap::CommandFactory as _;
    use std::io::Write as _;

    let cli = Cli::parse();
    let Some(command) = cli.command else {
        // The console build has no GUI to fall back to, so explain what to do.
        let mut help = Cli::command();
        let _ = help.print_help();
        println!();
        let _ = writeln!(
            std::io::stderr(),
            "提示：图形界面请运行 screenrec.exe（不带参数）"
        );
        return 2;
    };

    match run(command) {
        Ok(()) => 0,
        Err(err) => {
            // Not `eprintln!`: a GUI-subsystem process may have no usable
            // stderr, and a failed write would panic over the real message.
            let _ = writeln!(std::io::stderr(), "错误：{err:#}");
            1
        }
    }
}

// ---------------------------------------------------------------- record ----

fn record(args: RecordArgs) -> anyhow::Result<()> {
    let mut config = AppConfig::load();
    apply_overrides(&mut config, &args)?;

    // An explicitly requested folder must be reported if unusable; a default
    // or remembered one may be relocated so the recording still succeeds.
    match config.ensure_output_dir(args.output_dir.is_none()) {
        OutputDirOutcome::Ok => {}
        outcome @ OutputDirOutcome::Relocated { .. } => {
            if let Some(message) = outcome.message() {
                println!("注意：{message}");
            }
        }
        outcome @ OutputDirOutcome::Failed { .. } => {
            anyhow::bail!(outcome.message().unwrap_or_else(|| "录制文件夹不可写".to_string()));
        }
    }

    // Logging is best-effort: recording must still work without a writable
    // application-data folder.
    let _ = logging::init(log::LevelFilter::Info, true);

    let ffmpeg_info = ffmpeg::detect(config.ffmpeg_path.as_path());
    let source = resolve_source(&args)?;

    println!("捕获目标：{}", capture::session::describe_source(&source));
    if let Some(region) = config.region {
        println!("录制区域：{}（相对于该目标左上角）", region.label());
    }
    println!(
        "导出格式：{}",
        config
            .export_format(ffmpeg_info.is_some())
            .map(|format| format.describe())
            .unwrap_or_else(|err| err.to_string())
    );

    for remaining in (1..=args.countdown).rev() {
        println!("{remaining} 秒后开始录制…");
        std::thread::sleep(Duration::from_secs(1));
    }

    let recorder = Recorder::start(RecordingRequest {
        source,
        config,
        ffmpeg: ffmpeg_info.as_ref().map(|info| info.path.clone()),
    })?;

    let output = recorder.output().path.display().to_string();
    let shared = recorder.shared();
    println!("正在录制到：{output}");

    if args.duration == 0 {
        let stop_flag = shared.clone();
        let _ = ctrlc::set_handler(move || {
            println!("\n收到停止信号，正在保存…");
            stop_flag.request_stop();
        });
    } else {
        // The encoder is created on the first frame, which can take a moment;
        // start counting only once the file is really being written so the
        // requested duration is the duration of the output.
        let ready_deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < ready_deadline {
            match shared.status() {
                SessionStatus::Recording | SessionStatus::Paused => break,
                status if !status.is_active() => break,
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }

        let deadline = Instant::now() + Duration::from_secs(args.duration as u64);
        let mut last_report = Instant::now();
        while Instant::now() < deadline && shared.is_active() {
            std::thread::sleep(Duration::from_millis(100));
            if last_report.elapsed() >= Duration::from_secs(1) {
                last_report = Instant::now();
                let snapshot = shared.snapshot();
                println!(
                    "  {} · {} 帧 · {:.1} 秒",
                    snapshot.status.label(),
                    snapshot.frames,
                    snapshot.elapsed.as_secs_f32()
                );
            }
        }
    }

    // Ask the capture thread to stop, then give it a bounded amount of time to
    // flush the encoder (a stalled source must not hang the CLI).
    shared.request_stop();
    let status = recorder.wait_for(Duration::from_secs(30));
    match status {
        SessionStatus::Completed => {
            let snapshot = shared.snapshot();
            let path = snapshot
                .output
                .clone()
                .ok_or_else(|| anyhow::anyhow!("录制完成但没有输出文件"))?;
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "path": path,
                        "frames": snapshot.frames,
                        "duration_ms": snapshot.elapsed.as_millis() as u64,
                        "bytes": snapshot.file_size,
                        "backend": snapshot.backend,
                    })
                );
            } else {
                println!(
                    "录制完成：{}（{} 帧，{:.1} 秒，{}）",
                    path.display(),
                    snapshot.frames,
                    snapshot.elapsed.as_secs_f32(),
                    human_size(snapshot.file_size)
                );
            }
            Ok(())
        }
        SessionStatus::Failed(message) => {
            if args.json {
                println!("{}", serde_json::json!({ "ok": false, "error": message }));
            }
            anyhow::bail!("{message}")
        }
        other => anyhow::bail!("录制意外结束：{}", other.label()),
    }
}

/// Applies the watermark flags; any of them enables the watermark.
fn apply_watermark(
    watermark: &mut crate::watermark::WatermarkSettings,
    args: &RecordArgs,
) -> anyhow::Result<()> {
    use crate::watermark::{WatermarkAnchor, WatermarkMotion, WatermarkPattern, WatermarkRepeat};

    if let Some(text) = &args.watermark_text {
        watermark.text = text.clone();
        watermark.enabled = true;
    }
    if let Some(pattern) = &args.watermark_pattern {
        watermark.pattern = WatermarkPattern::from_id(pattern)
            .ok_or_else(|| anyhow::anyhow!("未知的水印图案：{pattern}"))?;
        watermark.enabled = true;
    }
    if let Some(motion) = &args.watermark_motion {
        watermark.motion = WatermarkMotion::from_id(motion)
            .ok_or_else(|| anyhow::anyhow!("未知的水印运动：{motion}"))?;
        watermark.enabled = true;
    }
    if let Some(repeat) = &args.watermark_repeat {
        watermark.repeat = WatermarkRepeat::from_id(repeat)
            .ok_or_else(|| anyhow::anyhow!("未知的水印重复方式：{repeat}"))?;
        watermark.enabled = true;
    }
    if let Some(opacity) = args.watermark_opacity {
        watermark.opacity = opacity.clamp(0.0, 1.0);
        watermark.enabled = true;
    }
    if let Some(size) = args.watermark_size {
        watermark.font_size = size.clamp(8, 512);
        watermark.enabled = true;
    }
    if let Some(colour) = &args.watermark_color {
        let cleaned = colour.trim_start_matches('#');
        let value = u32::from_str_radix(cleaned, 16)
            .map_err(|_| anyhow::anyhow!("水印颜色需要是 RRGGBB：{colour}"))?;
        watermark.color = [
            ((value >> 16) & 0xFF) as u8,
            ((value >> 8) & 0xFF) as u8,
            (value & 0xFF) as u8,
        ];
        watermark.enabled = true;
    }
    if let Some(anchor) = &args.watermark_anchor {
        watermark.anchor = WatermarkAnchor::from_id(anchor)
            .ok_or_else(|| anyhow::anyhow!("未知的水印位置：{anchor}"))?;
        watermark.enabled = true;
    }
    if let Some(speed) = args.watermark_speed {
        watermark.speed = speed.clamp(0.0, 2000.0);
        watermark.enabled = true;
    }
    Ok(())
}

fn apply_overrides(config: &mut AppConfig, args: &RecordArgs) -> anyhow::Result<()> {
    if let Some(format) = &args.format {
        config.container = Container::from_id(format)
            .ok_or_else(|| anyhow::anyhow!("未知的容器：{format}"))?;
        config.video_codec = formats::codecs_for(config.container)[0];
    }
    if let Some(codec) = &args.codec {
        config.video_codec = VideoCodec::from_id(codec)
            .ok_or_else(|| anyhow::anyhow!("未知的编码器：{codec}"))?;
    }
    if let Some(fps) = args.fps {
        config.frame_rate = fps;
    }
    if let Some(quality) = &args.quality {
        config.quality = QualityPreset::from_id(quality)
            .ok_or_else(|| anyhow::anyhow!("未知的画质：{quality}"))?;
    }
    if let Some(kbps) = args.bitrate_kbps {
        config.custom_bitrate_kbps = kbps;
        config.quality = QualityPreset::Custom;
    }
    if let Some(dir) = &args.output_dir {
        config.set_output_dir(dir.clone());
    }
    if let Some(template) = &args.filename {
        config.filename_template = template.clone();
    }
    if let Some(scale) = &args.scale {
        config.scale =
            ScaleOption::from_id(scale).ok_or_else(|| anyhow::anyhow!("未知的缩放：{scale}"))?;
    }
    if let Some(width) = args.max_width {
        config.max_output_width = width;
    }
    apply_watermark(&mut config.watermark, args)?;
    if args.no_cursor {
        config.capture_cursor = false;
    }
    if args.border {
        config.capture_border = true;
    }
    if args.no_audio {
        config.audio.enabled = false;
    } else if args.system_audio || args.mic {
        config.audio.enabled = true;
        if args.system_audio {
            config.audio.system = true;
        }
        if args.mic {
            config.audio.microphone = true;
        }
    }
    if let Some(ffmpeg) = &args.ffmpeg {
        config.ffmpeg_path = ffmpeg.clone();
    }
    if let Some(method) = &args.capture {
        config.capture_method = CaptureMethod::from_id(method)
            .ok_or_else(|| anyhow::anyhow!("未知的采集方式：{method}（可用 auto / wgc / dxgi）"))?;
    }
    if let Some(spec) = &args.region {
        config.region = Some(Region::parse(spec).map_err(|err| anyhow::anyhow!(err))?);
    }
    if args.test_pattern {
        config.test_pattern = true;
    }
    config.normalize();
    Ok(())
}

fn resolve_source(args: &RecordArgs) -> anyhow::Result<SourceSelection> {
    if args.picker {
        return Ok(SourceSelection::Picker);
    }

    if let Some(query) = &args.window {
        let windows = capture::sources::list_windows();
        return resolve_window_query(&windows, query);
    }

    let monitors = capture::sources::list_monitors();
    anyhow::ensure!(!monitors.is_empty(), "没有检测到任何显示器");

    let index = match args.monitor {
        Some(index) => index,
        None => monitors
            .iter()
            .find(|monitor| monitor.is_primary)
            .map(|monitor| monitor.index)
            .unwrap_or(monitors[0].index),
    };

    let monitor = monitors
        .iter()
        .find(|monitor| monitor.index == index)
        .ok_or_else(|| anyhow::anyhow!("找不到序号为 {index} 的显示器"))?;

    Ok(SourceSelection::monitor(index, monitor.name.clone()))
}

fn resolve_window_query(windows: &[WindowInfo], query: &str) -> anyhow::Result<SourceSelection> {
    let trimmed = query.trim();

    // A raw handle is the unambiguous form.
    let as_handle = trimmed
        .strip_prefix("0x")
        .map(|hex| i64::from_str_radix(hex, 16).ok())
        .unwrap_or_else(|| trimmed.parse::<i64>().ok());
    if let Some(handle) = as_handle {
        if let Some(window) = windows.iter().find(|window| window.hwnd == handle) {
            return Ok(SourceSelection::window(
                window.hwnd,
                window.title.clone(),
                window.process.clone(),
            ));
        }
        anyhow::bail!("找不到句柄为 {trimmed} 的窗口");
    }

    let matches: Vec<&WindowInfo> = windows
        .iter()
        .filter(|window| window.matches(trimmed))
        .collect();

    match matches.len() {
        0 => anyhow::bail!("找不到标题或进程包含“{trimmed}”的窗口"),
        1 => Ok(SourceSelection::window(
            matches[0].hwnd,
            matches[0].title.clone(),
            matches[0].process.clone(),
        )),
        _ => {
            let list = matches
                .iter()
                .take(10)
                .map(|window| format!("  {} · {}", window.hwnd, window.title))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!("“{trimmed}”匹配到多个窗口，请使用句柄指定：\n{list}")
        }
    }
}

// --------------------------------------------------------------- history ----

fn history(args: HistoryArgs) -> anyhow::Result<()> {
    let mut store = HistoryStore::load();

    if args.clear {
        let count = store.len();
        store.clear()?;
        println!("已清空 {count} 条录制记录（录制文件未被删除）");
        return Ok(());
    }

    if args.verify {
        store.refresh_existence();
    }

    if let Some(id) = args.remove {
        let entry = store.find(id).cloned();
        return match entry {
            Some(entry) => {
                store.remove(id)?;
                println!("已删除记录 #{}（{}），录制文件保留在 {}", id, entry.file_name(), entry.path.display());
                Ok(())
            }
            None => anyhow::bail!("找不到 id 为 {id} 的录制记录"),
        };
    }

    if let Some(id) = args.open {
        let entry = store
            .find(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("找不到 id 为 {id} 的录制记录"))?;
        anyhow::ensure!(entry.path.exists(), "录制文件已不存在：{}", entry.path.display());
        opener::open(&entry.path)
            .map_err(|err| anyhow::anyhow!("无法打开 {}：{err}", entry.path.display()))?;
        println!("已打开 {}", entry.path.display());
        return Ok(());
    }

    if args.json {
        let payload: Vec<serde_json::Value> = store
            .entries()
            .iter()
            .take(if args.limit == 0 { usize::MAX } else { args.limit })
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "created": entry.created_text(),
                    "created_unix_ms": entry.created_unix_ms,
                    "source_kind": entry.source_kind,
                    "source_label": entry.source_label,
                    "container": entry.container,
                    "codec": entry.codec,
                    "backend": entry.backend,
                    "path": entry.path,
                    "bytes": entry.bytes,
                    "duration_ms": entry.duration_ms,
                    "frames": entry.frames,
                    "frame_rate": entry.frame_rate,
                    "source_size": entry.source_size,
                    "output_size": entry.output_size,
                    "audio": entry.audio,
                    "region": entry.region,
                    "exists": entry.exists,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "path": store.path(),
                "total": store.len(),
                "total_bytes": store.total_bytes(),
                "entries": payload,
            })
        );
        return Ok(());
    }

    if store.is_empty() {
        println!("暂无录制记录（记录保存在 {}）", store.path().display());
        return Ok(());
    }

    println!(
        "记录文件：{}（共 {} 条，合计 {}）",
        store.path().display(),
        store.len(),
        human_size(store.total_bytes())
    );
    println!();
    println!(
        "{} {} {} {} {} 来源 / 文件",
        crate::units::pad_display("时间", 20),
        crate::units::pad_display("格式", 10),
        crate::units::pad_display("时长", 8),
        crate::units::pad_display("大小", 10),
        crate::units::pad_display("状态", 10),
    );

    let limit = if args.limit == 0 { usize::MAX } else { args.limit };
    let indent = " ".repeat(60);
    for entry in store.entries().iter().take(limit) {
        let status = if entry.exists { "正常" } else { "文件丢失" };
        let mut title = format!("{} · {}", entry.source_label, entry.file_name());
        if let Some(region) = entry.region_text() {
            title.push_str(&format!(" · {region}"));
        }
        println!(
            "{} {} {} {} {} {title}",
            crate::units::pad_display(&entry.created_text(), 20),
            crate::units::pad_display(&entry.container.to_uppercase(), 10),
            crate::units::pad_display(&crate::units::format_duration(entry.duration()), 8),
            crate::units::pad_display(&human_size(entry.bytes), 10),
            crate::units::pad_display(status, 10),
        );
        println!("{indent}{}", entry.path.display());
    }

    if store.len() > limit {
        println!();
        println!("（其余 {} 条请用 --limit 0 查看）", store.len() - limit);
    }

    let missing = store.len() - store.existing_count();
    if missing > 0 {
        println!();
        println!("提示：有 {missing} 条记录对应的文件已不在原位置，可用 --verify 刷新状态。");
    }
    Ok(())
}

// --------------------------------------------------------------- sources ----

fn list_sources(args: SourcesArgs) -> anyhow::Result<()> {
    let monitors = capture::sources::list_monitors();
    let windows = capture::sources::list_windows();

    if args.json {
        let payload = serde_json::json!({
            "monitors": monitors.iter().map(|m| serde_json::json!({
                "index": m.index,
                "name": m.name,
                "device_name": m.device_name,
                "width": m.width,
                "height": m.height,
                "refresh_rate": m.refresh_rate,
                "primary": m.is_primary,
            })).collect::<Vec<_>>(),
            "windows": windows.iter().map(|w| serde_json::json!({
                "hwnd": w.hwnd,
                "title": w.title,
                "process": w.process,
                "pid": w.pid,
                "width": w.width,
                "height": w.height,
            })).collect::<Vec<_>>(),
        });
        println!("{payload}");
        return Ok(());
    }

    println!("显示器（{} 个）", monitors.len());
    for monitor in &monitors {
        println!("  [{}] {}", monitor.index, monitor.describe());
    }

    println!();
    println!("窗口（{} 个，可用 --window 句柄或标题关键字选择）", windows.len());
    for window in windows.iter().take(60) {
        println!(
            "  {} {} {}x{}  {}",
            crate::units::pad_display(&format!("0x{:X}", window.hwnd), 12),
            crate::units::pad_display(&truncate(&window.title, 48), 48),
            window.width,
            window.height,
            window.process
        );
    }
    if windows.len() > 60 {
        println!("  … 其余 {} 个窗口已省略", windows.len() - 60);
    }
    Ok(())
}

// --------------------------------------------------------------- formats ----

fn list_formats() -> anyhow::Result<()> {
    let config = AppConfig::load();
    let ffmpeg_info = ffmpeg::detect(config.ffmpeg_path.as_path());

    println!(
        "FFmpeg：{}\n",
        ffmpeg_info
            .as_ref()
            .map(|info| info.summary())
            .unwrap_or_else(|| "未找到（仅内置编码器可用）".to_string())
    );

    println!(
        "{} {} {} 需要 FFmpeg",
        crate::units::pad_display("容器", 28),
        crate::units::pad_display("可用编码器", 34),
        crate::units::pad_display("后端", 26)
    );
    for container in Container::ALL {
        for (position, codec) in formats::codecs_for(container).iter().enumerate() {
            let resolved = formats::resolve(container, *codec, true);
            let (backend, needs) = match resolved {
                Ok(format) => (
                    format.backend.label().to_string(),
                    if format.requires_ffmpeg() { "是" } else { "否" }.to_string(),
                ),
                Err(err) => (err.to_string(), "-".to_string()),
            };
            let container_label = if position == 0 {
                container.label().to_string()
            } else {
                String::new()
            };
            println!(
                "{} {} {} {}",
                crate::units::pad_display(&container_label, 28),
                crate::units::pad_display(codec.label(), 34),
                crate::units::pad_display(&backend, 26),
                needs
            );
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- probe ----

/// Probes every built-in encoder by writing a tiny real file.
///
/// This is a diagnostic: it reports what works and exits 0. Pass `--strict` to
/// make missing combinations an error (useful in CI).
fn probe(args: ProbeArgs) -> anyhow::Result<()> {
    println!("正在检测本机可用的编码器…");
    let map = if args.output_dir.is_some() {
        let dir = args.output_dir.clone().unwrap_or_else(encode::capability::default_probe_dir);
        encode::capability::probe_into(&dir)
    } else {
        encode::capability::probe_all()
    };

    let ffmpeg_info = ffmpeg::detect(std::path::Path::new(""));
    println!(
        "FFmpeg：{}",
        ffmpeg_info
            .as_ref()
            .map(|info| info.summary())
            .unwrap_or_else(|| "未找到（MKV / WebM / MOV 等格式不可用）".to_string())
    );
    println!();

    let mut failed = 0usize;
    for (container, codec, result) in map.entries() {
        let name = format!("{}/{}", container.id(), codec.id());
        match result {
            Ok(bytes) => println!("  ✔ {name:<18} 写出 {}", human_size(bytes)),
            Err(err) => {
                failed += 1;
                println!("  ✘ {name:<18} {err}");
            }
        }
    }

    println!();
    println!("{}", map.summary());
    if failed > 0 {
        println!("不可用的组合会在界面中被标记；也可改用其它编码器或安装 FFmpeg。");
    }

    if let Some(dir) = &args.output_dir {
        println!("探测文件保留在 {}", dir.display());
    }

    if failed > 0 && args.strict {
        anyhow::bail!("有 {failed} 个编码器组合不可用");
    }
    Ok(())
}

// ----------------------------------------------------------------- frame ----

fn capture_frame(args: FrameArgs) -> anyhow::Result<()> {
    let _ = logging::init(log::LevelFilter::Info, true);

    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let size = crate::capture::screenshot::capture_monitor_to_file(args.monitor, &args.out)?;
    println!("已保存 {}（{}）", args.out.display(), human_size(size));
    Ok(())
}

// ---------------------------------------------------------------- doctor ----

fn doctor() -> anyhow::Result<()> {
    let _ = logging::init(log::LevelFilter::Info, true);

    println!("ScreenRec {}", env!("CARGO_PKG_VERSION"));
    println!("可执行文件：{}", std::env::current_exe()?.display());
    println!("配置目录：{}", crate::config::app_data_dir().display());
    println!("配置文件：{}", crate::config::config_path().display());
    println!("日志文件：{}", logging::current_log_path().display());

    let config = AppConfig::load();
    println!();
    println!("采集方式：{}", config.capture_method.label());
    let history = HistoryStore::load();
    println!(
        "录制记录：{} 条，合计 {}（{}）",
        history.len(),
        human_size(history.total_bytes()),
        history.path().display()
    );
    println!(
        "默认输出目录：{}",
        config.output_dir.display()
    );
    println!(
        "默认格式：{} / {}",
        config.container.id(),
        config.video_codec.id()
    );
    match config.output_dir_status() {
        Ok(()) => println!("输出目录可写：是"),
        Err(err) => {
            println!("输出目录可写：否（{err}）");
            match crate::paths::find_writable_fallback(&config.output_dir) {
                Some(fallback) => println!("将自动改用：{}", fallback.display()),
                None => println!("没有找到可写的备选目录，请用 --output-dir 指定"),
            }
        }
    }

    let ffmpeg_info = ffmpeg::detect(config.ffmpeg_path.as_path());
    println!(
        "FFmpeg：{}",
        ffmpeg_info
            .as_ref()
            .map(|info| info.summary())
            .unwrap_or_else(|| "未找到（可选）".to_string())
    );

    let monitors = capture::sources::list_monitors();
    println!();
    println!("显示器：{} 个", monitors.len());
    for monitor in &monitors {
        println!("  [{}] {}", monitor.index, monitor.describe());
    }

    let windows = capture::sources::list_windows();
    println!("可录制窗口：{} 个", windows.len());

    println!();
    println!("音频配置：{}", if config.audio.is_active() { "启用" } else { "关闭" });
    if config.audio.is_active() {
        println!(
            "  系统声音：{}，麦克风：{}，{} Hz / {} 声道",
            if config.audio.system { "开" } else { "关" },
            if config.audio.microphone { "开" } else { "关" },
            config.audio.sample_rate,
            config.audio.channels
        );
    }

    println!();
    println!("Windows 显示缩放等系统设置可能影响窗口尺寸，如录制结果异常请先用 `screenrec probe` 验证编码器。");
    Ok(())
}

// --------------------------------------------------------------- helpers ----

fn human_size(bytes: u64) -> String {
    crate::units::format_size(bytes)
}

/// Truncates a string to `max_columns` terminal columns, appending an ellipsis.
///
/// Uses display width rather than `char` count so Chinese titles line up.
fn truncate(text: &str, max_columns: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let width = crate::units::char_width(character);
        if used + width > max_columns {
            out.push('…');
            return out;
        }
        out.push(character);
        used += width;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.00 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn truncate_respects_display_width() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 10), "abc");
        // Each Chinese character occupies two columns.
        assert_eq!(truncate("记事本", 4), "记事…");
        assert_eq!(truncate("记事本", 6), "记事本");
        assert!(crate::units::display_width(&truncate("记事本标题很长", 10)) <= 11);
    }

    #[test]
    fn window_queries_accept_handles_and_titles() {
        let windows = vec![
            WindowInfo {
                hwnd: 0x1234,
                title: "记事本".into(),
                process: "notepad.exe".into(),
                pid: 10,
                width: 800,
                height: 600,
                is_foreground: true,
            },
            WindowInfo {
                hwnd: 0x5678,
                title: "计算器".into(),
                process: "calc.exe".into(),
                pid: 11,
                width: 400,
                height: 600,
                is_foreground: false,
            },
        ];

        let by_handle = resolve_window_query(&windows, "0x1234").unwrap();
        assert!(matches!(by_handle, SourceSelection::Window { hwnd: 0x1234, .. }));

        let by_title = resolve_window_query(&windows, "计算").unwrap();
        assert!(matches!(by_title, SourceSelection::Window { hwnd: 0x5678, .. }));

        assert!(resolve_window_query(&windows, "不存在").is_err());
    }

    #[test]
    fn ambiguous_window_queries_are_rejected() {
        let windows = vec![
            WindowInfo {
                hwnd: 1,
                title: "文档 - 记事本".into(),
                process: "notepad.exe".into(),
                pid: 10,
                width: 800,
                height: 600,
                is_foreground: false,
            },
            WindowInfo {
                hwnd: 2,
                title: "图片 - 记事本".into(),
                process: "notepad.exe".into(),
                pid: 11,
                width: 800,
                height: 600,
                is_foreground: false,
            },
        ];
        assert!(resolve_window_query(&windows, "记事本").is_err());
    }

    #[test]
    fn overrides_apply_to_the_config() {
        let mut config = AppConfig::default();
        let args = RecordArgs {
            monitor: None,
            window: None,
            picker: false,
            duration: 1,
            format: Some("gif".into()),
            codec: None,
            fps: Some(12),
            quality: Some("high".into()),
            bitrate_kbps: None,
            output_dir: Some(PathBuf::from(r"D:\clips")),
            filename: Some("{date}_test".into()),
            scale: Some("720p".into()),
            max_width: Some(800),
            no_cursor: true,
            border: true,
            system_audio: false,
            no_audio: true,
            mic: false,
            ffmpeg: None,
            countdown: 0,
            capture: Some("dxgi".into()),
            region: Some("100,200,640,480".into()),
            test_pattern: false,
            json: false,
            watermark_text: None,
            watermark_pattern: None,
            watermark_motion: None,
            watermark_repeat: None,
            watermark_opacity: None,
            watermark_size: None,
            watermark_color: None,
            watermark_anchor: None,
            watermark_speed: None,
        };
        apply_overrides(&mut config, &args).unwrap();
        assert_eq!(config.container, Container::Gif);
        assert_eq!(config.frame_rate, 12);
        assert_eq!(config.quality, QualityPreset::High);
        assert_eq!(config.output_dir, PathBuf::from(r"D:\clips"));
        assert_eq!(config.scale, ScaleOption::P720);
        assert_eq!(config.max_output_width, 800);
        assert!(!config.capture_cursor);
        assert!(config.capture_border);
        assert!(!config.audio.enabled);
        assert_eq!(config.capture_method, CaptureMethod::Dxgi);
    }

    #[test]
    fn unknown_format_names_are_rejected() {
        let mut config = AppConfig::default();
        let args = RecordArgs {
            monitor: None,
            window: None,
            picker: false,
            duration: 1,
            format: Some("not-a-format".into()),
            codec: None,
            fps: None,
            quality: None,
            bitrate_kbps: None,
            output_dir: None,
            filename: None,
            scale: None,
            max_width: None,
            no_cursor: false,
            border: false,
            system_audio: false,
            no_audio: false,
            mic: false,
            ffmpeg: None,
            countdown: 0,
            capture: None,
            region: None,
            test_pattern: false,
            json: false,
            watermark_text: None,
            watermark_pattern: None,
            watermark_motion: None,
            watermark_repeat: None,
            watermark_opacity: None,
            watermark_size: None,
            watermark_color: None,
            watermark_anchor: None,
            watermark_speed: None,
        };
        assert!(apply_overrides(&mut config, &args).is_err());
    }

    #[test]
    fn region_flag_is_parsed_and_rejects_bad_input() {
        let mut config = AppConfig::default();

        let mut args = RecordArgs {
            monitor: None,
            window: None,
            picker: false,
            duration: 1,
            format: None,
            codec: None,
            fps: None,
            quality: None,
            bitrate_kbps: None,
            output_dir: None,
            filename: None,
            scale: None,
            max_width: None,
            no_cursor: false,
            border: false,
            system_audio: false,
            no_audio: false,
            mic: false,
            ffmpeg: None,
            countdown: 0,
            capture: None,
            region: Some("800x600+100+200".into()),
            test_pattern: false,
            json: false,
            watermark_text: None,
            watermark_pattern: None,
            watermark_motion: None,
            watermark_repeat: None,
            watermark_opacity: None,
            watermark_size: None,
            watermark_color: None,
            watermark_anchor: None,
            watermark_speed: None,
        };
        apply_overrides(&mut config, &args).unwrap();
        assert_eq!(config.region, Some(Region::new(100, 200, 800, 600)));

        // Both notations must agree.
        args.region = Some("100,200,800,600".into());
        apply_overrides(&mut config, &args).unwrap();
        assert_eq!(config.region, Some(Region::new(100, 200, 800, 600)));

        // Bad input is reported instead of silently recording the whole screen.
        args.region = Some("nonsense".into());
        assert!(apply_overrides(&mut config, &args).is_err());

        // No flag keeps whatever the config had.
        args.region = None;
        apply_overrides(&mut config, &args).unwrap();
        assert_eq!(config.region, Some(Region::new(100, 200, 800, 600)));
    }
}
