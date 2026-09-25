use super::*;

/// One immediate sync and notice per session per cooldown, so a harness that
/// repeats the same failed turn does not flood the UI.
pub const IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingCredentialSync {
    pub(super) signal: CredentialSyncSignal,
    pub(super) profile_id: String,
}

/// Deduplicates the actor's sticky failure marker while retaining a newer
/// failure until its session cooldown expires.
#[derive(Debug, Default)]
pub struct CredentialSyncSignalTracker {
    pub(super) handled_ordinals: std::collections::BTreeMap<String, u64>,
    pub(super) last_attempts: std::collections::BTreeMap<String, Instant>,
    pub(super) pending: std::collections::BTreeMap<String, PendingCredentialSync>,
}

impl CredentialSyncSignalTracker {
    pub fn observe(&mut self, session_id: &str, profile_id: &str, signal: CredentialSyncSignal) {
        if self
            .handled_ordinals
            .get(session_id)
            .is_some_and(|handled| *handled >= signal.ordinal)
        {
            return;
        }
        let pending = PendingCredentialSync {
            signal,
            profile_id: profile_id.to_owned(),
        };
        match self.pending.entry(session_id.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(pending);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if entry.get().signal.ordinal <= pending.signal.ordinal =>
            {
                entry.insert(pending);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }

    pub(super) fn drain_due(
        &mut self,
        now: Instant,
    ) -> Vec<(String, String, CredentialSyncReason)> {
        let due = self
            .pending
            .keys()
            .filter(|session_id| {
                self.last_attempts.get(*session_id).is_none_or(|previous| {
                    now.saturating_duration_since(*previous) >= IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        due.into_iter()
            .map(|session_id| {
                let pending = self
                    .pending
                    .remove(&session_id)
                    .expect("due credential sync signal disappeared");
                self.handled_ordinals
                    .insert(session_id.clone(), pending.signal.ordinal);
                self.last_attempts.insert(session_id.clone(), now);
                (session_id, pending.profile_id, pending.signal.reason)
            })
            .collect()
    }
}

pub fn schedule_due_credential_syncs(
    tracker: &mut CredentialSyncSignalTracker,
    credential_sync: &CredentialSyncHandle,
    now: Instant,
) {
    for (session_id, profile_id, reason) in tracker.drain_due(now) {
        credential_sync.sync_profile_now(
            &profile_id,
            Some(CredentialSyncCause { session_id, reason }),
        );
    }
}

/// Turns finished credential syncs into UI notices.
///
/// The periodic cycle revisits every profile, so a session that keeps failing
/// the same way would post the same notice forever. The last failure message
/// per key is remembered and only a changed one speaks up again. Keys are the
/// profile for a whole-sync failure and the profile plus session for a
/// per-session failure.
#[derive(Debug, Default)]
pub struct CredentialSyncNotices {
    pub(super) last_failures: std::collections::BTreeMap<(String, Option<String>), String>,
}

pub fn log_credential_sync_actions(result: &mj_core::credentials::CredentialSyncResult) {
    let sessions = result.credential_sessions();
    if sessions > 0 {
        tracing::info!(
            profile_id = %result.profile_id,
            sessions,
            "refreshed harness credentials"
        );
    }
}

/// The extra option a Claude profile has after an auth failure.
///
/// Claude Code cannot refresh its rotating login early, so a container copy
/// can lose the single-use refresh race with the host. A setup token does not
/// rotate, which takes the race away rather than retrying it.
pub(super) fn setup_token_advice(
    profile_id: &str,
    harness: Option<mj_core::config::HarnessKind>,
) -> String {
    if harness == Some(mj_core::config::HarnessKind::Claude) {
        format!(
            ", or store a long-lived token with `mj login --profile {profile_id} --setup-token`"
        )
    } else {
        String::new()
    }
}

impl CredentialSyncNotices {
    /// Healthy no-op cycles stay out of the UI; only actions, new failures, and
    /// answers to an event-triggered reconciliation are worth a notice.
    pub fn notice(
        &mut self,
        result: &mj_core::credentials::CredentialSyncResult,
        harness: Option<mj_core::config::HarnessKind>,
        state: &State,
    ) -> Option<String> {
        let advice = setup_token_advice(&result.profile_id, harness);
        // Event-triggered syncs always speak: the upstream per-session
        // cooldown, not this dedup, is what keeps them rare.
        if let Some(trigger) = &result.trigger {
            let session_id = &trigger.session_id;
            let session = state.session_notice_name(session_id);
            let sync_failure = result.failure.as_deref().or_else(|| {
                result.failures().find_map(|(failed_session, detail)| {
                    (failed_session == session_id).then_some(detail)
                })
            });
            if let Some(detail) = sync_failure {
                return Some(match trigger.reason {
                    CredentialSyncReason::AuthenticationFailure => format!(
                        "Auth failure on profile {} (session {}); credential reconciliation failed: {detail}. Run `mj login --profile {}`{advice}.",
                        result.profile_id, session, result.profile_id
                    ),
                    CredentialSyncReason::EmptyPromptResponse => format!(
                        "Session {} returned no response; credential reconciliation for profile {} failed: {detail}. The failure is recorded in the transcript.",
                        session, result.profile_id
                    ),
                });
            }
            // The first ~80 columns are all most people read before a notice
            // scrolls off, so the profile leads and the advice trails.
            return Some(match (trigger.reason, result.pushed_to(session_id)) {
                (CredentialSyncReason::AuthenticationFailure, true) => format!(
                    "Auth failure on profile {} (session {}); refreshed credentials were pushed. Retry the prompt, and if it repeats run `mj login --profile {}`{advice}.",
                    result.profile_id, session, result.profile_id
                ),
                (CredentialSyncReason::AuthenticationFailure, false) => format!(
                    "Auth failure on profile {} (session {}); nothing fresher to push. Run `mj login --profile {}`{advice}.",
                    result.profile_id, session, result.profile_id
                ),
                (CredentialSyncReason::EmptyPromptResponse, true) => format!(
                    "Session {} returned no response; fresher credentials from profile {} were pushed. Retry the prompt.",
                    session, result.profile_id
                ),
                (CredentialSyncReason::EmptyPromptResponse, false) => format!(
                    "Session {} returned no response; profile {} had no newer credentials to push. The failure is recorded in the transcript.",
                    session, result.profile_id
                ),
            });
        }

        let mut failures = std::collections::BTreeMap::new();
        if let Some(detail) = &result.failure {
            failures.insert(
                (result.profile_id.clone(), None),
                format!(
                    "Credential sync for profile {} failed: {detail}",
                    result.profile_id
                ),
            );
        }
        for (session_id, detail) in result.failures() {
            failures.insert(
                (result.profile_id.clone(), Some(session_id.to_owned())),
                format!(
                    "Credential sync for profile {} (session {}) failed: {detail}",
                    result.profile_id,
                    state.session_notice_name(session_id)
                ),
            );
        }
        // A key that stopped failing is forgotten silently, so the same failure
        // after a clean cycle is reported again.
        self.last_failures
            .retain(|key, _| key.0 != result.profile_id || failures.contains_key(key));
        let mut notice = None;
        for (key, message) in failures {
            if self.last_failures.get(&key) != Some(&message) {
                notice.get_or_insert_with(|| message.clone());
            }
            self.last_failures.insert(key, message);
        }
        if notice.is_some() {
            return notice;
        }

        let mut parts = Vec::new();
        let skills = result.skills_sessions();
        if skills > 0 {
            parts.push(format!(
                "Synced skills for profile {} to {skills} session(s).",
                result.profile_id
            ));
        }
        let github_pushed = result.github_token_pushed_sessions();
        if github_pushed > 0 {
            parts.push(format!(
                "Synced the GitHub CLI token to {github_pushed} session(s)."
            ));
        }
        let github_removed = result.github_token_removed_sessions();
        if github_removed > 0 {
            parts.push(format!(
                "Removed the GitHub CLI token from {github_removed} session(s)."
            ));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}
