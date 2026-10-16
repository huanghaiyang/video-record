//! PNG / JPEG frame-sequence writer.

use std::fs::File;
use std::path::PathBuf;

use super::util::bgra_to_rgba;
use super::{FrameData, FrameSink, SinkParams};
use crate::formats::Container;
use crate::paths::PlannedOutput;

/// JPEG quality for sequence output (1-100).
const JPEG_QUALITY: u8 = 92;

pub struct ImageSequenceSink {
    dir: PathBuf,
    format: Container,
    index: u64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl ImageSequenceSink {
    pub fn new(
        format: Container,
        params: SinkParams,
        output: &PlannedOutput,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            format.is_image_sequence(),
            "{} 不是图像序列格式",
            format.id()
        );
        output.prepare()?;
        log::info!(
            "图像序列编码器已启动: {} · {}x{} → {}",
            format.id(),
            params.width,
            params.height,
            output.path.display()
        );

        Ok(Self {
            dir: output.path.clone(),
            format,
            index: 0,
            width: params.width,
            height: params.height,
            rgba: Vec::new(),
        })
    }

    fn write_frame(&self, rgba: &[u8], path: &std::path::Path) -> anyhow::Result<()> {
        match self.format {
            Container::PngSeq => {
                image::save_buffer_with_format(
                    path,
                    rgba,
                    self.width,
                    self.height,
                    image::ExtendedColorType::Rgba8,
                    image::ImageFormat::Png,
                )?;
            }
            Container::JpegSeq => {
                // JPEG has no alpha channel; the image crate rejects RGBA for
                // the JPEG encoder, so drop it.
                let rgb = rgba_to_rgb(rgba);
                let file = File::create(path)?;
                let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                    std::io::BufWriter::new(file),
                    JPEG_QUALITY,
                );
                encoder.encode(&rgb, self.width, self.height, image::ExtendedColorType::Rgb8)?;
            }
            other => anyhow::bail!("不支持的图像序列格式 {}", other.id()),
        }
        Ok(())
    }
}

/// Drops the alpha channel from an RGBA buffer.
fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    rgb
}

impl FrameSink for ImageSequenceSink {
    fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        anyhow::ensure!(
            frame.pixels.len() >= expected,
            "帧大小不匹配: {} < {}",
            frame.pixels.len(),
            expected
        );

        bgra_to_rgba(&frame.pixels[..expected], &mut self.rgba);
        self.index += 1;
        let path = self
            .dir
            .join(format!("frame_{:06}.{}", self.index, self.format.extension()));
        self.write_frame(&self.rgba, &path)
            .map_err(|err| anyhow::anyhow!("写入 {} 失败: {err}", path.display()))
    }

    fn finish(self: Box<Self>) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(self.index > 0, "没有捕获到任何帧，未生成图像序列");
        log::info!("图像序列完成: {} 帧 → {}", self.index, self.dir.display());
        Ok(self.dir.clone())
    }

    fn describe(&self) -> String {
        format!("图像序列 · {} 帧", self.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::Container;
    use crate::paths::PlannedOutput;

    /// Writes two frames and returns the output path plus the temporary root
    /// the caller cleans up after inspecting the files.
    fn write_two_frames(
        container: Container,
        extension: &str,
    ) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
        let dir = std::env::temp_dir().join(format!(
            "screenrec-imgsink-{}-{}",
            std::process::id(),
            container.id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;

        let output = if container.is_image_sequence() {
            PlannedOutput::sequence(dir.join("frames"))
        } else {
            PlannedOutput::file(dir.join(format!("out.{extension}")))
        };
        output.prepare()?;

        let params = SinkParams { width: 8, height: 4, fps: 5, bitrate: 1_000_000, audio: None };
        let mut sink: Box<dyn FrameSink> =
            Box::new(ImageSequenceSink::new(container, params, &output)?);

        let mut pixels = vec![0u8; 8 * 4 * 4];
        for (index, chunk) in pixels.chunks_exact_mut(4).enumerate() {
            chunk[0] = index as u8;
            chunk[1] = 128;
            chunk[2] = 255u8.saturating_sub(index as u8);
            chunk[3] = 255;
        }
        for frame in 0..2i64 {
            sink.push(&FrameData {
                pixels: &pixels,
                width: 8,
                height: 4,
                timestamp_100ns: frame * 2_000_000,
            })?;
        }
        let result = sink.finish()?;
        Ok((result, dir))
    }

    #[test]
    fn png_sequence_writes_readable_frames() {
        let (result, root) = write_two_frames(Container::PngSeq, "png").expect("png sequence");
        let first = result.join("frame_000001.png");
        assert!(first.exists(), "expected {first:?}");
        assert!(result.join("frame_000002.png").exists());
        assert!(std::fs::metadata(&first).unwrap().len() > 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn jpeg_sequence_drops_alpha_instead_of_failing() {
        // Regression: the image crate's JPEG encoder rejects RGBA input, which
        // used to make this export fail at save time.
        let (result, root) = write_two_frames(Container::JpegSeq, "jpg").expect("jpeg sequence");
        let frame = result.join("frame_000001.jpg");
        assert!(frame.exists(), "expected {frame:?}");
        assert!(std::fs::metadata(&frame).unwrap().len() > 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rgba_to_rgb_removes_the_alpha_channel() {
        let rgb = rgba_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 128]);
        assert_eq!(rgb, vec![1, 2, 3, 4, 5, 6]);
        assert!(rgba_to_rgb(&[]).is_empty());
    }
}
