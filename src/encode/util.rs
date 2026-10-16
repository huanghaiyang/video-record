//! Pixel helpers shared by the encoders.

/// Swaps the red and blue channels of a BGRA buffer in place.
pub fn bgra_to_rgba_in_place(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// Copies a BGRA buffer, swapping red/blue, into an RGBA `Vec`.
pub fn bgra_to_rgba(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(src);
    bgra_to_rgba_in_place(out);
}

/// Reverses the row order of a tightly packed BGRA frame.
///
/// The result is bottom-up: row 0 of the input becomes the last row of the
/// output. Used for Media Foundation, which interprets RGB buffers bottom-up
/// unless `MF_MT_DEFAULT_STRIDE` is negative — an attribute the encoder wrapper
/// does not expose.
pub fn flip_vertical(src: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    let stride = (width as usize) * 4;
    let rows = height as usize;
    out.clear();
    if stride == 0 || rows == 0 {
        return;
    }

    let available = src.len() / stride;
    if available < rows {
        // Truncated frame: fall back to a straight copy so nothing is invented.
        out.extend_from_slice(src);
        return;
    }

    out.reserve(stride * rows);
    for row in 0..rows {
        let start = (rows - 1 - row) * stride;
        out.extend_from_slice(&src[start..start + stride]);
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
pub fn scale_bgra(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let expected = (src_width as usize) * (src_height as usize) * 4;
    anyhow::ensure!(
        src.len() >= expected,
        "frame buffer too small: {} < {}",
        src.len(),
        expected
    );

    bgra_to_rgba(&src[..expected], scratch);
    let image = image::RgbaImage::from_raw(src_width, src_height, std::mem::take(scratch))
        .context("failed to build image from frame buffer")?;
    let resized = image::imageops::resize(
        &image,
        dst_width.max(1),
        dst_height.max(1),
        image::imageops::FilterType::Triangle,
    );
    rgba_to_bgra(resized.as_raw(), out);
    *scratch = image.into_raw();
    Ok(())
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
    out.reserve(pixels * 4);
    for pixel in src.chunks_exact(8) {
        let r = f16_to_f32(u16::from_le_bytes([pixel[0], pixel[1]]));
        let g = f16_to_f32(u16::from_le_bytes([pixel[2], pixel[3]]));
        let b = f16_to_f32(u16::from_le_bytes([pixel[4], pixel[5]]));
        let a = f16_to_f32(u16::from_le_bytes([pixel[6], pixel[7]]));
        out.push(linear_to_srgb_u8(b));
        out.push(linear_to_srgb_u8(g));
        out.push(linear_to_srgb_u8(r));
        out.push(if a.is_nan() { 255 } else { (a.clamp(0.0, 1.0) * 255.0) as u8 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn scaling_produces_the_requested_size() {
        let src = vec![128u8; 8 * 4 * 4];
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        scale_bgra(&src, 8, 4, 4, 2, &mut scratch, &mut out).unwrap();
        assert_eq!(out.len(), 4 * 2 * 4);
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
