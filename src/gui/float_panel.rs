//! Floating recording control bar.
//!
//! While a recording runs, a small always-on-top window shows the elapsed time,
//! the frame count and a stop button, so the main window can be hidden or
//! covered by the recorded application.
//!
//! The panel is explicitly **excluded from screen capture** with
//! `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`, which Windows 10 2004 and
//! later honour for both Windows Graphics Capture and Desktop Duplication. On
//! older builds the panel falls back to `WDA_MONITOR`, which makes it appear as a
//! black rectangle instead of leaking into the recording.

use std::sync::Arc;

use crate::capture::session::SessionShared;

/// What the user did in the floating panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatAction {
    Stop,
}

/// Title of the floating window; also used to find its handle.
const TITLE: &str = "ScreenRec 浮动控制";

/// State of the floating panel across frames.
#[derive(Default)]
pub struct FloatPanel {
    /// Native handle, once Windows has created the window and we found it.
    handle: Option<isize>,
    /// How often the handle lookup was attempted, so it can give up on failure.
    attempts: u32,
    /// True while the panel is on screen.
    visible: bool,
}

impl FloatPanel {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while the panel is shown.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Draws (or hides) the panel for this frame.
    ///
    /// Returns an action when the user pressed the stop button.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        snapshot: &crate::capture::session::SessionSnapshot,
        shared: Option<&Arc<SessionShared>>,
    ) -> Option<FloatAction> {
        let recording = snapshot.status.is_active();
        if !self.visible && !recording {
            return None;
        }

        // The viewport callback is called every frame and must be `Fn`, so the
        // button state travels back through an atomic rather than a captured
        // local. The data it draws is copied in for the same reason.
        let stop_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = std::sync::Arc::clone(&stop_requested);
        let shown = snapshot.clone();
        let viewport = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([300.0, 84.0])
            .with_min_inner_size([240.0, 72.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_transparent(false)
            .with_visible(recording);

        ctx.show_viewport_deferred(
            egui::ViewportId::from_hash_of("screenrec-float-panel"),
            viewport,
            move |ctx, _class| {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(egui::Color32::from_rgb(24, 28, 36))
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .corner_radius(8),
                    )
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            let pulse = if shown.status.is_active() { "●" } else { "■" };
                            ui.label(
                                egui::RichText::new(pulse)
                                    .color(egui::Color32::from_rgb(229, 72, 77))
                                    .size(18.0),
                            );
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(format_drief(shown.elapsed))
                                        .size(20.0)
                                        .strong()
                                        .color(egui::Color32::from_rgb(230, 235, 245)),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} 帧 · {}",
                                        shown.frames, shown.backend
                                    ))
                                    .size(11.0)
                                    .color(egui::Color32::from_rgb(140, 150, 165)),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new("停止").size(14.0),
                                            )
                                            .fill(egui::Color32::from_rgb(200, 50, 55))
                                            .corner_radius(6)
                                            .min_size(egui::vec2(62.0, 30.0)),
                                        )
                                        .clicked()
                                    {
                                        stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                                    }
                                },
                            );
                        });
                    });
            },
        );

        if recording {
            self.ensure_excluded();
        } else if self.visible {
            self.visible = false;
        }
        self.visible = recording;
        let _ = shared;
        if stop_requested.load(std::sync::atomic::Ordering::Relaxed) {
            stop_requested.store(false, std::sync::atomic::Ordering::Relaxed);
            Some(FloatAction::Stop)
        } else {
            None
        }
    }

    /// Applies the capture exclusion to the panel's native window.
    ///
    /// egui does not expose the `HWND`, so the window is looked up by its title
    /// (unique to this panel) for a few frames after it appears.
    fn ensure_excluded(&mut self) {
        #[cfg(windows)]
        {
            if self.handle.is_some() || self.attempts > 120 {
                return;
            }
            self.attempts += 1;
            use windows::core::HSTRING;
            use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
            let title = HSTRING::from(TITLE);
            let hwnd = unsafe { FindWindowW(None, &title) }.ok();
            let Some(hwnd) = hwnd else {
                return;
            };
            if let Err(err) = crate::paths::exclude_from_capture(hwnd) {
                log::warn!("浮动控制条无法设置为不参与录制：{err}");
            } else {
                log::info!("浮动控制条已排除在录制画面之外");
            }
            self.handle = Some(hwnd.0 as isize);
        }
    }
}

/// `mm:ss` (or `h:mm:ss`) for the panel.
fn format_drief(elapsed: std::time::Duration) -> String {
    let total = elapsed.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_formatted_as_minutes_and_seconds() {
        assert_eq!(format_drief(std::time::Duration::from_secs(0)), "00:00");
        assert_eq!(format_drief(std::time::Duration::from_secs(59)), "00:59");
        assert_eq!(format_drief(std::time::Duration::from_secs(61)), "01:01");
        assert_eq!(format_drief(std::time::Duration::from_secs(3661)), "1:01:01");
    }
}
