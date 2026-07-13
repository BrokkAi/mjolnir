use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn truncate_text_to_width(line: String, width: u16) -> String {
    let cap = width as usize;
    if line.width() <= cap {
        return line;
    }
    if cap > 3 {
        let mut out = String::new();
        let mut current_width = 0;
        let ellipsis_width = 3; // ASCII "..."
        let target = cap.saturating_sub(ellipsis_width);
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if current_width + w > target {
                break;
            }
            out.push(ch);
            current_width += w;
        }
        out.push_str("...");
        out
    } else {
        let mut out = String::new();
        let mut current_width = 0;
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if current_width + w > cap {
                break;
            }
            out.push(ch);
            current_width += w;
        }
        out
    }
}
