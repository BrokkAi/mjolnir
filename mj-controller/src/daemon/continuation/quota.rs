use super::*;
use crate::quota::{ProfileQuota, QuotaManager, QuotaRefreshRequest};
use crate::server::api::{CandidateQuota, SubagentBackend, SubagentCandidate};
use mj_core::continuation::QuotaRecovery;
use mj_core::state::{MoveSessionRequest, QuotaMoveContinuation};

#[derive(Debug)]
pub(super) enum Plan {
    Wait(QuotaRecovery),
    Move {
        request: Box<MoveSessionRequest>,
        destination: String,
        /// Why the session moves, as a clause for the destination's notice.
        reason: String,
        abstention: QuotaRecovery,
        fallback: QuotaRecovery,
    },
}
pub(super) type StartMove = Arc<
    dyn Fn(
            MoveSessionRequest,
            String,
            QuotaMoveContinuation,
        ) -> futures::future::BoxFuture<'static, Result<()>>
        + Send
        + Sync,
>;

pub(super) fn destinations(
    config: &mj_core::config::Config,
    source: &str,
    harness: mj_core::config::HarnessKind,
    model: &str,
    mut candidates: Vec<SubagentCandidate>,
    now: i64,
) -> Vec<SubagentCandidate> {
    candidates.retain(|c| {
        c.profile_id != source
            && c.harness == harness
            && config
                .native_continuity(harness, source, &c.profile_id)
                .unwrap_or(false)
            && c.choices.models.iter().any(|choice| choice.value == model)
            && !c.unhealthy.contains_key(model)
            && match &c.quota {
                CandidateQuota::Windows(w) => crate::quota::quota_score(w, model, now) != Some(0.0),
                _ => true,
            }
    });
    crate::server::api::rank_candidates(&mut candidates, None, model, now);
    candidates
}

fn local_reset(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|time| {
        time.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %Z (%:z)")
            .to_string()
    })
}

pub(super) type Resolver = Arc<
    dyn Fn(String, ManagedSessionView) -> futures::future::BoxFuture<'static, Result<Plan>>
        + Send
        + Sync,
>;
type Lane = Arc<tokio::sync::Mutex<Option<(std::time::Instant, ProfileQuota)>>>;

#[derive(Default)]
pub(super) struct Service {
    lanes: Mutex<BTreeMap<String, Lane>>,
}

impl Service {
    async fn refresh(&self, request: QuotaRefreshRequest) -> Result<ProfileQuota> {
        let identity = request.cache_identity();
        let lane = self
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(identity)
            .or_default()
            .clone();
        let mut cached = lane.lock().await;
        if let Some((at, report)) = &*cached
            && at.elapsed() < Duration::from_secs(30)
        {
            return Ok(report.clone());
        }
        let id = request.profile_id.clone();
        let mut manager = QuotaManager::default();
        manager.refresh_profiles(vec![request], |_| async {}).await;
        let report = manager.reports().get(&id).cloned();
        manager.shutdown().await;
        let report = report.context("quota refresh produced no report")?;
        *cached = Some((std::time::Instant::now(), report.clone()));
        Ok(report)
    }
}

/// How far ahead a quota reset may be and still earn an automatic resume.
pub(super) const AUTORESUME_HORIZON_MS: i64 = 16 * 60 * 60 * 1000;

/// Decide the stored deadline and the notice that explains it. A reset beyond
/// the horizon would resume the session against a world that has moved on, so
/// it records the wait rather than scheduling a continuation.
pub(super) fn deadline(
    reset_at_ms: Option<i64>,
    now_ms: i64,
    used_cache: bool,
) -> (Option<i64>, Option<i64>, String) {
    let distant = reset_at_ms.filter(|reset| reset.saturating_sub(now_ms) > AUTORESUME_HORIZON_MS);
    let reset_at_ms = reset_at_ms.filter(|_| distant.is_none());
    let retry_at_ms = reset_at_ms.and_then(|t| t.checked_add(60_000));
    let notice = match (retry_at_ms.and_then(local_reset), distant.and_then(local_reset)) {
        (Some(time), _) => format!(
            "Subscription quota reached. Automatically continuing at {time} (one minute after reset){}.",
            if used_cache {
                "; using last-known quota reset times because fresh reset data is unavailable"
            } else {
                ""
            }
        ),
        (None, Some(time)) => format!(
            "Subscription quota reached. Quota resets at {time}, more than {} hours away; no automatic continuation was scheduled because the pending work would likely be stale by then. Resume the session yourself if the work still applies.",
            AUTORESUME_HORIZON_MS / 3_600_000
        ),
        (None, None) => "Subscription quota reached. No reliable reset time is available from the provider, saved quota data, or this message; automatic continuation was not scheduled.".into(),
    };
    (reset_at_ms, retry_at_ms, notice)
}

pub(super) fn message(snapshot: &mj_core::state::ManagedSessionSnapshot) -> Option<String> {
    use mj_core::transcript::{TranscriptBody, materialized_chunks_text};
    let turn = ended_turn(snapshot)?;
    let reply = snapshot
        .materialized
        .transcript
        .iter()
        .rev()
        .filter(|item| item.position < turn.completed_ordinal)
        .take_while(|item| {
            turn.start_position
                .is_none_or(|start| item.position >= start)
        })
        .find_map(|item| match &item.body {
            TranscriptBody::Agent {
                chunks,
                streaming: false,
            } => Some(materialized_chunks_text(chunks)),
            _ => None,
        })
        .unwrap_or_default();
    let text = match turn.diagnostic {
        Some(diagnostic) => format!("{reply}\nProvider error: {}", diagnostic.message),
        None => reply,
    };
    // Preserve the whole current message; clipping could turn a quotation into
    // an apparent provider failure.
    (!text.trim().is_empty() && text.len() <= mj_core::continuation::ASSISTANT_BYTES)
        .then_some(text)
}

pub(super) async fn prepare(
    state: &Arc<RuntimeState>,
    service: &Service,
    id: &str,
    view: &ManagedSessionView,
) -> Result<Plan> {
    let snapshot = view.snapshot.as_ref().context("missing quota snapshot")?;
    let turn = ended_turn(snapshot).context("missing quota completion")?;
    let (profile_id, profile) = {
        let controller_owner = state.owner();
        let controller = controller_owner.controller();
        let session = controller
            .state
            .sessions
            .get(id)
            .context("session was removed")?;
        (
            session.last_profile.clone(),
            controller
                .config
                .profiles
                .get(&session.last_profile)
                .context("profile was removed")?
                .clone(),
        )
    };
    let harness = profile.kind;
    let profile_name = profile_id.clone();
    let request = tokio::task::spawn_blocking(move || {
        QuotaRefreshRequest::for_profile(
            &profile_name,
            &profile,
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        )
    })
    .await
    .context("prepare quota request")?;
    let identity = request.cache_identity();
    let refreshed = service.refresh(request).await;
    let cached = tokio::task::spawn_blocking(move || crate::database::load_quota_cache(&identity))
        .await
        .context("read cached quota resets")?;
    let fresh = match refreshed {
        Ok(report) if report.error.is_none() => Some(report),
        Ok(report) => {
            tracing::warn!(session=id, error=?report.error, "quota refresh unavailable; consulting last known resets");
            None
        }
        Err(error) => {
            tracing::warn!(session=id, %error, "quota refresh failed; consulting last known resets");
            None
        }
    };
    let cached = match cached {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(session=id, %error, "quota reset cache unavailable");
            None
        }
    };
    let mut windows = fresh
        .as_ref()
        .map(|r| r.windows.clone())
        .unwrap_or_default();
    let mut used_cache = false;
    if !fresh.as_ref().is_some_and(ProfileQuota::is_usage_priced)
        && let Some(cached) = cached
    {
        used_cache = crate::quota::merge_reset_windows(&mut windows, &cached.windows);
    }
    let now = chrono::DateTime::from_timestamp_millis(turn.completed_at_ms)
        .context("invalid completion timestamp")?
        .with_timezone(&chrono::Local)
        .fixed_offset();
    let explicit = turn
        .diagnostic
        .as_ref()
        .and_then(|d| d.reset_at.as_deref())
        .and_then(|text| crate::quota::normalize_reset_at(text, now))
        .map(|t| t.timestamp())
        .or_else(|| {
            message(snapshot)
                .and_then(|text| crate::quota::message_reset(&text, turn.completed_at_ms))
        });
    let c = &snapshot.operational.continuation;
    let consumed = c
        .quota_recovery
        .as_ref()
        .filter(|r| r.submitted)
        .and_then(|r| r.reset_at_ms)
        .map(|t| t / 1000);
    let model = mj_core::acp::AcceptedSessionConfig::from_configuration(
        &snapshot.operational.config,
        &snapshot.operational.config_options,
    )
    .model;
    let windows: Vec<_> = windows
        .into_iter()
        .filter(|w| {
            model
                .as_deref()
                .is_none_or(|m| crate::quota::window_applies(w, m))
        })
        .collect();
    let now_ms = mj_core::clock::epoch_millis();
    let reset = crate::quota::recovery_reset(&windows, explicit, now_ms / 1000, consumed);
    let (reset_at_ms, retry_at_ms, notice) =
        deadline(reset.and_then(|t| t.checked_mul(1000)), now_ms, used_cache);
    let mut fallback = QuotaRecovery {
        user_command_id: c.user_command_id.clone().context("missing user request")?,
        completed_command_id: turn.key.to_owned(),
        profile_id,
        reset_at_ms,
        retry_at_ms,
        notice,
        submitted: false,
    };
    if crate::quota::long_window_stop(
        &windows,
        model.as_deref().unwrap_or(""),
        explicit,
        reset,
        now_ms / 1000,
    ) {
        let candidates = match (&model, state.quota_report_backend()) {
            (Some(_), Some(backend)) => match backend.session_profile_candidates().await {
                Ok(candidates) => Some(candidates.offered),
                Err(error) => {
                    tracing::warn!(session=id, %error, "quota Move candidates unavailable");
                    None
                }
            },
            _ => None,
        };
        if let (Some(model), Some(candidates)) = (model, candidates) {
            let config = state.owner().controller().config.clone();
            let source = fallback.profile_id.clone();
            // The reason reads after "because" in the destination's notice.
            let reason = match reset
                .and_then(|s| s.checked_mul(1000))
                .and_then(local_reset)
            {
                Some(time) => format!(
                    "the quota for profile {source} resets at {time}, more than five hours away"
                ),
                None => format!("the quota for profile {source} has no known reset time"),
            };
            for candidate in
                destinations(&config, &source, harness, &model, candidates, now_ms / 1000)
            {
                match state.prepare_quota_move(id, &candidate.profile_id).await {
                    Ok(request) => {
                        let mut abstention = fallback.clone();
                        abstention.reset_at_ms = None;
                        abstention.retry_at_ms = None;
                        abstention.notice = format!(
                            "Subscription quota reached, and {reason}. Switching this session to profile {} to continue the interrupted work.",
                            candidate.profile_id
                        );
                        return Ok(Plan::Move {
                            request: Box::new(request),
                            destination: candidate.profile_id,
                            reason,
                            abstention,
                            fallback,
                        });
                    }
                    Err(error) => {
                        tracing::info!(session=id, profile=%candidate.profile_id, error=%format!("{error:#}"), "quota Move preparation refused")
                    }
                }
            }
            fallback.notice.push_str(&format!(
                " No other enabled profile of this harness with quota left can take over this session with model {model}."
            ));
        }
    }
    Ok(Plan::Wait(fallback))
}

fn cursor(view: &ManagedSessionView) -> Result<RelayCursor> {
    let s = view.snapshot.as_ref().context("missing relay snapshot")?;
    Ok(RelayCursor {
        ordinal: s.operational.latest_ordinal,
        digest: s.operational.latest_digest.clone(),
    })
}

pub(super) async fn schedule(
    env: &Environment,
    id: &str,
    view: ManagedSessionView,
    plan: Plan,
) -> Result<Box<ManagedSessionView>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let handle = env
            .control
            .wait_for_session(id, Duration::from_secs(5))
            .await?;
        let current = handle.view();
        let recovery = match &plan {
            Plan::Wait(r) => r,
            Plan::Move { abstention, .. } => abstention,
        };
        ensure!(
            (env.allowed)(id)
                && quota_schedulable(&current)
                && (env.profile)(id).as_deref() == Some(&recovery.profile_id)
                && evidence_ordinal(&current) == evidence_ordinal(&view),
            "quota evidence changed during refresh"
        );
        let command_id = format!(
            "quota-schedule-{}",
            view.snapshot
                .as_ref()
                .and_then(ended_turn)
                .context("missing quota completion")?
                .completed_ordinal
        );
        handle
            .submit(
                command_id,
                RelayCommand::SetQuotaRecovery {
                    expected: cursor(&current)?,
                    recovery: Some(Box::new(recovery.clone())),
                },
            )
            .await?;
        handle.sync_now().await?;
        let continuation = if view
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| goal_resumes_after_quota(&snapshot.operational))
        {
            QuotaMoveContinuation::ResumeGoal
        } else {
            QuotaMoveContinuation::Prompt
        };
        if let Plan::Move {
            request,
            destination,
            reason,
            mut fallback,
            ..
        } = plan
            && let Err(error) = (env.start_quota_move)(*request, reason, continuation).await
        {
            fallback.notice.push_str(&format!(
                " Could not start the Move to profile {destination}: {error:#}."
            ));
            handle
                .submit(
                    format!("quota-move-fallback-{}", fallback.completed_command_id),
                    RelayCommand::SetQuotaRecovery {
                        expected: cursor(&handle.view())?,
                        recovery: Some(Box::new(fallback)),
                    },
                )
                .await?;
            handle.sync_now().await?;
        }
        Ok(Box::new(handle.view()))
    })
    .await
    .context("quota recovery scheduling timed out")?
}

pub(super) async fn resume(
    env: &Environment,
    id: &str,
    view: &ManagedSessionView,
    clear: bool,
) -> Result<Box<ManagedSessionView>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let handle = env
            .control
            .wait_for_session(id, Duration::from_secs(5))
            .await?;
        let current = handle.view();
        let recovery = current
            .snapshot
            .as_ref()
            .and_then(|s| s.operational.continuation.quota_recovery.as_ref())
            .context("quota recovery was cancelled")?;
        ensure!(
            view.snapshot
                .as_ref()
                .and_then(|s| s.operational.continuation.quota_recovery.as_ref())
                == Some(recovery),
            "quota recovery changed"
        );
        let ordinal = current
            .snapshot
            .as_ref()
            .and_then(ended_turn)
            .context("missing blocked turn")?
            .completed_ordinal;
        let (command_id, command) = if clear {
            (
                format!("quota-cancel-{ordinal}"),
                RelayCommand::SetQuotaRecovery {
                    expected: cursor(&current)?,
                    recovery: None,
                },
            )
        } else {
            ensure!(
                (env.allowed)(id)
                    && quota_resumable(&current)
                    && (env.profile)(id).as_deref() == Some(&recovery.profile_id),
                "quota recovery no longer allowed"
            );
            let s = current.snapshot.as_ref().unwrap();
            // Older workers would take the command for a user's plain resume.
            if goal_resumes_after_quota(&s.operational) {
                // Restarting the goal restarts its own loop; a continuation
                // prompt would get one turn and leave the goal stopped.
                (
                    format!(
                        "{}{ordinal}",
                        mj_core::continuation::QUOTA_GOAL_RESUME_PREFIX
                    ),
                    RelayCommand::GoalControl {
                        action: mj_core::goal::GoalControlAction::Resume,
                    },
                )
            } else {
                (
                    format!("quota-retry-{ordinal}"),
                    RelayCommand::ResumeAfterQuota {
                        expected: cursor(&current)?,
                        completed_command_id: recovery.completed_command_id.clone(),
                    },
                )
            }
        };
        handle.submit(command_id, command).await?;
        handle.sync_now().await?;
        Ok(Box::new(handle.view()))
    })
    .await
    .context("quota recovery submission timed out")?
}
