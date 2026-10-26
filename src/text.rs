//! 显示宽度、补齐、截断 —— 报告与机器人共用一套。
//!
//! ⚠️ 补空格不能按 `chars().count()`：中文与全角在等宽字体里占**两格**，
//! 按字符数补，「阿里云-上海」与 `ucloud_usa` 就会错位。

/// 一行的显示宽度：CJK / 全角字符在等宽字体里占**两格** ✓。
/// 只按 `chars().count()` 补空格，中文名与英文名就会错位。
pub fn width(s: &str) -> usize {
    s.chars().map(|c| if wide(c) { 2 } else { 1 }).sum()
}

pub fn wide(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
    )
}

/// 左补空格：数字按显示宽度**右对齐**，一列数字才会对齐小数点前的位。
pub fn blank_left(s: &str, w: usize) -> String {
    let mut out = String::new();
    for _ in width(s)..w {
        out.push(' ');
    }
    out.push_str(s);
    out
}

/// 截到 `w` 列（含省略号）。长机器名在手机上会把整行挤到换行，换行之后对齐就废了。
/// 右补空格到 `w` 列：左对齐的列（名字）用它。
pub fn blank_to(s: &str, w: usize) -> String {
    let mut out = s.to_string();
    for _ in width(s)..w {
        out.push(' ');
    }
    out
}

pub fn clip(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = if wide(c) { 2 } else { 1 };
        if used + cw > w.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}
