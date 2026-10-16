//! Desktop application (egui/eframe).

pub mod app;
pub mod theme;
pub mod widgets;

pub use app::ScreenRecApp;

/// Runs the desktop application.
pub fn run() -> anyhow::Result<()> {
    let log_path = crate::logging::init_for_gui();
    if let Some(path) = &log_path {
        log::info!("启动界面，日志：{}", path.display());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ScreenRec — 屏幕录制")
            .with_inner_size([1060.0, 720.0])
            .with_min_inner_size([880.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ScreenRec",
        options,
        Box::new(|cc| Ok(Box::new(ScreenRecApp::new(cc)))),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
}

/// Reports a fatal startup problem in a message box, since a GUI build has no
/// console to print to.
pub fn show_fatal_error(message: &str) {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
    };

    let text = HSTRING::from(message);
    let caption = HSTRING::from("ScreenRec 启动失败");
    unsafe {
        MessageBoxW(None, &text, &caption, MB_OK | MB_ICONERROR | MB_SETFOREGROUND);
    }
}
