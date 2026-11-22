//! Tests of `mj_controller::controller::move_session` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use mj_controller::controller::Controller;
use mj_controller::controller::checkpoint::stand_in::*;
use mj_controller::controller::move_session::test_fixtures::*;
use mj_worker::stand_in_relay::seed_source_relay;
use std::fs::{self};
use std::path::PathBuf;

use mj_controller::controller::move_session::MoveSourceRelay;
use mj_controller::controller::test_support::{
    IsolatedTest, RefusingExecutor, checkpoint_test_session, resume_compatibility_config,
};
use mj_core::config::{Config, HarnessKind};
use mj_core::state::{MovePhase, ResumeQueueDisposition};

use mj_core::state::{SessionResourceAllocation, SessionState, State};

use mj_controller::targets::ProcessExecutor;
use mj_core::relay::{RelayCommand, RelayRequest};
use mj_worker::relay::DurableRelay;

// Hard-won: #1538: a retried Move stored the destination choice as the source settings
#[test]
fn move_outcome_publishes_safe_recovery_and_clears_it_after_a_successful_retry() {
    let name =
        test_name("move_outcome_publishes_safe_recovery_and_clears_it_after_a_successful_retry");
    if !isolated_test_child(&name, "MJ_MOVE_OUTCOME_CHILD") {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let session = checkpoint_test_session(MOVE_QUEUE_SESSION_ID);
    mj_controller::database::save_session(&session).unwrap();
    let mut controller = Controller {
        config: Config::default(),
        state: State::default(),
    };
    controller
        .state
        .sessions
        .insert(session.id.clone(), session.clone());
    let mut operation = source_recovery_operation(&session);
    let applied_size = Some(SessionResourceAllocation::Container {
        cpus: 2,
        memory_bytes: 4 << 30,
    });
    let requested_size = Some(SessionResourceAllocation::Container {
        cpus: 4,
        memory_bytes: 8 << 30,
    });
    let applied_mounts = vec![mj_controller::targets::AdditionalMount {
        source: PathBuf::from("/source"),
        destination: PathBuf::from("/source"),
        access: mj_controller::targets::MountAccess::Ro,
    }];
    let requested_mounts = vec![mj_controller::targets::AdditionalMount {
        source: PathBuf::from("/destination"),
        destination: PathBuf::from("/destination"),
        access: mj_controller::targets::MountAccess::Rw,
    }];
    let source = controller.state.sessions.get_mut(&session.id).unwrap();
    std::sync::Arc::make_mut(source.target_runtime.get_or_insert_with(|| {
        std::sync::Arc::new((&resume_compatibility_config().targets["local-bare"]).into())
    }))
    .container_settings = Some(mj_core::state::ContainerSettings {
        resource_allocation: applied_size.clone(),
        additional_mounts: applied_mounts.clone(),
        default_image: None,
    });
    operation.selection.resource_allocation = requested_size.clone();
    operation.selection.additional_mounts = Some(requested_mounts.clone());
    for _ in 0..2 {
        operation.capture_container_settings(&controller.state.sessions[&session.id]);
        mj_controller::database::accept_move_operation(&operation).unwrap();
        controller
            .state
            .sessions
            .get_mut(&session.id)
            .unwrap()
            .resource_allocation = requested_size.clone();
        controller
            .state
            .sessions
            .get_mut(&session.id)
            .unwrap()
            .additional_mounts = requested_mounts.clone();
        controller
            .finish_move_result(
                &mut operation,
                Err(anyhow::anyhow!("destination failed")),
                &RefusingExecutor("no command"),
            )
            .unwrap();
        let recovery =
            mj_controller::controller::move_session::ViewerMoveRecovery::from_operation(&operation)
                .unwrap();
        assert_eq!(recovery.source_resource_allocation, applied_size);
        assert_eq!(recovery.source_additional_mounts, applied_mounts);
    }
    operation.selection.resource_allocation = None;
    operation.selection.additional_mounts = None;
    operation.capture_container_settings(&controller.state.sessions[&session.id]);
    assert_eq!(operation.inherited_resource_allocation, requested_size);
    assert_eq!(operation.inherited_additional_mounts, requested_mounts);
    controller
        .state
        .sessions
        .get_mut(&session.id)
        .unwrap()
        .resource_allocation = None;
    controller
        .state
        .sessions
        .get_mut(&session.id)
        .unwrap()
        .additional_mounts
        .clear();
    let recovery =
        mj_controller::controller::move_session::ViewerMoveRecovery::from_operation(&operation)
            .unwrap();
    assert_eq!(recovery.destination_resource_allocation, requested_size);
    assert_eq!(recovery.destination_additional_mounts, requested_mounts);
    operation.phase = MovePhase::Capturing;
    let outcome = controller
        .finish_move_result(
            &mut operation,
            Err(anyhow::anyhow!("private-token at /private/worker")),
            &RefusingExecutor("Move outcome must not execute a command"),
        )
        .unwrap();
    assert_eq!(outcome.outcome, "failed");
    assert_eq!(operation.error, outcome.error);
    let reopened = mj_controller::database::load_state().unwrap();
    let record = &reopened.sessions[&session.id];
    assert_eq!(record.state, SessionState::Running);
    let snapshot =
        crate::server::ViewerSnapshot::from_config_state(&controller.config, &reopened, 1);
    let public = crate::server::api::ApiSession::from(&snapshot.sessions[0]);
    assert!(public.has_error);
    let message = public.error.unwrap();
    assert!(message.contains("capturing the source snapshot"));
    assert!(message.contains(&operation.operation_id));
    assert!(message.contains(outcome.recovery.as_deref().unwrap()));
    assert!(!message.contains("private-token"));
    assert!(!message.contains("/private/worker"));
    assert_eq!(
        controller.state.sessions[&session.id].last_error,
        record.last_error
    );
    controller
        .finish_move_result(
            &mut operation,
            Ok(()),
            &RefusingExecutor("Move outcome must not execute a command"),
        )
        .unwrap();
    let reopened = mj_controller::database::load_state().unwrap();
    assert_eq!(reopened.sessions[&session.id].last_error, None);
    assert_eq!(
        mj_controller::database::load_move_operation(&session.id)
            .unwrap()
            .unwrap()
            .error,
        None
    );

    // Completing a Move must not clear another operation's error.
    controller
        .state
        .sessions
        .get_mut(&session.id)
        .unwrap()
        .last_error = Some("another failure".into());
    controller
        .finish_move_result(
            &mut operation,
            Ok(()),
            &RefusingExecutor("Move outcome must not execute a command"),
        )
        .unwrap();
    assert_eq!(
        mj_controller::database::load_state().unwrap().sessions[&session.id]
            .last_error
            .as_deref(),
        Some("another failure")
    );
}

// Hard-won: #1498: idle active goals blocked quota Move at preparation and source lease.
#[test]
fn quota_move_preserves_idle_active_goal_at_both_admission_gates() {
    let name = test_name("quota_move_preserves_idle_active_goal_at_both_admission_gates");
    if std::env::var_os("MJ_QUOTA_ACTIVE_ADMISSION_CHILD").is_none() {
        for race in ["idle", "running", "completed", "barrier"] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(name.clone())
                .env("MJ_QUOTA_ACTIVE_RACE", race)
                .env(
                    "MJ_TEST_QUOTA_BARRIER_RACE",
                    if race == "barrier" { "1" } else { "0" },
                )
                .env("MJ_QUOTA_ACTIVE_ADMISSION_CHILD", "1")
                .env(
                    mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_JOURNALS_BEFORE_CLOSE,
                    mj_controller::controller::checkpoint::stand_in::LATE_HARNESS_NOTIFICATION,
                )
                .env(
                    mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_MAX_PROTOCOL,
                    mj_core::relay::RELAY_IN_PLACE_MOVE_PROTOCOL.to_string(),
                )
                .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
                .isolated_store(directory.path())
                .run();
        }
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let native = fixture
        .worker_root
        .join("profile/projects/fixture/native-session.jsonl");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(
        &native,
        b"{\"type\":\"assistant\",\"message\":{\"content\":\"native history\"}}\n",
    )
    .unwrap();
    fixture.controller.config.save().unwrap();
    let source_relay = seed_source_relay(&fixture.worker_root);
    let mut relay = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    relay
        .record_session_update(
            serde_json::from_value(serde_json::json!({
                "sessionUpdate":"session_info_update", "_meta": {
                    "goal":{"objective":"Finish work", "status":"active", "createdAt":42},
                    "execution":{"version":1,"status":"idle"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
    seed_quota_recovery(&mut relay, "quota-user-turn");
    drop(relay);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, std::time::Duration::from_secs(5))
            .await
            .unwrap()
            .sync_now()
            .await
            .unwrap();
        let mut controller = fixture.controller;
        let state = crate::daemon::RuntimeState::move_test_runtime(
            Controller {
                config: controller.config.clone(),
                state: controller.state.clone(),
            },
            channels.control.clone(),
        );
        // A failed pre-stop Move owns no recovery state. A historical session
        // checkpoint must not force a new profile selection through transfer.
        let mut failed = in_place_operation(&controller, IN_PLACE_SOURCE_PROFILE);
        failed.phase = MovePhase::Failed;
        failed.error = Some("failed before stopping source".into());
        mj_controller::database::save_move_operation(&failed).unwrap();
        let selection = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE).selection;
        let manual = state.prepare_move_session(selection).await.unwrap();
        assert!(
            !manual.active,
            "an idle goal alone does not require interruption"
        );
        assert!(manual.in_place);
        assert!(manual.route_preview.is_none());
        assert!(!manual.stops_subagents);
        let request = state
            .prepare_quota_move(LATCH_RELAY_SESSION, IN_PLACE_DESTINATION_PROFILE)
            .await
            .unwrap();
        assert!(!request.preparation.active);
        assert!(!request.acknowledge_interruption);
        channels.shutdown.shutdown().await.unwrap();
        let race = std::env::var("MJ_QUOTA_ACTIVE_RACE").unwrap();
        if race == "running" || race == "completed" {
            let mut relay =
                DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
            if race == "completed" {
                seed_quota_recovery(&mut relay, "new-user-turn");
            } else {
                relay
                    .record_observation(mj_core::relay::RelayObservation::HarnessTurnStarted {
                        started_at_ms: mj_core::clock::epoch_millis(),
                    })
                    .unwrap();
            }
        }
        let channels = start_source_relay_manager(&source_relay);
        channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, std::time::Duration::from_secs(5))
            .await
            .unwrap()
            .sync_now()
            .await
            .unwrap();
        let executor = recording_executor();
        let admitted = std::sync::atomic::AtomicBool::new(false);
        let outcome = controller
            .move_session_managed_controlled_with_admission(
                &mj_controller::controller::test_support::session_admission(
                    request.preparation.selection.session_id.as_str(),
                ),
                &mj_controller::profile_catalog::ProfileCatalog::new(
                    tokio_util::sync::CancellationToken::new(),
                ),
                request,
                Some((
                    "quota stopped".into(),
                    mj_core::state::QuotaMoveContinuation::Prompt,
                )),
                &executor,
                &channels.control,
                || {
                    let durable = mj_controller::database::load_move_operation(LATCH_RELAY_SESSION)
                        .unwrap()
                        .unwrap();
                    assert!(
                        durable.accepted_preparation.is_some(),
                        "acceptance preceded the committed intent"
                    );
                    assert_eq!(
                        durable.selection.profile_id.as_deref(),
                        Some(IN_PLACE_DESTINATION_PROFILE)
                    );
                    assert_eq!(
                        executor.move_stop_calls(),
                        0,
                        "source interruption preceded acceptance"
                    );
                    assert!(!admitted.swap(true, std::sync::atomic::Ordering::SeqCst));
                },
            )
            .await;
        if race == "idle" {
            assert!(admitted.load(std::sync::atomic::Ordering::SeqCst));
        }
        if matches!(race.as_str(), "running" | "completed") {
            assert!(!admitted.load(std::sync::atomic::Ordering::SeqCst));
        }
        channels.shutdown.shutdown().await.unwrap();
        if race != "idle" {
            let error = match outcome {
                Err(error) => format!("{error:#}"),
                Ok(outcome) => {
                    assert_eq!(outcome.outcome, "failed");
                    outcome.error.unwrap()
                }
            };
            let expected = match race.as_str() {
                "running" => "superseded",
                "completed" => "changed",
                "barrier" => "working",
                _ => unreachable!(),
            };
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            if race != "barrier" {
                assert_eq!(executor.move_stop_calls(), 0);
            }
            return;
        }
        let outcome = outcome.unwrap();
        assert_eq!(outcome.outcome, "completed", "{outcome:?}");
        assert_eq!(
            controller.state.sessions[LATCH_RELAY_SESSION].last_profile,
            IN_PLACE_DESTINATION_PROFILE
        );
        assert_eq!(
            fs::read(fixture.worker_root.join("build-cache.txt")).unwrap(),
            b"warm cache"
        );
        assert!(
            fs::read_to_string(&native)
                .unwrap()
                .contains("native history")
        );
        let moved = mj_controller::database::load_move_operation(LATCH_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert!(moved.in_place);
        assert!(moved.transfer_route.is_none());
        assert!(moved.restore_artifact().is_none());
        assert!(moved.in_place_harness_restarted);
        let live_relay = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
        let live_state = live_relay.operational_state();
        assert!(live_state.in_place_move_hold.is_none());
        assert!(live_state.goal.active());
        assert!(
            fs::read_to_string(fixture.worker_root.join("profile/settings.json"))
                .unwrap()
                .contains("\"destination\"")
        );
    });
}

/// A Native source has no parent tools, but its worker still has a request
/// queue (`ask_oracle`, `request_review`), so an in-place Move closes it first.
// Hard-won: #1470: the child-stop hook could stop daemon-side children after the quota check.
// Hard-won: dd56c0341: foreground Move blocked the result delivery its mutation drain needed.
// Hard-won: 05b7a7f89: review holds blocked replies and legacy wait retirement discarded delivery state.
// Hard-won: #1562: a Move drain held handoff admission while waiting on paused delivery.
#[test]
fn in_place_move_drains_the_queue_of_a_native_source() {
    let name = test_name("in_place_move_drains_the_queue_of_a_native_source");
    if std::env::var_os("MJ_MOVE_NATIVE_DRAIN_CHILD").is_none() {
        for mode in ["complete", "cancel", "handoff"] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(name.clone())
                .env("MJ_MOVE_NATIVE_DRAIN_MODE", mode)
                .env("MJ_MOVE_NATIVE_DRAIN_CHILD", "1")
                .env(
                    mj_controller::controller::checkpoint::stand_in::LATCH_RECORD_SUBAGENT_ADMISSION,
                    "1",
                )
                .env(
                    mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_JOURNALS_BEFORE_CLOSE,
                    mj_controller::controller::checkpoint::stand_in::LATE_HARNESS_NOTIFICATION,
                )
                .env(
                    mj_controller::controller::checkpoint::stand_in::LATCH_RELAY_MAX_PROTOCOL,
                    mj_core::relay::RELAY_IN_PLACE_MOVE_PROTOCOL.to_string(),
                )
                .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
                .isolated_store(directory.path())
                .run();
        }
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let InPlaceFixture {
        _directory,
        worker_root,
        mut controller,
        ..
    } = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let source = controller
        .state
        .sessions
        .get_mut(LATCH_RELAY_SESSION)
        .unwrap();
    source.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    mj_controller::database::save_session(source).unwrap();
    let mut operation = in_place_operation(&controller, IN_PLACE_DESTINATION_PROFILE);
    operation.quota_stop_reason = Some("weekly quota exhausted".into());
    operation.quota_continuation = Some(mj_core::state::QuotaMoveContinuation::Prompt);
    operation.queue = ResumeQueueDisposition::Start;
    // A live quota-stopped source has native history and pending recovery.
    let native = worker_root.join("profile/projects/fixture/native-session.jsonl");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(&native, b"{\"type\":\"assistant\"}\n").unwrap();
    // The automatic Move must belong to a pending, quota-stopped turn.
    let source_relay = seed_source_relay(&worker_root);
    let mut relay = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    seed_quota_recovery(&mut relay, "native-source-quota-turn");
    drop(relay);
    // An accepted mutation stays pending at the worker while its effect runs.
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "native-mutation".into(),
        created_at_ms: 1,
        action: mj_core::subagent::SubagentToolAction::SendMessage {
            child_session_id: "already-accepted-child".into(),
            message: "finish before Move".into(),
        },
    };
    mj_controller::database::prepare_delegation(
        LATCH_RELAY_SESSION.into(),
        mj_controller::database::PreparedDelegation {
            request: request.clone(),
            turn_target: None,
            spawn: None,
        },
    )
    .unwrap();
    let queue_path = worker_root.join("subagents.json");
    mj_core::config::atomic_write(&queue_path, &serde_json::to_vec(&serde_json::json!({
        "requests": {request.request_id.clone(): request}, "results": {}, "mutating_admission_open": true,
    })).unwrap()).unwrap();
    let review_request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "review-acceptance".into(),
        created_at_ms: 2,
        action: mj_core::subagent::SubagentToolAction::RequestReview { paths: Vec::new() },
    };
    let legacy_request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "legacy-acceptance".into(),
        created_at_ms: 3,
        action: mj_core::subagent::SubagentToolAction::LegacyWaitAgents,
    };
    let mut queue: serde_json::Value =
        serde_json::from_slice(&fs::read(&queue_path).unwrap()).unwrap();
    for request in [&review_request, &legacy_request] {
        mj_controller::database::prepare_delegation(
            LATCH_RELAY_SESSION.into(),
            mj_controller::database::PreparedDelegation {
                request: request.clone(),
                turn_target: None,
                spawn: None,
            },
        )
        .unwrap();
        queue["requests"].as_object_mut().unwrap().insert(
            request.request_id.clone(),
            serde_json::to_value(request).unwrap(),
        );
        mj_controller::database::record_delegation_result(
            LATCH_RELAY_SESSION.into(),
            mj_core::subagent::SubagentToolResult {
                request_id: request.request_id.clone(),
                completed_at_ms: 4,
                is_error: false,
                message: if request.request_id == "legacy-acceptance" {
                    "old wait answer"
                } else {
                    "review registered"
                }
                .into(),
            },
        )
        .unwrap();
    }
    mj_core::config::atomic_write(&queue_path, &serde_json::to_vec(&queue).unwrap()).unwrap();
    let review_result =
        mj_controller::database::load_delegation(LATCH_RELAY_SESSION, &review_request.request_id)
            .unwrap()
            .unwrap()
            .1
            .unwrap();
    let legacy_result = mj_controller::database::retire_legacy_wait(
        LATCH_RELAY_SESSION.into(),
        legacy_request.request_id,
    )
    .unwrap()
    .unwrap();
    let wait = std::sync::Arc::new(tokio::sync::Notify::new());
    let mode = std::env::var("MJ_MOVE_NATIVE_DRAIN_MODE").unwrap();
    let handoff_gate =
        (mode == "handoff").then(|| std::sync::Arc::new(mj_controller::upgrade::Gate::default()));
    let handoff_work = handoff_gate
        .as_ref()
        .map(|gate| gate.enter("session Move").unwrap());
    let executor = RecordingProcessExecutor {
        handoff_gate: handoff_gate.clone(),
        handoff_work: std::sync::Mutex::new(handoff_work),
        drain_wait: Some(wait.clone()),
        ..recording_executor()
    };
    let stored = mj_controller::database::StoredDelegationResult {
        result: mj_core::subagent::SubagentToolResult {
            request_id: "native-mutation".into(),
            completed_at_ms: 2,
            is_error: false,
            message: "accepted mutation finished".into(),
        },
        reported_finishes: Vec::new(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let channels = start_source_relay_manager(&source_relay);
        let handle = channels
            .control
            .wait_for_session(LATCH_RELAY_SESSION, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        let gate = std::sync::Arc::new(mj_controller::recovery_gate::RecoveryGate::default());
        // A worker-owned review can outlive a foreground Move; its compatible
        // reservation must not exclude the completion work Move waits for.
        let review = gate.reserve_when_idle(LATCH_RELAY_SESSION).await;
        let acceptance = gate.try_start(LATCH_RELAY_SESSION, mj_controller::recovery_gate::BackgroundWork::DelegationDelivery)
            .expect("review acceptance replies cannot wait for the review to finish");
        for result in [review_result, legacy_result] {
            crate::daemon::delegation::deliver(&handle, &acceptance, result, async {}).await.unwrap();
        }
        drop(acceptance);
        let delivered: serde_json::Value = serde_json::from_slice(&fs::read(&queue_path).unwrap()).unwrap();
        assert_eq!(delivered["results"]["review-acceptance"]["message"], "review registered");
        let compatibility: serde_json::Value = serde_json::from_str(delivered["results"]["legacy-acceptance"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(compatibility["status"], "reports_arrive_as_messages");
        assert!(mj_controller::database::load_delegation(LATCH_RELAY_SESSION, "legacy-acceptance").unwrap().is_none());
        let admission = mj_controller::recovery_gate::SessionAdmission::Foreground(
            gate.enter_foreground(
                LATCH_RELAY_SESSION,
                "Move", mj_controller::recovery_gate::LifecycleOrigin::Person,
                &std::sync::atomic::AtomicBool::new(false),
                &ProcessExecutor,
            )
            .await
            .unwrap(),
        );
        let mut moving = Box::pin(controller.execute_move(
            &admission,
            &mut operation,
            None,
            &executor,
            &channels.control,
            MoveSourceRelay::default(),
        ));
        tokio::select! {
            result = &mut moving => panic!("Move finished before its mutation: {result:?}"),
            () = wait.notified() => {}
        }
        let delivery = gate.try_start(LATCH_RELAY_SESSION,
            mj_controller::recovery_gate::BackgroundWork::DelegationDelivery).expect("the owner admits required completion work during drain");
        assert!(gate.try_start(LATCH_RELAY_SESSION, mj_controller::recovery_gate::BackgroundWork::WorkerReplacement).is_err());
        let delivery_lease = handle.lease_connection(&delivery).await.unwrap();
        let deferred = handle.sync_now().await.unwrap_err();
        assert!(mj_controller::session_manager::RelayJobDeferred::marks(&deferred), "{deferred:#}");
        // The actual drain must wait for this known lease deferral rather
        // than treating it as an unavailable worker or a failed Move.
        tokio::select! {
            result = &mut moving => panic!("Move failed during a delivery lease: {result:?}"),
            () = wait.notified() => {}
        }
        let queue: serde_json::Value =
            serde_json::from_slice(&fs::read(&queue_path).unwrap()).unwrap();
        assert_eq!(queue["mutating_admission_open"], false, "{queue}");
        assert!(queue["requests"]["native-mutation"].is_object());
        if mode != "complete" {
            if let Some(gate) = &handoff_gate {
                assert!(gate.try_close(), "handoff must close while the Move awaits paused delivery");
                executor.shutdown.store(true, std::sync::atomic::Ordering::Release);
            }
            executor.cancelled.store(true, std::sync::atomic::Ordering::Release);
            tokio::select! {
                () = mj_controller::recovery_gate::wait_until_cancelled(&delivery) => {},
                result = &mut moving => panic!("Move returned before the admitted delivery released ownership: {result:?}"),
            }
            assert!(futures::poll!(&mut moving).is_pending());
            drop(delivery_lease);
        } else {
            delivery_lease.release();
            let result = stored.clone();
            mj_core::runtime::spawn_blocking(move || {
                mj_controller::database::record_stored_delegation_result(LATCH_RELAY_SESSION.into(), result)
            }).await.unwrap().unwrap();
            crate::daemon::delegation::deliver(&handle, &delivery, stored.clone(), async {}).await.unwrap();
            // A cached retry of the acknowledged result issues no second RPC.
            crate::daemon::delegation::deliver(&handle, &delivery, stored.clone(), async {}).await.unwrap();
        }
        drop(delivery);
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), &mut moving)
            .await
            .expect("Move drain must complete or cancel while dispatch keeps delivery ownership");
        drop(moving);
        if mode != "complete" {
            let error = result.unwrap_err();
            if mode == "handoff" {
                let outcome = controller.finish_move_result(&mut operation, Err(error), &executor).unwrap();
                assert_eq!(outcome.outcome, "interrupted");
                let durable = mj_controller::database::load_move_operation(LATCH_RELAY_SESSION).unwrap().unwrap();
                assert_eq!(durable.phase, MovePhase::Capturing);
                assert!(!durable.cancellation_requested, "handoff must not become a user cancellation");
            } else {
                assert!(format!("{error:#}").contains("Move cancelled"), "{error:#}");
            }
            assert!(
                mj_controller::database::load_delegation(LATCH_RELAY_SESSION, "native-mutation")
                    .unwrap()
                    .unwrap()
                    .1
                    .is_none()
            );
            assert_eq!(fs::read_to_string(queue_path.with_extension("deliveries")).unwrap(), "review-acceptance\nlegacy-acceptance\n");
        } else {
            result.unwrap();
            assert!(
                mj_controller::database::load_delegation(LATCH_RELAY_SESSION, "native-mutation")
                    .unwrap()
                    .is_none(),
                "the admitted background delivery must acknowledge the durable result"
            );
            assert_eq!(
                fs::read_to_string(queue_path.with_extension("deliveries")).unwrap(),
                "review-acceptance\nlegacy-acceptance\nnative-mutation\n"
            );
        }
        channels.shutdown.shutdown().await.unwrap();
        drop(admission);
        drop(review);
        if mode == "handoff" {
            // A fresh manager and owner model the next daemon. It reads the
            // operation from storage, delivers the durable result, then resumes.
            let channels = start_source_relay_manager(&source_relay);
            let handle = channels.control.wait_for_session(LATCH_RELAY_SESSION, std::time::Duration::from_secs(10)).await.unwrap();
            let fresh = std::sync::Arc::new(mj_controller::recovery_gate::RecoveryGate::default());
            mj_controller::database::record_stored_delegation_result(LATCH_RELAY_SESSION.into(), stored.clone()).unwrap();
            let delivery = fresh.try_start(LATCH_RELAY_SESSION, mj_controller::recovery_gate::BackgroundWork::DelegationDelivery).unwrap();
            crate::daemon::delegation::deliver(&handle, &delivery, stored.clone(), async {}).await.unwrap();
            drop(delivery);
            let restored = mj_controller::database::load_move_operation(LATCH_RELAY_SESSION).unwrap().unwrap();
            let outcome = controller.recover_move_managed_controlled(
                &mj_controller::controller::test_support::session_admission(LATCH_RELAY_SESSION),
                restored, &recording_executor(), &channels.control,
            ).await.unwrap();
            assert_eq!(outcome.outcome, "completed", "{outcome:?}");
            channels.shutdown.shutdown().await.unwrap();
        }
        assert!(gate.try_start(LATCH_RELAY_SESSION, mj_controller::recovery_gate::BackgroundWork::DelegationDelivery).is_ok(),
            "completion and cancellation must both release the foreground phase");
    });
    assert_eq!(
        executor.move_stop_calls(),
        0,
        "an automatic Move must never invoke the hook that re-reads and stops children"
    );

    let queue: serde_json::Value =
        serde_json::from_slice(&fs::read(worker_root.join("subagents.json")).unwrap()).unwrap();
    let transitions = queue["admission_transitions"].as_array().unwrap();
    assert_eq!(
        &transitions[transitions.len() - 2..],
        &[
            serde_json::Value::Bool(false),
            serde_json::Value::Bool(true)
        ],
        "Move must close mutation admission and reopen it after completion or cancellation: {queue}"
    );
    assert_eq!(queue["mutating_admission_open"], true, "{queue}");
    if mode == "cancel" {
        assert!(queue["requests"]["native-mutation"].is_object(), "{queue}");
    } else {
        assert!(queue["requests"].as_object().unwrap().is_empty(), "{queue}");
        assert_eq!(
            queue["results"]["native-mutation"]["message"], "accepted mutation finished",
            "{queue}"
        );
    }
}

#[test]
fn in_place_move_releases_dispatch_after_post_hold_error() {
    if !isolate_in_place_move_dispatch_test("in_place_move_releases_dispatch_after_post_hold_error")
    {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let source_relay = seed_source_relay(&fixture.worker_root);
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    // An incomplete quota Move record reaches the queue step after taking
    // the hold. Its missing continuation action makes that step fail.
    operation.in_place_harness_restarted = true;
    operation.quota_stop_reason = Some("weekly quota exhausted".into());
    operation.queue = ResumeQueueDisposition::Start;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let channels = start_source_relay_manager(&source_relay);
            let error = fixture
                .controller
                .execute_move(
                    &mj_controller::controller::test_support::session_admission(
                        LATCH_RELAY_SESSION,
                    ),
                    &mut operation,
                    None,
                    &recording_executor(),
                    &channels.control,
                    MoveSourceRelay::default(),
                )
                .await
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("quota Move has no saved continuation action"),
                "{error:#}"
            );
            channels.shutdown.shutdown().await.unwrap();
        });
    assert_eq!(
        fs::read_to_string(source_relay.join("in-place-move-requests.log")).unwrap(),
        "hold_in_place_move_dispatch\nrelease_in_place_move_dispatch\n"
    );
    let relay = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    assert!(relay.operational_state().in_place_move_hold.is_none());
}

#[test]
fn in_place_move_recovery_at_dispatch_released_only_releases() {
    if !isolate_in_place_move_dispatch_test(
        "in_place_move_recovery_at_dispatch_released_only_releases",
    ) {
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let mut fixture = in_place_fixture(HarnessKind::Claude, HarnessKind::Claude);
    let source_relay = seed_source_relay(&fixture.worker_root);
    let mut operation = in_place_operation(&fixture.controller, IN_PLACE_DESTINATION_PROFILE);
    operation.quota_stop_reason = Some("weekly quota exhausted".into());
    operation.quota_continuation = Some(mj_core::state::QuotaMoveContinuation::Prompt);
    operation.in_place_harness_restarted = true;
    operation.queue = ResumeQueueDisposition::Start;
    let mut relay = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    let store_id = relay.operational_state().store_id.unwrap();
    let response = relay.handle(mj_core::relay::RelayRequestEnvelope {
        request_id: "seed-dispatch-hold".into(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: RelayRequest::HoldInPlaceMoveDispatch {
            operation_id: operation.operation_id.clone(),
            expected_store_id: store_id.clone(),
        },
    });
    assert!(
        matches!(response.body, mj_core::relay::RelayResponseBody::Ok { .. }),
        "{response:?}"
    );
    assert!(relay.operational_state().in_place_move_hold.is_some());
    drop(relay);
    operation.in_place_expected_store_id = Some(store_id);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // Attach while the Capturing operation owns the hold, so reconnect
            // cleanup cannot release it before the recovery branch is exercised.
            mj_controller::database::save_move_operation(&operation).unwrap();
            let channels = start_source_relay_manager(&source_relay);
            channels
                .control
                .wait_for_session(LATCH_RELAY_SESSION, std::time::Duration::from_secs(5))
                .await
                .unwrap()
                .sync_now()
                .await
                .unwrap();
            operation.phase = MovePhase::DispatchReleased;
            operation.queue_admission_started = true;
            // Recreate the same durable operation for both sides of the crash
            // boundary: before release and after release but before completion.
            for _ in 0..2 {
                mj_controller::database::save_move_operation(&operation).unwrap();
                let stored = mj_controller::database::load_move_operation(LATCH_RELAY_SESSION)
                    .unwrap()
                    .unwrap();
                let outcome = fixture
                    .controller
                    .recover_move_managed_controlled(
                        &mj_controller::controller::test_support::session_admission(
                            LATCH_RELAY_SESSION,
                        ),
                        stored,
                        &recording_executor(),
                        &channels.control,
                    )
                    .await
                    .unwrap();
                assert_eq!(outcome.outcome, "completed", "{outcome:?}");
                let settled = mj_controller::database::load_move_operation(LATCH_RELAY_SESSION)
                    .unwrap()
                    .unwrap();
                assert_eq!(settled.phase, MovePhase::Completed);
                assert!(settled.queue_admission_finished);
            }
            channels.shutdown.shutdown().await.unwrap();
        });
    assert_eq!(
        fs::read_to_string(source_relay.join("in-place-move-requests.log")).unwrap(),
        "release_in_place_move_dispatch\nrelease_in_place_move_dispatch\n"
    );
    let relay = DurableRelay::open(&source_relay, LATCH_RELAY_SESSION, "1.0.0").unwrap();
    assert!(relay.operational_state().in_place_move_hold.is_none());
}

fn isolate_in_place_move_dispatch_test(name: &str) -> bool {
    if std::env::var_os("MJ_TEST_RECORD_IN_PLACE_MOVE_REQUESTS").is_some() {
        return true;
    }
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(test_name(name))
        .env("MJ_TEST_RECORD_IN_PLACE_MOVE_REQUESTS", "1")
        .env(
            LATCH_RELAY_MAX_PROTOCOL,
            mj_core::relay::RELAY_IN_PLACE_MOVE_PROTOCOL.to_string(),
        )
        .env("MJ_WORKER_BINARY", fake_worker_dispatcher())
        .isolated_store(directory.path())
        .run();
    false
}

fn seed_quota_recovery(relay: &mut DurableRelay, id: &str) {
    fn submit(relay: &mut DurableRelay, id: &str, command: RelayCommand) {
        let response = relay.handle(mj_core::relay::RelayRequestEnvelope {
            request_id: id.into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Submit {
                command_id: id.into(),
                command,
            },
        });
        assert!(
            matches!(response.body, mj_core::relay::RelayResponseBody::Ok { .. }),
            "{response:?}"
        );
    }
    submit(
        relay,
        id,
        RelayCommand::Prompt {
            prompt: mj_core::continuation::prompt_blocks(),
        },
    );
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(
            id,
            mj_core::relay::RelayCommandOutcome::Prompt {
                work_facts: None,
                diagnostic: None,
                stop_reason: "quota_limit".into(),
                usage: None,
            },
        )
        .unwrap();
    if let Some(mut assessment) = relay.operational_state().assessment.as_deref().cloned() {
        assessment.action = Some(mj_core::assessment::Action::RecoverQuota);
        assessment.status = mj_core::assessment::Status::Deferred;
        relay
            .record_observation(mj_core::relay::RelayObservation::TurnAssessmentUpdated {
                assessment: Box::new(assessment),
            })
            .unwrap();
    }
    let operational = relay.operational_state();
    submit(
        relay,
        &format!("schedule-quota-{id}"),
        RelayCommand::SetQuotaRecovery {
            expected: mj_core::relay::RelayCursor {
                ordinal: operational.latest_ordinal,
                digest: operational.latest_digest,
            },
            recovery: Some(Box::new(mj_core::continuation::QuotaRecovery {
                user_command_id: id.into(),
                completed_command_id: id.into(),
                profile_id: IN_PLACE_SOURCE_PROFILE.into(),
                reset_at_ms: None,
                retry_at_ms: None,
                notice: "quota stopped".into(),
                submitted: false,
            })),
        },
    );
}

/// The executor the move tests use, running mj-worker's checkpoint operations.
fn recording_executor() -> RecordingProcessExecutor {
    RecordingProcessExecutor::new(WorkerCheckpoint {
        restore: mj_worker::checkpoint::restore_from_spec_file,
        export: |mut input| {
            let checkpoint = mj_worker::checkpoint::export_from_spec_reader(&mut input)?;
            Ok(serde_json::to_vec(&checkpoint)?)
        },
    })
}

fn test_name(short: &str) -> String {
    mj_controller::controller::test_support::test_name(module_path!(), short)
}
