//! Process-local composer state for the terminal surface.
//!
//! The session record contains a legacy draft used only when a terminal first
//! opens a session. Once opened, the terminal owns that session's composer for
//! the rest of the process. Keeping this cache separate means a controller
//! snapshot loaded by a background task cannot put old shared text back into a
//! composer that the user has already cleared.

use std::collections::BTreeMap;

use mj_controller::database::DetachedSessionDraft;

#[derive(Debug, Default)]
pub(crate) struct ComposerDraftCache {
    drafts: BTreeMap<String, DetachedSessionDraft>,
}

impl ComposerDraftCache {
    /// Return the cached draft, adopting the legacy session field only on the
    /// first actual open request for this process and session.
    pub(crate) fn open(&mut self, session_id: &str, inherited_input: &str) -> DetachedSessionDraft {
        self.drafts
            .entry(session_id.to_owned())
            .or_insert_with(|| DetachedSessionDraft {
                text: inherited_input.to_owned(),
                inherited_input: Some(inherited_input.to_owned()),
            })
            .clone()
    }

    /// Capture a warm chat's current composer without changing its inherited
    /// shared baseline. The baseline lets the daemon clear only the legacy
    /// value this client actually adopted.
    pub(crate) fn capture(
        &mut self,
        session_id: &str,
        text: String,
        inherited_input: &str,
    ) -> DetachedSessionDraft {
        let entry =
            self.drafts
                .entry(session_id.to_owned())
                .or_insert_with(|| DetachedSessionDraft {
                    text: String::new(),
                    inherited_input: Some(inherited_input.to_owned()),
                });
        entry.text = text;
        entry.clone()
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<&DetachedSessionDraft> {
        self.drafts.get(session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cleared_draft_stays_empty_when_a_stale_snapshot_is_reopened() {
        let mut cache = ComposerDraftCache::default();

        assert_eq!(cache.open("session-a", "/model g").text, "/model g");
        assert_eq!(
            cache.capture("session-a", String::new(), "/model g").text,
            ""
        );

        // A later controller reload still reports the old shared field, but
        // this client has already adopted and cleared its composer.
        assert_eq!(cache.open("session-a", "/model g").text, "");
        assert_eq!(
            cache
                .get("session-a")
                .and_then(|draft| draft.inherited_input.as_deref()),
            Some("/model g")
        );
    }

    #[test]
    fn sessions_keep_independent_current_text_and_baselines() {
        let mut cache = ComposerDraftCache::default();
        cache.open("session-a", "old a");
        cache.open("session-b", "old b");

        cache.capture("session-a", "new a".into(), "old a");
        cache.capture("session-b", String::new(), "old b");

        assert_eq!(cache.get("session-a").unwrap().text, "new a");
        assert_eq!(cache.get("session-b").unwrap().text, "");
        assert_eq!(
            cache.get("session-a").unwrap().inherited_input.as_deref(),
            Some("old a")
        );
        assert_eq!(
            cache.get("session-b").unwrap().inherited_input.as_deref(),
            Some("old b")
        );
    }

    #[test]
    fn capturing_again_keeps_the_first_shared_baseline() {
        let mut cache = ComposerDraftCache::default();
        cache.open("session-a", "legacy");
        cache.capture("session-a", "edited".into(), "legacy");
        let current = cache.capture("session-a", "cleared".into(), "a newer snapshot");

        assert_eq!(current.text, "cleared");
        assert_eq!(current.inherited_input.as_deref(), Some("legacy"));
    }

    #[test]
    fn two_terminal_caches_do_not_share_composer_text() {
        let mut first = ComposerDraftCache::default();
        let mut second = ComposerDraftCache::default();
        first.open("session-a", "legacy");
        second.open("session-a", "legacy");

        first.capture("session-a", "first terminal".into(), "legacy");
        second.capture("session-a", "second terminal".into(), "legacy");

        assert_eq!(first.get("session-a").unwrap().text, "first terminal");
        assert_eq!(second.get("session-a").unwrap().text, "second terminal");
    }
}
