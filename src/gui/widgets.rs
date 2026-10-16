//! Small reusable UI pieces.

use egui::{Color32, RichText, Ui};

use super::theme::colors;

/// A titled group with consistent spacing.
pub fn section<R>(ui: &mut Ui, title: &str, contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(4.0);
    ui.label(RichText::new(title).strong().size(15.0));
    let inner = ui.group(|ui| {
        ui.set_width(ui.available_width());
        contents(ui)
    });
    ui.add_space(4.0);
    inner.inner
}

/// Muted helper text.
pub fn hint(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(colors::MUTED));
}

/// A coloured pill, used for statuses.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(
        RichText::new(format!(" {text} "))
            .small()
            .color(Color32::WHITE)
            .background_color(color),
    );
}

/// `label: value` pair.
pub fn key_value(ui: &mut Ui, key: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).color(colors::MUTED));
        ui.label(value.into());
    });
}

/// A large action button.
pub fn action_button(ui: &mut Ui, text: &str, color: Color32, enabled: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(text).size(16.0).color(Color32::WHITE))
        .fill(color)
        .min_size(egui::vec2(160.0, 40.0));
    ui.add_enabled(enabled, button)
}

/// A read-only path with a "browse" button.
///
/// Returns true when the user picked a new folder.
pub fn folder_row(ui: &mut Ui, label: &str, path: &mut std::path::PathBuf) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
    });
    ui.horizontal(|ui| {
        let mut text = path.display().to_string();
        let response = ui.add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(ui.available_width() - 96.0)
                .hint_text("录制文件夹"),
        );
        if response.changed() {
            *path = std::path::PathBuf::from(text);
            changed = true;
        }
        if ui.button("选择…").clicked() {
            let mut dialog = rfd::FileDialog::new();
            if path.is_dir() {
                dialog = dialog.set_directory(path.clone());
            }
            if let Some(picked) = dialog.pick_folder() {
                *path = picked;
                changed = true;
            }
        }
    });
    changed
}

/// Formats a duration as `mm:ss` (or `h:mm:ss` past an hour).
pub use crate::units::format_duration;

/// Human readable byte size.
pub use crate::units::format_size;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_helpers_stay_available() {
        assert_eq!(format_duration(std::time::Duration::from_secs(65)), "01:05");
        assert_eq!(format_size(1024), "1.00 KB");
    }
}
