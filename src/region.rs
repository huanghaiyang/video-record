//! Rectangles for area recording.
//!
//! A [`Region`] is expressed in physical pixels relative to the top-left corner
//! of the **captured source** (the monitor or window being recorded), which is
//! also the frame the crop is applied to. Keeping it source-relative means the
//! geometry does not depend on where that source sits on the virtual desktop.
//!
//! The type lives outside `capture` so that the encoder helpers can crop
//! without depending on the capture module.

use serde::{Deserialize, Serialize};

/// A rectangular part of a captured frame, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// Smallest region that is still worth encoding.
    pub const MIN_SIZE: u32 = 16;

    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Region covering a whole frame of the given size.
    pub fn full(width: u32, height: u32) -> Self {
        Self { x: 0, y: 0, width, height }
    }

    pub fn right(&self) -> u32 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(&self) -> u32 {
        self.y.saturating_add(self.height)
    }

    pub fn is_valid(&self) -> bool {
        self.width >= Self::MIN_SIZE && self.height >= Self::MIN_SIZE
    }

    /// True when the region covers the whole frame, so no work is needed.
    pub fn covers(&self, width: u32, height: u32) -> bool {
        self.x == 0 && self.y == 0 && self.width >= width && self.height >= height
    }

    /// Fits the region inside a frame of `max_width` x `max_height`.
    ///
    /// The size is preserved where possible; the origin moves back so a region
    /// that was dragged past the edge still lands inside the frame.
    pub fn clamp_to(&self, max_width: u32, max_height: u32) -> Self {
        if max_width < Self::MIN_SIZE || max_height < Self::MIN_SIZE {
            // Degenerate source: keep it usable rather than panicking.
            return Self {
                x: 0,
                y: 0,
                width: max_width.max(1),
                height: max_height.max(1),
            };
        }

        let width = self.width.clamp(Self::MIN_SIZE, max_width);
        let height = self.height.clamp(Self::MIN_SIZE, max_height);
        let x = self.x.min(max_width - width);
        let y = self.y.min(max_height - height);

        Self { x, y, width, height }
    }

    /// `800×600 @ (100, 200)`
    pub fn label(&self) -> String {
        format!("{}×{} @ ({}, {})", self.width, self.height, self.x, self.y)
    }

    /// `100,200,800,600`
    pub fn to_coords(&self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }

    /// Parses `x,y,w,h` or the `WxH+X+Y` form used by FFmpeg and many tools.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("区域不能为空，请填写 x,y,宽,高".to_string());
        }

        if let Some(region) = Self::parse_wxh(text) {
            return region;
        }
        Self::parse_commas(text)
    }

    fn parse_wxh(text: &str) -> Option<Result<Self, String>> {
        let (size, offsets) = text.split_once('+')?;
        let (width, height) = size.split_once(['x', 'X'])?;
        let width: u32 = width.trim().parse().ok()?;
        let height: u32 = height.trim().parse().ok()?;

        let mut parts = offsets.split('+');
        let x: u32 = parts.next()?.trim().parse().ok()?;
        let y: u32 = parts.next()?.trim().parse().ok()?;
        if parts.next().is_some() {
            return Some(Err(format!("区域格式无法识别：{text}")));
        }

        Some(Self::validated(x, y, width, height, text))
    }

    fn parse_commas(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split(',').map(str::trim).collect();
        if parts.len() != 4 {
            return Err(format!(
                "区域格式无法识别：{text}（可用 100,200,800,600 或 800x600+100+200）"
            ));
        }

        let mut numbers = [0u32; 4];
        for (index, part) in parts.iter().enumerate() {
            numbers[index] = part
                .parse()
                .map_err(|_| format!("区域第 {} 个数值无效：{part}", index + 1))?;
        }

        Self::validated(numbers[0], numbers[1], numbers[2], numbers[3], text)
    }

    fn validated(x: u32, y: u32, width: u32, height: u32, source: &str) -> Result<Self, String> {
        let region = Self { x, y, width, height };
        if !region.is_valid() {
            return Err(format!(
                "区域太小：{}（宽高至少 {} 像素）",
                source,
                Self::MIN_SIZE
            ));
        }
        Ok(region)
    }
}

impl std::fmt::Display for Region {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.to_coords())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_parse_from_both_common_notations() {
        let expected = Region::new(100, 200, 800, 600);
        assert_eq!(Region::parse("100,200,800,600").unwrap(), expected);
        assert_eq!(Region::parse(" 100 , 200 , 800 , 600 ").unwrap(), expected);
        assert_eq!(Region::parse("800x600+100+200").unwrap(), expected);
        assert_eq!(Region::parse("800X600+100+200").unwrap(), expected);
    }

    #[test]
    fn invalid_regions_are_rejected_with_a_helpful_message() {
        for text in ["", "abc", "1,2,3", "1,2,3,4,5", "a,2,3,4", "100,200,x,600"] {
            let error = Region::parse(text).expect_err(text);
            assert!(!error.is_empty(), "empty error for {text}");
        }

        // Below the minimum size.
        let error = Region::parse("10,10,8,8").unwrap_err();
        assert!(error.contains("太小"), "{error}");

        // Well-formed but with a trailing offset must not be silently accepted.
        assert!(Region::parse("800x600+100+200+300").is_err());
    }

    #[test]
    fn labels_and_coordinates_round_trip() {
        let region = Region::new(16, 32, 640, 480);
        assert_eq!(region.label(), "640×480 @ (16, 32)");
        assert_eq!(region.to_coords(), "16,32,640,480");
        assert_eq!(Region::parse(&region.to_string()).unwrap(), region);
        assert_eq!(region.right(), 656);
        assert_eq!(region.bottom(), 512);
    }

    #[test]
    fn clamping_keeps_a_region_inside_the_frame() {
        let frame = (1920, 1080);

        // Already inside: untouched.
        let region = Region::new(100, 100, 800, 600);
        assert_eq!(region.clamp_to(frame.0, frame.1), region);

        // Hanging off the right/bottom edge: the size is kept, the origin moves back.
        let region = Region::new(1900, 1070, 320, 240);
        let clamped = region.clamp_to(frame.0, frame.1);
        assert_eq!(clamped, Region::new(1600, 840, 320, 240));
        assert!(clamped.right() <= frame.0 && clamped.bottom() <= frame.1);

        // Larger than the frame: shrunk to the frame.
        let huge = Region::new(0, 0, 5000, 5000).clamp_to(frame.0, frame.1);
        assert_eq!(huge, Region::full(1920, 1080));

        // Completely outside: pulled back inside.
        let outside = Region::new(9000, 9000, 400, 300).clamp_to(frame.0, frame.1);
        assert!(outside.right() <= frame.0 && outside.bottom() <= frame.1);
    }

    #[test]
    fn clamping_never_produces_an_invalid_region() {
        for (width, height) in [(0u32, 0u32), (8, 8), (16, 16), (1920, 1080)] {
            let region = Region::new(5000, 5000, 4, 4).clamp_to(width, height);
            assert!(region.width >= 1 && region.height >= 1);
            assert!(region.right() <= width.max(1) || width < Region::MIN_SIZE);
        }
    }

    #[test]
    fn a_full_frame_region_is_recognised() {
        assert!(Region::full(1920, 1080).covers(1920, 1080));
        assert!(Region::new(0, 0, 1920, 1080).covers(1920, 1080));
        assert!(!Region::new(1, 0, 1919, 1080).covers(1920, 1080));
        assert!(!Region::full(1920, 1080).covers(2560, 1440));
    }

    #[test]
    fn regions_survive_a_json_round_trip() {
        let region = Region::new(10, 20, 640, 360);
        let text = serde_json::to_string(&region).unwrap();
        assert_eq!(serde_json::from_str::<Region>(&text).unwrap(), region);
    }
}
