//! Composer text editing: insertion, cursor motion, kill and yank, and the
//! visual wrapping the terminal cursor follows.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use super::rendering::sanitize_terminal_text;
use super::{ChatAction, ChatState, PromptPayload, attachments};
use crate::text_input::{
    line_end, line_start, next_grapheme, next_word_end, previous_grapheme, previous_word_start,
};

impl ChatState {
    pub(super) fn replace_input_range(
        &mut self,
        range: std::ops::Range<usize>,
        inserted: &PromptPayload,
    ) -> PromptPayload {
        if range.is_empty() && inserted.text.is_empty() && inserted.images.is_empty() {
            return PromptPayload::text("");
        }
        let (cursor, removed) =
            attachments::replace_range(&mut self.input, &mut self.input_images, range, inserted);
        self.input_cursor = cursor;
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();
        self.next_image_number = self.next_image_number.max(
            self.input_images
                .iter()
                .map(|image| image.number.saturating_add(1))
                .max()
                .unwrap_or(1),
        );
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
        removed
    }

    pub(super) fn insert_character(&mut self, character: char) {
        self.replace_input_range(
            self.input_cursor..self.input_cursor,
            &PromptPayload::text(character.to_string()),
        );
    }

    pub(super) fn handle_terminal_paste(&mut self, pasted: &str) -> ChatAction {
        if self.earlier.is_some() {
            return ChatAction::None;
        }
        if pasted.is_empty() {
            // Terminals signal non-text clipboard content (such as images)
            // with an empty bracketed paste. Respect the same modal routing
            // as Ctrl-V and let the background clipboard reader retrieve it.
            // An agent without image support cannot use that content, so
            // say so instead of reading the clipboard.
            if !self.prompt_images_supported
                && self.elicitation.is_none()
                && self.clipboard_target() == super::input_state::ClipboardTarget::Composer
            {
                self.set_notice(super::input_state::IMAGE_PASTE_UNSUPPORTED_NOTICE);
                return ChatAction::None;
            }
            return self.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        }
        self.paste_or_attach(pasted)
    }

    /// Paste into the composer, or, when the paste is only image paths and
    /// the agent takes images, ask for those files to be attached.
    pub(super) fn paste_or_attach(&mut self, pasted: &str) -> ChatAction {
        if self.elicitation.is_none()
            && self.history_search.is_none()
            && self.prompt_images_supported
            && self.clipboard_target() == super::input_state::ClipboardTarget::Composer
        {
            let text = sanitize_terminal_text(pasted);
            if let Some(images) = super::pasted_paths::image_paths(&text) {
                return ChatAction::AttachPaste { text, images };
            }
        }
        self.handle_paste(pasted);
        ChatAction::None
    }

    /// Insert already sanitized text at the cursor, without looking for paths.
    pub(super) fn paste_plain(&mut self, text: &str) {
        self.replace_input_range(
            self.input_cursor..self.input_cursor,
            &PromptPayload::text(text),
        );
    }

    /// Host-facing paste: bracketed-paste text lands in the composer draft.
    pub fn paste(&mut self, pasted: &str) {
        self.handle_paste(pasted);
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
        self.replace_input_range(
            self.input_cursor..self.input_cursor,
            &PromptPayload::text(pasted),
        );
    }

    pub(super) fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let start = previous_grapheme(&self.input, self.input_cursor);
        self.replace_input_range(start..self.input_cursor, &PromptPayload::text(""));
    }

    pub(super) fn delete(&mut self) {
        if self.input_cursor >= self.input.len() {
            return;
        }
        let end = next_grapheme(&self.input, self.input_cursor);
        self.replace_input_range(self.input_cursor..end, &PromptPayload::text(""));
    }

    pub(super) fn move_input_cursor(&mut self, delta: isize) {
        self.input_cursor = if delta.is_negative() {
            previous_grapheme(&self.input, self.input_cursor)
        } else {
            next_grapheme(&self.input, self.input_cursor)
        };
        self.input_cursor = attachments::snap_cursor(&self.input_images, self.input_cursor, delta);
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn move_to_line_start(&mut self, cross_boundary: bool) {
        let start = line_start(&self.input, self.input_cursor);
        self.input_cursor = if cross_boundary && self.input_cursor == start && start > 0 {
            line_start(&self.input, start - 1)
        } else {
            start
        };
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn move_to_line_end(&mut self, cross_boundary: bool) {
        let end = line_end(&self.input, self.input_cursor);
        self.input_cursor = if cross_boundary && self.input_cursor == end && end < self.input.len()
        {
            line_end(&self.input, end + 1)
        } else {
            end
        };
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn move_vertical(&mut self, direction: isize) {
        let start = line_start(&self.input, self.input_cursor);
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
            line_start(&self.input, start - 1)
        } else {
            let end = line_end(&self.input, self.input_cursor);
            if end == self.input.len() {
                self.input_cursor = self.input.len();
                self.preferred_column = None;
                self.update_autocomplete();
                return;
            }
            end + 1
        };
        let target_end = line_end(&self.input, target_start);
        self.input_cursor = self.input[target_start..target_end]
            .grapheme_indices(true)
            .nth(column)
            .map_or(target_end, |(offset, _)| target_start + offset);
        // Only the composer needs this: a cursor that lands inside an
        // `[image N]` marker is pushed to the near edge of the whole marker.
        self.input_cursor =
            attachments::snap_cursor(&self.input_images, self.input_cursor, direction);
        self.preferred_column = Some(column);
        self.update_autocomplete();
    }

    pub(super) fn move_word(&mut self, direction: isize) {
        self.input_cursor = if direction.is_negative() {
            previous_word_start(&self.input, self.input_cursor)
        } else {
            next_word_end(&self.input, self.input_cursor)
        };
        self.input_cursor =
            attachments::snap_cursor(&self.input_images, self.input_cursor, direction);
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(super) fn kill_range(&mut self, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        let removed = self.replace_input_range(range, &PromptPayload::text(""));
        self.kill_buffer = removed.text;
        self.kill_images = removed.images;
    }

    pub(super) fn kill_to_line_start(&mut self) {
        let start = line_start(&self.input, self.input_cursor);
        if start == self.input_cursor && start > 0 {
            self.kill_range(start - 1..start);
        } else {
            self.kill_range(start..self.input_cursor);
        }
    }

    /// Kill to the end of the line. A `chained` kill appends to the kill
    /// buffer, in Emacs order, so a later yank restores the whole block.
    pub(super) fn kill_to_line_end(&mut self, chained: bool) {
        let end = line_end(&self.input, self.input_cursor);
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
            PromptPayload {
                text: std::mem::take(&mut self.kill_buffer),
                images: std::mem::take(&mut self.kill_images),
            }
        } else {
            PromptPayload::text("")
        };
        self.kill_range(range);
        if !previous.text.is_empty() {
            attachments::replace_range(
                &mut self.kill_buffer,
                &mut self.kill_images,
                0..0,
                &previous,
            );
        }
    }

    pub(super) fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let payload = PromptPayload {
            text: self.kill_buffer.clone(),
            images: self.kill_images.clone(),
        };
        self.replace_input_range(self.input_cursor..self.input_cursor, &payload);
    }
}

pub(super) use crate::components::text_layout::{
    grapheme_offset_for_wrapped_row, input_cursor_visual_position, input_visual_rows,
    set_input_cursor, wrapped_row_for_grapheme_offset,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatAction;
    use crate::chat::test_support::{ctrl, key, snapshot};
    use crossterm::event::KeyCode;
    use mj_core::relay::{ActivePrompt, WorkerPhase};

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
    fn control_k_and_control_y_round_trip_a_line_holding_an_image_marker() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_prompt_images_supported(true);
        chat.set_input("before\nafter".into());
        chat.handle_key(ctrl('a'));
        assert!(chat.reserve_attachment(0));
        assert_eq!(chat.input, "before\n[image 1]after");
        assert_eq!(chat.input_images.len(), 1);

        chat.handle_key(ctrl('a'));
        chat.handle_key(ctrl('k'));
        assert_eq!(chat.input, "before\n");
        assert!(chat.input_images.is_empty());

        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "before\n[image 1]after");
        assert_eq!(chat.input_images.len(), 1);
        assert_eq!(chat.input_images[0].range, 7.."before\n[image 1]".len());
    }
}
