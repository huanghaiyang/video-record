//! Draws the application icon and writes `assets/icon.png` + `assets/icon.ico`.
//!
//! The icon is generated instead of being a binary blob in the repository, so it
//! can be reviewed, tweaked and regenerated:
//!
//! ```text
//! cargo run --release --example make_icon
//! ```
//!
//! Everything is drawn at 4x and box-averaged down, which is what gives the
//! edges their antialiasing. The ICO is written by hand (PNG-compressed entries,
//! which Windows Vista and later accept) and then parsed back as a self-check.

use std::path::{Path, PathBuf};

use image::{ExtendedColorType, ImageFormat};
use screenrec::encode::util;

/// Supersampling factor: each output size is rendered at this multiple.
const SUPERSAMPLE: u32 = 4;

/// Sizes embedded in the ICO file, smallest first (Windows picks per context).
const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// Size of the standalone PNG, used for the in-app window icon.
const PNG_SIZE: u32 = 512;

/// A rounded rectangle in normalized coordinates.
#[derive(Clone, Copy)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
}

impl Rect {
    const fn new(x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> Self {
        Self { x0, y0, x1, y1, radius }
    }
}

/// A BGRA canvas with source-over blending, so shapes can be layered.
struct Canvas {
    size: u32,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(size: u32) -> Self {
        Self {
            size,
            // Start fully transparent.
            pixels: vec![0; (size as usize) * (size as usize) * 4],
        }
    }

    fn size_f(&self) -> f32 {
        self.size as f32
    }

    /// Source-over blend of a straight (non-premultiplied) colour.
    fn blend(&mut self, x: u32, y: u32, colour: [u8; 3], alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let alpha = alpha.clamp(0.0, 1.0);
        let offset = ((y * self.size + x) * 4) as usize;
        let destination_alpha = self.pixels[offset + 3] as f32 / 255.0;
        let out_alpha = alpha + destination_alpha * (1.0 - alpha);
        if out_alpha <= 0.0 {
            return;
        }

        // BGRA in memory: [0] = blue, [1] = green, [2] = red.
        for channel in 0..3 {
            let source = colour[2 - channel] as f32 / 255.0;
            let destination = self.pixels[offset + channel] as f32 / 255.0;
            let blended = (source * alpha + destination * destination_alpha * (1.0 - alpha))
                / out_alpha;
            self.pixels[offset + channel] = (blended * 255.0 + 0.5) as u8;
        }
        self.pixels[offset + 3] = (out_alpha * 255.0 + 0.5) as u8;
    }

    /// Rounded rectangle with a vertical gradient.
    fn rounded_rect(&mut self, rect: Rect, top: [u8; 3], bottom: [u8; 3], alpha: f32) {
        let size = self.size_f();
        let (px0, py0, px1, py1) = (rect.x0 * size, rect.y0 * size, rect.x1 * size, rect.y1 * size);
        let radius = rect.radius * size;
        let from = py0.floor().max(0.0) as u32;
        let to = py1.ceil().min(size) as u32;

        for y in from..to {
            let fy = y as f32 + 0.5;
            let t = ((fy - py0) / (py1 - py0)).clamp(0.0, 1.0);
            let colour = [
                lerp_u8(top[0], bottom[0], t),
                lerp_u8(top[1], bottom[1], t),
                lerp_u8(top[2], bottom[2], t),
            ];
            let from_x = px0.floor().max(0.0) as u32;
            let to_x = px1.ceil().min(size) as u32;
            for x in from_x..to_x {
                let fx = x as f32 + 0.5;
                if inside_rounded(fx, fy, px0, py0, px1, py1, radius) {
                    self.blend(x, y, colour, alpha);
                }
            }
        }
    }

    /// Outline of a rounded rectangle.
    fn rounded_rect_stroke(
        &mut self,
        rect: Rect,
        thickness: f32,
        colour: [u8; 3],
        alpha: f32,
    ) {
        let size = self.size_f();
        let (px0, py0, px1, py1) = (rect.x0 * size, rect.y0 * size, rect.x1 * size, rect.y1 * size);
        let radius = rect.radius * size;
        let thickness = thickness * size;
        let from = py0.floor().max(0.0) as u32;
        let to = py1.ceil().min(size) as u32;

        for y in from..to {
            let fy = y as f32 + 0.5;
            let from_x = px0.floor().max(0.0) as u32;
            let to_x = px1.ceil().min(size) as u32;
            for x in from_x..to_x {
                let fx = x as f32 + 0.5;
                let outer = inside_rounded(fx, fy, px0, py0, px1, py1, radius);
                let inner = inside_rounded(
                    fx,
                    fy,
                    px0 + thickness,
                    py0 + thickness,
                    px1 - thickness,
                    py1 - thickness,
                    (radius - thickness).max(0.0),
                );
                if outer && !inner {
                    self.blend(x, y, colour, alpha);
                }
            }
        }
    }

    /// Filled circle with a vertical gradient.
    fn circle(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        top: [u8; 3],
        bottom: [u8; 3],
        alpha: f32,
    ) {
        let size = self.size_f();
        let (cx, cy, radius) = (cx * size, cy * size, radius * size);
        let from = (cy - radius).floor().max(0.0) as u32;
        let to = (cy + radius).ceil().min(size) as u32;

        for y in from..to {
            let fy = y as f32 + 0.5;
            let t = ((fy - (cy - radius)) / (radius * 2.0)).clamp(0.0, 1.0);
            let colour = [
                lerp_u8(top[0], bottom[0], t),
                lerp_u8(top[1], bottom[1], t),
                lerp_u8(top[2], bottom[2], t),
            ];
            let from_x = (cx - radius).floor().max(0.0) as u32;
            let to_x = (cx + radius).ceil().min(size) as u32;
            for x in from_x..to_x {
                let fx = x as f32 + 0.5;
                let dx = fx - cx;
                let dy = fy - cy;
                if dx * dx + dy * dy <= radius * radius {
                    self.blend(x, y, colour, alpha);
                }
            }
        }
    }
    /// Outline of a circle.
    fn circle_stroke(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        thickness: f32,
        colour: [u8; 3],
        alpha: f32,
    ) {
        let size = self.size_f();
        let (cx, cy, radius, thickness) =
            (cx * size, cy * size, radius * size, thickness * size);
        let outer = radius + thickness / 2.0;
        let inner = (radius - thickness / 2.0).max(0.0);
        let from = (cy - outer).floor().max(0.0) as u32;
        let to = (cy + outer).ceil().min(size) as u32;

        for y in from..to {
            let fy = y as f32 + 0.5;
            let from_x = (cx - outer).floor().max(0.0) as u32;
            let to_x = (cx + outer).ceil().min(size) as u32;
            for x in from_x..to_x {
                let fx = x as f32 + 0.5;
                let distance_squared = (fx - cx) * (fx - cx) + (fy - cy) * (fy - cy);
                if distance_squared <= outer * outer && distance_squared >= inner * inner {
                    self.blend(x, y, colour, alpha);
                }
            }
        }
    }
}

fn inside_rounded(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    // A radius larger than half the shorter side would make the clamp below
    // panic, so saturate it; that also matches how such a shape looks.
    let radius = radius.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0).max(0.0);
    let nearest_x = x.clamp(x0 + radius, x1 - radius);
    let nearest_y = y.clamp(y0 + radius, y1 - radius);
    let dx = x - nearest_x;
    let dy = y - nearest_y;
    dx * dx + dy * dy <= radius * radius
}

fn lerp_u8(from: u8, to: u8, t: f32) -> u8 {
    (from as f32 + (to as f32 - from as f32) * t.clamp(0.0, 1.0)) as u8
}

/// Draws the icon at one size, supersampled and averaged down.
fn render(size: u32) -> Vec<u8> {
    let big = render_oversampled(size * SUPERSAMPLE);

    // Box-average by exactly SUPERSAMPLE using the pipeline's scaler, then swap
    // BGRA to RGBA for encoding.
    let mut downscaled = Vec::new();
    let mut scratch = Vec::new();
    util::scale_bgra(&big, size * SUPERSAMPLE, size * SUPERSAMPLE, size, size, &mut scratch, &mut downscaled)
        .expect("downscale");

    // util::bgra_to_rgba swaps in place; the canvas is already BGRA.
    util::bgra_to_rgba_in_place(&mut downscaled);
    downscaled
}

/// The actual artwork, in normalized coordinates so it scales to any size.
fn render_oversampled(size: u32) -> Vec<u8> {
    let mut canvas = Canvas::new(size);

    // Rounded tile with a dark vertical gradient.
    canvas.rounded_rect(
        Rect::new(0.030, 0.030, 0.970, 0.970, 0.215),
        [0x3A, 0x46, 0x60], // top
        [0x16, 0x1A, 0x23], // bottom
        1.0,
    );
    // Faint outline so the tile keeps an edge on dark backgrounds.
    canvas.rounded_rect_stroke(
        Rect::new(0.030, 0.030, 0.970, 0.970, 0.215),
        0.012,
        [0xFF, 0xFF, 0xFF],
        0.07,
    );

    // Monitor body.
    canvas.rounded_rect(
        Rect::new(0.140, 0.200, 0.860, 0.680, 0.055),
        [0x4E, 0x5C, 0x78],
        [0x32, 0x3B, 0x4E],
        1.0,
    );
    canvas.rounded_rect_stroke(
        Rect::new(0.140, 0.200, 0.860, 0.680, 0.055),
        0.016,
        [0xFF, 0xFF, 0xFF],
        0.20,
    );

    // Stand: neck then base.
    canvas.rounded_rect(
        Rect::new(0.455, 0.680, 0.545, 0.760, 0.012),
        [0x3E, 0x49, 0x5E],
        [0x2E, 0x36, 0x46],
        1.0,
    );
    canvas.rounded_rect(
        Rect::new(0.330, 0.760, 0.670, 0.805, 0.022),
        [0x46, 0x53, 0x6A],
        [0x2E, 0x36, 0x46],
        1.0,
    );

    // Record dot: the part that still reads at 16 px.
    canvas.circle(0.500, 0.442, 0.152, [0xFF, 0x74, 0x64], [0xD3, 0x25, 0x1B], 1.0);
    // Soft highlight for a little volume.
    canvas.circle(0.452, 0.392, 0.052, [0xFF, 0xFF, 0xFF], [0xFF, 0xFF, 0xFF], 0.30);

    // Inner rim of the dot, so it does not look like a flat blob when large.
    canvas.circle_stroke(0.500, 0.442, 0.140, 0.013, [0x7A, 0x10, 0x0A], 0.55);

    canvas.pixels
}

/// One PNG-compressed image inside the ICO container.
struct IcoFrame {
    size: u32,
    png: Vec<u8>,
}

fn encode_png(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut png = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png);
    image::write_buffer_with_format(
        &mut cursor,
        rgba,
        size,
        size,
        ExtendedColorType::Rgba8,
        ImageFormat::Png,
    )
    .expect("png encode");
    png
}

/// Writes a multi-size ICO with PNG-compressed entries.
fn write_ico(path: &Path, frames: &[IcoFrame]) -> std::io::Result<()> {
    let mut out = Vec::new();

    // ICONDIR
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());

    // ICONDIRENTRY per frame, then the image data.
    let mut offset = 6 + 16 * frames.len() as u32;
    for frame in frames {
        // 256 is encoded as 0 in the single-byte fields.
        out.push(if frame.size >= 256 { 0 } else { frame.size as u8 });
        out.push(if frame.size >= 256 { 0 } else { frame.size as u8 });
        out.push(0); // palette size
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(frame.png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += frame.png.len() as u32;
    }
    for frame in frames {
        out.extend_from_slice(&frame.png);
    }

    std::fs::write(path, out)
}

/// Parses the ICO back so a malformed container cannot ship unnoticed.
fn verify_ico(path: &Path) -> std::io::Result<Vec<(u32, usize)>> {
    let bytes = std::fs::read(path)?;
    let read_u16 = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    let read_u32 = |at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize
    };

    assert_eq!(read_u16(0), 0, "reserved field");
    assert_eq!(read_u16(2), 1, "type must be icon");
    let count = read_u16(4);
    assert!(count > 0, "no images");

    let mut frames = Vec::new();
    for index in 0..count {
        let base = 6 + index * 16;
        let width = if bytes[base] == 0 { 256 } else { bytes[base] as usize };
        let height = if bytes[base + 1] == 0 { 256 } else { bytes[base + 1] as usize };
        assert_eq!(width, height, "icon entries must be square");
        let length = read_u32(base + 8);
        let start = read_u32(base + 12);
        assert!(start + length <= bytes.len(), "entry {index} runs past the file");
        // Every entry is a PNG: check the signature.
        assert_eq!(
            &bytes[start..start + 8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "entry {index} is not a PNG"
        );
        // ...and that a real decoder accepts it.
        let decoded = image::load_from_memory(&bytes[start..start + length])
            .expect("embedded png must decode");
        assert_eq!((decoded.width(), decoded.height()), (width as u32, height as u32));
        frames.push((width as u32, length));
    }
    Ok(frames)
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("create assets directory");

    // Master PNG for the in-app window icon.
    let master = render(PNG_SIZE);
    let png_path = assets.join("icon.png");
    std::fs::write(&png_path, encode_png(&master, PNG_SIZE)).expect("write icon.png");
    println!(
        "{}: {PNG_SIZE}x{PNG_SIZE}, {} 字节",
        png_path.display(),
        std::fs::metadata(&png_path).unwrap().len()
    );

    // Multi-size ICO for the executable.
    let frames: Vec<IcoFrame> = ICO_SIZES
        .iter()
        .map(|size| IcoFrame { size: *size, png: encode_png(&render(*size), *size) })
        .collect();
    let ico_path = assets.join("icon.ico");
    write_ico(&ico_path, &frames).expect("write icon.ico");

    let verified = verify_ico(&ico_path).expect("verify icon.ico");
    println!(
        "{}: {} 字节, {} 个尺寸 -> {}",
        ico_path.display(),
        std::fs::metadata(&ico_path).unwrap().len(),
        verified.len(),
        verified
            .iter()
            .map(|(size, bytes)| format!("{size}px/{bytes}B"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Also drop a 32 px preview next to the others, which is the size that
    // decides whether the icon is legible in a taskbar.
    let preview = render(32);
    let preview_path = assets.join("icon_32_preview.png");
    std::fs::write(&preview_path, encode_png(&preview, 32)).expect("write preview");
    println!("{}: 预览", preview_path.display());
}
