use super::*;
use crate::quota::{ProfileQuota, QuotaManager, QuotaRefreshRequest};
use mj_core::continuation::QuotaRecovery;

pub(super) type Resolver = Arc<
    dyn Fn(String, ManagedSessionView) -> futures::future::BoxFuture<'static, Result<QuotaRecovery>>
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
    let local = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms).map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %Z (%:z)")
                .to_string()
        })
    };
    let distant = reset_at_ms.filter(|reset| reset.saturating_sub(now_ms) > AUTORESUME_HORIZON_MS);
    let reset_at_ms = reset_at_ms.filter(|_| distant.is_none());
    let retry_at_ms = reset_at_ms.and_then(|t| t.checked_add(60_000));
    let notice = match (retry_at_ms.and_then(local), distant.and_then(local)) {
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
) -> Result<QuotaRecovery> {
    let snapshot = view.snapshot.as_ref().context("missing quota snapshot")?;
    let turn = ended_turn(snapshot).context("missing quota completion")?;
    let (profile_id, profile) = {
        let controller = state
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
    let reset = crate::quota::recovery_reset(
        &windows,
        explicit,
        mj_core::clock::epoch_millis() / 1000,
        consumed,
    );
    let (reset_at_ms, retry_at_ms, notice) = deadline(
        reset.and_then(|t| t.checked_mul(1000)),
        mj_core::clock::epoch_millis(),
        used_cache,
    );
    Ok(QuotaRecovery {
        user_command_id: c.user_command_id.clone().context("missing user request")?,
        completed_command_id: turn.key.to_owned(),
        profile_id,
        reset_at_ms,
        retry_at_ms,
        notice,
        submitted: false,
    })
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
    recovery: QuotaRecovery,
) -> Result<Box<ManagedSessionView>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let handle = env
            .control
            .wait_for_session(id, Duration::from_secs(5))
            .await?;
        let current = handle.view();
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
                    recovery: Some(Box::new(recovery)),
                },
            )
            .await?;
        handle.sync_now().await?;
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
            if current_rules(s) && s.operational.goal.resumable_after_quota() {
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
