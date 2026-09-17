//! Word-wrapped text layout shared by multiline controls and the composer.

use std::borrow::Cow;
use std::collections::VecDeque;

use ratatui::Frame;
use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputGrapheme {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) width: usize,
    whitespace: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub(crate) start: usize,
    pub(crate) graphemes: Vec<InputGrapheme>,
}

/// How [`truncate_to_cells`] treats the text it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncate {
    /// Fold every run of whitespace, newlines included, into one space before
    /// measuring, so a multi-line value reads as one row.
    pub collapse_whitespace: bool,
    /// Drop trailing whitespace and punctuation before the ellipsis, so a cut
    /// in the middle of a phrase does not read as `alpha,…`.
    pub trim_punctuation: bool,
}

impl Truncate {
    /// Cut the text where it runs out of room and keep everything before it.
    pub const PLAIN: Self = Self {
        collapse_whitespace: false,
        trim_punctuation: false,
    };
    /// Fold the value onto one row and cut it where a reader would: no
    /// dangling comma, dash, or space in front of the ellipsis.
    pub const SUMMARY: Self = Self {
        collapse_whitespace: true,
        trim_punctuation: true,
    };
}

/// The characters a cut swallows before its ellipsis: whitespace, ASCII
/// punctuation, and the typographic marks that read as punctuation.
pub(crate) fn trim_before_ellipsis(character: char) -> bool {
    character.is_whitespace()
        || character.is_ascii_punctuation()
        || matches!(
            character,
            '…' | '–' | '—' | '‘' | '’' | '“' | '”' | '•' | '·'
        )
}

/// Shorten `text` to at most `width` terminal cells, marking a cut with a
/// trailing ellipsis.
///
/// Width is measured in display cells, so a full-width glyph costs two. Text
/// that already fits comes back unchanged apart from the requested whitespace
/// collapsing. A `width` of zero yields an empty string and a `width` of one
/// yields the ellipsis alone, because nothing else fits beside it.
pub fn truncate_to_cells(text: &str, width: usize, options: Truncate) -> String {
    let text = if options.collapse_whitespace {
        Cow::Owned(text.split_whitespace().collect::<Vec<_>>().join(" "))
    } else {
        Cow::Borrowed(text)
    };
    if text.width() <= width {
        return text.into_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    // One cell is reserved for the ellipsis itself.
    let budget = width - 1;
    let mut kept = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells > budget {
            break;
        }
        used += cells;
        kept.push(character);
    }
    if options.trim_punctuation {
        kept.truncate(kept.trim_end_matches(trim_before_ellipsis).len());
    }
    kept.push('…');
    kept
}

/// Return the wrapped rows and the visual cursor position for a text value.
///
/// The wrapping policy mirrors ratatui's `WordWrapper` with `trim = false`,
/// which is the policy used by the composer and multiline text fields.
pub(crate) fn multiline_rows(
    input: &str,
    cursor: usize,
    width: usize,
) -> (Vec<VisualRow>, (usize, usize)) {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line_offset = 0;
    for line in input.split('\n') {
        rows.extend(
            wrap_input_line(line, line_offset, width)
                .into_iter()
                .map(|graphemes| VisualRow {
                    start: graphemes
                        .first()
                        .map_or(line_offset, |grapheme| grapheme.start),
                    graphemes,
                }),
        );
        line_offset = line_offset.saturating_add(line.len()).saturating_add(1);
    }
    (rows, input_cursor_visual_position(input, cursor, width))
}

pub(crate) fn wrapped_row_for_grapheme_offset(
    line: &str,
    width: usize,
    grapheme_offset: usize,
) -> usize {
    let wrapped = wrap_input_line_with_trim(line, 0, width.max(1), true);
    let grapheme_byte = line
        .grapheme_indices(true)
        .nth(grapheme_offset)
        .map_or(line.len(), |(offset, _)| offset);
    for (row, graphemes) in wrapped.iter().enumerate() {
        let last = graphemes
            .last()
            .map_or(grapheme_byte, |grapheme| grapheme.end);
        if grapheme_byte < last {
            return row;
        }
        // `last` is an exclusive source offset. When a hard wrap starts at
        // that exact offset, the anchor belongs to the following row; using
        // `<=` here moved it back one visual row on every resize.
        if grapheme_byte == last
            && wrapped
                .get(row + 1)
                .and_then(|next| next.first())
                .is_none_or(|next| next.start != grapheme_byte)
        {
            return row;
        }
    }
    wrapped.len().saturating_sub(1)
}

pub(crate) fn grapheme_offset_for_wrapped_row(line: &str, width: usize, row: usize) -> usize {
    let wrapped = wrap_input_line_with_trim(line, 0, width.max(1), true);
    let Some(graphemes) = wrapped.get(row) else {
        return line.graphemes(true).count();
    };
    graphemes.first().map_or_else(
        || line.graphemes(true).count(),
        |grapheme| line[..grapheme.start].graphemes(true).count(),
    )
}

pub fn input_cursor_visual_position(input: &str, cursor: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let cursor = cursor.min(input.len());
    let mut line_offset = 0;
    let mut row_offset = 0;

    for line in input.split('\n') {
        let line_end = line_offset + line.len();
        let wrapped = wrap_input_line(line, line_offset, width);
        if cursor <= line_end {
            let mut previous = (0, row_offset);
            for (wrapped_row, graphemes) in wrapped.iter().enumerate() {
                let row = row_offset + wrapped_row;
                let mut column = 0;
                for grapheme in graphemes {
                    let start = (column, row);
                    if cursor <= grapheme.start {
                        return start;
                    }
                    column += grapheme.width;
                    let end = if column >= width {
                        (0, row + 1)
                    } else {
                        (column, row)
                    };
                    if cursor <= grapheme.end {
                        return end;
                    }
                    previous = end;
                }
            }
            return previous;
        }
        row_offset += wrapped.len();
        line_offset = line_end + 1;
    }

    (0, row_offset)
}

/// Mirror ratatui's `WordWrapper` layout so the terminal cursor follows the
/// word-wrapped `Paragraph`, including whitespace discarded at wrap points.
fn wrap_input_line(line: &str, offset: usize, width: usize) -> Vec<Vec<InputGrapheme>> {
    wrap_input_line_with_trim(line, offset, width, false)
}

/// Word-wrap one input line with the same trim option as ratatui's
/// `Paragraph`. Composer input uses `trim = false`; question messages use
/// `trim = true`, which is why both callers share this implementation instead
/// of trying to infer one policy from the other.
pub(crate) fn wrap_input_line_with_trim(
    line: &str,
    offset: usize,
    width: usize,
    trim: bool,
) -> Vec<Vec<InputGrapheme>> {
    let mut wrapped = Vec::new();
    let mut pending_line = Vec::new();
    let mut pending_word = Vec::new();
    let mut pending_whitespace = VecDeque::<InputGrapheme>::new();
    let mut line_width = 0;
    let mut word_width = 0;
    let mut whitespace_width = 0;
    let mut previous_was_non_whitespace = false;

    for (start, symbol) in line.grapheme_indices(true) {
        let symbol_width = display_width(symbol);
        if symbol_width > width {
            continue;
        }
        // Ratatui filters control graphemes before wrapping and rendering.
        // Keep them as zero-width map anchors so their byte offsets survive
        // mouse editing, while excluding them from word-boundary decisions.
        let control = symbol.contains(char::is_control);
        let whitespace = !control
            && (symbol == "\u{200b}"
                || (symbol.chars().all(char::is_whitespace) && symbol != "\u{00a0}"));
        let grapheme = InputGrapheme {
            start: offset + start,
            end: offset + start + symbol.len(),
            width: symbol_width,
            whitespace,
        };
        let word_found = previous_was_non_whitespace && whitespace;
        let trimmed_overflow = pending_line.is_empty() && trim && word_width + symbol_width > width;
        let whitespace_overflow =
            pending_line.is_empty() && trim && whitespace_width + symbol_width > width;
        let untrimmed_overflow = pending_line.is_empty()
            && !trim
            && word_width + whitespace_width + symbol_width > width;

        if word_found || trimmed_overflow || whitespace_overflow || untrimmed_overflow {
            if !pending_line.is_empty() || !trim {
                pending_line.extend(pending_whitespace.drain(..));
                line_width += whitespace_width;
            }
            pending_line.append(&mut pending_word);
            line_width += word_width;
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= width;
        let pending_word_overflow =
            symbol_width > 0 && line_width + whitespace_width + word_width >= width;
        if line_full || pending_word_overflow {
            let mut remaining_width = width.saturating_sub(line_width);
            wrapped.push(std::mem::take(&mut pending_line));
            line_width = 0;

            while let Some(pending) = pending_whitespace.front() {
                if pending.width > remaining_width {
                    break;
                }
                whitespace_width -= pending.width;
                remaining_width -= pending.width;
                pending_whitespace.pop_front();
            }
            if whitespace && pending_whitespace.is_empty() {
                previous_was_non_whitespace = false;
                continue;
            }
        }

        if grapheme.whitespace {
            whitespace_width += grapheme.width;
            pending_whitespace.push_back(grapheme);
        } else {
            word_width += grapheme.width;
            pending_word.push(grapheme);
        }
        previous_was_non_whitespace = !whitespace;
    }

    if pending_line.is_empty() && pending_word.is_empty() && !pending_whitespace.is_empty() && trim
    {
        wrapped.push(Vec::new());
    }
    if !pending_line.is_empty() || !trim {
        pending_line.extend(pending_whitespace);
    }
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() {
        wrapped.push(pending_line);
    }
    if wrapped.is_empty() {
        wrapped.push(Vec::new());
    }
    wrapped
}

fn display_width(text: &str) -> usize {
    if text.contains(char::is_control) {
        0
    } else {
        text.width()
    }
}

/// Place the terminal cursor over word-wrapped composer input, `queue_rows`
/// below the top of `area` and adjusted by the paragraph's `scroll`.
pub fn set_input_cursor(
    frame: &mut Frame,
    area: Rect,
    input: &str,
    cursor: usize,
    queue_rows: usize,
    scroll: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = usize::from(area.width);
    let (column, input_row) = input_cursor_visual_position(input, cursor, width);
    let row = queue_rows.saturating_add(input_row).saturating_sub(scroll);
    if row < usize::from(area.height) {
        frame.set_cursor_position((
            area.x + column.min(width.saturating_sub(1)) as u16,
            area.y + row as u16,
        ));
    }
}

/// Rows the word-wrapped input occupies at `width`.
pub fn input_visual_rows(input: &str, width: usize) -> usize {
    input_cursor_visual_position(input, input.len(), width).1 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_widget_text_removes_cutoff_whitespace_and_punctuation() {
        let options = Truncate::SUMMARY;
        assert_eq!(truncate_to_cells("alpha, beta", 7, options), "alpha…");
        assert_eq!(truncate_to_cells("alpha - beta", 8, options), "alpha…");
        assert_eq!(truncate_to_cells("alpha beta", 20, options), "alpha beta");
        assert_eq!(truncate_to_cells("alpha\n beta", 20, options), "alpha beta");
    }

    #[test]
    fn truncation_measures_display_cells_not_characters() {
        let plain = Truncate::PLAIN;
        // Each ideograph is two cells wide, so only two fit beside the ellipsis.
        assert_eq!(truncate_to_cells("一二三四", 5, plain), "一二…");
        assert_eq!(truncate_to_cells("一二", 4, plain), "一二");
        assert_eq!(truncate_to_cells("alpha", 1, plain), "…");
        assert_eq!(truncate_to_cells("alpha", 0, plain), "");
        assert_eq!(truncate_to_cells("", 0, plain), "");
    }

    #[test]
    fn plain_truncation_keeps_the_punctuation_it_cuts_after() {
        let plain = Truncate::PLAIN;
        assert_eq!(truncate_to_cells("alpha, beta", 7, plain), "alpha,…");
        assert_eq!(truncate_to_cells("alpha  beta", 20, plain), "alpha  beta");
    }
}
