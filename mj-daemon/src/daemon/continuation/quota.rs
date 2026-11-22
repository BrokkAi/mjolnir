use super::*;
use crate::server::api::{SubagentBackend, SubagentCandidate};
use crate::server_runtime::api::{QuotaRead, QuotaWait};
use mj_controller::quota::{ProfileQuota, QuotaRefreshRequest};
use mj_core::continuation::QuotaRecovery;
use mj_core::state::{MoveSessionRequest, QuotaMoveContinuation};

#[derive(Debug)]
pub(super) enum Plan {
    Pending(QuotaWait),
    Wait(QuotaRecovery),
    ChildSwitch {
        destination: String,
        reservation: Option<mj_controller::quota_telemetry::PlacementReservation>,
        fallback: QuotaRecovery,
    },
    Move {
        request: Box<MoveSessionRequest>,
        destination: String,
        reservation: Option<mj_controller::quota_telemetry::PlacementReservation>,
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

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) fn destinations(
    config: &mj_core::config::Config,
    source: &str,
    harness: mj_core::config::HarnessKind,
    model: &str,
    candidates: Vec<SubagentCandidate>,
    now: i64,
    execution_policy: mj_core::config::ExecutionPolicy,
    placement: mj_controller::profile_scheduler::Placement,
    category: Option<&str>,
    telemetry: &mj_controller::quota_telemetry::QuotaTelemetrySnapshot,
    power_ranking: &mj_controller::power_ranking::PowerRankingSnapshot,
) -> Result<Vec<SubagentCandidate>> {
    let mut wrapped = wrap_destinations(
        eligible_destinations(
            config,
            source,
            harness,
            model,
            candidates,
            now,
            execution_policy,
        ),
        model,
    );
    let ctx = mj_controller::profile_scheduler::SchedulingContext {
        execution_policy,
        placement,
        category,
        ranking_anchor: None,
        prefer: &config.quota.prefer,
        telemetry,
        power_ranking,
        now_seconds: now,
    };
    mj_controller::profile_scheduler::draw_scheduled_candidates(&mut wrapped, &ctx)?;
    Ok(wrapped.into_iter().map(|w| w.candidate).collect())
}

fn eligible_destinations(
    config: &mj_core::config::Config,
    source: &str,
    harness: mj_core::config::HarnessKind,
    model: &str,
    mut candidates: Vec<SubagentCandidate>,
    now: i64,
    execution_policy: mj_core::config::ExecutionPolicy,
) -> Vec<SubagentCandidate> {
    candidates.retain(|c| {
        c.profile_id != source
            && c.harness == harness
            && mj_controller::profile_scheduler::guardian_eligible(c.harness, execution_policy)
            && config
                .native_continuity(harness, source, &c.profile_id)
                .unwrap_or(false)
            && c.choices.models.iter().any(|choice| choice.value == model)
            && !c.unhealthy.contains_key(model)
            && mj_controller::profile_scheduler::ineligibility(c, model, now).is_none()
    });
    candidates
}

struct DestinationCandidate {
    candidate: SubagentCandidate,
    scheduled: mj_controller::profile_scheduler::ScheduledCandidate,
}
impl AsRef<mj_controller::profile_scheduler::ScheduledCandidate> for DestinationCandidate {
    fn as_ref(&self) -> &mj_controller::profile_scheduler::ScheduledCandidate {
        &self.scheduled
    }
}

fn wrap_destinations(candidates: Vec<SubagentCandidate>, model: &str) -> Vec<DestinationCandidate> {
    candidates
        .into_iter()
        .map(|candidate| {
            let scheduled = mj_controller::profile_scheduler::ScheduledCandidate {
                profile_id: candidate.profile_id.clone(),
                harness: candidate.harness,
                model: model.to_owned(),
                effort: None,
                quota: candidate.quota.clone(),
                may_consume_credits: candidate.may_consume_credits,
                cache_identity: candidate.cache_identity.clone(),
            };
            DestinationCandidate {
                candidate,
                scheduled,
            }
        })
        .collect()
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
                "; using last-known quota reset times"
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
    id: &str,
    view: &ManagedSessionView,
) -> Result<Plan> {
    let snapshot = view.snapshot.as_ref().context("missing quota snapshot")?;
    let turn = ended_turn(snapshot).context("missing quota completion")?;
    let (profile_id, profile, child, handed_back) = {
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
            controller.state.subagents.contains_key(id),
            controller_owner
                .committed()
                .and_then(|committed| committed.subagent_reports.get(id))
                .and_then(|report| report.handback.as_ref())
                .is_some_and(|handback| handback.answers(turn.key)),
        )
    };
    let harness = profile.kind;
    let profile_name = profile_id.clone();
    let request = mj_core::runtime::spawn_blocking(move || {
        QuotaRefreshRequest::for_profile(
            &profile_name,
            &profile,
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        )
    })
    .await
    .context("prepare quota request")?;
    let identity = request.cache_identity();
    let observed = state
        .quota_report_backend()
        .context("quota report service is unavailable")
        .and_then(|backend| backend.quota_report(&profile_id, &identity));
    let cached = mj_core::runtime::spawn_blocking(move || {
        mj_controller::database::load_quota_cache(&identity)
    })
    .await
    .context("read cached quota resets")?;
    let (observed, wait) = match observed {
        Ok(QuotaRead::Ready(report)) => {
            (Some(report).filter(|report| report.error.is_none()), None)
        }
        Ok(QuotaRead::Pending(wait)) => (None, Some(wait)),
        Err(error) => {
            tracing::warn!(session=id, %error, "quota report unavailable; consulting last known resets");
            (None, None)
        }
    };
    let cached = match cached {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(session=id, %error, "quota reset cache unavailable");
            None
        }
    };
    let mut windows = observed
        .as_ref()
        .map(|r| r.windows.clone())
        .unwrap_or_default();
    if !observed.as_ref().is_some_and(ProfileQuota::is_usage_priced)
        && let Some(cached) = cached
    {
        mj_controller::quota::merge_reset_windows(&mut windows, &cached.windows);
    }
    let now = chrono::DateTime::from_timestamp_millis(turn.completed_at_ms)
        .context("invalid completion timestamp")?
        .with_timezone(&chrono::Local)
        .fixed_offset();
    let stated = turn
        .diagnostic
        .as_ref()
        .and_then(|d| d.reset_at.as_deref())
        .and_then(|text| mj_controller::quota::normalize_reset_at(text, now))
        .map(|t| mj_controller::quota::StatedReset {
            at: t.timestamp(),
            relative: false,
        })
        .or_else(|| {
            message(snapshot)
                .and_then(|text| mj_controller::quota::message_reset(&text, turn.completed_at_ms))
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
                .is_none_or(|m| mj_controller::quota::window_applies(w, m))
        })
        .collect();
    let now_ms = mj_core::clock::epoch_millis();
    let reset = mj_controller::quota::retry_deadline(&windows, stated, now_ms / 1000, consumed);
    if reset.is_none()
        && !handed_back
        && let Some(wait) = wait
    {
        return Ok(Plan::Pending(wait));
    }
    // These are retained poller observations, not a provider query made for
    // this recovery. Attribute the selected reset after model filtering.
    let used_cache = reset.is_some_and(|reset| {
        windows
            .iter()
            .any(|window| window.resets_at_epoch_seconds == Some(reset))
    });
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
    if child {
        fallback.reset_at_ms = None;
        fallback.retry_at_ms = None;
        fallback.notice = match reset
            .and_then(|time| time.checked_mul(1000))
            .and_then(local_reset)
        {
            Some(time) => format!(
                "Profile {} is out of quota until {time}.",
                fallback.profile_id
            ),
            None => format!(
                "Profile {} is out of quota. The reset time is unknown.",
                fallback.profile_id
            ),
        };
    }
    if !handed_back
        && (!child || harness != mj_core::config::HarnessKind::OpenCode)
        && mj_controller::quota::long_window_stop(
            &windows,
            model.as_deref().unwrap_or(""),
            stated.map(|stated| stated.at),
            reset,
            now_ms / 1000,
        )
    {
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
            let execution_policy = if state
                .owner()
                .controller()
                .state
                .sessions
                .get(id)
                .is_some_and(|session| session.is_yolo_target(&config))
            {
                mj_core::config::ExecutionPolicy::Unconstrained
            } else {
                mj_core::config::ExecutionPolicy::ConfiguredApprovals
            };
            let placement = if child {
                mj_controller::profile_scheduler::Placement::Child
            } else {
                mj_controller::profile_scheduler::Placement::TopLevel
            };
            let category: &'static str = if child {
                match state
                    .owner()
                    .controller()
                    .state
                    .subagents
                    .get(id)
                    .map(|record| record.kind)
                {
                    Some(mj_core::subagent::SubagentKind::Oracle) => "oracle",
                    Some(mj_core::subagent::SubagentKind::Reviewer) => "reviewer",
                    _ => "straightforward",
                }
            } else {
                "primary"
            };
            let power_ranking = state.power_ranking.snapshot();
            let mut refused = false;
            let mut remaining = wrap_destinations(
                eligible_destinations(
                    &config,
                    &source,
                    harness,
                    &model,
                    candidates,
                    now_ms / 1000,
                    execution_policy,
                ),
                &model,
            );
            while !remaining.is_empty() {
                let reservation = state.quota_telemetry.draw_and_reserve(
                    &mut remaining,
                    execution_policy,
                    placement,
                    Some(category),
                    None,
                    &config.quota.prefer,
                    &power_ranking,
                    now_ms / 1000,
                )?;
                let Some(reservation) = reservation else {
                    break;
                };
                let candidate = remaining.remove(0).candidate;
                if child {
                    return Ok(Plan::ChildSwitch {
                        destination: candidate.profile_id,
                        reservation: Some(reservation),
                        fallback,
                    });
                }
                match state.prepare_quota_move(id, &candidate.profile_id).await {
                    Ok(request) => {
                        // The record keeps the fallback's times, so a Move that
                        // fails leaves the retry on the source profile in place.
                        // A completed Move changes the profile, which clears it.
                        let mut abstention = fallback.clone();
                        abstention.notice = move_notice(&reason, &candidate.profile_id, &fallback);
                        return Ok(Plan::Move {
                            request: Box::new(request),
                            destination: candidate.profile_id,
                            reservation: Some(reservation),
                            reason,
                            abstention,
                            fallback,
                        });
                    }
                    Err(error) => {
                        drop(reservation);
                        refused = true;
                        preparation_refused(
                            &mut fallback.notice,
                            id,
                            &candidate.profile_id,
                            &error,
                            &format!("quota-move-prepare-{id}-{}", turn.completed_ordinal),
                        );
                    }
                }
            }
            if !refused {
                append_bounded(
                    &mut fallback.notice,
                    &format!(
                        " No other enabled profile of this harness with quota or paid credits left can take over this session with model {model}."
                    ),
                );
            }
        }
    }
    if child {
        append_bounded(
            &mut fallback.notice,
            " Start a new sub-agent or send again after quota resets.",
        );
    }
    Ok(Plan::Wait(fallback))
}

pub(super) fn preparation_refused(
    notice: &mut String,
    session: &str,
    profile: &str,
    error: &anyhow::Error,
    reference: &str,
) {
    tracing::info!(session, profile, reference, error=%format!("{error:#}"), "quota Move preparation refused");
    append_bounded(
        notice,
        &format!(
            " Automatic Move to profile {profile} was refused: {}.",
            crate::daemon::session_move::public_quota_failure(error, reference)
        ),
    );
}

/// The longest notice the worker accepts in a `SetQuotaRecovery` (checked in
/// `mj-worker/src/relay/commands.rs`).
pub(super) const NOTICE_BYTES: usize = 4096;

/// Appends `suffix` to `notice`, cutting the suffix on a character boundary and
/// ending it with an ellipsis when the whole would pass the worker's limit.
pub(super) fn append_bounded(notice: &mut String, suffix: &str) {
    let room = NOTICE_BYTES.saturating_sub(notice.len());
    if suffix.len() <= room {
        notice.push_str(suffix);
        return;
    }
    const ELLIPSIS: &str = "…";
    let keep = suffix.floor_char_boundary(room.saturating_sub(ELLIPSIS.len()));
    notice.push_str(&suffix[..keep]);
    if room >= ELLIPSIS.len() {
        notice.push_str(ELLIPSIS);
    }
}

/// The notice stored while a quota Move runs. It promises only what the stored
/// record does: with a retry time, the source profile continues then if the
/// Move fails while the session is still running. A Move that fails after it
/// stopped the source leaves no connected worker to retry, and Resume refuses
/// that session; its failure message tells the user to retry the Move, so the
/// notice points there instead of promising a retry.
pub(super) fn move_notice(reason: &str, destination: &str, fallback: &QuotaRecovery) -> String {
    let mut notice = format!(
        "Subscription quota reached, and {reason}. Switching this session to profile {destination} to continue the interrupted work."
    );
    if let Some(time) = fallback.retry_at_ms.and_then(local_reset) {
        notice.push_str(&format!(
            " If the switch fails while the session is still running, it continues on profile {} at {time} (one minute after reset). \
             If the session has already stopped, the failure message says how to retry the Move.",
            fallback.profile_id
        ));
    }
    notice
}

fn cursor(view: &ManagedSessionView) -> Result<RelayCursor> {
    let s = view.snapshot.as_ref().context("missing relay snapshot")?;
    Ok(RelayCursor {
        ordinal: s.operational.latest_ordinal,
        digest: s.operational.latest_digest.clone(),
    })
}

#[derive(Clone)]
pub(super) struct Schedule {
    command_id: String,
    command: RelayCommand,
    plan: Arc<Plan>,
    state: ScheduleState,
}

#[derive(Clone)]
enum ScheduleState {
    Unconfirmed,
    Accepted,
    Fallback(Box<PendingFallback>),
    FallbackAccepted,
    // Preserve the exact unanswered exchange, but discard its daemon plan
    // once it is answered. Foreground release then supplies a fresh plan.
    Stale(Box<ScheduleState>),
    Finished,
}

#[derive(Clone)]
struct PendingFallback {
    command_id: String,
    command: RelayCommand,
}

pub(super) fn invalidate(env: &Environment, id: &str) {
    if let Some(saved) = env.schedules.lock().unwrap().get_mut(id)
        && !matches!(saved.state, ScheduleState::Stale(_))
    {
        saved.state = ScheduleState::Stale(Box::new(saved.state.clone()));
    }
}

fn same_recovery_context(
    env: &Environment,
    id: &str,
    view: &ManagedSessionView,
    record: &QuotaRecovery,
) -> bool {
    (env.profile)(id).as_ref() == Some(&record.profile_id)
        && view.snapshot.as_ref().is_some_and(|s| {
            s.operational.continuation.user_command_id.as_ref() == Some(&record.user_command_id)
                && s.operational.continuation.completed_command_id.as_ref()
                    == Some(&record.completed_command_id)
        })
}

pub(super) fn unfinished(env: &Environment, id: &str, view: &ManagedSessionView) -> bool {
    env.schedules.lock().unwrap().get(id).is_some_and(|saved| {
        if matches!(saved.state, ScheduleState::Finished) {
            return false;
        }
        let RelayCommand::SetQuotaRecovery {
            recovery: Some(record),
            ..
        } = &saved.command
        else {
            return false;
        };
        same_recovery_context(env, id, view, record)
    })
}

fn set_state(env: &Environment, id: &str, state: ScheduleState) {
    if let Some(saved) = env.schedules.lock().unwrap().get_mut(id) {
        saved.state = state;
    }
}

fn admit(env: &Environment, id: &str) -> Result<mj_controller::recovery_gate::RecoveryAttempt> {
    env.session_gate
        .try_start(
            id,
            mj_controller::recovery_gate::BackgroundWork::QuotaScheduling,
        )
        .map_err(|refusal| {
            tracing::info!(session = id, holders = ?refusal.holders, "quota scheduling deferred");
            anyhow::anyhow!(
                "quota scheduling deferred by {}",
                refusal.holders.join(", ")
            )
        })
}

fn invalid_request(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<mj_controller::session_manager::SubmitRefusalCode>()
        .is_some_and(|code| code.0 == mj_core::relay::RelayErrorCode::InvalidRequest)
}

fn record_finished(env: &Environment, id: &str) {
    set_state(env, id, ScheduleState::Finished);
}

async fn send_fallback(
    env: &Environment,
    id: &str,
    handle: &mj_controller::session_manager::ManagedSessionHandle,
    admission: &mj_controller::recovery_gate::RecoveryAttempt,
    pending: PendingFallback,
    stale: bool,
) -> Result<bool> {
    ensure!(
        !admission.load(Ordering::Acquire),
        "quota fallback preempted"
    );
    let result = handle
        .submit(pending.command_id.clone(), pending.command.clone())
        .await;
    if let Err(error) = &result {
        if invalid_request(error) {
            tracing::warn!(session = id, command_id = %pending.command_id, error = ?error,
                "quota fallback command ID explicitly refused; completion will not be resubmitted");
            record_finished(env, id);
            return Ok(false);
        }
        if error
            .downcast_ref::<mj_client::session::Refused>()
            .is_some()
        {
            if let RelayCommand::SetQuotaRecovery {
                recovery: Some(record),
                ..
            } = &pending.command
                && !same_recovery_context(env, id, &handle.view(), record)
            {
                // Explicit refusal proves the old fallback never landed. A
                // newer turn/profile must not be held behind that stale payload.
                record_finished(env, id);
                bail!(
                    "obsolete quota fallback was not accepted; reconsider the current completion"
                );
            }
            let mut retry = pending;
            if let RelayCommand::SetQuotaRecovery { expected, .. } = &mut retry.command {
                *expected = cursor(&handle.view())?;
            }
            let state = ScheduleState::Fallback(Box::new(retry));
            set_state(
                env,
                id,
                if stale {
                    ScheduleState::Stale(Box::new(state))
                } else {
                    state
                },
            );
        }
    }
    result?;
    let state = ScheduleState::FallbackAccepted;
    set_state(
        env,
        id,
        if stale {
            ScheduleState::Stale(Box::new(state))
        } else {
            state
        },
    );
    Ok(true)
}

pub(super) async fn schedule(
    env: &Environment,
    id: &str,
    view: ManagedSessionView,
    plan: Plan,
) -> Result<Box<ManagedSessionView>> {
    let plan = match plan {
        Plan::ChildSwitch {
            destination,
            reservation,
            mut fallback,
        } => {
            let turn = view
                .snapshot
                .as_ref()
                .and_then(ended_turn)
                .context("missing quota completion")?;
            let runtime = env
                .child_switch_runtime
                .as_ref()
                .context("missing child quota runtime")?;
            match runtime
                .switch_child_quota_profile(
                    id.to_owned(),
                    fallback.profile_id.clone(),
                    destination,
                    turn.completed_ordinal,
                )
                .await
            {
                Ok(()) => {
                    if let Some(reservation) = reservation {
                        reservation.commit(Some(id.to_owned()));
                    }
                    let handle = env
                        .control
                        .find_session(id)
                        .await?
                        .context("replacement child actor stopped")?;
                    return Ok(Box::new(handle.view()));
                }
                Err(error) => tracing::warn!(session=id, %error, "child quota switch failed"),
            }
            append_bounded(
                &mut fallback.notice,
                " The sub-agent could not switch profiles. Start a new sub-agent or send again after quota resets.",
            );
            Plan::Wait(fallback)
        }
        plan => plan,
    };
    let plan = Arc::new(plan);
    let admission = admit(env, id)?;
    let handle = env
        .control
        .wait_for_session(id, Duration::from_secs(5))
        .await?;
    let current = handle.view();
    let command_id = format!(
        "quota-schedule-{}",
        view.snapshot
            .as_ref()
            .and_then(ended_turn)
            .context("missing quota completion")?
            .completed_ordinal
    );
    let previous = env.schedules.lock().unwrap().get(id).cloned();
    let mut schedule = if let Some(previous) = previous
        .filter(|s| !matches!(s.state, ScheduleState::Finished) || s.command_id == command_id)
    {
        if matches!(previous.state, ScheduleState::Finished) {
            return Ok(Box::new(current));
        }
        previous
    } else {
        let recovery = match plan.as_ref() {
            Plan::Pending(_) => bail!("quota report has not been published yet"),
            Plan::ChildSwitch { .. } => {
                unreachable!("child switches are handled before scheduling")
            }
            Plan::Wait(r) => r,
            Plan::Move { abstention, .. } => abstention,
        };
        // A new daemon has no remaining in-memory Move decision. The worker
        // owns whether the old schedule was accepted.
        if current
            .snapshot
            .as_ref()
            .and_then(|s| s.operational.continuation.quota_recovery.as_ref())
            .is_some_and(|r| r.completed_command_id == recovery.completed_command_id)
        {
            return Ok(Box::new(current));
        }
        ensure!(
            (env.allowed)(id)
                && quota_schedulable(&current)
                && (env.profile)(id).as_deref() == Some(&recovery.profile_id)
                && evidence_ordinal(&current) == evidence_ordinal(&view),
            "quota evidence changed during refresh"
        );
        let schedule = Schedule {
            command_id: command_id.clone(),
            command: RelayCommand::SetQuotaRecovery {
                expected: cursor(&current)?,
                recovery: Some(Box::new(recovery.clone())),
            },
            plan: plan.clone(),
            state: ScheduleState::Unconfirmed,
        };
        env.schedules
            .lock()
            .unwrap()
            .insert(id.to_owned(), schedule.clone());
        schedule
    };
    ensure!(
        !admission.load(Ordering::Acquire),
        "quota scheduling preempted"
    );
    let fresh = matches!(schedule.state, ScheduleState::Stale(_));
    if let ScheduleState::Stale(previous) = schedule.state {
        schedule.state = *previous;
    }
    if matches!(schedule.state, ScheduleState::Unconfirmed) {
        let result = handle
            .submit(schedule.command_id.clone(), schedule.command.clone())
            .await;
        if let Err(error) = &result {
            if invalid_request(error) {
                tracing::warn!(session = id, command_id = %schedule.command_id, error = ?error,
                    "quota command ID explicitly refused; completion will not be resubmitted");
                record_finished(env, id);
                ensure!(
                    schedule.command_id == command_id,
                    "earlier quota command was reconciled; reconsider the new completion"
                );
                return Ok(Box::new(handle.view()));
            }
            if error
                .downcast_ref::<mj_client::session::Refused>()
                .is_some()
            {
                env.schedules.lock().unwrap().remove(id);
            }
        }
        result?;
        schedule.state = ScheduleState::Accepted;
        set_state(
            env,
            id,
            if fresh {
                ScheduleState::Stale(Box::new(ScheduleState::Accepted))
            } else {
                ScheduleState::Accepted
            },
        );
    }
    if let ScheduleState::Fallback(pending) = &schedule.state {
        if !send_fallback(
            env,
            id,
            &handle,
            &admission,
            pending.as_ref().clone(),
            fresh,
        )
        .await?
        {
            return Ok(Box::new(handle.view()));
        }
        schedule.state = ScheduleState::FallbackAccepted;
    }
    handle.sync_now().await?;
    ensure!(
        !admission.load(Ordering::Acquire),
        "quota scheduling preempted"
    );
    if schedule.command_id != command_id {
        record_finished(env, id);
        bail!("earlier quota command was reconciled; reconsider the new completion");
    }
    let current = handle.view();
    let original_profile = match &schedule.command {
        RelayCommand::SetQuotaRecovery {
            recovery: Some(r), ..
        } => &r.profile_id,
        _ => unreachable!("a quota schedule records recovery"),
    };
    if evidence_ordinal(&current) != evidence_ordinal(&view)
        || !(env.allowed)(id)
        || (env.profile)(id).as_ref() != Some(original_profile)
        || !quota_schedulable(&current)
    {
        record_finished(env, id);
        return Ok(Box::new(current));
    }
    let stale_move_fallback = if fresh && matches!(plan.as_ref(), Plan::Wait(_)) {
        match schedule.plan.as_ref() {
            Plan::Move {
                abstention,
                fallback,
                ..
            } if current
                .snapshot
                .as_ref()
                .and_then(|s| s.operational.continuation.quota_recovery.as_ref())
                .is_some_and(|record| record == abstention) =>
            {
                Some(fallback.clone())
            }
            _ => None,
        }
    } else {
        None
    };
    if fresh {
        schedule.plan = plan.clone();
        schedule.state = ScheduleState::Accepted;
        if let Some(saved) = env.schedules.lock().unwrap().get_mut(id) {
            saved.plan = schedule.plan.clone();
            saved.state = ScheduleState::Accepted;
        }
    } else if matches!(schedule.state, ScheduleState::FallbackAccepted) {
        record_finished(env, id);
        return Ok(Box::new(current));
    }
    match schedule.plan.as_ref() {
        Plan::Pending(_) | Plan::ChildSwitch { .. } => {
            unreachable!("quota scheduling only records waits and Moves")
        }
        Plan::Wait(_) => {
            if let Some(recovery) = stale_move_fallback {
                let pending = PendingFallback {
                    command_id: format!("quota-move-fallback-{}", recovery.completed_command_id),
                    command: RelayCommand::SetQuotaRecovery {
                        expected: cursor(&current)?,
                        recovery: Some(Box::new(recovery.clone())),
                    },
                };
                set_state(env, id, ScheduleState::Fallback(Box::new(pending.clone())));
                if !send_fallback(env, id, &handle, &admission, pending, false).await? {
                    return Ok(Box::new(handle.view()));
                }
                handle.sync_now().await?;
                ensure!(
                    !admission.load(Ordering::Acquire),
                    "quota fallback preempted"
                );
            }
            record_finished(env, id);
            Ok(Box::new(handle.view()))
        }
        Plan::Move {
            request,
            destination,
            reservation,
            reason,
            fallback,
            ..
        } => {
            // Foreground lifecycle ownership must be requested after releasing
            // scheduling. Keep the daemon decision unfinished until it settles.
            drop(admission);
            let continuation = if view
                .snapshot
                .as_ref()
                .is_some_and(|s| goal_resumes_after_quota(&s.operational))
            {
                QuotaMoveContinuation::ResumeGoal
            } else {
                QuotaMoveContinuation::Prompt
            };
            match (env.start_quota_move)((**request).clone(), reason.clone(), continuation).await {
                Ok(()) => {
                    if let Some(reservation) = reservation {
                        reservation.commit(Some(id.to_owned()));
                    }
                    record_finished(env, id);
                    Ok(Box::new(handle.view()))
                }
                Err(error) => {
                    let mut fallback = fallback.clone();
                    let reference =
                        format!("quota-move-start-{id}-{}", fallback.completed_command_id);
                    tracing::warn!(session=id, profile=%destination, %reference, error=%format!("{error:#}"), "quota Move admission refused");
                    append_bounded(
                        &mut fallback.notice,
                        &format!(
                            " Could not start the Move to profile {destination}: {}.",
                            crate::daemon::session_move::public_quota_failure(&error, &reference)
                        ),
                    );
                    let pending = PendingFallback {
                        command_id: format!(
                            "quota-move-fallback-{}",
                            fallback.completed_command_id
                        ),
                        command: RelayCommand::SetQuotaRecovery {
                            expected: cursor(&handle.view())?,
                            recovery: Some(Box::new(fallback)),
                        },
                    };
                    set_state(env, id, ScheduleState::Fallback(Box::new(pending.clone())));
                    let fallback_admission = admit(env, id)?;
                    if !send_fallback(env, id, &handle, &fallback_admission, pending, false).await?
                    {
                        return Ok(Box::new(handle.view()));
                    }
                    handle.sync_now().await?;
                    ensure!(
                        !fallback_admission.load(Ordering::Acquire),
                        "quota fallback preempted"
                    );
                    record_finished(env, id);
                    Ok(Box::new(handle.view()))
                }
            }
        }
    }
}

pub(super) async fn resume(
    env: &Environment,
    id: &str,
    view: &ManagedSessionView,
    clear: bool,
) -> Result<Box<ManagedSessionView>> {
    let admission = admit(env, id)?;
    async {
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
                    mj_core::continuation::GeneratedPrompt::QuotaRetry.command_id(ordinal),
                    RelayCommand::ResumeAfterQuota {
                        expected: cursor(&current)?,
                        completed_command_id: recovery.completed_command_id.clone(),
                    },
                )
            }
        };
        ensure!(
            !admission.load(Ordering::Acquire),
            "quota continuation preempted"
        );
        handle.submit(command_id, command).await?;
        handle.sync_now().await?;
        Ok(Box::new(handle.view()))
    }
    .await
}
