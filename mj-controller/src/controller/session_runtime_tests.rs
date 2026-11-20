//! Tests of the session runtime together with the controller above it: a
//! workspace lease during reconnect, model policies from a controller's
//! records, and harness failures a controller loads back.

// These tests need Unix, so off Unix their imports and fixtures go unused.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{mpsc, oneshot, watch};

use agent_client_protocol::v1::schema::ContentBlock;
use mj_core::relay::{RelayCommand, RelayOperationalState};

use crate::session_manager::stand_in::*;
use crate::session_manager::test_internals::*;
use crate::session_manager::*;
use crate::targets::CommandSpec;
use crate::test_isolation::{IsolatedTest, test_name};

fn exact_test_name(test: &str) -> String {
    test_name(module_path!(), test)
}

/// MJ_DATA_DIR is process-global, so every test that reaches the
/// controller database runs in an exact child with its own data directory.
fn run_in_isolated_child(marker: &str, test: &str) {
    let directory = tempfile::tempdir().unwrap();
    IsolatedTest::new(exact_test_name(test))
        .env(marker, "1")
        .env("MJ_DATA_DIR", directory.path())
        .env("MJ_CONFIG_DIR", directory.path().join("config"))
        .run();
}

fn fail_if_the_actor_stalls(reason: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        eprintln!("{reason}");
        std::process::exit(101);
    });
}

// Hard-won: c2107d5c: admission repair hid failed startup and prevented automatic worker replacement.
#[cfg(unix)]
#[tokio::test]
async fn reconnect_retains_failed_preparation_for_worker_replacement() {
    const CHILD: &str = "MJ_TEST_FAILED_PREPARATION_REPLACEMENT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(exact_test_name(
            "reconnect_retains_failed_preparation_for_worker_replacement",
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "failed-preparation-replacement")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    mj_core::config::Config::update(|config| {
        config.github.watch.enabled = true;
        Ok(())
    })
    .unwrap();
    register_leased_relay_session();
    let relay_directory = tempfile::tempdir().unwrap();
    let relay_root = relay_directory.path().join(LEASED_RELAY_SESSION);
    std::fs::create_dir(&relay_root).unwrap();
    let durable_target = mj_core::state::TargetLocator::LocalBare {
        worker_root: relay_root.clone(),
    };
    let mut session = crate::database::load_session_record(LEASED_RELAY_SESSION)
        .unwrap()
        .unwrap();
    session.harness_kind = mj_core::config::HarnessKind::Claude;
    session.subagents = Some(mj_core::subagent::SubagentPolicy::Native);
    session.target = Some(Box::new(durable_target.clone()));
    crate::database::save_session(&session).unwrap();

    let failure = mj_core::relay::HarnessPreparation::Failed {
        step: "harness-profile".into(),
        error: "missing staged Claude delegation server".into(),
        at_ms: 1,
    };
    let preparation_path = relay_root.join("preparation.json");
    std::fs::write(&preparation_path, serde_json::to_vec(&failure).unwrap()).unwrap();
    let mut target = leased_relay_target(&relay_root);
    target.worker_recovery = Some(WorkerRecoveryPlan {
        source_target: durable_target.clone(),
        target: None,
        workspace: None,
        exit_record: None,
        liveness_probe: CommandSpec::new("printf", ["alive\n"]).into(),
        binary_refresh: None,
        launch_refresh: None,
        restart: crate::targets::CommandPlan {
            description: "failed preparation is not a dead transport".into(),
            commands: Vec::new(),
        }
        .into(),
    });
    let owner = crate::database::test_fixtures::worker_test_permit(
        LEASED_RELAY_SESSION,
        "test failed replacement",
    )
    .unwrap()
    .unwrap();
    owner
        .scope_blocking(|| {
            owner.begin_restart(&durable_target, String::new())?;
            crate::database::advance_worker_restart(
                LEASED_RELAY_SESSION,
                owner.operation_id(),
                crate::database::WorkerRestartPhase::Swapping,
            )?;
            crate::database::advance_worker_restart(
                LEASED_RELAY_SESSION,
                owner.operation_id(),
                crate::database::WorkerRestartPhase::AwaitingReadiness,
            )
        })
        .unwrap();
    let mut connection = None;
    let gate = crate::database::test_fixtures::worker_test_gate();
    // Hard-won: 583e6ea48: an actor reported ordinary unpark startup as a vanished running worker.
    std::fs::write(
        relay_root.join("worker-unreachable"),
        b"boot has not bound control yet",
    )
    .unwrap();
    let error = sync_actor_connection(&gate, &target, &mut connection)
        .await
        .unwrap_err();
    assert!(error.is::<RelayJobDeferred>(), "{error:#}");
    assert!(connection.is_none());
    std::fs::remove_file(relay_root.join("worker-unreachable")).unwrap();
    let snapshot = sync_actor_connection(&gate, &target, &mut connection)
        .await
        .expect("failed startup must remain observable for automatic replacement")
        .unwrap();
    assert_eq!(
        snapshot.operational.harness_preparation,
        Some(failure.clone())
    );
    assert!(!relay_root.join("subagent-admission.json").exists());
    assert!(
        crate::database::load_worker_restart(LEASED_RELAY_SESSION)
            .unwrap()
            .is_some(),
        "an active replacement owner retains its intent"
    );
    drop(owner);
    // Hard-won: 8443db6c9: a recovery copy holding the session deferred an unreachable worker's failure as if it owned the boot.
    {
        let copy = gate
            .try_start(
                LEASED_RELAY_SESSION,
                crate::recovery_gate::BackgroundWork::DelegationDelivery,
            )
            .unwrap();
        std::fs::write(
            relay_root.join("worker-unreachable"),
            b"worker vanished during the copy",
        )
        .unwrap();
        let mut fresh = None;
        let error = sync_actor_connection(&gate, &target, &mut fresh)
            .await
            .unwrap_err();
        assert!(!error.is::<RelayJobDeferred>(), "{error:#}");
        std::fs::remove_file(relay_root.join("worker-unreachable")).unwrap();
        drop(copy);
    }
    sync_actor_connection(
        &crate::database::test_fixtures::worker_test_gate(),
        &target,
        &mut connection,
    )
    .await
    .unwrap();
    assert!(
        crate::database::load_worker_restart(LEASED_RELAY_SESSION)
            .unwrap()
            .is_none(),
        "terminal startup failure settles an abandoned replacement intent"
    );
    // Old workers refuse the ordinary idle reservation after failed startup.
    assert!(
        !connection
            .as_mut()
            .unwrap()
            .reserve_idle("ordinary-idle-reservation".into())
            .await
            .unwrap()
    );
    drop(connection);

    let manager = spawn_session_manager().unwrap();
    manager.targets.send_replace(vec![target]);
    let handle = manager
        .control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    handle.sync_now().await.unwrap();
    assert!(handle.view().connected);
    handle
        .enqueue_submit(
            "durable-prompt-after-failure".into(),
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::from("work that must survive replacement")],
            },
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let shell_path = relay_root.join("active-user-shell.json");
    std::fs::write(
        &shell_path,
        serde_json::to_vec(&mj_core::relay::ActiveUserShell {
            command_id: "live-user-shell".into(),
            command: "sleep 60".into(),
            created_at_ms: 1,
            started_at_ms: Some(1),
        })
        .unwrap(),
    )
    .unwrap();
    handle.sync_now().await.unwrap();
    assert!(
        crate::controller::IdleWorkspaceLease::acquire_for_upgrade(
            &crate::database::test_fixtures::session_admission(handle.session_id()),
            &handle,
            mj_core::config::HarnessKind::Claude,
        )
        .await
        .unwrap()
        .is_none(),
        "failed startup must not authorize stopping a live user shell"
    );
    std::fs::remove_file(shell_path).unwrap();
    let mut lease = crate::controller::IdleWorkspaceLease::acquire_for_upgrade(
        &crate::database::test_fixtures::session_admission(handle.session_id()),
        &handle,
        mj_core::config::HarnessKind::Claude,
    )
    .await
    .unwrap()
    .expect("terminal startup failure admits replacement under a checkpoint barrier");
    lease.verify().await.unwrap();
    lease.release().await.unwrap();
    handle.sync_now().await.unwrap();
    let operational = handle.view().snapshot.unwrap().operational;
    assert_eq!(operational.harness_preparation, Some(failure));
    assert!(
        operational
            .active_prompt
            .iter()
            .map(|prompt| prompt.command_id.as_str())
            .chain(
                operational
                    .queued_prompts
                    .iter()
                    .map(|prompt| prompt.command_id.as_str()),
            )
            .any(|id| id == "durable-prompt-after-failure")
    );

    std::fs::write(
        preparation_path,
        serde_json::to_vec(&mj_core::relay::HarnessPreparation::Preparing {
            step: "harness-profile".into(),
            since_ms: 1,
        })
        .unwrap(),
    )
    .unwrap();
    handle.sync_now().await.unwrap();
    assert!(
        crate::controller::IdleWorkspaceLease::acquire_for_upgrade(
            &crate::database::test_fixtures::session_admission(handle.session_id()),
            &handle,
            mj_core::config::HarnessKind::Claude,
        )
        .await
        .unwrap()
        .is_none(),
        "preparation still in progress must not be stopped"
    );
    manager.shutdown.shutdown().await.unwrap();
}

// Hard-won: a2afafcb3: live model selectors bypassed the profile allowlist.
#[cfg(unix)]
#[tokio::test]
async fn model_policy_refilters_the_same_actor_and_preserves_raw_observations() {
    const CHILD: &str = "MJ_TEST_MODEL_POLICY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "model_policy_refilters_the_same_actor_and_preserves_raw_observations",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    register_leased_relay_session();
    let directory = tempfile::tempdir().unwrap();
    let mut target = leased_relay_target(directory.path());
    target
        .spec
        .env
        .insert(RELAY_SESSION_CONFIGURED.into(), "1".into());
    target
        .spec
        .env
        .insert("MJ_TEST_MODEL_POLICY".into(), "1".into());
    let mut manager = spawn_session_manager().unwrap();
    let policy = |models: &[&str]| {
        BTreeMap::from([(
            LEASED_RELAY_SESSION.into(),
            SessionModelPolicy {
                profile: "gateway".into(),
                offer: mj_core::config::ModelOffer {
                    models: models.iter().map(|model| (*model).into()).collect(),
                    ..Default::default()
                },
            },
        )])
    };
    manager
        .control
        .model_policies
        .send_replace(policy(&["allowed"]));
    manager.targets.send_replace(vec![target]);
    let handle = manager
        .control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    let update = next_policy_update(&mut manager.updates).await;
    let values = |state: &RelayOperationalState| {
        mj_core::acp::session_config_choices(&state.config_options, "model")
            .into_iter()
            .map(|choice| choice.value)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        values(&update.view.snapshot.unwrap().operational),
        ["current", "allowed"]
    );
    // Hard-won: 193e758af: error republication replaced raw catalogue options.
    let mut disconnected = handle.view();
    disconnected.connected = false;
    disconnected.error = Some(ViewError::Unreachable("test disconnect".into()));
    let (watch, _) = watch::channel(handle.view());
    let (sender, mut receiver) = coalesced_update_channel();
    publish_view(LEASED_RELAY_SESSION, disconnected, &watch, &sender);
    assert!(receiver.try_recv().unwrap().raw_observation.is_none());
    let raw = update.raw_observation.unwrap();
    assert_eq!(values(&raw), ["current", "allowed", "hidden"]);
    mj_core::config::Config::update(|config| {
        config.profiles.insert(
            "gateway".into(),
            mj_core::config::HarnessProfile {
                models: vec!["allowed".into()],
                legacy_subagents: None,
                ..mj_core::config::HarnessProfile::new(
                    mj_core::config::HarnessKind::Codex,
                    directory.path().to_path_buf(),
                )
            },
        );
        Ok(())
    })
    .unwrap();
    assert!(
        crate::controller::profile_config::observe(
            "gateway".into(),
            String::new(),
            raw,
            crate::controller::profile_config::ObservationCheck::NoProbe
        )
        .await
        .unwrap()
    );
    let cached = crate::controller::profile_config::discover("gateway".into(), None, false)
        .await
        .unwrap();
    assert_eq!(
        cached
            .models
            .iter()
            .map(|choice| choice.value.as_str())
            .collect::<Vec<_>>(),
        ["current", "allowed", "hidden"]
    );
    let error = handle
        .enqueue_submit(
            "hidden-model".into(),
            RelayCommand::SetConfig {
                key: "model".into(),
                value: "hidden".into(),
            },
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("gateway") && error.to_string().contains("models setting"),
        "{error:?}"
    );
    let error = handle
        .enqueue_submit(
            "hidden-model-alias".into(),
            RelayCommand::SetConfig {
                key: "provider-model".into(),
                value: "hidden".into(),
            },
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("models setting"));
    let mut controller = crate::controller::Controller {
        config: mj_core::config::Config::load().unwrap(),
        state: crate::database::load_state().unwrap(),
    };
    controller
        .state
        .sessions
        .get_mut(LEASED_RELAY_SESSION)
        .unwrap()
        .last_profile = "gateway".into();
    let profile = controller.config.profiles.get_mut("gateway").unwrap();
    profile.models = vec!["hidden".into()];
    // Disabling a profile for new sessions must not remove its live-session policy.
    profile.enabled = false;
    manager
        .control
        .update_model_policies(&controller.config, &controller.state);
    // A sync already under way may still publish what it saw before the
    // edit; the edit must reach the next views, not necessarily the first.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let update = next_policy_update(&mut manager.updates).await;
            if values(&update.view.snapshot.unwrap().operational) == ["current", "hidden"] {
                break;
            }
        }
    })
    .await
    .expect("the policy edit refilters the live session's models");
    let same = manager.control.session(LEASED_RELAY_SESSION).await.unwrap();
    assert!(
        handle.commands.same_channel(&same.commands),
        "policy edits must retain the actor"
    );
    assert!(!handle.is_stopped());
    assert_eq!(
        values(&handle.view().snapshot.unwrap().operational),
        ["current", "hidden"]
    );
    manager.shutdown.shutdown().await.unwrap();
}

#[cfg(unix)]
// Hard-won: 0353d626b review: a daemon handoff lost worker-reported startup failures.
#[tokio::test]
async fn harness_failures_persist_legacy_expiry_and_worker_reports() {
    const CHILD: &str = "MJ_TEST_REPORTED_HARNESS_FAILURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        run_in_isolated_child(
            CHILD,
            "harness_failures_persist_legacy_expiry_and_worker_reports",
        );
        return;
    }
    let _writer = crate::database::install_isolated_test_writer();
    fail_if_the_actor_stalls("serialized readiness failure stalled");
    for worker_deadline in [false, true] {
        register_leased_relay_session();
        let relay_root = tempfile::tempdir().unwrap();
        if worker_deadline {
            let failure = mj_core::relay::HarnessPreparation::Failed {
                step: "session-open".into(),
                error:
                    "the harness did not open its session within 300s; use Restart agent to retry"
                        .into(),
                at_ms: 1,
            };
            std::fs::write(
                relay_root.path().join("preparation.json"),
                serde_json::to_vec(&failure).unwrap(),
            )
            .unwrap();
        } else {
            std::fs::write(
                relay_root.path().join("memory-sync-protocol"),
                (mj_core::relay::RELAY_WORKER_STARTUP_DEADLINE_PROTOCOL - 1).to_string(),
            )
            .unwrap();
        }
        let (commands, commands_rx) = mpsc::channel(4);
        let (_releases, releases_rx) = mpsc::unbounded_channel();
        let (retirement, retirement_rx) = watch::channel(false);
        let (view_tx, mut views) = watch::channel(ManagedSessionView::default());
        let (updates, _updates_rx) = coalesced_update_channel();
        let (attempt_tx, mut attempts) = mpsc::unbounded_channel();
        let attempt_number = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release_write = Arc::new(tokio::sync::Notify::new());
        let release_handler = release_write.clone();
        let request_log = relay_root.path().join(RELAY_REQUEST_LOG);
        assert!(
            updates
                .harness_failure
                .set(Arc::new(move |id, timeout| {
                    let attempt_tx = attempt_tx.clone();
                    let attempt_number = attempt_number.clone();
                    let release = release_handler.clone();
                    let request_log = request_log.clone();
                    Box::pin(async move {
                        let attempt =
                            attempt_number.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let syncs = std::fs::read_to_string(&request_log)
                            .unwrap()
                            .lines()
                            .filter(|line| *line == "attach")
                            .count();
                        attempt_tx.send((attempt, syncs)).unwrap();
                        match attempt {
                            0 => return Ok(false), // A lifecycle/revision guard rejected the write.
                            1 => anyhow::bail!("injected store write failure"),
                            2 => release.notified().await,
                            _ => anyhow::bail!("an accepted write must not be applied again"),
                        }
                        mj_core::runtime::spawn_blocking(move || {
                            let mut controller = crate::controller::Controller::load()?;
                            controller.fail_unready_session(
                                &id,
                                &timeout.cause(),
                                &timeout.observed_updated_at,
                            )
                        })
                        .await
                        .context("apply test readiness failure")?
                    })
                }))
                .is_ok()
        );
        let actor = mj_core::runtime::spawn(run_session_actor(
            leased_relay_target(relay_root.path()),
            commands_rx,
            releases_rx,
            retirement_rx,
            view_tx,
            updates,
        ));
        views
            .wait_for(|view| view.connected && view.snapshot.is_some())
            .await
            .unwrap();
        if !worker_deadline {
            tokio::time::pause();
            tokio::time::advance(HARNESS_READINESS_TIMEOUT + Duration::from_secs(1)).await;
            tokio::time::resume();
        }
        let mut previous_syncs = 0;
        for expected in 0..3 {
            let (attempt, syncs) = attempts.recv().await.unwrap();
            assert_eq!(attempt, expected);
            assert!(
                syncs > previous_syncs,
                "a retry requires a later successful worker sync"
            );
            previous_syncs = syncs;
        }
        // The third handler is waiting just before its store write. A new view
        // cannot be observed or overtake that decision inside the actor loop.
        let (reply, mut response) = oneshot::channel();
        commands.send(ActorCommand::Sync { reply }).await.unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(
            crate::database::load_session_record(LEASED_RELAY_SESSION)
                .unwrap()
                .unwrap()
                .state,
            mj_core::state::SessionState::Running
        );
        release_write.notify_one();
        response.await.unwrap().unwrap();
        let record = crate::database::load_session_record(LEASED_RELAY_SESSION)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, mj_core::state::SessionState::Error);
        assert!(record.last_error.unwrap().contains("Restart agent"));
        retirement.send(true).unwrap();
        actor.await.unwrap();
        assert!(
            attempts.try_recv().is_err(),
            "only the accepted write completes this wait"
        );
    }
}

#[cfg(unix)]
async fn next_policy_update(updates: &mut SessionManagerUpdates) -> SessionManagerUpdate {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let update = updates.recv().await.unwrap();
            if update
                .view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.operational.native_session_is_ready())
            {
                break update;
            }
        }
    })
    .await
    .unwrap()
}
