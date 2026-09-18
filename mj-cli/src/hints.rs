//! One-time hints for this client: things worth saying once, such as the
//! prefix key colliding with tmux, and never again once seen.
//!
//! They live in a small JSON file beside `config.toml` rather than in the
//! daemon's database, because they belong to the person's terminal habits,
//! not to any session, and a fresh clone of the configuration directory
//! should reset them.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// A hint the dashboard shows once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Hint {
    /// The configured prefix is `ctrl+b` inside tmux or GNU screen.
    PrefixCollision,
    /// The first dashboard of a fresh install, before any key was pressed.
    PrefixKeys,
}

impl Hint {
    fn name(self) -> &'static str {
        match self {
            Self::PrefixCollision => "prefix_collision",
            Self::PrefixKeys => "prefix_keys",
        }
    }
}

/// The hints already shown, read from and written to one file.
#[derive(Debug, Default)]
pub(crate) struct SeenHints {
    seen: BTreeSet<String>,
    path: PathBuf,
}

impl SeenHints {
    pub(crate) fn load() -> Self {
        Self::load_from(mj_core::config::config_dir().join("client-hints.json"))
    }

    fn load_from(path: PathBuf) -> Self {
        let seen = std::fs::read(&path)
            .ok()
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
            .and_then(|value| {
                value.get("seen").and_then(|seen| {
                    seen.as_array().map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| entry.as_str().map(str::to_owned))
                            .collect()
                    })
                })
            })
            .unwrap_or_default();
        Self { seen, path }
    }

    /// Whether the hint still has to be shown. Marking it seen and saving
    /// happen together, so a hint is never shown twice even if the dashboard
    /// exits without saving anything else.
    pub(crate) fn take(&mut self, hint: Hint) -> bool {
        if !self.seen.insert(hint.name().to_owned()) {
            return false;
        }
        if let Err(error) = self.save() {
            tracing::warn!(%error, "could not record a shown hint");
        }
        true
    }

    fn save(&self) -> Result<()> {
        let body = serde_json::to_vec_pretty(&serde_json::json!({ "seen": self.seen }))
            .context("serialize client hints")?;
        mj_core::config::atomic_write(&self.path, &body)
    }
}

/// Whether the dashboard is running under tmux or GNU screen, whose own
/// prefix would swallow `ctrl+b` before it reaches the dashboard.
pub(crate) fn inside_multiplexer() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
        || std::env::var_os("STY").is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_is_taken_once_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        let mut hints = SeenHints::load_from(path.clone());
        assert!(hints.take(Hint::PrefixCollision));
        assert!(!hints.take(Hint::PrefixCollision));
        assert!(hints.take(Hint::PrefixKeys));

        let mut reloaded = SeenHints::load_from(path);
        assert!(!reloaded.take(Hint::PrefixCollision));
        assert!(!reloaded.take(Hint::PrefixKeys));
    }

    #[test]
    fn an_unreadable_file_means_nothing_was_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        std::fs::write(&path, b"not json").unwrap();
        let mut hints = SeenHints::load_from(path);
        assert!(hints.take(Hint::PrefixKeys));
    }
}
