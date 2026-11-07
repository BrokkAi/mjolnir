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
            for (child, observation) in &observations {
                if completion_ids.len() >= 32 {
                    break;
                }
                let Some(outcome) = observation.outcome.as_ref() else {
                    continue;
                };
                if noticed.get(child) == Some(&outcome.completed_ordinal)
                    || completion_ids.values().any(|(id, _)| id == child)
                    || retry.get(child).is_some_and(|retry| {
                        retry.ordinal == outcome.completed_ordinal && retry.at > Instant::now()
                    })
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
            completed = completions.join_next_with_id(), if !completions.is_empty() => {
                let Some(completed) = completed else { continue };
                let (task_id, result) = match completed {
                    Ok((id, result)) => (id, result),
                    Err(error) => (error.id(), Err(anyhow::Error::from(error))),
                };
                if let Some((id, ordinal)) = completion_ids.remove(&task_id) {
                    match result {
                        Ok(ChildCompletion::Settled) => { noticed.insert(id.clone(), ordinal); retry.remove(&id); }
                        Ok(ChildCompletion::RetryPark | ChildCompletion::AwaitingReview | ChildCompletion::Draining) => {
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
        observation.in_flight,
        outcome,
        review_open,
    )
    .await
}

async fn settle_child(
    state: &RuntimeState,
    backend: &ApiBackend,
    work: crate::upgrade::Work,
    relation: mj_core::subagent::SubagentRecord,
    in_flight: Vec<String>,
    outcome: mj_core::state::MaterializedTurnOutcome,
    review_open: bool,
) -> Result<ChildCompletion> {
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
    if relation.noticed_turn != Some(outcome.completed_ordinal) {
        let reminded = backend
            .remind_subagent_to_hand_back(&child_id, relation.handback_tool, &outcome, &in_flight)
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

    struct BusyThenParked(AtomicUsize);
    impl ExportRuntime for BusyThenParked {
        fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord> {
            (session_id == "parent").then(|| {
                crate::daemon::tests::runtime_test_session(
                    "parent",
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
        let state = crate::daemon::tests::test_runtime_state();
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(0)));
        // The parent has no actor. Durable report enqueue must not need one
        // or prevent either park attempt.
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            parks.clone(),
        ));
        state.install_report_backend(&backend);
        let observation = DelegationObservation {
            requests: Vec::new(),
            completed: Vec::new(),
            in_flight: Vec::new(),
            credential_signal: None,
            outcome: Some(mj_core::state::MaterializedTurnOutcome {
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
        let state = crate::daemon::tests::test_runtime_state();
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(1)));
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            parks.clone(),
        ));
        state.install_report_backend(&backend);
        let outcome = mj_core::state::MaterializedTurnOutcome {
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
        };
        let stored = crate::database::load_subagent("child").unwrap().unwrap();
        let settle = |review_open| {
            settle_child(
                &state,
                &backend,
                crate::upgrade::activity_unless_draining("test completion").unwrap(),
                stored.clone(),
                Vec::new(),
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
        // With the review gone, the same turn is judged as before.
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
            parent.review = Some(review);
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
                requests: Vec::new(),
                completed: Vec::new(),
                in_flight: Vec::new(),
                credential_signal: None,
                outcome: Some(mj_core::state::MaterializedTurnOutcome {
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
}
