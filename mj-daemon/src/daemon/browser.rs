//! Browser-only facts captured alongside the terminal projection.
use super::*;
use crate::server::{BrowserTranscript, ViewerLaunchFailure};
use crate::server_runtime::{ConversationProjectionKey, ProjectSourceKey};
use mj_client::runtime_feed::RuntimeProjection;
use mj_core::snapshot_map::{SharedSnapshot, SnapshotMap};
use mj_core::state::ProjectSourceIdentity;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConversationPreview {
    key: ConversationProjectionKey,
    pub(crate) last_message_at_ms: Option<i64>,
    pub(crate) lines: Vec<String>,
}

impl ConversationPreview {
    pub(crate) fn of(key: ConversationProjectionKey, transcript: &BrowserTranscript) -> Self {
        let last_message_at_ms = transcript
            .entries
            .iter()
            .filter(|entry| matches!(entry.role, "user" | "agent"))
            .filter_map(|entry| entry.recorded_at_ms)
            .max();
        let mut lines = transcript
            .entries
            .iter()
            .rev()
            .flat_map(|entry| {
                entry
                    .lines
                    .iter()
                    .enumerate()
                    .rev()
                    .filter_map(|(index, line)| {
                        let line = line.trim();
                        (!line.is_empty()).then_some((index, &entry.label, line))
                    })
            })
            .take(4)
            .map(|(index, label, line)| {
                if index == 0 {
                    format!("{label}: {line}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>();
        lines.reverse();
        Self {
            key,
            last_message_at_ms,
            lines,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResolvedProjectSource {
    pub(crate) key: ProjectSourceKey,
    pub(crate) source: ProjectSourceIdentity,
}

#[derive(Clone, Default)]
pub(crate) struct BrowserInputs {
    pub(crate) conversations: SnapshotMap<String, ConversationPreview>,
    pub(crate) launch_failures: SharedSnapshot<Vec<ViewerLaunchFailure>>,
    pub(crate) action_errors: SnapshotMap<String, String>,
    pub(crate) project_sources: SnapshotMap<String, ResolvedProjectSource>,
    pub(crate) notices: SharedSnapshot<Vec<RuntimeNotice>>,
}

impl BrowserInputs {
    /// Preserve the last web action error until a later successful action for
    /// the same session. Retain launch notices even if provisioning rolled
    /// back its provisional record.
    pub(crate) fn record_action(
        &mut self,
        id: Option<&str>,
        error: Option<&str>,
        launch_failure: Option<ViewerLaunchFailure>,
    ) -> bool {
        let mut changed = false;
        if let Some(id) = id {
            let errors = &mut self.action_errors;
            if errors.get(id).map(String::as_str) != error {
                if let Some(error) = error {
                    errors.insert(id.to_owned(), error.to_owned());
                } else {
                    errors.remove_shared(id);
                }
                changed = true;
            }
        }
        if let Some(failure) = launch_failure {
            let failures = &mut *self.launch_failures;
            failures.push(failure);
            if failures.len() > 16 {
                failures.remove(0);
            }
            changed = true;
        }
        changed
    }

    pub(super) fn same_as(&self, other: &Self) -> bool {
        self.conversations
            .changes(&other.conversations)
            .next()
            .is_none()
            && self.launch_failures == other.launch_failures
            && self
                .action_errors
                .changes(&other.action_errors)
                .next()
                .is_none()
            && self
                .project_sources
                .changes(&other.project_sources)
                .next()
                .is_none()
            && self.notices == other.notices
    }
}

pub(super) struct BrowserState {
    inputs: BrowserInputs,
    // The conversation endpoint reads the latest transcript. History needs
    // only the summary that the session row reads.
    transcripts: tokio::sync::watch::Sender<SnapshotMap<String, BrowserTranscript>>,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self {
            inputs: BrowserInputs::default(),
            transcripts: tokio::sync::watch::channel(SnapshotMap::new()).0,
        }
    }
}

impl BrowserState {
    pub(super) fn capture(
        &mut self,
        owner: &RuntimeStateOwner,
        before: &RuntimeProjection,
        records: &SnapshotMap<String, SessionRecord>,
    ) -> BrowserInputs {
        let controller = owner.controller();
        let tails = &owner.transcripts;
        let changed_records = before.records.changes(records).map(|(id, _)| id);
        for id in changed_records.chain(before.transcripts.changes(tails).map(|(id, _)| id)) {
            let conversation_ended = !records
                .get(id)
                .is_some_and(|record| record.state.is_active())
                || !tails.contains_key(id);
            if conversation_ended && self.inputs.conversations.remove_shared(id).is_some() {
                self.transcripts.send_modify(|transcripts| {
                    transcripts.remove_shared(id);
                });
            }
            if !records.contains_key(id) {
                self.inputs.action_errors.remove_shared(id);
            }
        }
        let invalid = |id: &String, resolved: &ResolvedProjectSource| {
            (!resolved.key.matches(id, controller)).then(|| id.clone())
        };
        let invalid: BTreeSet<_> = if before.metadata.config == owner.config_snapshot {
            let relationships = before
                .subagents
                .changes(&controller.state.subagents)
                .map(|(id, _)| id);
            let records = before.records.changes(records).flat_map(|(id, record)| {
                // When a previously absent parent appears, its children stop
                // owning their project identity even if their rows stayed put.
                let children = (record.is_some() && !before.records.contains_key(id))
                    .then(|| owner.indexes.children.get(id))
                    .flatten();
                std::iter::once(id).chain(children.into_iter().flat_map(SnapshotMap::keys))
            });
            records
                .chain(relationships)
                .filter_map(|id| invalid(id, self.inputs.project_sources.get(id)?))
                .collect()
        } else {
            self.inputs
                .project_sources
                .iter()
                .filter_map(|(id, resolved)| invalid(id, resolved))
                .collect()
        };
        for id in invalid {
            self.inputs.project_sources.remove_shared(&id);
        }
        self.inputs.clone()
    }
}

impl RuntimeState {
    pub(crate) fn browser_conversations(
        &self,
    ) -> tokio::sync::watch::Receiver<SnapshotMap<String, BrowserTranscript>> {
        self.browser
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .transcripts
            .subscribe()
    }

    pub(crate) fn install_browser_conversation(
        &self,
        id: String,
        key: ConversationProjectionKey,
        transcript: BrowserTranscript,
    ) {
        let owner = self.owner();
        if !owner
            .controller()
            .state
            .sessions
            .get(&id)
            .is_some_and(|record| record.state.is_active())
        {
            return;
        }
        let Some(tail) = owner.transcripts.get(&id) else {
            return;
        };
        let latest = ConversationProjectionKey::of(&tail.header);
        if latest.ordinal < key.ordinal {
            return;
        }
        let mut browser = self.browser.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = browser.inputs.conversations.get(&id)
            && (key.ordinal < previous.key.ordinal
                || (key.ordinal == previous.key.ordinal && (key == previous.key || key != latest)))
        {
            return;
        }
        browser
            .inputs
            .conversations
            .insert(id.clone(), ConversationPreview::of(key, &transcript));
        browser.transcripts.send_modify(|transcripts| {
            transcripts.insert_shared(id, transcript);
        });
        self.publish_revision();
    }

    pub(crate) fn install_browser_project_source(
        &self,
        id: String,
        resolved: ResolvedProjectSource,
    ) {
        let owner = self.owner();
        if !resolved.key.matches(&id, owner.controller()) {
            return;
        }
        let mut browser = self.browser.lock().unwrap_or_else(PoisonError::into_inner);
        if browser.inputs.project_sources.get(&id) != Some(&resolved) {
            browser.inputs.project_sources.insert_shared(id, resolved);
            self.publish_revision();
        }
    }

    pub(crate) fn record_browser_action(
        &self,
        id: Option<&str>,
        error: Option<&str>,
        launch_failure: Option<ViewerLaunchFailure>,
    ) {
        let _owner = self.owner();
        let mut browser = self.browser.lock().unwrap_or_else(PoisonError::into_inner);
        let changed = browser.inputs.record_action(id, error, launch_failure);
        if changed {
            self.publish_revision();
        }
    }
}
