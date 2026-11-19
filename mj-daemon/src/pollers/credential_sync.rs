use super::*;
use mj_core::credentials::{LoginFailureVerdict, LoginFingerprint, RejectedLogins, SyncStep};

/// One immediate sync and notice per session per cooldown for an empty prompt
/// response, so a harness that repeats it does not flood the UI. An
/// authentication failure is never held back (#1547): repeating it is
/// idempotent (a push happens only when the profile's token differs from the
/// session's, a refusal is recorded once per token), and holding it let the
/// decision run on a later snapshot that mistook a renewal for a refusal.
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

    pub(super) fn drain_due(&mut self, now: Instant) -> Vec<(String, CredentialSyncCause)> {
        let due = self
            .pending
            .keys()
            .filter(|session_id| {
                self.pending[*session_id].signal.reason
                    == CredentialSyncReason::AuthenticationFailure
                    || self.last_attempts.get(*session_id).is_none_or(|previous| {
                        now.saturating_duration_since(*previous)
                            >= IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN
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
                (
                    pending.profile_id,
                    CredentialSyncCause {
                        session_id,
                        ordinal: pending.signal.ordinal,
                        reason: pending.signal.reason,
                        login_fingerprint: pending.signal.login_fingerprint,
                    },
                )
            })
            .collect()
    }
}

pub fn schedule_due_credential_syncs(
    tracker: &mut CredentialSyncSignalTracker,
    credential_sync: &CredentialSyncHandle,
    rejected: &std::sync::Mutex<RejectedLogins>,
    now: Instant,
) {
    for (profile_id, cause) in tracker.drain_due(now) {
        let mut rejected = rejected.lock().expect("rejected logins poisoned");
        if credential_sync.sync_profile_now(&profile_id, Some(cause.clone())) {
            rejected.accept(&profile_id, &cause);
        }
    }
}

pub(crate) async fn current_profile_login(
    profile: &mj_core::config::HarnessProfile,
) -> anyhow::Result<Option<LoginFingerprint>> {
    let kind = profile.kind;
    if kind == mj_core::config::HarnessKind::Claude {
        // A plain read (also for the Keychain): only the sync pass reads under
        // the native lock and creates a refusal; this re-checks it and may lag.
        return Ok(
            mj_controller::worker_client::current_claude_access_token(profile)
                .await
                .map(LoginFingerprint::AccessToken),
        );
    }
    let mj_core::config::HarnessLoginStore::File { marker } =
        kind.login_store(mj_core::config::HarnessHost::current(), &profile.home)
    else {
        return Ok(None);
    };
    let profile = profile.clone();
    tokio::task::spawn_blocking(move || {
        // Codex's auth scheme comes from its config file.
        if profile.auth_scheme().uses_native_login_file() {
            LoginFingerprint::read_file(kind, &marker)
        } else {
            Ok(None)
        }
    })
    .await?
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
    pub(super) last_failures:
        std::collections::BTreeMap<(String, Option<String>, Option<SyncStep>), String>,
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

/// One credential-sync notice and whether it only reports routine syncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialNotice {
    pub text: String,
    pub routine: bool,
}

impl CredentialSyncNotices {
    /// Healthy no-op cycles stay out of the UI; only actions, new failures, and
    /// answers to an event-triggered reconciliation are worth a notice.
    #[cfg(test)]
    pub fn notice(
        &mut self,
        result: &mj_core::credentials::CredentialSyncResult,
        _harness: Option<mj_core::config::HarnessKind>,
        state: &State,
    ) -> Option<String> {
        self.leveled_notice(result, state, None)
            .map(|notice| notice.text)
    }

    /// The notice with the one decision about how loudly to log it. A routine
    /// summary of syncs that went well is not a warning; a failure, an
    /// authentication problem, or anything the person has to act on is.
    pub fn leveled_notice(
        &mut self,
        result: &mj_core::credentials::CredentialSyncResult,
        state: &State,
        login_verdict: Option<&LoginFailureVerdict>,
    ) -> Option<CredentialNotice> {
        let warning = |text: String| CredentialNotice {
            text,
            routine: false,
        };
        // Completed event-triggered syncs always speak. An authentication
        // failure is handled once per failed turn; only an empty response
        // waits out the per-session cooldown upstream.
        if let Some(trigger) = &result.trigger {
            let session_id = &trigger.session_id;
            let session = state.session_notice_name(session_id);
            let sync_failure = result.failure.as_deref().or_else(|| {
                result.failures().find_map(|(failed_session, _, detail)| {
                    (failed_session == session_id).then_some(detail)
                })
            });
            if trigger.reason == CredentialSyncReason::AuthenticationFailure {
                let profile = &result.profile_id;
                // The person hears what nothing else will resolve: a refused
                // login, a reconciliation that failed, or a cause that stays
                // unknown. A stale login that was or will be replaced by the
                // renewed one needs nothing from them.
                let conclusion = match login_verdict {
                    Some(LoginFailureVerdict::Refused) => format!(
                        "the provider refused the login checked during reconciliation. Run `mj login --profile {profile}`."
                    ),
                    _ if let Some(detail) = sync_failure => {
                        format!("credential reconciliation failed: {detail}.")
                    }
                    Some(LoginFailureVerdict::Renewed) if result.pushed_to(session_id) => {
                        tracing::info!(
                            profile_id = %profile,
                            session_id = %session_id,
                            "the session held a stale login. The renewed login was pushed."
                        );
                        return None;
                    }
                    Some(LoginFailureVerdict::Renewed) => {
                        tracing::info!(
                            profile_id = %profile,
                            session_id = %session_id,
                            "the session failed on a stale login. The profile's login has since been renewed."
                        );
                        return None;
                    }
                    Some(LoginFailureVerdict::Unrefused { reason }) => format!(
                        "a refusal of the profile's current login was not established: {reason}."
                    ),
                    _ => return None,
                };
                return Some(warning(format!(
                    "Auth failure on profile {profile} (session {session}): {conclusion}"
                )));
            }
            if let Some(detail) = sync_failure {
                return Some(warning(format!(
                    "Session {} returned no response; credential reconciliation for profile {} failed: {detail}. The failure is recorded in the transcript.",
                    session, result.profile_id
                )));
            }
            if !result
                .outcomes
                .iter()
                .any(|outcome| outcome.session_id == *session_id)
            {
                return None;
            }
            // Either kind of replaced credentials is worth a retry.
            let replaced = result.outcomes.iter().any(|outcome| {
                outcome.session_id == *session_id
                    && outcome.outcome.as_ref().is_ok_and(|actions| {
                        actions.iter().any(|action| {
                            matches!(
                            action,
                            mj_core::credentials::CredentialSyncAction::Pushed
                                | mj_core::credentials::CredentialSyncAction::AwsCredentialsPushed
                        )
                        })
                    })
            });
            return Some(warning(if replaced {
                format!(
                    "Session {} returned no response; fresher credentials from profile {} were pushed. Retry the prompt.",
                    session, result.profile_id
                )
            } else {
                format!(
                    "Session {} returned no response; profile {} had no newer credentials to push. The failure is recorded in the transcript.",
                    session, result.profile_id
                )
            }));
        }

        let mut failures = std::collections::BTreeMap::new();
        if let Some(detail) = &result.failure {
            failures.insert(
                (result.profile_id.clone(), None, None),
                format!(
                    "Credential sync for profile {} failed: {detail}",
                    result.profile_id
                ),
            );
        }
        // Every failure of a step is said: a second one under the same key is
        // added to the first, never put in its place.
        let mut details = std::collections::BTreeMap::<_, Vec<&str>>::new();
        for (session_id, step, detail) in result.failures() {
            details
                .entry((result.profile_id.clone(), Some(session_id.to_owned()), step))
                .or_default()
                .push(detail);
        }
        for (key, details) in details {
            let session = state.session_notice_name(key.1.as_deref().unwrap_or_default());
            failures.insert(
                key,
                format!(
                    "Credential sync for profile {} (session {session}) failed: {}",
                    result.profile_id,
                    details.join("; ")
                ),
            );
        }
        // A key that stopped failing is forgotten silently, so the same failure
        // after a clean cycle is reported again. A result can end only the
        // failures of the steps it checked: the renewal task's result is about
        // one session's role-scoped AWS credentials, and every other result
        // is about everything but those.
        self.last_failures.retain(|key, _| {
            key.0 != result.profile_id
                || failures.contains_key(key)
                || if result.partial {
                    !(key.2 == Some(SyncStep::Aws)
                        && key.1.as_ref().is_some_and(|session| {
                            result
                                .outcomes
                                .iter()
                                .any(|outcome| outcome.session_id == *session)
                        }))
                } else {
                    key.2 == Some(SyncStep::Aws)
                }
        });
        // Every key is recorded as reported, so every newly failed step is
        // said now, in one notice.
        let mut new_messages = Vec::new();
        for (key, message) in failures {
            if self.last_failures.get(&key) != Some(&message) {
                new_messages.push(message.clone());
            }
            self.last_failures.insert(key, message);
        }
        let notice = (!new_messages.is_empty()).then(|| new_messages.join(" "));
        if let Some(text) = notice {
            return Some(warning(text));
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
        let slopcop_pushed = result.slopcop_login_pushed_sessions();
        if slopcop_pushed > 0 {
            parts.push(format!(
                "Synced the slopcop login to {slopcop_pushed} session(s)."
            ));
        }
        let slopcop_removed = result.slopcop_login_removed_sessions();
        if slopcop_removed > 0 {
            parts.push(format!(
                "Removed the slopcop login from {slopcop_removed} session(s)."
            ));
        }
        (!parts.is_empty()).then(|| CredentialNotice {
            text: parts.join(" "),
            routine: true,
        })
    }
}
