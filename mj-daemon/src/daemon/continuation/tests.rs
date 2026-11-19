use super::*;
use mj_core::continuation::ContinuationState;
use mj_core::relay::RelaySnapshot;
use mj_core::state::{
    ManagedSessionSnapshot, MaterializedSession, MaterializedTurnOutcome, ProjectionWindow,
    TranscriptBody, TranscriptItem,
};
use serde_json::json;

fn live_sessions(ids: &[&str]) -> LiveSessions {
    ids.iter()
        .map(|id| {
            let record =
                super::super::tests::runtime_test_session(id, "workspace", SessionState::Running);
            ((*id).to_owned(), record)
        })
        .collect()
}
use tokio::sync::mpsc;

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
            work_facts: None,
            answered_through: None,
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
    // A worker before the assessment protocol: its recoveries are resumed,
    // but the daemon never classifies its turns.
    operational.relay_protocol_version = Some(24);
    operational.continuation = ContinuationState {
        user_command_id: Some("request".into()),
        completed_command_id: completed.then(|| "request".into()),
        ..Default::default()
    };
    ManagedSessionView {
        attention: Default::default(),
        connected: true,
        error: None,
        snapshot: Some(ManagedSessionSnapshot {
            window: ProjectionWindow::of(&materialized),
            materialized,
            operational: operational.into(),
            latest_credential_sync_signal: None,
            worker_build: None,
            subagent_requests: Default::default(),
            subagent_completions: Default::default(),
            subagent_queue_error: None,
        }),
    }
}

/// Marks the view's last turn as one the worker's own assessment found
/// blocked by a provider quota, as a worker on the assessment protocol reports
/// it. The daemon acts on that assessment and never classifies a turn itself.
fn quota_assessed(v: &mut ManagedSessionView) {
    use mj_core::assessment::{
        Action, Failure, Input, Judgment, Status, TurnAssessment, Verdict, Work,
    };
    let state = &mut v.snapshot.as_mut().unwrap().operational;
    state.relay_protocol_version = Some(mj_core::assessment::PROTOCOL);
    state.assessment_context = Some(Default::default());
    let mut assessment = TurnAssessment::pending(
        state.continuation.completed_command_id.clone().unwrap(),
        99,
        99,
        mj_core::activity::verdict::CompletionEvidence::bounded("harness_turn_settled", None),
    );
    assessment.verdict = Some(Verdict {
        failure: Judgment {
            choice: Failure::Quota,
            confidence: 0.99,
            probabilities: Default::default(),
        },
        input: Judgment {
            choice: Input::Unclear,
            confidence: 0.5,
            probabilities: Default::default(),
        },
        work: Judgment {
            choice: Work::Unclear,
            confidence: 0.5,
            probabilities: Default::default(),
        },
        background: None,
        reply: None,
    });
    assessment.action = Some(Action::RecoverQuota);
    assessment.status = Status::Deferred;
    state.assessment = Some(std::sync::Arc::new(assessment));
}

async fn receive(rx: &mut SessionManagerUpdates) -> SessionManagerUpdate {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap()
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
async fn a_worker_quota_assessment_schedules_recovery_whatever_the_spent_allowance() {
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let environment = Environment {
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        log: None,
        control: remote.control,
        allowed: Arc::new(|_| true),
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(|_, view| {
            Box::pin(async move { Ok(quota::Plan::Wait(recovery_for(&view, None))) })
        }),
    };
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_in(environment, remote.updates, cancellation.clone());
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
        login_fingerprint: None,
    });
    quota_assessed(&mut blocked);
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
    // The "timed-" cases hold a record with a deadline. A Move that owns the
    // session (`moving`) leaves it alone and nothing fires while it runs; any
    // other owner, or a changed profile, clears it whatever the time; and once
    // the Move has ended on the same profile it fires at its deadline.
    for case in [
        "future",
        "unknown",
        "due",
        "disabled",
        "moving",
        "moved",
        "timed-moving",
        "timed-moving-future",
        "timed-other-lifecycle",
        "timed-moved",
        "timed-move-failed",
    ] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let enabled = !matches!(
            case,
            "disabled"
                | "moving"
                | "timed-moving"
                | "timed-moving-future"
                | "timed-other-lifecycle"
                | "timed-moved"
        );
        let moving = matches!(
            case,
            "moving" | "timed-moving" | "timed-moving-future" | "timed-moved"
        );
        let environment = Environment {
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move start") })
            }),
            log: None,
            control: remote.control,
            allowed: Arc::new(move |_| enabled),
            claim: Arc::new(move |_| {
                if moving {
                    Claim::Move
                } else if enabled {
                    Claim::Allowed
                } else {
                    Claim::Blocked
                }
            }),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| panic!("quota-blocked turn must not be reviewed")),
            profile: Arc::new(move |_| {
                Some(
                    if case.ends_with("moved") {
                        "destination"
                    } else {
                        "test"
                    }
                    .into(),
                )
            }),
            quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected refresh") })),
        };
        let cancellation = CancellationToken::new();
        let (mut updates, task) = spawn_in(environment, remote.updates, cancellation.clone());
        let mut recovered = view("one", true);
        let deadline = match case {
            "unknown" | "moving" | "moved" => None,
            "future" | "timed-moving-future" | "timed-other-lifecycle" => {
                Some(mj_core::clock::epoch_millis() + 60_000)
            }
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
        if matches!(
            case,
            "future" | "unknown" | "moving" | "timed-moving" | "timed-moving-future"
        ) {
            assert!(
                tokio::time::timeout(Duration::from_millis(250), remote.requests.recv())
                    .await
                    .is_err()
            );
        } else {
            let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap_or_else(|_| panic!("{case}: no recovery request within 3 s"))
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
            if enabled && !case.ends_with("moved") {
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

/// F18: a due recovery that the loop declines to submit is never silent. With
/// a harness turn still open the loop submits nothing, and the decision log
/// names the fact that refused it, once however many minutes pass.
#[tokio::test]
async fn a_due_recovery_that_cannot_be_submitted_says_which_fact_refused_it() {
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let log_dir = tempfile::tempdir().unwrap();
    let environment = Environment {
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        log: Some(mj_core::jev::DecisionLog::open(log_dir.path().to_path_buf()).unwrap()),
        control: remote.control,
        allowed: Arc::new(|_| true),
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| panic!("quota-blocked turn must not be reviewed")),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected refresh") })),
    };
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_in(environment, remote.updates, cancellation.clone());
    let mut stuck = view("one", true);
    let recovery = recovery_for(&stuck, Some(mj_core::clock::epoch_millis() - 1));
    let operational = &mut stuck.snapshot.as_mut().unwrap().operational;
    operational.continuation.quota_recovery = Some(recovery);
    operational.harness_turn = Some(mj_core::relay::HarnessTurn { started_at_ms: 1 });
    remote.publisher.publish("one".into(), stuck).await.unwrap();
    receive(&mut updates).await;
    // Several ticks pass; nothing is submitted.
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), remote.requests.recv())
            .await
            .is_err()
    );
    let page = mj_core::jev::read(log_dir.path(), "one", None).unwrap();
    assert_eq!(
        page.decisions.len(),
        1,
        "logged once for one set of reasons"
    );
    let decision = &page.decisions[0];
    assert_eq!(decision.kind, "quota-recovery");
    assert_eq!(decision.status, "deferred");
    assert!(
        decision
            .action
            .contains("a turn the harness started is open"),
        "{}",
        decision.action
    );
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

/// The first decision recorded for session "one", with its technical detail,
/// which a listing leaves out.
/// `view(id, true)` after the harness started a turn on its own at ordinal 10
/// and ended it at 12 with `reply`.
fn self_started(id: &str, reply: &str) -> ManagedSessionView {
    let mut v = view(id, true);
    let s = v.snapshot.as_mut().unwrap();
    s.materialized.transcript.push(Arc::new(TranscriptItem {
        stable_id: format!("{}10", mj_core::transcript::HARNESS_TURN_ITEM_PREFIX),
        position: 10,
        latest_content_event_ordinal: None,
        created_at_ms: 10,
        last_changed_at_ms: 10,
        body: TranscriptBody::System {
            text: mj_core::transcript::HARNESS_TURN_TEXT.into(),
        },
    }));
    s.materialized.transcript.push(Arc::new(TranscriptItem {
        stable_id: "agent:self-started".into(),
        position: 11,
        latest_content_event_ordinal: Some(11),
        created_at_ms: 11,
        last_changed_at_ms: 11,
        body: TranscriptBody::Agent {
            chunks: vec![json!({"content":{"type":"text","text":reply}})],
            streaming: false,
        },
    }));
    s.window = ProjectionWindow::of(&s.materialized);
    let turn_id = mj_core::continuation::harness_turn_id(10);
    s.operational.continuation.completed_command_id = Some(turn_id.clone());
    s.operational.continuation.harness_turn = Some(mj_core::continuation::HarnessCompletion {
        id: turn_id,
        start_position: 10,
        settled_ordinal: 12,
        settled_at_ms: 12,
    });
    v
}

#[tokio::test]
async fn a_self_started_turn_that_hits_the_quota_schedules_recovery_against_itself() {
    // Workers before the assessment protocol have no worker assessment, and
    // the daemon no longer classifies their turns: they are not scheduled.
    for protocol in [24, 22, mj_core::assessment::PROTOCOL] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let log_dir = tempfile::tempdir().unwrap();
        let log = mj_core::jev::DecisionLog::open(log_dir.path().to_path_buf()).unwrap();
        let environment = Environment {
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move start") })
            }),
            log: Some(log),
            control: remote.control,
            allowed: Arc::new(|_| true),
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: Arc::new(|_| Some("test".into())),
            quota: Arc::new(|_, view| {
                Box::pin(async move { Ok(quota::Plan::Wait(recovery_for(&view, None))) })
            }),
        };
        let cancellation = CancellationToken::new();
        let (mut updates, task) = spawn_in(environment, remote.updates, cancellation.clone());
        let mut before = view("one", true);
        let mut after = self_started("one", "You've hit your session limit · resets 1:40pm");
        // Keep the preceding prompted completion cancelled in both views.
        for v in [&mut before, &mut after] {
            let snapshot = v.snapshot.as_mut().unwrap();
            snapshot
                .materialized
                .last_turn_outcome
                .as_mut()
                .unwrap()
                .outcome = TurnOutcomeKind::Completed {
                stop_reason: "cancelled".into(),
            };
            snapshot.operational.relay_protocol_version = Some(protocol);
        }
        remote
            .publisher
            .publish("one".into(), before)
            .await
            .unwrap();
        receive(&mut updates).await;
        if protocol >= mj_core::assessment::PROTOCOL {
            quota_assessed(&mut after);
        }
        remote.publisher.publish("one".into(), after).await.unwrap();
        receive(&mut updates).await;
        if protocol < mj_core::assessment::PROTOCOL {
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
                command_id,
                command,
                reply,
                ..
            } = request
            else {
                panic!("expected quota scheduling");
            };
            assert_eq!(command_id, "quota-schedule-12");
            assert!(
                matches!(command, RelayCommand::SetQuotaRecovery { recovery: Some(ref r), .. }
                if r.completed_command_id == mj_core::continuation::harness_turn_id(10))
            );
            reply
                .send(Err("end isolated scheduling probe".into()))
                .unwrap();
            assert!(
                mj_core::jev::read(log_dir.path(), "one", None)
                    .unwrap()
                    .decisions
                    .is_empty(),
                "only the worker owns a new assessment's diagnostic"
            );
        }
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_due_recovery_resumes_a_usage_limited_goal_and_leaves_a_spent_budget_alone() {
    for reason in ["usage", "budget"] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let environment = Environment {
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move start") })
            }),
            log: None,
            control: remote.control,
            allowed: Arc::new(|_| true),
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: Arc::new(|_| Some("test".into())),
            quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected refresh") })),
        };
        let cancellation = CancellationToken::new();
        let (mut updates, task) = spawn_in(environment, remote.updates, cancellation.clone());
        let mut recovered = view("one", true);
        let recovery = recovery_for(&recovered, Some(mj_core::clock::epoch_millis() - 1));
        let operational = &mut recovered.snapshot.as_mut().unwrap().operational;
        operational.continuation.quota_recovery = Some(recovery);
        operational.goal.capability = Some(mj_core::goal::GoalCapability {
            version: 1,
            control_method: "_session/goal".into(),
            actions: vec!["resume".into()],
        });
        operational.goal.snapshot = Some(mj_core::goal::GoalSnapshot {
            objective: "Finish the campaign".into(),
            status: "limited".into(),
            created_at: Some(100_000),
            control_method: Some("_session/goal".into()),
            details: [("limitReason".to_owned(), json!(reason))].into(),
        });
        remote
            .publisher
            .publish("one".into(), recovered)
            .await
            .unwrap();
        receive(&mut updates).await;
        if reason == "budget" {
            assert!(
                tokio::time::timeout(Duration::from_millis(250), remote.requests.recv())
                    .await
                    .is_err(),
                "only the user may continue a goal whose budget is spent"
            );
        } else {
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
                panic!("expected goal resume");
            };
            assert_eq!(command_id, "quota-goal-resume-3");
            assert!(matches!(
                command,
                RelayCommand::GoalControl {
                    action: mj_core::goal::GoalControlAction::Resume
                }
            ));
            reply.send(Err("end isolated resume probe".into())).unwrap();
        }
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}

#[test]
fn distant_quota_reset_records_the_wait_instead_of_scheduling_one() {
    let now = 1_700_000_000_000;
    let horizon = quota::AUTORESUME_HORIZON_MS;

    let (reset, retry, notice) = quota::deadline(Some(now + horizon), now, false);
    assert_eq!(reset, Some(now + horizon));
    assert_eq!(retry, Some(now + horizon + 60_000));
    assert!(notice.contains("Automatically continuing at"), "{notice}");

    let (reset, retry, notice) = quota::deadline(Some(now + horizon + 1), now, false);
    assert_eq!((reset, retry), (None, None));
    assert!(notice.contains("more than 16 hours away"), "{notice}");
    assert!(notice.contains("2023-11-"), "{notice}");

    let (reset, retry, notice) = quota::deadline(None, now, false);
    assert_eq!((reset, retry), (None, None));
    assert!(notice.contains("No reliable reset time"), "{notice}");
}

#[tokio::test]
async fn handoff_defers_a_quota_assessment_without_consuming_its_trigger() {
    let remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let (calls, mut requests) = mpsc::unbounded_channel();
    let environment = Environment {
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        quota: Arc::new(move |_, _| {
            calls.send(()).unwrap();
            Box::pin(async { anyhow::bail!("end isolated quota probe") })
        }),
        profile: Arc::new(|_| Some("test".into())),
        log: None,
        control: remote.control,
        allowed: Arc::new(|_| true),
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| {}),
    };
    let gate = Arc::new(mj_controller::upgrade::Gate::default());
    let blocker = gate.enter("accepted operation").unwrap();
    assert!(!gate.try_close());
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_with_gate(
        environment,
        remote.updates,
        cancellation.clone(),
        gate.clone(),
    );
    let mut assessed = view("one", true);
    quota_assessed(&mut assessed);
    remote
        .publisher
        .publish("one".into(), assessed)
        .await
        .unwrap();
    receive(&mut updates).await;
    assert!(requests.try_recv().is_err());
    // When the upgrading client goes away, draining lapses. No new worker
    // publication is needed to recover the refused assessment.
    drop(blocker);
    tokio::time::timeout(Duration::from_secs(12), async {
        while gate.is_draining() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::time::timeout(Duration::from_secs(3), requests.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::resume();
    assert!(!gate.is_draining());
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

/// The update service with no sessions, reading updates from `input`.
fn idle_feed(
    control: SessionManagerControl,
    input: SessionManagerUpdates,
    cancellation: CancellationToken,
) -> UpdateFeed {
    let environment = Environment {
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        log: None,
        control,
        allowed: Arc::new(|_| false),
        claim: Arc::new(|_| Claim::Blocked),
        live: Arc::new(LiveSessions::new),
        review: Arc::new(|_, _| {}),
        quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected quota request") })),
        profile: Arc::new(|_| None),
    };
    let (updates, service) = spawn_in(environment, input, cancellation);
    UpdateFeed::new(updates, service)
}

/// A shutdown stops the continuation service, which closes the serving
/// loop's update channel while the shutdown token is also ready. The feed
/// must report that as a shutdown, not as a stopped session manager. The
/// second round also closes the session manager's channel at the same moment,
/// so the service sees both endings in one poll.
#[tokio::test]
async fn shutdown_ends_the_update_feed_without_reporting_a_session_manager_stop() {
    for manager_closes_too in [false, true] {
        let remote = spawn_remote_session_manager().unwrap();
        let (manager, input) = coalesced_update_channel();
        let cancellation = CancellationToken::new();
        let mut feed = idle_feed(remote.control, input, cancellation.clone());
        // The service has not been polled yet, so whatever is set here is
        // ready together when it first runs.
        cancellation.cancel();
        let manager = (!manager_closes_too).then_some(manager);
        let end = tokio::time::timeout(Duration::from_secs(3), feed.next())
            .await
            .expect("the feed ends promptly");
        assert!(
            matches!(end, Ok(None)),
            "a shutdown was reported as {end:?} (manager closed too: {manager_closes_too})"
        );
        feed.join().await.unwrap();
        drop(manager);
        remote.shutdown.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_session_manager_that_stops_without_a_shutdown_request_is_an_error() {
    let remote = spawn_remote_session_manager().unwrap();
    let (manager, input) = coalesced_update_channel();
    let cancellation = CancellationToken::new();
    let mut feed = idle_feed(remote.control, input, cancellation.clone());
    drop(manager);
    let error = tokio::time::timeout(Duration::from_secs(3), feed.next())
        .await
        .expect("the feed ends promptly")
        .expect_err("an unrequested stop is a failure");
    assert_eq!(
        error.to_string(),
        "controller daemon session manager stopped"
    );
    // The daemon's epilogue cancels and joins the service afterwards. The
    // failure was already reported, so the join does not repeat it.
    cancellation.cancel();
    feed.join().await.unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

#[test]
fn quota_move_candidates_filter_model_health_and_keep_usage_priced_last() {
    use crate::server::api::{CandidateQuota, SubagentCandidate};
    use mj_core::config::{Config, HarnessKind, HarnessProfile};
    let mut config = Config::default();
    let candidates: Vec<_> = [
        "source",
        "api",
        "healthy",
        "unknown",
        "paid",
        "empty",
        "unhealthy",
        "other",
        "wrong-model",
        "windowed",
    ]
    .into_iter()
    .map(|id| {
        let kind = if id == "other" {
            HarnessKind::Claude
        } else {
            HarnessKind::Codex
        };
        config.profiles.insert(
            id.into(),
            HarnessProfile {
                enabled: true,
                environment: Default::default(),
                context_window_bytes: None,
                models: Default::default(),
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                codex_profile: None,
                legacy_muse_auto_review: None,
                load_balancing_may_consume_credits: false,
                claude_permission_mode: None,
                aws: None,
                ..mj_core::config::HarnessProfile::new(kind, PathBuf::default())
            },
        );
        let model = if id == "wrong-model" {
            "another-model"
        } else {
            "model"
        };
        let mut unhealthy = BTreeMap::new();
        if id == "unhealthy" {
            unhealthy.insert(
                "model".into(),
                mj_controller::profile_health::Unhealthy {
                    fault: mj_controller::profile_health::ProfileFault::QuotaExhausted,
                    until: std::time::SystemTime::now() + Duration::from_secs(60),
                    message: "quota".into(),
                },
            );
        }
        SubagentCandidate {
            profile_id: id.into(),
            harness: kind,
            choices: mj_core::worker_launch::ProfileConfig {
                model: Some(model.into()),
                models: vec![mj_core::acp::SessionConfigChoice {
                    value: model.into(),
                    name: model.into(),
                    description: None,
                }],
                efforts: vec![],
                observed_at: 1,
            },
            may_consume_credits: true,
            quota: match id {
                "api" => CandidateQuota::UsagePriced,
                "unknown" => CandidateQuota::Unknown,
                _ => CandidateQuota::Subscription {
                    monthly_price_usd: None,
                    refreshed_at_epoch_seconds: 0,
                    paid_credits: if matches!(id, "paid" | "unhealthy") {
                        mj_client::quota::PaidCredits::limited(75.0)
                    } else {
                        None
                    },
                    windows: vec![mj_controller::quota::QuotaWindow {
                        label: if id == "windowed" {
                            "5h".into()
                        } else {
                            "Week".into()
                        },
                        remaining_percent: Some(if matches!(id, "paid" | "empty" | "unhealthy") {
                            0
                        } else if id == "windowed" {
                            90
                        } else {
                            10
                        }),
                        duration_seconds: None,
                        model_scope: None,
                        used: None,
                        limit: None,
                        resets: None,
                        resets_at_epoch_seconds: None,
                    }],
                },
            },
            unhealthy,
        }
    })
    .collect();
    let ordered = quota::destinations(
        &config,
        "source",
        HarnessKind::Codex,
        "model",
        candidates,
        1,
    )
    .unwrap();
    assert_eq!(
        ordered
            .iter()
            .map(|c| c.profile_id.as_str())
            .collect::<Vec<_>>(),
        // A top-level quota move ranks a profile inside a five-hour window
        // below unwindowed and unknown quota, above paid credits and API.
        ["healthy", "unknown", "windowed", "paid", "api"]
    );
}

fn quota_move_request() -> mj_core::state::MoveSessionRequest {
    // This fixture crosses the continuation coordinator / Move admission boundary.
    serde_json::from_value(json!({
        "preparation": {
            "selection": {"session_id":"one", "profile_id":"destination", "target_template_id":null,
                "additional_mounts":null, "resource_allocation":null},
            "source_profile_id":"test", "source_target_template_id":"local",
            "cross_harness":false, "active":false, "in_place":true,
            "queued_commands":[], "fingerprint":"test", "operation_id":"quota-move-test"
        }, "queue":"start", "acknowledge_interruption":false
    }))
    .unwrap()
}

fn private_quota_move_error() -> anyhow::Error {
    let error = toml::from_str::<toml::Value>(
        r#"experimental_bearer_token = "quota-secret-canary" unexpected"#,
    )
    .unwrap_err();
    let error =
        anyhow::Error::new(error).context("read /private/profile/config.toml on secret-ssh-host");
    assert!(
        format!("{error:#}").contains("quota-secret-canary"),
        "the real TOML diagnostic carries the credential"
    );
    error
}

// Hard-won: #1470: raw preparation diagnostics leaked credentials through the quota recovery command.
#[tokio::test]
async fn quota_schedule_writes_timed_record_before_one_move_and_restores_deadline_on_refusal() {
    for case in [
        "weekly",
        "goal",
        "refused",
        "refused-private",
        "fallback-admission",
        "fallback-unknown",
        "five-hour",
        "preparation-private",
        "preparation-marked-private",
        "preparation-safe",
        "late-refused",
        "late-private",
        "late-outcome",
    ] {
        let moves = !matches!(
            case,
            "five-hour" | "preparation-private" | "preparation-marked-private" | "preparation-safe"
        );
        let rejected = matches!(
            case,
            "refused" | "refused-private" | "fallback-admission" | "fallback-unknown"
        );
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let session_gate = Arc::new(mj_controller::recovery_gate::RecoveryGate::default());
        let fallback_blocker = Arc::new(std::sync::Mutex::new(None));
        let environment = Environment {
            session_gate: session_gate.clone(),
            schedules: Default::default(),
            log: None,
            control: remote.control,
            allowed: Arc::new(|_| true),
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: Arc::new(|_| Some("test".into())),
            quota: Arc::new(|_, _| Box::pin(async { anyhow::bail!("unexpected refresh") })),
            start_quota_move: {
                let calls = calls.clone();
                let session_gate = session_gate.clone();
                let blocker = fallback_blocker.clone();
                Arc::new(move |request, reason, continuation| {
                    assert_eq!(
                        request.preparation.selection.profile_id.as_deref(),
                        Some("destination")
                    );
                    assert_eq!(reason, "the weekly quota is exhausted");
                    assert_eq!(
                        continuation,
                        if case == "goal" {
                            mj_core::state::QuotaMoveContinuation::ResumeGoal
                        } else {
                            mj_core::state::QuotaMoveContinuation::Prompt
                        }
                    );
                    calls.fetch_add(1, Ordering::SeqCst);
                    let gate = session_gate.clone();
                    let blocker = blocker.clone();
                    Box::pin(async move {
                        if case == "fallback-admission" {
                            *blocker.lock().unwrap() = Some(
                                gate.try_start(
                                    "one",
                                    mj_controller::recovery_gate::BackgroundWork::WorkerReplacement,
                                )
                                .unwrap(),
                            );
                        }
                        if matches!(case, "fallback-admission" | "fallback-unknown") {
                            return Err(anyhow::anyhow!("temporary Move refusal"));
                        }
                        if case == "refused" {
                            return Err(private_quota_move_error().context(
                                mj_core::refusal::Refusal::precondition("lifecycle busy"),
                            ));
                        }
                        if case == "refused-private" {
                            return Err(private_quota_move_error());
                        }
                        Ok(())
                    })
                })
            },
        };
        let failure_control = environment.control.clone();
        let mut blocked = view("one", true);
        if case == "goal" {
            let snapshot = blocked.snapshot.as_mut().unwrap();
            snapshot.operational.goal = serde_json::from_value(serde_json::json!({
                "known": true,
                "capability": {"version": 1, "controlMethod": "_session/goal", "actions": ["resume"]},
                "snapshot": {"objective": "Finish the interrupted work", "status": "limited", "createdAt": 42,
                    "controlMethod": "_session/goal", "limitReason": "usage"},
                "execution": {"version": 1, "status": "idle"}
            })).unwrap();
        }
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        let mut fallback =
            recovery_for(&blocked, Some(mj_core::clock::epoch_millis() + 18_060_000));
        if matches!(
            case,
            "preparation-private" | "preparation-marked-private" | "preparation-safe"
        ) {
            let error = if case == "preparation-safe" {
                private_quota_move_error().context(mj_core::refusal::Refusal::precondition(
                    "Cannot switch profiles automatically: existing sub-agents would be stopped",
                ).with_code(mj_controller::controller::move_session::QUOTA_MOVE_SAFETY_REFUSAL))
            } else if case == "preparation-marked-private" {
                let error = private_quota_move_error();
                let message = format!("{error:#}");
                error.context(mj_core::refusal::Refusal::precondition(message))
            } else {
                private_quota_move_error()
            };
            quota::preparation_refused(
                &mut fallback.notice,
                "one",
                "destination",
                &error,
                "quota-move-prepare-one-3",
            );
        }
        let plan = if !moves {
            quota::Plan::Wait(fallback.clone())
        } else {
            let mut abstention = fallback.clone();
            abstention.notice =
                quota::move_notice("the weekly quota is exhausted", "destination", &fallback);
            quota::Plan::Move {
                request: Box::new(quota_move_request()),
                destination: "destination".into(),
                reason: "the weekly quota is exhausted".into(),
                abstention,
                fallback: fallback.clone(),
            }
        };
        let retry_environment = environment.clone();
        let mut task = Some(mj_core::runtime::spawn(async move {
            quota::schedule(&environment, "one", blocked.clone(), plan).await
        }));
        let mut unknown_fallback = None;
        for step in 0..if case == "fallback-unknown" {
            3
        } else if rejected {
            2
        } else {
            1
        } {
            let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap();
            let RemoteSessionRequest::Submit { command, reply, .. } = request else {
                panic!("expected recovery")
            };
            if case == "fallback-unknown" && step == 1 {
                unknown_fallback = Some(command.clone());
            }
            if case == "fallback-unknown" && step == 2 {
                assert_eq!(
                    Some(&command),
                    unknown_fallback.as_ref(),
                    "unknown fallback acceptance must retain its exact cursor and notice"
                );
            }
            let wire = serde_json::to_string(&command).unwrap();
            for private in ["quota-secret-canary", "/private/profile", "secret-ssh-host"] {
                assert!(
                    !wire.contains(private),
                    "{case} leaked {private} to the worker journal command"
                );
            }
            let RelayCommand::SetQuotaRecovery {
                recovery: Some(recovery),
                ..
            } = command
            else {
                panic!("expected recovery record")
            };
            if step == 0 && moves {
                // The record written before the Move carries the retry, so a
                // Move that fails leaves it in place.
                assert_eq!(recovery.retry_at_ms, fallback.retry_at_ms);
                assert_eq!(recovery.reset_at_ms, fallback.reset_at_ms);
                assert!(recovery.retry_at_ms.is_some());
                assert!(
                    recovery.notice.starts_with(
                        "Subscription quota reached, and the weekly quota is exhausted. \
                         Switching this session to profile destination to continue the interrupted work. \
                         If the switch fails while the session is still running, it continues on profile test at "
                    ),
                    "{}",
                    recovery.notice
                );
                assert!(recovery.notice.ends_with(
                    " (one minute after reset). If the session has already stopped, \
                     the failure message says how to retry the Move."
                ));
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(recovery.retry_at_ms, fallback.retry_at_ms);
                if matches!(case, "refused" | "refused-private") {
                    assert!(recovery.notice.contains("quota-move-start-one-"));
                } else if matches!(case, "preparation-private" | "preparation-marked-private") {
                    assert!(recovery.notice.contains("quota-move-prepare-one-3"));
                } else if case == "preparation-safe" {
                    assert!(
                        recovery
                            .notice
                            .contains("existing sub-agents would be stopped")
                    );
                }
            }
            blocked = view("one", true);
            blocked
                .snapshot
                .as_mut()
                .unwrap()
                .operational
                .continuation
                .quota_recovery = Some(*recovery);
            remote
                .publisher
                .publish("one".into(), blocked.clone())
                .await
                .unwrap();
            if case == "fallback-unknown" && step == 1 {
                reply
                    .send(Err(mj_client::session::SubmitFailure {
                        message: "lost fallback acknowledgement".into(),
                        unconfirmed: true,
                        refused: false,
                    }))
                    .unwrap();
                assert!(task.take().unwrap().await.unwrap().is_err());
                let env = retry_environment.clone();
                let view = blocked.clone();
                let recovery = fallback.clone();
                task = Some(mj_core::runtime::spawn(async move {
                    quota::schedule(&env, "one", view, quota::Plan::Wait(recovery)).await
                }));
                continue;
            }
            reply.send(Ok(4 + step)).unwrap();
            let sync = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap();
            let RemoteSessionRequest::Sync { reply, .. } = sync else {
                panic!("expected sync")
            };
            reply.send(Ok(())).unwrap();
            if case == "fallback-admission" && step == 0 {
                assert!(task.take().unwrap().await.unwrap().is_err());
                fallback_blocker.lock().unwrap().take();
                let env = retry_environment.clone();
                let view = blocked.clone();
                let recovery = fallback.clone();
                task = Some(mj_core::runtime::spawn(async move {
                    quota::schedule(&env, "one", view, quota::Plan::Wait(recovery)).await
                }));
            }
        }
        task.take().unwrap().await.unwrap().unwrap();
        if matches!(case, "late-refused" | "late-private" | "late-outcome") {
            // No Move record exists for preflight errors. The worker must
            // receive the failure after its earlier "Switching" notice.
            let outcome: Result<mj_core::state::MoveOutcome> = if case == "late-refused" {
                Err(private_quota_move_error().context(
                    mj_core::refusal::Refusal::precondition(
                        "sub-agents appeared after preparation; prepare and confirm Move again",
                    )
                    .with_code(mj_controller::controller::move_session::QUOTA_MOVE_SAFETY_REFUSAL),
                ))
            } else if case == "late-outcome" {
                Ok(mj_core::state::MoveOutcome {
                    operation_id: "quota-move-test".into(),
                    session_id: "one".into(),
                    profile_id: "destination".into(),
                    target_template_id: "local".into(),
                    outcome: "failed".into(),
                    error: Some(format!("{:#}", private_quota_move_error())),
                    recovery: Some(
                        "Source retained and still running. Retry Move when ready.".into(),
                    ),
                })
            } else {
                Err(private_quota_move_error())
            };
            let text = crate::daemon::session_move::quota_move_failure_notice(
                &outcome,
                "one",
                "quota-move-test",
                "destination",
            )
            .expect("a failed Move reports a conversation notice");
            let recording = mj_core::runtime::spawn(async move {
                crate::daemon::session_move::record_quota_move_failure(
                    &failure_control,
                    "one",
                    "quota-move-test",
                    text,
                )
                .await
            });
            let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap();
            let RemoteSessionRequest::Submit { command, reply, .. } = request else {
                panic!("expected durable failure notice")
            };
            let wire = serde_json::to_string(&command).unwrap();
            for private in ["quota-secret-canary", "/private/profile", "secret-ssh-host"] {
                assert!(
                    !wire.contains(private),
                    "{case} leaked {private} in its asynchronous failure notice"
                );
            }
            let RelayCommand::RecordNotice { text } = command else {
                panic!("a failed switch must be recorded in the conversation")
            };
            assert!(
                text.contains("automatic switch to profile destination") && text.contains("failed")
            );
            if case == "late-refused" {
                assert!(text.contains("sub-agents appeared after preparation"));
            } else {
                assert!(text.contains("quota-move-test"));
            }
            if case == "late-outcome" {
                assert!(text.contains("Source retained and still running"));
            }
            reply.send(Ok(8)).unwrap();
            recording.await.unwrap().unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(moves));
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(150), remote.requests.recv()).await,
            Ok(Some(_))
        ));
        remote.shutdown.shutdown().await.unwrap();
    }
}

#[test]
fn move_notice_promises_a_retry_only_when_the_record_has_one_and_stays_under_the_worker_limit() {
    let blocked = view("one", true);
    let timed = recovery_for(&blocked, Some(mj_core::clock::epoch_millis() + 18_060_000));
    let untimed = recovery_for(&blocked, None);
    let notice = quota::move_notice("the quota is gone", "destination", &timed);
    assert!(notice.contains(
        "If the switch fails while the session is still running, it continues on profile test at"
    ));
    let notice = quota::move_notice("the quota is gone", "destination", &untimed);
    assert!(!notice.contains("If the switch fails"), "{notice}");

    // The longest reason `prepare` builds: a long profile id twice plus a time.
    let profile = "p".repeat(512);
    let mut long = timed;
    long.profile_id = profile.clone();
    let reason = format!(
        "the quota for profile {profile} resets at 2026-11-06 12:00:00 UTC (+00:00), more than five hours away"
    );
    let notice = quota::move_notice(&reason, &profile, &long);
    assert!(
        notice.len() <= quota::NOTICE_BYTES,
        "{} bytes",
        notice.len()
    );
}

#[test]
fn an_appended_error_is_cut_on_a_character_boundary_within_the_worker_limit() {
    let blocked = view("one", true);
    let mut notice = recovery_for(&blocked, None).notice;
    quota::append_bounded(&mut notice, " short error.");
    assert!(notice.ends_with(" short error."));

    // Multi-byte text, so a byte cut lands inside a character.
    let mut notice = recovery_for(&blocked, None).notice;
    quota::append_bounded(&mut notice, &"é".repeat(5000));
    assert!(
        notice.len() <= quota::NOTICE_BYTES,
        "{} bytes",
        notice.len()
    );
    assert!(notice.ends_with('…'));

    // A notice already at the limit takes nothing and stays valid.
    let mut notice = "x".repeat(quota::NOTICE_BYTES);
    quota::append_bounded(&mut notice, " more");
    assert_eq!(notice.len(), quota::NOTICE_BYTES);
}

fn lifecycle_entry(kind: LifecycleKind) -> ActiveLifecycle {
    let (_completed, result) = tokio::sync::watch::channel(None);
    ActiveLifecycle {
        origin: super::super::lifecycle::LifecycleOrigin::Person,
        acceptance: tokio::sync::watch::channel(Some(Ok(()))).1,
        move_operation_id: None,
        restart_intent: None,
        upgrade_work: Arc::new(Mutex::new(None)),
        phase: LifecyclePhase::Executing,
        operation_id: "operation".into(),
        control: LifecycleControl::default(),
        kind,
        started_at_epoch_seconds: 1,
        active_stages: BTreeMap::new(),
        progress: LifecycleProgress::new(),
        resume_workspace_id: None,
        resume_destination: None,
        notice: None,
        request_key: None,
        _move_guard: None,
        result,
    }
}

// The tick decides whether to keep a timed quota record from one `claim`, so a
// Move that fails between two reads cannot make a free session look owned.
// Each answer below is one reading of the lifecycle entry, the Move owner and
// the setting; `allowed` is exactly the `Allowed` answer.
#[tokio::test]
async fn claim_reads_the_session_owner_once() {
    let state = super::super::tests::test_runtime_state();
    let session = "claim-session";
    assert_eq!(
        claim(&state, session),
        Claim::Blocked,
        "a session the controller does not know is not claimable"
    );
    state.owner().install_controller(Controller {
        config: Config::default(),
        state: {
            let mut records = mj_core::state::State::default();
            records.sessions.insert(
                session.into(),
                mj_controller::controller::test_support::checkpoint_test_session(session),
            );
            records
        },
    });
    assert_eq!(claim(&state, session), Claim::Allowed);
    assert!(allowed(&state, session));

    state
        .owner()
        .lifecycle
        .insert(session.into(), lifecycle_entry(LifecycleKind::Resume));
    assert_eq!(claim(&state, session), Claim::Blocked);
    assert!(!allowed(&state, session));

    state
        .owner()
        .lifecycle
        .insert(session.into(), lifecycle_entry(LifecycleKind::Move));
    assert_eq!(claim(&state, session), Claim::Move);
    assert!(!allowed(&state, session));
    state.owner().lifecycle.remove(session);

    // The Move owner alone, before its lifecycle entry exists or after the
    // entry is gone, is still a Move.
    let guard =
        mj_controller::controller::move_session::MoveMutationGuard::reserve(session).unwrap();
    assert_eq!(claim(&state, session), Claim::Move);
    drop(guard);
    assert_eq!(claim(&state, session), Claim::Allowed);
}

// Hard-won: #1487: quota recovery queried providers again and could lose model-scoped poller data.
#[tokio::test]
async fn quota_recovery_reads_full_owner_reports_without_requesting_another_probe() {
    use mj_controller::controller::test_support::{IsolatedTest, test_name};
    use mj_controller::quota::{ProfileQuota, QuotaRefreshRequest, QuotaWindow};
    use mj_core::config::{Config, HarnessKind, HarnessProfile};

    const CHILD: &str = "MJ_TEST_QUOTA_RECOVERY_OWNER";
    const NAME: &str = "quota_recovery_reads_full_owner_reports_without_requesting_another_probe";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), NAME))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "quota-recovery-1487")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let home = tempfile::tempdir().unwrap();
    let profile = HarnessProfile::new(HarnessKind::Claude, home.path().to_path_buf());
    let mut config = Config::default();
    config.profiles.insert("claude".into(), profile.clone());
    let mut session = super::super::tests::runtime_test_session(
        "one",
        "workspace",
        mj_core::state::SessionState::Running,
    );
    session.last_profile = "claude".into();
    session.harness_kind = HarnessKind::Claude;
    let mut controller = Controller {
        config: config.clone(),
        state: Default::default(),
    };
    controller.state.sessions.insert("one".into(), session);
    let state = super::super::tests::test_runtime_state_with(controller);
    let now = mj_core::clock::epoch_seconds() as i64;
    let reset = now + 2 * 3600;
    let window = |label: &str, scope: Option<&str>, remaining, reset| QuotaWindow {
        label: label.into(),
        model_scope: scope.map(str::to_owned),
        remaining_percent: Some(remaining),
        resets_at_epoch_seconds: Some(reset),
        resets: None,
        duration_seconds: Some(5 * 3600),
        used: None,
        limit: None,
    };
    let stored = ProfileQuota {
        profile_id: "claude".into(),
        harness: HarnessKind::Claude,
        windows: vec![window("5H", None, 70, now + 3 * 3600)],
        paid_credits: None,
        monthly_price_usd: None,
        banked_resets: None,
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: now as u64,
        rate_limited_until_epoch_seconds: None,
        next_check_at_epoch_seconds: None,
    };
    let request = QuotaRefreshRequest::for_profile("claude", &profile, home.path().to_path_buf());
    mj_controller::database::save_quota_report(&request.cache_identity(), &stored).unwrap();
    let reports = Arc::new(Mutex::new(mj_controller::quota::QuotaReportMap::new()));
    let (publications, quota_feed) =
        tokio::sync::watch::channel(mj_controller::quota::QuotaReportMap::new());
    let catalog = mj_controller::profile_catalog::ProfileCatalog::with_probe(
        mj_controller::profile_catalog::counting_probe(Arc::new(
            std::sync::atomic::AtomicUsize::new(0),
        )),
    );
    catalog.sync(&config);
    let (refresh, mut refreshes) = mpsc::unbounded_channel();
    let backend = Arc::new(
        crate::server_runtime::api::ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            state.clone(),
        )
        .with_quota_reports(reports.clone())
        .with_quota_publications(quota_feed)
        .with_quota_refresh(refresh.clone())
        .with_profile_catalog(catalog),
    );
    state.install_report_backend(&backend);
    state.attach_quota_refresh(refresh);
    let mut blocked = view("one", true);
    let snapshot = blocked.snapshot.as_mut().unwrap();
    snapshot
        .operational
        .config
        .insert("model".into(), "claude-opus".into());
    snapshot
        .materialized
        .last_turn_outcome
        .as_mut()
        .unwrap()
        .completed_at_ms = now * 1000;
    // The event can arrive after the poll: a retained positive percentage
    // still supplies its model's reset time without a synchronous recheck.
    for remaining in [0, 17] {
        let mut observed = stored.clone();
        observed.windows = vec![
            window("5H (Opus)", Some("opus"), remaining, reset),
            window("5H (Sonnet)", Some("sonnet"), 0, now + 1800),
        ];
        assert!(
            mj_controller::quota::surface_quota_report(&observed)
                .windows
                .is_empty()
        );
        reports.lock().unwrap().insert(
            "claude".into(),
            mj_controller::quota::QuotaReportPublication {
                login_error: None,
                cache_identity: request.cache_identity(),
                report: Some(observed),
            },
        );
        publications.send_replace(reports.lock().unwrap().clone());
        let quota::Plan::Wait(recovery) = quota::prepare(&state, "one", &blocked).await.unwrap()
        else {
            panic!("a short scoped reset must wait on its original profile");
        };
        assert_eq!(recovery.reset_at_ms, Some(reset * 1000));
        assert_eq!(recovery.retry_at_ms, Some((reset + 60) * 1000));
        assert!(
            recovery
                .notice
                .contains("using last-known quota reset times")
        );
        assert!(
            refreshes.try_recv().is_err(),
            "recovery must not request a probe"
        );
    }
    // A missing owner report keeps the existing reset-cache recovery path.
    reports.lock().unwrap().clear();
    publications.send_replace(reports.lock().unwrap().clone());
    let quota::Plan::Wait(recovery) = quota::prepare(&state, "one", &blocked).await.unwrap() else {
        panic!("stored short reset must remain recoverable");
    };
    assert_eq!(recovery.reset_at_ms, Some((now + 3 * 3600) * 1000));
    assert!(
        recovery
            .notice
            .contains("using last-known quota reset times")
    );
    while let Ok(intent) = refreshes.try_recv() {
        assert_eq!(intent, super::super::QuotaRefreshIntent::ReconcileSources);
    }
}

// Hard-won: #1487: a first quota check could strand recovery, and waiting for it must not hold daemon handoff.
#[tokio::test]
async fn quota_owner_wait_rechecks_identity_and_turn_without_holding_handoff() {
    use futures::FutureExt;
    use mj_controller::controller::test_support::{IsolatedTest, test_name};
    use mj_controller::profile_catalog::{ProfileCatalog, counting_probe};
    use mj_controller::quota::{
        ProfileQuota, QuotaRefreshRequest, QuotaReportMap, QuotaReportPublication, QuotaWindow,
    };
    use mj_core::config::{HarnessKind, HarnessProfile};

    async fn observed<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("isolated continuation transition")
    }

    const CHILD: &str = "MJ_TEST_QUOTA_OWNER_WAIT";
    const NAME: &str = "quota_owner_wait_rechecks_identity_and_turn_without_holding_handoff";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), NAME))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "quota-owner-wait-1487")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    for case in [
        "ready",
        "assessment-reconnect",
        "waiting-reconnect",
        "permission-reenabled",
        "failure",
        "identity",
        "input",
        "handoff",
        "owner-stopped",
        "complete",
    ] {
        let home = tempfile::tempdir().unwrap();
        let profile = HarnessProfile::new(HarnessKind::Claude, home.path());
        let mut config = Config::default();
        config.profiles.clear();
        config.profiles.insert("claude".into(), profile.clone());
        let mut session = super::super::tests::runtime_test_session(
            "one",
            "workspace",
            mj_core::state::SessionState::Running,
        );
        session.last_profile = "claude".into();
        session.harness_kind = HarnessKind::Claude;
        let mut controller = Controller {
            config: config.clone(),
            state: Default::default(),
        };
        controller.state.sessions.insert("one".into(), session);
        let state = super::super::tests::test_runtime_state_with(controller);
        let identity_for = |config: &Config| {
            QuotaRefreshRequest::for_profile(
                "claude",
                &config.profiles["claude"],
                home.path().to_path_buf(),
            )
            .cache_identity()
        };
        let mut identity = identity_for(&config);
        let reports = Arc::new(Mutex::new(QuotaReportMap::from([(
            "claude".into(),
            QuotaReportPublication {
                login_error: None,
                cache_identity: identity.clone(),
                report: None,
            },
        )])));
        let (publications, quota_feed) =
            tokio::sync::watch::channel(reports.lock().unwrap().clone());
        let catalog = ProfileCatalog::with_probe(counting_probe(Arc::new(
            std::sync::atomic::AtomicUsize::new(0),
        )));
        catalog.sync(&config);
        let (refresh, mut refreshes) = mpsc::unbounded_channel();
        let backend = Arc::new(
            crate::server_runtime::api::ApiBackend::new(
                state.session_manager.client(),
                Arc::new(|_| None),
                state.clone(),
            )
            .with_quota_reports(reports.clone())
            .with_quota_publications(quota_feed)
            .with_quota_refresh(refresh)
            .with_profile_catalog(catalog.clone()),
        );
        state.install_report_backend(&backend);
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let (prepared, mut preparations) = mpsc::unbounded_channel();
        let permitted = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut environment = Environment {
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move") })
            }),
            quota: {
                let state = state.clone();
                Arc::new(move |id, view| {
                    let state = state.clone();
                    let prepared = prepared.clone();
                    Box::pin(async move {
                        let result = quota::prepare(&state, &id, &view).await;
                        prepared
                            .send(
                                result
                                    .as_ref()
                                    .is_ok_and(|plan| matches!(plan, quota::Plan::Pending(_))),
                            )
                            .unwrap();
                        result
                    })
                })
            },
            profile: Arc::new(|_| Some("claude".into())),
            log: None,
            control: remote.control.clone(),
            allowed: {
                let permitted = permitted.clone();
                Arc::new(move |_| permitted.load(Ordering::Acquire))
            },
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
        };
        let gate = Arc::new(mj_controller::upgrade::Gate::default());
        let mut cancellation = CancellationToken::new();
        let (mut updates, mut task) = spawn_with_gate(
            environment.clone(),
            remote.updates,
            cancellation.clone(),
            gate.clone(),
        );
        let mut blocked = view("one", true);
        quota_assessed(&mut blocked);
        if case == "complete" {
            // A reply the worker found finished is left untouched.
            let assessment = std::sync::Arc::make_mut(
                blocked
                    .snapshot
                    .as_mut()
                    .unwrap()
                    .operational
                    .assessment
                    .as_mut()
                    .unwrap(),
            );
            assessment.action = Some(mj_core::assessment::Action::Finished);
            assessment.status = mj_core::assessment::Status::Assessed;
        }
        if case == "assessment-reconnect" {
            let mut disconnected = blocked.clone();
            disconnected.connected = false;
            remote
                .publisher
                .publish("one".into(), disconnected)
                .await
                .unwrap();
            receive(&mut updates).await;
            assert!(preparations.try_recv().is_err());
        }
        // Initial attachment alone must rediscover the durable assessment.
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        receive(&mut updates).await;
        if case != "complete" {
            receive(&mut updates).await;
        }
        if case == "complete" {
            assert!(
                preparations.try_recv().is_err(),
                "a completed non-quota reply must remain untouched"
            );
            assert!(remote.requests.recv().now_or_never().is_none());
            // The manager suppresses byte-identical views; advance only the
            // relay frontier so this is a new publication of the same turn.
            let mut same_turn = blocked;
            same_turn
                .snapshot
                .as_mut()
                .unwrap()
                .operational
                .latest_ordinal += 1;
            remote
                .publisher
                .publish("one".into(), same_turn)
                .await
                .unwrap();
            receive(&mut updates).await;
            assert!(
                preparations.try_recv().is_err(),
                "a finished reply is never prepared for quota recovery"
            );
            cancellation.cancel();
            task.await.unwrap().unwrap();
            remote.shutdown.shutdown().await.unwrap();
            continue;
        }
        assert!(observed(preparations.recv()).await.unwrap());
        assert!(
            gate.active_labels().is_empty(),
            "a quota publication wait must hold no admission: {case}"
        );
        assert!(
            remote.requests.recv().now_or_never().is_none(),
            "no terminal no-retry record before the owner reports"
        );
        if case == "owner-stopped" {
            // Keeping ApiBackend alive must not keep its publication owner
            // alive: consumers hold receivers, never the owner's sender.
            drop(publications);
            receive(&mut updates).await;
            assert!(remote.requests.recv().now_or_never().is_none());
            assert!(preparations.try_recv().is_err());
            cancellation.cancel();
            task.await.unwrap().unwrap();
            remote.shutdown.shutdown().await.unwrap();
            continue;
        }
        if case == "permission-reenabled" {
            permitted.store(false, Ordering::Release);
            receive(&mut updates).await;
            assert!(preparations.try_recv().is_err());
            permitted.store(true, Ordering::Release);
            assert!(
                observed(preparations.recv()).await.unwrap(),
                "restoring permission must reconsider the same completion without a worker publication"
            );
            receive(&mut updates).await;
        }
        let reset = mj_core::clock::epoch_seconds() as i64 + 3600;
        let report = ProfileQuota {
            profile_id: "claude".into(),
            harness: HarnessKind::Claude,
            windows: vec![QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(0),
                duration_seconds: Some(5 * 3600),
                resets: None,
                resets_at_epoch_seconds: Some(reset),
                model_scope: None,
                used: None,
                limit: None,
            }],
            paid_credits: None,
            monthly_price_usd: None,
            banked_resets: None,
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: mj_core::clock::epoch_seconds(),
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        };
        let publish_report = |identity: &str, report: ProfileQuota| {
            let mut reports = reports.lock().unwrap();
            reports.insert(
                "claude".into(),
                QuotaReportPublication {
                    login_error: None,
                    cache_identity: identity.into(),
                    report: Some(report),
                },
            );
            publications.send_replace(reports.clone());
        };
        if case == "input" {
            let mut input = view("one", false);
            input
                .snapshot
                .as_mut()
                .unwrap()
                .operational
                .continuation
                .user_command_id = Some("new-user-request".into());
            remote.publisher.publish("one".into(), input).await.unwrap();
            receive(&mut updates).await;
            publish_report(&identity, report);
            cancellation.cancel();
            task.await.unwrap().unwrap();
            assert!(
                remote.requests.recv().now_or_never().is_none(),
                "user input must cancel the old quota wait"
            );
            remote.shutdown.shutdown().await.unwrap();
            continue;
        }
        if case == "waiting-reconnect" {
            let mut disconnected = blocked.clone();
            disconnected.connected = false;
            remote
                .publisher
                .publish("one".into(), disconnected)
                .await
                .unwrap();
            receive(&mut updates).await;
            // The owner can finish while no worker is connected; no active
            // waiter or additional provider check is needed to preserve it.
            publish_report(&identity, report.clone());
            assert!(preparations.try_recv().is_err());
            assert!(remote.requests.recv().now_or_never().is_none());
            let mut reconnected = blocked.clone();
            reconnected
                .snapshot
                .as_mut()
                .unwrap()
                .operational
                .background_commands
                .push(mj_core::relay::BackgroundCommand {
                    id: "still-running".into(),
                    started_at_ms: 1,
                    command: "sleep infinity".into(),
                    can_stop: true,
                });
            remote
                .publisher
                .publish("one".into(), reconnected.clone())
                .await
                .unwrap();
            receive(&mut updates).await;
            // A new publication with the same turn/evidence must join the
            // resumed decision, rather than start another one.
            reconnected
                .snapshot
                .as_mut()
                .unwrap()
                .operational
                .latest_ordinal += 1;
            remote
                .publisher
                .publish("one".into(), reconnected)
                .await
                .unwrap();
            receive(&mut updates).await;
        }
        if case == "identity" {
            config.profiles.get_mut("claude").unwrap().home = home.path().join("other-account");
            let sessions = state.owner().controller().state.clone();
            state.owner().install_controller(Controller {
                config: config.clone(),
                state: sessions,
            });
            catalog.sync(&config);
            publish_report(&identity, report.clone());
            assert!(
                observed(preparations.recv()).await.unwrap(),
                "old-account report must leave current account pending"
            );
            receive(&mut updates).await;
            assert!(remote.requests.recv().now_or_never().is_none());
            identity = identity_for(&config);
        }
        if case == "handoff" {
            let blocker = gate.enter("bounded daemon control").unwrap();
            assert!(!gate.try_close(), "the control operation holds admission");
            // Draining cancels the restartable watch, not the durable turn.
            receive(&mut updates).await;
            drop(blocker);
            assert!(gate.try_close(), "the quota wait must not hold handoff");
            cancellation.cancel();
            task.await.unwrap().unwrap();
            remote.shutdown.shutdown().await.unwrap();
            remote = spawn_remote_session_manager().unwrap();
            remote.targets.send_replace(vec![RelaySessionTarget {
                session_id: "one".into(),
                spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
                worker_recovery: None,
                project_memory: None,
            }]);
            environment.control = remote.control.clone();
            cancellation = CancellationToken::new();
            (updates, task) = spawn_with_gate(
                environment,
                remote.updates,
                cancellation.clone(),
                Arc::new(mj_controller::upgrade::Gate::default()),
            );
            remote
                .publisher
                .publish("one".into(), blocked)
                .await
                .unwrap();
            receive(&mut updates).await;
            receive(&mut updates).await;
            assert!(
                observed(preparations.recv()).await.unwrap(),
                "new daemon must recover the pending check from its durable turn"
            );
        }
        let mut report = report;
        if case == "failure" {
            report.windows.clear();
            report.error = Some("provider unavailable".into());
        }
        publish_report(&identity, report);
        while let Ok(intent) = refreshes.try_recv() {
            assert_eq!(intent, super::super::QuotaRefreshIntent::ReconcileSources);
        }
        assert!(
            !observed(preparations.recv()).await.unwrap(),
            "a matching completed check must end the wait"
        );
        let request = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap();
        let RemoteSessionRequest::Submit { command, reply, .. } = request else {
            panic!("expected quota scheduling")
        };
        let RelayCommand::SetQuotaRecovery {
            recovery: Some(recovery),
            ..
        } = command
        else {
            panic!("expected recovery record")
        };
        if case == "failure" {
            assert_eq!(recovery.retry_at_ms, None);
            assert!(recovery.notice.contains("No reliable reset time"));
        } else {
            assert_eq!(recovery.reset_at_ms, Some(reset * 1000));
            assert_eq!(recovery.retry_at_ms, Some((reset + 60) * 1000));
        }
        if case == "waiting-reconnect" {
            assert!(
                preparations.try_recv().is_err(),
                "reconnect prepares the quota decision once"
            );
        }
        reply
            .send(Err("end isolated scheduling probe".into()))
            .unwrap();
        receive(&mut updates).await;
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}

// Hard-won: #1562: refused quota scheduling spun again on every policy tick.
#[tokio::test(start_paused = true)]
async fn refused_quota_scheduling_waits_for_backoff_or_a_new_assessment() {
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let gate = Arc::new(mj_controller::recovery_gate::RecoveryGate::default());
    let holder = gate
        .try_start(
            "one",
            mj_controller::recovery_gate::BackgroundWork::WorkerReplacement,
        )
        .unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let environment = Environment {
        session_gate: gate,
        schedules: Default::default(),
        log: None,
        control: remote.control,
        allowed: Arc::new(|_| true),
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        start_quota_move: Arc::new(|_, _, _| Box::pin(async { bail!("unexpected Move") })),
        quota: {
            let calls = calls.clone();
            Arc::new(move |_, view| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(quota::Plan::Wait(recovery_for(&view, None))) })
            })
        },
    };
    let cancellation = CancellationToken::new();
    let (_updates, task) = spawn_with_gate(
        environment,
        remote.updates,
        cancellation.clone(),
        Arc::new(mj_controller::upgrade::Gate::default()),
    );
    let mut blocked = view("one", true);
    quota_assessed(&mut blocked);
    remote
        .publisher
        .publish("one".into(), blocked.clone())
        .await
        .unwrap();
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(29)).await;
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "ticks cannot consume the backoff"
    );
    // A genuinely new assessment may reconsider immediately.
    Arc::make_mut(
        blocked
            .snapshot
            .as_mut()
            .unwrap()
            .operational
            .assessment
            .as_mut()
            .unwrap(),
    )
    .revision += 1;
    remote
        .publisher
        .publish("one".into(), blocked)
        .await
        .unwrap();
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(holder);
    tokio::time::advance(Duration::from_secs(29)).await;
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    let request = remote.requests.recv().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let RemoteSessionRequest::Submit { reply, .. } = request else {
        panic!("expected admitted schedule")
    };
    reply.send(Err("end scheduling probe".into())).unwrap();
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

#[cfg(unix)]
// Hard-won: #1562: the outer quota deadline expired while the worker was still accepting a command.
#[tokio::test]
async fn slow_quota_scheduling_finishes_and_yields_to_a_persons_move() {
    use mj_controller::controller::test_support::{IsolatedTest, test_name};
    use mj_controller::session_manager::stand_in::{
        LEASED_RELAY_SESSION as ID, leased_relay_target, register_leased_relay_session,
    };
    const CHILD: &str = "MJ_TEST_SLOW_QUOTA_ADMISSION";
    if std::env::var_os(CHILD).is_none() {
        for mode in ["complete", "move"] {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "slow_quota_scheduling_finishes_and_yields_to_a_persons_move",
            ))
            .env(CHILD, mode)
            .isolated_store(directory.path())
            .run();
        }
        return;
    }
    let mode = std::env::var(CHILD).unwrap();
    // The relay this test starts reports to its stderr, which the client
    // logs at debug and nothing else shows; a failure below prints it.
    let log = mj_controller::test_log::CapturedLog::default();
    tracing::subscriber::set_global_default(log.clone()).expect("isolated test log subscriber");
    let _writer = mj_controller::database::install_isolated_test_writer();
    register_leased_relay_session();
    let root = tempfile::tempdir().unwrap();
    let mut relay = mj_worker::relay::DurableRelay::open(root.path(), ID, "test").unwrap();
    relay
        .record_observation(mj_core::relay::RelayObservation::SessionOpened {
            native_session_id: "quota-fixture".into(),
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
            resumed: false,
        })
        .unwrap();
    relay
        .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
            config_options: vec![],
        })
        .unwrap();
    let accepted = relay.handle(mj_core::relay::RelayRequestEnvelope {
        request_id: "prompt".into(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: mj_core::relay::RelayRequest::Submit {
            command_id: "quota-request".into(),
            command: RelayCommand::Prompt {
                prompt: mj_core::continuation::prompt_blocks(),
            },
        },
    });
    assert!(
        matches!(accepted.body, mj_core::relay::RelayResponseBody::Ok { .. }),
        "{accepted:?}"
    );
    relay.claim_pending_commands(true).unwrap();
    relay
        .record_command_completed(
            "quota-request",
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
    drop(relay);
    let manager = mj_controller::session_manager::spawn_session_manager().unwrap();
    manager
        .targets
        .send_replace(vec![leased_relay_target(root.path())]);
    let handle = manager
        .control
        .wait_for_session(ID, Duration::from_secs(5))
        .await
        .unwrap();
    handle.sync_now().await.unwrap();
    let blocked = handle.view();
    let session_gate = Arc::new(mj_controller::recovery_gate::RecoveryGate::default());
    let environment = Environment {
        session_gate: session_gate.clone(),
        schedules: Default::default(),
        log: None,
        control: manager.control.clone(),
        allowed: Arc::new(|_| true),
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&[ID])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(|_, _| Box::pin(async { bail!("unused resolver") })),
        start_quota_move: Arc::new(|_, _, _| {
            Box::pin(async { bail!("unexpected automatic Move") })
        }),
    };
    std::fs::write(root.path().join("block-quota-submit"), b"hold").unwrap();
    let recovery = recovery_for(&blocked, None);
    let scheduling = mj_core::runtime::spawn(async move {
        quota::schedule(&environment, ID, blocked, quota::Plan::Wait(recovery)).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("quota-submit-waiting").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real relay reached the held quota exchange");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(16)).await;
    assert!(
        !scheduling.is_finished(),
        "transport queue time must not expire the whole schedule"
    );
    tokio::time::resume();
    let moving = (mode == "move").then(|| {
        mj_core::runtime::spawn({
            let gate = session_gate.clone();
            let handle = handle.clone();
            async move {
                let admission = gate
                    .enter_foreground(
                        ID,
                        "Move",
                        mj_controller::recovery_gate::LifecycleOrigin::Person,
                        &std::sync::atomic::AtomicBool::new(false),
                        &mj_controller::targets::ProcessExecutor,
                    )
                    .await
                    .unwrap();
                let lease = handle.lease_connection(&admission).await.unwrap();
                lease.release();
            }
        })
    });
    if moving.is_some() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }
    std::fs::remove_file(root.path().join("block-quota-submit")).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), scheduling)
        .await
        .unwrap()
        .unwrap();
    if let Some(moving) = moving {
        assert!(
            result.is_err(),
            "a person's foreground Move preempts scheduling"
        );
        tokio::time::timeout(Duration::from_secs(5), moving)
            .await
            .unwrap()
            .unwrap();
    } else {
        let scheduled = result.unwrap_or_else(|error| {
            panic!(
                "quota scheduling failed: {error:#}\nlogged:\n{}",
                log.at_or_above(tracing::Level::DEBUG).join("\n")
            )
        });
        assert!(
            scheduled
                .snapshot
                .unwrap()
                .operational
                .continuation
                .quota_recovery
                .is_some()
        );
    }
    manager.shutdown.shutdown().await.unwrap();
}

// Hard-won: #1562: a new assessment changed the payload of an uncertain quota command.
#[tokio::test]
async fn quota_schedule_keeps_the_identical_command_until_acceptance_is_known() {
    use futures::FutureExt;
    for phase in ["primary", "fallback"] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let environment = Environment {
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            log: None,
            control: remote.control,
            allowed: Arc::new(|_| true),
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: Arc::new(|_| Some("test".into())),
            quota: Arc::new(|_, _| Box::pin(async { bail!("unused resolver") })),
            start_quota_move: Arc::new(|_, _, _| {
                Box::pin(async { bail!("Move admission refused") })
            }),
        };
        let mut blocked = view("one", true);
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        let first = mj_core::runtime::spawn({
            let env = environment.clone();
            let view = blocked.clone();
            async move {
                let recovery = recovery_for(&view, None);
                let plan = if phase == "primary" {
                    quota::Plan::Wait(recovery)
                } else {
                    let mut abstention = recovery.clone();
                    abstention.notice =
                        quota::move_notice("quota exhausted", "destination", &recovery);
                    quota::Plan::Move {
                        request: Box::new(quota_move_request()),
                        destination: "destination".into(),
                        reason: "quota exhausted".into(),
                        abstention,
                        fallback: recovery,
                    }
                };
                quota::schedule(&env, "one", view, plan).await
            }
        });
        let RemoteSessionRequest::Submit {
            command_id,
            command,
            reply,
            ..
        } = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("first schedule")
        };
        let (command_id, command, reply) = if phase == "fallback" {
            if let RelayCommand::SetQuotaRecovery {
                recovery: Some(record),
                ..
            } = &command
            {
                blocked
                    .snapshot
                    .as_mut()
                    .unwrap()
                    .operational
                    .continuation
                    .quota_recovery = Some((**record).clone());
            }
            remote
                .publisher
                .publish("one".into(), blocked.clone())
                .await
                .unwrap();
            reply.send(Ok(3)).unwrap();
            let RemoteSessionRequest::Sync { reply, .. } =
                tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("primary schedule sync")
            };
            reply.send(Ok(())).unwrap();
            let RemoteSessionRequest::Submit {
                command_id,
                command,
                reply,
                ..
            } = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap()
            else {
                panic!("fallback to reconcile")
            };
            (command_id, command, reply)
        } else {
            (command_id, command, reply)
        };
        reply
            .send(Err(mj_client::session::SubmitFailure {
                message: "lost acknowledgement".into(),
                unconfirmed: true,
                refused: false,
            }))
            .unwrap();
        assert!(first.await.unwrap().is_err());
        let snapshot = blocked.snapshot.as_mut().unwrap();
        snapshot
            .materialized
            .last_turn_outcome
            .as_mut()
            .unwrap()
            .completed_ordinal += 1;
        snapshot
            .materialized
            .last_turn_outcome
            .as_mut()
            .unwrap()
            .command_id = "next-request".into();
        snapshot.operational.continuation.completed_command_id = Some("next-request".into());
        snapshot.operational.continuation.quota_recovery = None;

        blocked
            .snapshot
            .as_mut()
            .unwrap()
            .operational
            .latest_ordinal += 1;
        blocked.snapshot.as_mut().unwrap().operational.latest_digest = "new evidence".into();
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        let second = mj_core::runtime::spawn({
            let env = environment.clone();
            let view = blocked.clone();
            async move {
                let mut recovery = recovery_for(&view, None);
                recovery.notice = "new assessment notice".into();
                quota::schedule(&env, "one", view, quota::Plan::Wait(recovery)).await
            }
        });
        let RemoteSessionRequest::Submit {
            command_id: retried_id,
            command: retried,
            reply,
            ..
        } = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("reconciliation")
        };
        assert_eq!(retried_id, command_id);
        assert_eq!(
            retried, command,
            "unknown acceptance must retain the original cursor and notice"
        );
        if phase == "fallback" {
            reply
                .send(Err(mj_client::session::SubmitFailure {
                    message: "old fallback evidence is stale".into(),
                    unconfirmed: false,
                    refused: true,
                }))
                .unwrap();
        } else {
            reply.send(Ok(4)).unwrap();
            let RemoteSessionRequest::Sync { reply, .. } =
                tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("acceptance sync")
            };
            reply.send(Ok(())).unwrap();
        }
        assert!(
            second.await.unwrap().is_err(),
            "reconciling the earlier completion must leave the newer one retryable"
        );
        let third = mj_core::runtime::spawn({
            let env = environment.clone();
            let view = blocked.clone();
            async move {
                let recovery = recovery_for(&view, None);
                quota::schedule(&env, "one", view, quota::Plan::Wait(recovery)).await
            }
        });
        let RemoteSessionRequest::Submit {
            command_id: next_id,
            command: next,
            reply,
            ..
        } = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("new completion after reconciliation")
        };
        assert_ne!(next_id, command_id);
        assert_ne!(next, command);
        reply.send(Ok(5)).unwrap();
        let RemoteSessionRequest::Sync { reply, .. } =
            tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("new acceptance sync")
        };
        reply.send(Ok(())).unwrap();
        third.await.unwrap().unwrap();
        let recovery = recovery_for(&blocked, None);
        quota::schedule(&environment, "one", blocked, quota::Plan::Wait(recovery))
            .await
            .unwrap();
        assert!(
            remote.requests.recv().now_or_never().is_none(),
            "an answered completion must not resubmit"
        );
        remote.shutdown.shutdown().await.unwrap();
    }
}

// Hard-won: f7b3aab75: accepting the worker record discarded the unfinished Move after personal preemption.
#[tokio::test]
async fn personal_release_reassesses_quota_without_replaying_the_old_move() {
    use futures::FutureExt;
    for case in ["fresh-move", "fresh-wait", "person-move"] {
        let moved_profile = case == "person-move";
        let wait_after = case == "fresh-wait";
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let session_gate = Arc::new(mj_controller::recovery_gate::RecoveryGate::default());
        let profile = Arc::new(std::sync::Mutex::new("test".to_owned()));
        let preparations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let moves = Arc::new(std::sync::Mutex::new(Vec::new()));
        let launched = Arc::new(tokio::sync::Notify::new());
        let environment = Environment {
            session_gate: session_gate.clone(),
            schedules: Default::default(),
            log: None,
            control: remote.control,
            allowed: Arc::new(|_| true),
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: {
                let profile = profile.clone();
                Arc::new(move |_| Some(profile.lock().unwrap().clone()))
            },
            quota: {
                let preparations = preparations.clone();
                Arc::new(move |_, view| {
                    let ordinal = preparations.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        let destination = if ordinal == 0 {
                            "old-automatic"
                        } else {
                            "fresh-automatic"
                        };
                        let mut request = quota_move_request();
                        request.preparation.selection.profile_id = Some(destination.into());
                        let fallback = recovery_for(
                            &view,
                            if ordinal == 0 {
                                Some(mj_core::clock::epoch_millis() + 3_600_000)
                            } else {
                                None
                            },
                        );
                        if ordinal > 0 && wait_after {
                            return Ok(quota::Plan::Wait(fallback));
                        }
                        let mut abstention = fallback.clone();
                        abstention.notice =
                            quota::move_notice("quota exhausted", destination, &fallback);
                        Ok(quota::Plan::Move {
                            request: Box::new(request),
                            destination: destination.into(),
                            reason: "quota exhausted".into(),
                            abstention,
                            fallback,
                        })
                    })
                })
            },
            start_quota_move: {
                let moves = moves.clone();
                let launched = launched.clone();
                Arc::new(move |request, _, _| {
                    moves
                        .lock()
                        .unwrap()
                        .push(request.preparation.selection.profile_id.unwrap());
                    launched.notify_one();
                    Box::pin(async { Ok(()) })
                })
            },
        };
        let cancellation = CancellationToken::new();
        let (_updates, task) = spawn_with_gate(
            environment.clone(),
            remote.updates,
            cancellation.clone(),
            Arc::new(mj_controller::upgrade::Gate::default()),
        );
        let mut blocked = view("one", true);
        quota_assessed(&mut blocked);
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        let RemoteSessionRequest::Submit { command, reply, .. } =
            tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("initial quota schedule")
        };
        let RelayCommand::SetQuotaRecovery { recovery, .. } = command else {
            panic!("quota record")
        };
        blocked
            .snapshot
            .as_mut()
            .unwrap()
            .operational
            .continuation
            .quota_recovery = recovery.map(|r| *r);
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let mut personal = Box::pin(session_gate.enter_foreground(
            "one",
            "checkpoint",
            mj_controller::recovery_gate::LifecycleOrigin::Person,
            &cancelled,
            &mj_controller::targets::ProcessExecutor,
        ));
        assert!(futures::poll!(&mut personal).is_pending());
        reply.send(Ok(4)).unwrap();
        let RemoteSessionRequest::Sync { reply, .. } =
            tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("accepted quota sync")
        };
        reply.send(Ok(())).unwrap();
        let personal = tokio::time::timeout(Duration::from_secs(3), personal)
            .await
            .unwrap()
            .unwrap();
        assert!(
            moves.lock().unwrap().is_empty(),
            "the preempted plan cannot launch"
        );
        if moved_profile {
            *profile.lock().unwrap() = "person-destination".into();
            let snapshot = blocked.snapshot.as_mut().unwrap();
            snapshot.operational.continuation.quota_recovery = None;
            let assessment = Arc::make_mut(snapshot.operational.assessment.as_mut().unwrap());
            assessment.action = Some(mj_core::assessment::Action::Finished);
            assessment.status = mj_core::assessment::Status::Assessed;
            remote
                .publisher
                .publish("one".into(), blocked.clone())
                .await
                .unwrap();
        }
        drop(personal);
        if !moved_profile {
            let RemoteSessionRequest::Sync { reply, .. } =
                tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("accepted record stands; only daemon intent is reassessed")
            };
            reply.send(Ok(())).unwrap();
            if wait_after {
                let RemoteSessionRequest::Submit { command, reply, .. } =
                    tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                        .await
                        .unwrap()
                        .unwrap()
                else {
                    panic!("stale Move notice needs its fallback decision")
                };
                let RelayCommand::SetQuotaRecovery {
                    recovery: Some(record),
                    ..
                } = command
                else {
                    panic!("fresh wait fallback")
                };
                assert!(!record.notice.contains("Switching"));
                assert!(
                    record.retry_at_ms.is_some(),
                    "the accepted source continuation survives a daemon-only reassessment"
                );
                reply.send(Ok(5)).unwrap();
                let RemoteSessionRequest::Sync { reply, .. } =
                    tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                        .await
                        .unwrap()
                        .unwrap()
                else {
                    panic!("fallback sync")
                };
                reply.send(Ok(())).unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(3), launched.notified())
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                while quota::unfinished(&environment, "one", &blocked) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                *moves.lock().unwrap(),
                if wait_after {
                    Vec::<String>::new()
                } else {
                    vec!["fresh-automatic".into()]
                }
            );
            assert_eq!(preparations.load(Ordering::SeqCst), 2);
            let automatic = session_gate
                .enter_foreground(
                    "one",
                    "automatic Move",
                    mj_controller::recovery_gate::LifecycleOrigin::Automatic,
                    &cancelled,
                    &mj_controller::targets::ProcessExecutor,
                )
                .await
                .unwrap();
            drop(automatic);
        }
        for _ in 0..128 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            preparations.load(Ordering::SeqCst),
            if moved_profile { 1 } else { 2 }
        );
        assert!(
            !moved_profile || moves.lock().unwrap().is_empty(),
            "a person's destination must never be overridden"
        );
        assert!(
            remote.requests.recv().now_or_never().is_none(),
            "automatic release must not replan a failed automatic Move"
        );
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
}
