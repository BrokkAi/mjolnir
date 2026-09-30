//! The daemon owns delegation; control surfaces only observe its effects.
use crate::daemon::RuntimeState;
use crate::server_runtime::api::ApiBackend;
use crate::session_manager::{DelegationObservation, DelegationUpdates, SessionManagerControl};
use anyhow::{Context, Result};
use mj_core::subagent::{SubagentToolAction, SubagentToolResult};
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

enum Completed {
    Executed(SubagentToolResult, bool),
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
    let mut revisions = state.revisions();
    let mut dispatch = SubagentDispatch::default();
    let mut jobs = tokio::task::JoinSet::new();
    let mut identities = BTreeMap::<tokio::task::Id, Identity>::new();
    let mut observations = BTreeMap::<String, DelegationObservation>::new();
    let mut completions = tokio::task::JoinSet::new();
    let mut completion_ids = BTreeMap::<tokio::task::Id, (String, u64)>::new();
    let mut noticed = BTreeMap::<String, u64>::new();
    let mut retry = BTreeMap::<String, Instant>::new();
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
                                SubagentToolAction::WaitAgents { .. }
                                    | SubagentToolAction::SendInput { .. }
                                    | SubagentToolAction::Spawn { .. }
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
                                tracing::warn!(parent_session_id = %parent, request_id = %result.request_id, %error, "delegation delivery failed; retaining result for retry");
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
                let Some(outcome) = observation.outcome.as_ref().filter(|_| observation.idle)
                else {
                    continue;
                };
                if noticed.get(child) == Some(&outcome.completed_ordinal)
                    || completion_ids.values().any(|(id, _)| id == child)
                    || retry.get(child).is_some_and(|at| *at > Instant::now())
                {
                    continue;
                }
                let Ok(work) =
                    crate::upgrade::activity_unless_draining("delegation child completion")
                else {
                    continue;
                };
                let child_id = child.clone();
                let backend = backend.clone();
                let state = state.clone();
                let observation = observation.clone();
                let task = completions.spawn(async move {
                    let _work = work;
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
                    // when `send_input` starts it again. That observer first
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
                                is_error = result.is_error, "delegation execution completed");
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
                        Ok(ChildCompletion::RetryPark) => { retry.insert(id, Instant::now() + Duration::from_secs(1)); }
                        Err(error) => {
                            tracing::warn!(child_session_id = %id, %error, "delegation child completion failed");
                            retry.insert(id, Instant::now() + Duration::from_secs(1));
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
            _ = tick.tick() => policy.tick(),
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
        .context("child completion has no outcome")?;
    if relation
        .noticed_turn
        .is_some_and(|turn| turn > outcome.completed_ordinal)
    {
        return Ok(ChildCompletion::Settled);
    }
    if relation.noticed_turn != Some(outcome.completed_ordinal) {
        let title = state.session_record(&child_id).map_or_else(
            || relation.task_name.clone(),
            |session| session.listed_title().to_owned(),
        );
        let reminded = backend
            .remind_subagent_to_hand_back(
                &child_id,
                relation.handback_tool,
                &outcome,
                &observation.in_flight,
            )
            .await?;
        if !reminded {
            backend
                .record_subagent_completion_notice(
                    relation.parent_session_id,
                    &child_id,
                    &title,
                    &outcome,
                )
                .await?;
        }
        let id = child_id.clone();
        tokio::task::spawn_blocking(move || {
            crate::database::mark_subagent_turn_noticed(&id, outcome.completed_ordinal)
        })
        .await??;
        if reminded {
            return Ok(ChildCompletion::Settled);
        }
    }
    // A persisted notice and a successful park are different facts. Only the
    // worker's reservation decides whether its processes can be stopped.
    if !observation.in_flight.is_empty() {
        return Ok(ChildCompletion::RetryPark);
    }
    match backend.park_subagent(&child_id).await? {
        crate::controller::ParkOutcome::Busy => Ok(ChildCompletion::RetryPark),
        crate::controller::ParkOutcome::Parked | crate::controller::ParkOutcome::NotRunning => {
            Ok(ChildCompletion::Settled)
        }
    }
}

async fn deliver(runtime: &RuntimeState, parent: &str, result: SubagentToolResult) -> Result<()> {
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
    result: SubagentToolResult,
) -> Result<()> {
    let handle = runtime.workspace_session_handle(parent).await?;
    let mut lease = handle.lease_connection().await?;
    let request_id = result.request_id.clone();
    let delivered = lease
        .connection_mut()
        .complete_subagent_request(result)
        .await;
    lease.release();
    delivered?;
    let parent = parent.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::database::acknowledge_delegation(parent, request_id)
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
        fn session_record(&self, _: &str) -> Option<mj_core::state::SessionRecord> {
            None
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

    #[tokio::test]
    async fn a_reported_child_retries_busy_parking_without_repeating_its_notice() {
        const CHILD: &str = "MJ_TEST_DELEGATION_PARK_RETRY";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_reported_child_retries_busy_parking_without_repeating_its_notice",
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
        let mut relation = crate::daemon::tests::runtime_test_subagent("child", "parent");
        relation.noticed_turn = Some(10);
        crate::database::save_subagent_session(&child, &relation).unwrap();
        let state = crate::daemon::tests::test_runtime_state();
        let parks = Arc::new(BusyThenParked(AtomicUsize::new(0)));
        // No parent handle exists: attempting to send its notice again fails.
        let backend = Arc::new(ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            parks.clone(),
        ));
        let observation = DelegationObservation {
            requests: Vec::new(),
            completed: Vec::new(),
            idle: true,
            in_flight: Vec::new(),
            credential_signal: None,
            outcome: Some(mj_core::state::MaterializedTurnOutcome {
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
            complete_child(state, backend, "child".into(), observation)
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
}
