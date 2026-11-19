//! Tests of `mj_controller::session_manager` whose checks reach into the daemon layer,
//! so they live in mj-daemon.

use mj_controller::session_manager::stand_in::*;
use mj_controller::session_manager::*;
use std::sync::Arc;
use std::time::Duration;

use mj_controller::controller::test_support::{IsolatedTest, test_name};

// Hard-won: ec2-m8id16-b move stall: slow durable replies must finish once without a delivery deadline.
#[tokio::test]
async fn delegation_completes_with_web_disabled_and_dashboard_unconsumed() {
    const CHILD: &str = "MJ_TEST_DELEGATION_NO_DASHBOARD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(exact_test_name(
            "delegation_completes_with_web_disabled_and_dashboard_unconsumed",
        ))
        .env(CHILD, "1")
        .env("MJ_INSTANCE", "delegation-1171-unit")
        .isolated_store(directory.path())
        .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    register_leased_relay_session();
    let relay_root = tempfile::tempdir().unwrap();
    let mut parent = mj_controller::database::load_state()
        .unwrap()
        .sessions
        .remove(LEASED_RELAY_SESSION)
        .unwrap();
    parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
        worker_root: relay_root.path().join(LEASED_RELAY_SESSION),
    }));
    mj_controller::database::save_session(&parent).unwrap();
    let (feed, observations) = delegation_channel();
    let mut manager =
        spawn_session_manager_observed(Some(feed), Arc::new(NoRecoveryPlanner)).unwrap();
    manager
        .targets
        .send_replace(vec![leased_relay_target(relay_root.path())]);
    let recovery = crate::recovery::RecoveryCoordinator::spawn(manager.control.clone());
    let upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
        manager.control.clone(),
        &recovery.observer(),
    );
    let mut controller = mj_controller::controller::Controller::load().unwrap();
    controller.config.phone.enabled = false;
    controller.config.profiles.clear();
    let state = Arc::new(crate::daemon::RuntimeState::new(
        manager.control.clone(),
        controller,
        recovery.observer(),
        upgrades.observer(),
        Vec::new(),
    ));
    let stop = tokio_util::sync::CancellationToken::new();
    let (_services, task) = crate::daemon::delegation::spawn(
        state.clone(),
        manager.control.clone(),
        observations,
        stop.clone(),
    );
    let handle = manager
        .control
        .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(10))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while handle.view().snapshot.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let before = handle
        .view()
        .snapshot
        .unwrap()
        .materialized
        .applied_event_ordinal;
    std::fs::write(relay_root.path().join("slow-delegation-delivery"), "").unwrap();
    // The worker's request changes without a transcript event. No reader ever
    // consumes manager.updates and no web server or remote facade exists.
    let request = mj_core::subagent::SubagentToolRequest {
        originating_command_id: None,
        request_id: "list-without-dashboard".into(),
        created_at_ms: chrono::Utc::now().timestamp_millis(),
        action: mj_core::subagent::SubagentToolAction::ListAgents,
    };
    std::fs::write(
        relay_root.path().join("subagent-request.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let path = relay_root.path().join("subagent-result.json");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("delegation completes without a dashboard consumer");
    let result: mj_core::subagent::SubagentToolResult =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(result.request_id, request.request_id);
    assert!(!result.is_error, "{}", result.message);
    assert_eq!(
        handle
            .view()
            .snapshot
            .unwrap()
            .materialized
            .applied_event_ordinal,
        before
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while mj_controller::database::load_delegation(LEASED_RELAY_SESSION, &request.request_id)
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("delivery must acknowledge the durable result");
    let log = std::fs::read_to_string(relay_root.path().join(RELAY_REQUEST_LOG)).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| *line == "complete_subagent_request")
            .count(),
        1,
        "a slow result must have only one delivery attempt"
    );
    // A second slow reply is safe to cancel during handoff: the result is
    // persisted before delivery, and must remain available to the next daemon.
    let request = mj_core::subagent::SubagentToolRequest {
        request_id: "slow-reply-at-handoff".into(),
        ..request
    };
    std::fs::write(
        relay_root.path().join("subagent-request.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let log = std::fs::read_to_string(relay_root.path().join(RELAY_REQUEST_LOG)).unwrap();
            if log
                .lines()
                .filter(|line| *line == "complete_subagent_request")
                .count()
                == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the second reply reaches the slow exchange");
    // The original check deliberately left dashboard updates unconsumed.
    // For handoff, consume them as the daemon does: each queued update holds
    // admission until its reader advances. Delivery itself must hold none.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            while manager.updates.try_recv().is_ok() {}
            if mj_controller::upgrade::gate().try_close() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "durable delivery blocked handoff: {:?}",
            mj_controller::upgrade::active_labels()
        )
    });
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("handoff cancels delivery without waiting for the slow relay")
        .unwrap()
        .unwrap();
    assert!(
        mj_controller::database::load_delegation(LEASED_RELAY_SESSION, &request.request_id)
            .unwrap()
            .unwrap()
            .1
            .is_some()
    );
    manager.shutdown.shutdown().await.unwrap();
}

fn exact_test_name(test: &str) -> String {
    test_name(module_path!(), test)
}
