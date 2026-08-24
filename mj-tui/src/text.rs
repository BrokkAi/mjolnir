use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// First line of `text`, hard-capped at `max` characters with an ellipsis.
pub(crate) fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

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

#[cfg(test)]
mod tests {
    use super::{first_line, truncate_text_to_width};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn text_that_fits_is_preserved() {
        let line = "界abc".to_string();

        assert_eq!(truncate_text_to_width(line.clone(), 5), line);
    }

    #[test]
    fn truncation_reserves_room_for_ellipsis() {
        assert_eq!(truncate_text_to_width("abcdef".to_string(), 5), "ab...");
        assert_eq!(truncate_text_to_width("界abcd".to_string(), 5), "界...");
    }

    #[test]
    fn narrow_widths_truncate_without_ellipsis() {
        assert_eq!(truncate_text_to_width("abcdef".to_string(), 3), "abc");
        assert_eq!(truncate_text_to_width("界abc".to_string(), 1), "");
        assert_eq!(truncate_text_to_width("abcdef".to_string(), 0), "");
    }

    #[test]
    fn truncation_preserves_zero_width_characters() {
        let truncated = truncate_text_to_width("e\u{301}xyzz".to_string(), 4);

        assert_eq!(truncated, "e\u{301}...");
        assert_eq!(truncated.width(), 4);
    }

    #[test]
    fn first_line_truncates_at_character_boundaries() {
        assert_eq!(first_line("hello\nworld", 60), "hello");
        assert_eq!(first_line("", 60), "");
        assert_eq!(first_line("éééé", 3), "éé…");
    }
}
