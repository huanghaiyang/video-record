//! Runtime encoder capability detection.
//!
//! The set of containers Media Foundation can actually write depends on the
//! installed Media Foundation Transforms, which differ between machines: this
//! one has no MPEG-2 or MJPEG encoder, for example. Rather than let the user
//! pick a format that will fail at save time, the app probes each built-in
//! combination once by writing a tiny real file.
//!
//! FFmpeg-backed combinations are not probed; FFmpeg's own encoder list is
//! stable, and the binary is verified separately.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::encode::{self, FrameData, FrameSink, RowOrder, SinkParams, SinkRequest};
use crate::formats::{self, Container, QualityPreset, ScaleOption, VideoCodec};
use crate::paths::PlannedOutput;

/// Result of probing one combination.
pub type CapabilityResult = Result<u64, String>;

/// What this machine can produce.
#[derive(Debug, Clone, Default)]
pub struct CapabilityMap {
    results: HashMap<(Container, VideoCodec), CapabilityResult>,
    /// Set when the probe could not run at all (typically an unwritable temp
    /// directory). The UI then shows "未知" instead of claiming 0/0.
    probe_error: Option<String>,
}

impl CapabilityMap {
    /// Why the probe could not run, when it could not.
    pub fn probe_error(&self) -> Option<&str> {
        self.probe_error.as_deref()
    }

    /// `None` when the combination was not probed (for example FFmpeg-only).
    pub fn is_supported(&self, container: Container, codec: VideoCodec) -> Option<bool> {
        self.results.get(&(container, codec)).map(Result::is_ok)
    }

    /// Failure reason, when the probe failed.
    pub fn reason(&self, container: Container, codec: VideoCodec) -> Option<&str> {
        match self.results.get(&(container, codec)) {
            Some(Err(message)) => Some(message.as_str()),
            _ => None,
        }
    }

    pub fn bytes(&self, container: Container, codec: VideoCodec) -> Option<u64> {
        self.results.get(&(container, codec)).and_then(|r| r.as_ref().ok()).copied()
    }

    pub fn supported_count(&self) -> usize {
        self.results.values().filter(|r| r.is_ok()).count()
    }

    pub fn total(&self) -> usize {
        self.results.len()
    }

    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// Entries in a stable order, for display.
    pub fn entries(&self) -> Vec<(Container, VideoCodec, CapabilityResult)> {
        let mut entries: Vec<_> = self
            .results
            .iter()
            .map(|((container, codec), result)| (*container, *codec, result.clone()))
            .collect();
        entries.sort_by_key(|(container, codec, _)| (container.id(), codec.id()));
        entries
    }

    pub fn summary(&self) -> String {
        format!("{}/{} 个内置编码组合可用", self.supported_count(), self.total())
    }
}

/// Probes every combination the built-in backends are expected to handle.
///
/// Writes into a temporary directory which is removed afterwards; takes on the
/// order of a second on a healthy machine and fails fast where a transform is
/// missing.
pub fn probe_all() -> CapabilityMap {
    let mut dir = std::env::temp_dir().join(format!("screenrec-capability-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        // Temp not writable (sandboxed or locked-down profile): probe next to the
        // per-user data directory instead.
        dir = crate::config::app_data_dir().join("capability-probe");
    }
    let map = probe_into(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    map
}

/// Same as [`probe_all`] but into a caller-provided directory (kept on disk).
pub fn probe_into(dir: &std::path::Path) -> CapabilityMap {
    let mut results = HashMap::new();

    let mut write_error = None;
    if let Err(err) = std::fs::create_dir_all(dir) {
        log::warn!("无法创建编码器检测目录 {}: {err}", dir.display());
        write_error = Some(format!("无法创建检测目录 {}：{err}", dir.display()));
    }

    for container in Container::ALL {
        for codec in formats::codecs_for(container) {
            // Only the built-in paths can be probed; FFmpeg combinations are
            // validated by FFmpeg itself at run time.
            let native = formats::is_native_combo(container, *codec);
            let in_process = container.is_image_sequence() || container.is_animated_image();
            if !native && !in_process {
                continue;
            }

            let path = dir.join(format!(
                "probe_{}_{}.{}",
                container.id(),
                codec.id(),
                container.extension()
            ));
            let result = probe_one(container, *codec, &path);
            log::debug!(
                "编码器检测 {}/{}: {}",
                container.id(),
                codec.id(),
                match &result {
                    Ok(bytes) => format!("可用（{bytes} 字节）"),
                    Err(err) => format!("不可用（{err}）"),
                }
            );
            results.insert((container, *codec), result);
        }
    }

    CapabilityMap {
        results,
        probe_error: write_error,
    }
}

/// Writes two frames through the real sink and reports the resulting size.
fn probe_one(
    container: Container,
    codec: VideoCodec,
    path: &std::path::Path,
) -> CapabilityResult {
    let format = formats::resolve(container, codec, false)
        .map_err(|err| err.to_string())?;

    let (width, height) = formats::plan_size(container, ScaleOption::Original, 320, 240);
    let fps = 10u32;
    let params = SinkParams { width, height, fps, bitrate: 1_000_000, audio: None };

    let output = if container.is_image_sequence() {
        PlannedOutput::sequence(path.to_path_buf())
    } else {
        PlannedOutput::file(path.to_path_buf())
    };

    let mut sink = encode::create_sink(SinkRequest {
        format,
        params: params.clone(),
        output: &output,
        ffmpeg: None,
        quality: QualityPreset::Medium,
        prefer_hardware: false,
    })
    .map_err(|err| format!("{err:#}"))?;

    push_probe_frames(&mut *sink, width, height, fps).map_err(|err| format!("{err:#}"))?;
    let produced = sink.finish().map_err(|err| format!("{err:#}"))?;

    total_size(&produced, container)
}

fn total_size(path: &std::path::Path, container: Container) -> CapabilityResult {
    if container.is_image_sequence() {
        let mut total = 0u64;
        for entry in std::fs::read_dir(path).map_err(|err| err.to_string())? {
            total += entry
                .map_err(|err| err.to_string())?
                .metadata()
                .map(|meta| meta.len())
                .unwrap_or(0);
        }
        Ok(total)
    } else {
        Ok(std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
    }
}

/// Generates a couple of high-contrast frames so the encoder has real content.
fn push_probe_frames(
    sink: &mut dyn FrameSink,
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<()> {
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    for index in 0..2u64 {
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * width + x) * 4) as usize;
                let level = (((x + y + index as u32 * 16) % 256) as u8).max(16);
                pixels[offset] = level;
                pixels[offset + 1] = level.wrapping_mul(3);
                pixels[offset + 2] = 255u8.saturating_sub(level);
                pixels[offset + 3] = 255;
            }
        }
        let timestamp = (index as i64 * 10_000_000) / i64::from(fps.max(1));
        // This probe feeds the sink directly, so it honours the requested order.
        let mut flipped = Vec::new();
        let pixels = match sink.row_order() {
            RowOrder::TopDown => &pixels,
            RowOrder::BottomUp => {
                crate::encode::util::flip_vertical(&pixels, width, height, &mut flipped);
                &flipped
            }
        };
        sink.push(&FrameData {
            pixels,
            width,
            height,
            timestamp_100ns: timestamp,
            row_order: sink.row_order(),
        })?;
    }
    Ok(())
}

/// Directory used by [`probe_into`] when the caller wants to keep the files.
pub fn default_probe_dir() -> PathBuf {
    std::env::temp_dir().join("screenrec-probe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_map_reports_unknown() {
        let map = CapabilityMap::default();
        assert!(map.is_empty());
        assert_eq!(map.is_supported(Container::Mp4, VideoCodec::H264), None);
        assert_eq!(map.reason(Container::Mp4, VideoCodec::H264), None);
        assert_eq!(map.supported_count(), 0);
        assert!(map.entries().is_empty());
    }

    #[test]
    fn results_are_reported_per_combination() {
        let mut results = HashMap::new();
        results.insert((Container::Mp4, VideoCodec::H264), Ok(1024));
        results.insert((Container::Mp4, VideoCodec::Hevc), Err("缺少解码器".to_string()));
        let map = CapabilityMap {
        results,
        probe_error: None,
    };

        assert_eq!(map.is_supported(Container::Mp4, VideoCodec::H264), Some(true));
        assert_eq!(map.bytes(Container::Mp4, VideoCodec::H264), Some(1024));
        assert_eq!(map.is_supported(Container::Mp4, VideoCodec::Hevc), Some(false));
        assert_eq!(map.reason(Container::Mp4, VideoCodec::Hevc), Some("缺少解码器"));
        assert_eq!(map.supported_count(), 1);
        assert_eq!(map.total(), 2);
        assert!(map.summary().contains("1/2"));
    }

    #[test]
    fn entries_are_sorted_for_stable_output() {
        let mut results = HashMap::new();
        results.insert((Container::Wmv, VideoCodec::Wmv3), Ok(1));
        results.insert((Container::Mp4, VideoCodec::H264), Ok(1));
        let map = CapabilityMap {
        results,
        probe_error: None,
    };
        let ids: Vec<String> = map
            .entries()
            .into_iter()
            .map(|(container, codec, _)| format!("{}/{}", container.id(), codec.id()))
            .collect();
        assert_eq!(ids, vec!["mp4/h264".to_string(), "wmv/wmv3".to_string()]);
    }

    #[test]
    fn probing_into_a_directory_tests_the_builtin_paths() {
        // A real probe is I/O heavy; assert the shape of the result rather than
        // asserting that every combination works, which is machine dependent.
        let dir = std::env::temp_dir().join(format!("screenrec-cap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let map = probe_into(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(map.total() >= 5, "expected several probed combinations");
        // Image output is implemented in-process, so it must always succeed.
        assert_eq!(
            map.is_supported(Container::PngSeq, VideoCodec::H264),
            Some(true),
            "PNG sequence should always work"
        );
        assert_eq!(
            map.is_supported(Container::Gif, VideoCodec::H264),
            Some(true),
            "GIF should always work"
        );
        // FFmpeg-only containers must not be probed at all.
        assert_eq!(map.is_supported(Container::Mkv, VideoCodec::H264), None);
    }
}
