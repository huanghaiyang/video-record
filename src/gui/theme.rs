//! Fonts and colours.
//!
//! egui ships Latin-only fonts, so a CJK face is loaded from the Windows font
//! directory; without it every Chinese label renders as tofu boxes.

use std::sync::Arc;

use crate::config::ThemeMode;

/// Accent colours used across the UI.
pub mod colors {
    use egui::Color32;

    pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x9A, 0xFF);
    pub const RECORD: Color32 = Color32::from_rgb(0xE5, 0x4B, 0x4B);
    pub const PAUSE: Color32 = Color32::from_rgb(0xE8, 0xA5, 0x3B);
    pub const OK: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x8A);
    pub const WARN: Color32 = Color32::from_rgb(0xE0, 0xA0, 0x30);
    pub const ERROR: Color32 = Color32::from_rgb(0xE0, 0x5A, 0x5A);
    pub const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA2, 0xAE);
}

/// Candidate CJK fonts, best first.
const CJK_FONTS: [&str; 8] = [
    "msyh.ttc",
    "msyhl.ttc",
    "msyhbd.ttc",
    "simhei.ttf",
    "Deng.ttf",
    "simsun.ttc",
    "msjh.ttc",
    "msjhl.ttc",
];

/// Symbol fonts used as a fallback for glyphs the CJK face lacks (⚠, ✔, …).
const SYMBOL_FONTS: [&str; 3] = ["seguisym.ttf", "seguiemj.ttf", "seguisb.ttf"];

/// Installs the default fonts plus CJK and symbol fallbacks.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut fallbacks: Vec<String> = Vec::new();

    if let Some((name, bytes)) = load_font(&CJK_FONTS) {
        fonts
            .font_data
            .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        // CJK first for proportional text so Chinese glyphs are always found.
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        fallbacks.push("cjk".to_owned());
        log::info!("已加载中文字体：{name}");
    } else {
        log::warn!("未找到中文字体，界面中的中文可能显示为方块");
    }

    if let Some((name, bytes)) = load_font(&SYMBOL_FONTS) {
        fonts
            .font_data
            .insert("symbols".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        fallbacks.push("symbols".to_owned());
        log::info!("已加载符号字体：{name}");
    }

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        for name in &fallbacks {
            if !list.contains(name) {
                list.push(name.clone());
            }
        }
    }

    ctx.set_fonts(fonts);
}

fn load_font(candidates: &[&str]) -> Option<(String, Vec<u8>)> {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
    let fonts_dir = std::path::Path::new(&windir).join("Fonts");

    for name in candidates {
        let path = fonts_dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() => return Some((name.to_string(), bytes)),
            _ => continue,
        }
    }
    None
}

/// Applies colours and spacing.
pub fn apply_style(ctx: &egui::Context, mode: ThemeMode) {
    let mut visuals = match mode {
        ThemeMode::Light => egui::Visuals::light(),
        _ => egui::Visuals::dark(),
    };

    visuals.selection.bg_fill = colors::ACCENT.gamma_multiply(0.45);
    visuals.hyperlink_color = colors::ACCENT;
    if matches!(mode, ThemeMode::Dark | ThemeMode::System) {
        visuals.panel_fill = egui::Color32::from_rgb(0x16, 0x18, 0x1C);
        visuals.window_fill = egui::Color32::from_rgb(0x1B, 0x1E, 0x23);
    }

    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 24.0;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cjk_font_exists_on_a_normal_windows_install() {
        // This is a smoke test for the lookup logic; it reports what it found
        // instead of asserting, because minimal Windows images may not carry
        // any of the candidates.
        match load_font(&CJK_FONTS) {
            Some((name, bytes)) => {
                assert!(!bytes.is_empty());
                assert!(CJK_FONTS.contains(&name.as_str()));
            }
            None => eprintln!("no CJK font found (acceptable on minimal images)"),
        }
    }

    #[test]
    fn symbol_fonts_are_looked_up_separately() {
        match load_font(&SYMBOL_FONTS) {
            Some((name, bytes)) => {
                assert!(!bytes.is_empty());
                assert!(SYMBOL_FONTS.contains(&name.as_str()));
            }
            None => eprintln!("no symbol font found (acceptable on minimal images)"),
        }
    }
}
