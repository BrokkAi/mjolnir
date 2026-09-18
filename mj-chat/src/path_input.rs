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

    fn reply(candidates: &[&str], insert: Option<&str>, truncated: bool) -> PathCompletion {
        PathCompletion {
            candidates: candidates.iter().map(|text| (*text).to_owned()).collect(),
            insert: insert.map(ToOwned::to_owned),
            truncated,
        }
    }

    #[test]
    fn path_editing_preserves_draft_until_apply() {
        let mut input = PathInput::new();
        crate::components::PathField::apply(
            &mut input,
            crate::components::FieldEdit::Paste("~/.codex4".into()),
        );
        assert_eq!(input.value(), "~/.codex4");
        assert_eq!(
            input.resolve(Some(Path::new("/home/test"))).unwrap(),
            Path::new("/home/test/.codex4")
        );
        assert_eq!(input.value(), "~/.codex4");
        assert!(input.resolve(None).is_err());
        assert_eq!(input.value(), "~/.codex4");
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
    fn single_match_is_inserted_without_a_popup() {
        let mut input = PathInput::from("~/pr");
        input.request_completion();
        assert!(
            input.apply_completion("~/pr", reply(&["~/projects/"], Some("~/projects/"), false))
        );
        assert_eq!(input.value(), "~/projects/");
        assert_eq!(input.cursor(), input.value().len());
        assert!(!input.is_completing());
    }

    #[test]
    fn common_prefix_is_inserted_and_popup_opens() {
        let mut input = PathInput::from("~/p");
        input.request_completion();
        assert!(input.apply_completion(
            "~/p",
            reply(&["~/projects/", "~/provision/"], Some("~/pro"), false),
        ));
        assert_eq!(input.value(), "~/pro");
        assert!(input.is_completing());
        assert_eq!(input.completions().len(), 2);
        assert_eq!(input.completion_selected(), 0);
        assert!(!input.completion_truncated());
    }

    #[test]
    fn edit_dismisses_the_popup() {
        let mut input = PathInput::from("~/p");
        input.request_completion();
        input.apply_completion("~/p", reply(&["~/projects/", "~/provision/"], None, true));
        assert!(input.is_completing());
        assert!(input.completion_truncated());
        let outcome = crate::components::PathField::apply(
            &mut input,
            crate::components::FieldEdit::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::NONE,
            )),
        );
        assert!(outcome.changed());
        assert!(!input.is_completing());
        assert!(!input.completion_truncated());
    }

    #[test]
    fn accept_replaces_the_value() {
        let mut input = PathInput::from("~/p");
        input.request_completion();
        input.apply_completion("~/p", reply(&["~/projects/", "~/provision/"], None, false));
        input.select_completion(9);
        assert_eq!(input.completion_selected(), 1);
        assert!(input.accept_completion());
        assert_eq!(input.value(), "~/provision/");
        assert_eq!(input.cursor(), input.value().len());
        assert!(!input.is_completing());
        assert!(!input.accept_completion());
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
