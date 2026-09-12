//! Path editing reuses the text editor; resolution happens only on apply.
use crate::text_input::TextInput;
use std::{
    fmt,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathInput(TextInput);

impl PathInput {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_value(value: String) -> Self {
        Self(TextInput::from_value(value))
    }
    pub fn resolve(&self, home: Option<&Path>) -> anyhow::Result<PathBuf> {
        mj_core::path_input::expand_home(Path::new(self.value()), home)
    }
    pub fn apply_local(&mut self) -> anyhow::Result<PathBuf> {
        let path = mj_core::path_input::expand_local(Path::new(self.value()))?;
        self.set_value(path.to_string_lossy().into_owned());
        Ok(path)
    }
}
impl Deref for PathInput {
    type Target = TextInput;
    fn deref(&self) -> &TextInput {
        &self.0
    }
}
impl DerefMut for PathInput {
    fn deref_mut(&mut self) -> &mut TextInput {
        &mut self.0
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
        self.0.fmt(f)
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
}
