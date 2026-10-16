//! Per-frame cost measurements for the recording hot path.
//!
//! Screen capture itself (driver + WGC/DXGI) cannot be measured without a real
//! desktop session, so this concentrates on the CPU work the pipeline does per
//! frame — the part that scales with resolution and is worth optimising.
//!
//! Run with a release build, otherwise the numbers are meaningless:
//!
//! ```text
//! cargo run --release --example perf
//! cargo run --release --example perf -- 3840 2160 60
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use screenrec::capture::pipeline::{FramePipeline, SinkFactory};
use screenrec::capture::SessionShared;
use screenrec::encode::util;
use screenrec::encode::{FrameData, FrameSink};
use screenrec::region::Region;

/// A sink that accepts frames and does nothing, isolating pipeline cost from
/// encoder cost.
struct NullSink {
    bytes: u64,
}

impl FrameSink for NullSink {
    fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()> {
        // Touch the data so the call cannot be optimised away.
        self.bytes = self.bytes.wrapping_add(frame.pixels.len() as u64);
        std::hint::black_box(frame.pixels);
        Ok(())
    }

    fn finish(self: Box<Self>) -> anyhow::Result<PathBuf> {
        std::hint::black_box(self.bytes);
        Ok(PathBuf::from("null"))
    }

    fn describe(&self) -> String {
        "空编码器".to_string()
    }
}

fn null_factory(target: Option<(u32, u32)>) -> SinkFactory {
    Box::new(move |width: u32, height: u32| {
        let (out_width, out_height) = target.unwrap_or((width, height));
        Ok((Box::new(NullSink { bytes: 0 }) as Box<dyn FrameSink>, out_width, out_height))
    })
}

fn bench<T>(label: &str, iterations: u32, mut body: impl FnMut() -> T) -> Duration {
    // One warm-up pass so lazily built tables and allocations are not counted.
    body();
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(body());
    }
    let elapsed = start.elapsed();
    let per_call = elapsed / iterations;
    println!(
        "  {label:<44} {:>9.3} ms/次   ({:.1} ms 总计 / {iterations} 次)",
        per_call.as_secs_f64() * 1_000.0,
        elapsed.as_secs_f64() * 1_000.0
    );
    per_call
}

fn main() {
    let mut args = std::env::args().skip(1);
    let width: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(3840);
    let height: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(2160);
    let fps: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(60);

    let pixels = (width as usize) * (height as usize);
    println!("分辨率 {width}x{height}（{:.1} M 像素/帧），目标 {fps} fps\n", pixels as f64 / 1e6);

    // ---- resolution-independent per-frame work -------------------------
    println!("单帧 CPU 处理：");
    let bgra = vec![128u8; pixels * 4];
    let rgba16 = vec![0u8; pixels * 8];

    let mut out_bgra = Vec::new();
    let hdr = bench("HDR: Rgba16F → BGRA (scRGB→sRGB)", 15, || {
        util::hdr_fp16_to_bgra(&rgba16, &mut out_bgra);
    });
    let hdr_rows = bench("HDR: 同上，按行并行", 15, || {
        util::hdr_fp16_to_bgra_rows(&rgba16, width, &mut out_bgra);
    });

    let mut out_crop = Vec::new();
    let crop = bench("区域裁剪 → 800x600", 30, || {
        util::crop_bgra(&bgra, width, height, Region::new(100, 100, 800, 600), &mut out_crop)
            .unwrap();
    });

    let mut out_flip = Vec::new();
    let flip = bench("MF 行序翻转（自下而上）", 30, || {
        util::flip_vertical(&bgra, width, height, &mut out_flip);
    });

    let mut scratch = Vec::new();
    let mut out_scale = Vec::new();
    let scale_1080 = if height > 1080 {
        bench("缩放 → 1920x1080（Triangle）", 15, || {
            util::scale_bgra(&bgra, width, height, 1920, 1080, &mut scratch, &mut out_scale).unwrap();
        })
    } else {
        Duration::ZERO
    };
    let scale_720 = if height > 720 {
        bench("缩放 → 1280x720（Triangle）", 15, || {
            util::scale_bgra(&bgra, width, height, 1280, 720, &mut scratch, &mut out_scale).unwrap();
        })
    } else {
        Duration::ZERO
    };

    // ---- whole pipeline -------------------------------------------------
    // Frames arrive at the source refresh rate while the recording targets a
    // possibly lower fps: that is the case the "drop frames that are not due"
    // path exists for. Only the CPU time of the push itself is accumulated,
    // not the simulated interval between frames.
    println!("\n整条管线（模拟 60 Hz 采集源，编码器为空实现，只统计 CPU 时间）：");

    let run_paced = |label: &str,
                     target_fps: u32,
                     crop: Option<Region>,
                     target: Option<(u32, u32)>,
                     source_frames: u32| {
        let shared = Arc::new(SessionShared::new());
        let mut pipeline =
            FramePipeline::new(null_factory(target), Arc::clone(&shared), target_fps, None, true);
        pipeline.set_crop(crop);
        let interval = Duration::from_micros(1_000_000 / 60);
        let mut cpu = Duration::ZERO;
        for _ in 0..source_frames {
            let start = Instant::now();
            pipeline.push_bgra(&bgra, width, height).unwrap();
            cpu += start.elapsed();
            std::thread::sleep(interval);
        }
        let emitted = pipeline.emitted_frames();
        let per_source = cpu / source_frames;
        println!(
            "  {label:<34} {:>8.3} ms/源帧   CPU 合计 {:>6.1} ms/{:.1}s  输出 {} 帧",
            per_source.as_secs_f64() * 1_000.0,
            cpu.as_secs_f64() * 1_000.0,
            source_frames as f64 / 60.0,
            emitted
        );
        per_source
    };

    let source_frames = 60;
    run_paced("4K 全屏 → 30 fps", 30, None, None, source_frames);
    run_paced("4K 全屏 → 60 fps", 60, None, None, source_frames);
    run_paced("4K → 1080p，30 fps", 30, None, Some((1920, 1080)), source_frames);
    run_paced(
        "4K 区域 800x600 → 30 fps",
        30,
        Some(Region::new(100, 100, 800, 600)),
        None,
        source_frames,
    );

    println!("\n汇总（单帧 CPU）：");
    println!(
        "  HDR 转换            {:>8.3} ms  (单线程 {:.3} ms)",
        hdr_rows.as_secs_f64() * 1_000.0,
        hdr.as_secs_f64() * 1_000.0
    );
    println!(
        "  裁剪 / 翻转 / 缩放   {:>8.3} / {:.3} / {:.3} ms",
        crop.as_secs_f64() * 1_000.0,
        flip.as_secs_f64() * 1_000.0,
        scale_1080.max(scale_720).as_secs_f64() * 1_000.0
    );

    // ---- what the numbers mean for a frame budget ----------------------
    let budget = Duration::from_secs_f64(1.0 / fps as f64);
    for (label, cost) in [("HDR 转换", hdr_rows), ("缩放到 1080p", scale_1080)] {
        if cost.is_zero() {
            continue;
        }
        let share = cost.as_secs_f64() / budget.as_secs_f64() * 100.0;
        println!(
            "  {label} 占 {fps} fps 单帧预算的 {share:.0}%{}",
            if share > 100.0 { "  <- 单线程无法达标" } else { "" }
        );
    }
}
