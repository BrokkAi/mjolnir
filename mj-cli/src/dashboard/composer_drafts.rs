//! Process-local composer ownership and ordered, coalescing durability.
use mj_controller::database::DetachedSessionDraft;
use std::collections::BTreeMap;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ComposerDraftCache {
    drafts: BTreeMap<String, DetachedSessionDraft>,
    #[serde(default)]
    persistence: BTreeMap<String, DraftPersistence>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct DraftPersistence {
    desired: u64,
    saved: u64,
    #[serde(skip)]
    in_flight: Option<u64>,
    #[serde(skip)]
    failed: bool,
}

impl ComposerDraftCache {
    fn remember(&mut self, session_id: &str) {
        let draft = &self.drafts[session_id];
        // Older upgrade handoffs contain only drafts. An adopted seed still
        // needs retirement even if its composer has already been cleared.
        self.persistence
            .entry(session_id.to_owned())
            .or_insert_with(|| DraftPersistence {
                desired: u64::from(
                    !draft.text.is_empty()
                        || draft
                            .inherited_input
                            .as_ref()
                            .is_some_and(|s| !s.is_empty()),
                ),
                ..Default::default()
            });
    }

    pub(crate) fn open(&mut self, session_id: &str, inherited_input: &str) -> DetachedSessionDraft {
        self.drafts
            .entry(session_id.to_owned())
            .or_insert_with(|| DetachedSessionDraft {
                text: inherited_input.to_owned(),
                inherited_input: Some(inherited_input.to_owned()),
            });
        self.remember(session_id);
        self.drafts[session_id].clone()
    }

    pub(crate) fn capture(
        &mut self,
        session_id: &str,
        text: String,
        inherited_input: &str,
    ) -> DetachedSessionDraft {
        self.open(session_id, inherited_input);
        let entry = self.drafts.get_mut(session_id).unwrap();
        if entry.text != text {
            entry.text = text;
            let state = self.persistence.get_mut(session_id).unwrap();
            state.desired = state
                .desired
                .checked_add(1)
                .expect("terminal draft revision exhausted");
            state.failed = false;
        }
        entry.clone()
    }

    /// Removed sessions retain their empty local composer but have no writes
    /// to retry. Their records and physical persistence belong to lifecycle.
    pub(crate) fn discard(&mut self, session_id: &str) {
        self.drafts
            .insert(session_id.to_owned(), DetachedSessionDraft::default());
        // Keep the in-flight revision until its completion arrives, so quit
        // cannot lose a request whose tracker guard has just been released.
        if let Some(state) = self.persistence.get_mut(session_id) {
            state.saved = state.desired;
            state.failed = false;
        }
    }

    pub(crate) fn get(&self, session_id: &str) -> Option<&DetachedSessionDraft> {
        self.drafts.get(session_id)
    }

    pub(crate) fn start(
        &mut self,
        session_id: &str,
        retry: bool,
    ) -> Option<(u64, DetachedSessionDraft)> {
        if !self.drafts.contains_key(session_id) {
            return None;
        }
        self.remember(session_id);
        let state = self.persistence.get_mut(session_id).unwrap();
        if state.in_flight.is_some() {
            return None;
        }
        if state.desired <= state.saved || (state.failed && !retry) {
            return None;
        }
        state.failed = false;
        state.in_flight = Some(state.desired);
        Some((state.desired, self.drafts[session_id].clone()))
    }

    pub(crate) fn complete(&mut self, session_id: &str, revision: u64, succeeded: bool) {
        let Some(state) = self.persistence.get_mut(session_id) else {
            return;
        };
        if state.in_flight != Some(revision) {
            return;
        }
        state.in_flight = None;
        if succeeded {
            state.saved = state.saved.max(revision);
            self.drafts.get_mut(session_id).unwrap().inherited_input = None;
        }
        // A new edit is its own retry trigger; a failed unchanged revision
        // stays dirty until an explicit flush, without a polling retry loop.
        state.failed = !succeeded && state.desired == revision;
    }

    pub(crate) fn session_ids(&self) -> Vec<String> {
        self.drafts.keys().cloned().collect()
    }

    pub(crate) fn is_saving(&self) -> bool {
        self.persistence
            .values()
            .any(|state| state.in_flight.is_some())
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
    #[test]
    fn delayed_ack_coalesces_edits_and_clear_without_blocking_other_sessions() {
        let mut cache = ComposerDraftCache::default();
        cache.capture("a", "typed".into(), "");
        let (first, _) = cache.start("a", false).unwrap();
        cache.capture("a", "newer".into(), "");
        cache.capture("a", String::new(), "");
        cache.capture("b", "other".into(), "");
        let (other, _) = cache.start("b", false).unwrap();
        assert!(cache.start("a", false).is_none());
        cache.complete("a", first, true);
        assert!(
            cache.is_saving(),
            "the other session retains its outstanding ACK"
        );
        let (latest, draft) = cache.start("a", false).unwrap();
        assert!(latest > first);
        assert!(
            draft.text.is_empty(),
            "sending must durably retire the earlier draft"
        );
        assert!(draft.inherited_input.is_none());
        cache.complete("a", first, true); // Duplicate old ACK cannot retire latest.
        assert!(cache.start("a", false).is_none());
        cache.complete("a", latest, true);
        cache.complete("b", other, true);
        assert!(!cache.is_saving());
        cache.capture("a", String::new(), "stale seed");
        assert!(
            cache.start("a", true).is_none(),
            "unchanged navigation/quit cannot write"
        );
    }

    #[test]
    fn failures_remain_dirty_until_new_edit_or_explicit_flush() {
        let mut cache = ComposerDraftCache::default();
        cache.open("empty", "");
        assert!(cache.start("empty", true).is_none());
        cache.open("a", "inherited");
        let (first, _) = cache.start("a", false).unwrap();
        cache.complete("a", first, false);
        assert!(cache.start("a", false).is_none());
        cache.capture("a", "inherited".into(), "");
        assert!(cache.start("a", false).is_none());
        let (retry, _) = cache.start("a", true).unwrap();
        assert_eq!(retry, first);
        cache.capture("a", "changed while retrying".into(), "");
        cache.complete("a", retry, false);
        let (newer, _) = cache.start("a", false).unwrap();
        assert!(newer > retry);
        assert!(cache.start("a", true).is_none()); // Quit requests final flush.
        cache.complete("a", newer, false);
        assert!(
            cache.start("a", false).is_none(),
            "quit must not retry the same uncertain in-flight revision"
        );
        assert!(!cache.is_saving());
        // A later explicit flush may retry a value already known to have failed.
        let (final_attempt, _) = cache.start("a", true).unwrap();
        assert_eq!(final_attempt, newer);
        cache.complete("a", final_attempt, false);
        assert!(
            cache.start("a", false).is_none(),
            "failed final flush must not spin"
        );
    }
}
