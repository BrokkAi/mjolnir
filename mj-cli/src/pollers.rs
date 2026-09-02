//! Background feeds for the control surfaces.
//!
//! Everything here runs off the event loop and reports back over a channel:
//! harness quota refreshes, worker session polling, per-session resource and
//! deployment capacity probes, credential-sync scheduling, and the one-shot
//! tasks that recover interrupted closes. The loop that consumes them never
//! blocks; see [`Feed`] for the wait-then-drain shape they all share.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hel::clock::epoch_seconds;
use hel::hel_config::HelConfig;
use hel::hel_controller::Controller;
use hel::hel_credentials::{
    CredentialSyncCause, CredentialSyncHandle, CredentialSyncReason, CredentialSyncSignal,
    CredentialSyncTarget,
};
use hel::hel_quota::{QuotaManager, QuotaRefreshOutcome, QuotaRefreshRequest};
use hel::hel_recovery::{RecoveryCoordinator, RecoveryResult};
use hel::hel_session_manager::{
    ManagedSessionView, RelaySessionTarget, RemoteSessionRequest, SessionManagerControl,
    SessionManagerShutdown, SessionManagerUpdate, SessionManagerUpdates, ViewError,
    spawn_remote_session_manager,
};
use hel::hel_state::{
    HelState, ManagedSessionSnapshot, MaterializedSession, SessionRecord,
    SessionResourceAllocation, SessionState,
};
use hel::hel_targets::{
    CancellableProcessExecutor, CommandOutput, CommandSpec, DeploymentCapacityKind,
    DeploymentCapacityTarget, DeploymentCapacityUsage, SessionResourceProbe, SessionResourceUsage,
};
use hel::hel_worker_client::CredentialSyncCoordinator;
use hel_tui::DashboardState;

use crate::daemon;
use crate::dashboard::io::DashboardIoUpdate;
use crate::short_id;

pub(crate) const QUOTA_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// When a quota reading stops counting as current. A reading only goes stale
/// once a scheduled refresh should already have replaced it, so this is
/// derived from the refresh interval rather than chosen next to it: a shorter
/// threshold would label every healthy quota "stale" for part of every cycle.
/// The extra interval is slack for a refresh that is itself still running.
pub(crate) const QUOTA_STALE_AFTER: Duration =
    Duration::from_secs(2 * QUOTA_REFRESH_INTERVAL.as_secs());
pub(crate) const RESOURCE_POLL_INTERVAL: Duration = Duration::from_secs(60);
const RESOURCE_POLL_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const CAPACITY_POLL_INTERVAL: Duration = Duration::from_secs(30);
const WORKER_DIAGNOSIS_TIMEOUT: Duration = Duration::from_secs(15);

/// Something a control loop waits on and then drains: one awaited receive for
/// the `select!` arm, and a non-blocking receive for the batch that follows.
pub(crate) trait FeedSource {
    type Item;

    /// Cancel-safe: a wait that loses the race must not drop a message.
    fn wait(&mut self) -> impl Future<Output = Option<Self::Item>>;

    fn poll_now(&mut self) -> Option<Self::Item>;
}

impl<T> FeedSource for tokio::sync::mpsc::Receiver<T> {
    type Item = T;

    fn wait(&mut self) -> impl Future<Output = Option<T>> {
        self.recv()
    }

    fn poll_now(&mut self) -> Option<T> {
        self.try_recv().ok()
    }
}

impl<T> FeedSource for tokio::sync::mpsc::UnboundedReceiver<T> {
    type Item = T;

    fn wait(&mut self) -> impl Future<Output = Option<T>> {
        self.recv()
    }

    fn poll_now(&mut self) -> Option<T> {
        self.try_recv().ok()
    }
}

impl<T: Clone> FeedSource for tokio::sync::watch::Receiver<T> {
    type Item = T;

    async fn wait(&mut self) -> Option<T> {
        self.changed().await.ok()?;
        Some(self.borrow_and_update().clone())
    }

    fn poll_now(&mut self) -> Option<T> {
        self.has_changed()
            .ok()
            .filter(|changed| *changed)
            .map(|_| self.borrow_and_update().clone())
    }
}

impl FeedSource for SessionManagerUpdates {
    type Item = SessionManagerUpdate;

    fn wait(&mut self) -> impl Future<Output = Option<SessionManagerUpdate>> {
        self.recv()
    }

    fn poll_now(&mut self) -> Option<SessionManagerUpdate> {
        self.try_recv().ok()
    }
}

impl FeedSource for RecoveryCoordinator {
    type Item = RecoveryResult;

    fn wait(&mut self) -> impl Future<Output = Option<RecoveryResult>> {
        self.result()
    }

    fn poll_now(&mut self) -> Option<RecoveryResult> {
        self.try_result()
    }
}

impl FeedSource for CredentialSyncCoordinator {
    type Item = hel::hel_credentials::CredentialSyncResult;

    fn wait(&mut self) -> impl Future<Output = Option<Self::Item>> {
        self.result()
    }

    fn poll_now(&mut self) -> Option<Self::Item> {
        self.try_result()
    }
}

/// One background feed as a control loop uses it.
///
/// The `select!` arm hands the message that woke the loop to [`Feed::accept`],
/// and the drain that follows walks [`Feed::next_ready`] until the feed is
/// empty, so a burst of updates costs one draw. A closed channel reports `None`
/// for ever, which would leave its arm permanently ready; `accept` retires the
/// feed instead, and [`Feed::is_open`] gates the arm.
pub(crate) struct Feed<S: FeedSource> {
    source: S,
    pending: Option<S::Item>,
    open: bool,
}

impl<S: FeedSource> Feed<S> {
    pub(crate) fn new(source: S) -> Self {
        Self {
            source,
            pending: None,
            open: true,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn wait(&mut self) -> impl Future<Output = Option<S::Item>> {
        self.source.wait()
    }

    /// Latches the message that won the select and reports whether the loop
    /// must redraw.
    pub(crate) fn accept(&mut self, message: Option<S::Item>) -> bool {
        match message {
            Some(message) => {
                self.pending = Some(message);
                true
            }
            None => {
                self.open = false;
                false
            }
        }
    }

    /// The next message for the batch drain: the one that won the select
    /// first, then whatever queued behind it.
    pub(crate) fn next_ready(&mut self) -> Option<S::Item> {
        self.pending.take().or_else(|| self.source.poll_now())
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct QuotaRefreshBatch {
    pub(crate) generation: u64,
    pub(crate) profiles: Vec<QuotaRefreshRequest>,
}

#[derive(Debug)]
pub(crate) enum QuotaUpdate {
    Refreshing { profile_ids: Vec<String> },
    Report(QuotaRefreshOutcome),
    Finished { generation: u64 },
}

pub(crate) type WorkerPollTarget = RelaySessionTarget;
pub(crate) type WorkerPollUpdate = SessionManagerUpdate;

#[derive(Debug)]
struct WorkerDiagnosisEpisode {
    id: u64,
    error: String,
    diagnosed: bool,
}

#[derive(Debug, Default)]
pub(crate) struct WorkerDiagnosisTracker {
    next_episode: u64,
    current: std::collections::BTreeMap<String, WorkerDiagnosisEpisode>,
    pending: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkerDiagnosisCompletion {
    pub(crate) display_error: Option<String>,
    pub(crate) restart_episode: Option<u64>,
}

impl WorkerDiagnosisTracker {
    pub(crate) fn observe(
        &mut self,
        session_id: &str,
        connected: bool,
        error: Option<String>,
    ) -> Option<u64> {
        if connected || error.is_none() {
            self.current.remove(session_id);
        }
        let error = error?;
        let episode = self
            .current
            .entry(session_id.to_owned())
            .or_insert_with(|| {
                self.next_episode = self.next_episode.wrapping_add(1).max(1);
                WorkerDiagnosisEpisode {
                    id: self.next_episode,
                    error: error.clone(),
                    diagnosed: false,
                }
            });
        episode.error = error;
        if episode.diagnosed || self.pending.contains_key(session_id) {
            return None;
        }
        self.pending.insert(session_id.to_owned(), episode.id);
        Some(episode.id)
    }

    pub(crate) fn finish(
        &mut self,
        session_id: &str,
        episode_id: u64,
    ) -> WorkerDiagnosisCompletion {
        if self.pending.get(session_id) != Some(&episode_id) {
            return WorkerDiagnosisCompletion::default();
        }
        self.pending.remove(session_id);
        let Some(current) = self.current.get_mut(session_id) else {
            return WorkerDiagnosisCompletion::default();
        };
        if current.id == episode_id {
            current.diagnosed = true;
            return WorkerDiagnosisCompletion {
                display_error: Some(current.error.clone()),
                restart_episode: None,
            };
        }
        if !current.diagnosed {
            self.pending.insert(session_id.to_owned(), current.id);
            return WorkerDiagnosisCompletion {
                display_error: None,
                restart_episode: Some(current.id),
            };
        }
        WorkerDiagnosisCompletion::default()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResourcePollTarget {
    session_id: String,
    probe: SessionResourceProbe,
}

#[derive(Debug)]
pub(crate) struct ResourcePollUpdate {
    pub(crate) session_id: String,
    pub(crate) usage: SessionResourceUsage,
}

#[derive(Debug)]
pub(crate) struct CapacityPollUpdate {
    pub(crate) target_id: String,
    pub(crate) result: std::result::Result<Option<DeploymentCapacityUsage>, String>,
    pub(crate) sampled_at_epoch_seconds: u64,
}

pub(crate) fn projected_queued_prompts(
    controller: &Controller,
) -> Result<std::collections::BTreeMap<String, Vec<hel::hel_worker::QueuedPrompt>>> {
    let queues = hel::hel_database::load_materialized_queued_prompts()?;
    Ok(controller
        .state
        .sessions
        .keys()
        .filter_map(|session_id| {
            queues
                .get(session_id)
                .map(|queue| (session_id.clone(), queued_prompt_entries(queue)))
        })
        .collect())
}

pub(crate) fn quota_refresh_profiles(controller: &Controller) -> Vec<QuotaRefreshRequest> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    controller
        .config
        .profiles
        .iter()
        .map(|(id, profile)| {
            let mut environment = profile.environment.clone();
            environment.insert(
                profile.home_env().to_string(),
                profile.home.to_string_lossy().into_owned(),
            );
            QuotaRefreshRequest {
                profile_id: id.clone(),
                harness: profile.kind,
                source_home: profile.home.clone(),
                executable: profile.executable.clone(),
                environment,
                cwd: cwd.clone(),
            }
        })
        .collect()
}

pub(crate) fn spawn_quota_refresher() -> (
    tokio::sync::watch::Sender<QuotaRefreshBatch>,
    tokio::sync::mpsc::Receiver<QuotaUpdate>,
) {
    let (profiles_tx, mut profiles_rx) = tokio::sync::watch::channel(QuotaRefreshBatch::default());
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let mut quotas = QuotaManager::default();
        let mut batch = QuotaRefreshBatch::default();
        let mut interval = tokio::time::interval(QUOTA_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick(), if !batch.profiles.is_empty() => {
                    if !refresh_profile_quotas(
                        &mut quotas,
                        batch.generation,
                        &batch.profiles,
                        &updates_tx,
                    ).await {
                        break;
                    }
                }
                changed = profiles_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("quota profile target feed closed; stopping quota refresher");
                        break;
                    }
                    batch = profiles_rx.borrow_and_update().clone();
                    if !refresh_profile_quotas(
                        &mut quotas,
                        batch.generation,
                        &batch.profiles,
                        &updates_tx,
                    ).await {
                        break;
                    }
                }
            }
        }
        quotas.shutdown().await;
    });
    (profiles_tx, updates_rx)
}

async fn refresh_profile_quotas(
    quotas: &mut QuotaManager,
    generation: u64,
    profiles: &[QuotaRefreshRequest],
    updates: &tokio::sync::mpsc::Sender<QuotaUpdate>,
) -> bool {
    let ids = profiles
        .iter()
        .map(|profile| profile.profile_id.clone())
        .collect::<Vec<_>>();
    if updates
        .send(QuotaUpdate::Refreshing { profile_ids: ids })
        .await
        .is_err()
    {
        tracing::debug!("quota update consumer closed before refresh started");
        return false;
    }
    // Keep draining even if the UI is gone so codex clients return to the
    // manager for a clean shutdown; just stop sending.
    let delivered = AtomicBool::new(true);
    quotas
        .refresh_profiles(profiles.to_vec(), |quota| {
            let delivered = &delivered;
            async move {
                if delivered.load(Ordering::Acquire)
                    && updates.send(QuotaUpdate::Report(quota)).await.is_err()
                {
                    tracing::debug!("quota update consumer closed while reporting a profile");
                    delivered.store(false, Ordering::Release);
                }
            }
        })
        .await;
    if !delivered.into_inner() {
        return false;
    }
    if updates
        .send(QuotaUpdate::Finished { generation })
        .await
        .is_err()
    {
        tracing::debug!(
            generation,
            "quota update consumer closed before refresh completed"
        );
        false
    } else {
        true
    }
}

pub(crate) fn complete_manual_quota_refresh(
    pending_generation: &mut Option<u64>,
    completed_generation: u64,
) -> bool {
    if *pending_generation != Some(completed_generation) {
        return false;
    }
    *pending_generation = None;
    true
}

pub(crate) fn dashboard_worker_targets(controller: &Controller) -> Vec<WorkerPollTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session_target_is_pollable(session))
        .filter_map(|session| {
            let spec = match controller.reconnect_command(&session.id) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build worker poll target: {error:#}");
                    return None;
                }
            };
            Some(WorkerPollTarget {
                session_id: session.id.clone(),
                spec,
                worker_recovery: match controller.worker_recovery_plan(&session.id) {
                    Ok(plan) => Some(plan),
                    Err(error) => {
                        tracing::debug!(session_id = %session.id, "worker recovery target unavailable: {error:#}");
                        None
                    }
                },
                project_memory: match controller.project_memory_sync_target(&session.id) {
                    Ok(target) => Some(target),
                    Err(error) => {
                        tracing::debug!(session_id = %session.id, "project memory target unavailable: {error:#}");
                        None
                    }
                },
            })
        })
        .collect()
}

pub(crate) fn dashboard_worker_targets_excluding(
    controller: &Controller,
    excluded_sessions: &std::collections::BTreeSet<String>,
) -> Vec<WorkerPollTarget> {
    let mut targets = dashboard_worker_targets(controller);
    targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    targets
}

/// Sessions whose worker can answer credential requests right now. Sessions
/// still provisioning or already disconnected would only produce connection
/// errors, so they stay out.
pub(crate) fn credential_sync_targets(controller: &Controller) -> Vec<CredentialSyncTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| {
            matches!(
                session.state,
                SessionState::Running | SessionState::Checkpointing
            ) && session.target.is_some()
        })
        .filter_map(|session| {
            let profile = controller.config.profiles.get(&session.last_profile)?;
            let spec = match controller.reconnect_command(&session.id) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build credential sync target: {error:#}");
                    return None;
                }
            };
            let sync_github_token = target_syncs_github_token(session.target.as_ref());
            Some(CredentialSyncTarget {
                session_id: session.id.clone(),
                profile_id: session.last_profile.clone(),
                harness: profile.kind,
                profile_home: profile.home.clone(),
                sync_github_token,
                spec,
            })
        })
        .collect()
}

fn target_syncs_github_token(target: Option<&hel::hel_state::TargetLocator>) -> bool {
    target.is_some()
        && !matches!(
            target,
            Some(hel::hel_state::TargetLocator::LocalBare { .. })
        )
}

/// One immediate sync and notice per session per cooldown, so a harness that
/// repeats the same failed turn does not flood the UI.
pub(crate) const IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingCredentialSync {
    signal: CredentialSyncSignal,
    profile_id: String,
}

/// Deduplicates the actor's sticky failure marker while retaining a newer
/// failure until its session cooldown expires.
#[derive(Debug, Default)]
pub(crate) struct CredentialSyncSignalTracker {
    handled_ordinals: std::collections::BTreeMap<String, u64>,
    last_attempts: std::collections::BTreeMap<String, Instant>,
    pending: std::collections::BTreeMap<String, PendingCredentialSync>,
}

impl CredentialSyncSignalTracker {
    pub(crate) fn observe(
        &mut self,
        session_id: &str,
        profile_id: &str,
        signal: CredentialSyncSignal,
    ) {
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

    fn drain_due(&mut self, now: Instant) -> Vec<(String, String, CredentialSyncReason)> {
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

pub(crate) fn schedule_due_credential_syncs(
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
pub(crate) struct CredentialSyncNotices {
    last_failures: std::collections::BTreeMap<(String, Option<String>), String>,
}

pub(crate) fn log_credential_sync_actions(result: &hel::hel_credentials::CredentialSyncResult) {
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
fn setup_token_advice(profile_id: &str, harness: Option<hel::hel_config::HarnessKind>) -> String {
    if harness == Some(hel::hel_config::HarnessKind::Claude) {
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
    pub(crate) fn notice(
        &mut self,
        result: &hel::hel_credentials::CredentialSyncResult,
        harness: Option<hel::hel_config::HarnessKind>,
    ) -> Option<String> {
        let advice = setup_token_advice(&result.profile_id, harness);
        // Event-triggered syncs always speak: the upstream per-session
        // cooldown, not this dedup, is what keeps them rare.
        if let Some(trigger) = &result.trigger {
            let session_id = &trigger.session_id;
            let sync_failure = result.failure.as_deref().or_else(|| {
                result.failures().find_map(|(failed_session, detail)| {
                    (failed_session == session_id).then_some(detail)
                })
            });
            if let Some(detail) = sync_failure {
                return Some(match trigger.reason {
                    CredentialSyncReason::AuthenticationFailure => format!(
                        "Auth failure on profile {} (session {}); credential reconciliation failed: {detail}. Run `mj login --profile {}`{advice}.",
                        result.profile_id,
                        short_id(session_id),
                        result.profile_id
                    ),
                    CredentialSyncReason::EmptyPromptResponse => format!(
                        "Session {} returned no response; credential reconciliation for profile {} failed: {detail}. The failure is recorded in the transcript.",
                        short_id(session_id),
                        result.profile_id
                    ),
                });
            }
            // The first ~80 columns are all most people read before a notice
            // scrolls off, so the profile leads and the advice trails.
            return Some(match (trigger.reason, result.pushed_to(session_id)) {
                (CredentialSyncReason::AuthenticationFailure, true) => format!(
                    "Auth failure on profile {} (session {}); refreshed credentials were pushed. Retry the prompt, and if it repeats run `mj login --profile {}`{advice}.",
                    result.profile_id,
                    short_id(session_id),
                    result.profile_id
                ),
                (CredentialSyncReason::AuthenticationFailure, false) => format!(
                    "Auth failure on profile {} (session {}); nothing fresher to push. Run `mj login --profile {}`{advice}.",
                    result.profile_id,
                    short_id(session_id),
                    result.profile_id
                ),
                (CredentialSyncReason::EmptyPromptResponse, true) => format!(
                    "Session {} returned no response; fresher credentials from profile {} were pushed. Retry the prompt.",
                    short_id(session_id),
                    result.profile_id
                ),
                (CredentialSyncReason::EmptyPromptResponse, false) => format!(
                    "Session {} returned no response; profile {} had no newer credentials to push. The failure is recorded in the transcript.",
                    short_id(session_id),
                    result.profile_id
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
                    short_id(session_id)
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

fn dashboard_resource_targets(controller: &Controller) -> Vec<ResourcePollTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session_target_is_pollable(session))
        .filter_map(|session| {
            match controller.resource_probe(&session.id) {
                Ok(probe) => Some(ResourcePollTarget {
                    session_id: session.id.clone(),
                    probe,
                }),
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build resource poll target: {error:#}");
                    None
                }
            }
        })
        .collect()
}

/// `is_active` means visible on the active dashboard, not necessarily backed
/// by a live target. A recoverable error stays visible so the user can resume
/// its checkpoint, but its failed target must not keep reconnecting or being
/// sampled.
///
/// `Provisioning` is excluded for the same reason `credential_sync_targets`
/// excludes it, and for a sharper one: a session gets its `target` as soon as
/// the target itself exists, which is *before* its worker binary has been
/// copied into place. Polling that window means running `execve` on a file
/// `cp` still holds open for writing, which fails with `ETXTBSY` and leaves
/// the session recorded as unreachable. Provisioning connects to its own
/// worker when it is ready and then marks the session `Running`, which is when
/// there is something here to poll.
pub(crate) fn session_target_is_pollable(session: &hel::hel_state::SessionRecord) -> bool {
    session.state.is_active()
        && !matches!(
            session.state,
            SessionState::Error | SessionState::Provisioning
        )
        && session.target.is_some()
}

pub(crate) fn refresh_dashboard_poll_targets(
    controller: &Controller,
    worker_targets_tx: &tokio::sync::watch::Sender<Vec<WorkerPollTarget>>,
    resource_targets_tx: &tokio::sync::watch::Sender<Vec<ResourcePollTarget>>,
    credential_sync: &CredentialSyncHandle,
    excluded_sessions: &std::collections::BTreeSet<String>,
) {
    let worker_targets = dashboard_worker_targets_excluding(controller, excluded_sessions);
    worker_targets_tx.send_replace(worker_targets);
    let mut resource_targets = dashboard_resource_targets(controller);
    resource_targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    resource_targets_tx.send_replace(resource_targets);
    let mut credential_targets = credential_sync_targets(controller);
    credential_targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    credential_sync.set_targets(credential_targets);
}

pub(crate) fn spawn_aws_resource_options_resolution(
    config: HelConfig,
    target_id: String,
    updates: tokio::sync::mpsc::UnboundedSender<(
        String,
        std::result::Result<Vec<SessionResourceAllocation>, String>,
    )>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(
        format!("resolving resources for {target_id}"),
        cancelled.clone(),
    );
    let _task = tokio::task::spawn_blocking(move || {
        let controller = Controller {
            config,
            state: HelState::default(),
        };
        let result = controller
            .resolve_aws_resource_options(&target_id, &CancellableProcessExecutor::new(cancelled))
            .map_err(|error| format!("{error:#}"));
        if let Err(error) = updates.send((target_id.clone(), result)) {
            tracing::debug!(target_id, %error, "AWS resource options result dropped after dashboard shutdown");
        }
        drop(guard);
    });
}

pub(crate) fn spawn_dashboard_resource_poller() -> (
    tokio::sync::watch::Sender<Vec<ResourcePollTarget>>,
    tokio::sync::mpsc::Sender<String>,
    tokio::sync::mpsc::Receiver<ResourcePollUpdate>,
) {
    let (targets_tx, mut targets_rx) =
        tokio::sync::watch::channel(Vec::<ResourcePollTarget>::new());
    let (triggers_tx, mut triggers_rx) = tokio::sync::mpsc::channel(64);
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let mut targets = std::collections::BTreeMap::new();
        let mut last_started = std::collections::BTreeMap::new();
        let mut interval = tokio::time::interval(RESOURCE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let due = targets.values().cloned().collect::<Vec<_>>();
                    for target in due {
                        schedule_resource_sample(target, &mut last_started, &updates_tx);
                    }
                }
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("resource poll target feed closed; stopping resource poller");
                        break;
                    }
                    targets = targets_rx
                        .borrow_and_update()
                        .iter()
                        .cloned()
                        .map(|target| (target.session_id.clone(), target))
                        .collect();
                    last_started.retain(|session_id, _| targets.contains_key(session_id));
                    let due = targets.values().cloned().collect::<Vec<_>>();
                    for target in due {
                        schedule_resource_sample(target, &mut last_started, &updates_tx);
                    }
                }
                session_id = triggers_rx.recv() => {
                    let Some(session_id) = session_id else {
                        break;
                    };
                    if let Some(target) = targets.get(&session_id).cloned() {
                        schedule_resource_sample(target, &mut last_started, &updates_tx);
                    }
                }
            }
        }
    });
    (targets_tx, triggers_tx, updates_rx)
}

fn resource_sample_is_due(
    last_started: Option<&tokio::time::Instant>,
    now: tokio::time::Instant,
) -> bool {
    last_started.is_none_or(|started| now.duration_since(*started) >= RESOURCE_POLL_INTERVAL)
}

fn schedule_resource_sample(
    target: ResourcePollTarget,
    last_started: &mut std::collections::BTreeMap<String, tokio::time::Instant>,
    updates: &tokio::sync::mpsc::Sender<ResourcePollUpdate>,
) {
    let now = tokio::time::Instant::now();
    if !resource_sample_is_due(last_started.get(&target.session_id), now) {
        return;
    }
    last_started.insert(target.session_id.clone(), now);
    let updates = updates.clone();
    tokio::spawn(async move {
        let usage = match tokio::time::timeout(
            RESOURCE_POLL_TIMEOUT,
            collect_session_resource_usage(&target.probe),
        )
        .await
        {
            Ok(Ok(usage)) => Some(usage),
            Ok(Err(error)) => {
                tracing::warn!(session_id = %target.session_id, "resource probe failed: {error:#}");
                None
            }
            Err(_) => {
                tracing::warn!(session_id = %target.session_id, "resource probe timed out");
                None
            }
        };
        let Some(usage) = usage else {
            return;
        };
        if let Err(error) = updates
            .send(ResourcePollUpdate {
                session_id: target.session_id.clone(),
                usage,
            })
            .await
        {
            tracing::debug!(session_id = %target.session_id, %error, "resource probe result dropped after dashboard shutdown");
        }
    });
}

async fn collect_session_resource_usage(
    probe: &SessionResourceProbe,
) -> Result<SessionResourceUsage> {
    let memory = execute_resource_command(&probe.memory).await?;
    let disk = match &probe.disk {
        Some(command) => match execute_resource_command(command).await {
            Ok(output) => Some(output),
            Err(error) => {
                tracing::debug!(purpose = %command.purpose, "optional disk resource probe failed: {error:#}");
                None
            }
        },
        None => None,
    };
    hel::hel_targets::parse_resource_usage(
        &memory.stdout,
        disk.as_ref().map(|output| output.stdout.as_slice()),
    )
}

pub(crate) fn spawn_dashboard_capacity_poller() -> (
    tokio::sync::watch::Sender<Vec<DeploymentCapacityTarget>>,
    tokio::sync::mpsc::Sender<()>,
    tokio::sync::mpsc::Receiver<CapacityPollUpdate>,
) {
    let (targets_tx, mut targets_rx) =
        tokio::sync::watch::channel(Vec::<DeploymentCapacityTarget>::new());
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(64);
    let (triggers_tx, mut triggers_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let mut targets = Vec::new();
        let mut interval = tokio::time::interval(CAPACITY_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    schedule_capacity_samples(&targets, &updates_tx);
                }
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("capacity poll target feed closed; stopping capacity poller");
                        break;
                    }
                    targets = targets_rx.borrow_and_update().clone();
                    schedule_capacity_samples(&targets, &updates_tx);
                }
                trigger = triggers_rx.recv() => {
                    if trigger.is_none() {
                        break;
                    }
                    schedule_capacity_samples(&targets, &updates_tx);
                }
            }
        }
    });
    (targets_tx, triggers_tx, updates_rx)
}

fn schedule_capacity_samples(
    targets: &[DeploymentCapacityTarget],
    updates: &tokio::sync::mpsc::Sender<CapacityPollUpdate>,
) {
    for target in targets.iter().cloned() {
        let updates = updates.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(RESOURCE_POLL_TIMEOUT, collect_capacity(&target))
                .await
                .map_err(|_| "capacity probe timed out".to_string())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            if let Err(error) = updates
                .send(CapacityPollUpdate {
                    target_id: target.id.clone(),
                    result,
                    sampled_at_epoch_seconds: epoch_seconds(),
                })
                .await
            {
                tracing::debug!(target_id = %target.id, %error, "capacity probe result dropped after dashboard shutdown");
            }
        });
    }
}

async fn collect_capacity(
    target: &DeploymentCapacityTarget,
) -> Result<Option<DeploymentCapacityUsage>> {
    if let Some(error) = &target.probe_error {
        anyhow::bail!("capacity probe is unavailable: {error}");
    }
    if target.local {
        return tokio::task::spawn_blocking(collect_local_capacity)
            .await
            .context("join local capacity probe")?
            .map(Some);
    }
    match target.kind {
        DeploymentCapacityKind::Host => {
            let mut last_error = None;
            for command in &target.probes {
                match execute_resource_command(command).await {
                    Ok(output) => {
                        return hel::hel_targets::parse_host_capacity(&output.stdout).map(Some);
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no host probe is configured")))
        }
        DeploymentCapacityKind::AwsFleet => {
            if target.probes.is_empty() {
                return Ok(None);
            }
            let mut tasks = tokio::task::JoinSet::new();
            for command in target.probes.clone() {
                tasks.spawn(async move {
                    let output = execute_resource_command(&command).await?;
                    hel::hel_targets::parse_aws_allocated_capacity(&output.stdout)
                });
            }
            let mut usages = Vec::new();
            while let Some(result) = tasks.join_next().await {
                usages.push(result.context("join EC2 capacity probe")??);
            }
            aggregate_aws_capacity(&usages).map(Some)
        }
    }
}

pub(crate) fn aggregate_aws_capacity(
    usages: &[DeploymentCapacityUsage],
) -> Result<DeploymentCapacityUsage> {
    let mut total = DeploymentCapacityUsage {
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes: 0,
        logical_cores: 0,
        disk_total_bytes: Some(0),
    };
    for usage in usages {
        total.memory_total_bytes = total
            .memory_total_bytes
            .checked_add(usage.memory_total_bytes)
            .context("aggregate EC2 RAM overflow")?;
        total.logical_cores = total
            .logical_cores
            .checked_add(usage.logical_cores)
            .context("aggregate EC2 core count overflow")?;
        total.disk_total_bytes = Some(
            total
                .disk_total_bytes
                .unwrap_or(0)
                .checked_add(usage.disk_total_bytes.unwrap_or(0))
                .context("aggregate EC2 disk overflow")?,
        );
    }
    Ok(total)
}

fn collect_local_capacity() -> Result<DeploymentCapacityUsage> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    system.refresh_cpu_all();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    system.refresh_cpu_usage();
    Ok(DeploymentCapacityUsage {
        cpu_percent: Some(system.global_cpu_usage().round().clamp(0.0, 100.0) as u8),
        memory_used_bytes: system
            .total_memory()
            .saturating_sub(system.available_memory()),
        memory_total_bytes: system.total_memory(),
        logical_cores: system
            .cpus()
            .len()
            .try_into()
            .context("logical CPU count overflow")?,
        disk_total_bytes: None,
    })
}

async fn execute_resource_command(command: &CommandSpec) -> Result<CommandOutput> {
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .envs(&command.env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = process
        .spawn()
        .with_context(|| format!("start {} for {}", command.program, command.purpose))?;
    // stdin is null; nothing writes while output drains, so this cannot hit
    // the write-then-wait deadlock the disallowed_methods lint guards against.
    #[allow(clippy::disallowed_methods)]
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("wait for {}", command.purpose))?;
    let command_output = CommandOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    };
    if command_output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            command_output.status,
            String::from_utf8_lossy(&command_output.stderr).trim()
        );
    }
    Ok(command_output)
}

pub(crate) struct RemoteDashboardWorkerPoller {
    pub(crate) targets: tokio::sync::watch::Sender<Vec<WorkerPollTarget>>,
    pub(crate) updates: SessionManagerUpdates,
    pub(crate) control: SessionManagerControl,
    pub(crate) shutdown: SessionManagerShutdown,
    pub(crate) lifecycles: tokio::sync::watch::Receiver<Vec<daemon::RuntimeLifecycleView>>,
    /// Reviews the daemon is running for this workspace's sessions.
    pub(crate) reviews: tokio::sync::watch::Receiver<Vec<hel::hel_review::host::RuntimeReviewView>>,
    pub(crate) config: tokio::sync::watch::Receiver<hel::hel_config::HelConfig>,
    pub(crate) records: tokio::sync::watch::Receiver<Vec<SessionRecord>>,
}

/// What a session looked like the last time a view was published for it.
///
/// The poller compares this before reading anything, so a session that has not
/// moved costs one comparison rather than a full transcript load. Nothing here
/// grows with the transcript: the projection is identified by its ordinal and
/// digest, and the operational state is bounded by the relay's own command and
/// configuration surface.
#[derive(Debug, Clone, PartialEq)]
struct PublishedView {
    projection_ordinal: u64,
    projection_digest: String,
    operational: Option<hel::hel_worker::RelayOperationalState>,
    connected: bool,
    error: Option<String>,
}

impl PublishedView {
    fn of(runtime: &crate::daemon::RuntimeSessionView) -> Self {
        Self {
            projection_ordinal: runtime.projection_ordinal,
            projection_digest: runtime.projection_digest.clone(),
            operational: runtime.operational.clone(),
            connected: runtime.connected,
            error: runtime.error.as_ref().map(|error| format!("{error:?}")),
        }
    }

    fn matches(&self, runtime: &crate::daemon::RuntimeSessionView) -> bool {
        *self == Self::of(runtime)
    }
}

const PROJECTION_CONVERGENCE_RETRIES: u8 = 20;
const PROJECTION_CONVERGENCE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionMismatch {
    published_ordinal: u64,
    published_digest: String,
    durable_ordinal: u64,
    durable_digest: String,
}

#[derive(Default)]
struct ProjectionConvergence {
    attempts: std::collections::BTreeMap<String, (ProjectionMismatch, u8)>,
}

impl ProjectionConvergence {
    fn converged(&mut self, session_id: &str) {
        self.attempts.remove(session_id);
    }

    /// Give a lifecycle rollback and the daemon's cached relay view a bounded
    /// window to converge. Repeating the same mismatch eventually reports the
    /// integrity failure instead of hiding it indefinitely.
    fn should_retry(&mut self, session_id: &str, mismatch: ProjectionMismatch) -> bool {
        let entry = self
            .attempts
            .entry(session_id.to_owned())
            .or_insert_with(|| (mismatch.clone(), 0));
        if entry.0 != mismatch {
            *entry = (mismatch, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= PROJECTION_CONVERGENCE_RETRIES
    }
}

pub(crate) fn spawn_remote_dashboard_worker_poller(
    workspace_id: String,
) -> Result<RemoteDashboardWorkerPoller> {
    let channels = spawn_remote_session_manager()?;
    let hel::hel_session_manager::RemoteSessionManagerChannels {
        targets,
        control,
        updates,
        shutdown,
        publisher,
        mut requests,
    } = channels;
    let (lifecycle_tx, lifecycle_rx) = tokio::sync::watch::channel(Vec::new());
    let (reviews_tx, reviews_rx) = tokio::sync::watch::channel(Vec::new());
    let (config_tx, config_rx) = tokio::sync::watch::channel(hel::hel_config::HelConfig::default());
    let (records_tx, records_rx) = tokio::sync::watch::channel(Vec::new());
    tokio::spawn(async move {
        let mut revision = 0_u64;
        let mut requests_open = true;
        let mut projection_convergence = ProjectionConvergence::default();
        // What was last published for each session, so an unchanged session
        // costs nothing. Without this every poll re-read and re-deserialised
        // every live session's whole transcript to discover that none of it
        // had moved, which made the cost of showing anything proportional to
        // everything that had ever happened in the conversation. The stored
        // value is bounded by the relay's operational state; the transcript
        // itself is never held here.
        let mut published = std::collections::BTreeMap::<String, PublishedView>::new();
        // One session's requests reach the daemon in the order they were made;
        // different sessions still overlap.
        let mut request_order = hel::hel_session_manager::SessionRequestOrder::new();
        loop {
            tokio::select! {
                request = requests.recv(), if requests_open => {
                    let Some(request) = request else {
                        requests_open = false;
                        continue;
                    };
                    request_order.dispatch(request, forward_remote_session_request);
                }
                snapshot = poll_daemon_runtime(workspace_id.clone(), revision) => {
                    match snapshot {
                        Ok(snapshot) => {
                            let snapshot_revision = snapshot.revision;
                            let mut retry_projection = false;
                            config_tx.send_if_modified(|config| {
                                if *config == snapshot.config {
                                    false
                                } else {
                                    *config = snapshot.config.clone();
                                    true
                                }
                            });
                            records_tx.send_if_modified(|records| {
                                if *records == snapshot.records {
                                    false
                                } else {
                                    records.clone_from(&snapshot.records);
                                    true
                                }
                            });
                            lifecycle_tx.send_replace(snapshot.lifecycles);
                            reviews_tx.send_replace(snapshot.reviews);
                            for runtime in snapshot.sessions {
                                let session_id = runtime.session_id.clone();
                                // Nothing about this session has moved, so
                                // there is nothing to read and nothing to say.
                                // Consumers hold the last view they were sent.
                                if published
                                    .get(&session_id)
                                    .is_some_and(|last| last.matches(&runtime))
                                {
                                    continue;
                                }
                                let fingerprint = PublishedView::of(&runtime);
                                let view = match runtime.operational.clone() {
                                    Some(operational) => {
                                        // Bounded: the window is everything any
                                        // viewer shows. Reading the whole
                                        // transcript here was work proportional
                                        // to the conversation, on every poll a
                                        // session moved.
                                        let loaded = tokio::task::spawn_blocking({
                                            let session_id = session_id.clone();
                                            move || hel::hel_database::load_materialized_projection_tail(
                                                &session_id,
                                                hel::hel_database::PROJECTION_TAIL_ITEMS,
                                            )
                                        }).await;
                                        match loaded {
                                            Ok(Ok(Some((materialized, window))))
                                                if materialized.applied_event_ordinal > runtime.projection_ordinal
                                                    || (materialized.applied_event_ordinal == runtime.projection_ordinal
                                                        && materialized.applied_event_digest == runtime.projection_digest) =>
                                            {
                                                projection_convergence.converged(&session_id);
                                                ManagedSessionView {
                                                    snapshot: Some(ManagedSessionSnapshot {
                                                        materialized,
                                                        window,
                                                        operational,
                                                        latest_credential_sync_signal:
                                                            runtime.latest_credential_sync_signal,
                                                    }),
                                                    connected: runtime.connected,
                                                    error: runtime.error,
                                                }
                                            }
                                            Ok(Ok(Some((materialized, _)))) => {
                                                let mismatch = ProjectionMismatch {
                                                    published_ordinal: runtime.projection_ordinal,
                                                    published_digest: runtime.projection_digest.clone(),
                                                    durable_ordinal: materialized.applied_event_ordinal,
                                                    durable_digest: materialized.applied_event_digest.clone(),
                                                };
                                                if projection_convergence.should_retry(&session_id, mismatch) {
                                                    retry_projection = true;
                                                    continue;
                                                }
                                                let detail = if materialized.applied_event_ordinal
                                                    < runtime.projection_ordinal
                                                {
                                                    format!(
                                                        "daemon published projection {} but SQLite contains only {} after a bounded convergence retry",
                                                        runtime.projection_ordinal,
                                                        materialized.applied_event_ordinal,
                                                    )
                                                } else {
                                                    format!(
                                                        "daemon and SQLite projection digests differ at ordinal {} after a bounded convergence retry",
                                                        runtime.projection_ordinal,
                                                    )
                                                };
                                                ManagedSessionView {
                                                    snapshot: None,
                                                    connected: false,
                                                    error: Some(ViewError::ProjectionIntegrity(detail)),
                                                }
                                            }
                                            Ok(Ok(None)) => ManagedSessionView {
                                                snapshot: None,
                                                connected: false,
                                                error: Some(ViewError::ProjectionIntegrity(
                                                    "daemon published a session with no durable projection".into(),
                                                )),
                                            },
                                            Ok(Err(error)) => ManagedSessionView {
                                                snapshot: None,
                                                connected: false,
                                                error: Some(ViewError::ProjectionIntegrity(format!(
                                                    "load daemon-owned projection: {error:#}",
                                                ))),
                                            },
                                            Err(error) => ManagedSessionView {
                                                snapshot: None,
                                                connected: false,
                                                error: Some(ViewError::ProjectionIntegrity(format!(
                                                    "projection load task failed: {error}",
                                                ))),
                                            },
                                        }
                                    }
                                    None => ManagedSessionView {
                                        snapshot: None,
                                        connected: runtime.connected,
                                        error: runtime.error,
                                    },
                                };
                                // Only a view that was actually built is
                                // remembered: an error path must be retried on
                                // the next poll rather than cached as current.
                                if view.snapshot.is_some() {
                                    published.insert(session_id.clone(), fingerprint);
                                } else {
                                    published.remove(&session_id);
                                }
                                if publisher.publish(session_id, view).await.is_err() {
                                    return;
                                }
                            }
                            if retry_projection {
                                tokio::time::sleep(PROJECTION_CONVERGENCE_RETRY_DELAY).await;
                            } else {
                                revision = revision.max(snapshot_revision);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not refresh sessions from controller daemon");
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    }
                }
            }
            if !requests_open {
                return;
            }
        }
    });
    Ok(RemoteDashboardWorkerPoller {
        targets,
        updates,
        control,
        shutdown,
        lifecycles: lifecycle_rx,
        reviews: reviews_rx,
        config: config_rx,
        records: records_rx,
    })
}

async fn poll_daemon_runtime(
    workspace_id: String,
    after_revision: u64,
) -> Result<daemon::RuntimeSnapshot> {
    let mut daemon = daemon::connect_or_start().await?;
    daemon.runtime_snapshot(workspace_id, after_revision).await
}

async fn forward_remote_session_request(request: RemoteSessionRequest) {
    match request {
        RemoteSessionRequest::Submit {
            session_id,
            command_id,
            command,
            reply,
        } => {
            let result = async {
                daemon::connect_or_start()
                    .await?
                    .submit_session_command(session_id, command_id, command)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Sync { session_id, reply } => {
            let result = async {
                daemon::connect_or_start()
                    .await?
                    .sync_session(session_id)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::RespondElicitation {
            session_id,
            elicitation_id,
            response,
            reply,
        } => {
            let result = async {
                daemon::connect_or_start()
                    .await?
                    .respond_elicitation(session_id, elicitation_id, response)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Reviewer {
            session_id,
            role,
            action,
            reply,
        } => {
            let result = async {
                daemon::connect_or_start()
                    .await?
                    .reviewer_action(session_id, role, action)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
    }
}

pub(crate) fn apply_worker_poll_update(
    controller: &mut Controller,
    dashboard: &mut DashboardState,
    update: WorkerPollUpdate,
    dashboard_io_tx: &tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    tracker: &crate::dashboard::CriticalOperationTracker,
) -> Result<bool> {
    if apply_worker_record_update(controller, &update, Some((dashboard_io_tx, tracker)))? {
        dashboard.set_state(controller.state.clone());
    }
    match update.view.error {
        Some(ViewError::Unreachable(detail)) => {
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.set_notice(format!(
                "Session {}: relay unreachable: {detail}; collecting worker diagnostics…",
                &update.session_id[..update.session_id.len().min(8)]
            ));
        }
        Some(ViewError::TargetMissing(detail)) => {
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.set_notice(format!(
                "Session {}: {detail}; recording the missing target…",
                &update.session_id[..update.session_id.len().min(8)]
            ));
            if controller
                .state
                .sessions
                .get(&update.session_id)
                .is_some_and(|session| {
                    matches!(
                        session.state,
                        SessionState::Provisioning
                            | SessionState::Running
                            | SessionState::Disconnected
                            | SessionState::Error
                    )
                })
            {
                spawn_worker_record_persistence(
                    update.session_id.clone(),
                    WorkerRecordPersistence::TargetMissing {
                        session_id: update.session_id.clone(),
                        detail,
                        updated_at: chrono::Utc::now()
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    },
                    dashboard_io_tx.clone(),
                    tracker.clone(),
                );
            }
        }
        Some(ViewError::ProjectionIntegrity(detail)) => {
            // Deterministic failure: no worker diagnostics. Like an
            // unreachable relay, it is a live poll fact and is never
            // persisted as a session last_error.
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.set_notice(format!(
                "Session {}: transcript projection failed: {detail}",
                &update.session_id[..update.session_id.len().min(8)]
            ));
        }
        None => {}
    }
    Ok(update.view.snapshot.is_some())
}

pub(crate) fn apply_worker_record_update(
    controller: &mut Controller,
    update: &WorkerPollUpdate,
    dashboard_io: Option<(
        &tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
        &crate::dashboard::CriticalOperationTracker,
    )>,
) -> Result<bool> {
    let Some(snapshot) = update.view.snapshot.as_ref() else {
        return Ok(false);
    };
    let Some(session) = controller.state.sessions.get(&update.session_id) else {
        return Ok(false);
    };
    let projected_title = snapshot.resolved_title();
    let changed_title =
        (session.acp_session_title != projected_title).then(|| projected_title.clone());
    let mut changed = false;
    if let Some(title) = changed_title {
        if dashboard_io.is_none() {
            hel::hel_database::set_session_acp_title(&update.session_id, title.as_deref())?;
        }
        controller
            .state
            .sessions
            .get_mut(&update.session_id)
            .expect("session disappeared while updating its ACP title")
            .acp_session_title = title;
        if let Some((dashboard_io_tx, tracker)) = dashboard_io {
            spawn_worker_record_persistence(
                update.session_id.clone(),
                WorkerRecordPersistence::AcpTitle {
                    title: projected_title,
                },
                dashboard_io_tx.clone(),
                tracker.clone(),
            );
        }
        changed = true;
    }
    Ok(changed)
}

#[derive(Debug)]
pub(crate) enum WorkerRecordPersistence {
    AcpTitle {
        title: Option<String>,
    },
    TargetMissing {
        session_id: String,
        detail: String,
        updated_at: String,
    },
}

#[derive(Debug)]
pub(crate) enum WorkerRecordPersistenceOutcome {
    Saved,
    TargetMissing(SessionState),
    Unchanged,
}

fn spawn_worker_record_persistence(
    session_id: String,
    operation: WorkerRecordPersistence,
    updates: tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let label = match &operation {
        WorkerRecordPersistence::AcpTitle { .. } => "saving agent title for",
        WorkerRecordPersistence::TargetMissing { .. } => "saving missing target for",
    };
    let guard = tracker.begin(format!("{label} {}", crate::short_id(&session_id)));
    tokio::spawn(async move {
        let result = async {
            let mut daemon = daemon::connect_or_start().await?;
            match &operation {
                WorkerRecordPersistence::AcpTitle { title } => daemon
                    .set_session_acp_title(session_id, title.clone())
                    .await
                    .map(|()| WorkerRecordPersistenceOutcome::Saved),
                WorkerRecordPersistence::TargetMissing {
                    session_id,
                    detail,
                    updated_at,
                } => daemon
                    .mark_session_target_missing(
                        session_id.clone(),
                        detail.clone(),
                        updated_at.clone(),
                    )
                    .await
                    .map(|state| {
                        state.map_or(
                            WorkerRecordPersistenceOutcome::Unchanged,
                            WorkerRecordPersistenceOutcome::TargetMissing,
                        )
                    }),
            }
        }
        .await
        .map_err(|error| format!("{error:#}"));
        if let Err(error) =
            updates.send(DashboardIoUpdate::WorkerRecordPersistence { operation, result })
        {
            tracing::debug!(%error, "worker record persistence result dropped after dashboard shutdown");
        }
        drop(guard);
    });
}

pub(crate) fn spawn_worker_diagnosis(
    controller: &Controller,
    session_id: String,
    episode_id: u64,
    updates: tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(
        format!("diagnosing session {}", crate::short_id(&session_id)),
        cancelled.clone(),
    );
    let diagnostic_controller = Controller {
        config: controller.config.clone(),
        state: controller.state.clone(),
    };
    tokio::spawn(async move {
        let task_session_id = session_id.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(WORKER_DIAGNOSIS_TIMEOUT);
            diagnostic_controller.diagnose_worker_controlled(&task_session_id, &executor)
        })
        .await;
        let result = joined.map_err(|error| format!("worker diagnosis task failed: {error}"));
        if let Err(error) = updates.send(DashboardIoUpdate::WorkerDiagnosis {
            session_id: session_id.clone(),
            episode_id,
            result,
        }) {
            tracing::debug!(%session_id, %error, "worker diagnosis result dropped after dashboard shutdown");
        }
        drop(guard);
    });
}

pub(crate) fn queued_prompt_projection(
    session: &MaterializedSession,
) -> Vec<hel::hel_worker::QueuedPrompt> {
    queued_prompt_entries(&session.queued_prompts)
}

fn queued_prompt_entries(
    prompts: &[hel::hel_state::MaterializedQueuedPrompt],
) -> Vec<hel::hel_worker::QueuedPrompt> {
    prompts
        .iter()
        .map(|prompt| hel::hel_worker::QueuedPrompt {
            id: prompt.command_id.clone(),
            text: hel::hel_chat::materialized_content_text(&prompt.content),
            attachments: Vec::new(),
            created_at_ms: prompt.queued_at_ms,
        })
        .collect()
}

pub(crate) enum LifecycleSuccess {
    Created,
    Resumed {
        profile_id: String,
        target_id: String,
    },
    Closed,
    ForceStopped,
    DestroyedStopped,
}

pub(crate) struct LifecycleUpdate {
    pub(crate) session_id: String,
    pub(crate) result: std::result::Result<LifecycleSuccess, String>,
}

pub(crate) fn interrupted_close_session_ids(controller: &Controller) -> Vec<String> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| {
            matches!(
                session.state,
                SessionState::Closing | SessionState::Destroying
            ) && session.target.is_some()
        })
        .map(|session| session.id.clone())
        .collect()
}

pub(crate) fn spawn_interrupted_close_recovery(
    session_id: String,
    session_manager: SessionManagerControl,
    recovery_observer: hel::hel_state::RecoveryObserver,
    cancelled: Arc<AtomicBool>,
    updates: tokio::sync::mpsc::UnboundedSender<LifecycleUpdate>,
    tracker: Option<crate::dashboard::CriticalOperationTracker>,
) -> tokio::task::JoinHandle<()> {
    let guard = tracker.map(|tracker| {
        tracker.begin_cancellable(
            format!("recovering session {}", crate::short_id(&session_id)),
            cancelled.clone(),
        )
    });
    let runtime = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        let operation_session_id = session_id.clone();
        let joined = tokio::task::spawn_blocking(move || {
            (|| -> Result<()> {
                let _recovery_reservation = reserve_recovery_or_cancel(
                    &recovery_observer,
                    &operation_session_id,
                    &cancelled,
                )?;
                let mut controller = Controller::load()?;
                let executor = CancellableProcessExecutor::new(cancelled);
                runtime.block_on(controller.recover_interrupted_close_managed(
                    &operation_session_id,
                    &executor,
                    &session_manager,
                ))
            })()
            .map(|()| LifecycleSuccess::Closed)
            .map_err(|error| format!("{error:#}"))
        })
        .await;
        let result = match joined {
            Ok(result) => result,
            Err(error) => Err(format!("interrupted close recovery task failed: {error}")),
        };
        if let Err(error) = updates.send(LifecycleUpdate {
            session_id: session_id.clone(),
            result,
        }) {
            tracing::debug!(%session_id, %error, "interrupted close result dropped after dashboard shutdown");
        }
        drop(guard);
    })
}

pub(crate) fn reserve_recovery_or_cancel(
    observer: &hel::hel_state::RecoveryObserver,
    session_id: &str,
    cancelled: &AtomicBool,
) -> Result<hel::hel_state::RecoveryReservation> {
    let reservation = observer.reserve(session_id);
    // The reservation stops the next copy; cancelling preempts the one already
    // running so a lifecycle operation never queues behind a long or wedged
    // copy.
    observer.cancel_busy(session_id);
    while observer.is_busy(session_id) {
        if cancelled.load(Ordering::Acquire) {
            bail!("operation cancelled while waiting for recovery copy");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(reservation)
}

#[cfg(test)]
mod tests {

    /// The poller used to re-read and re-deserialise every live session's whole
    /// transcript on every runtime snapshot, then compare ordinals to discover
    /// that nothing had moved. On a real session that is 28,066 rows and
    /// 635 MiB, per poll. The comparison has to happen before the read.
    #[test]
    fn an_unchanged_session_is_recognised_without_reading_its_transcript() {
        let runtime = runtime_view("session-1", 42, "digest-42");
        let published = PublishedView::of(&runtime);

        assert!(
            published.matches(&runtime),
            "an identical snapshot was treated as a change, so it would be re-read"
        );

        // Anything a viewer would notice has to defeat the skip.
        let advanced = runtime_view("session-1", 43, "digest-43");
        assert!(
            !published.matches(&advanced),
            "a moved projection was mistaken for an unchanged one"
        );

        // A digest change at the same ordinal is a rewritten projection, not a
        // quiet one: the convergence path exists precisely for this.
        let rewritten = runtime_view("session-1", 42, "digest-other");
        assert!(
            !published.matches(&rewritten),
            "a rewritten projection at the same ordinal was skipped"
        );

        // The transcript can stand still while the agent starts a turn, and a
        // viewer has to see that.
        let mut busy = runtime_view("session-1", 42, "digest-42");
        busy.connected = false;
        assert!(
            !published.matches(&busy),
            "a disconnect was skipped as unchanged"
        );
    }

    fn runtime_view(
        session_id: &str,
        projection_ordinal: u64,
        projection_digest: &str,
    ) -> crate::daemon::RuntimeSessionView {
        crate::daemon::RuntimeSessionView {
            session_id: session_id.to_owned(),
            projection_ordinal,
            projection_digest: projection_digest.to_owned(),
            operational: None,
            latest_credential_sync_signal: None,
            connected: true,
            error: None,
        }
    }
    use super::*;

    fn podman_controller(state: SessionState) -> Controller {
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut config = HelConfig::default();
        config.targets.insert(
            "podman".into(),
            hel::hel_config::TargetTemplate::LocalPodman {
                container: hel::hel_config::ContainerTemplate {
                    image: "ubuntu:24.04".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: std::collections::BTreeMap::new(),
                },
            },
        );
        let mut hel_state = HelState::default();
        hel_state.sessions.insert(
            session_id.into(),
            hel::hel_state::SessionRecord {
                workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                archived: false,
                container_cpus: None,
                container_memory: None,
                id: session_id.into(),
                title: "poll target".into(),
                harness_kind: hel::hel_config::HarnessKind::Codex,
                last_profile: "codex".into(),
                bundle_id: "project".into(),
                project_directory: None,
                managed_worktree: None,
                target_template_id: "podman".into(),
                resource_allocation: None,
                additional_mounts: Vec::new(),
                state,
                target: Some(hel::hel_state::TargetLocator::LocalPodman {
                    container_id: "a".repeat(64),
                }),
                native_session_id: None,
                acp_session_title: None,
                session_title_override: None,
                created_at: "2026-08-27T00:00:00Z".into(),
                updated_at: "2026-08-27T00:00:00Z".into(),
                viewed_through_event_ordinal: 0,
                draft_input: String::new(),
                last_error: None,
                last_checkpoint_error: None,
                checkpoint: None,
            },
        );
        Controller {
            config,
            state: hel_state,
        }
    }

    #[test]
    fn recoverable_error_session_stays_out_of_live_target_pollers() {
        let running = podman_controller(SessionState::Running);
        assert_eq!(dashboard_worker_targets(&running).len(), 1);
        assert_eq!(dashboard_resource_targets(&running).len(), 1);

        let recoverable_error = podman_controller(SessionState::Error);
        assert!(dashboard_worker_targets(&recoverable_error).is_empty());
        assert!(dashboard_resource_targets(&recoverable_error).is_empty());
    }

    /// A session gets its `target` as soon as the target exists, which is
    /// before its worker binary has finished being copied into place. Polling
    /// that window runs `execve` on a file `cp` still holds open for writing:
    /// `ETXTBSY`, and a session recorded as unreachable while it was merely
    /// still being built.
    #[test]
    fn a_provisioning_session_is_not_polled_before_its_worker_exists() {
        let provisioning = podman_controller(SessionState::Provisioning);
        assert!(
            provisioning
                .state
                .sessions
                .values()
                .all(|session| session.target.is_some())
        );

        assert!(dashboard_worker_targets(&provisioning).is_empty());
        assert!(dashboard_resource_targets(&provisioning).is_empty());

        // Provisioning connects to its own worker and then marks the session
        // running, which is when there is something to poll.
        let running = podman_controller(SessionState::Running);
        assert_eq!(dashboard_worker_targets(&running).len(), 1);
        assert_eq!(dashboard_resource_targets(&running).len(), 1);
    }

    #[test]
    fn lifecycle_owned_session_stays_out_of_worker_targets() {
        let controller = podman_controller(SessionState::Running);
        assert_eq!(dashboard_worker_targets(&controller).len(), 1);

        let excluded = controller
            .state
            .sessions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        assert!(dashboard_worker_targets_excluding(&controller, &excluded).is_empty());
    }

    #[test]
    fn projection_rollback_race_retries_before_reporting_integrity_failure() {
        let mismatch = ProjectionMismatch {
            published_ordinal: 39,
            published_digest: "published".into(),
            durable_ordinal: 36,
            durable_digest: "durable".into(),
        };
        let mut convergence = ProjectionConvergence::default();

        for _ in 0..PROJECTION_CONVERGENCE_RETRIES {
            assert!(convergence.should_retry("session-1", mismatch.clone()));
        }
        assert!(
            !convergence.should_retry("session-1", mismatch),
            "a persistent mismatch must still become an integrity error"
        );

        convergence.converged("session-1");
        assert!(convergence.attempts.is_empty());
    }

    #[test]
    fn a_changed_projection_mismatch_gets_its_own_convergence_window() {
        let mut convergence = ProjectionConvergence::default();
        let stale_lineage = ProjectionMismatch {
            published_ordinal: 39,
            published_digest: "old-lineage".into(),
            durable_ordinal: 36,
            durable_digest: "checkpoint".into(),
        };
        for _ in 0..=PROJECTION_CONVERGENCE_RETRIES {
            convergence.should_retry("session-1", stale_lineage.clone());
        }
        let equal_frontier_different_lineage = ProjectionMismatch {
            published_ordinal: 39,
            published_digest: "old-lineage".into(),
            durable_ordinal: 39,
            durable_digest: "new-lineage".into(),
        };

        assert!(convergence.should_retry("session-1", equal_frontier_different_lineage));
    }

    #[test]
    fn worker_diagnosis_is_coalesced_for_one_unreachable_episode() {
        let mut tracker = WorkerDiagnosisTracker::default();
        let episode = tracker
            .observe("session-1", false, Some("connection refused".into()))
            .unwrap();

        assert_eq!(
            tracker.observe("session-1", false, Some("still unreachable".into())),
            None
        );
        assert_eq!(
            tracker.finish("session-1", episode),
            WorkerDiagnosisCompletion {
                display_error: Some("still unreachable".into()),
                restart_episode: None,
            }
        );
        assert_eq!(
            tracker.observe("session-1", false, Some("third poll".into())),
            None
        );
    }

    #[test]
    fn stale_worker_diagnosis_is_not_published_after_reconnect() {
        let mut tracker = WorkerDiagnosisTracker::default();
        let first = tracker
            .observe("session-1", false, Some("first outage".into()))
            .unwrap();
        assert_eq!(tracker.observe("session-1", true, None), None);
        assert_eq!(
            tracker.observe("session-1", false, Some("new outage".into())),
            None
        );

        let completion = tracker.finish("session-1", first);
        assert_eq!(completion.display_error, None);
        let second = completion.restart_episode.unwrap();
        assert_eq!(
            tracker.finish("session-1", second).display_error.as_deref(),
            Some("new outage")
        );
    }

    #[test]
    fn stale_worker_diagnosis_is_not_published_after_a_terminal_poll_error() {
        let mut tracker = WorkerDiagnosisTracker::default();
        let episode = tracker
            .observe("session-1", false, Some("relay failed".into()))
            .unwrap();

        assert_eq!(tracker.observe("session-1", false, None), None);
        assert_eq!(
            tracker.finish("session-1", episode),
            WorkerDiagnosisCompletion::default()
        );
    }

    #[tokio::test]
    async fn quota_refresh_completion_keeps_its_generation() {
        let mut quotas = QuotaManager::default();
        let (updates, mut received) = tokio::sync::mpsc::channel(4);
        assert!(refresh_profile_quotas(&mut quotas, 42, &[], &updates).await);
        assert!(matches!(
            received.recv().await,
            Some(QuotaUpdate::Refreshing {
                profile_ids,
            }) if profile_ids.is_empty()
        ));
        assert!(matches!(
            received.recv().await,
            Some(QuotaUpdate::Finished { generation: 42 })
        ));

        let mut pending = Some(43);
        assert!(!complete_manual_quota_refresh(&mut pending, 42));
        assert_eq!(pending, Some(43));
        assert!(complete_manual_quota_refresh(&mut pending, 43));
        assert_eq!(pending, None);
        quotas.shutdown().await;
    }

    #[test]
    fn resource_samples_are_throttled_to_one_per_minute() {
        let started = tokio::time::Instant::now();
        assert!(!resource_sample_is_due(
            Some(&started),
            started + Duration::from_secs(59),
        ));
        assert!(resource_sample_is_due(
            Some(&started),
            started + RESOURCE_POLL_INTERVAL,
        ));
    }

    #[test]
    fn capacity_samples_refresh_every_thirty_seconds() {
        assert_eq!(CAPACITY_POLL_INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn a_new_credential_signal_waits_out_the_cooldown_without_being_lost() {
        let signal = |ordinal, reason| CredentialSyncSignal { ordinal, reason };
        let mut tracker = CredentialSyncSignalTracker::default();
        let started = Instant::now();
        tracker.observe(
            "session",
            "work",
            signal(41, CredentialSyncReason::AuthenticationFailure),
        );
        assert_eq!(
            tracker.drain_due(started),
            vec![(
                "session".into(),
                "work".into(),
                CredentialSyncReason::AuthenticationFailure
            )]
        );

        tracker.observe(
            "session",
            "work",
            signal(42, CredentialSyncReason::AuthenticationFailure),
        );
        assert!(
            tracker
                .drain_due(started + Duration::from_secs(60))
                .is_empty()
        );
        tracker.observe(
            "session",
            "new-profile",
            signal(43, CredentialSyncReason::EmptyPromptResponse),
        );
        assert_eq!(tracker.pending["session"].signal.ordinal, 43);

        // No repeated observation is needed: the loop timer drains the sticky
        // failure once its cooldown expires.
        assert_eq!(
            tracker.drain_due(started + IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN),
            vec![(
                "session".into(),
                "new-profile".into(),
                CredentialSyncReason::EmptyPromptResponse
            )]
        );
        tracker.observe(
            "session",
            "new-profile",
            signal(43, CredentialSyncReason::EmptyPromptResponse),
        );
        assert!(
            tracker
                .drain_due(started + (IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN * 2))
                .is_empty()
        );

        tracker.observe(
            "other",
            "personal",
            signal(1, CredentialSyncReason::AuthenticationFailure),
        );
        assert_eq!(
            tracker.drain_due(started + Duration::from_secs(60)),
            vec![(
                "other".into(),
                "personal".into(),
                CredentialSyncReason::AuthenticationFailure
            )]
        );
    }

    #[test]
    fn a_healthy_credential_cycle_stays_out_of_the_ui() {
        let result = hel::hel_credentials::CredentialSyncResult {
            profile_id: "work".into(),
            trigger: None,
            failure: None,
            outcomes: Vec::new(),
        };
        assert_eq!(CredentialSyncNotices::default().notice(&result, None), None);
    }

    #[test]
    fn github_tokens_sync_to_every_remote_target_but_raw_localhost() {
        use hel::hel_state::TargetLocator;

        let remotes = [
            TargetLocator::LocalPodman {
                container_id: "podman".into(),
            },
            TargetLocator::AppleContainer {
                container_id: "apple".into(),
            },
            TargetLocator::AwsEc2 {
                instance_id: "i-123".into(),
                address: Some("example.invalid".into()),
            },
            TargetLocator::SshBare {
                host: "ssh.example".into(),
                workspace: "/workspace".into(),
                worker_id: None,
            },
            TargetLocator::SshPodman {
                host: "ssh.example".into(),
                container_id: "remote-podman".into(),
            },
        ];
        for target in &remotes {
            assert!(target_syncs_github_token(Some(target)), "{target:?}");
        }
        assert!(!target_syncs_github_token(Some(
            &TargetLocator::LocalBare {
                worker_root: "/tmp/worker".into(),
            }
        )));
        assert!(!target_syncs_github_token(None));
    }

    #[test]
    fn an_authentication_failure_notice_says_whether_anything_was_pushed() {
        use hel::hel_credentials::{
            CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult,
        };

        let mut notices = CredentialSyncNotices::default();
        let pushed = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: Some(CredentialSyncCause {
                session_id: "018f9dd2-a3b4".into(),
                reason: CredentialSyncReason::AuthenticationFailure,
            }),
            failure: None,
            outcomes: vec![CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Ok(vec![CredentialSyncAction::Pushed]),
            }],
        };
        let notice = notices.notice(&pushed, None).unwrap();
        assert!(notice.contains("were pushed"), "{notice}");
        assert!(notice.contains("mj login --profile work"), "{notice}");

        let nothing_to_push = CredentialSyncResult {
            trigger: Some(CredentialSyncCause {
                session_id: "018f9dd2-a3b4".into(),
                reason: CredentialSyncReason::AuthenticationFailure,
            }),
            outcomes: Vec::new(),
            ..pushed
        };
        let notice = notices.notice(&nothing_to_push, None).unwrap();
        assert!(notice.contains("nothing fresher"), "{notice}");
        assert!(notice.contains("mj login --profile work"), "{notice}");
        // The per-session cooldown upstream limits these; the dedup must not.
        assert_eq!(notices.notice(&nothing_to_push, None), Some(notice));
    }

    #[test]
    fn a_claude_authentication_failure_offers_the_long_lived_token() {
        use hel::hel_config::HarnessKind;
        use hel::hel_credentials::{CredentialSyncOutcome, CredentialSyncResult};

        let result = CredentialSyncResult {
            profile_id: "claude-max".into(),
            trigger: Some(CredentialSyncCause {
                session_id: "018f9dd2-a3b4".into(),
                reason: CredentialSyncReason::AuthenticationFailure,
            }),
            failure: None,
            outcomes: Vec::new(),
        };

        let claude = CredentialSyncNotices::default()
            .notice(&result, Some(HarnessKind::Claude))
            .unwrap();
        assert!(
            claude.ends_with(
                "Run `mj login --profile claude-max`, or store a long-lived token with `mj login --profile claude-max --setup-token`."
            ),
            "{claude}"
        );

        // Only Claude can rotate ahead of expiry this way.
        let codex = CredentialSyncNotices::default()
            .notice(&result, Some(HarnessKind::Codex))
            .unwrap();
        assert!(
            codex.ends_with("Run `mj login --profile claude-max`."),
            "{codex}"
        );

        // The advice also reaches a failed reconciliation, not only a clean one.
        let failed = CredentialSyncResult {
            outcomes: vec![CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Err("worker proxy disconnected".into()),
            }],
            ..result
        };
        let claude_failure = CredentialSyncNotices::default()
            .notice(&failed, Some(HarnessKind::Claude))
            .unwrap();
        assert!(
            claude_failure.contains("--setup-token`."),
            "{claude_failure}"
        );
    }

    #[test]
    fn an_empty_prompt_notice_does_not_claim_authentication_failed() {
        use hel::hel_credentials::{
            CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult,
        };

        let result = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: Some(CredentialSyncCause {
                session_id: "018f9dd2-a3b4".into(),
                reason: CredentialSyncReason::EmptyPromptResponse,
            }),
            failure: None,
            outcomes: vec![CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Ok(vec![CredentialSyncAction::Pushed]),
            }],
        };
        let notice = CredentialSyncNotices::default()
            .notice(&result, None)
            .unwrap();
        assert!(notice.contains("returned no response"), "{notice}");
        assert!(notice.contains("were pushed"), "{notice}");
        assert!(!notice.contains("Auth failure"), "{notice}");
    }

    #[test]
    fn an_immediate_sync_failure_is_not_reported_as_no_new_credentials() {
        use hel::hel_credentials::CredentialSyncResult;

        let result = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: Some(CredentialSyncCause {
                session_id: "018f9dd2-a3b4".into(),
                reason: CredentialSyncReason::AuthenticationFailure,
            }),
            failure: Some("controller credential file is unreadable".into()),
            outcomes: Vec::new(),
        };
        let notice = CredentialSyncNotices::default()
            .notice(&result, None)
            .unwrap();
        assert!(notice.contains("reconciliation failed"), "{notice}");
        assert!(notice.contains("credential file is unreadable"), "{notice}");
        assert!(!notice.contains("nothing fresher"), "{notice}");
    }

    #[test]
    fn a_failed_credential_sync_is_reported() {
        use hel::hel_credentials::{CredentialSyncOutcome, CredentialSyncResult};

        let result = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: None,
            failure: None,
            outcomes: vec![CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Err("worker proxy disconnected".into()),
            }],
        };
        let notice = CredentialSyncNotices::default()
            .notice(&result, None)
            .unwrap();
        assert!(notice.contains("worker proxy disconnected"), "{notice}");
    }

    #[test]
    fn a_repeated_credential_failure_is_reported_once_until_it_changes() {
        use hel::hel_credentials::{
            CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult,
        };

        let failed = |detail: &str| CredentialSyncResult {
            profile_id: "work".into(),
            trigger: None,
            failure: None,
            outcomes: vec![CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Err(detail.to_owned()),
            }],
        };
        let mut notices = CredentialSyncNotices::default();

        assert!(
            notices
                .notice(&failed("worker proxy disconnected"), None)
                .is_some()
        );
        assert_eq!(
            notices.notice(&failed("worker proxy disconnected"), None),
            None
        );

        let changed = notices.notice(&failed("container is gone"), None).unwrap();
        assert!(changed.contains("container is gone"), "{changed}");
        assert_eq!(notices.notice(&failed("container is gone"), None), None);

        // A clean cycle forgets the failure, so a recurrence is reported again.
        let healthy = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: None,
            failure: None,
            outcomes: vec![CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Ok(vec![CredentialSyncAction::Pushed]),
            }],
        };
        assert_eq!(notices.notice(&healthy, None), None);
        assert!(notices.notice(&failed("container is gone"), None).is_some());
    }

    #[test]
    fn a_repeated_whole_sync_failure_is_reported_once_per_profile() {
        use hel::hel_credentials::CredentialSyncResult;

        let failed = |profile_id: &str| CredentialSyncResult {
            profile_id: profile_id.to_owned(),
            trigger: None,
            failure: Some("controller home is unreadable".into()),
            outcomes: Vec::new(),
        };
        let mut notices = CredentialSyncNotices::default();

        let notice = notices.notice(&failed("work"), None).unwrap();
        assert!(notice.contains("profile work"), "{notice}");
        assert_eq!(notices.notice(&failed("work"), None), None);
        // Another profile failing the same way is its own key.
        assert!(notices.notice(&failed("personal"), None).is_some());
        assert_eq!(notices.notice(&failed("work"), None), None);
    }

    #[test]
    fn skills_and_github_syncs_speak_while_harness_credentials_stay_out_of_the_notice() {
        use hel::hel_credentials::{
            CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult,
        };

        let result = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: None,
            failure: None,
            outcomes: vec![
                CredentialSyncOutcome {
                    session_id: "018f9dd2-a3b4".into(),
                    outcome: Ok(vec![
                        CredentialSyncAction::Pushed,
                        CredentialSyncAction::SkillsPushed,
                        CredentialSyncAction::GithubTokenPushed,
                    ]),
                },
                CredentialSyncOutcome {
                    session_id: "018f9dd2-bbbb".into(),
                    outcome: Ok(vec![
                        CredentialSyncAction::SkillsPushed,
                        CredentialSyncAction::GithubTokenRemoved,
                    ]),
                },
            ],
        };
        let notice = CredentialSyncNotices::default()
            .notice(&result, None)
            .unwrap();
        assert!(!notice.contains("harness credentials"), "{notice}");
        assert!(
            notice.contains("Synced skills for profile work to 2 session(s)."),
            "{notice}"
        );
        assert!(
            notice.contains("Synced the GitHub CLI token to 1 session(s)."),
            "{notice}"
        );
        assert!(
            notice.contains("Removed the GitHub CLI token from 1 session(s)."),
            "{notice}"
        );
    }

    #[test]
    fn aws_capacity_sums_live_instance_allocations() {
        let total = aggregate_aws_capacity(&[
            DeploymentCapacityUsage {
                cpu_percent: None,
                memory_used_bytes: 0,
                memory_total_bytes: 8,
                logical_cores: 2,
                disk_total_bytes: Some(100),
            },
            DeploymentCapacityUsage {
                cpu_percent: None,
                memory_used_bytes: 0,
                memory_total_bytes: 16,
                logical_cores: 4,
                disk_total_bytes: Some(200),
            },
        ])
        .unwrap();

        assert_eq!(total.memory_total_bytes, 24);
        assert_eq!(total.logical_cores, 6);
        assert_eq!(total.disk_total_bytes, Some(300));
    }
}
