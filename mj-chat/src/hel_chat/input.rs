//! Composer text editing: insertion, cursor motion, kill and yank, and the
//! visual wrapping the terminal cursor follows.

use std::collections::VecDeque;

use ratatui::Frame;
use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;

use super::ChatState;
use super::rendering::{display_width, sanitize_terminal_text};

impl ChatState {
    pub(super) fn insert_character(&mut self, character: char) {
        self.input.insert(self.input_cursor, character);
        self.input_cursor += character.len_utf8();
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn handle_paste(&mut self, pasted: &str) {
        if let Some(dialog) = self.elicitation.as_mut() {
            dialog.paste(pasted);
            return;
        }
        let pasted = sanitize_terminal_text(pasted);
        if pasted.is_empty() {
            return;
        }
        if let Some(search) = self.history_search.as_mut() {
            search.query.push_str(&pasted.replace(['\r', '\n'], " "));
            self.refresh_history_search();
            return;
        }
        self.input.insert_str(self.input_cursor, &pasted);
        self.input_cursor += pasted.len();
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let start = previous_grapheme_boundary(&self.input, self.input_cursor);
        self.input.replace_range(start..self.input_cursor, "");
        self.input_cursor = start;
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn delete(&mut self) {
        if self.input_cursor >= self.input.len() {
            return;
        }
        let end = next_grapheme_boundary(&self.input, self.input_cursor);
        self.input.replace_range(self.input_cursor..end, "");
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn move_input_cursor(&mut self, delta: isize) {
        self.input_cursor = if delta.is_negative() {
            previous_grapheme_boundary(&self.input, self.input_cursor)
        } else {
            next_grapheme_boundary(&self.input, self.input_cursor)
        };
        self.preferred_column = None;
        self.update_autocomplete();
    }

    fn line_start(&self) -> usize {
        self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.input[self.input_cursor..]
            .find('\n')
            .map_or(self.input.len(), |index| self.input_cursor + index)
    }

    pub(super) fn move_to_line_start(&mut self, cross_boundary: bool) {
        let start = self.line_start();
        self.input_cursor = if cross_boundary && self.input_cursor == start && start > 0 {
            self.input[..start - 1]
                .rfind('\n')
                .map_or(0, |index| index + 1)
        } else {
            start
        };
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn move_to_line_end(&mut self, cross_boundary: bool) {
        let end = self.line_end();
        self.input_cursor = if cross_boundary && self.input_cursor == end && end < self.input.len()
        {
            let next = end + 1;
            self.input[next..]
                .find('\n')
                .map_or(self.input.len(), |index| next + index)
        } else {
            end
        };
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn move_vertical(&mut self, direction: isize) {
        let start = self.line_start();
        let column = self
            .preferred_column
            .unwrap_or_else(|| self.input[start..self.input_cursor].graphemes(true).count());
        let target_start = if direction.is_negative() {
            if start == 0 {
                self.input_cursor = 0;
                self.preferred_column = None;
                self.update_autocomplete();
                return;
            }
            self.input[..start - 1]
                .rfind('\n')
                .map_or(0, |index| index + 1)
        } else {
            let end = self.line_end();
            if end == self.input.len() {
                self.input_cursor = self.input.len();
                self.preferred_column = None;
                self.update_autocomplete();
                return;
            }
            end + 1
        };
        let target_end = self.input[target_start..]
            .find('\n')
            .map_or(self.input.len(), |index| target_start + index);
        self.input_cursor = self.input[target_start..target_end]
            .grapheme_indices(true)
            .nth(column)
            .map_or(target_end, |(offset, _)| target_start + offset);
        self.preferred_column = Some(column);
        self.update_autocomplete();
    }

    pub(super) fn previous_word_start(&self) -> usize {
        let prefix = &self.input[..self.input_cursor];
        let trimmed = prefix.trim_end_matches(char::is_whitespace);
        if trimmed.is_empty() {
            return 0;
        }
        let run_start = trimmed
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        let run = &trimmed[run_start..];
        let mut start = run_start + run.len();
        let mut class = None;
        for (index, character) in run.char_indices().rev() {
            let next_class = word_class(character);
            if class.is_some_and(|class| class != next_class) {
                break;
            }
            class = Some(next_class);
            start = run_start + index;
        }
        start
    }

    pub(super) fn next_word_end(&self) -> usize {
        let suffix = &self.input[self.input_cursor..];
        let Some(non_space) = suffix.find(|character: char| !character.is_whitespace()) else {
            return self.input.len();
        };
        let run = &suffix[non_space..];
        let mut end = 0;
        let mut class = None;
        for (index, character) in run.char_indices() {
            if character.is_whitespace() {
                break;
            }
            let next_class = word_class(character);
            if class.is_some_and(|class| class != next_class) {
                break;
            }
            class = Some(next_class);
            end = index + character.len_utf8();
        }
        self.input_cursor + non_space + end
    }

    pub(super) fn move_word(&mut self, direction: isize) {
        self.input_cursor = if direction.is_negative() {
            self.previous_word_start()
        } else {
            self.next_word_end()
        };
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn kill_range(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.kill_buffer = self.input[range.clone()].to_owned();
        self.input.replace_range(range.clone(), "");
        self.input_cursor = range.start;
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn kill_to_line_start(&mut self) {
        let start = self.line_start();
        if start == self.input_cursor && start > 0 {
            self.kill_range(start - 1..start);
        } else {
            self.kill_range(start..self.input_cursor);
        }
    }

    /// Kill to the end of the line. A `chained` kill appends to the kill
    /// buffer, in Emacs order, so a later yank restores the whole block.
    pub(super) fn kill_to_line_end(&mut self, chained: bool) {
        let end = self.line_end();
        let range = if end == self.input_cursor && end < self.input.len() {
            end..end + 1
        } else {
            self.input_cursor..end
        };
        if range.is_empty() {
            // Nothing was killed, so leave any chained buffer intact.
            return;
        }
        let previous = if chained {
            std::mem::take(&mut self.kill_buffer)
        } else {
            String::new()
        };
        self.kill_range(range);
        if !previous.is_empty() {
            self.kill_buffer.insert_str(0, &previous);
        }
    }

    pub(super) fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let text = self.kill_buffer.clone();
        self.input.insert_str(self.input_cursor, &text);
        self.input_cursor += text.len();
        self.history_index = None;
        self.update_autocomplete();
    }
}

pub(super) fn previous_grapheme_boundary(input: &str, cursor: usize) -> usize {
    input[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_grapheme_boundary(input: &str, cursor: usize) -> usize {
    input[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(input.len(), |(index, _)| cursor + index)
}

fn word_class(character: char) -> bool {
    const SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";
    SEPARATORS.contains(character)
}

pub(super) fn set_input_cursor(
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

pub(super) fn input_visual_rows(input: &str, width: usize) -> usize {
    input_cursor_visual_position(input, input.len(), width).1 + 1
}

pub(super) fn input_cursor_visual_position(
    input: &str,
    cursor: usize,
    width: usize,
) -> (usize, usize) {
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

#[derive(Clone, Copy)]
struct InputGrapheme {
    start: usize,
    end: usize,
    width: usize,
    whitespace: bool,
}

/// Mirror ratatui's `WordWrapper` layout so the terminal cursor follows the
/// word-wrapped `Paragraph`, including whitespace discarded at wrap points.
fn wrap_input_line(line: &str, offset: usize, width: usize) -> Vec<Vec<InputGrapheme>> {
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
        let whitespace = symbol == "\u{200b}"
            || (symbol.chars().all(char::is_whitespace) && symbol != "\u{00a0}");
        let grapheme = InputGrapheme {
            start: offset + start,
            end: offset + start + symbol.len(),
            width: symbol_width,
            whitespace,
        };
        let word_found = previous_was_non_whitespace && whitespace;
        let untrimmed_overflow =
            pending_line.is_empty() && word_width + whitespace_width + symbol_width > width;

        if word_found || untrimmed_overflow {
            pending_line.extend(pending_whitespace.drain(..));
            line_width += whitespace_width;
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

    if pending_line.is_empty() && pending_word.is_empty() && !pending_whitespace.is_empty() {
        wrapped.push(Vec::new());
    }
    pending_line.extend(pending_whitespace);
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() {
        wrapped.push(pending_line);
    }
    if wrapped.is_empty() {
        wrapped.push(Vec::new());
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_chat::test_support::{ctrl, key, snapshot};
    use crate::hel_chat::{ChatAction, active::render_full_frame as render};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use hel::hel_worker::{ActivePrompt, WorkerPhase};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn multiline_paste_is_one_draft_and_one_queued_prompt() {
        let mut running = snapshot();
        running.phase = WorkerPhase::Running;
        running.active_prompt = Some(ActivePrompt {
            request_id: "p".into(),
            text: "busy".into(),
            attachments: vec![],
        });
        let mut chat = ChatState::new(&running, &[]);

        chat.handle_paste("first\r\nsecond\rthird");

        assert_eq!(chat.input, "first\nsecond\nthird");
        assert!(chat.queued_prompts.is_empty());
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Prompt("first\nsecond\nthird".into())
        );
        assert!(chat.queued_prompts.is_empty());
    }

    #[test]
    fn composer_cursor_follows_a_word_moved_to_the_next_visual_row() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("abcdefgh ijkl".into());
        let mut terminal = Terminal::new(TestBackend::new(14, 12)).expect("terminal");

        terminal
            .draw(|frame| render(frame, &mut chat, false))
            .expect("draw chat");
        let (word_end_x, word_row) = {
            let buffer = terminal.backend().buffer();
            let word_row = (buffer.area.y..buffer.area.bottom())
                .find(|&y| {
                    (buffer.area.x..buffer.area.right())
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains("ijkl")
                })
                .expect("wrapped word");
            let word_start_x = (buffer.area.x..buffer.area.right())
                .find(|&x| buffer[(x, word_row)].symbol() == "i")
                .expect("wrapped word start");
            (word_start_x + 4, word_row)
        };

        terminal
            .backend_mut()
            .assert_cursor_position((word_end_x, word_row));
        assert_eq!(
            input_cursor_visual_position(&chat.input, chat.input.len(), 12),
            (4, 1)
        );
        assert_eq!(input_cursor_visual_position(&chat.input, 9, 12), (0, 1));
        assert_eq!(input_cursor_visual_position(&chat.input, 10, 12), (1, 1));
    }

    #[test]
    fn editor_supports_cursor_insertion_deletion_and_prompt_history() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("ac".into());
        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(key(KeyCode::Char('b')));
        assert_eq!(chat.input, "abc");
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.input, "ac");
        chat.handle_key(key(KeyCode::Delete));
        assert_eq!(chat.input, "a");

        chat.set_input("remember me".into());
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Prompt("remember me".into())
        );
        chat.phase = WorkerPhase::Idle;
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "remember me");
        chat.handle_key(key(KeyCode::Down));
        assert!(chat.input.is_empty());
    }

    #[test]
    fn readline_line_movement_kill_and_yank_match_codex() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("alpha beta\ngamma".into());
        chat.handle_key(ctrl('a'));
        assert_eq!(&chat.input[chat.input_cursor..], "gamma");
        chat.handle_key(ctrl('a'));
        assert_eq!(chat.input_cursor, 0);
        chat.handle_key(ctrl('e'));
        assert_eq!(&chat.input[..chat.input_cursor], "alpha beta");
        chat.handle_key(ctrl('k'));
        assert_eq!(chat.input, "alpha betagamma");
        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "alpha beta\ngamma");
    }

    #[test]
    fn sequential_control_k_accumulates_one_yankable_block() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("line1\nline2".into());
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('k'));
        chat.handle_key(ctrl('k'));
        assert_eq!(chat.input, "line2");
        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "line1\nline2");
    }

    #[test]
    fn any_key_between_control_k_presses_restarts_the_kill_buffer() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("line1\nline2".into());
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('k'));
        chat.handle_key(key(KeyCode::Right));
        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(ctrl('k'));
        assert_eq!(chat.kill_buffer, "\n");

        chat.set_input("line1\nline2".into());
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('k'));
        chat.handle_key(key(KeyCode::Char('x')));
        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('k'));
        assert_eq!(chat.kill_buffer, "x");
    }

    #[test]
    fn readline_word_edits_and_grapheme_cursor_are_atomic() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("one two 👩‍💻".into());
        chat.handle_key(key(KeyCode::Left));
        assert_eq!(&chat.input[chat.input_cursor..], "👩‍💻");
        chat.handle_key(ctrl('w'));
        assert_eq!(chat.input, "one 👩‍💻");
        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "one two 👩‍💻");
        chat.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(&chat.input[chat.input_cursor..], "two 👩‍💻");
    }
}
