//! Human readable formatting shared by the UI, the CLI and the history list.

/// Formats a duration as `mm:ss`, or `h:mm:ss` past an hour.
pub fn format_duration(duration: std::time::Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Formats a byte count with a binary unit suffix.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Terminal columns a string occupies.
///
/// CJK and other full-width characters take two columns, which is why a naive
/// `{:<20}` produces ragged tables for Chinese text.
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Terminal columns one character occupies.
pub fn char_width(character: char) -> usize {
    if is_wide(character) {
        2
    } else {
        1
    }
}

/// Pads a string to `width` terminal columns (never truncates).
pub fn pad_display(text: &str, width: usize) -> String {
    let current = display_width(text);
    let mut padded = text.to_string();
    if current < width {
        padded.push_str(&" ".repeat(width - current));
    }
    padded
}

/// True for characters that occupy two terminal columns.
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0x303E      // CJK radicals, Kangxi, punctuation
        | 0x3041..=0x33FF      // Kana, CJK compatibility
        | 0x3400..=0x4DBF      // CJK extension A
        | 0x4E00..=0x9FFF      // CJK unified ideographs
        | 0xA000..=0xA4CF      // Yi
        | 0xAC00..=0xD7A3      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFE30..=0xFE6F      // CJK compatibility forms
        | 0xFF00..=0xFF60      // Full-width forms
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F    // emoji
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD    // CJK extensions B+
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn durations_are_formatted() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00");
        assert_eq!(format_duration(Duration::from_secs(5)), "00:05");
        assert_eq!(format_duration(Duration::from_secs(75)), "01:15");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn sizes_are_formatted() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.00 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.00 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.00 GB");
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("时间"), 4);
        // "MP4" (3) + space (1) + "视频" (4)
        assert_eq!(display_width("MP4 视频"), 8);
        assert_eq!(display_width(""), 0);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('汉'), 2);
    }

    #[test]
    fn padding_aligns_chinese_and_latin_columns() {
        assert_eq!(pad_display("时间", 8), "时间    ");
        assert_eq!(pad_display("time", 8), "time    ");
        assert_eq!(display_width(&pad_display("时间", 8)), 8);
        assert_eq!(display_width(&pad_display("time", 8)), 8);

        // Longer than the target: left untouched rather than truncated.
        assert_eq!(pad_display("很长的标题", 4), "很长的标题");
    }
}
