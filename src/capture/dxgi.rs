//! DXGI Desktop Duplication capture backend.
//!
//! Windows Graphics Capture is the preferred API (it also captures single
//! windows), but it can be unavailable or blocked — remote sessions, exclusive
//! full-screen applications, restricted tokens, older drivers. Desktop
//! Duplication covers the common case of "record this whole monitor" and is
//! used automatically when Graphics Capture refuses to hand out an item.

use std::sync::Arc;
use std::time::Duration;

use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat, Error as DxgiError};
use windows_capture::monitor::Monitor;

use super::pipeline::FramePipeline;
use super::session::SessionShared;
use crate::encode::util;

/// Only BGRA8 can be written by the image/encoder paths without conversion.
const SUPPORTED_FORMATS: [DxgiDuplicationFormat; 1] = [DxgiDuplicationFormat::Bgra8];

/// How long a single frame acquisition may block before the loop ticks.
const ACQUIRE_TIMEOUT_MS: u32 = 120;

/// Records a monitor until the session is asked to stop.
pub fn run(
    monitor_index: usize,
    mut pipeline: FramePipeline,
    shared: Arc<SessionShared>,
) -> anyhow::Result<()> {
    let monitor = Monitor::from_index(monitor_index)
        .map_err(|err| anyhow::anyhow!("无法打开显示器 {monitor_index}：{err}"))?;

    let mut api = DxgiDuplicationApi::new_options(monitor, &SUPPORTED_FORMATS)
        .map_err(|err| anyhow::anyhow!("无法启动桌面复制（DXGI）：{err}"))?;

    log::info!("DXGI 桌面复制已启动（显示器 {monitor_index}）");

    let mut staging: Vec<u8> = Vec::new();
    let mut converted: Vec<u8> = Vec::new();
    let mut hdr: Vec<u8> = Vec::new();
    let mut outcome: anyhow::Result<()> = Ok(());
    let mut hdr_warned = false;

    while !shared.is_stop_requested() {
        let mut recreate = false;

        match api.acquire_next_frame(ACQUIRE_TIMEOUT_MS) {
            Ok(mut frame) => {
                let width = frame.width();
                let height = frame.height();
                let format = frame.format();

                let result = {
                    let buffer = match frame.buffer() {
                        Ok(buffer) => buffer,
                        Err(err) => {
                            log::warn!("读取桌面复制帧失败: {err}");
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                    };
                    let data = buffer.as_nopadding_buffer(&mut staging);

                    match format {
                        DxgiDuplicationFormat::Bgra8 => {
                            pipeline.push_bgra(data, width, height)
                        }
                        DxgiDuplicationFormat::Rgba8 => {
                            converted.clear();
                            converted.extend_from_slice(data);
                            util::bgra_to_rgba_in_place(&mut converted);
                            pipeline.push_bgra(&converted, width, height)
                        }
                        DxgiDuplicationFormat::Rgba16F => {
                            // HDR desktops only ever duplicate as 16-bit float.
                            if !hdr_warned {
                                hdr_warned = true;
                                log::info!(
                                    "显示器处于 HDR 模式，正在把 16 位浮点画面转换为 SDR 后再编码"
                                );
                            }
                            util::hdr_fp16_to_bgra(data, &mut hdr);
                            pipeline.push_bgra(&hdr, width, height)
                        }
                    }
                };

                if let Err(err) = result {
                    outcome = Err(err);
                    break;
                }
            }
            Err(DxgiError::Timeout) => {
                // Nothing changed on screen: extend the timeline so the
                // recording keeps its real duration.
                if let Err(err) = pipeline.tick() {
                    outcome = Err(err);
                    break;
                }
            }
            Err(DxgiError::AccessLost) => recreate = true,
            Err(err) => {
                outcome = Err(anyhow::anyhow!("桌面复制失败：{err}"));
                break;
            }
        }

        if recreate {
            log::warn!("桌面复制访问丢失，正在重建会话");
            match api.recreate() {
                Ok(new_api) => api = new_api,
                Err(err) => {
                    outcome = Err(anyhow::anyhow!("重建桌面复制会话失败：{err}"));
                    break;
                }
            }
        }
    }

    if let Err(err) = &outcome {
        pipeline.set_failure(format!("{err:#}"));
    }
    pipeline.finalize();

    // When the loop ended because of an error the session state already carries
    // the message; otherwise it completed normally.
    outcome
}
