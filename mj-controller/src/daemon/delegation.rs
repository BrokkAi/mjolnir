//! The daemon owns delegation; control surfaces only observe its effects.
use crate::daemon::RuntimeState;
use crate::database::StoredDelegationResult;
use crate::server_runtime::api::ApiBackend;
use crate::session_manager::{DelegationObservation, DelegationUpdates, SessionManagerControl};
use anyhow::{Context, Result};
use mj_core::subagent::SubagentToolAction;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
mod dispatch;
mod policy;
use dispatch::{Identity, Job, SubagentDispatch};
pub(crate) use policy::Services;

enum ChildCompletion {
    Settled,
    RetryPark,
    /// The child's own review is open, so its ended turn is not yet a missing
    /// handback. The verdict wakes the child; this checks again if the review
    /// ends without waking it (a cancel).
    AwaitingReview,
    /// The daemon began handing off before the completion could be admitted;
    /// the next daemon settles the child.
    Draining,
    /// The child ended its reminder turn waiting for processes it started: no
    /// report, no park. Judged again at `give_up_at_ms`, or sooner when the
    /// worker publishes that the processes exited.
    Held {
        give_up_at_ms: i64,
    },
    /// The parent was told, but the child's processes are still running, so
    /// the park waits until the parent's turn that received the report ends.
    /// Judged again when the parent's observation changes.
    ParkAfterParentTurn {
        parent: String,
    },
}

/// First wait after a child completion fails or finds the child busy.
const COMPLETION_RETRY_BASE: Duration = Duration::from_secs(1);
/// A completion that keeps failing is tried this rarely (#1456): reviewer
/// completion reaches the worker restart queue, which a flat 1 s retry kept full.
const COMPLETION_FAILURE_CAP: Duration = Duration::from_secs(300);
/// A busy child is polled more often: it is working, not failing.
const COMPLETION_BUSY_CAP: Duration = Duration::from_secs(30);

/// When one child outcome's completion may run again, and how it has been
/// going. Settling the child forgets it; a later outcome starts afresh.
struct CompletionRetry {
    ordinal: u64,
    /// When to try again; `None` waits for an observation that clears it.
    at: Option<Instant>,
    failures: u32,
    last_failure: Option<String>,
}

impl CompletionRetry {
    /// Schedules the next attempt after `previous`. `failure` is the error of
    /// an attempt that failed, or `None` when the child was only busy. Returns
    /// whether the failure is news (the first, or a different cause), so a
    /// persistent failure is warned about once rather than at every retry.
    fn schedule(
        previous: Option<Self>,
        ordinal: u64,
        now: Instant,
        failure: Option<String>,
    ) -> (Self, bool) {
        let (failures, last_failure) = previous
            .filter(|item| item.ordinal == ordinal)
            .map_or((0, None), |item| (item.failures, item.last_failure));
        let failures = failures.saturating_add(1);
        let (cap, report, last_failure) = match failure {
            Some(failure) => (
                COMPLETION_FAILURE_CAP,
                last_failure.as_ref() != Some(&failure),
                Some(failure),
            ),
            None => (COMPLETION_BUSY_CAP, false, last_failure),
        };
        let delay = crate::recovery::backoff_delay(COMPLETION_RETRY_BASE, cap, failures);
        (
            Self {
                ordinal,
                at: Some(now + delay),
                failures,
                last_failure,
            },
            report,
        )
    }

    /// A completion that is waiting on a fact, not failing: it runs again at
    /// `at`, or when an observation clears this entry.
    fn waiting(ordinal: u64, at: Option<Instant>) -> Self {
        Self {
            ordinal,
            at,
            failures: 0,
            last_failure: None,
        }
    }

    fn holds(&self, ordinal: u64, now: Instant) -> bool {
        self.ordinal == ordinal && self.at.is_none_or(|at| at > now)
    }
}

enum Completed {
    Executed(StoredDelegationResult, bool),
    Delivered,
    Unaccepted,
    QuotaSources(Vec<crate::quota::QuotaRefreshRequest>),
}

type QuotaSourceReader = Arc<
    dyn Fn(crate::controller::Controller) -> Vec<crate::quota::QuotaRefreshRequest> + Send + Sync,
>;

pub(crate) fn spawn(
    state: Arc<RuntimeState>,
    manager: SessionManagerControl,
    updates: DelegationUpdates,
    cancellation: CancellationToken,
) -> (Services, tokio::task::JoinHandle<Result<()>>) {
    spawn_with_quota_source_reader(
        state,
        manager,
        updates,
        cancellation,
        Arc::new(|controller| crate::pollers::quota_refresh_profiles(&controller)),
    )
}

fn spawn_with_quota_source_reader(
    state: Arc<RuntimeState>,
    manager: SessionManagerControl,
    updates: DelegationUpdates,
    cancellation: CancellationToken,
    quota_source_reader: QuotaSourceReader,
) -> (Services, tokio::task::JoinHandle<Result<()>>) {
    let policy = policy::Policy::new(
        state.clone(),
        &manager,
        &cancellation,
        quota_source_reader.clone(),
    );
    let services = policy.services.clone();
    let task = tokio::spawn(run(state, updates, policy, cancellation));
    (services, task)
}

async fn run(
    state: Arc<RuntimeState>,
    mut updates: DelegationUpdates,
    mut policy: policy::Policy,
    cancellation: CancellationToken,
) -> Result<()> {
    let backend = policy.services.backend.clone();
    // Housekeeping for stores written by builds that kept acknowledged rows.
    if let Err(error) =
        tokio::task::spawn_blocking(crate::database::prune_acknowledged_delegations).await?
    {
        tracing::warn!(%error, "could not prune acknowledged delegation records");
    }
    // Finished children whose reports are not yet in the mailbox are
    // rebuilt by the mailbox outbox, on its first sweep at startup and on
    // every sweep after it. It retries a parent whose delegation queue is
    // still connecting. A pass here would duplicate that work, and a failure
    // in it would stop this coordinator and so the daemon.
    let mut revisions = state.revisions();
    let mut dispatch = SubagentDispatch::default();
    let mut jobs = tokio::task::JoinSet::new();
    let mut identities = BTreeMap::<tokio::task::Id, Identity>::new();
    let mut observations = BTreeMap::<String, DelegationObservation>::new();
    let mut completions = tokio::task::JoinSet::new();
    let mut completion_ids = BTreeMap::<tokio::task::Id, (String, u64)>::new();
    let mut noticed = BTreeMap::<String, u64>::new();
    let mut retry = BTreeMap::<String, CompletionRetry>::new();
    // Children whose park waits for their parent's turn, by parent: the
    // parent's next observation ends the wait.
    let mut parked_behind = BTreeMap::<String, String>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut quota_open = true;
    let mut refresh_open = true;
    loop {
        if let Some(config) = policy.start_quota_source_scan() {
            let controller = crate::controller::Controller {
                config,
                state: Default::default(),
            };
            let reader = policy.quota_source_reader.clone();
            let task = jobs.spawn(async move {
                let requests = quota_source_scan(move || reader(controller)).await?;
                Ok(Completed::QuotaSources(requests))
            });
            policy.quota_source_task_started(task.id());
        }
        if !crate::upgrade::is_draining() {
            for (id, job) in dispatch.ready(Instant::now()) {
                let backend = backend.clone();
                let runtime = state.clone();
                let parent = id.0.clone();
                tracing::debug!(parent_session_id = %parent, request_id = %id.1, "delegation dispatch");
                let task = jobs.spawn(async move {
                    match job {
                        Job::Execute(request) => {
                            let _admission = if matches!(
                                request.action,
                                SubagentToolAction::SendInput { .. }
                                    | SubagentToolAction::SendMessage { .. }
                                    | SubagentToolAction::Spawn { .. }
                                    | SubagentToolAction::RequestReview { .. }
                                    | SubagentToolAction::AskOracle { .. }
                                    | SubagentToolAction::OverrideReview { .. }
                                    | SubagentToolAction::SetReviewBaseline { .. }
                                    // Its lifecycle persists Closing before yielding
                                    // admission; the next daemon finishes the close.
                                    | SubagentToolAction::CloseAgent { .. }
                                    // A temporary copy is safe to repeat after handoff.
                                    | SubagentToolAction::Fetch { .. }
                            ) {
                                None
                            } else {
                                let Ok(work) = crate::upgrade::activity_unless_draining(
                                    "delegation execution",
                                ) else {
                                    return Ok(Completed::Unaccepted);
                                };
                                Some(work)
                            };
                            let result = backend.execute_subagent_tool_durable(parent.clone(), request).await?;
                            // Durable results can wait for the replacement daemon.
                            drop(_admission);
                            let delivered = deliver(&runtime, &parent, result.clone()).await;
                            if let Err(error) = &delivered {
                                tracing::warn!(parent_session_id = %parent, request_id = %result.result.request_id, %error, "delegation delivery failed; retaining result for retry");
                            }
                            Ok(Completed::Executed(result, delivered.is_ok()))
                        }
                        Job::Deliver(result) => {
                            deliver(&runtime, &parent, result).await?;
                            Ok(Completed::Delivered)
                        }
                    }
                });
                identities.insert(task.id(), id);
            }
            for (child, observation) in &observations {
                if completion_ids.len() >= 32 {
                    break;
                }
                let Some(outcome) = observation.outcome.as_ref() else {
                    continue;
                };
                if noticed.get(child) == Some(&outcome.completed_ordinal)
                    || completion_ids.values().any(|(id, _)| id == child)
                    || retry
                        .get(child)
                        .is_some_and(|retry| retry.holds(outcome.completed_ordinal, Instant::now()))
                {
                    continue;
                }
                let child_id = child.clone();
                let backend = backend.clone();
                let state = state.clone();
                let observation = observation.clone();
                let task = completions.spawn(async move {
                    complete_child(state, backend, child_id, observation).await
                });
                completion_ids.insert(task.id(), (child.clone(), outcome.completed_ordinal));
            }
        }
        tokio::select! {
            _ = cancellation.cancelled() => break,
            update = updates.recv() => {
                let Some((id, observation)) = update else { anyhow::bail!("delegation observation feed stopped") };
                // A change in the parent ends the wait of children whose park
                // follows its turn; a change in a held child's liveness ends
                // its wait for the give-up time.
                let waiting_on_it = parked_behind
                    .iter()
                    .filter(|(_, parent)| **parent == id)
                    .map(|(child, _)| child.clone())
                    .collect::<Vec<_>>();
                for child in waiting_on_it {
                    retry.remove(&child);
                }
                if observation.as_ref().map(|o| &o.work_liveness)
                    != observations.get(&id).map(|o| &o.work_liveness)
                {
                    retry.remove(&id);
                }
                if let Some(observation) = observation {
                    policy.observe(&id, &observation);
                    dispatch.observe(&id, &observation.requests);
                    observations.insert(id, observation);
                } else {
                    dispatch.retire(&id);
                    observations.remove(&id);
                    retry.remove(&id);
                    // A parked child loses its observer and gets a new one
                    // when `send_message` starts it again. That observer first
                    // reports the turn this loop already settled; forgetting
                    // it here parked the child again under the input that
                    // was starting it. Forget a session only once it is gone.
                    if state.session_record(&id).is_none() {
                        noticed.remove(&id);
                    }
                }
            }
            completed = jobs.join_next_with_id(), if !jobs.is_empty() => {
                let Some(completed) = completed else { continue };
                let (task_id, result) = match completed {
                    Ok((id, result)) => (id, result),
                    Err(error) => (error.id(), Err(anyhow::Error::from(error))),
                };
                if policy.is_quota_source_task(task_id) {
                    match result {
                        Ok(Completed::QuotaSources(requests)) => {
                            policy.quota_sources_finished(task_id, Ok(requests));
                        }
                        Ok(_) => {
                            policy.quota_sources_finished(
                                task_id,
                                Err(anyhow::anyhow!("quota source scan returned the wrong result")),
                            );
                        }
                        Err(error) => policy.quota_sources_finished(task_id, Err(error)),
                    }
                    continue;
                }
                if let Some(id) = identities.remove(&task_id) {
                    match result {
                        Ok(Completed::Executed(result, delivered)) => {
                            tracing::debug!(parent_session_id = %id.0, request_id = %id.1, delivered,
                                is_error = result.result.is_error, "delegation execution completed");
                            dispatch.executed(&id, result);
                            dispatch.delivered(&id, delivered);
                        },
                        Ok(Completed::Delivered) => {
                            tracing::debug!(parent_session_id = %id.0, request_id = %id.1, "delegation result delivered");
                            dispatch.delivered(&id, true);
                        }
                        Ok(Completed::Unaccepted) => dispatch.unaccepted(&id),
                        Ok(Completed::QuotaSources(_)) => {
                            tracing::error!("untracked quota source scan completion");
                        }
                        Err(error) => {
                            tracing::warn!(parent_session_id = %id.0, request_id = %id.1, %error, "delegation task failed");
                            dispatch.failed_task(&id);
                        }
                    }
                }
            }
            completed = completions.join_next_with_id(), if !completions.is_empty() => {
                let Some(completed) = completed else { continue };
                let (task_id, result) = match completed {
                    Ok((id, result)) => (id, result),
                    Err(error) => (error.id(), Err(anyhow::Error::from(error))),
                };
                if let Some((id, ordinal)) = completion_ids.remove(&task_id) {
                    match result {
                        Ok(ChildCompletion::Settled) => { noticed.insert(id.clone(), ordinal); retry.remove(&id); parked_behind.remove(&id); }
                        Ok(ChildCompletion::RetryPark | ChildCompletion::AwaitingReview | ChildCompletion::Draining) => {
                            parked_behind.remove(&id);
                            let (next, _) = CompletionRetry::schedule(retry.remove(&id), ordinal, Instant::now(), None);
                            retry.insert(id, next);
                        }
                        Ok(ChildCompletion::Held { give_up_at_ms }) => {
                            let remaining = u64::try_from(give_up_at_ms.saturating_sub(mj_core::clock::epoch_millis())).unwrap_or(0);
                            retry.insert(id, CompletionRetry::waiting(ordinal, Some(Instant::now() + Duration::from_millis(remaining))));
                        }
                        Ok(ChildCompletion::ParkAfterParentTurn { parent }) => {
                            parked_behind.insert(id.clone(), parent);
                            retry.insert(id, CompletionRetry::waiting(ordinal, None));
                        }
                        Err(error) => {
                            let failure = format!("{error:#}");
                            let (next, report) = CompletionRetry::schedule(retry.remove(&id), ordinal, Instant::now(), Some(failure.clone()));
                            if report {
                                tracing::warn!(child_session_id = %id, error = %failure, retry_in_seconds = next.at.map_or(0, |at| (at - Instant::now()).as_secs()), "delegation child completion failed; retrying with backoff");
                            }
                            retry.insert(id, next);
                        }
                    }
                }
            }
            changed = revisions.changed() => { changed.context("daemon revision feed stopped")?; policy.sync(false); }
            intent = policy.refresh_rx.recv(), if refresh_open => {
                match intent {
                    Some(intent) => policy.refresh(intent),
                    None => {
                        refresh_open = false;
                        tracing::error!("delegation quota refresh intent feed stopped");
                    }
                }
            },
            update = policy.quota_rx.recv(), if quota_open => match update {
                Some(update) => policy.quota(update),
                None => { quota_open = false; tracing::error!("delegation quota service stopped"); }
            },
            _ = tick.tick() => {
                policy.tick();
            },
        }
        // Coalesced receives can be immediately ready without Tokio's channel budget.
        tokio::task::yield_now().await;
    }
    jobs.abort_all();
    completions.abort_all();
    while let Some(result) = jobs.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::error!(%error, "delegation task failed during shutdown");
        }
    }
    while let Some(result) = completions.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::error!(%error, "delegation completion failed during shutdown");
        }
    }
    Ok(())
}

async fn quota_source_scan(
    read: impl FnOnce() -> Vec<crate::quota::QuotaRefreshRequest> + Send + 'static,
) -> Result<Vec<crate::quota::QuotaRefreshRequest>> {
    tokio::task::spawn_blocking(read)
        .await
        .context("quota source scan blocking task failed")
}

async fn complete_child(
    state: Arc<RuntimeState>,
    backend: Arc<ApiBackend>,
    child_id: String,
    observation: DelegationObservation,
) -> Result<ChildCompletion> {
    let id = child_id.clone();
    let relation =
        tokio::task::spawn_blocking(move || crate::database::load_subagent(&id)).await??;
    let Some(relation) = relation else {
        return Ok(ChildCompletion::Settled);
    };
    let outcome = observation
        .outcome
        .clone()
        .context("child completion has no outcome")?;
    // The review host owns whether a session's review is open. Asked once,
    // before anything is decided about the ended turn, and before taking the
    // handoff admission: during startup the host defers the answer until its
    // interrupted-review sweep ends, and a handoff must not wait on reviews.
    let review_open = state.child_awaits_review(&child_id).await;
    let Ok(work) = crate::upgrade::activity_unless_draining("delegation child completion") else {
        return Ok(ChildCompletion::Draining);
    };
    settle_child(
        &state,
        &backend,
        work,
        relation,
        ChildSeen {
            in_flight: observation.in_flight,
            liveness: observation.work_liveness,
        },
        outcome,
        review_open,
    )
    .await
}

/// What the worker published beside a child's ended turn.
struct ChildSeen {
    /// Commands queued or running on the child.
    in_flight: Vec<String>,
    /// Whether the processes the turn left running are still alive.
    liveness: Option<mj_core::work_facts::WorkLiveness>,
}

async fn settle_child(
    state: &RuntimeState,
    backend: &ApiBackend,
    work: crate::upgrade::Work,
    relation: mj_core::subagent::SubagentRecord,
    seen: ChildSeen,
    outcome: mj_core::state::MaterializedTurnOutcome,
    review_open: bool,
) -> Result<ChildCompletion> {
    let ChildSeen {
        in_flight,
        liveness,
    } = seen;
    let child_id = relation.child_session_id.clone();
    if relation.kind == mj_core::subagent::SubagentKind::Reviewer {
        let id = child_id.clone();
        let command_id = outcome.command_id.clone();
        let answer =
            tokio::task::spawn_blocking(move || crate::database::load_subagent_report(&id))
                .await??
                .handback
                .filter(|handback| handback.answers(&command_id))
                .map(|handback| handback.message);
        state
            .review_host()
            .reviewer_finished(relation.parent_session_id, child_id.clone(), answer)
            .await
            .map_err(anyhow::Error::msg)?;
        // The review host owns reviewer closes and retries a failing one with
        // backoff; closing here as well resubmitted it every second (#1456).
        state.review_host().close_child(child_id);
        return Ok(ChildCompletion::Settled);
    }
    if relation
        .noticed_turn
        .is_some_and(|turn| turn > outcome.completed_ordinal)
    {
        return Ok(ChildCompletion::Settled);
    }
    // A child that requested review and ended its turn is waiting for the
    // verdict, not forgetting to hand back: no reminder, no notice, no park.
    if review_open {
        return Ok(ChildCompletion::AwaitingReview);
    }
    // A reminder turn is judged every time: whether its child is held depends
    // on the worker's liveness, which changes without a new turn.
    if relation.noticed_turn != Some(outcome.completed_ordinal)
        || mj_core::subagent::is_handback_reminder(&outcome.command_id)
    {
        let nudge = backend
            .remind_subagent_to_hand_back(
                &child_id,
                relation.handback_tool,
                &outcome,
                liveness.as_ref(),
                &in_flight,
            )
            .await?;
        if let crate::server_runtime::api::ReportNudge::Holding { give_up_at_ms } = nudge {
            return Ok(ChildCompletion::Held { give_up_at_ms });
        }
        let id = child_id.clone();
        tokio::task::spawn_blocking(move || {
            crate::database::mark_subagent_turn_noticed(&id, outcome.completed_ordinal)
        })
        .await??;
        if nudge == crate::server_runtime::api::ReportNudge::Pending {
            return Ok(ChildCompletion::Settled);
        }
    }
    // Reconciliation asks the review host about sibling children, which can
    // wait on the startup sweep: it runs outside the handoff admission, which
    // is taken again for the park.
    drop(work);
    // Park only once the parent has been told, or when this ending is not news.
    // The report and the marker that covers this ending are written together,
    // so the marker, read after the reconcile, says which it is. A finish that
    // is news but not yet covered is asked again: the readers' snapshot may not
    // have caught up with the observation (the session actor runs ahead of it).
    // The retry backs off and ends when the report is enqueued; no path parks a
    // child whose parent has not heard of it.
    match backend
        .reconcile_child_report(
            &relation.parent_session_id,
            &child_id,
            outcome.completed_ordinal,
        )
        .await?
    {
        crate::server_runtime::api::ChildReport::Told => {}
        crate::server_runtime::api::ChildReport::NotNews(why) => {
            tracing::debug!(child_session_id = %child_id, why, "a child's ending is not news; it is not parked for it");
            return Ok(ChildCompletion::Settled);
        }
        // The reconcile's view says the child has not finished. A view that is
        // behind the observation is not news, so it is asked again; a child that
        // went on to a newer turn is: that turn's ending is what will be
        // reported.
        crate::server_runtime::api::ChildReport::NotFinished => {
            let child = child_id.clone();
            let (last, active) = tokio::task::spawn_blocking(move || {
                crate::server_runtime::api::load_child_turns(&child)
            })
            .await??;
            return Ok(if last > Some(outcome.completed_ordinal) || active {
                ChildCompletion::Settled
            } else {
                ChildCompletion::RetryPark
            });
        }
        crate::server_runtime::api::ChildReport::Awaiting => {
            return Ok(ChildCompletion::RetryPark);
        }
    }
    // Stopping the child stops the processes it started. Its parent may answer
    // "continue" to the report it just received, so the park waits for the end
    // of the parent turn that received it.
    if backend
        .park_waits_for_parent_turn(
            &relation.parent_session_id,
            &child_id,
            outcome.completed_ordinal,
            liveness.as_ref(),
        )
        .await?
    {
        return Ok(ChildCompletion::ParkAfterParentTurn {
            parent: relation.parent_session_id,
        });
    }
    let Ok(_work) = crate::upgrade::activity_unless_draining("delegation child park") else {
        return Ok(ChildCompletion::Draining);
    };
    match backend.park_subagent(&child_id).await? {
        crate::controller::ParkOutcome::Busy => Ok(ChildCompletion::RetryPark),
        crate::controller::ParkOutcome::Parked | crate::controller::ParkOutcome::NotRunning => {
            Ok(ChildCompletion::Settled)
        }
    }
}

async fn deliver(
    runtime: &RuntimeState,
    parent: &str,
    result: StoredDelegationResult,
) -> Result<()> {
    let _work = crate::upgrade::activity_unless_draining("delegation result delivery")?;
    tokio::time::timeout(
        Duration::from_secs(5),
        deliver_result(runtime, parent, result),
    )
    .await
    .context("delegation result delivery timed out; durable result retained")?
}

async fn deliver_result(
    runtime: &RuntimeState,
    parent: &str,
    stored_result: StoredDelegationResult,
) -> Result<()> {
    let handle = runtime.workspace_session_handle(parent).await?;
    let mut lease = handle.lease_connection().await?;
    let request_id = stored_result.result.request_id.clone();
    let delivered = lease
        .connection_mut()
        .complete_subagent_request(stored_result.result.clone())
        .await;
    lease.release();
    delivered?;
    if let Err(error) = runtime.reconcile_parent_reports(parent).await {
        tracing::warn!(
            parent_session_id = parent,
            error = format!("{error:#}"),
            "could not enqueue the parent's finished child reports after result delivery"
        );
    }
    let parent = parent.to_owned();
    let acknowledged_parent = parent.clone();
    tokio::task::spawn_blocking(move || {
        crate::database::acknowledge_delegation(acknowledged_parent, request_id)
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::ParkOutcome;
    use crate::controller::test_support::{IsolatedTest, test_name};
    use crate::server_runtime::api::ExportRuntime;
    use mj_client::session::BoxFuture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Hard-won: #1487: native provider source reads blocked unrelated daemon events.
    #[cfg(unix)]
    #[tokio::test]
    async fn slow_quota_source_read_does_not_block_runtime_revision_handling() {
        const CHILD: &str = "MJ_TEST_QUOTA_SOURCE_DISPATCH";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "slow_quota_source_read_does_not_block_runtime_revision_handling",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "quota-source-dispatch-1487")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let manager = crate::session_manager::spawn_session_manager().unwrap();
        let recovery = crate::recovery::RecoveryCoordinator::spawn(manager.control.clone());
        let upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
            manager.control.clone(),
            &recovery.observer(),
        );
        let source_home = tempfile::tempdir().unwrap();
        let mut initial_config = mj_core::config::Config::default();
        initial_config.profiles.insert(
            "claude".into(),
            mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Claude,
                source_home.path().to_path_buf(),
            ),
        );
        let initial = crate::controller::Controller {
            config: initial_config.clone(),
            state: Default::default(),
        };
        let state = Arc::new(crate::daemon::RuntimeState::new_with_controller_loader(
            manager.control.clone(),
            initial,
            recovery.observer(),
            upgrades.observer(),
            Vec::new(),
            || {
                Ok(crate::controller::Controller {
                    config: mj_core::config::Config::default(),
                    state: Default::default(),
                })
            },
        ));
        let (feed, updates) = crate::session_manager::delegation_channel();
        let (first_done_tx, first_done_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_done_tx = Arc::new(std::sync::Mutex::new(Some(first_done_tx)));
        let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let calls = Arc::new(AtomicUsize::new(0));
        let reader_calls = calls.clone();
        let reader_first_done = first_done_tx.clone();
        let reader_started = started_tx.clone();
        let reader_release = release_rx.clone();
        let reader = Arc::new(move |_controller| {
            let call = reader_calls.fetch_add(1, Ordering::SeqCst);
            if call == 1 {
                if let Some(started_tx) = reader_started
                    .lock()
                    .expect("started signal poisoned")
                    .take()
                {
                    let _ = started_tx.send(());
                }
                reader_release
                    .lock()
                    .expect("release receiver poisoned")
                    .recv_timeout(Duration::from_secs(10))
                    .expect("test releases the blocked source reader");
            }
            if call == 0
                && let Some(first_done_tx) = reader_first_done
                    .lock()
                    .expect("first completion signal poisoned")
                    .take()
            {
                let _ = first_done_tx.send(());
            }
            // This regression exercises dispatcher ownership, never providers
            // or platform credential stores.
            Vec::new()
        });
        let stop = CancellationToken::new();
        let (services, task) = spawn_with_quota_source_reader(
            state.clone(),
            manager.control.clone(),
            updates,
            stop.clone(),
            reader,
        );
        let _feed = feed;
        tokio::time::timeout(Duration::from_secs(5), first_done_rx)
            .await
            .expect("the production dispatcher should finish its initial source scan")
            .unwrap();
        assert!(matches!(
            services.backend.quota_report("claude", "old-identity"),
            Ok(crate::server_runtime::api::QuotaRead::Pending(_))
        ));
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("source reconciliation should dispatch another source scan")
            .unwrap();

        let (config, catalog) = {
            let mut owner = state.owner();
            let mut config = owner.controller().config.clone();
            config.phone.enabled = !config.phone.enabled;
            owner.install_config(config.clone());
            let catalog = state
                .profile_catalog
                .get()
                .expect("Policy installs the profile catalog")
                .clone();
            (config, catalog)
        };
        state.publish_revision();
        let synced = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if catalog.live().is_ok_and(|live| live == config) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        release_tx.send(()).unwrap();
        synced.expect("the delegation loop should sync config while source I/O is blocked");
        assert!(calls.load(Ordering::SeqCst) >= 1);

        stop.cancel();
        task.await.unwrap().unwrap();
    }

    struct BusyThenParked(AtomicUsize);
    impl ExportRuntime for BusyThenParked {
        fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord> {
            matches!(session_id, "parent" | "child").then(|| {
                crate::daemon::tests::runtime_test_session(
                    session_id,
                    "workspace",
                    mj_core::state::SessionState::Running,
                )
            })
        }
        fn checkpoint_now(
            &self,
            _: String,
        ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
            Box::pin(async { anyhow::bail!("a park does not checkpoint") })
        }
        fn park_subagent(self: Arc<Self>, _: String) -> BoxFuture<'static, Result<ParkOutcome>> {
            Box::pin(async move {
                Ok(if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    ParkOutcome::Busy
                } else {
                    ParkOutcome::Parked
                })
            })
        }
    }

    /// The session actor's view of the child: connected and idle after `turn`,
    /// which is what a delegation observation is made from.
    async fn publish_idle_child(
        manager: &crate::daemon::tests::TestRemoteManager,
        turn: &mj_core::state::MaterializedTurnOutcome,
    ) {
        let mut view = crate::daemon::tests::ready_startup_view();
        view.snapshot
            .as_mut()
            .unwrap()
            .materialized
            .last_turn_outcome = Some(turn.clone());
        manager
            .publisher
            .publish("child".into(), view)
            .await
            .unwrap();
    }

    /// A child's stored last turn: what the readers' snapshot knows of it.
    fn store_finished_turn(
        child: &str,
        command_id: &str,
        completed_ordinal: u64,
    ) -> mj_core::state::MaterializedTurnOutcome {
        let turn = mj_core::state::MaterializedTurnOutcome {
            answered_through: None,
            diagnostic: None,
            usage: None,
            command_id: command_id.into(),
            accepted_ordinal: Some(1),
            turn_start_position: None,
            completed_ordinal,
            completed_at_ms: 1,
            outcome: mj_core::state::TurnOutcomeKind::Completed {
                stop_reason: "end_turn".into(),
            },
            work_facts: None,
        };
        let mut conversation = mj_core::state::MaterializedSession::empty(child);
        conversation.applied_event_ordinal = completed_ordinal;
        conversation.applied_event_digest = format!("{completed_ordinal:064x}");
        conversation.last_turn_outcome = Some(turn.clone());
        crate::database::save_materialized_session(&conversation).unwrap();
        turn
    }

    // Hard-won: 5bb43721a: child completion wrote a redundant notice to the parent.
    #[tokio::test]
    async fn a_finished_child_retries_busy_parking_without_a_parent_notice() {
        const CHILD: &str = "MJ_TEST_DELEGATION_PARK_RETRY";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_finished_child_retries_busy_parking_without_a_parent_notice",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-1171-park")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("park retry").unwrap();
        let parent = crate::daemon::tests::runtime_test_session(
            "parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        let child = crate::daemon::tests::runtime_test_session(
            "child",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        crate::database::save_session(&parent).unwrap();
        let relation = crate::daemon::tests::runtime_test_subagent("child", "parent");
        crate::database::save_subagent_session(&child, &relation).unwrap();
        let manager = crate::daemon::tests::TestRemoteManager::for_session("child").await;
        let state = crate::daemon::tests::test_runtime_state_with_manager(&manager);
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(0)));
        // The parent has no actor. Durable report enqueue must not need one
        // or prevent either park attempt.
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            parks.clone(),
        ));
        state.install_report_backend(&backend);
        // The readers' snapshot holds the turn the observation reports.
        let turn = store_finished_turn("child", "turn", 10);
        publish_idle_child(&manager, &turn).await;
        let observation = DelegationObservation {
            requests: Default::default(),
            completed: Vec::new(),
            in_flight: Vec::new(),
            credential_signal: None,
            work_liveness: None,
            undelivered_mailbox_event_keys: Vec::new(),
            outcome: Some(turn),
        };
        assert!(matches!(
            complete_child(
                state.clone(),
                backend.clone(),
                "child".into(),
                observation.clone()
            )
            .await
            .unwrap(),
            ChildCompletion::RetryPark
        ));
        assert!(matches!(
            complete_child(state.clone(), backend, "child".into(), observation)
                .await
                .unwrap(),
            ChildCompletion::Settled
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 2);
        assert_eq!(
            crate::database::load_subagent("child")
                .unwrap()
                .unwrap()
                .noticed_turn,
            Some(10)
        );
    }

    // A child that asked for review and ended its turn is waiting for the
    // verdict. The completion loop must not treat that as a missing handback:
    // no reminder, no notice, no park, until the review is gone.
    #[tokio::test]
    async fn a_child_with_an_open_review_is_not_reminded_noticed_or_parked() {
        const CHILD: &str = "MJ_TEST_DELEGATION_OPEN_REVIEW";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_child_with_an_open_review_is_not_reminded_noticed_or_parked",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-open-review")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("open review").unwrap();
        let parent = crate::daemon::tests::runtime_test_session(
            "parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        let child = crate::daemon::tests::runtime_test_session(
            "child",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        crate::database::save_session(&parent).unwrap();
        let mut relation = crate::daemon::tests::runtime_test_subagent("child", "parent");
        relation.handback_tool = true;
        crate::database::save_subagent_session(&child, &relation).unwrap();
        let manager = crate::daemon::tests::TestRemoteManager::for_session("child").await;
        let state = crate::daemon::tests::test_runtime_state_with_manager(&manager);
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(1)));
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            parks.clone(),
        ));
        state.install_report_backend(&backend);
        let outcome = store_finished_turn("child", "turn", 10);
        publish_idle_child(&manager, &outcome).await;
        let stored = crate::database::load_subagent("child").unwrap().unwrap();
        let settle = |review_open| {
            settle_child(
                &state,
                &backend,
                crate::upgrade::activity_unless_draining("test completion").unwrap(),
                stored.clone(),
                ChildSeen {
                    in_flight: Vec::new(),
                    liveness: None,
                },
                outcome.clone(),
                review_open,
            )
        };
        assert!(matches!(
            settle(true).await.unwrap(),
            ChildCompletion::AwaitingReview
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 1, "no park attempt");
        assert_eq!(
            crate::database::load_subagent("child")
                .unwrap()
                .unwrap()
                .noticed_turn,
            None
        );
        let report = crate::database::load_subagent_report("child").unwrap();
        assert!(report.reminder.is_none() && report.reminder_failed_for.is_none());
        // With the review gone, the same turn is judged as before: the child
        // handed back, so it is reported and parked.
        assert!(
            crate::database::record_subagent_handback(
                "child",
                &mj_core::subagent::SubagentHandback {
                    command_id: "turn".into(),
                    message: "done".into(),
                    recorded_at_ms: 1,
                },
            )
            .unwrap()
        );
        assert!(matches!(
            settle(false).await.unwrap(),
            ChildCompletion::Settled
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 2);
        assert_eq!(
            crate::database::load_subagent("child")
                .unwrap()
                .unwrap()
                .noticed_turn,
            Some(10)
        );
    }

    // A completion is decided from the readers' snapshot, which refreshes after
    // the write that makes a turn durable returns, while the observation comes
    // from the session actor and can run ahead of it. Behind the observation the
    // snapshot says nothing yet: no report, no park, and the completion asks
    // again; once it has caught up the report is enqueued and only then is the
    // child parked, once.
    #[tokio::test]
    async fn a_child_is_not_parked_until_the_parent_has_its_report() {
        const CHILD: &str = "MJ_TEST_DELEGATION_PARK_AFTER_REPORT";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_child_is_not_parked_until_the_parent_has_its_report",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-park-after-report")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("park after report").unwrap();
        let parent = crate::daemon::tests::runtime_test_session(
            "parent",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        let child = crate::daemon::tests::runtime_test_session(
            "child",
            &workspace.id,
            mj_core::state::SessionState::Running,
        );
        crate::database::save_session(&parent).unwrap();
        crate::database::save_subagent_session(
            &child,
            &crate::daemon::tests::runtime_test_subagent("child", "parent"),
        )
        .unwrap();
        let manager = crate::daemon::tests::TestRemoteManager::for_session("child").await;
        let state = crate::daemon::tests::test_runtime_state_with_manager(&manager);
        // Every park attempt succeeds, so a park can only be refused by the rule.
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(1)));
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            parks.clone(),
        ));
        state.install_report_backend(&backend);

        // The snapshot holds the child's earlier turn; the observation is the
        // turn after it.
        let earlier = store_finished_turn("child", "earlier-turn", 5);
        let newer = mj_core::state::MaterializedTurnOutcome {
            command_id: "turn".into(),
            completed_ordinal: 10,
            ..earlier
        };
        publish_idle_child(&manager, &newer).await;
        let observation = DelegationObservation {
            requests: Default::default(),
            completed: Vec::new(),
            in_flight: Vec::new(),
            credential_signal: None,
            work_liveness: None,
            undelivered_mailbox_event_keys: Vec::new(),
            outcome: Some(newer),
        };
        let complete = || {
            complete_child(
                state.clone(),
                backend.clone(),
                "child".into(),
                observation.clone(),
            )
        };
        let told = || crate::database::pending_mailbox_events(10).unwrap().len();
        assert!(matches!(
            complete().await.unwrap(),
            ChildCompletion::RetryPark
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 1, "no park while behind");
        assert_eq!(told(), 0, "nothing is reported from a snapshot behind");

        // The snapshot catches up: the report is enqueued, and then the child is
        // parked.
        store_finished_turn("child", "turn", 10);
        assert!(matches!(
            complete().await.unwrap(),
            ChildCompletion::Settled
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 2, "parked once, after");
        assert_eq!(told(), 1, "the parent has its report");
        let marker = crate::database::load_subagent("child")
            .unwrap()
            .unwrap()
            .reported_finish
            .expect("the report's marker is written with it");
        assert!(marker.covers(10));
    }

    struct ParkedParents;
    impl ExportRuntime for ParkedParents {
        fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord> {
            session_id.starts_with("parent").then(|| {
                crate::daemon::tests::runtime_test_session(
                    session_id,
                    "workspace",
                    mj_core::state::SessionState::Running,
                )
            })
        }
        fn checkpoint_now(
            &self,
            _: String,
        ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
            Box::pin(async { anyhow::bail!("a park does not checkpoint") })
        }
        fn park_subagent(self: Arc<Self>, _: String) -> BoxFuture<'static, Result<ParkOutcome>> {
            Box::pin(async { Ok(ParkOutcome::Parked) })
        }
    }

    // After a failed review-host startup recovery its open-review answer is an
    // error. Delegation decides what that means: a child that cannot request
    // review never asks, and one that can settles as having no open review.
    #[tokio::test]
    async fn children_settle_when_the_review_host_failed_its_startup_recovery() {
        const CHILD: &str = "MJ_TEST_DELEGATION_REVIEW_HOST_FAILED";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "children_settle_when_the_review_host_failed_its_startup_recovery",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-review-host-failed")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("host failed").unwrap();
        let mut controller = crate::controller::Controller {
            config: Default::default(),
            state: Default::default(),
        };
        for (parent_id, child_id, review) in [
            (
                "parent-off",
                "child-off",
                mj_core::config::SessionReview::Off,
            ),
            (
                "parent-push",
                "child-push",
                mj_core::config::SessionReview::Push {
                    reviewers: Some(vec![mj_core::config::SubagentChoice {
                        model: "review-model".into(),
                        effort: None,
                    }]),
                },
            ),
        ] {
            let mut parent = crate::daemon::tests::runtime_test_session(
                parent_id,
                &workspace.id,
                mj_core::state::SessionState::Running,
            );
            parent.review = Some(Box::new(review));
            let child = crate::daemon::tests::runtime_test_session(
                child_id,
                &workspace.id,
                mj_core::state::SessionState::Running,
            );
            crate::database::save_session(&parent).unwrap();
            let relation = crate::daemon::tests::runtime_test_subagent(child_id, parent_id);
            crate::database::save_subagent_session(&child, &relation).unwrap();
            controller.state.sessions.insert(parent_id.into(), parent);
            controller.state.sessions.insert(child_id.into(), child);
            controller
                .state
                .subagents
                .insert(child_id.into(), relation.into());
        }
        let state = crate::daemon::tests::test_runtime_state_with(controller);
        assert!(state.child_can_request_review("child-push"));
        assert!(!state.child_can_request_review("child-off"));
        state
            .review_host()
            .fail_startup_recovery("the sweep could not read the store");
        assert!(
            state
                .review_host()
                .has_open_review("child-push")
                .await
                .is_err(),
            "the host answers with an error once its recovery failed"
        );
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            Arc::new(ParkedParents),
        ));
        state.install_report_backend(&backend);
        for child in ["child-off", "child-push"] {
            let observation = DelegationObservation {
                requests: Default::default(),
                completed: Vec::new(),
                in_flight: Vec::new(),
                credential_signal: None,
                work_liveness: None,
                undelivered_mailbox_event_keys: Vec::new(),
                outcome: Some(mj_core::state::MaterializedTurnOutcome {
                    work_facts: None,
                    answered_through: None,
                    diagnostic: None,
                    usage: None,
                    command_id: "turn".into(),
                    accepted_ordinal: Some(1),
                    turn_start_position: None,
                    completed_ordinal: 10,
                    completed_at_ms: 1,
                    outcome: mj_core::state::TurnOutcomeKind::Completed {
                        stop_reason: "end_turn".into(),
                    },
                }),
            };
            assert!(
                matches!(
                    complete_child(state.clone(), backend.clone(), child.into(), observation)
                        .await
                        .unwrap(),
                    ChildCompletion::Settled
                ),
                "{child} settles"
            );
        }
    }

    // Everything a held-child test needs: a running parent, a child that has
    // the handback tool, and a park that counts its attempts.
    struct HeldChild {
        manager: crate::daemon::tests::TestRemoteManager,
        state: Arc<RuntimeState>,
        backend: Arc<ApiBackend>,
        parks: Arc<ToggleParent>,
    }

    struct ToggleParent {
        running: std::sync::atomic::AtomicBool,
        parks: AtomicUsize,
    }
    impl ExportRuntime for ToggleParent {
        fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord> {
            matches!(session_id, "parent" | "child").then(|| {
                crate::daemon::tests::runtime_test_session(
                    session_id,
                    "workspace",
                    if session_id == "parent" && !self.running.load(Ordering::SeqCst) {
                        mj_core::state::SessionState::Stopped
                    } else {
                        mj_core::state::SessionState::Running
                    },
                )
            })
        }
        fn checkpoint_now(
            &self,
            _: String,
        ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
            Box::pin(async { anyhow::bail!("a park does not checkpoint") })
        }
        fn park_subagent(self: Arc<Self>, _: String) -> BoxFuture<'static, Result<ParkOutcome>> {
            Box::pin(async move {
                self.parks.fetch_add(1, Ordering::SeqCst);
                Ok(ParkOutcome::Parked)
            })
        }
    }

    impl HeldChild {
        async fn new(name: &str) -> Self {
            let workspace = crate::database::create_workspace(name).unwrap();
            let parent = crate::daemon::tests::runtime_test_session(
                "parent",
                &workspace.id,
                mj_core::state::SessionState::Running,
            );
            let child = crate::daemon::tests::runtime_test_session(
                "child",
                &workspace.id,
                mj_core::state::SessionState::Running,
            );
            crate::database::save_session(&parent).unwrap();
            let mut relation = crate::daemon::tests::runtime_test_subagent("child", "parent");
            relation.handback_tool = true;
            crate::database::save_subagent_session(&child, &relation).unwrap();
            let manager =
                crate::daemon::tests::TestRemoteManager::for_sessions(&["child", "parent"]).await;
            let state = crate::daemon::tests::test_runtime_state_with_manager(&manager);
            let parks = Arc::new(ToggleParent {
                running: true.into(),
                parks: AtomicUsize::new(0),
            });
            let backend = Arc::new(ApiBackend::new(
                state.session_manager.client(),
                Arc::new(|_| None),
                parks.clone(),
            ));
            state.install_report_backend(&backend);
            Self {
                manager,
                state,
                backend,
                parks,
            }
        }

        /// The child idle after `turn`, as its worker published it.
        async fn child_ended(&self, turn: &mj_core::state::MaterializedTurnOutcome) {
            let mut view = crate::daemon::tests::ready_startup_view();
            let snapshot = view.snapshot.as_mut().unwrap();
            snapshot.materialized.last_turn_outcome = Some(turn.clone());
            // The journal position marks which publication the actor serves.
            let mut operational = (*snapshot.operational).clone();
            operational.latest_ordinal = turn.completed_ordinal;
            snapshot.operational = operational.into();
            self.manager
                .publisher
                .publish("child".into(), view)
                .await
                .unwrap();
            let handle = self
                .state
                .session_manager
                .client()
                .session("child".to_owned())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while handle.view().snapshot.as_ref().is_none_or(|snapshot| {
                    snapshot.operational.latest_ordinal != turn.completed_ordinal
                }) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the child's view is served");
        }

        /// What the delegation loop does with the child's ended turn, and the
        /// commands it sent the child's worker meanwhile.
        async fn settle(
            &mut self,
            liveness: Option<mj_core::work_facts::WorkLiveness>,
            outcome: &mj_core::state::MaterializedTurnOutcome,
        ) -> (
            ChildCompletion,
            Vec<(String, String, mj_core::relay::RelayCommand)>,
        ) {
            let stored = crate::database::load_subagent("child").unwrap().unwrap();
            let (done, sent) = tokio::join!(
                settle_child(
                    &self.state,
                    &self.backend,
                    crate::upgrade::activity_unless_draining("test completion").unwrap(),
                    stored,
                    ChildSeen {
                        in_flight: Vec::new(),
                        liveness,
                    },
                    outcome.clone(),
                    false,
                ),
                self.manager.accept_submissions()
            );
            (done.unwrap(), sent)
        }

        /// The parent's view with the given facts, waited for until the
        /// actor serves it.
        async fn parent_view(
            &self,
            execution: mj_core::state::MaterializedExecutionState,
            latest_ordinal: u64,
            undelivered: Vec<String>,
        ) {
            let mut view = crate::daemon::tests::ready_startup_view();
            let snapshot = view.snapshot.as_mut().unwrap();
            snapshot.materialized.execution = execution;
            let mut operational = (*snapshot.operational).clone();
            operational.latest_ordinal = latest_ordinal;
            operational.undelivered_mailbox_event_keys = undelivered;
            snapshot.operational = operational.into();
            self.manager
                .publisher
                .publish("parent".into(), view)
                .await
                .unwrap();
            let handle = self
                .state
                .session_manager
                .client()
                .session("parent".to_owned())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while handle
                    .view()
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.operational.latest_ordinal != latest_ordinal)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the parent's view is served");
        }

        fn told(&self) -> usize {
            crate::database::pending_mailbox_events(10).unwrap().len()
        }
    }

    fn turn_at(
        command_id: &str,
        completed_ordinal: u64,
        completed_at_ms: i64,
        work_facts: Option<mj_core::work_facts::WorkFacts>,
    ) -> mj_core::state::MaterializedTurnOutcome {
        let mut turn = store_finished_turn("child", command_id, completed_ordinal);
        turn.completed_at_ms = completed_at_ms;
        turn.work_facts = work_facts;
        // The readers' snapshot holds the same turn as the observation.
        let mut conversation = mj_core::state::MaterializedSession::empty("child");
        conversation.applied_event_ordinal = completed_ordinal;
        conversation.applied_event_digest = format!("{completed_ordinal:064x}");
        conversation.last_turn_outcome = Some(turn.clone());
        crate::database::save_materialized_session(&conversation).unwrap();
        turn
    }

    fn liveness(completed_ordinal: u64, alive: u32) -> Option<mj_core::work_facts::WorkLiveness> {
        Some(mj_core::work_facts::WorkLiveness {
            completed_ordinal,
            alive,
            checked_at_ms: 0,
        })
    }

    fn one_running_process() -> mj_core::work_facts::WorkFacts {
        use mj_core::work_facts::*;
        WorkFacts {
            observed_at_ms: 0,
            processes: Observed::Known(ProcessFacts {
                running: 1,
                shown: vec![RunningProcess {
                    pid: 41,
                    elapsed_secs: 12,
                    command: "cargo test".into(),
                    detached: false,
                }],
                omitted: 0,
                identities: vec![ProcessIdentity {
                    pid: 41,
                    start: "s".into(),
                }],
                unattributed: 0,
            }),
            worktree: Observed::unavailable("not under test"),
        }
    }

    fn isolated(test: &str, instance: &str) -> bool {
        const CHILD: &str = "MJ_TEST_DELEGATION_HOLD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), test))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", instance)
            .isolated_store(root.path())
            .run();
        false
    }

    // A child that ended its reminder turn waiting for its own processes is
    // neither reported nor parked. When they exit it is woken once; after the
    // give-up time it is asked once to hand back what it has, and only if that
    // turn also ends without a handback is its last message reported.
    #[tokio::test]
    async fn a_held_child_is_woken_once_and_given_up_on_only_after_its_deadline() {
        if !isolated(
            "a_held_child_is_woken_once_and_given_up_on_only_after_its_deadline",
            "delegation-held-child",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut held = HeldChild::new("held child").await;
        let now = mj_core::clock::epoch_millis();
        let reminder = mj_core::subagent::handback_reminder_command_id(10);
        let turn = turn_at(&reminder, 20, now, Some(one_running_process()));
        held.child_ended(&turn).await;

        // Waiting: no report, no park, no prompt, and nothing marked. Judged
        // again from the same state, as a restarted loop would.
        for _ in 0..2 {
            let (done, sent) = held.settle(liveness(20, 2), &turn).await;
            assert!(
                matches!(done, ChildCompletion::Held { give_up_at_ms } if give_up_at_ms == now + mj_core::subagent::HOLD_GIVE_UP_MS)
            );
            assert!(sent.is_empty(), "{sent:?}");
        }
        assert_eq!(held.parks.parks.load(Ordering::SeqCst), 0);
        assert_eq!(held.told(), 0);
        assert_eq!(
            crate::database::load_subagent("child")
                .unwrap()
                .unwrap()
                .noticed_turn,
            None
        );

        // The processes exited: one wake prompt, still no report or park.
        let (done, sent) = held.settle(liveness(20, 0), &turn).await;
        assert!(matches!(done, ChildCompletion::Settled));
        assert_eq!(
            sent,
            vec![(
                "child".to_owned(),
                "hold-wake-20".to_owned(),
                mj_core::relay::RelayCommand::HoldFollowUp {
                    completed_command_id: reminder.clone(),
                    completed_ordinal: 20,
                    kind: mj_core::subagent::HoldFollowUpKind::ProcessesExited,
                    running: Vec::new(),
                }
            )]
        );
        assert_eq!(held.parks.parks.load(Ordering::SeqCst), 0);
        assert_eq!(held.told(), 0);

        // The woken child ended its turn without a handback: it is reminded
        // under the reminder's own id, not woken again, and held afresh.
        let woken = turn_at("hold-wake-20", 30, now, None);
        held.child_ended(&woken).await;
        let (done, sent) = held.settle(None, &woken).await;
        assert!(matches!(done, ChildCompletion::Settled));
        assert_eq!(
            sent.iter()
                .map(|(_, id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["handback-reminder-30"]
        );

        // Past the deadline with processes still running: one give-up prompt
        // naming them.
        let stale = now - mj_core::subagent::HOLD_GIVE_UP_MS - 1_000;
        let reminder = mj_core::subagent::handback_reminder_command_id(30);
        let turn = turn_at(&reminder, 40, stale, Some(one_running_process()));
        held.child_ended(&turn).await;
        let (done, sent) = held.settle(liveness(40, 1), &turn).await;
        assert!(matches!(done, ChildCompletion::Settled));
        let [(_, id, command)] = sent.as_slice() else {
            panic!("exactly one give-up prompt: {sent:?}");
        };
        assert_eq!(id, "hold-give-up-40");
        assert!(matches!(
            command,
            mj_core::relay::RelayCommand::HoldFollowUp {
                kind: mj_core::subagent::HoldFollowUpKind::GiveUp,
                running,
                ..
            } if running.len() == 1 && running[0].contains("cargo test")
        ));
        assert_eq!(held.parks.parks.load(Ordering::SeqCst), 0);

        // The give-up turn ended without a handback: its last message is the
        // report, and the child is parked.
        let gave_up = turn_at("hold-give-up-40", 50, now, None);
        held.child_ended(&gave_up).await;
        let (done, sent) = held.settle(None, &gave_up).await;
        assert!(
            matches!(done, ChildCompletion::Settled),
            "{}",
            match done {
                ChildCompletion::RetryPark => "RetryPark",
                ChildCompletion::Held { .. } => "Held",
                ChildCompletion::ParkAfterParentTurn { .. } => "ParkAfterParentTurn",
                ChildCompletion::AwaitingReview => "AwaitingReview",
                ChildCompletion::Draining => "Draining",
                ChildCompletion::Settled => "Settled",
            }
        );
        assert!(sent.is_empty(), "{sent:?}");
        assert_eq!(held.told(), 1, "the parent hears of it once");
        assert_eq!(held.parks.parks.load(Ordering::SeqCst), 1);
    }

    // A park stops the processes the child started, so a told child with live
    // processes waits for the end of the parent turn that received its report,
    // and the parent's answer to it ends the park for good.
    #[tokio::test]
    async fn a_told_child_with_live_processes_is_parked_after_its_parents_turn() {
        if !isolated(
            "a_told_child_with_live_processes_is_parked_after_its_parents_turn",
            "delegation-park-after-parent-turn",
        ) {
            return;
        }
        use mj_core::state::MaterializedExecutionState::{Idle, Running};
        let _writer = crate::database::install_isolated_test_writer();
        let mut held = HeldChild::new("park gate").await;
        let turn = turn_at("turn", 10, 1, None);
        held.child_ended(&turn).await;
        assert!(
            crate::database::record_subagent_handback(
                "child",
                &mj_core::subagent::SubagentHandback {
                    command_id: "turn".into(),
                    message: "done".into(),
                    recorded_at_ms: 1,
                },
            )
            .unwrap()
        );
        let parked = |held: &HeldChild| held.parks.parks.load(Ordering::SeqCst);

        // The parent is still in the turn that will receive the report.
        held.parent_view(Running { started_at_ms: 1 }, 1, Vec::new())
            .await;
        let (done, _) = held.settle(liveness(10, 1), &turn).await;
        assert!(matches!(
            done,
            ChildCompletion::ParkAfterParentTurn { ref parent } if parent == "parent"
        ));
        assert_eq!((held.told(), parked(&held)), (1, 0));

        // The parent's worker accepted the report but has not handed it to
        // the agent: the idle parent is about to run the turn that gets it.
        let key = crate::database::load_report_mail("parent", "child")
            .unwrap()
            .expect("the report is in the outbox")
            .key;
        crate::database::mark_mailbox_event_accepted(&key, "mailbox-command", 12).unwrap();
        held.parent_view(Idle, 12, vec![key.clone()]).await;
        let (done, _) = held.settle(liveness(10, 1), &turn).await;
        assert!(matches!(done, ChildCompletion::ParkAfterParentTurn { .. }));
        assert_eq!(parked(&held), 0);

        // Delivered, and the parent idle again: its turn has ended.
        held.parent_view(Idle, 13, Vec::new()).await;
        let (done, _) = held.settle(liveness(10, 1), &turn).await;
        assert!(matches!(done, ChildCompletion::Settled));
        assert_eq!(parked(&held), 1);
    }

    // Control: nothing alive, nothing to wait for; and a parent that is no
    // longer running cannot answer, so it holds nothing back.
    #[tokio::test]
    async fn a_child_without_live_processes_or_with_a_stopped_parent_is_parked_at_once() {
        if !isolated(
            "a_child_without_live_processes_or_with_a_stopped_parent_is_parked_at_once",
            "delegation-park-gate-open",
        ) {
            return;
        }
        use mj_core::state::MaterializedExecutionState::Running;
        let _writer = crate::database::install_isolated_test_writer();
        let mut held = HeldChild::new("park gate open").await;
        let turn = turn_at("turn", 10, 1, None);
        held.child_ended(&turn).await;
        assert!(
            crate::database::record_subagent_handback(
                "child",
                &mj_core::subagent::SubagentHandback {
                    command_id: "turn".into(),
                    message: "done".into(),
                    recorded_at_ms: 1,
                },
            )
            .unwrap()
        );
        held.parent_view(Running { started_at_ms: 1 }, 1, Vec::new())
            .await;
        let (done, _) = held.settle(liveness(10, 0), &turn).await;
        assert!(matches!(done, ChildCompletion::Settled));
        assert_eq!(held.parks.parks.load(Ordering::SeqCst), 1);

        held.parks.running.store(false, Ordering::SeqCst);
        let (done, _) = held.settle(liveness(10, 3), &turn).await;
        assert!(matches!(done, ChildCompletion::Settled));
        assert_eq!(held.parks.parks.load(Ordering::SeqCst), 2);
    }

    // The parent's answer to a report reaches the child's outbox before the
    // parent's turn ends. The park decision, serialized with the lifecycle,
    // sees it and leaves the worker running.
    #[tokio::test]
    async fn a_parent_message_in_the_outbox_prevents_the_park() {
        if !isolated(
            "a_parent_message_in_the_outbox_prevents_the_park",
            "delegation-park-outbox-message",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let held = HeldChild::new("outbox message").await;
        let event = mj_core::mailbox::MailboxEvent {
            key: "subagent-message-r1".into(),
            source: "mjolnir".into(),
            wake: true,
            created_at_ms: 1,
            body: mj_core::mailbox::MailboxEventBody::ParentMessage {
                text: "continue".into(),
            },
        };
        crate::database::enqueue_mailbox_event(
            &event.key,
            "child",
            &serde_json::to_string(&event).unwrap(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            held.state.park_subagent("child".into()).await.unwrap(),
            ParkOutcome::Busy
        );
    }
}
