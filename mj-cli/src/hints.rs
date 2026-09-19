//! One-time hints for this client: things worth saying once, such as the
//! prefix key colliding with tmux, and never again once seen.
//!
//! They live in a small JSON file beside `config.toml` rather than in the
//! daemon's database, because they belong to the person's terminal habits,
//! not to any session, and a fresh clone of the configuration directory
//! should reset them.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};

use mj_chat::chat::{NOTICE_MINIMUM_DISPLAY, Notices};

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

    /// Whether the hint still has to be shown.
    pub(crate) fn pending(&self, hint: Hint) -> bool {
        !self.seen.contains(hint.name())
    }

    /// Records a hint that has been on screen. Marking it seen and saving
    /// happen together, so a hint is never shown twice even if the dashboard
    /// exits without saving anything else. Only a hint the notice bar actually
    /// carried may be recorded: writing one into a bar that never draws it
    /// spends it unread.
    pub(crate) fn mark_shown(&mut self, hint: Hint) {
        if !self.seen.insert(hint.name().to_owned()) {
            return;
        }
        if let Err(error) = self.save() {
            tracing::warn!(%error, "could not record a shown hint");
        }
    }

    fn save(&self) -> Result<()> {
        let body = serde_json::to_vec_pretty(&serde_json::json!({ "seen": self.seen }))
            .context("serialize client hints")?;
        mj_core::config::atomic_write(&self.path, &body)
    }
}

/// The terminal multiplexer the dashboard is running under, whose own prefix
/// would swallow `ctrl+b` before it reaches the dashboard. The name goes into
/// the collision notice, so there is one place that reads the environment.
pub(crate) fn multiplexer_name() -> Option<&'static str> {
    if std::env::var_os("TMUX").is_some_and(|value| !value.is_empty()) {
        return Some("tmux");
    }
    if std::env::var_os("STY").is_some_and(|value| !value.is_empty()) {
        return Some("screen");
    }
    None
}

/// One hint waiting for the notice bar, or holding it.
#[derive(Debug)]
struct QueuedHint {
    hint: Hint,
    text: String,
}

/// The hint on screen now: when it went up, and whether the file records it.
#[derive(Debug)]
struct ShownHint {
    hint: Hint,
    text: String,
    since: Instant,
    recorded: bool,
}

/// The first-launch hints, shown one after another as the notice bar comes
/// free.
///
/// A hint is recorded as shown only once it has been in the bar, because a
/// launch that restores a conversation gives the bar to its own reports first
/// and a hint written over those would be spent unread. Each hint then holds
/// the bar for [`NOTICE_MINIMUM_DISPLAY`] before the next one takes over, so
/// two hints on one launch are both readable.
#[derive(Debug)]
pub(crate) struct PendingHints {
    seen: SeenHints,
    queue: VecDeque<QueuedHint>,
    showing: Option<ShownHint>,
}

impl PendingHints {
    pub(crate) fn new(seen: SeenHints) -> Self {
        Self {
            seen,
            queue: VecDeque::new(),
            showing: None,
        }
    }

    /// Queues `hint` unless it has been shown on an earlier launch.
    pub(crate) fn queue(&mut self, hint: Hint, text: impl Into<String>) {
        if !self.seen.pending(hint) {
            return;
        }
        self.queue.push_back(QueuedHint {
            hint,
            text: text.into(),
        });
    }

    /// Gives the next hint the notice bar when `ready` says the surface has
    /// settled and nothing else is reporting, and records the hint already
    /// there now that a frame has carried it. Answers whether the bar changed,
    /// so the caller can draw the frame that shows it.
    pub(crate) fn pump(&mut self, notices: &Notices, ready: bool, now: Instant) -> bool {
        let mut bar_holds_a_hint = false;
        if let Some(showing) = self.showing.as_mut() {
            // It was set before the previous frame, so it has been drawn.
            if !showing.recorded {
                self.seen.mark_shown(showing.hint);
                showing.recorded = true;
            }
            bar_holds_a_hint = notices.current().as_deref() == Some(showing.text.as_str());
            let its_turn = bar_holds_a_hint
                && now.saturating_duration_since(showing.since) < NOTICE_MINIMUM_DISPLAY;
            if its_turn {
                return false;
            }
            self.showing = None;
        }
        if !ready || self.queue.is_empty() {
            return false;
        }
        // Anything else in the bar is about what the user just did, and that
        // outranks a hint; the hint whose turn has ended is ours to replace.
        if !bar_holds_a_hint && notices.current().is_some() {
            return false;
        }
        let next = self.queue.pop_front().expect("the queue is not empty");
        notices.set(next.text.clone());
        if notices.current().as_deref() != Some(next.text.as_str()) {
            // A failure still holds the bar for its minimum display; wait.
            self.queue.push_front(next);
            return false;
        }
        self.showing = Some(ShownHint {
            hint: next.hint,
            text: next.text,
            since: now,
            recorded: false,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shown_hint_is_recorded_once_and_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        let mut hints = SeenHints::load_from(path.clone());
        assert!(hints.pending(Hint::PrefixCollision));
        hints.mark_shown(Hint::PrefixCollision);
        assert!(!hints.pending(Hint::PrefixCollision));
        hints.mark_shown(Hint::PrefixKeys);

        let reloaded = SeenHints::load_from(path);
        assert!(!reloaded.pending(Hint::PrefixCollision));
        assert!(!reloaded.pending(Hint::PrefixKeys));
    }

    #[test]
    fn an_unreadable_file_means_nothing_was_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        std::fs::write(&path, b"not json").unwrap();
        let hints = SeenHints::load_from(path);
        assert!(hints.pending(Hint::PrefixKeys));
    }

    fn queued(path: &std::path::Path) -> PendingHints {
        let mut hints = PendingHints::new(SeenHints::load_from(path.to_path_buf()));
        hints.queue(Hint::PrefixCollision, "collision");
        hints.queue(Hint::PrefixKeys, "keys");
        hints
    }

    /// The launch that restores a conversation reports on that in the notice
    /// bar. A hint written over it would be spent unread, so it waits — and
    /// the file must not record it while it waits.
    #[test]
    fn a_hint_waits_for_the_notice_bar_and_is_recorded_only_once_it_is_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        let mut hints = queued(&path);
        let notices = Notices::default();
        let now = Instant::now();

        // The surface has not settled: the startup pick or an attach is still
        // running.
        assert!(!hints.pump(&notices, false, now));
        assert!(notices.current().is_none());

        // Settled, but the bar carries the attach's own report.
        notices.set("Opening session… Esc cancels.");
        assert!(!hints.pump(&notices, true, now));
        assert_eq!(
            notices.current().as_deref(),
            Some("Opening session… Esc cancels.")
        );
        assert!(SeenHints::load_from(path.clone()).pending(Hint::PrefixCollision));

        // The attach finished and cleared the bar.
        notices.clear();
        assert!(hints.pump(&notices, true, now));
        assert_eq!(notices.current().as_deref(), Some("collision"));
        // Recorded only on the pump after the frame that drew it.
        assert!(SeenHints::load_from(path.clone()).pending(Hint::PrefixCollision));
        assert!(!hints.pump(&notices, true, now));
        assert!(!SeenHints::load_from(path).pending(Hint::PrefixCollision));
    }

    /// Inside tmux a first launch has two things to say, so the second hint
    /// follows the first instead of replacing it or being dropped.
    #[test]
    fn two_queued_hints_are_both_shown_one_after_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        let mut hints = queued(&path);
        let notices = Notices::default();
        let now = Instant::now();

        assert!(hints.pump(&notices, true, now));
        assert_eq!(notices.current().as_deref(), Some("collision"));
        // Still the collision notice's turn.
        assert!(!hints.pump(&notices, true, now));
        assert_eq!(notices.current().as_deref(), Some("collision"));

        let later = now + NOTICE_MINIMUM_DISPLAY;
        assert!(hints.pump(&notices, true, later));
        assert_eq!(notices.current().as_deref(), Some("keys"));
        assert!(!hints.pump(&notices, true, later));
        assert_eq!(notices.current().as_deref(), Some("keys"));

        let reloaded = SeenHints::load_from(path);
        assert!(!reloaded.pending(Hint::PrefixCollision));
        assert!(!reloaded.pending(Hint::PrefixKeys));
    }

    /// A hint already shown on an earlier launch is never queued again.
    #[test]
    fn a_recorded_hint_is_not_queued_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-hints.json");
        let mut seen = SeenHints::load_from(path.clone());
        seen.mark_shown(Hint::PrefixCollision);
        let mut hints = PendingHints::new(seen);
        hints.queue(Hint::PrefixCollision, "collision");
        hints.queue(Hint::PrefixKeys, "keys");
        let notices = Notices::default();
        assert!(hints.pump(&notices, true, Instant::now()));
        assert_eq!(notices.current().as_deref(), Some("keys"));
    }
}
