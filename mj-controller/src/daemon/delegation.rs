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
}

/// First wait after a child completion fails or finds the child busy.
const COMPLETION_RETRY_BASE: Duration = Duration::from_secs(1);
/// A completion that keeps failing is tried this rarely (#1456): reviewer
/// completion reaches the worker restart queue, which a flat 1 s retry kept full.
const COMPLETION_FAILURE_CAP: Duration = Duration::from_secs(300);
/// A busy child is polled more often: it is working, not failing.
const COMPLETION_BUSY_CAP: Duration = Duration::from_secs(30);

/// The most completions that run at once. Reviewers, oracles and workers share
/// it, so nothing that waits on a worker may hold one of its slots.
const COMPLETION_POOL: usize = 32;

#[derive(Debug, PartialEq, Eq)]
enum CompletionStep {
    /// Ask the child's worker what it left, in a task of its own.
    CollectFacts(String, u64),
    /// Report the finish and park: runs in the bounded pool, briefly.
    Complete(String, u64),
}

/// What to start next for the finished children, each with the turn that
/// completed. A child whose facts for that turn are not stored yet collects
/// them first, with no limit on how many do so at once; only a child whose
/// facts are stored takes a pool slot, so children waiting on their workers can
/// never occupy the slots other completions need. `skip` says which children
/// are already settled, running or waiting out a retry.
fn completion_steps(
    finished: &[(String, u64)],
    skip: impl Fn(&str, u64) -> bool,
    facts_done: &BTreeMap<String, u64>,
    running_completions: usize,
) -> Vec<CompletionStep> {
    let mut steps = Vec::new();
    let mut free = COMPLETION_POOL.saturating_sub(running_completions);
    for (child, ordinal) in finished {
        if skip(child, *ordinal) {
            continue;
        }
        if facts_done.get(child) != Some(ordinal) {
            steps.push(CompletionStep::CollectFacts(child.clone(), *ordinal));
        } else if free > 0 {
            free -= 1;
            steps.push(CompletionStep::Complete(child.clone(), *ordinal));
        }
    }
    steps
}

/// What becomes of a child's working-tree read when its task ends. One that
/// failed or stopped (a panic included) must not leave the tree pending for
/// ever: it is stored as unavailable, with why, and the parent is told by the
/// follow-up that stores owe.
async fn settle_tree_read(child: &str, ordinal: u64, result: Result<()>) {
    let Err(error) = result else {
        return;
    };
    let reason = format!("the working-tree read stopped: {error:#}");
    tracing::warn!(child_session_id = %child, error = %reason, "could not record a finished child's working tree");
    let stored = tokio::task::spawn_blocking({
        let child = child.to_owned();
        move || {
            crate::database::record_finish_worktree(
                child,
                ordinal,
                mj_core::work_facts::Observed::unavailable(reason),
            )
        }
    })
    .await;
    match stored {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(child_session_id = %child, error = format!("{error:#}"), "could not store the unavailable working tree of a finished child");
        }
        Err(error) => {
            tracing::warn!(child_session_id = %child, %error, "storing the unavailable working tree of a finished child stopped");
        }
    }
}

/// When one child outcome's completion may run again, and how it has been
/// going. Settling the child forgets it; a later outcome starts afresh.
struct CompletionRetry {
    ordinal: u64,
    at: Instant,
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
                at: now + delay,
                failures,
                last_failure,
            },
            report,
        )
    }
}

enum Completed {
    Executed(StoredDelegationResult, bool),
    Delivered,
    Unaccepted,
}

pub(crate) fn spawn(
    state: Arc<RuntimeState>,
    manager: SessionManagerControl,
    updates: DelegationUpdates,
    cancellation: CancellationToken,
) -> (Services, tokio::task::JoinHandle<Result<()>>) {
    let policy = policy::Policy::new(state.clone(), &manager, &cancellation);
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
    // What each finished child's worker observed is collected by a task of its
    // own per child, outside the bounded completion pool: a child waiting on its
    // worker (an idle checkpoint, a slow Git read) must not hold one of the
    // completion slots every other child, reviewer and oracle needs. The
    // completion that follows a child's facts runs in the pool, briefly.
    let mut fact_tasks = tokio::task::JoinSet::new();
    let mut fact_ids = BTreeMap::<tokio::task::Id, (String, u64)>::new();
    let mut facts_done = BTreeMap::<String, u64>::new();
    // The working tree, which has no fixed duration, is read after the report
    // can be built, by another task of the same child, and nothing waits for it.
    let mut tree_tasks = tokio::task::JoinSet::new();
    let mut tree_ids = BTreeMap::<tokio::task::Id, (String, u64)>::new();
    let mut noticed = BTreeMap::<String, u64>::new();
    let mut retry = BTreeMap::<String, CompletionRetry>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut quota_open = true;
    loop {
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
            let finished = observations
                .iter()
                .filter_map(|(child, observation)| {
                    Some((
                        child.clone(),
                        observation.outcome.as_ref()?.completed_ordinal,
                    ))
                })
                .collect::<Vec<_>>();
            let steps = completion_steps(
                &finished,
                |child, ordinal| {
                    noticed.get(child) == Some(&ordinal)
                        || completion_ids.values().any(|(id, _)| id == child)
                        || fact_ids.values().any(|(id, _)| id == child)
                        || retry.get(child).is_some_and(|retry| {
                            retry.ordinal == ordinal && retry.at > Instant::now()
                        })
                },
                &facts_done,
                completion_ids.len(),
            );
            for step in steps {
                match step {
                    // In this child's own task and with no admission: a
                    // replaced daemon asks again before any report exists.
                    CompletionStep::CollectFacts(child, ordinal) => {
                        let state = state.clone();
                        let child_id = child.clone();
                        let task = fact_tasks.spawn(async move {
                            state.collect_finish_facts_once(&child_id, ordinal).await
                        });
                        fact_ids.insert(task.id(), (child, ordinal));
                    }
                    CompletionStep::Complete(child, ordinal) => {
                        let Ok(work) =
                            crate::upgrade::activity_unless_draining("delegation child completion")
                        else {
                            continue;
                        };
                        let Some(observation) = observations.get(&child).cloned() else {
                            continue;
                        };
                        let child_id = child.clone();
                        let backend = backend.clone();
                        let state = state.clone();
                        let task = completions.spawn(async move {
                            complete_child(state, backend, child_id, observation, work).await
                        });
                        completion_ids.insert(task.id(), (child, ordinal));
                    }
                }
            }
        }
        tokio::select! {
            _ = cancellation.cancelled() => break,
            update = updates.recv() => {
                let Some((id, observation)) = update else { anyhow::bail!("delegation observation feed stopped") };
                if let Some(observation) = observation {
                    policy.observe(&id, &observation);
                    dispatch.observe(&id, &observation.requests);
                    observations.insert(id, observation);
                } else {
                    dispatch.retire(&id);
                    observations.remove(&id);
                    retry.remove(&id);
                    facts_done.remove(&id);
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
                        Err(error) => {
                            tracing::warn!(parent_session_id = %id.0, request_id = %id.1, %error, "delegation task failed");
                            dispatch.failed_task(&id);
                        }
                    }
                }
            }
            completed = fact_tasks.join_next_with_id(), if !fact_tasks.is_empty() => {
                let Some(completed) = completed else { continue };
                let (task_id, result) = match completed {
                    Ok((id, result)) => (id, result),
                    Err(error) => (error.id(), Err(anyhow::Error::from(error))),
                };
                if let Some((id, ordinal)) = fact_ids.remove(&task_id) {
                    match result {
                        Ok(()) => {
                            let state = state.clone();
                            let child_id = id.clone();
                            let task = tree_tasks.spawn(async move {
                                state.collect_finish_worktree(&child_id, ordinal).await
                            });
                            tree_ids.insert(task.id(), (id.clone(), ordinal));
                            facts_done.insert(id, ordinal);
                        }
                        Err(error) => {
                            let failure = format!("{error:#}");
                            let (next, report) = CompletionRetry::schedule(retry.remove(&id), ordinal, Instant::now(), Some(failure.clone()));
                            if report {
                                tracing::warn!(child_session_id = %id, error = %failure, retry_in_seconds = (next.at - Instant::now()).as_secs(), "collecting a finished child's facts failed; retrying with backoff");
                            }
                            retry.insert(id, next);
                        }
                    }
                }
            }
            completed = tree_tasks.join_next_with_id(), if !tree_tasks.is_empty() => {
                let Some(completed) = completed else { continue };
                let (task_id, result) = match completed {
                    Ok((id, result)) => (id, result),
                    Err(error) => (error.id(), Err(anyhow::Error::from(error))),
                };
                if let Some((id, ordinal)) = tree_ids.remove(&task_id) {
                    settle_tree_read(&id, ordinal, result).await;
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
                        Ok(ChildCompletion::Settled) => { noticed.insert(id.clone(), ordinal); retry.remove(&id); }
                        Ok(ChildCompletion::RetryPark) => {
                            let (next, _) = CompletionRetry::schedule(retry.remove(&id), ordinal, Instant::now(), None);
                            retry.insert(id, next);
                        }
                        Err(error) => {
                            let failure = format!("{error:#}");
                            let (next, report) = CompletionRetry::schedule(retry.remove(&id), ordinal, Instant::now(), Some(failure.clone()));
                            if report {
                                tracing::warn!(child_session_id = %id, error = %failure, retry_in_seconds = (next.at - Instant::now()).as_secs(), "delegation child completion failed; retrying with backoff");
                            }
                            retry.insert(id, next);
                        }
                    }
                }
            }
            changed = revisions.changed() => { changed.context("daemon revision feed stopped")?; policy.sync(false); }
            _ = policy.refresh_rx.recv() => policy.sync(true),
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
    fact_tasks.abort_all();
    tree_tasks.abort_all();
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

async fn complete_child(
    state: Arc<RuntimeState>,
    backend: Arc<ApiBackend>,
    child_id: String,
    observation: DelegationObservation,
    work: crate::upgrade::Work,
) -> Result<ChildCompletion> {
    let id = child_id.clone();
    let relation =
        tokio::task::spawn_blocking(move || crate::database::load_subagent(&id)).await??;
    let Some(relation) = relation else {
        return Ok(ChildCompletion::Settled);
    };
    let outcome = observation
        .outcome
        .context("child completion has no outcome")?;
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
    if relation.noticed_turn != Some(outcome.completed_ordinal) {
        let reminded = backend
            .remind_subagent_to_hand_back(
                &child_id,
                relation.handback_tool,
                &outcome,
                &observation.in_flight,
            )
            .await?;
        let id = child_id.clone();
        tokio::task::spawn_blocking(move || {
            crate::database::mark_subagent_turn_noticed(&id, outcome.completed_ordinal)
        })
        .await??;
        if reminded {
            return Ok(ChildCompletion::Settled);
        }
    }
    let _work = work;
    // Park only once the parent has been told, or when this ending is not news.
    // The report and the marker that covers this ending are written together,
    // so the marker, read after the reconcile, says which it is. A finish that
    // is news but not yet covered is asked again: the readers' snapshot may not
    // have caught up with the observation (the session actor runs ahead of it),
    // or the report is held for its facts. The retry backs off and ends when the
    // report is enqueued; no path parks a child whose parent has not heard of it.
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
    // The park proceeds once the report is out, and does not wait for the working
    // tree: that read has no fixed duration and can still be running. The park
    // stops the worker that runs it, and the tree is then stored as unavailable,
    // with why, and delivered so.
    match backend.park_subagent(&child_id).await? {
        crate::controller::ParkOutcome::Busy => Ok(ChildCompletion::RetryPark),
        crate::controller::ParkOutcome::Parked => {
            let id = child_id.clone();
            let ordinal = outcome.completed_ordinal;
            tokio::task::spawn_blocking(move || {
                crate::database::record_finish_worktree(
                    id,
                    ordinal,
                    mj_core::work_facts::Observed::unavailable(
                        "the child was parked before its working tree could be read",
                    ),
                )
            })
            .await??;
            Ok(ChildCompletion::Settled)
        }
        crate::controller::ParkOutcome::NotRunning => Ok(ChildCompletion::Settled),
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
        };
        let mut conversation = mj_core::state::MaterializedSession::empty(child);
        conversation.applied_event_ordinal = completed_ordinal;
        conversation.applied_event_digest = format!("{completed_ordinal:064x}");
        conversation.last_turn_outcome = Some(turn.clone());
        crate::database::save_materialized_session(&conversation).unwrap();
        turn
    }

    /// The park proceeds with a working-tree read still in flight (a hung
    /// `git status` must never keep a worker, harness and slot alive), and the
    /// tree is stored as unavailable, with why, and delivered so.
    #[tokio::test]
    async fn a_park_does_not_wait_for_a_stalled_tree_read_and_stores_it_unavailable() {
        const CHILD: &str = "MJ_TEST_DELEGATION_PARK_NO_HOLD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_park_does_not_wait_for_a_stalled_tree_read_and_stores_it_unavailable",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-park-no-hold")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("park no hold").unwrap();
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
        // The finish's facts are stored with the tree still being read, and the
        // report went out that way (the follow-up is owed).
        let mut facts = mj_core::work_facts::WorkFacts::unavailable("not needed");
        facts.worktree = mj_core::work_facts::Observed::Pending;
        crate::database::record_finish_facts("child".into(), 10, facts.clone()).unwrap();
        let manager = crate::daemon::tests::TestRemoteManager::for_session("child").await;
        let state = crate::daemon::tests::test_runtime_state_with_manager(&manager);
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(1)));
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
            requests: Vec::new(),
            completed: Vec::new(),
            in_flight: Vec::new(),
            credential_signal: None,
            outcome: Some(turn),
        };
        assert!(matches!(
            complete_child(
                state.clone(),
                backend,
                "child".into(),
                observation,
                crate::upgrade::activity_unless_draining("test completion").unwrap(),
            )
            .await
            .unwrap(),
            ChildCompletion::Settled
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 2, "the park waited");
        let recorded = crate::database::load_work_facts(&["child".to_owned()]).unwrap();
        assert!(matches!(
            &recorded["child"].finish.as_ref().unwrap().worktree,
            mj_core::work_facts::Observed::Unavailable { reason }
                if reason.contains("the child was parked before its working tree could be read")
        ));
    }

    /// The worker owns acceptance: a scan whose answer says the worker had
    /// accepted newer work makes the scan unfit for that finish (it may show the
    /// new work's processes), whatever the store and the actor's view say yet.
    /// It is stored as unavailable, with that reason; an answer that does not say
    /// is an unknown, which is unavailable too; and with no newer work the scan
    /// stands.
    #[tokio::test]
    async fn processes_the_worker_says_newer_work_may_have_changed_are_not_the_finishs() {
        const CHILD: &str = "MJ_TEST_DELEGATION_PROCESS_SCAN_RACE";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "processes_the_worker_says_newer_work_may_have_changed_are_not_the_finishs",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-process-scan-race")
            .isolated_store(root.path())
            .run();
            return;
        }
        use mj_core::work_facts::{Observed, ProcessFacts, ProcessIdentity, RunningProcess};
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("process scan race").unwrap();
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
        let scan = |accepted_ordinal: Option<u64>| async move {
            crate::daemon::views::FinishScan {
                facts: mj_core::work_facts::WorkFacts {
                    observed_at_ms: 1,
                    processes: Observed::Known(ProcessFacts {
                        unattributed: 0,
                        running: 1,
                        shown: vec![RunningProcess {
                            pid: 41301,
                            elapsed_secs: 5,
                            command: "cargo build".into(),
                            detached: false,
                        }],
                        omitted: 0,
                        identities: vec![ProcessIdentity {
                            pid: 41301,
                            start: "boot:1".into(),
                        }],
                    }),
                    worktree: Observed::Pending,
                },
                accepted_ordinal,
            }
        };
        let state = crate::daemon::tests::test_runtime_state();
        let stored = || {
            crate::database::load_work_facts(&["child".to_owned()]).unwrap()["child"]
                .finish
                .clone()
                .unwrap()
                .processes
        };
        // Nothing newer was accepted (the newest work is the finish's own turn):
        // the scan stands.
        state
            .collect_finish_facts_from("child", 9, scan(Some(8)))
            .await
            .unwrap();
        assert!(matches!(stored(), Observed::Known(_)));

        // The worker accepted a prompt after the finish, while the store and the
        // actor's view know nothing of it yet.
        state
            .collect_finish_facts_from("child", 10, scan(Some(12)))
            .await
            .unwrap();
        assert!(
            matches!(
                &stored(),
                Observed::Unavailable { reason }
                    if reason.contains("started new work before its processes were read")
            ),
            "{:?}",
            stored()
        );

        // A worker that does not say is an unknown.
        state
            .collect_finish_facts_from("child", 11, scan(None))
            .await
            .unwrap();
        assert!(
            matches!(
                &stored(),
                Observed::Unavailable { reason }
                    if reason.contains("did not say whether the child had accepted new work")
            ),
            "{:?}",
            stored()
        );
    }

    // A completion is decided from the readers' snapshot, which refreshes after
    // the write that makes a turn durable returns, while the observation comes
    // from the session actor and can run ahead of it. Behind the observation the
    // snapshot says nothing yet: no report, no park, and the completion asks
    // again. Once it has caught up the report is still held until the finish's
    // facts are collected, and the child is parked only after the report is
    // enqueued, once.
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
            requests: Vec::new(),
            completed: Vec::new(),
            in_flight: Vec::new(),
            credential_signal: None,
            outcome: Some(newer),
        };
        let complete = || {
            complete_child(
                state.clone(),
                backend.clone(),
                "child".into(),
                observation.clone(),
                crate::upgrade::activity("test completion").unwrap(),
            )
        };
        let told = || crate::database::pending_mailbox_events(10).unwrap().len();
        assert!(matches!(
            complete().await.unwrap(),
            ChildCompletion::RetryPark
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 1, "no park while behind");
        assert_eq!(told(), 0, "nothing is reported from a snapshot behind");

        // The snapshot catches up, but the finish's facts are not collected:
        // the report is held, and so is the park.
        store_finished_turn("child", "turn", 10);
        assert!(matches!(
            complete().await.unwrap(),
            ChildCompletion::RetryPark
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 1, "no park while held");
        assert_eq!(told(), 0, "no report before its facts");

        // The facts are stored: the report is enqueued, and then the child is
        // parked.
        crate::database::record_finish_facts(
            "child".into(),
            10,
            mj_core::work_facts::WorkFacts::unavailable("not needed here"),
        )
        .unwrap();
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

    #[tokio::test]
    async fn a_tree_task_that_panics_stores_the_tree_as_unavailable() {
        const CHILD: &str = "MJ_TEST_DELEGATION_TREE_PANIC";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_tree_task_that_panics_stores_the_tree_as_unavailable",
            ))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "delegation-tree-panic")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("tree panic").unwrap();
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
        let mut facts = mj_core::work_facts::WorkFacts::unavailable("not needed");
        facts.worktree = mj_core::work_facts::Observed::Pending;
        crate::database::record_finish_facts("child".into(), 10, facts).unwrap();
        // The task panics, as the loop's JoinSet reports it.
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async { panic!("the read blew up") });
        let (_, result) = match tasks.join_next_with_id().await.unwrap() {
            Ok((id, ())) => (id, Ok(())),
            Err(error) => (error.id(), Err(anyhow::Error::from(error))),
        };
        settle_tree_read("child", 10, result).await;
        let recorded = crate::database::load_work_facts(&["child".to_owned()]).unwrap();
        assert!(matches!(
            &recorded["child"].finish.as_ref().unwrap().worktree,
            mj_core::work_facts::Observed::Unavailable { reason }
                if reason.contains("the working-tree read stopped")
        ));
    }

    #[test]
    fn children_waiting_for_their_facts_never_occupy_the_completion_pool() {
        let finished = (0..100)
            .map(|index| (format!("child-{index:03}"), 7))
            .collect::<Vec<_>>();
        // Nobody's facts are stored: every child starts collecting, none takes
        // a completion slot, however many there are.
        let steps = completion_steps(&finished, |_, _| false, &BTreeMap::new(), 0);
        assert_eq!(steps.len(), 100);
        assert!(
            steps
                .iter()
                .all(|step| matches!(step, CompletionStep::CollectFacts(_, 7)))
        );
        // Of children whose facts are stored, the pool still limits how many
        // complete at once, and the ones still collecting do not count in it.
        let facts_done = finished
            .iter()
            .skip(10)
            .map(|(child, ordinal)| (child.clone(), *ordinal))
            .collect::<BTreeMap<_, _>>();
        let steps = completion_steps(&finished, |_, _| false, &facts_done, 4);
        let completing = steps
            .iter()
            .filter(|step| matches!(step, CompletionStep::Complete(..)))
            .count();
        let collecting = steps
            .iter()
            .filter(|step| matches!(step, CompletionStep::CollectFacts(..)))
            .count();
        assert_eq!((completing, collecting), (COMPLETION_POOL - 4, 10));
        // A child that is settled, running or waiting is left alone.
        let steps = completion_steps(&finished, |child, _| child != "child-050", &facts_done, 0);
        assert_eq!(steps, [CompletionStep::Complete("child-050".into(), 7)]);
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
        // The finish's facts are stored before its completion runs, as the
        // child's own task does.
        crate::database::record_finish_facts(
            "child".into(),
            10,
            mj_core::work_facts::WorkFacts::unavailable("not needed here"),
        )
        .unwrap();
        // The readers' snapshot holds the turn the observation reports.
        let turn = store_finished_turn("child", "turn", 10);
        publish_idle_child(&manager, &turn).await;
        let observation = DelegationObservation {
            requests: Vec::new(),
            completed: Vec::new(),
            in_flight: Vec::new(),
            credential_signal: None,
            outcome: Some(turn),
        };
        assert!(matches!(
            complete_child(
                state.clone(),
                backend.clone(),
                "child".into(),
                observation.clone(),
                crate::upgrade::activity_unless_draining("test completion").unwrap(),
            )
            .await
            .unwrap(),
            ChildCompletion::RetryPark
        ));
        assert!(matches!(
            complete_child(
                state.clone(),
                backend,
                "child".into(),
                observation,
                crate::upgrade::activity_unless_draining("test completion").unwrap(),
            )
            .await
            .unwrap(),
            ChildCompletion::Settled
        ));
        assert_eq!(parks.0.load(Ordering::SeqCst), 2);
        // The child's own facts task asks once for this finish (this child has
        // no worker, so they are stored as unknown, with why), and asking again
        // for the same finish changes nothing.
        state.collect_finish_facts("child", 11).await.unwrap();
        state.collect_finish_facts("child", 11).await.unwrap();
        let recorded = crate::database::load_work_facts(&["child".to_owned()]).unwrap();
        let recorded = &recorded["child"];
        assert_eq!(recorded.finish_ordinal, Some(11));
        assert!(matches!(
            &recorded.finish.as_ref().unwrap().processes,
            mj_core::work_facts::Observed::Unavailable { reason }
                if reason.contains("worker") || reason.contains("session")
        ));
        assert_eq!(
            crate::database::load_subagent("child")
                .unwrap()
                .unwrap()
                .noticed_turn,
            Some(10)
        );
    }
}
