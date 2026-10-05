//! Path editing reuses the text editor; resolution happens only on apply.
use crate::components::ControlKind;
use crate::text_input::TextInput;
use mj_core::path_completion::PathCompletion;
use std::{
    fmt,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

/// The completion popup state owned by one path field.
///
/// `requested` holds the text a reply is still expected for, so a reply that
/// arrives after the user kept typing can be dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Completion {
    candidates: Vec<String>,
    selected: usize,
    truncated: bool,
    requested: Option<String>,
}

impl Completion {
    fn clear_popup(&mut self) {
        self.candidates.clear();
        self.selected = 0;
        self.truncated = false;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathInput {
    text: TextInput,
    completion: Completion,
}

impl PathInput {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_value(value: String) -> Self {
        Self {
            text: TextInput::from_value(value),
            completion: Completion::default(),
        }
    }
    pub fn resolve(&self, home: Option<&Path>) -> anyhow::Result<PathBuf> {
        mj_core::path_input::expand_home(Path::new(self.value()), home)
    }
    pub fn apply_local(&mut self) -> anyhow::Result<PathBuf> {
        let path = mj_core::path_input::expand_local(Path::new(self.value()))?;
        self.set_value(path.to_string_lossy().into_owned());
        Ok(path)
    }

    /// The candidates currently offered by the popup.
    #[must_use]
    pub fn completions(&self) -> &[String] {
        &self.completion.candidates
    }

    /// The highlighted candidate.
    #[must_use]
    pub fn completion_selected(&self) -> usize {
        self.completion.selected
    }

    /// Whether a popup of candidates is open.
    #[must_use]
    pub fn is_completing(&self) -> bool {
        !self.completion.candidates.is_empty()
    }

    /// Whether the host had more matches than it was willing to send.
    #[must_use]
    pub fn completion_truncated(&self) -> bool {
        self.completion.truncated
    }

    /// The text a completion reply is still expected for.
    #[must_use]
    pub fn completion_pending(&self) -> Option<&str> {
        self.completion.requested.as_deref()
    }

    /// The control declaration this field registers with a form.
    #[must_use]
    pub fn control_kind(&self) -> ControlKind {
        ControlKind::PathField {
            len: self.completion.candidates.len(),
            selected: self.completion.selected,
            expanded: self.is_completing(),
        }
    }

    /// Records a completion request for the current text, returning the prefix
    /// to ask the host about. An empty field, or one already waiting for a
    /// reply about this exact text, asks for nothing.
    pub fn request_completion(&mut self) -> Option<String> {
        let value = self.text.value();
        if value.is_empty() || self.completion.requested.as_deref() == Some(value) {
            return None;
        }
        let value = value.to_owned();
        self.completion.requested = Some(value.clone());
        Some(value)
    }

    /// Applies a host reply. A reply for text the field no longer holds is
    /// dropped and reported as unapplied.
    pub fn apply_completion(&mut self, prefix: &str, completion: PathCompletion) -> bool {
        self.completion.requested = None;
        if self.text.value() != prefix {
            return false;
        }
        if let Some(insert) = completion.insert.as_deref()
            && insert != prefix
        {
            self.text.set_value(insert);
        }
        if completion.candidates.len() > 1 {
            self.completion.candidates = completion.candidates;
            self.completion.selected = 0;
            self.completion.truncated = completion.truncated;
        } else {
            self.completion.clear_popup();
        }
        true
    }

    /// Moves the popup highlight, clamped to the candidates on offer.
    pub fn select_completion(&mut self, index: usize) {
        if self.completion.candidates.is_empty() {
            self.completion.selected = 0;
        } else {
            self.completion.selected = index.min(self.completion.candidates.len() - 1);
        }
    }

    /// Replaces the text with the highlighted candidate and closes the popup.
    pub fn accept_completion(&mut self) -> bool {
        let Some(candidate) = self
            .completion
            .candidates
            .get(self.completion.selected)
            .cloned()
        else {
            return false;
        };
        self.text.set_value(candidate);
        self.completion.clear_popup();
        self.completion.requested = None;
        true
    }

    /// Closes the popup and forgets any pending request.
    pub fn dismiss_completion(&mut self) {
        self.completion.clear_popup();
        self.completion.requested = None;
    }
}
impl Deref for PathInput {
    type Target = TextInput;
    fn deref(&self) -> &TextInput {
        &self.text
    }
}
impl DerefMut for PathInput {
    fn deref_mut(&mut self) -> &mut TextInput {
        &mut self.text
    }
}
impl From<String> for PathInput {
    fn from(value: String) -> Self {
        Self::from_value(value)
    }
}
impl From<&str> for PathInput {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}
impl fmt::Display for PathInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.text.fmt(f)
    }
}
impl PartialEq<str> for PathInput {
    fn eq(&self, other: &str) -> bool {
        self.value() == other
    }
}
impl PartialEq<&str> for PathInput {
    fn eq(&self, other: &&str) -> bool {
        self.value() == *other
    }
}
impl AsRef<std::ffi::OsStr> for PathInput {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.value().as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use std::fmt::Write as _;

    fn reply(candidates: &[&str], insert: Option<&str>, truncated: bool) -> PathCompletion {
        PathCompletion {
            candidates: candidates.iter().map(|text| (*text).to_owned()).collect(),
            insert: insert.map(ToOwned::to_owned),
            truncated,
        }
    }

    fn path_field_screen(input: &PathInput) -> String {
        let mut terminal = Terminal::new(TestBackend::new(40, 14)).expect("terminal");
        let mut form = crate::components::Form::<u8>::new();
        form.declare(1, input.control_kind());
        form.end_frame(1);
        terminal
            .draw(|frame| {
                form.begin_frame();
                crate::components::PathField::render(
                    frame,
                    Rect::new(0, 1, 24, 1),
                    input,
                    &mut form,
                    1,
                );
                form.end_frame(1);
            })
            .expect("render path field");
        crate::golden::buffer_lines(terminal.backend().buffer()).join("\n")
    }

    fn append_path_state(output: &mut String, label: &str, input: &PathInput) {
        writeln!(output, "=== {label} (40x14) ===").expect("write heading");
        writeln!(
            output,
            "value: {:?}; cursor: {}; control: {:?}; pending: {:?}; truncated: {}",
            input.value(),
            input.cursor(),
            input.control_kind(),
            input.completion_pending(),
            input.completion_truncated()
        )
        .expect("write field state");
        output.push_str(&path_field_screen(input));
        output.push('\n');
    }

    #[test]
    fn golden_path_field() {
        let mut output = String::new();
        let mut input = PathInput::new();
        let edit = crate::components::PathField::apply(
            &mut input,
            crate::components::FieldEdit::Paste("~/.codex4".into()),
        );
        writeln!(output, "=== edited draft (40x14) ===\nedit: {edit:?}").expect("write edit");
        append_path_state(&mut output, "before resolution", &input);
        writeln!(
            output,
            "resolved with supplied home: {:?}",
            input
                .resolve(Some(Path::new("/workspace/user")))
                .expect("resolve draft")
        )
        .expect("write resolved path");
        writeln!(
            output,
            "missing home rejected: {}",
            input.resolve(None).is_err()
        )
        .expect("write missing-home result");
        append_path_state(&mut output, "draft after resolution attempts", &input);
        output.push_str("=== end of rendered states ===\n");
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "path-field", &output);
    }

    #[test]
    fn reply_for_stale_text_is_ignored() {
        let mut input = PathInput::from("~/pr");
        assert_eq!(input.request_completion().as_deref(), Some("~/pr"));
        input.set_value("~/proj");
        assert!(!input.apply_completion("~/pr", reply(&["~/pr1/", "~/pr2/"], None, false)));
        assert_eq!(input.value(), "~/proj");
        assert!(!input.is_completing());
        assert_eq!(input.completion_pending(), None);
    }

    #[test]
    fn golden_path_completion() {
        let mut output = String::new();

        let mut input = PathInput::from("~/pr");
        let request = input.request_completion();
        let applied =
            input.apply_completion("~/pr", reply(&["~/projects/"], Some("~/projects/"), false));
        writeln!(
            output,
            "single completion request: {request:?}; reply applied: {applied}"
        )
        .expect("write single completion");
        append_path_state(&mut output, "single match inserts without popup", &input);

        let mut input = PathInput::from("~/p");
        let request = input.request_completion();
        let applied = input.apply_completion(
            "~/p",
            reply(&["~/projects/", "~/provision/"], Some("~/pro"), false),
        );
        writeln!(
            output,
            "common-prefix request: {request:?}; reply applied: {applied}"
        )
        .expect("write common-prefix completion");
        append_path_state(&mut output, "common prefix and two candidates", &input);

        let mut input = PathInput::from("~/p");
        input.request_completion();
        input.apply_completion("~/p", reply(&["~/projects/", "~/provision/"], None, true));
        append_path_state(&mut output, "truncated candidates before edit", &input);
        let outcome = crate::components::PathField::apply(
            &mut input,
            crate::components::FieldEdit::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
            )),
        );
        writeln!(output, "edit outcome: {outcome:?}").expect("write edit outcome");
        append_path_state(&mut output, "popup dismissed after edit", &input);

        let mut input = PathInput::from("~/p");
        input.request_completion();
        input.apply_completion("~/p", reply(&["~/projects/", "~/provision/"], None, false));
        input.select_completion(9);
        append_path_state(&mut output, "selected candidate is clamped", &input);
        let accepted = input.accept_completion();
        let accepted_again = input.accept_completion();
        writeln!(
            output,
            "accepted: {accepted}; accepting closed popup: {accepted_again}"
        )
        .expect("write acceptance result");
        append_path_state(&mut output, "accepted candidate", &input);
        output.push_str("=== end of rendered states ===\n");
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "path-completion", &output);
    }

    #[test]
    fn request_is_not_repeated_while_pending() {
        let mut input = PathInput::new();
        assert_eq!(input.request_completion(), None);
        input.set_value("~/pr");
        assert_eq!(input.request_completion().as_deref(), Some("~/pr"));
        assert_eq!(input.request_completion(), None);
        input.set_value("~/prq");
        assert_eq!(input.request_completion().as_deref(), Some("~/prq"));
        input.dismiss_completion();
        assert_eq!(input.completion_pending(), None);
        assert_eq!(input.request_completion().as_deref(), Some("~/prq"));
    }
}
