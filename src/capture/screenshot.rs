//! Single-frame capture ("screenshots") through DXGI Desktop Duplication.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat};
use windows_capture::encoder::ImageFormat;
use windows_capture::monitor::Monitor;

use crate::encode::util;

/// The only duplication format the image encoder can serialise.
const SUPPORTED_FORMATS: [DxgiDuplicationFormat; 1] = [DxgiDuplicationFormat::Bgra8];

/// Picks the image format that matches the file extension.
pub fn format_for_path(path: &Path) -> ImageFormat {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => ImageFormat::Jpeg,
        Some("bmp") => ImageFormat::Bmp,
        Some("tif" | "tiff") => ImageFormat::Tiff,
        _ => ImageFormat::Png,
    }
}

/// Captures one frame of `monitor_index` into `path`.
///
/// Desktop Duplication returns empty frames right after the session starts, so
/// this retries for a short while before giving up.
///
/// Pixels are read back and written with the `image` crate rather than the
/// encoder's `save_as_image`, because the duplication session may hand out a
/// pixel format the image encoder rejects.
pub fn capture_monitor_to_file(monitor_index: usize, path: &Path) -> anyhow::Result<u64> {
    let monitor = Monitor::from_index(monitor_index)
        .map_err(|err| anyhow::anyhow!("无法打开显示器 {monitor_index}：{err}"))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // The default duplication formats include 16-bit float, which cannot be
    // serialised as an image, so ask for BGRA8 explicitly.
    let mut api = DxgiDuplicationApi::new_options(monitor, &SUPPORTED_FORMATS)
        .map_err(|err| anyhow::anyhow!("无法启动桌面复制：{err}"))?;

    let mut staging: Vec<u8> = Vec::new();
    let mut rgba: Vec<u8> = Vec::new();
    let mut last_error = None;

    for _ in 0..40 {
        match api.acquire_next_frame(500) {
            Ok(mut frame) => {
                let width = frame.width();
                let height = frame.height();
                let format = frame.format();

                {
                    let buffer = frame
                        .buffer()
                        .map_err(|err| anyhow::anyhow!("读取画面数据失败：{err}"))?;
                    let data = buffer.as_nopadding_buffer(&mut staging);
                    rgba.clear();
                    match format {
                        DxgiDuplicationFormat::Bgra8 => {
                            util::bgra_to_rgba(data, &mut rgba);
                        }
                        DxgiDuplicationFormat::Rgba8 => rgba.extend_from_slice(data),
                        DxgiDuplicationFormat::Rgba16F => {
                            // HDR desktops always duplicate as 16-bit float.
                            let mut bgra = Vec::new();
                            util::hdr_fp16_to_bgra(data, &mut bgra);
                            util::bgra_to_rgba(&bgra, &mut rgba);
                        }
                    }
                }

                write_image(path, &rgba, width, height)?;
                let size = std::fs::metadata(path)?.len();
                log::info!("截图已保存：{}（{size} 字节）", path.display());
                return Ok(size);
            }
            Err(err) => {
                last_error = Some(err.to_string());
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    }

    anyhow::bail!(
        "抓取画面失败：{}",
        last_error.unwrap_or_else(|| "未知错误".to_string())
    )
}

/// Serialises an RGBA buffer using the format implied by the extension.
fn write_image(path: &Path, rgba: &[u8], width: u32, height: u32) -> anyhow::Result<()> {
    let expected = (width as usize) * (height as usize) * 4;
    anyhow::ensure!(
        rgba.len() >= expected,
        "画面数据不完整：{} < {}",
        rgba.len(),
        expected
    );
    let rgba = &rgba[..expected];

    match format_for_path(path) {
        ImageFormat::Jpeg => {
            // JPEG has no alpha channel; drop it rather than failing.
            let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
            for pixel in rgba.chunks_exact(4) {
                rgb.extend_from_slice(&pixel[..3]);
            }
            let file = File::create(path)?;
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                std::io::BufWriter::new(file),
                92,
            );
            encoder.encode(&rgb, width, height, image::ExtendedColorType::Rgb8)?;
        }
        ImageFormat::Bmp => image::save_buffer_with_format(
            path,
            rgba,
            width,
            height,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Bmp,
        )?,
        _ => image::save_buffer_with_format(
            path,
            rgba,
            width,
            height,
            image::ExtendedColorType::Rgba8,
            image::ImageFormat::Png,
        )?,
    }
    Ok(())
}

/// Captures one frame of the primary monitor.
pub fn capture_primary_to_file(path: &Path) -> anyhow::Result<u64> {
    let index = crate::capture::sources::list_monitors()
        .iter()
        .find(|monitor| monitor.is_primary)
        .map(|monitor| monitor.index)
        .unwrap_or(0);
    capture_monitor_to_file(index, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_selects_the_image_format() {
        assert!(matches!(
            format_for_path(&PathBuf::from("a.png")),
            ImageFormat::Png
        ));
        assert!(matches!(
            format_for_path(&PathBuf::from("a.JPG")),
            ImageFormat::Jpeg
        ));
        assert!(matches!(
            format_for_path(&PathBuf::from("a.bmp")),
            ImageFormat::Bmp
        ));
        assert!(matches!(
            format_for_path(&PathBuf::from("a.unknown")),
            ImageFormat::Png
        ));
    }
}
