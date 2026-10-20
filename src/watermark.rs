//! 水印：预渲染贴图 + 每帧 alpha 合成。
//!
//! 录屏是逐帧的热路径，所以这里把工作拆成两半：
//!
//! 1. [`render_sprite`] **只做一次**：用 GDI 把文字画成白字黑底的 32 位 DIB，
//!    再以像素亮度当覆盖率 alpha（GDI 的抗锯齿会给出中间灰度）；图案则直接
//!    程序化生成。颜色与不透明度在这一步就烘焙进贴图，得到
//!    [`WatermarkSprite`]（RGBA，带 alpha）。调用方负责缓存这个贴图。
//! 2. [`composite`] **每帧**：按 [`placements`] 给出的坐标把贴图 alpha 混合
//!    进 BGRA 帧，只做裁剪与整数混合，没有任何分配。
//!
//! [`placements`] 是 `elapsed` 的纯函数（没有内部状态、没有随机数），因此
//! 暂停/续录、改帧率或重放同一时刻都能得到完全一样的水印位置。
//!
//! # pipeline 里的典型用法
//!
//! ```no_run
//! # use std::time::Duration;
//! # use screenrec::watermark::{self, WatermarkSettings, WatermarkSprite};
//! # fn demo(frame: &mut [u8], settings: &WatermarkSettings) -> anyhow::Result<()> {
//! // 设置变化时（或开始录制时）重建一次贴图，然后一直缓存
//! let mut cache: Option<WatermarkSprite> = None;
//! if cache.is_none() {
//!     cache = Some(watermark::render_sprite(settings)?);
//! }
//! // 每帧：只做混合
//! if let Some(sprite) = cache.as_ref() {
//!     watermark::composite(frame, 1920, 1080, sprite, settings, Duration::ZERO)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! 注意：颜色与不透明度已经烘焙进贴图，改动 `color` / `opacity` / `font_size`
//! / `text` / `pattern` 之后需要重新调用 [`render_sprite`]；只有位置、运动、
//! 重复方式（`anchor` / `margin` / `motion` / `speed` / `repeat`）可以就地改。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextCharacterExtra, SetTextColor, TextOutW,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, FF_DONTCARE, FW_NORMAL, HBITMAP, HDC, HFONT,
    HGDIOBJ, OUT_TT_PRECIS, TRANSPARENT,
};

/// 渲染文字时使用的字体，GDI 找不到时会自动回退到最接近的字体。
const FONT_FACE: &str = "Microsoft YaHei";

/// 字号下限（像素）。再小就只剩糊点，没有意义。
const MIN_FONT_SIZE: u32 = 8;

/// 字号上限（像素）。防止异常配置把贴图撑爆。
const MAX_FONT_SIZE: u32 = 320;

/// 贴图单边的像素上限。
const MAX_SPRITE_EDGE: u32 = 8192;

/// 贴图总像素上限（≈ 2000×2000），超出说明配置有误。
const MAX_SPRITE_PIXELS: u64 = 4_000_000;

/// 一帧最多贴多少张图。极端配置（极小字号 + 4K 平铺）下截断，防止单帧卡死。
const MAX_PLACEMENTS: usize = 16_384;

/// 基础图案：与文字叠加，或单独作为几何纹理。
///
/// 图案是程序化生成的，不依赖 GDI；平铺（[`WatermarkRepeat::Tile`]）时每个
/// 图案瓦片的四条边都是无缝的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkPattern {
    /// 不使用图案。
    #[default]
    None,
    /// 圆点矩阵。
    Dots,
    /// 45° 双向斜线网格。
    Grid,
    /// 圆角方块描边 + 圆心实心点（录制标志）。
    Logo,
}

impl WatermarkPattern {
    /// 供 UI 遍历的全部取值。
    pub const ALL: [Self; 4] = [Self::None, Self::Dots, Self::Grid, Self::Logo];

    /// 中文名称，用于界面与日志。
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "无",
            Self::Dots => "圆点",
            Self::Grid => "网格",
            Self::Logo => "徽标",
        }
    }

    /// 稳定的字符串 id（kebab-case），用于 CLI 与界面展示。
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dots => "dots",
            Self::Grid => "grid",
            Self::Logo => "logo",
        }
    }

    /// 从 id 解析图案；大小写、`-` 与 `_` 都不敏感，未知值返回 `None`。
    pub fn from_id(id: &str) -> Option<Self> {
        let needle = normalize_id(id);
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.id() == needle)
    }
}

/// 静态水印停靠的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkAnchor {
    /// 左上角。
    TopLeft,
    /// 右上角。
    TopRight,
    /// 左下角。
    BottomLeft,
    /// 右下角。
    #[default]
    BottomRight,
    /// 画面正中央。
    Center,
}

impl WatermarkAnchor {
    /// 供 UI 遍历的全部取值。
    pub const ALL: [Self; 5] = [
        Self::TopLeft,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomRight,
        Self::Center,
    ];

    /// 中文名称，用于界面与日志。
    pub fn label(self) -> &'static str {
        match self {
            Self::TopLeft => "左上",
            Self::TopRight => "右上",
            Self::BottomLeft => "左下",
            Self::BottomRight => "右下",
            Self::Center => "居中",
        }
    }

    /// 稳定的字符串 id（kebab-case），用于 CLI 与界面展示。
    pub fn id(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::TopRight => "top-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomRight => "bottom-right",
            Self::Center => "center",
        }
    }

    /// 从 id 解析位置；大小写、`-` 与 `_` 都不敏感，未知值返回 `None`。
    pub fn from_id(id: &str) -> Option<Self> {
        let needle = normalize_id(id);
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.id() == needle)
    }
}

/// 水印的运动方式。
///
/// 所有运动都是 `elapsed` 的纯函数：位置由“已累计时间 × 速度”算出来，
/// 没有积分、没有内部状态，所以暂停后继续不会跳变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkMotion {
    /// 静止在锚点上。
    #[default]
    None,
    /// 向左滚动，滚出画面后从另一侧重新进入。
    ScrollLeft,
    /// 向右滚动。
    ScrollRight,
    /// 向上滚动。
    ScrollUp,
    /// 向下滚动。
    ScrollDown,
    /// 斜向（左上）滚动，两个方向各自循环。
    Diagonal,
    /// 在画面内来回反弹（碰边折返），不会离开画面。
    Bounce,
}

impl WatermarkMotion {
    /// 供 UI 遍历的全部取值。
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::ScrollLeft,
        Self::ScrollRight,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::Diagonal,
        Self::Bounce,
    ];

    /// 中文名称，用于界面与日志。
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "静止",
            Self::ScrollLeft => "向左滚动",
            Self::ScrollRight => "向右滚动",
            Self::ScrollUp => "向上滚动",
            Self::ScrollDown => "向下滚动",
            Self::Diagonal => "斜向滚动",
            Self::Bounce => "来回反弹",
        }
    }

    /// 稳定的字符串 id（kebab-case），用于 CLI 与界面展示。
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ScrollLeft => "scroll-left",
            Self::ScrollRight => "scroll-right",
            Self::ScrollUp => "scroll-up",
            Self::ScrollDown => "scroll-down",
            Self::Diagonal => "diagonal",
            Self::Bounce => "bounce",
        }
    }

    /// 从 id 解析运动方式；大小写、`-` 与 `_` 都不敏感，未知值返回 `None`。
    pub fn from_id(id: &str) -> Option<Self> {
        let needle = normalize_id(id);
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.id() == needle)
    }
}

/// 水印的重复方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkRepeat {
    /// 只贴一张。
    #[default]
    Off,
    /// 按“贴图尺寸 + 间隔”铺满整帧。
    Tile,
    /// 跑马灯：只占一行（或一列），沿运动方向循环滚动。
    Ticker,
}

impl WatermarkRepeat {
    /// 供 UI 遍历的全部取值。
    pub const ALL: [Self; 3] = [Self::Off, Self::Tile, Self::Ticker];

    /// 中文名称，用于界面与日志。
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "单个",
            Self::Tile => "平铺",
            Self::Ticker => "跑马灯",
        }
    }

    /// 稳定的字符串 id（kebab-case），用于 CLI 与界面展示。
    pub fn id(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Tile => "tile",
            Self::Ticker => "ticker",
        }
    }

    /// 从 id 解析重复方式；大小写、`-` 与 `_` 都不敏感，未知值返回 `None`。
    pub fn from_id(id: &str) -> Option<Self> {
        let needle = normalize_id(id);
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.id() == needle)
    }
}

/// 归一化字符串 id：去掉首尾空白、转小写，并把下划线视同连字符，
/// 这样配置里的 `snake_case` 和命令行里的 `kebab-case` 都能对上。
fn normalize_id(id: &str) -> String {
    id.trim().to_ascii_lowercase().replace('_', "-")
}

/// 水印设置。
///
/// 这份结构体会被写进配置，因此带 `#[serde(default)]`：旧版本配置文件里缺少
/// 的字段一律回落到 [`Default`] 的值，不会导致读取失败。字段的语义见各个
/// 字段的文档。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatermarkSettings {
    /// 总开关。
    pub enabled: bool,
    /// 文字内容，可含中文（用 GDI + `Microsoft YaHei` 渲染）。
    pub text: String,
    /// 基础图案，与文字叠加显示，或单独使用。
    pub pattern: WatermarkPattern,
    /// 字号，即文字在输出画面里的像素高度（会被夹到 8..=320）。
    pub font_size: u32,
    /// 文字与图案的颜色（RGB）。
    pub color: [u8; 3],
    /// 不透明度，0..=1。渲染贴图时烘焙进 alpha。
    pub opacity: f32,
    /// 静态位置的边距（像素）。负值会把水印推到画面外。平铺/跑马灯时它是相邻
    /// 贴图间隔的下限，实际间距还会被抬到至少 0.75 倍贴图高且不小于 24 像素，
    /// 避免密集平铺时文字连成一片（见内部函数 `tile_gap`）。
    pub margin: i32,
    /// 静态锚点。
    pub anchor: WatermarkAnchor,
    /// 运动方式。
    pub motion: WatermarkMotion,
    /// 运动速度，像素/秒。0 表示不移动。
    pub speed: f32,
    /// 重复方式：单个 / 平铺 / 跑马灯。
    pub repeat: WatermarkRepeat,
}

impl Default for WatermarkSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            text: "ScreenRec".to_string(),
            pattern: WatermarkPattern::None,
            font_size: 32,
            color: [255, 255, 255],
            opacity: 0.75,
            margin: 24,
            anchor: WatermarkAnchor::BottomRight,
            motion: WatermarkMotion::None,
            speed: 60.0,
            repeat: WatermarkRepeat::Off,
        }
    }
}

impl WatermarkSettings {
    /// 是否真的会画出东西：开关打开，且有文字或图案。
    ///
    /// pipeline 可以在每帧合成前用它跳过一整帧的水印开销。
    pub fn is_active(&self) -> bool {
        self.enabled && (!self.text.trim().is_empty() || self.pattern != WatermarkPattern::None)
    }
}

/// 预渲染好的水印贴图：RGBA、带 alpha、按行紧凑排列（无 padding）。
///
/// 由 [`render_sprite`] 生成并缓存在调用方；[`composite`] 只读它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatermarkSprite {
    width: u32,
    height: u32,
    /// 每像素 4 字节，顺序为 R、G、B、A。
    pixels: Vec<u8>,
}

impl WatermarkSprite {
    /// 用现成的 RGBA 数据构造贴图，`pixels` 长度必须是 `width * height * 4`。
    ///
    /// 主要用于测试，或让调用方把自定义图标当成水印贴图。
    pub fn from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> anyhow::Result<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("水印贴图尺寸过大：{width}×{height}"))?;
        anyhow::ensure!(
            pixels.len() == expected,
            "水印贴图数据长度不匹配：{} != {expected}（{width}×{height}×4）",
            pixels.len()
        );
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// 贴图宽度（像素）。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 贴图高度（像素）。
    pub fn height(&self) -> u32 {
        self.height
    }

    /// `(宽, 高)`，方便直接传给 [`placements`] / [`composite`]。
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// RGBA 像素数据（只读）。
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// 空贴图（任意一边为 0），此时什么都不会画。
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.pixels.is_empty()
    }
}

/// 把设置渲染成一张贴图；调用方缓存它，每帧只做 [`composite`]。
///
/// 文字走 GDI（`CreateFontW` + 32 位自顶向下 DIB，白字黑底），以像素亮度作为
/// 覆盖率 alpha；图案程序化生成。两者都存在时，图案先铺底、文字叠在上面。
/// 颜色与 `opacity` 在这一步乘进 RGBA，所以它们变化后需要重新渲染。
///
/// # 错误
///
/// 文字和图案都为空、GDI 调用失败（创建 DC / 字体 / 位图、测量或绘制失败）、
/// 或算出的贴图过大时，返回带中文说明的错误，不会 panic。
pub fn render_sprite(settings: &WatermarkSettings) -> anyhow::Result<WatermarkSprite> {
    let font_size = settings.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    let text = settings.text.trim();

    // 中文密排会糊成一片，加一点字距；字号越大间距越大。
    let letter_spacing = (font_size / 12) as i32;
    let text_mask = if text.is_empty() {
        None
    } else {
        Some(render_text_mask(text, font_size, letter_spacing)?)
    };
    let pattern_mask = pattern_tile(settings.pattern, font_size);

    // 文字四周留一点内边距，避免贴边与抗锯齿被裁掉。
    let pad = (font_size / 8).clamp(2, 12);
    let (width, height) = match (&text_mask, &pattern_mask) {
        (Some(text), _) => (text.width + pad * 2, text.height + pad * 2),
        (None, Some(tile)) => (tile.width, tile.height),
        (None, None) => anyhow::bail!("水印内容为空：请填写文字或选择图案"),
    };

    if width > MAX_SPRITE_EDGE
        || height > MAX_SPRITE_EDGE
        || u64::from(width) * u64::from(height) > MAX_SPRITE_PIXELS
    {
        anyhow::bail!("水印贴图过大：{width}×{height}（请减小字号或缩短文字）");
    }

    // 覆盖率层：先铺图案，再把文字叠上去（取较大值，等价于两层的“覆盖并集”）。
    let mut coverage = vec![0u8; (width as usize) * (height as usize)];
    if let Some(tile) = &pattern_mask {
        fill_tiled(&mut coverage, width, height, tile);
    }
    if let Some(text) = &text_mask {
        overlay(&mut coverage, width, pad, pad, text);
    }

    let opacity = if settings.opacity.is_finite() {
        settings.opacity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let scale = (opacity * 255.0).round() as u32;
    let [red, green, blue] = settings.color;

    let mut pixels = vec![0u8; coverage.len() * 4];
    for (index, alpha) in coverage.iter().enumerate() {
        if *alpha == 0 {
            continue;
        }
        // alpha = 覆盖率 × 不透明度，四舍五入
        let alpha = (u32::from(*alpha) * scale + 127) / 255;
        if alpha == 0 {
            continue;
        }
        let base = index * 4;
        pixels[base] = red;
        pixels[base + 1] = green;
        pixels[base + 2] = blue;
        pixels[base + 3] = alpha as u8;
    }

    log::debug!("水印贴图已生成：{width}×{height}，不透明度 {opacity:.2}");
    Ok(WatermarkSprite {
        width,
        height,
        pixels,
    })
}

/// 平铺/跑马灯时相邻水印之间的间距。
///
/// `margin` 是下限，但至少要 0.75 倍贴图高、且不小于 24 像素：否则密集平铺时
/// 相邻文字会连成一片，读不出来。间距与贴图尺寸一起决定滚动周期。
pub(crate) fn tile_gap(sprite_height: u32, settings: &WatermarkSettings) -> f64 {
    f64::from(settings.margin.max(0))
        .max(f64::from(sprite_height) * 0.75)
        .max(24.0)
}

/// 计算每一张贴图的左上角坐标（像素）。
///
/// * 返回的坐标允许为负，表示贴图有一部分在画面外，[`composite`] 会裁剪；
/// * 结果是 `elapsed` 的纯函数：同一组入参永远得到同一组坐标；
/// * 平铺以“贴图尺寸 + 间距”为步距铺满整帧，动画时整张网格一起滚动；
/// * 跑马灯只占一行（水平运动）或一列（垂直运动），沿运动方向循环；
/// * 反弹（[`WatermarkMotion::Bounce`]）把位置限制在画面内，碰边折返；
/// * 数量上限 `16384`，极端配置下会截断（其余参数仍然确定）。
pub fn placements(
    width: u32,
    height: u32,
    sprite: (u32, u32),
    settings: &WatermarkSettings,
    elapsed: Duration,
) -> Vec<(i32, i32)> {
    let frame_w = f64::from(width);
    let frame_h = f64::from(height);
    let sprite_w = f64::from(sprite.0);
    let sprite_h = f64::from(sprite.1);
    if frame_w < 1.0 || frame_h < 1.0 || sprite_w < 1.0 || sprite_h < 1.0 {
        return Vec::new();
    }

    let gap = tile_gap(sprite.1, settings);
    let stride_x = (sprite_w + gap).max(1.0);
    let stride_y = (sprite_h + gap).max(1.0);

    let speed = if settings.speed.is_finite() {
        f64::from(settings.speed.max(0.0))
    } else {
        0.0
    };
    let traveled = speed * elapsed.as_secs_f64();
    let traveled = if traveled.is_finite() {
        traveled.max(0.0)
    } else {
        0.0
    };

    match settings.repeat {
        WatermarkRepeat::Off => {
            let base = anchor_origin(
                (frame_w, frame_h),
                (sprite_w, sprite_h),
                settings.margin,
                settings.anchor,
            );
            if settings.motion == WatermarkMotion::Bounce {
                let x = bounce_position(base.0, traveled, frame_w - sprite_w);
                let y = bounce_position(base.1, traveled, frame_h - sprite_h);
                return vec![(round_to_i32(x), round_to_i32(y))];
            }
            // 单个贴图滚出画面后要等它完全离开再从另一侧进入，所以周期是
            // “画面 + 贴图”的长度。
            let (dx, dy) = scroll_offsets(
                settings.motion,
                traveled,
                frame_w + sprite_w,
                frame_h + sprite_h,
            );
            vec![(round_to_i32(base.0 + dx), round_to_i32(base.1 + dy))]
        }
        WatermarkRepeat::Tile => {
            let (dx, dy) = scroll_offsets(settings.motion, traveled, stride_x, stride_y);
            let columns = axis_positions(dx, frame_w, sprite_w, stride_x);
            let rows = axis_positions(dy, frame_h, sprite_h, stride_y);
            let mut out = Vec::with_capacity(
                columns
                    .len()
                    .saturating_mul(rows.len())
                    .min(MAX_PLACEMENTS),
            );
            for &top in &rows {
                for &left in &columns {
                    if out.len() >= MAX_PLACEMENTS {
                        return out;
                    }
                    out.push((left, top));
                }
            }
            out
        }
        WatermarkRepeat::Ticker => {
            let base = anchor_origin(
                (frame_w, frame_h),
                (sprite_w, sprite_h),
                settings.margin,
                settings.anchor,
            );
            let (dx, dy) = scroll_offsets(settings.motion, traveled, stride_x, stride_y);
            match settings.motion {
                WatermarkMotion::ScrollUp | WatermarkMotion::ScrollDown => {
                    let left = round_to_i32(base.0);
                    axis_positions(dy, frame_h, sprite_h, stride_y)
                        .into_iter()
                        .map(|top| (left, top))
                        .collect()
                }
                _ => {
                    let top = round_to_i32(base.1);
                    axis_positions(dx, frame_w, sprite_w, stride_x)
                        .into_iter()
                        .map(|left| (left, top))
                        .collect()
                }
            }
        }
    }
}

/// 把贴图按 [`placements`] 混合进 BGRA 帧。
///
/// * `frame` 是 BGRA、自上而下、无 padding，长度至少 `width * height * 4`
///   （多出来的尾部字节不会被写入）；
/// * 只写 RGB 三个通道，alpha 字节保持原样（视频编码不使用它）；
/// * `settings.enabled == false`、`opacity <= 0` 或贴图为空时直接返回，
///   一个字节都不动；
/// * 贴图越出画面的部分被裁剪，不会写到缓冲区之外。
///
/// # 错误
///
/// 帧尺寸为 0、`width * height * 4` 溢出，或 `frame` 比需要的短时返回中文
/// 错误；这些都属于调用方的契约问题，不是正常运行时的情况。
pub fn composite(
    frame: &mut [u8],
    width: u32,
    height: u32,
    sprite: &WatermarkSprite,
    settings: &WatermarkSettings,
    elapsed: Duration,
) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("水印合成失败：帧尺寸无效（{width}×{height}）");
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("水印合成失败：帧尺寸过大（{width}×{height}）"))?;
    anyhow::ensure!(
        frame.len() >= expected,
        "水印合成失败：帧数据长度不匹配（{} < {expected}，{width}×{height}×4）",
        frame.len()
    );

    if !settings.enabled || settings.opacity <= 0.0 || sprite.is_empty() {
        return Ok(());
    }

    let frame_w = width as usize;
    let frame_h = height as usize;
    let sprite_w = sprite.width as usize;
    let sprite_h = sprite.height as usize;

    for (left, top) in placements(width, height, (sprite.width, sprite.height), settings, elapsed) {
        // 先把贴图矩形裁到帧内（贴图可以有一部分在画面外）。
        let x0 = i64::from(left).max(0);
        let y0 = i64::from(top).max(0);
        let x1 = (i64::from(left) + sprite_w as i64).min(frame_w as i64);
        let y1 = (i64::from(top) + sprite_h as i64).min(frame_h as i64);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }

        let top = i64::from(top);
        let left = i64::from(left);
        for y in y0..y1 {
            let source_row = (y - top) as usize * sprite_w * 4;
            let destination_row = y as usize * frame_w * 4;
            for x in x0..x1 {
                let source = source_row + (x - left) as usize * 4;
                let alpha = u32::from(sprite.pixels[source + 3]);
                if alpha == 0 {
                    continue;
                }
                let destination = destination_row + x as usize * 4;
                let inverse = 255 - alpha;
                // 帧是 B、G、R、A，贴图是 R、G、B、A。
                frame[destination] = blend(
                    sprite.pixels[source + 2],
                    frame[destination],
                    alpha,
                    inverse,
                );
                frame[destination + 1] = blend(
                    sprite.pixels[source + 1],
                    frame[destination + 1],
                    alpha,
                    inverse,
                );
                frame[destination + 2] =
                    blend(sprite.pixels[source], frame[destination + 2], alpha, inverse);
            }
        }
    }

    Ok(())
}

/// 一帧像素的整数 alpha 混合：`源 × a + 目标 × (1 - a)`，四舍五入。
fn blend(source: u8, destination: u8, alpha: u32, inverse: u32) -> u8 {
    ((u32::from(source) * alpha + u32::from(destination) * inverse + 127) / 255) as u8
}

/// 锚点对应的左上角坐标（未做取整）。`margin` 为负时会把水印推出画面。
fn anchor_origin(
    frame: (f64, f64),
    sprite: (f64, f64),
    margin: i32,
    anchor: WatermarkAnchor,
) -> (f64, f64) {
    let (frame_w, frame_h) = frame;
    let (sprite_w, sprite_h) = sprite;
    let offset = f64::from(margin);
    let left = offset;
    let top = offset;
    let right = frame_w - offset - sprite_w;
    let bottom = frame_h - offset - sprite_h;
    let center_x = (frame_w - sprite_w) / 2.0;
    let center_y = (frame_h - sprite_h) / 2.0;

    match anchor {
        WatermarkAnchor::TopLeft => (left, top),
        WatermarkAnchor::TopRight => (right, top),
        WatermarkAnchor::BottomLeft => (left, bottom),
        WatermarkAnchor::BottomRight => (right, bottom),
        WatermarkAnchor::Center => (center_x, center_y),
    }
}

/// 运动在一个周期内的位移（`traveled` 已经按周期取模）。
///
/// 返回值为相对锚点的偏移；滚动方向决定正负，反弹在网格模式下等同静止。
fn scroll_offsets(
    motion: WatermarkMotion,
    traveled: f64,
    period_x: f64,
    period_y: f64,
) -> (f64, f64) {
    let wrap = |period: f64| {
        if period > 0.0 {
            traveled % period
        } else {
            0.0
        }
    };

    match motion {
        WatermarkMotion::ScrollLeft => (-wrap(period_x), 0.0),
        WatermarkMotion::ScrollRight => (wrap(period_x), 0.0),
        WatermarkMotion::ScrollUp => (0.0, -wrap(period_y)),
        WatermarkMotion::ScrollDown => (0.0, wrap(period_y)),
        WatermarkMotion::Diagonal => (-wrap(period_x), -wrap(period_y)),
        WatermarkMotion::None | WatermarkMotion::Bounce => (0.0, 0.0),
    }
}

/// 在一维上做“碰边折返”：从 `start` 出发走 `traveled` 后落在 `[0, limit]` 内。
///
/// 用三角波实现，因此永远不可能跑出边界；`limit <= 0`（贴图比画面还大）时
/// 固定在 0。
fn bounce_position(start: f64, traveled: f64, limit: f64) -> f64 {
    if limit <= 0.0 {
        return 0.0;
    }
    let span = 2.0 * limit;
    let phase = (start + traveled).rem_euclid(span);
    if phase <= limit {
        phase
    } else {
        span - phase
    }
}

/// 一维上铺满 `[0, limit)` 所需的贴图起点，步距 `stride`。
///
/// 起点先按 `stride` 取模落进 `(-stride, 0]`，这样既保证画面左/上边缘之前
/// 那一格不会缺，也保证循环次数与画面尺寸成正比（不会因超大 margin 卡住）。
fn axis_positions(start: f64, limit: f64, size: f64, stride: f64) -> Vec<i32> {
    let stride = stride.max(1.0);
    let mut at = start % stride;
    if at > 0.0 {
        at -= stride;
    }

    let mut out = Vec::new();
    while at < limit && out.len() < MAX_PLACEMENTS {
        // 完全落在画面外（右/下侧或左/上侧）的那一格不必返回。
        if at + size > 0.0 {
            out.push(round_to_i32(at));
        }
        at += stride;
    }
    out
}

/// 四舍五入成坐标，并把极端值夹到 `i32` 范围内（`as` 转换本身是饱和的，
/// 这里显式写出来是为了让边界行为一目了然）。
fn round_to_i32(value: f64) -> i32 {
    value.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

// ---------------------------------------------------------------------------
// 图案（程序化生成）
// ---------------------------------------------------------------------------

/// 一张 8 位覆盖率遮罩：0 = 完全透明，255 = 完全不透明。
#[derive(Debug, Clone)]
struct Mask {
    width: u32,
    height: u32,
    alpha: Vec<u8>,
}

/// 生成图案瓦片；[`WatermarkPattern::None`] 返回 `None`。
fn pattern_tile(pattern: WatermarkPattern, font_size: u32) -> Option<Mask> {
    match pattern {
        WatermarkPattern::None => None,
        WatermarkPattern::Dots => Some(tile_dots(font_size)),
        WatermarkPattern::Grid => Some(tile_grid(font_size)),
        WatermarkPattern::Logo => Some(tile_logo(font_size)),
    }
}

/// 圆点矩阵：瓦片正中一个圆点，平铺后形成点阵。
fn tile_dots(font_size: u32) -> Mask {
    let pitch = (font_size as f32 / 2.0).clamp(6.0, 48.0).ceil() as u32;
    let center = pitch as f32 / 2.0;
    let radius = (pitch as f32 * 0.18).max(1.0);
    sample_tile(pitch, move |x, y| {
        let dx = x + 0.5 - center;
        let dy = y + 0.5 - center;
        dx * dx + dy * dy <= radius * radius
    })
}

/// 45° 双向斜线网格。线位置按 `pitch` 取模，所以平铺时四边无缝。
fn tile_grid(font_size: u32) -> Mask {
    let pitch = (font_size as f32 / 2.0).clamp(6.0, 64.0).ceil() as u32;
    let pitch = pitch.max(4);
    let span = pitch as f32;
    let half_width = (span * 0.05).clamp(0.6, 1.6);
    sample_tile(pitch, move |x, y| {
        let rising = (x + y).rem_euclid(span);
        let falling = (x - y).rem_euclid(span);
        rising < half_width
            || rising > span - half_width
            || falling < half_width
            || falling > span - half_width
    })
}

/// 徽标：圆角方块描边 + 圆心实心点。
fn tile_logo(font_size: u32) -> Mask {
    let size = font_size.clamp(16, 256);
    let side = size as f32;
    let half = side * 0.43;
    let corner = side * 0.22;
    let ring = (side * 0.05).clamp(1.0, 6.0);
    let dot = side * 0.17;

    sample_tile(size, move |x, y| {
        let px = x + 0.5 - side / 2.0;
        let py = y + 0.5 - side / 2.0;
        // 圆角矩形的有符号距离场：|sd| <= 线宽一半 即为描边。
        let qx = px.abs() - (half - corner);
        let qy = py.abs() - (half - corner);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        let signed = outside + qx.max(qy).min(0.0) - corner;
        let frame = signed.abs() <= ring / 2.0;
        let center = (px * px + py * py).sqrt() <= dot;
        frame || center
    })
}

/// 用“每像素 2×2 超采样”把几何判定函数画成覆盖率瓦片，边缘自带抗锯齿。
fn sample_tile(size: u32, inside: impl Fn(f32, f32) -> bool) -> Mask {
    const OFFSETS: [f32; 2] = [0.25, 0.75];
    let size = size.max(2);
    let mut alpha = vec![0u8; (size as usize) * (size as usize)];

    for y in 0..size {
        for x in 0..size {
            let mut hits = 0u32;
            for &offset_y in &OFFSETS {
                for &offset_x in &OFFSETS {
                    if inside(x as f32 + offset_x, y as f32 + offset_y) {
                        hits += 1;
                    }
                }
            }
            alpha[(y * size + x) as usize] = (hits * 255 / 4) as u8;
        }
    }

    Mask {
        width: size,
        height: size,
        alpha,
    }
}

/// 把瓦片按它自己的尺寸循环铺满 `width × height` 的覆盖率层（取较大值）。
fn fill_tiled(coverage: &mut [u8], width: u32, height: u32, tile: &Mask) {
    let tile_w = tile.width as usize;
    for y in 0..height {
        let source_row = (y % tile.height) as usize * tile_w;
        let destination_row = y as usize * width as usize;
        for x in 0..width {
            let value = tile.alpha[source_row + (x % tile.width) as usize];
            let slot = &mut coverage[destination_row + x as usize];
            if value > *slot {
                *slot = value;
            }
        }
    }
}

/// 把遮罩叠加到 `(offset_x, offset_y)`，取较大值（覆盖并集）。
fn overlay(coverage: &mut [u8], width: u32, offset_x: u32, offset_y: u32, mask: &Mask) {
    let width = width as usize;
    for row in 0..mask.height {
        let destination_row = (offset_y + row) as usize * width + offset_x as usize;
        let source_row = row as usize * mask.width as usize;
        for column in 0..mask.width as usize {
            let value = mask.alpha[source_row + column];
            let slot = &mut coverage[destination_row + column];
            if value > *slot {
                *slot = value;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GDI 文字渲染
// ---------------------------------------------------------------------------

/// GDI 内存 DC：只负责在离开作用域时 `DeleteDC`。
struct MemoryDc(HDC);

impl Drop for MemoryDc {
    fn drop(&mut self) {
        // SAFETY: 句柄来自 `CreateCompatibleDC`，且只在这里释放一次。
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

/// 已经选进 DC 的字体：释放时先换回旧对象再删除，避免把字体留在 DC 里。
struct SelectedFont {
    dc: HDC,
    font: HFONT,
    previous: HGDIOBJ,
}

impl Drop for SelectedFont {
    fn drop(&mut self) {
        // SAFETY: `font` 是 `CreateFontW` 的产物，`previous` 是 `SelectObject`
        // 返回的原对象；判定成功后才构造本结构体。
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.font.into());
        }
    }
}

/// 已经选进 DC 的 DIB 位图：释放时先换回旧对象再删除位图。
struct SelectedBitmap {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}

impl Drop for SelectedBitmap {
    fn drop(&mut self) {
        // SAFETY: 同 `SelectedFont`；位图不能在仍被选中的状态下删除。
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}

/// 用 GDI 把文字画成白字黑底的 32 位自顶向下 DIB，再按亮度折算成覆盖率。
///
/// GDI 不会写 alpha 通道，但抗锯齿会给出灰色像素，所以“亮度”就是天然的
/// 覆盖率。任何一步失败都返回中文错误，不会 panic；已分配的 GDI 资源由
/// `MemoryDc` / `SelectedFont` / `SelectedBitmap` 的 `Drop` 负责回收。
fn render_text_mask(text: &str, font_size: u32, letter_spacing: i32) -> anyhow::Result<Mask> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    anyhow::ensure!(!wide.is_empty(), "水印文字为空");

    // SAFETY: 所有句柄都由上面的 RAII 包装在离开作用域时释放；像素只在
    // `CreateDIBSection` 返回的缓冲区有效期内读取。
    unsafe {
        let dc = CreateCompatibleDC(None);
        anyhow::ensure!(!dc.is_invalid(), "创建内存 DC 失败（GDI 不可用）");
        let _dc_guard = MemoryDc(dc);

        let face: Vec<u16> = FONT_FACE.encode_utf16().chain(std::iter::once(0)).collect();
        let font = CreateFontW(
            -(font_size as i32), // 负值 = 字符高度（不含内部行距）
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            // 抗锯齿（而不是 ClearType），这样灰度就是干净的覆盖率。
            ANTIALIASED_QUALITY,
            u32::from(DEFAULT_PITCH.0) | u32::from(FF_DONTCARE.0),
            PCWSTR::from_raw(face.as_ptr()),
        );
        anyhow::ensure!(
            !font.is_invalid(),
            "创建字体失败（{FONT_FACE}，{font_size} 像素）"
        );
        let previous_font = SelectObject(dc, font.into());
        if previous_font.is_invalid() {
            let _ = DeleteObject(font.into());
            anyhow::bail!("选择水印字体失败（GDI）");
        }
        let _font_guard = SelectedFont {
            dc,
            font,
            previous: previous_font,
        };

        let mut extent = SIZE::default();
        anyhow::ensure!(
            GetTextExtentPoint32W(dc, &wide, &mut extent).as_bool(),
            "测量水印文字尺寸失败（GDI）"
        );
        anyhow::ensure!(
            extent.cx > 0 && extent.cy > 0,
            "水印文字尺寸为空（{text}）"
        );
        // `GetTextExtentPoint32W` 不计入后面 `SetTextCharacterExtra` 加的字距，
        // 直接拿它当位图宽度会把文字右端裁掉（曾把 "TEST" 画成 "TE"）。
        let advance = (letter_spacing.max(0) as u32).saturating_mul(wide.len() as u32);
        let width = (extent.cx as u32).saturating_add(advance).saturating_add(2);
        let height = extent.cy as u32;
        anyhow::ensure!(
            width <= MAX_SPRITE_EDGE
                && height <= MAX_SPRITE_EDGE
                && u64::from(width) * u64::from(height) <= MAX_SPRITE_PIXELS,
            "水印文字过大：{width}×{height}"
        );

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // 负高度 = 自顶向下，与帧的排列一致
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(err) => anyhow::bail!("创建水印位图失败：{err}"),
        };
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            anyhow::bail!("水印位图没有可写像素（GDI）");
        }
        let previous_bitmap = SelectObject(dc, bitmap.into());
        if previous_bitmap.is_invalid() {
            let _ = DeleteObject(bitmap.into());
            anyhow::bail!("选择水印位图失败（GDI）");
        }
        let _bitmap_guard = SelectedBitmap {
            dc,
            bitmap,
            previous: previous_bitmap,
        };

        let byte_len = (width as usize) * (height as usize) * 4;
        let surface = bits.cast::<u8>();
        // 明确铺黑：后面靠亮度反推覆盖率，背景必须确定是 0。
        std::ptr::write_bytes(surface, 0, byte_len);

        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));
        SetTextCharacterExtra(dc, letter_spacing);
        anyhow::ensure!(TextOutW(dc, 0, 0, &wide).as_bool(), "绘制水印文字失败（GDI）");
        // 读回像素前必须把 GDI 的绘制命令刷进 DIB。
        let _ = GdiFlush();

        let source = std::slice::from_raw_parts(surface, byte_len);
        let mut alpha = Vec::with_capacity((width as usize) * (height as usize));
        for pixel in source.chunks_exact(4) {
            // DIB 的字节序是 B、G、R、A；白字黑底下三通道亮度即覆盖率。
            let luminance =
                (u32::from(pixel[0]) * 28 + u32::from(pixel[1]) * 151 + u32::from(pixel[2]) * 77)
                    >> 8;
            alpha.push(luminance as u8);
        }

        Ok(Mask {
            width,
            height,
            alpha,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打开开关、完全不透明的一套设置，便于合成测试。
    fn active() -> WatermarkSettings {
        WatermarkSettings {
            enabled: true,
            opacity: 1.0,
            ..Default::default()
        }
    }

    /// 纯白、完全不透明的方块贴图。
    fn white_sprite(width: u32, height: u32) -> WatermarkSprite {
        let pixels = vec![255u8; (width as usize) * (height as usize) * 4];
        WatermarkSprite::from_rgba(width, height, pixels).expect("构造测试贴图")
    }

    #[test]
    fn static_anchors_place_the_sprite_in_each_corner() {
        let frame = (200u32, 100u32);
        let sprite = (40u32, 20u32);
        let at = |anchor: WatermarkAnchor| {
            let settings = WatermarkSettings {
                margin: 10,
                anchor,
                motion: WatermarkMotion::None,
                repeat: WatermarkRepeat::Off,
                ..Default::default()
            };
            placements(frame.0, frame.1, sprite, &settings, Duration::ZERO)
        };

        assert_eq!(at(WatermarkAnchor::TopLeft), vec![(10, 10)]);
        assert_eq!(at(WatermarkAnchor::TopRight), vec![(150, 10)]);
        assert_eq!(at(WatermarkAnchor::BottomLeft), vec![(10, 70)]);
        assert_eq!(at(WatermarkAnchor::BottomRight), vec![(150, 70)]);
        assert_eq!(at(WatermarkAnchor::Center), vec![(80, 40)]);
    }

    #[test]
    fn degenerate_frames_and_sprites_produce_no_placements() {
        let settings = active();
        assert!(placements(0, 480, (10, 10), &settings, Duration::ZERO).is_empty());
        assert!(placements(640, 0, (10, 10), &settings, Duration::ZERO).is_empty());
        assert!(placements(640, 480, (0, 10), &settings, Duration::ZERO).is_empty());
        assert!(placements(640, 480, (10, 0), &settings, Duration::ZERO).is_empty());
    }

    #[test]
    fn tiling_covers_the_whole_frame_without_escaping() {
        let frame = (128u32, 64u32);
        let sprite = (16u32, 8u32);
        let settings = WatermarkSettings {
            repeat: WatermarkRepeat::Tile,
            motion: WatermarkMotion::None,
            margin: 0,
            ..Default::default()
        };
        let list = placements(frame.0, frame.1, sprite, &settings, Duration::ZERO);
        assert!(!list.is_empty(), "平铺没有产生任何贴图");

        // 间距由 tile_gap 决定，贴图之间必须真的留出空隙（不重叠、也不无缝相接）。
        let gap = tile_gap(sprite.1, &settings) as i32;
        let stride_x = sprite.0 as i32 + gap;
        let stride_y = sprite.1 as i32 + gap;
        for (left, top) in &list {
            assert_eq!(left % stride_x, 0, "水平步距不一致：{left}");
            assert_eq!(top % stride_y, 0, "垂直步距不一致：{top}");
        }

        // 铺满：网格必须从画面左/上边缘之前起步，并且最后一个步距越过右/下边缘
        // （否则整帧最外圈会留下一条比间距还宽的空带）。
        let min_x = list.iter().map(|(left, _)| *left).min().unwrap_or(0);
        let max_x = list.iter().map(|(left, _)| *left).max().unwrap_or(0);
        let min_y = list.iter().map(|(_, top)| *top).min().unwrap_or(0);
        let max_y = list.iter().map(|(_, top)| *top).max().unwrap_or(0);
        assert!(min_x <= 0, "左侧没有铺到：{min_x}");
        assert!(min_y <= 0, "上方没有铺到：{min_y}");
        assert!(
            max_x + sprite.0 as i32 + gap >= frame.0 as i32,
            "右侧没有铺满"
        );
        assert!(
            max_y + sprite.1 as i32 + gap >= frame.1 as i32,
            "下方没有铺满"
        );

        // 每张图都必须还看得见，且不会偏离画面太远。
        for (left, top) in &list {
            assert!(*left < frame.0 as i32 && *left + sprite.0 as i32 > 0, "越界：{left}");
            assert!(*top < frame.1 as i32 && *top + sprite.1 as i32 > 0, "越界：{top}");
        }
    }

    #[test]
    fn tiling_uses_at_least_the_margin_as_the_gap() {
        let settings = WatermarkSettings {
            enabled: true,
            text: "wm".into(),
            opacity: 1.0,
            margin: 8,
            repeat: WatermarkRepeat::Tile,
            ..Default::default()
        };
        let positions = placements(800, 600, (100, 40), &settings, Duration::ZERO);
        assert!(positions.len() > 1, "平铺应铺出多个水印");
        // 相邻两列（同一行）之间的水平间距必须明显大于贴图宽度，否则文字会连成一片。
        let mut row: Vec<i32> = positions.iter().map(|(x, _)| *x).collect();
        row.sort_unstable();
        row.dedup();
        if row.len() > 1 {
            let stride = row[1] - row[0];
            assert!(
                stride >= 100 + 8,
                "平铺步距 {stride} 应不小于贴图宽度 + 边距"
            );
        }
    }

    #[test]
    fn ticker_animation_returns_to_the_same_place_after_one_period() {
        const SPEED: f32 = 64.0;
        let sprite = (16u32, 10u32);
        let settings = WatermarkSettings {
            repeat: WatermarkRepeat::Ticker,
            motion: WatermarkMotion::ScrollLeft,
            speed: SPEED,
            margin: 0,
            ..Default::default()
        };
        // 一个周期 = 一个步距（贴图宽 + 间距）走完的时间。
        let stride = sprite.0 as f64 + tile_gap(sprite.1, &settings);
        let period = Duration::from_secs_f64(stride / f64::from(SPEED));
        let start = placements(200, 50, sprite, &settings, Duration::ZERO);
        let half = placements(200, 50, sprite, &settings, period / 2);
        let closed = placements(200, 50, sprite, &settings, period);

        assert!(!start.is_empty());
        assert_eq!(start, closed, "跑马灯一个周期后没有回到原位");
        assert_ne!(start, half, "跑马灯半周期时位置不该与起点相同");
    }

    #[test]
    fn tiling_animation_is_also_periodic() {
        const SPEED: f32 = 40.0;
        let sprite = (20u32, 10u32);
        let settings = WatermarkSettings {
            repeat: WatermarkRepeat::Tile,
            motion: WatermarkMotion::ScrollUp,
            speed: SPEED,
            margin: 0,
            ..Default::default()
        };
        // 周期 = 垂直步距（贴图高 + 间距）/ 速度
        let stride = sprite.1 as f64 + tile_gap(sprite.1, &settings);
        let period = Duration::from_secs_f64(stride / f64::from(SPEED));
        let start = placements(120, 80, sprite, &settings, Duration::ZERO);
        let closed = placements(120, 80, sprite, &settings, period);
        assert_eq!(start, closed);
        assert!(placements(120, 80, sprite, &settings, Duration::from_millis(125)) != start);
    }

    #[test]
    fn scrolling_advances_monotonically() {
        let sprite = (20u32, 10u32);
        let left = WatermarkSettings {
            anchor: WatermarkAnchor::TopLeft,
            margin: 0,
            motion: WatermarkMotion::ScrollLeft,
            speed: 50.0,
            ..Default::default()
        };
        let at = |millis: u64| placements(400, 100, sprite, &left, Duration::from_millis(millis))[0];
        let first = at(0);
        let second = at(200);
        let third = at(400);
        assert_eq!(first, (0, 0));
        assert_eq!(second, (-10, 0)); // 50 px/s × 0.2s
        assert_eq!(third, (-20, 0));
        assert!(second.0 < first.0 && third.0 < second.0);

        let down = WatermarkSettings {
            anchor: WatermarkAnchor::TopLeft,
            margin: 0,
            motion: WatermarkMotion::ScrollDown,
            speed: 50.0,
            ..Default::default()
        };
        let y = |millis: u64| placements(400, 400, sprite, &down, Duration::from_millis(millis))[0];
        assert!(y(200).1 > y(0).1 && y(400).1 > y(200).1);
        assert_eq!(y(200).1, 10);
    }

    #[test]
    fn diagonal_motion_wraps_instead_of_escaping() {
        let sprite = (40u32, 20u32);
        let frame = (320u32, 180u32);
        let settings = WatermarkSettings {
            anchor: WatermarkAnchor::TopLeft,
            margin: 0,
            motion: WatermarkMotion::Diagonal,
            speed: 120.0,
            ..Default::default()
        };

        let mut saw_negative = false;
        for step in 0..40u64 {
            let elapsed = Duration::from_millis(step * 100);
            let list = placements(frame.0, frame.1, sprite, &settings, elapsed);
            assert_eq!(list.len(), 1);
            let (x, y) = list[0];
            // 周期 = 画面 + 贴图，所以坐标不会跑出这个范围。
            assert!(x > -(frame.0 as i32 + sprite.0 as i32), "x 逃得太远：{x}");
            assert!(y > -(frame.1 as i32 + sprite.1 as i32), "y 逃得太远：{y}");
            assert!(x <= 0 && y <= 0, "斜向滚动应当往左上走：({x}, {y})");
            saw_negative |= x < 0 && y < 0;
        }
        assert!(saw_negative, "斜向滚动没有产生位移");
    }

    #[test]
    fn bounce_stays_inside_the_frame() {
        let sprite = (40u32, 20u32);
        let frame = (200u32, 120u32);
        let settings = WatermarkSettings {
            anchor: WatermarkAnchor::TopLeft,
            margin: 0,
            motion: WatermarkMotion::Bounce,
            speed: 90.0,
            ..Default::default()
        };
        let max_x = frame.0 as i32 - sprite.0 as i32;
        let max_y = frame.1 as i32 - sprite.1 as i32;
        let mut lowest = i32::MAX;
        let mut highest = i32::MIN;

        for step in 0..200u64 {
            let elapsed = Duration::from_millis(step * 37);
            let list = placements(frame.0, frame.1, sprite, &settings, elapsed);
            assert_eq!(list.len(), 1);
            let (x, y) = list[0];
            assert!((0..=max_x).contains(&x), "x 跑出画面：{x}");
            assert!((0..=max_y).contains(&y), "y 跑出画面：{y}");
            lowest = lowest.min(x);
            highest = highest.max(x);
        }
        // 确实来回走过，而不是卡在起点。
        assert!(lowest == 0, "反弹没有碰到左边界：{lowest}");
        assert!(highest == max_x, "反弹没有碰到右边界：{highest}");
    }

    #[test]
    fn placements_is_a_pure_function() {
        let settings = WatermarkSettings {
            repeat: WatermarkRepeat::Tile,
            motion: WatermarkMotion::Diagonal,
            speed: 33.0,
            ..Default::default()
        };
        let elapsed = Duration::from_millis(1_234);
        let first = placements(640, 360, (30, 15), &settings, elapsed);
        let second = placements(640, 360, (30, 15), &settings, elapsed);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn static_motion_ignores_time() {
        let settings = WatermarkSettings {
            margin: 8,
            ..Default::default()
        };
        let first = placements(640, 480, (50, 20), &settings, Duration::ZERO);
        let later = placements(640, 480, (50, 20), &settings, Duration::from_secs(3600));
        assert_eq!(first, later);
    }

    #[test]
    fn composite_rejects_a_frame_of_the_wrong_size() {
        let sprite = white_sprite(4, 4);
        let settings = active();
        let mut frame = vec![0u8; 8];
        let error = composite(&mut frame, 4, 4, &sprite, &settings, Duration::ZERO)
            .expect_err("长度不足应当报错");
        assert!(error.to_string().contains("长度"), "{error}");

        let mut frame = vec![0u8; 64];
        let error = composite(&mut frame, 0, 4, &sprite, &settings, Duration::ZERO)
            .expect_err("零尺寸应当报错");
        assert!(error.to_string().contains("尺寸"), "{error}");
    }

    #[test]
    fn zero_opacity_or_disabled_never_touches_the_frame() {
        let sprite = white_sprite(8, 8);
        let frame_size = (32u32, 24u32);
        let mut frame = vec![17u8; (frame_size.0 * frame_size.1 * 4) as usize];
        let before = frame.clone();

        let transparent = WatermarkSettings {
            opacity: 0.0,
            ..active()
        };
        composite(&mut frame, frame_size.0, frame_size.1, &sprite, &transparent, Duration::ZERO)
            .expect("合成透明水印");
        assert_eq!(frame, before, "不透明度为 0 时改动了帧");

        let disabled = WatermarkSettings {
            enabled: false,
            ..active()
        };
        composite(&mut frame, frame_size.0, frame_size.1, &sprite, &disabled, Duration::ZERO)
            .expect("合成已关闭的水印");
        assert_eq!(frame, before, "关闭时改动了帧");
    }

    #[test]
    fn composite_draws_inside_the_frame_and_clips_outside() {
        let frame_size = (64u32, 64u32);
        let pixels = (frame_size.0 * frame_size.1 * 4) as usize;
        let canary: u8 = 0xAB;
        // 尾部多留 8 字节当哨兵，用来抓越界写。
        let mut frame = vec![0u8; pixels + 8];
        for byte in &mut frame[pixels..] {
            *byte = canary;
        }

        let sprite = white_sprite(16, 16);
        // 负边距把贴图推到左上角之外（-6,-6），一部分在画面外。
        let settings = WatermarkSettings {
            margin: -6,
            anchor: WatermarkAnchor::TopLeft,
            ..active()
        };
        composite(&mut frame, frame_size.0, frame_size.1, &sprite, &settings, Duration::ZERO)
            .expect("合成水印");

        // 画面内被覆盖到的 (0,0) 确实画上了白色（alpha 通道保持原样）。
        assert_eq!(&frame[..3], &[255, 255, 255]);
        assert_eq!(frame[3], 0, "不应改写帧的 alpha 通道");

        // 完全没被覆盖的右下角保持原样。
        let untouched = ((60 * frame_size.0 + 60) * 4) as usize;
        assert_eq!(&frame[untouched..untouched + 4], &[0, 0, 0, 0]);

        // 哨兵没有被越界写坏。
        assert!(
            frame[pixels..].iter().all(|byte| *byte == canary),
            "合成时写到了缓冲区之外"
        );
    }

    #[test]
    fn composite_blends_half_transparent_pixels() {
        let frame_size = (16u32, 16u32);
        let mut frame = vec![0u8; (frame_size.0 * frame_size.1 * 4) as usize];
        let pixels = vec![255u8; 4 * 4 * 4];
        let mut sprite = WatermarkSprite::from_rgba(4, 4, pixels).expect("构造贴图");
        // 把 alpha 改成一半，验证混合公式（而不是直接覆盖）。
        for pixel in sprite.pixels.chunks_exact_mut(4) {
            pixel[3] = 128;
        }
        let settings = WatermarkSettings {
            margin: 0,
            anchor: WatermarkAnchor::TopLeft,
            ..active()
        };
        composite(&mut frame, frame_size.0, frame_size.1, &sprite, &settings, Duration::ZERO)
            .expect("合成水印");
        // 255 × 128 / 255 ≈ 128
        let value = frame[0];
        assert!((126..=129).contains(&value), "半透明混合结果异常：{value}");
    }

    #[test]
    fn sprite_construction_validates_the_length() {
        assert!(WatermarkSprite::from_rgba(2, 2, vec![0u8; 16]).is_ok());
        assert!(WatermarkSprite::from_rgba(2, 2, vec![0u8; 15]).is_err());
        assert!(WatermarkSprite::from_rgba(2, 2, vec![0u8; 17]).is_err());
        assert!(WatermarkSprite::from_rgba(0, 0, Vec::new()).is_ok());
        assert!(WatermarkSprite::from_rgba(0, 0, Vec::new())
            .expect("空贴图")
            .is_empty());
    }

    #[test]
    fn pattern_only_sprites_are_generated_without_gdi() {
        for pattern in [
            WatermarkPattern::Dots,
            WatermarkPattern::Grid,
            WatermarkPattern::Logo,
        ] {
            let settings = WatermarkSettings {
                text: String::new(),
                pattern,
                ..Default::default()
            };
            let sprite = render_sprite(&settings).expect("图案贴图应当能生成");
            assert!(sprite.width() > 0 && sprite.height() > 0, "{pattern:?}");
            assert_eq!(
                sprite.pixels().len(),
                (sprite.width() * sprite.height() * 4) as usize,
                "{pattern:?}"
            );
            assert!(
                sprite.pixels().chunks_exact(4).any(|pixel| pixel[3] > 0),
                "{pattern:?} 图案完全没有像素"
            );
        }
    }

    #[test]
    fn empty_content_is_rejected() {
        let settings = WatermarkSettings {
            text: "   ".to_string(),
            pattern: WatermarkPattern::None,
            ..Default::default()
        };
        let error = render_sprite(&settings).expect_err("空内容应当报错");
        assert!(error.to_string().contains("水印"), "{error}");
    }

    #[test]
    fn text_sprite_renders_mixed_chinese_and_latin() {
        let settings = WatermarkSettings {
            enabled: true,
            text: "ScreenRec 录屏 2026".to_string(),
            pattern: WatermarkPattern::None,
            ..Default::default()
        };

        match render_sprite(&settings) {
            Ok(sprite) => {
                assert!(sprite.width() > 0 && sprite.height() > 0);
                assert_eq!(
                    sprite.pixels().len(),
                    (sprite.width() * sprite.height() * 4) as usize
                );
                // sprite 高度应当与字号同量级（留了少量内边距）。
                assert!(
                    sprite.height() >= settings.font_size,
                    "贴图高度 {} 小于字号 {}",
                    sprite.height(),
                    settings.font_size
                );
                assert!(
                    sprite.pixels().chunks_exact(4).any(|pixel| pixel[3] > 0),
                    "渲染出的文字贴图是全透明的"
                );

                // 确认真的“画了字”而不是一块实心矩形：既要有完全透明的背景，
                // 也要有抗锯齿产生的一堆中间灰度。
                let mut levels = std::collections::BTreeSet::new();
                for pixel in sprite.pixels().chunks_exact(4) {
                    levels.insert(pixel[3]);
                }
                assert!(
                    levels.contains(&0),
                    "文字贴图没有透明背景，像是没画上字"
                );
                assert!(
                    levels.len() > 3,
                    "文字贴图只有 {} 种 alpha，边缘没有抗锯齿",
                    levels.len()
                );
            }
            Err(error) => {
                // 无桌面的 CI / 服务会话里 GDI 可能不可用：跳过断言并说明原因。
                eprintln!("跳过文字渲染断言：GDI 不可用（{error}）");
            }
        }
    }

    #[test]
    fn text_and_pattern_are_combined() {
        let settings = WatermarkSettings {
            text: "ScreenRec".to_string(),
            pattern: WatermarkPattern::Dots,
            ..Default::default()
        };
        match render_sprite(&settings) {
            Ok(sprite) => {
                // 有文字时画布会比单独的图案瓦片大。
                let tile = render_sprite(&WatermarkSettings {
                    text: String::new(),
                    pattern: WatermarkPattern::Dots,
                    ..Default::default()
                })
                .expect("图案贴图");
                assert!(sprite.width() > tile.width());
                assert!(sprite.pixels().chunks_exact(4).any(|pixel| pixel[3] > 0));
            }
            Err(error) => eprintln!("跳过文字叠加断言：GDI 不可用（{error}）"),
        }
    }

    #[test]
    fn settings_survive_a_json_round_trip_and_tolerate_old_configs() {
        let settings = WatermarkSettings {
            enabled: true,
            text: "ScreenRec 录屏".to_string(),
            pattern: WatermarkPattern::Dots,
            font_size: 48,
            color: [12, 200, 90],
            opacity: 0.5,
            margin: -8,
            anchor: WatermarkAnchor::Center,
            motion: WatermarkMotion::Diagonal,
            speed: 42.0,
            repeat: WatermarkRepeat::Tile,
        };
        let json = serde_json::to_string(&settings).expect("序列化水印设置");
        let parsed: WatermarkSettings = serde_json::from_str(&json).expect("反序列化水印设置");
        assert_eq!(parsed, settings);

        // 旧配置只有少数字段时，其余字段回落到 Default。
        let partial: WatermarkSettings =
            serde_json::from_str(r#"{"enabled":true,"text":"旧配置"}"#).expect("读取旧配置");
        let default = WatermarkSettings::default();
        assert!(partial.enabled);
        assert_eq!(partial.text, "旧配置");
        assert_eq!(partial.font_size, default.font_size);
        assert_eq!(partial.anchor, default.anchor);
        assert_eq!(partial.repeat, default.repeat);
    }

    #[test]
    fn ids_round_trip_and_accept_both_spellings() {
        for pattern in WatermarkPattern::ALL {
            assert_eq!(WatermarkPattern::from_id(pattern.id()), Some(pattern));
            assert_eq!(
                WatermarkPattern::from_id(&pattern.id().to_ascii_uppercase()),
                Some(pattern)
            );
        }
        for anchor in WatermarkAnchor::ALL {
            assert_eq!(WatermarkAnchor::from_id(anchor.id()), Some(anchor));
        }
        for motion in WatermarkMotion::ALL {
            assert_eq!(WatermarkMotion::from_id(motion.id()), Some(motion));
        }
        for repeat in WatermarkRepeat::ALL {
            assert_eq!(WatermarkRepeat::from_id(repeat.id()), Some(repeat));
        }

        // CLI 的 kebab-case 与配置里的 snake_case 都要能认。
        assert_eq!(
            WatermarkMotion::from_id("scroll-left"),
            Some(WatermarkMotion::ScrollLeft)
        );
        assert_eq!(
            WatermarkMotion::from_id("scroll_left"),
            Some(WatermarkMotion::ScrollLeft)
        );
        assert_eq!(
            WatermarkMotion::from_id("  Scroll-Left "),
            Some(WatermarkMotion::ScrollLeft)
        );
        assert_eq!(
            WatermarkAnchor::from_id("bottom-right"),
            Some(WatermarkAnchor::BottomRight)
        );
        assert_eq!(
            WatermarkPattern::from_id("DOTS"),
            Some(WatermarkPattern::Dots)
        );
        assert_eq!(
            WatermarkRepeat::from_id("ticker"),
            Some(WatermarkRepeat::Ticker)
        );

        // 未知值给 None，由调用方给出中文错误。
        assert_eq!(WatermarkPattern::from_id("star"), None);
        assert_eq!(WatermarkMotion::from_id(""), None);
        assert_eq!(WatermarkRepeat::from_id("滚动"), None);
        assert_eq!(WatermarkAnchor::from_id("middle"), None);

        // 这些 id 是 CLI `--watermark-*` 的公开契约，不要随意改名。
        assert_eq!(WatermarkPattern::None.id(), "none");
        assert_eq!(WatermarkMotion::ScrollDown.id(), "scroll-down");
        assert_eq!(WatermarkAnchor::TopLeft.id(), "top-left");
        assert_eq!(WatermarkRepeat::Off.id(), "off");
    }

    #[test]
    fn inactive_settings_are_detected() {
        assert!(!WatermarkSettings::default().is_active());
        assert!(active().is_active());
        assert!(!WatermarkSettings {
            text: "  ".to_string(),
            ..active()
        }
        .is_active());
        assert!(WatermarkSettings {
            text: String::new(),
            pattern: WatermarkPattern::Grid,
            ..active()
        }
        .is_active());
    }
}
