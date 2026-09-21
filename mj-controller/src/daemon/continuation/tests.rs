use super::*;
use mj_core::continuation::{ContinuationState, ContinuationVerdict};
use mj_core::relay::RelaySnapshot;
use mj_core::state::{
    ManagedSessionSnapshot, MaterializedSession, MaterializedTurnOutcome, ProjectionWindow,
    TranscriptBody, TranscriptItem,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

fn view(id: &str, completed: bool) -> ManagedSessionView {
    let mut materialized = MaterializedSession::empty(id);
    materialized.transcript = vec![
        Arc::new(TranscriptItem {
            stable_id: "user:request".into(),
            position: 1,
            latest_content_event_ordinal: Some(1),
            created_at_ms: 1,
            last_changed_at_ms: 1,
            body: TranscriptBody::User {
                content: vec![json!({"type":"text","text":"Implement the fix and run the tests."})],
            },
        }),
        Arc::new(TranscriptItem {
            stable_id: "agent:reply".into(),
            position: 2,
            latest_content_event_ordinal: Some(2),
            created_at_ms: 2,
            last_changed_at_ms: 2,
            body: TranscriptBody::Agent {
                chunks: vec![
                    json!({"content":{"type":"text","text":"Implemented. Shall I run the tests?"}}),
                ],
                streaming: false,
            },
        }),
    ];
    if completed {
        materialized.last_turn_outcome = Some(MaterializedTurnOutcome {
            diagnostic: None,
            usage: None,
            command_id: "request".into(),
            accepted_ordinal: Some(1),
            turn_start_position: Some(1),
            completed_ordinal: 3,
            completed_at_ms: 3,
            outcome: TurnOutcomeKind::Completed {
                stop_reason: "end_turn".into(),
            },
        });
    }
    let mut operational = RelaySnapshot::new(id.into()).operational_state();
    operational.relay_protocol_version = Some(mj_core::relay::RELAY_PROTOCOL_VERSION);
    operational.continuation = ContinuationState {
        user_command_id: Some("request".into()),
        completed_command_id: completed.then(|| "request".into()),
        ..Default::default()
    };
    ManagedSessionView {
        connected: true,
        error: None,
        snapshot: Some(ManagedSessionSnapshot {
            window: ProjectionWindow::of(&materialized),
            materialized,
            operational,
            latest_credential_sync_signal: None,
            worker_build: None,
            subagent_requests: vec![],
            subagent_results: vec![],
        }),
    }
}

async fn receive(rx: &mut SessionManagerUpdates) -> SessionManagerUpdate {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn continuation_checks_run_concurrently_and_defer_review_until_each_settles() {
    let remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(
        ["one", "two"]
            .into_iter()
            .map(|id| RelaySessionTarget {
                session_id: id.into(),
                spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
                worker_recovery: None,
                project_memory: None,
            })
            .collect(),
    );
    let log_dir = tempfile::tempdir().unwrap();
    let decision_log = mj_core::jev::DecisionLog::open(log_dir.path().into()).unwrap();
    let reviews = Arc::new(Mutex::new(Vec::new()));
    let enabled = Arc::new(AtomicBool::new(true));
    let environment = Environment {
        quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected quota request") })),
        profile: Arc::new(|_| Some("test".into())),
        log: Some(decision_log),
        control: remote.control,
        allowed: {
            let enabled = enabled.clone();
            Arc::new(move |_| enabled.load(Ordering::SeqCst))
        },
        live: Arc::new(|| ["one".into(), "two".into()].into()),
        review: {
            let reviews = reviews.clone();
            Arc::new(move |id, view| {
                if view
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .materialized
                    .last_turn_outcome
                    .is_some()
                {
                    reviews.lock().unwrap().push(id.to_owned());
                }
            })
        },
    };
    let (calls, mut requests) = mpsc::unbounded_channel();
    let classifier: Classifier = Arc::new(move |evidence, _diagnostic| {
        let (tx, rx) = oneshot::channel::<Result<ContinuationVerdict>>();
        calls.send((evidence, tx)).unwrap();
        Box::pin(async move { rx.await.context("fake classifier cancelled")? })
    });
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_in(
        environment,
        remote.updates,
        cancellation.clone(),
        classifier,
    );
    for id in ["one", "two"] {
        remote
            .publisher
            .publish(id.into(), view(id, false))
            .await
            .unwrap();
        receive(&mut updates).await;
        remote
            .publisher
            .publish(id.into(), view(id, true))
            .await
            .unwrap();
        let update = receive(&mut updates).await;
        assert_eq!(
            update.view.snapshot.unwrap().operational.activity,
            Some(ActivityState::CheckingContinuation)
        );
    }
    let (evidence, first) = tokio::time::timeout(Duration::from_secs(3), requests.recv())
        .await
        .unwrap()
        .unwrap();
    let (_, second) = tokio::time::timeout(Duration::from_secs(3), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        evidence.messages[0].text,
        "Implement the fix and run the tests."
    );
    assert!(reviews.lock().unwrap().is_empty());
    first
        .send(Ok(ContinuationVerdict {
            quota_limit: 0.0,
            unfinished: 0.0,
            no_input_needed: 1.0,
        }))
        .unwrap();
    receive(&mut updates).await;
    assert_eq!(reviews.lock().unwrap().len(), 1);
    // Disabling cancels the independent remaining HTTP request and releases review.
    enabled.store(false, Ordering::SeqCst);
    receive(&mut updates).await;
    assert_eq!(reviews.lock().unwrap().len(), 2);
    tokio::task::yield_now().await;
    assert!(second.is_closed());
    let statuses: Vec<_> = ["one", "two"]
        .iter()
        .flat_map(|id| {
            mj_core::jev::read(log_dir.path(), id, None)
                .unwrap()
                .decisions
        })
        .map(|d| d.status)
        .collect();
    assert!(statuses.contains(&"uncertain".to_owned()));
    assert!(statuses.contains(&"cancelled".to_owned()));
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn continuation_does_not_revive_old_idle_sessions_and_new_input_cancels_a_check() {
    let remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(
        ["one", "two"]
            .into_iter()
            .map(|id| RelaySessionTarget {
                session_id: id.into(),
                spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
                worker_recovery: None,
                project_memory: None,
            })
            .collect(),
    );
    let (calls, mut requests) = mpsc::unbounded_channel();
    let classifier: Classifier = Arc::new(move |_, _| {
        let (tx, rx) = oneshot::channel::<Result<ContinuationVerdict>>();
        calls.send(tx).unwrap();
        Box::pin(async move { rx.await.context("fake classifier cancelled")? })
    });
    let log_dir = tempfile::tempdir().unwrap();
    let log = mj_core::jev::DecisionLog::open(log_dir.path().into()).unwrap();
    let environment = Environment {
        quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected quota request") })),
        profile: Arc::new(|_| Some("test".into())),
        log: Some(log),
        control: remote.control,
        allowed: Arc::new(|_| true),
        live: Arc::new(|| ["one".into()].into()),
        review: Arc::new(|_, _| {}),
    };
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_in(
        environment,
        remote.updates,
        cancellation.clone(),
        classifier,
    );
    remote
        .publisher
        .publish("one".into(), view("one", true))
        .await
        .unwrap();
    receive(&mut updates).await;
    assert!(requests.try_recv().is_err());
    remote
        .publisher
        .publish("one".into(), view("one", false))
        .await
        .unwrap();
    receive(&mut updates).await;
    remote
        .publisher
        .publish("one".into(), view("one", true))
        .await
        .unwrap();
    receive(&mut updates).await;
    let reply = tokio::time::timeout(Duration::from_secs(3), requests.recv())
        .await
        .unwrap()
        .unwrap();
    let mut next = view("one", true);
    next.snapshot.as_mut().unwrap().operational.continuation = ContinuationState {
        user_command_id: Some("new-request".into()),
        ..Default::default()
    };
    remote.publisher.publish("one".into(), next).await.unwrap();
    let update = receive(&mut updates).await;
    assert_ne!(
        update.view.snapshot.unwrap().operational.activity,
        Some(ActivityState::CheckingContinuation)
    );
    tokio::task::yield_now().await;
    assert!(reply.is_closed());
    assert_eq!(
        mj_core::jev::read(log_dir.path(), "one", None)
            .unwrap()
            .decisions[0]
            .status,
        "stale"
    );
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn continuation_submits_a_guarded_prompt_and_reviews_only_after_the_chain() {
    for rejected in [false, true] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let log_dir = tempfile::tempdir().unwrap();
        let log = mj_core::jev::DecisionLog::open(log_dir.path().into()).unwrap();
        let reviews = Arc::new(Mutex::new(Vec::new()));
        let environment = Environment {
            quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected quota request") })),
            profile: Arc::new(|_| Some("test".into())),
            log: Some(log),
            control: remote.control,
            allowed: Arc::new(|_| true),
            live: Arc::new(|| ["one".into()].into()),
            review: {
                let reviews = reviews.clone();
                Arc::new(move |_, v| {
                    if eligible(v) {
                        reviews.lock().unwrap().push(v.clone());
                    }
                })
            },
        };
        let (calls, mut requests) = mpsc::unbounded_channel();
        let classifier: Classifier = Arc::new(move |_, _| {
            let (tx, rx) = oneshot::channel::<Result<ContinuationVerdict>>();
            calls.send(tx).unwrap();
            Box::pin(async move { rx.await.context("fake classifier cancelled")? })
        });
        let cancellation = CancellationToken::new();
        let (mut updates, task) = spawn_in(
            environment,
            remote.updates,
            cancellation.clone(),
            classifier,
        );
        remote
            .publisher
            .publish("one".into(), view("one", false))
            .await
            .unwrap();
        receive(&mut updates).await;
        remote
            .publisher
            .publish("one".into(), view("one", true))
            .await
            .unwrap();
        receive(&mut updates).await;
        let answer = tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
        answer
            .send(Ok(ContinuationVerdict {
                quota_limit: 0.0,
                unfinished: 0.99,
                no_input_needed: 0.99,
            }))
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap();
        let RemoteSessionRequest::Submit {
            command_id,
            command,
            reply,
            ..
        } = request
        else {
            panic!("expected continuation prompt")
        };
        assert_eq!(command_id, "auto-continue-3-1");
        assert!(
            matches!(command, RelayCommand::ContinueAuthorizedWork { user_command_id, completed_command_id, attempt: 1, .. }
        if user_command_id == "request" && completed_command_id == "request")
        );
        if rejected {
            reply
                .send(Err(
                    "continuation guard rejected changed worker frontier".into()
                ))
                .unwrap();
            receive(&mut updates).await;
            let records = mj_core::jev::read(log_dir.path(), "one", None).unwrap();
            assert_eq!(records.decisions[0].status, "failed");
            assert!(
                records.decisions[0]
                    .answer
                    .contains("already-requested work remains")
            );
            assert!(!records.decisions[0].action.contains("accepted"));
            cancellation.cancel();
            task.await.unwrap().unwrap();
            remote.shutdown.shutdown().await.unwrap();
            continue;
        }
        reply.send(Ok(4)).unwrap();
        let mut running = view("one", true);
        let operational = &mut running.snapshot.as_mut().unwrap().operational;
        operational.execution = mj_core::relay::RelayExecutionState::Running;
        operational.continuation.attempts = 1;
        operational.continuation.completed_command_id = None;
        remote
            .publisher
            .publish("one".into(), running)
            .await
            .unwrap();
        receive(&mut updates).await;
        assert!(reviews.lock().unwrap().is_empty());
        // The submitted job is supervised even when the worker's running view arrives first.
        for _ in 0..20 {
            if mj_core::jev::read(log_dir.path(), "one", None)
                .unwrap()
                .decisions
                .iter()
                .any(|d| d.status == "applied")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            mj_core::jev::read(log_dir.path(), "one", None)
                .unwrap()
                .decisions
                .iter()
                .any(|d| d.status == "applied")
        );
        let mut finished = view("one", true);
        let s = finished.snapshot.as_mut().unwrap();
        s.operational.continuation.attempts = 1;
        s.operational.continuation.completed_command_id = Some(command_id.clone());
        s.materialized
            .last_turn_outcome
            .as_mut()
            .unwrap()
            .command_id = command_id;
        remote
            .publisher
            .publish("one".into(), finished)
            .await
            .unwrap();
        let update = receive(&mut updates).await;
        assert_eq!(
            update.view.snapshot.unwrap().operational.activity,
            Some(ActivityState::CheckingContinuation)
        );
        let answer = tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
        answer.send(Err(anyhow!("classifier unavailable"))).unwrap();
        receive(&mut updates).await;
        assert_eq!(reviews.lock().unwrap().len(), 1);
        assert!(
            mj_core::jev::read(log_dir.path(), "one", None)
                .unwrap()
                .decisions
                .iter()
                .any(|d| d.status == "failed")
        );
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}

fn recovery_for(
    v: &ManagedSessionView,
    retry_at_ms: Option<i64>,
) -> mj_core::continuation::QuotaRecovery {
    let c = &v.snapshot.as_ref().unwrap().operational.continuation;
    mj_core::continuation::QuotaRecovery {
        user_command_id: c.user_command_id.clone().unwrap(),
        completed_command_id: c.completed_command_id.clone().unwrap(),
        profile_id: "test".into(),
        reset_at_ms: retry_at_ms.map(|t| t - 60_000),
        retry_at_ms,
        notice: "Quota limit; waiting for reset".into(),
        submitted: false,
    }
}

#[tokio::test]
async fn quota_classification_survives_exhausted_allowance_and_oversized_authorization() {
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let environment = Environment {
        log: None,
        control: remote.control,
        allowed: Arc::new(|_| true),
        live: Arc::new(|| ["one".into()].into()),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(|_, view| Box::pin(async move { Ok(recovery_for(&view, None)) })),
    };
    let classifier: Classifier = Arc::new(|evidence, _| {
        Box::pin(async move {
            assert!(evidence.messages.is_empty());
            assert!(
                evidence
                    .quota_message
                    .as_ref()
                    .unwrap()
                    .contains("Provider error")
            );
            Ok(ContinuationVerdict {
                quota_limit: 0.99,
                unfinished: 1.0,
                no_input_needed: 1.0,
            })
        })
    });
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_in(
        environment,
        remote.updates,
        cancellation.clone(),
        classifier,
    );
    remote
        .publisher
        .publish("one".into(), view("one", false))
        .await
        .unwrap();
    receive(&mut updates).await;
    let mut blocked = view("one", true);
    let s = blocked.snapshot.as_mut().unwrap();
    s.operational.continuation.attempts = 3;
    s.operational.continuation.suppressed = true;
    let turn = s.materialized.last_turn_outcome.as_mut().unwrap();
    turn.outcome = TurnOutcomeKind::Completed {
        stop_reason: "Error".into(),
    };
    turn.diagnostic = Some(mj_core::diagnostic::TurnDiagnostic {
        message: "Subscription allowance consumed".into(),
        code: None,
        http_status: None,
        reset_at: None,
    });
    let item = Arc::make_mut(&mut s.materialized.transcript[0]);
    item.body = TranscriptBody::User {
        content: vec![json!({"type":"text", "text":"x".repeat(40_000)})],
    };
    remote
        .publisher
        .publish("one".into(), blocked)
        .await
        .unwrap();
    receive(&mut updates).await;
    let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let RemoteSessionRequest::Submit { command, reply, .. } = request else {
        panic!("expected quota scheduling");
    };
    assert!(
        matches!(command, RelayCommand::SetQuotaRecovery { recovery: Some(ref r), .. } if r.retry_at_ms.is_none())
    );
    reply
        .send(Err("end isolated scheduling probe".into()))
        .unwrap();
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovered_deadlines_wait_resume_or_clear_without_reclassification() {
    for case in ["future", "unknown", "due", "disabled"] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let enabled = case != "disabled";
        let environment = Environment {
            log: None,
            control: remote.control,
            allowed: Arc::new(move |_| enabled),
            live: Arc::new(|| ["one".into()].into()),
            review: Arc::new(|_, _| panic!("quota-blocked turn must not be reviewed")),
            profile: Arc::new(|_| Some("test".into())),
            quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected refresh") })),
        };
        let classifier: Classifier = Arc::new(|_, _| {
            Box::pin(async { panic!("durable recovery must not be reclassified") })
        });
        let cancellation = CancellationToken::new();
        let (mut updates, task) = spawn_in(
            environment,
            remote.updates,
            cancellation.clone(),
            classifier,
        );
        let mut recovered = view("one", true);
        let deadline = match case {
            "unknown" => None,
            "future" => Some(mj_core::clock::epoch_millis() + 60_000),
            _ => Some(mj_core::clock::epoch_millis() - 1),
        };
        let recovery = recovery_for(&recovered, deadline);
        recovered
            .snapshot
            .as_mut()
            .unwrap()
            .operational
            .continuation
            .quota_recovery = Some(recovery);
        remote
            .publisher
            .publish("one".into(), recovered)
            .await
            .unwrap();
        receive(&mut updates).await;
        if matches!(case, "future" | "unknown") {
            assert!(
                tokio::time::timeout(Duration::from_millis(250), remote.requests.recv())
                    .await
                    .is_err()
            );
        } else {
            let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap();
            let RemoteSessionRequest::Submit {
                command,
                reply,
                command_id,
                ..
            } = request
            else {
                panic!("expected guarded recovery");
            };
            if enabled {
                assert_eq!(command_id, "quota-retry-3");
                assert!(matches!(command, RelayCommand::ResumeAfterQuota { .. }));
            } else {
                assert!(matches!(
                    command,
                    RelayCommand::SetQuotaRecovery { recovery: None, .. }
                ));
            }
            reply.send(Err("end isolated resume probe".into())).unwrap();
        }
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}
