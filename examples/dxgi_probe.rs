//! Diagnostic helper: dump what DXGI Desktop Duplication actually returns.
//!
//! ```text
//! cargo run --example dxgi_probe -- 1
//! ```
//!
//! Prints, for each requested pixel format, the negotiated format, the frame
//! size, statistic of the raw samples and writes a PNG next to the working
//! directory so the result can be inspected visually.

use std::path::PathBuf;

use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat};
use windows_capture::monitor::Monitor;

use screenrec::encode::util;

fn main() -> anyhow::Result<()> {
    let index: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);

    let monitors = screenrec::capture::sources::list_monitors();
    for monitor in &monitors {
        println!("monitor [{}] {} ", monitor.index, monitor.describe());
    }

    let monitor = Monitor::from_index(index)?;
    println!(
        "target: {}x{} @ {}Hz",
        monitor.width()?,
        monitor.height()?,
        monitor.refresh_rate().unwrap_or(0)
    );

    for requested in [
        DxgiDuplicationFormat::Bgra8,
        DxgiDuplicationFormat::Rgba8,
        DxgiDuplicationFormat::Rgba16F,
    ] {
        println!("\n--- requested {requested:?} ---");
        match probe(monitor, requested) {
            Ok(()) => {}
            Err(err) => println!("  failed: {err}"),
        }
    }
    Ok(())
}

fn probe(monitor: Monitor, requested: DxgiDuplicationFormat) -> anyhow::Result<()> {
    let mut api = DxgiDuplicationApi::new_options(monitor, &[requested])?;
    let mut staging = Vec::new();

    for attempt in 0..20 {
        let mut frame = match api.acquire_next_frame(500) {
            Ok(frame) => frame,
            Err(err) => {
                println!("  attempt {attempt}: {err}");
                std::thread::sleep(std::time::Duration::from_millis(80));
                continue;
            }
        };

        let width = frame.width();
        let height = frame.height();
        let format = frame.format();
        let (bytes, stats, sample): (usize, String, Vec<u8>) = {
            let buffer = frame.buffer()?;
            let data = buffer.as_nopadding_buffer(&mut staging);
            (data.len(), describe(data, format), data[..data.len().min(32)].to_vec())
        };

        println!("  negotiated: {format:?}  {width}x{height}  {bytes} bytes");
        println!("  first bytes: {sample:02X?}");
        println!("  {stats}");

        // Write a PNG for visual inspection.
        let path = PathBuf::from(format!("dxgi_probe_{requested:?}.png"));
        match format {
            DxgiDuplicationFormat::Bgra8 => {
                let mut rgba = Vec::new();
                util::bgra_to_rgba(&staging[..(width as usize) * (height as usize) * 4], &mut rgba);
                image::save_buffer_with_format(
                    &path,
                    &rgba,
                    width,
                    height,
                    image::ExtendedColorType::Rgba8,
                    image::ImageFormat::Png,
                )?;
            }
            DxgiDuplicationFormat::Rgba8 => {
                image::save_buffer_with_format(
                    &path,
                    &staging[..(width as usize) * (height as usize) * 4],
                    width,
                    height,
                    image::ExtendedColorType::Rgba8,
                    image::ImageFormat::Png,
                )?;
            }
            DxgiDuplicationFormat::Rgba16F => {
                let mut bgra = Vec::new();
                util::hdr_fp16_to_bgra(&staging[..(width as usize) * (height as usize) * 8], &mut bgra);
                let mut rgba = Vec::new();
                util::bgra_to_rgba(&bgra, &mut rgba);
                image::save_buffer_with_format(
                    &path,
                    &rgba,
                    width,
                    height,
                    image::ExtendedColorType::Rgba8,
                    image::ImageFormat::Png,
                )?;
            }
        }
        println!("  wrote {}", path.display());
        return Ok(());
    }

    anyhow::bail!("no frame acquired")
}

/// Summarises the raw samples so an all-white or all-black buffer is obvious.
fn describe(data: &[u8], format: DxgiDuplicationFormat) -> String {
    match format {
        DxgiDuplicationFormat::Rgba16F => {
            let mut min = f32::MAX;
            let mut max = f32::MIN;
            let mut sum = 0f64;
            let mut count = 0f64;
            for chunk in data.chunks_exact(8) {
                for channel in 0..4 {
                    let value = util::f16_to_f32(u16::from_le_bytes([
                        chunk[channel * 2],
                        chunk[channel * 2 + 1],
                    ]));
                    if value.is_finite() {
                        min = min.min(value);
                        max = max.max(value);
                        sum += value as f64;
                        count += 1.0;
                    }
                }
            }
            format!(
                "f16 samples min={min:.4} max={max:.4} mean={:.4}",
                if count > 0.0 { sum / count } else { 0.0 }
            )
        }
        _ => {
            let mut min = u8::MAX;
            let mut max = u8::MIN;
            let mut sum = 0f64;
            for &value in data.iter().step_by(4) {
                min = min.min(value);
                max = max.max(value);
                sum += value as f64;
            }
            let count = (data.len() / 4).max(1) as f64;
            format!("u8 red channel min={min} max={max} mean={:.2}", sum / count)
        }
    }
}
