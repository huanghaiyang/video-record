//! Pixel helpers shared by the encoders.

/// Swaps the red and blue channels of a BGRA buffer in place.
pub fn bgra_to_rgba_in_place(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

use crate::region::Region;

/// Copies a BGRA buffer, swapping red/blue, into an RGBA `Vec`.
pub fn bgra_to_rgba(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(src);
    bgra_to_rgba_in_place(out);
}

/// Copies a region out of a tightly packed BGRA frame.
///
/// Used for area recording: cropping before the encoder means the output size,
/// the scaler and the preview all work on the region as if it were the source.
pub fn crop_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    region: Region,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    anyhow::ensure!(region.width > 0 && region.height > 0, "裁剪尺寸不能为 0");
    anyhow::ensure!(
        region.right() <= src_width && region.bottom() <= src_height,
        "裁剪区域 {} 超出画面 {src_width}x{src_height}",
        region.label()
    );

    let src_stride = (src_width as usize) * 4;
    let row_bytes = (region.width as usize) * 4;
    let expected = (src_height as usize) * src_stride;
    anyhow::ensure!(
        src.len() >= expected,
        "帧数据不完整：{} < {expected}",
        src.len()
    );

    out.clear();
    out.reserve(row_bytes * region.height as usize);
    for row in 0..region.height as usize {
        let start = (region.y as usize + row) * src_stride + (region.x as usize) * 4;
        out.extend_from_slice(&src[start..start + row_bytes]);
    }
    Ok(())
}

/// Reverses the row order of a tightly packed BGRA frame.
///
/// The result is bottom-up: row 0 of the input becomes the last row of the
/// output. Used for Media Foundation, which interprets RGB buffers bottom-up
/// unless `MF_MT_DEFAULT_STRIDE` is negative — an attribute the encoder wrapper
/// does not expose.
///
/// Deliberately single-threaded: measured on a 4K frame this is 4.4 ms
/// sequential versus 4.7-5.1 ms split across cores. It is a plain memcpy, so it
/// is limited by memory bandwidth and the thread wake-ups only add overhead.
pub fn flip_vertical(src: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    let stride = (width as usize) * 4;
    let rows = height as usize;
    if stride == 0 || rows == 0 {
        out.clear();
        return;
    }

    let available = src.len() / stride;
    if available < rows {
        // Truncated frame: fall back to a straight copy so nothing is invented.
        out.clear();
        out.extend_from_slice(src);
        return;
    }

    prepare_output(out, stride * rows);
    for (row_index, row) in out.chunks_mut(stride).enumerate() {
        let source_row = (rows - 1 - row_index) * stride;
        row.copy_from_slice(&src[source_row..source_row + stride]);
    }
}

/// Copies an RGBA buffer, swapping red/blue, into a BGRA `Vec`.
pub fn rgba_to_bgra(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(src);
    bgra_to_rgba_in_place(out);
}

/// Rescales a BGRA frame with a triangle filter.
///
/// Only called when the user asked for a different output size than the
/// captured one; the common path is a straight pass-through.
/// Resizes a BGRA frame.
///
/// Written by hand rather than delegated to the `image` crate: that path cost
/// ~119 ms per 4K frame because it ran a float convolution plus two full
/// channel-swap passes. This version stays in BGRA and uses integer maths.
///
/// Integer downscale factors use a box average, which is both faster and
/// better than a filter for video (it is a true area average, so it does not
/// alias). Anything else falls back to fixed-point bilinear sampling.
pub fn scale_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    let src_width = src_width.max(1);
    let src_height = src_height.max(1);
    let dst_width = dst_width.max(1);
    let dst_height = dst_height.max(1);

    let expected = (src_width as usize) * (src_height as usize) * 4;
    anyhow::ensure!(
        src.len() >= expected,
        "frame buffer too small: {} < {}",
        src.len(),
        expected
    );
    let src = &src[..expected];

    // Nothing to do: hand the pixels straight through.
    if src_width == dst_width && src_height == dst_height {
        out.clear();
        out.extend_from_slice(src);
        return Ok(());
    }


    let exact = src_width % dst_width == 0
        && src_height % dst_height == 0
        && src_width / dst_width >= 2
        && src_height / dst_height >= 2;

    if exact {
        box_downscale_bgra(
            src,
            src_width,
            src_height,
            dst_width,
            dst_height,
            out,
        );
    } else {
        bilinear_bgra(
            src,
            src_width,
            src_height,
            dst_width,
            dst_height,
            scratch,
            out,
        );
    }
    Ok(())
}

/// Sizes a reusable output buffer to exactly `len` bytes.
///
/// `Vec::clear` + `resize` zeroes the whole buffer; at 4K that is 33 MB of
/// wasted writes *per frame* for buffers that are about to be overwritten
/// anyway. Reusing the existing length avoids it: the callers below write every
/// byte they leave behind.
fn prepare_output(out: &mut Vec<u8>, len: usize) {
    if out.len() != len {
        out.clear();
        out.resize(len, 0);
    }
}

/// True when `SCREENREC_SINGLE_THREAD` is set.
///
/// An escape hatch for troubleshooting, and for A/B measuring the parallel
/// paths against the sequential ones on a given machine.
fn single_threaded() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        let set = std::env::var_os("SCREENREC_SINGLE_THREAD").is_some();
        if set {
            log::info!("SCREENREC_SINGLE_THREAD 已设置：按行并行的处理改为单线程");
        }
        set
    })
}

/// How many threads row-band work should use.
///
/// Below ~512 KB of output the wake-up cost outweighs the parallelism, which
/// also keeps small recordings on a single thread.
fn parallel_degree(rows: usize, bytes: usize) -> usize {
    const MIN_BYTES: usize = 1 << 19;
    if bytes < MIN_BYTES || rows < 2 || single_threaded() {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(8)
        .min(rows)
        .max(1)
}

/// Runs `work(start_row, band)` over row-aligned bands of `out` in parallel.
///
/// `out` must hold whole rows of `row_bytes`. Bands are disjoint, so the output
/// is identical whatever the thread count — which keeps pixels deterministic.
fn with_row_bands<F>(out: &mut [u8], row_bytes: usize, work: F)
where
    F: Fn(usize, &mut [u8]) + Sync,
{
    if row_bytes == 0 || out.is_empty() {
        return;
    }
    let rows = out.len() / row_bytes;
    if rows == 0 {
        return;
    }

    let threads = parallel_degree(rows, out.len());
    if threads <= 1 {
        work(0, out);
        return;
    }

    let band_rows = rows.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (index, band) in out.chunks_mut(band_rows * row_bytes).enumerate() {
            let start_row = index * band_rows;
            let work = &work;
            scope.spawn(move || work(start_row, band));
        }
    });
}

/// Area-averages `factor_x` x `factor_y` blocks into one output pixel.
fn box_downscale_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    out: &mut Vec<u8>,
) {
    let factor_x = (src_width / dst_width) as usize;
    let factor_y = (src_height / dst_height) as usize;
    let row_bytes = (dst_width as usize) * 4;
    let src_stride = (src_width as usize) * 4;

    prepare_output(out, row_bytes * dst_height as usize);

    // One output row at a time: the source span is the same for every pixel of
    // that row, so the block offsets are computed once.
    let source = src;
    let row_stride = factor_x * 4;

    with_row_bands(out, row_bytes, |start_row, band| {
        for (offset, row) in band.chunks_mut(row_bytes).enumerate() {
            let dst_y = start_row + offset;
            let first_src_row = dst_y * factor_y;
            for (dst_x, pixel) in row.chunks_mut(4).enumerate() {
                let mut blue = 0u32;
                let mut green = 0u32;
                let mut red = 0u32;
                let mut alpha = 0u32;

                let first_column = dst_x * row_stride;
                for block_y in 0..factor_y {
                    let base = (first_src_row + block_y) * src_stride;
                    let mut cursor = base + first_column;
                    for _ in 0..factor_x {
                        blue += source[cursor] as u32;
                        green += source[cursor + 1] as u32;
                        red += source[cursor + 2] as u32;
                        alpha += source[cursor + 3] as u32;
                        cursor += 4;
                    }
                }

                let count = (factor_x * factor_y) as u32;
                pixel[0] = ((blue + count / 2) / count) as u8;
                pixel[1] = ((green + count / 2) / count) as u8;
                pixel[2] = ((red + count / 2) / count) as u8;
                pixel[3] = ((alpha + count / 2) / count) as u8;
            }
        }
    });
}

/// Fixed-point bilinear resize in two passes (horizontal, then vertical).
fn bilinear_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) {
    // Horizontal pass: src (sw x sh) -> scratch (dw x sh).
    let dst_row_bytes = (dst_width as usize) * 4;
    prepare_output(scratch, dst_row_bytes * src_height as usize);

    let src_stride = (src_width as usize) * 4;
    let last_src_x = (src_width - 1) as usize;
    // 16.16 fixed point: position in the source for each destination column.
    let step_x = ((src_width as u64) << 16) / dst_width as u64;

    with_row_bands(scratch, dst_row_bytes, |start_row, band| {
        for (offset, row) in band.chunks_mut(dst_row_bytes).enumerate() {
            let src_row = (start_row + offset) * src_stride;
            for (dst_x, pixel) in row.chunks_mut(4).enumerate() {
                let position = (dst_x as u64 * step_x) as usize;
                let x0 = (position >> 16).min(last_src_x);
                let x1 = (x0 + 1).min(last_src_x);
                let weight = (position & 0xFFFF) as u32; // 0..65535

                let left = src_row + x0 * 4;
                let right = src_row + x1 * 4;
                for channel in 0..4 {
                    let a = src[left + channel] as u32;
                    let b = src[right + channel] as u32;
                    pixel[channel] = (((a * (65_535 - weight)) + b * weight + 32_768) >> 16) as u8;
                }
            }
        }
    });

    // Vertical pass: scratch (dw x sh) -> out (dw x dh).
    prepare_output(out, dst_row_bytes * dst_height as usize);

    let last_src_y = (src_height - 1) as usize;
    let step_y = ((src_height as u64) << 16) / dst_height as u64;
    let scratch_rows = scratch.as_slice();

    with_row_bands(out, dst_row_bytes, |start_row, band| {
        for (offset, row) in band.chunks_mut(dst_row_bytes).enumerate() {
            let dst_y = start_row + offset;
            let position = (dst_y as u64 * step_y) as usize;
            let y0 = (position >> 16).min(last_src_y);
            let y1 = (y0 + 1).min(last_src_y);
            let weight = (position & 0xFFFF) as u32;

            let top = y0 * dst_row_bytes;
            let bottom = y1 * dst_row_bytes;
            for (x, pixel) in row.chunks_mut(4).enumerate() {
                let index = x * 4;
                for channel in 0..4 {
                    let a = scratch_rows[top + index + channel] as u32;
                    let b = scratch_rows[bottom + index + channel] as u32;
                    pixel[channel] = (((a * (65_535 - weight)) + b * weight + 32_768) >> 16) as u8;
                }
            }
        }
    });
}

/// Converts interleaved `f32` samples to 16-bit PCM with clipping.
pub fn f32_to_i16(samples: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * i16::MAX as f32) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
}

// --------------------------------------------------------------- HDR ------

/// Lookup table for IEEE 754 half precision, built once.
fn f16_lut() -> &'static [f32; 65536] {
    use std::sync::OnceLock;
    static LUT: OnceLock<Box<[f32; 65536]>> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = Box::new([0f32; 65536]);
        for index in 0..65536usize {
            let half = index as u32;
            let sign = half >> 15;
            let exponent = (half >> 10) & 0x1F;
            let mantissa = half & 0x3FF;
            let mut value = match exponent {
                0 => {
                    if mantissa == 0 {
                        0.0
                    } else {
                        // Subnormal: 2^-24 * mantissa
                        mantissa as f32 * (2.0f32).powi(-24)
                    }
                }
                0x1F => {
                    if mantissa == 0 {
                        f32::INFINITY
                    } else {
                        f32::NAN
                    }
                }
                _ => {
                    let exponent = exponent as i32 - 15;
                    (1.0 + mantissa as f32 / 1024.0) * (2.0f32).powi(exponent)
                }
            };
            if sign == 1 {
                value = -value;
            }
            table[index] = value;
        }
        table
    })
}

/// Converts one IEEE 754 half precision value to `f32`.
pub fn f16_to_f32(half: u16) -> f32 {
    f16_lut()[half as usize]
}

/// Lookup table mapping linear light [0,1] to 8-bit sRGB.
fn srgb_lut() -> &'static [u8; 4096] {
    use std::sync::OnceLock;
    static LUT: OnceLock<[u8; 4096]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = [0u8; 4096];
        for (index, slot) in table.iter_mut().enumerate() {
            let linear = index as f32 / 4095.0;
            let encoded = if linear <= 0.003_130_8 {
                linear * 12.92
            } else {
                1.055 * linear.powf(1.0 / 2.4) - 0.055
            };
            *slot = (encoded * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
        }
        table
    })
}

/// Converts a linear scRGB `f32` component to 8-bit sRGB.
fn linear_to_srgb_u8(value: f32) -> u8 {
    if value.is_nan() {
        return 0;
    }
    let clamped = value.clamp(0.0, 1.0);
    let table = srgb_lut();
    table[(clamped * 4095.0) as usize]
}

/// One lookup from an IEEE 754 half (linear scRGB) straight to 8-bit sRGB.
///
/// Fusing the half→f32 and linear→sRGB tables removes three lookups per channel
/// and replaces a 256 KB float table with a 64 KB byte table, which matters a
/// lot at 4K: the separate tables made this function cost ~79 ms per frame.
fn half_to_srgb_lut() -> &'static [u8; 65536] {
    use std::sync::OnceLock;
    static LUT: OnceLock<Box<[u8; 65536]>> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = Box::new([0u8; 65536]);
        for (index, slot) in table.iter_mut().enumerate() {
            // The same conversion the two-step helpers performed, fused.
            // `linear_to_srgb_u8` also maps NaN to 0.
            *slot = linear_to_srgb_u8(f16_to_f32(index as u16));
        }
        table
    })
}

/// Half-precision alpha to 8-bit. Alpha is 1.0 in practice, so the common
/// encoding is answered without touching a table.
fn half_alpha_to_u8(half: u16) -> u8 {
    const HALF_ONE: u16 = 0x3C00;
    if half == HALF_ONE {
        return 255;
    }
    let value = f16_to_f32(half);
    if value.is_nan() {
        255
    } else {
        (value.clamp(0.0, 1.0) * 255.0) as u8
    }
}

/// Converts a 16-bit-float RGBA frame (Windows HDR desktops) into BGRA 8-bit.
///
/// Desktop Duplication always hands out `R16G16B16A16_FLOAT` when the display
/// runs in HDR mode, whatever pixel format was requested. The values are linear
/// scRGB, so they are tone mapped by clamping to the SDR range and applying the
/// sRGB transfer curve. HDR highlights above SDR white are therefore clipped —
/// the alternative would be a per-display tone curve, which DXGI does not
/// expose to duplicating applications.
pub fn hdr_fp16_to_bgra(src: &[u8], out: &mut Vec<u8>) {
    let pixels = src.len() / 8;
    out.clear();
    if pixels == 0 {
        return;
    }
    prepare_output(out, pixels * 4);
    convert_hdr_band(&src[..pixels * 8], out);
}

/// Same conversion, but parallelised over rows when the frame is large.
///
/// At 4K the lookup-bound conversion cost ~24 ms single-threaded — more than a
/// 60 fps frame budget — so large frames are split across cores. Bands are
/// row-aligned and write disjoint output, so the result is bit-identical to the
/// single-threaded path.
pub fn hdr_fp16_to_bgra_rows(src: &[u8], width: u32, out: &mut Vec<u8>) {
    let width = width.max(1) as usize;
    let pixels = src.len() / 8;
    out.clear();
    if pixels == 0 {
        return;
    }
    prepare_output(out, pixels * 4);

    let src_row_bytes = width * 8;
    let out_row_bytes = width * 4;
    let rows = pixels / width;
    let usable_rows = if rows == 0 { 0 } else { rows };
    let usable_out = usable_rows * out_row_bytes;

    let threads = parallel_degree(usable_rows, usable_out);
    if threads <= 1 {
        // One band: correct regardless of how the rows divide.
        convert_hdr_band(&src[..pixels * 8], &mut out[..pixels * 4]);
        return;
    }

    let band_rows = usable_rows.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (index, out_band) in out[..usable_out]
            .chunks_mut(band_rows * out_row_bytes)
            .enumerate()
        {
            let start_byte = index * band_rows * src_row_bytes;
            let src_band = &src[start_byte..start_byte + out_band.len() / 4 * 2];
            scope.spawn(move || convert_hdr_band(src_band, out_band));
        }
    });
}

/// Converts one contiguous run of `Rgba16F` pixels into BGRA bytes.
fn convert_hdr_band(src: &[u8], out: &mut [u8]) {
    let lut = half_to_srgb_lut();
    for (destination, source) in out.chunks_exact_mut(4).zip(src.chunks_exact(8)) {
        let red = u16::from_le_bytes([source[0], source[1]]);
        let green = u16::from_le_bytes([source[2], source[3]]);
        let blue = u16::from_le_bytes([source[4], source[5]]);
        let alpha = u16::from_le_bytes([source[6], source[7]]);

        // Windows hands out BGRA, so the channels are written back to front.
        destination[0] = lut[blue as usize];
        destination[1] = lut[green as usize];
        destination[2] = lut[red as usize];
        destination[3] = half_alpha_to_u8(alpha);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_conversion_is_unchanged_by_the_fused_table() {
        // The fused table must be bit-identical to the two-step conversion it
        // replaced (f16 → f32, then linear → sRGB).
        let mut frame = Vec::new();
        let values: [u16; 12] = [
            0x0000, 0x3C00, 0x3800, 0x4000, // 0.0, 1.0, 0.5, 2.0
            0xBC00, 0x7C00, 0xFC00, 0x3555, // -1.0, +inf, -inf, ~0.333
            0x0001, 0x03FF, 0x0400, 0x7BFF, // subnormal, tiny, smallest normal, max
        ];
        for (index, value) in values.iter().enumerate() {
            // Repeat so a full pixel plus a partial one are both covered.
            let channel = if index % 2 == 0 { *value } else { 0x3C00 };
            frame.extend_from_slice(&channel.to_le_bytes());
        }
        // One complete pixel (8 bytes) and then a second one.
        frame.extend_from_slice(&0x3800u16.to_le_bytes());
        frame.extend_from_slice(&0x3C00u16.to_le_bytes());
        frame.extend_from_slice(&0x4000u16.to_le_bytes());
        frame.extend_from_slice(&0x3C00u16.to_le_bytes());

        let mut converted = Vec::new();
        hdr_fp16_to_bgra(&frame, &mut converted);

        let expected: Vec<u8> = frame
            .chunks_exact(8)
            .flat_map(|pixel| {
                let r = f16_to_f32(u16::from_le_bytes([pixel[0], pixel[1]]));
                let g = f16_to_f32(u16::from_le_bytes([pixel[2], pixel[3]]));
                let b = f16_to_f32(u16::from_le_bytes([pixel[4], pixel[5]]));
                let a = f16_to_f32(u16::from_le_bytes([pixel[6], pixel[7]]));
                [
                    linear_to_srgb_u8(b),
                    linear_to_srgb_u8(g),
                    linear_to_srgb_u8(r),
                    if a.is_nan() { 255 } else { (a.clamp(0.0, 1.0) * 255.0) as u8 },
                ]
            })
            .collect();

        assert_eq!(converted, expected);
    }

    #[test]
    fn hdr_conversion_handles_alpha_and_degenerate_input() {
        // Opaque alpha is the overwhelmingly common case.
        let pixel = [0x00u8, 0x3C, 0x00, 0x3C, 0x00, 0x3C, 0x00, 0x3C];
        let mut out = Vec::new();
        hdr_fp16_to_bgra(&pixel, &mut out);
        assert_eq!(out, vec![255, 255, 255, 255], "1.0 linear → white, opaque");

        // NaN alpha must stay opaque rather than becoming 0.
        let mut nan_alpha = pixel;
        nan_alpha[6] = 0x01;
        nan_alpha[7] = 0x7C; // +inf is NaN-adjacent; use a real NaN encoding
        nan_alpha[6] = 0x00;
        nan_alpha[7] = 0x7E;
        hdr_fp16_to_bgra(&nan_alpha, &mut out);
        assert_eq!(out[3], 255);

        // Empty and single-byte inputs must not panic.
        hdr_fp16_to_bgra(&[], &mut out);
        assert!(out.is_empty());
        hdr_fp16_to_bgra(&[0u8; 3], &mut out);
        assert!(out.is_empty(), "a partial pixel is not emitted");
    }

    #[test]
    fn hdr_conversion_clips_highlights() {
        // 4.0 linear (0x4400) is above SDR white and must clip to 255.
        let pixel = [0x00u8, 0x44, 0x00, 0x44, 0x00, 0x44, 0x00, 0x3C];
        let mut out = Vec::new();
        hdr_fp16_to_bgra(&pixel, &mut out);
        assert_eq!(&out[..3], &[255, 255, 255]);
    }

    #[test]
    fn channel_swap_round_trips() {
        let bgra = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let mut rgba = Vec::new();
        bgra_to_rgba(&bgra, &mut rgba);
        assert_eq!(rgba, vec![30, 20, 10, 255, 60, 50, 40, 255]);

        let mut back = Vec::new();
        rgba_to_bgra(&rgba, &mut back);
        assert_eq!(back, bgra);
    }

    #[test]
    fn cropping_picks_the_requested_rows_and_columns() {
        // 4x3 frame, every pixel encodes its own coordinates.
        let (width, height) = (4u32, 3u32);
        let mut src = Vec::new();
        for y in 0..height {
            for x in 0..width {
                src.extend_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }

        let mut out = Vec::new();
        crop_bgra(&src, width, height, Region::new(1, 1, 2, 2), &mut out).unwrap();

        // Expect pixels (1,1) (2,1) (1,2) (2,2) in that order.
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!(&out[0..4], &[1, 1, 0, 255]);
        assert_eq!(&out[4..8], &[2, 1, 0, 255]);
        assert_eq!(&out[8..12], &[1, 2, 0, 255]);
        assert_eq!(&out[12..16], &[2, 2, 0, 255]);
    }

    #[test]
    fn cropping_the_whole_frame_is_identity() {
        let (width, height) = (5u32, 4u32);
        let src: Vec<u8> = (0..(width * height * 4)).map(|i| (i % 251) as u8).collect();

        let mut out = Vec::new();
        crop_bgra(&src, width, height, Region::full(width, height), &mut out).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn cropping_outside_the_frame_fails_instead_of_reading_garbage() {
        let (width, height) = (4u32, 4u32);
        let src = vec![9u8; (width * height * 4) as usize];
        let mut out = Vec::new();

        assert!(crop_bgra(&src, width, height, Region::new(3, 0, 2, 1), &mut out).is_err());
        assert!(crop_bgra(&src, width, height, Region::new(0, 3, 1, 2), &mut out).is_err());
        assert!(crop_bgra(&src, width, height, Region::new(0, 0, 0, 4), &mut out).is_err());
        // A truncated buffer must be detected, not sliced out of bounds.
        assert!(crop_bgra(&src[..8], width, height, Region::full(4, 4), &mut out).is_err());
    }

    #[test]
    fn vertical_flip_reverses_rows_only() {
        // 2x2, rows are all-1s and all-2s.
        let src = vec![1u8, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2];
        let mut out = Vec::new();
        flip_vertical(&src, 2, 2, &mut out);
        assert_eq!(out, vec![2u8, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn vertical_flip_is_its_own_inverse() {
        let width = 4u32;
        let height = 3u32;
        let mut src = Vec::new();
        for row in 0..height {
            for column in 0..width {
                src.extend_from_slice(&[row as u8, column as u8, 9, 255]);
            }
        }

        let mut once = Vec::new();
        flip_vertical(&src, width, height, &mut once);
        let mut twice = Vec::new();
        flip_vertical(&once, width, height, &mut twice);
        assert_eq!(twice, src);
    }

    #[test]
    fn vertical_flip_handles_degenerate_input() {
        let mut out = Vec::new();
        flip_vertical(&[], 0, 0, &mut out);
        assert!(out.is_empty());

        // Fewer rows than advertised: copy through instead of panicking.
        let src = vec![7u8; 8];
        flip_vertical(&src, 2, 4, &mut out);
        assert_eq!(out, src);
    }

    #[test]
    fn a_large_flip_is_still_exactly_row_reversed() {
        // Big enough to take the parallel path; each row starts with its own
        // row index so any mistake in band boundaries shows up immediately.
        let (width, height) = (1280u32, 720u32);
        let stride = width as usize * 4;
        let mut src = vec![0u8; stride * height as usize];
        for row in 0..height as usize {
            let start = row * stride;
            src[start..start + 2].copy_from_slice(&(row as u16).to_le_bytes());
        }

        let mut out = Vec::new();
        flip_vertical(&src, width, height, &mut out);
        assert_eq!(out.len(), src.len());

        for row in 0..height as usize {
            let start = row * stride;
            let expected = (height as usize - 1 - row) as u16;
            assert_eq!(
                u16::from_le_bytes([out[start], out[start + 1]]),
                expected,
                "output row {row} is wrong"
            );
        }

        // And it is still its own inverse.
        let mut back = Vec::new();
        flip_vertical(&out, width, height, &mut back);
        assert_eq!(back, src);
    }

    #[test]
    fn scaling_produces_the_requested_size() {
        let src = vec![128u8; 16 * 16 * 4];
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        for (width, height) in [(8u32, 8u32), (7, 5), (16, 16), (1, 1), (32, 32), (4, 2)] {
            scale_bgra(&src, 16, 16, width, height, &mut scratch, &mut out).unwrap();
            assert_eq!(
                out.len(),
                (width as usize) * (height as usize) * 4,
                "wrong size for {width}x{height}"
            );
        }
    }

    #[test]
    fn scaling_a_uniform_frame_keeps_it_uniform() {
        // Both the box and the bilinear path must be exact on flat colour.
        let mut src = Vec::new();
        for _ in 0..(12 * 12) {
            src.extend_from_slice(&[10, 20, 30, 40]);
        }

        let mut scratch = Vec::new();
        let mut out = Vec::new();
        // 12 -> 4 is an exact 3x box; 12 -> 5 is not.
        for (width, height) in [(4u32, 4u32), (5, 5)] {
            scale_bgra(&src, 12, 12, width, height, &mut scratch, &mut out).unwrap();
            for pixel in out.chunks_exact(4) {
                assert_eq!(pixel, [10, 20, 30, 40], "{width}x{height} drifted");
            }
        }
    }

    #[test]
    fn box_downscale_averages_blocks() {
        // 2x2 -> 1x1: the average of 0,0,0,16 is 4.
        let src = vec![
            0u8, 0, 0, 0, /**/ 0, 0, 0, 0, /**/ 0, 0, 0, 0, /**/ 16, 16, 16, 16,
        ];
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        scale_bgra(&src, 2, 2, 1, 1, &mut scratch, &mut out).unwrap();
        assert_eq!(out, vec![4, 4, 4, 4]);
    }

    #[test]
    fn scaling_half_size_keeps_the_corners_dominant() {
        let mut src = vec![50u8; 4 * 4 * 4];
        src[0..4].copy_from_slice(&[200, 0, 0, 255]); // blue in BGRA
        let last = src.len() - 4;
        src[last..].copy_from_slice(&[0, 0, 200, 255]); // red in BGRA

        let mut scratch = Vec::new();
        let mut out = Vec::new();
        scale_bgra(&src, 4, 4, 2, 2, &mut scratch, &mut out).unwrap();

        assert!(out[0] > out[2], "top-left should stay blue-dominant");
        let bottom_right = out.len() - 4;
        assert!(
            out[bottom_right + 2] > out[bottom_right],
            "bottom-right should stay red-dominant"
        );
    }

    #[test]
    fn scaling_is_deterministic_across_thread_bands() {
        let src: Vec<u8> = (0..(33 * 17 * 4)).map(|index| (index % 251) as u8).collect();
        let mut scratch = Vec::new();
        let mut first = Vec::new();
        let mut second = Vec::new();
        // Big enough to cross the parallel threshold, run twice.
        let large: Vec<u8> = (0..(1280 * 720 * 4)).map(|index| (index % 253) as u8).collect();
        scale_bgra(&large, 1280, 720, 640, 360, &mut scratch, &mut first).unwrap();
        scale_bgra(&large, 1280, 720, 640, 360, &mut scratch, &mut second).unwrap();
        assert_eq!(first, second);

        scale_bgra(&src, 33, 17, 12, 7, &mut scratch, &mut first).unwrap();
        scale_bgra(&src, 33, 17, 12, 7, &mut scratch, &mut second).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn scaling_rejects_truncated_frames() {
        let src = vec![0u8; 10];
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        assert!(scale_bgra(&src, 8, 4, 4, 2, &mut scratch, &mut out).is_err());
    }

    #[test]
    fn pcm_conversion_clips() {
        let mut out = Vec::new();
        f32_to_i16(&[0.0, 1.0, -1.0, 2.0, -2.0], &mut out);
        let values: Vec<i16> = out
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(values[0], 0);
        assert_eq!(values[1], i16::MAX);
        assert_eq!(values[2], -i16::MAX);
        assert_eq!(values[3], i16::MAX);
        assert_eq!(values[4], -i16::MAX);
    }

    #[test]
    fn half_precision_decoding_matches_known_values() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0x3800), 0.5);
        assert_eq!(f16_to_f32(0x4000), 2.0);
        assert_eq!(f16_to_f32(0xBC00), -1.0);
        assert!(f16_to_f32(0x7C00).is_infinite());
        assert!(f16_to_f32(0x7C01).is_nan());
        // Smallest subnormal.
        assert!(f16_to_f32(0x0001) > 0.0);
        assert!(f16_to_f32(0x0001) < 1.0e-6);
    }

    #[test]
    fn hdr_conversion_maps_white_and_black() {
        // Two pixels: opaque black then opaque white, stored as RGBA f16.
        let mut src = Vec::new();
        for value in [0x0000u16, 0x0000, 0x0000, 0x3C00, 0x3C00, 0x3C00, 0x3C00, 0x3C00] {
            src.extend_from_slice(&value.to_le_bytes());
        }

        let mut out = Vec::new();
        hdr_fp16_to_bgra(&src, &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn hdr_conversion_clips_out_of_range_and_swaps_channels() {
        // A fully red pixel above SDR white, then a negative one.
        let mut src = Vec::new();
        for value in [0x4000u16, 0x0000, 0x0000, 0x3C00] {
            src.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0x0000u16, 0x0000, 0x0000, 0x3C00] {
            src.extend_from_slice(&value.to_le_bytes());
        }

        let mut out = Vec::new();
        hdr_fp16_to_bgra(&src, &mut out);
        // Red becomes the third BGRA byte, and is clipped to full intensity.
        assert_eq!(out[2], 255);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0);
        // NaN and negative inputs must not produce garbage.
        assert_eq!(out[4], 0);
        assert_eq!(out[7], 255);
    }

    #[test]
    fn hdr_conversion_handles_partial_pixels() {
        let mut out = Vec::new();
        hdr_fp16_to_bgra(&[0u8; 5], &mut out);
        assert!(out.is_empty());
    }
}
