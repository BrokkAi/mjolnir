//! Nonblocking readline-style editing for text fields embedded in Hel's TUIs.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fmt;
use std::ops::Deref;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Unhandled,
    Handled,
    Changed,
}

impl EditOutcome {
    #[must_use]
    pub fn changed(self) -> bool {
        self == Self::Changed
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputFilter {
    #[default]
    Any,
    AsciiAlphabeticUppercase,
    /// Session ids are lowercase hex; typing one to confirm a destructive
    /// action must not survive stray characters or capitals.
    AsciiHexLowercase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    cursor: usize,
    kill_buffer: Box<str>,
    chain_kill: bool,
    max_chars: Option<usize>,
    filter: InputFilter,
    multiline: bool,
    /// The column a vertical motion aims for, so a short line in between does
    /// not pull the caret left. Every other cursor change clears it.
    preferred_column: Option<usize>,
    history: Option<Box<InputHistory>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputHistory {
    entries: Vec<String>,
    index: Option<usize>,
    draft: String,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for TextInput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

impl fmt::Display for TextInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.value())
    }
}

impl From<String> for TextInput {
    fn from(value: String) -> Self {
        Self::from_value(value)
    }
}

impl From<&str> for TextInput {
    fn from(value: &str) -> Self {
        Self::from_value(value)
    }
}

impl From<TextInput> for String {
    fn from(input: TextInput) -> Self {
        input.into_value()
    }
}

impl From<TextInput> for PathBuf {
    fn from(input: TextInput) -> Self {
        input.into_value().into()
    }
}

impl AsRef<std::ffi::OsStr> for TextInput {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.value.as_ref()
    }
}

impl PartialEq<str> for TextInput {
    fn eq(&self, other: &str) -> bool {
        self.value == other
    }
}

impl PartialEq<&str> for TextInput {
    fn eq(&self, other: &&str) -> bool {
        self.value == *other
    }
}

impl Extend<char> for TextInput {
    fn extend<T: IntoIterator<Item = char>>(&mut self, iter: T) {
        for character in iter {
            self.insert_str(&character.to_string());
        }
    }
}

impl TextInput {
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            kill_buffer: Box::default(),
            chain_kill: false,
            max_chars: None,
            filter: InputFilter::Any,
            multiline: false,
            preferred_column: None,
            history: None,
        }
    }

    #[must_use]
    pub fn multiline() -> Self {
        Self {
            multiline: true,
            ..Self::new()
        }
    }

    #[must_use]
    pub fn from_value(value: impl Into<String>) -> Self {
        let mut input = Self::new();
        input.set_value(value);
        input
    }

    #[must_use]
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = Some(max_chars);
        self.enforce_limit();
        self
    }

    #[must_use]
    pub fn with_filter(mut self, filter: InputFilter) -> Self {
        self.filter = filter;
        let value = std::mem::take(&mut self.value);
        self.set_value(value);
        self
    }

    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = (!history.is_empty()).then(|| {
            Box::new(InputHistory {
                entries: history,
                index: None,
                draft: String::new(),
            })
        });
    }

    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Places the caret at a grapheme boundary, clamping offsets from a drawn field.
    pub fn set_cursor(&mut self, offset: usize) {
        self.cursor = self
            .value
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .chain(std::iter::once(self.value.len()))
            .take_while(|index| *index <= offset)
            .last()
            .unwrap_or(0);
        self.chain_kill = false;
        self.preferred_column = None;
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value.clear();
        self.cursor = 0;
        let value = value.into();
        self.insert_filtered(&value);
        self.cursor = self.value.len();
        self.leave_history();
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
        self.preferred_column = None;
        self.leave_history();
    }

    #[must_use]
    pub fn with_cursor_marker(&self, marker: &str) -> String {
        let mut rendered = String::with_capacity(self.value.len() + marker.len());
        rendered.push_str(&self.value[..self.cursor]);
        rendered.push_str(marker);
        rendered.push_str(&self.value[self.cursor..]);
        rendered
    }

    pub fn push(&mut self, character: char) {
        self.insert_str(&character.to_string());
    }

    pub fn push_str(&mut self, text: &str) {
        self.insert_str(text);
    }

    pub fn pop(&mut self) -> Option<char> {
        if self.cursor != self.value.len() || self.value.is_empty() {
            return None;
        }
        let character = self.value.pop()?;
        self.cursor = self.value.len();
        self.preferred_column = None;
        Some(character)
    }

    #[must_use]
    pub fn into_value(self) -> String {
        self.value
    }

    pub fn insert_str(&mut self, text: &str) -> bool {
        let before = self.value.clone();
        self.insert_filtered(text);
        self.leave_history();
        self.value != before
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditOutcome {
        let (code, modifiers) = normalize_key(key.code, key.modifiers);
        let chained = std::mem::take(&mut self.chain_kill);
        if modifiers.contains(KeyModifiers::CONTROL) {
            let changed = match code {
                KeyCode::Char('a') => return self.handle_line_start(true),
                KeyCode::Char('e') => return self.handle_line_end(true),
                KeyCode::Char('b') => {
                    return self.move_to(previous_grapheme(&self.value, self.cursor));
                }
                KeyCode::Char('f') => return self.move_to(next_grapheme(&self.value, self.cursor)),
                KeyCode::Char('h') => self.backspace(),
                KeyCode::Char('d') => self.delete(),
                KeyCode::Char('u') => self.kill_line_backward(),
                KeyCode::Char('k') => {
                    let changed = self.kill_line_forward(chained);
                    self.chain_kill = true;
                    changed
                }
                KeyCode::Char('w') | KeyCode::Backspace => {
                    self.kill(self.previous_word_start()..self.cursor, false)
                }
                KeyCode::Char('y') => self.yank(),
                KeyCode::Left => return self.move_to(self.previous_word_start()),
                KeyCode::Right => return self.move_to(self.next_word_end()),
                KeyCode::Delete => self.kill(self.cursor..self.next_word_end(), false),
                KeyCode::Char('p') | KeyCode::Up => return self.handle_vertical(-1),
                KeyCode::Char('n') | KeyCode::Down => return self.handle_vertical(1),
                _ => return EditOutcome::Unhandled,
            };
            return if changed {
                EditOutcome::Changed
            } else {
                EditOutcome::Handled
            };
        }
        if modifiers.contains(KeyModifiers::ALT) {
            return match code {
                KeyCode::Char('b') | KeyCode::Left => self.move_to(self.previous_word_start()),
                KeyCode::Char('f') | KeyCode::Right => self.move_to(self.next_word_end()),
                KeyCode::Char('d') | KeyCode::Delete => {
                    Self::changed(self.kill(self.cursor..self.next_word_end(), false))
                }
                KeyCode::Backspace => {
                    Self::changed(self.kill(self.previous_word_start()..self.cursor, false))
                }
                _ => EditOutcome::Unhandled,
            };
        }
        match code {
            KeyCode::Left => self.move_to(previous_grapheme(&self.value, self.cursor)),
            KeyCode::Right => self.move_to(next_grapheme(&self.value, self.cursor)),
            KeyCode::Home => self.handle_line_start(false),
            KeyCode::End => self.handle_line_end(false),
            KeyCode::Backspace => Self::changed(self.backspace()),
            KeyCode::Delete => Self::changed(self.delete()),
            KeyCode::Up => self.handle_vertical(-1),
            KeyCode::Down => self.handle_vertical(1),
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::SUPER) && !character.is_control() =>
            {
                Self::changed(self.insert_str(&character.to_string()))
            }
            _ => EditOutcome::Unhandled,
        }
    }

    /// Ctrl-A and Home. A single-line field has one line, so only a multiline
    /// field distinguishes the line start from the start of the value.
    fn handle_line_start(&mut self, cross_boundary: bool) -> EditOutcome {
        if !self.multiline {
            return self.move_to(0);
        }
        self.move_to_line_start(cross_boundary);
        EditOutcome::Handled
    }

    /// Ctrl-E and End; the mirror of `handle_line_start`.
    fn handle_line_end(&mut self, cross_boundary: bool) -> EditOutcome {
        if !self.multiline {
            return self.move_to(self.value.len());
        }
        self.move_to_line_end(cross_boundary);
        EditOutcome::Handled
    }

    /// Up and Down. A multiline field walks its own lines; a field with a
    /// history walks the history instead, because that is the only way to
    /// reach it from the keyboard.
    fn handle_vertical(&mut self, direction: isize) -> EditOutcome {
        if !self.multiline || self.history.is_some() {
            return self.move_history(direction);
        }
        self.move_vertical(direction);
        EditOutcome::Handled
    }

    /// Ctrl-U: a single-line field kills back to offset zero, which is what
    /// `kill_to_line_start` computes for a value without newlines anyway.
    fn kill_line_backward(&mut self) -> bool {
        if !self.multiline {
            return self.kill(0..self.cursor, false);
        }
        self.kill_to_line_start()
    }

    /// Ctrl-K; the mirror of `kill_line_backward`.
    fn kill_line_forward(&mut self, chained: bool) -> bool {
        if !self.multiline {
            return self.kill(self.cursor..self.value.len(), chained);
        }
        self.kill_to_line_end(chained)
    }

    /// Moves to the start of the current line. With `cross_boundary`, a press
    /// that is already at the line start moves to the previous line's start.
    pub fn move_to_line_start(&mut self, cross_boundary: bool) {
        let start = line_start(&self.value, self.cursor);
        self.cursor = if cross_boundary && self.cursor == start && start > 0 {
            line_start(&self.value, start - 1)
        } else {
            start
        };
        self.preferred_column = None;
    }

    /// Moves to the end of the current line; the mirror of `move_to_line_start`.
    pub fn move_to_line_end(&mut self, cross_boundary: bool) {
        let end = line_end(&self.value, self.cursor);
        self.cursor = if cross_boundary && self.cursor == end && end < self.value.len() {
            line_end(&self.value, end + 1)
        } else {
            end
        };
        self.preferred_column = None;
    }

    /// Moves one line up (negative `direction`) or down, aiming for the column
    /// the first vertical move started from so a short line in between does
    /// not pull the caret permanently left.
    pub fn move_vertical(&mut self, direction: isize) {
        let start = line_start(&self.value, self.cursor);
        let column = self
            .preferred_column
            .unwrap_or_else(|| self.value[start..self.cursor].graphemes(true).count());
        let target_start = if direction.is_negative() {
            if start == 0 {
                self.cursor = 0;
                self.preferred_column = None;
                return;
            }
            line_start(&self.value, start - 1)
        } else {
            let end = line_end(&self.value, self.cursor);
            if end == self.value.len() {
                self.cursor = self.value.len();
                self.preferred_column = None;
                return;
            }
            end + 1
        };
        let target_end = line_end(&self.value, target_start);
        self.cursor = self.value[target_start..target_end]
            .grapheme_indices(true)
            .nth(column)
            .map_or(target_end, |(offset, _)| target_start + offset);
        self.preferred_column = Some(column);
    }

    /// Kills back to the start of the line, or the preceding newline when the
    /// caret already sits there. Reports whether anything was killed.
    pub fn kill_to_line_start(&mut self) -> bool {
        let start = line_start(&self.value, self.cursor);
        let range = if start == self.cursor && start > 0 {
            start - 1..start
        } else {
            start..self.cursor
        };
        self.kill(range, false)
    }

    /// Kills to the end of the line, or the newline itself when the caret
    /// already sits there. A `chained` kill appends to the kill buffer, in
    /// Emacs order, so a later yank restores the whole block.
    pub fn kill_to_line_end(&mut self, chained: bool) -> bool {
        let end = line_end(&self.value, self.cursor);
        let range = if end == self.cursor && end < self.value.len() {
            end..end + 1
        } else {
            self.cursor..end
        };
        self.kill(range, chained)
    }

    fn changed(changed: bool) -> EditOutcome {
        if changed {
            EditOutcome::Changed
        } else {
            EditOutcome::Handled
        }
    }

    fn move_to(&mut self, cursor: usize) -> EditOutcome {
        self.cursor = cursor;
        self.preferred_column = None;
        EditOutcome::Handled
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = previous_grapheme(&self.value, self.cursor);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.preferred_column = None;
        self.leave_history();
        true
    }

    fn delete(&mut self) -> bool {
        if self.cursor == self.value.len() {
            return false;
        }
        let end = next_grapheme(&self.value, self.cursor);
        self.value.replace_range(self.cursor..end, "");
        self.preferred_column = None;
        self.leave_history();
        true
    }

    fn kill(&mut self, range: std::ops::Range<usize>, append: bool) -> bool {
        if range.is_empty() {
            return false;
        }
        let killed = self.value[range.clone()].to_owned();
        if append {
            let mut combined = self.kill_buffer.to_string();
            combined.push_str(&killed);
            self.kill_buffer = combined.into_boxed_str();
        } else {
            self.kill_buffer = killed.into_boxed_str();
        }
        self.value.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.preferred_column = None;
        self.leave_history();
        true
    }

    fn yank(&mut self) -> bool {
        if self.kill_buffer.is_empty() {
            return false;
        }
        let killed = self.kill_buffer.to_string();
        self.insert_str(&killed)
    }

    fn previous_word_start(&self) -> usize {
        previous_word_start(&self.value, self.cursor)
    }

    fn next_word_end(&self) -> usize {
        next_word_end(&self.value, self.cursor)
    }

    fn move_history(&mut self, delta: isize) -> EditOutcome {
        let Some(history) = self.history.as_mut() else {
            return EditOutcome::Unhandled;
        };
        let next = match (history.index, delta.is_negative()) {
            (None, true) => {
                history.draft.clone_from(&self.value);
                Some(history.entries.len() - 1)
            }
            (None, false) => None,
            (Some(index), true) => Some(index.saturating_sub(1)),
            (Some(index), false) if index + 1 < history.entries.len() => Some(index + 1),
            (Some(_), false) => None,
        };
        let value = next
            .and_then(|index| history.entries.get(index).cloned())
            .unwrap_or_else(|| history.draft.clone());
        self.set_value(value);
        self.history
            .as_mut()
            .expect("history remains configured")
            .index = next;
        EditOutcome::Changed
    }

    fn leave_history(&mut self) {
        if let Some(history) = self.history.as_mut() {
            history.index = None;
        }
    }

    fn insert_filtered(&mut self, text: &str) {
        self.preferred_column = None;
        for mut character in text.chars() {
            if character.is_control() && !(self.multiline && matches!(character, '\n' | '\t')) {
                continue;
            }
            match self.filter {
                InputFilter::Any => {}
                InputFilter::AsciiAlphabeticUppercase => {
                    if !character.is_ascii_alphabetic() {
                        continue;
                    }
                    character = character.to_ascii_uppercase();
                }
                InputFilter::AsciiHexLowercase => {
                    if !character.is_ascii_hexdigit() {
                        continue;
                    }
                    character = character.to_ascii_lowercase();
                }
            }
            if self
                .max_chars
                .is_some_and(|max| self.value.chars().count() >= max)
            {
                break;
            }
            self.value.insert(self.cursor, character);
            self.cursor += character.len_utf8();
        }
    }

    fn enforce_limit(&mut self) {
        let Some(max) = self.max_chars else {
            return;
        };
        if let Some((index, _)) = self.value.char_indices().nth(max) {
            self.value.truncate(index);
        }
        self.cursor = self.cursor.min(self.value.len());
        self.preferred_column = None;
    }
}

#[must_use]
pub fn normalize_key(code: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(character) = code else {
        return (code, modifiers);
    };
    if modifiers.is_empty() {
        let value = u32::from(character);
        if (1..=26).contains(&value)
            && let Some(control) = char::from_u32(value - 1 + u32::from('a'))
        {
            modifiers.insert(KeyModifiers::CONTROL);
            return (KeyCode::Char(control), modifiers);
        }
    }
    if character.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
            return (KeyCode::Char(character.to_ascii_lowercase()), modifiers);
        }
    }
    (code, modifiers)
}

#[must_use]
pub fn previous_grapheme(input: &str, cursor: usize) -> usize {
    input[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

#[must_use]
pub fn next_grapheme(input: &str, cursor: usize) -> usize {
    input[cursor..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(input.len(), |(index, _)| cursor + index)
}

/// The offset of the start of the word before `cursor`, readline style: a run
/// of whitespace is skipped, then the run of same-class characters that follows
/// it. "Class" is word characters versus the punctuation in `SEPARATORS`.
#[must_use]
pub fn previous_word_start(text: &str, cursor: usize) -> usize {
    let prefix = &text[..cursor];
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
        let next = word_class(character);
        if class.is_some_and(|class| class != next) {
            break;
        }
        class = Some(next);
        start = run_start + index;
    }
    start
}

/// The offset of the end of the word after `cursor`; the mirror of
/// `previous_word_start`.
#[must_use]
pub fn next_word_end(text: &str, cursor: usize) -> usize {
    let suffix = &text[cursor..];
    let Some(non_space) = suffix.find(|character: char| !character.is_whitespace()) else {
        return text.len();
    };
    let run = &suffix[non_space..];
    let mut end = 0;
    let mut class = None;
    for (index, character) in run.char_indices() {
        if character.is_whitespace() {
            break;
        }
        let next = word_class(character);
        if class.is_some_and(|class| class != next) {
            break;
        }
        class = Some(next);
        end = index + character.len_utf8();
    }
    cursor + non_space + end
}

/// The offset just after the newline that precedes `cursor`, or zero.
#[must_use]
pub fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

/// The offset of the newline that follows `cursor`, or the end of `text`.
#[must_use]
pub fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index)
}

fn word_class(character: char) -> bool {
    const SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";
    SEPARATORS.contains(character)
}

/// Normalizes a terminal paste for a single-line field without collapsing spaces.
pub fn single_line_paste(pasted: &str) -> String {
    pasted.trim_matches(['\r', '\n']).replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    #[test]
    fn readline_edits_at_unicode_grapheme_boundaries() {
        let mut input = TextInput::from_value("one 👩‍💻 two");
        input.handle_key(ctrl('a'));
        input.handle_key(key(KeyCode::Right));
        input.handle_key(key(KeyCode::Right));
        input.handle_key(key(KeyCode::Right));
        input.handle_key(key(KeyCode::Right));
        input.handle_key(key(KeyCode::Delete));
        assert_eq!(input.value(), "one  two");

        // Word edits step over a whole grapheme cluster, not a char.
        let mut input = TextInput::from_value("one two 👩‍💻");
        input.handle_key(key(KeyCode::Left));
        assert_eq!(&input.value()[input.cursor()..], "👩‍💻");
        input.handle_key(ctrl('w'));
        assert_eq!(input.value(), "one 👩‍💻");
        input.handle_key(ctrl('y'));
        assert_eq!(input.value(), "one two 👩‍💻");
        input.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(&input.value()[input.cursor()..], "two 👩‍💻");
    }

    #[test]
    fn kill_and_yank_follow_readline_bindings() {
        let mut input = TextInput::from_value("alpha beta");
        input.handle_key(ctrl('w'));
        assert_eq!(input.value(), "alpha ");
        input.handle_key(ctrl('y'));
        assert_eq!(input.value(), "alpha beta");
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('k'));
        input.handle_key(ctrl('y'));
        assert_eq!(input.value(), "alpha beta");
    }

    #[test]
    fn readline_line_movement_kill_and_yank_match_codex() {
        let mut input = TextInput::multiline();
        input.set_value("alpha beta\ngamma");
        input.handle_key(ctrl('a'));
        assert_eq!(&input.value()[input.cursor()..], "gamma");
        input.handle_key(ctrl('a'));
        assert_eq!(input.cursor(), 0);
        input.handle_key(ctrl('e'));
        assert_eq!(&input.value()[..input.cursor()], "alpha beta");
        input.handle_key(ctrl('k'));
        assert_eq!(input.value(), "alpha betagamma");
        input.handle_key(ctrl('y'));
        assert_eq!(input.value(), "alpha beta\ngamma");
    }

    #[test]
    fn sequential_control_k_accumulates_one_yankable_block() {
        let mut input = TextInput::multiline();
        input.set_value("line1\nline2");
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('k'));
        input.handle_key(ctrl('k'));
        assert_eq!(input.value(), "line2");
        input.handle_key(ctrl('y'));
        assert_eq!(input.value(), "line1\nline2");
    }

    #[test]
    fn any_key_between_control_k_presses_restarts_the_kill_buffer() {
        let mut input = TextInput::multiline();
        input.set_value("line1\nline2");
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('k'));
        input.handle_key(key(KeyCode::Right));
        input.handle_key(key(KeyCode::Left));
        input.handle_key(ctrl('k'));
        assert_eq!(&*input.kill_buffer, "\n");

        input.set_value("line1\nline2");
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('k'));
        input.handle_key(key(KeyCode::Char('x')));
        input.handle_key(ctrl('a'));
        input.handle_key(ctrl('k'));
        assert_eq!(&*input.kill_buffer, "x");
    }

    #[test]
    fn vertical_motion_keeps_the_preferred_column_across_a_short_line() {
        let mut input = TextInput::multiline();
        input.set_value("abcdef\nxy\nghijkl");
        input.set_cursor(5);

        input.handle_key(key(KeyCode::Down));
        assert_eq!(input.cursor(), 9, "clamped to the end of the short line");
        input.handle_key(key(KeyCode::Down));
        assert_eq!(input.cursor(), 15, "back to the remembered column");

        // Any other motion forgets the column, so the next Down starts over.
        input.handle_key(key(KeyCode::Home));
        input.handle_key(key(KeyCode::Up));
        assert_eq!(input.cursor(), 7);
    }

    #[test]
    fn single_line_fields_keep_their_whole_value_motions() {
        let mut input = TextInput::from_value("alpha beta");
        input.handle_key(ctrl('a'));
        assert_eq!(input.cursor(), 0);
        input.handle_key(ctrl('e'));
        assert_eq!(input.cursor(), "alpha beta".len());
        input.handle_key(ctrl('u'));
        assert_eq!(input.value(), "");
    }

    #[test]
    fn optional_history_restores_the_draft() {
        let mut input = TextInput::from_value("draft");
        input.set_history(vec!["first".into(), "second".into()]);
        input.handle_key(key(KeyCode::Up));
        assert_eq!(input.value(), "second");
        input.handle_key(key(KeyCode::Down));
        assert_eq!(input.value(), "draft");
    }

    #[test]
    fn filtered_confirmation_never_records_invalid_text() {
        let mut input = TextInput::new()
            .with_max_chars(4)
            .with_filter(InputFilter::AsciiAlphabeticUppercase);
        input.insert_str("s-t0op");
        assert_eq!(input.value(), "STOP");
    }

    #[test]
    fn hex_confirmation_drops_non_hex_characters_and_lowercases() {
        let mut input = TextInput::new()
            .with_max_chars(8)
            .with_filter(InputFilter::AsciiHexLowercase);
        input.insert_str("zz0123ABcD!");
        assert_eq!(input.value(), "0123abcd");
    }
}
