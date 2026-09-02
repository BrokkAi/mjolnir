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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    cursor: usize,
    kill_buffer: Box<str>,
    chain_kill: bool,
    max_chars: Option<usize>,
    filter: InputFilter,
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
            history: None,
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
                KeyCode::Char('a') => return self.move_to(0),
                KeyCode::Char('e') => return self.move_to(self.value.len()),
                KeyCode::Char('b') => {
                    return self.move_to(previous_grapheme(&self.value, self.cursor));
                }
                KeyCode::Char('f') => return self.move_to(next_grapheme(&self.value, self.cursor)),
                KeyCode::Char('h') => self.backspace(),
                KeyCode::Char('d') => self.delete(),
                KeyCode::Char('u') => self.kill(0..self.cursor, false),
                KeyCode::Char('k') => {
                    let changed = self.kill(self.cursor..self.value.len(), chained);
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
                KeyCode::Char('p') | KeyCode::Up => return self.move_history(-1),
                KeyCode::Char('n') | KeyCode::Down => return self.move_history(1),
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
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(self.value.len()),
            KeyCode::Backspace => Self::changed(self.backspace()),
            KeyCode::Delete => Self::changed(self.delete()),
            KeyCode::Up => self.move_history(-1),
            KeyCode::Down => self.move_history(1),
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::SUPER) && !character.is_control() =>
            {
                Self::changed(self.insert_str(&character.to_string()))
            }
            _ => EditOutcome::Unhandled,
        }
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
        EditOutcome::Handled
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = previous_grapheme(&self.value, self.cursor);
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.leave_history();
        true
    }

    fn delete(&mut self) -> bool {
        if self.cursor == self.value.len() {
            return false;
        }
        let end = next_grapheme(&self.value, self.cursor);
        self.value.replace_range(self.cursor..end, "");
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
        let prefix = &self.value[..self.cursor];
        let trimmed = prefix.trim_end_matches(char::is_whitespace);
        if trimmed.is_empty() {
            return 0;
        }
        let run_start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
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

    fn next_word_end(&self) -> usize {
        let suffix = &self.value[self.cursor..];
        let Some(non_space) = suffix.find(|c: char| !c.is_whitespace()) else {
            return self.value.len();
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
        self.cursor + non_space + end
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
        for mut character in text.chars() {
            if matches!(character, '\r' | '\n') || character.is_control() {
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

fn word_class(character: char) -> bool {
    const SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";
    SEPARATORS.contains(character)
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
}
