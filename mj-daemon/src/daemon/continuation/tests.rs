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

pub(super) fn view(id: &str, completed: bool) -> ManagedSessionView {
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
pub(super) fn quota_assessed(v: &mut ManagedSessionView) {
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
        child_switch_runtime: None,
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        log: None,
        control: remote.control,
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
            child_switch_runtime: None,
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move start") })
            }),
            log: None,
            control: remote.control,
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
        child_switch_runtime: None,
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        log: Some(mj_core::jev::DecisionLog::open(log_dir.path().to_path_buf()).unwrap()),
        control: remote.control,
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
            child_switch_runtime: None,
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move start") })
            }),
            log: Some(log),
            control: remote.control,
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
            child_switch_runtime: None,
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _, _| {
                Box::pin(async { anyhow::bail!("unexpected Move start") })
            }),
            log: None,
            control: remote.control,
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
        child_switch_runtime: None,
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        quota: Arc::new(move |_, _| {
            calls.send(()).unwrap();
            Box::pin(async { anyhow::bail!("end isolated quota probe") })
        }),
        profile: Arc::new(|_| Some("test".into())),
        log: None,
        control: remote.control,
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
        child_switch_runtime: None,
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        start_quota_move: Arc::new(|_, _, _, _| {
            Box::pin(async { anyhow::bail!("unexpected Move start") })
        }),
        log: None,
        control,
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
                mjolnir_guardian: true,
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
            cache_identity: None,
        }
    })
    .collect();
    let telemetry = mj_controller::quota_telemetry::QuotaTelemetrySnapshot::default();
    let power_ranking = mj_controller::power_ranking::PowerRankingSnapshot::baseline();
    let ordered = quota::destinations(
        &config,
        "source",
        HarnessKind::Codex,
        "model",
        candidates,
        1,
        mj_controller::profile_scheduler::Placement::TopLevel,
        Some("primary"),
        &telemetry,
        power_ranking,
    )
    .unwrap();
    let ids: Vec<&str> = ordered.iter().map(|c| c.profile_id.as_str()).collect();
    // Unified tier ladder (legacy top-level active-5h tier retired):
    // both included subscriptions ("healthy", "windowed") lead Tier 0 above
    // "unknown" (Tier 2), "paid" (Tier 4), and "api" (Tier 5).
    assert_eq!(ids.len(), 5);
    assert!(ids[..2].contains(&"healthy") && ids[..2].contains(&"windowed"));
    assert_eq!(&ids[2..], ["unknown", "paid", "api"]);
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

#[test]
fn concurrent_quota_destination_reservations_spill_to_peer_and_drop_releases() {
    use mj_controller::profile_scheduler::{CandidateQuota, Placement, ScheduledCandidate};
    use mj_controller::quota::QuotaWindow;
    use mj_core::config::HarnessKind;

    let store = mj_controller::quota_telemetry::QuotaTelemetryStore::new();
    let now = 1_700_000_000;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let candidate = move |id: &str, remaining_percent| ScheduledCandidate {
        profile_id: id.into(),
        harness: HarnessKind::Codex,
        model: "model".into(),
        effort: None,
        may_consume_credits: false,
        quota: CandidateQuota::Subscription {
            monthly_price_usd: None,
            paid_credits: None,
            refreshed_at_epoch_seconds: now as u64,
            windows: vec![QuotaWindow {
                label: "weekly".into(),
                remaining_percent: Some(remaining_percent),
                resets_at_epoch_seconds: Some(now + 86400),
                duration_seconds: Some(604800),
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
            }],
        },
        cache_identity: Some(id.into()),
    };
    let first_store = store.clone();
    let first_barrier = barrier.clone();
    let first = std::thread::spawn(move || {
        let mut candidates = vec![candidate("nearly-empty", 3)];
        let reservation = first_store
            .draw_and_reserve(
                &mut candidates,
                Placement::TopLevel,
                Some("primary"),
                None,
                &[],
                mj_controller::power_ranking::PowerRankingSnapshot::baseline(),
                now,
            )
            .unwrap()
            .unwrap();
        first_barrier.wait();
        reservation
    });
    let second_store = store.clone();
    let second = std::thread::spawn(move || {
        barrier.wait();
        let mut candidates = vec![candidate("nearly-empty", 3), candidate("peer", 80)];
        let reservation = second_store
            .draw_and_reserve(
                &mut candidates,
                Placement::TopLevel,
                Some("primary"),
                None,
                &[],
                mj_controller::power_ranking::PowerRankingSnapshot::baseline(),
                now,
            )
            .unwrap()
            .unwrap();
        (candidates[0].profile_id.clone(), reservation)
    });
    let first = first.join().unwrap();
    let (second_pick, second_reservation) = second.join().unwrap();
    assert_eq!(second_pick, "peer");
    drop(first);
    assert_eq!(
        store
            .snapshot()
            .recent_placements
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        1
    );
    drop(second_reservation);
    assert_eq!(
        store
            .snapshot()
            .recent_placements
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        0
    );
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
#[test]
fn quota_move_preparation_notices_only_expose_certified_diagnostics() {
    for safety_refusal in [false, true] {
        let mut notice = String::new();
        let error = private_quota_move_error().context(if safety_refusal {
            mj_core::refusal::Refusal::precondition("A sub-agent would be stopped")
                .with_code(mj_controller::controller::move_session::QUOTA_MOVE_SAFETY_REFUSAL)
        } else {
            mj_core::refusal::Refusal::precondition("private refusal quota-secret-canary")
        });
        quota::preparation_refused(
            &mut notice,
            "one",
            "destination",
            &error,
            "prepare-reference",
        );
        assert!(!notice.contains("quota-secret-canary"), "{notice}");
        let expected = if safety_refusal {
            "A sub-agent would be stopped"
        } else {
            "prepare-reference"
        };
        assert!(notice.contains(expected), "{notice}");
    }
}

#[tokio::test]
async fn automatic_move_settles_before_any_wait_and_releases_failed_placement() {
    use futures::FutureExt;
    use mj_controller::profile_scheduler::{CandidateQuota, Placement, ScheduledCandidate};
    use mj_controller::quota::QuotaWindow;
    use mj_core::config::HarnessKind;

    for outcome in [
        "completed",
        "refused",
        "failed",
        "cancelled",
        "unchanged",
        "profile-changed",
    ] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let telemetry = mj_controller::quota_telemetry::QuotaTelemetryStore::new();
        let now = 1_700_000_000;
        let draw = move |store: &mj_controller::quota_telemetry::QuotaTelemetryStore| {
            store
                .draw_and_reserve(
                    &mut vec![ScheduledCandidate {
                        profile_id: "destination".into(),
                        harness: HarnessKind::Codex,
                        model: "model".into(),
                        effort: None,
                        may_consume_credits: false,
                        quota: CandidateQuota::Subscription {
                            monthly_price_usd: None,
                            paid_credits: None,
                            refreshed_at_epoch_seconds: now as u64,
                            windows: vec![QuotaWindow {
                                label: "weekly".into(),
                                remaining_percent: Some(3),
                                resets_at_epoch_seconds: Some(now + 86400),
                                duration_seconds: Some(604800),
                                model_scope: None,
                                used: None,
                                limit: None,
                                resets: None,
                            }],
                        },
                        cache_identity: Some("destination".into()),
                    }],
                    Placement::TopLevel,
                    Some("primary"),
                    None,
                    &[],
                    mj_controller::power_ranking::PowerRankingSnapshot::baseline(),
                    now,
                )
                .unwrap()
        };
        let reservation = draw(&telemetry).expect("eligible destination");
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let finish = Arc::new(tokio::sync::Notify::new());
        let moved = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let plan_reservation = Arc::new(Mutex::new(Some(reservation)));
        let environment = Environment {
            child_switch_runtime: None,
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            log: None,
            control: remote.control,
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: {
                let moved = moved.clone();
                Arc::new(move |_| {
                    Some(
                        if moved.load(Ordering::Acquire) {
                            "person"
                        } else {
                            "test"
                        }
                        .into(),
                    )
                })
            },
            quota: Arc::new(move |_, view| {
                let reservation = plan_reservation
                    .lock()
                    .unwrap()
                    .take()
                    .expect("Move must be planned once");
                Box::pin(async move {
                    Ok(quota::Plan::Move {
                        request: Box::new(quota_move_request()),
                        reason: "the weekly quota is exhausted".into(),
                        destination: "destination".into(),
                        reservation: Some(reservation),
                        fallback: recovery_for(
                            &view,
                            Some(mj_core::clock::epoch_millis() + 3_600_000),
                        ),
                    })
                })
            }),
            start_quota_move: {
                let finish = finish.clone();
                Arc::new(move |request, _, continuation, work| {
                    assert_eq!(
                        continuation,
                        mj_core::state::QuotaMoveContinuation::ResumeGoal
                    );
                    let finish = finish.clone();
                    started.send(()).unwrap();
                    Box::pin(async move {
                        drop(work);
                        finish.notified().await;
                        if outcome == "refused" {
                            return Err(private_quota_move_error());
                        }
                        Ok(mj_core::state::MoveOutcome {
                            operation_id: request.preparation.operation_id,
                            session_id: "one".into(),
                            profile_id: "destination".into(),
                            target_template_id: "local".into(),
                            outcome: (if outcome == "profile-changed" {
                                "cancelled"
                            } else {
                                outcome
                            })
                            .into(),
                            error: Some(format!("{:#}", private_quota_move_error())),
                            recovery: Some("The source remains running.".into()),
                        })
                    })
                })
            },
        };
        let cancellation = CancellationToken::new();
        let (mut updates, task) = spawn_with_gate(
            environment,
            remote.updates,
            cancellation.clone(),
            Arc::new(mj_controller::upgrade::Gate::default()),
        );
        let mut blocked = view("one", true);
        quota_assessed(&mut blocked);
        blocked.snapshot.as_mut().unwrap().operational.goal = serde_json::from_value(json!({
            "known": true,
            "capability": {"version": 1, "controlMethod": "_session/goal", "actions": ["resume"]},
            "snapshot": {"objective": "Finish the interrupted work", "status": "limited", "createdAt": 42,
                "controlMethod": "_session/goal", "limitReason": "usage"},
            "execution": {"version": 1, "status": "idle"}
        })).unwrap();
        remote
            .publisher
            .publish("one".into(), blocked.clone())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), starts.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            remote.requests.recv().now_or_never().is_none(),
            "{outcome}: command preceded Move"
        );
        assert_eq!(
            telemetry.snapshot().recent_placements["destination"].len(),
            1
        );
        if outcome == "profile-changed" {
            moved.store(true, Ordering::Release);
        }
        finish.notify_one();
        if !matches!(outcome, "completed" | "profile-changed") {
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
                panic!("expected ordinary Wait")
            };
            assert!(command_id.starts_with("quota-schedule-"), "{command_id}");
            assert!(
                !telemetry
                    .snapshot()
                    .recent_placements
                    .contains_key("destination"),
                "failed Move retains no capacity while Wait is in flight"
            );
            assert!(
                draw(&telemetry).is_some(),
                "the same destination can be reserved again"
            );
            let RelayCommand::SetQuotaRecovery {
                recovery: Some(recovery),
                ..
            } = command
            else {
                panic!("expected quota recovery")
            };
            assert!(recovery.retry_at_ms.is_some());
            assert!(recovery.notice.contains("quota-move-test"));
            assert_eq!(
                recovery.notice.contains("The source remains running."),
                outcome != "refused"
            );
            for private in ["quota-secret-canary", "/private/profile", "secret-ssh-host"] {
                assert!(!recovery.notice.contains(private), "{outcome}: {private}");
            }
            blocked
                .snapshot
                .as_mut()
                .unwrap()
                .operational
                .continuation
                .quota_recovery = Some(*recovery);
            remote
                .publisher
                .publish("one".into(), blocked)
                .await
                .unwrap();
            reply.send(Ok(4)).unwrap();
            let RemoteSessionRequest::Sync { reply, .. } =
                tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("expected Wait sync")
            };
            reply.send(Ok(())).unwrap();
        }
        // Advance past the planning backoff while the same quota assessment is live.
        receive(&mut updates).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        tokio::time::resume();
        assert!(starts.try_recv().is_err(), "{outcome}: Move restarted");
        assert!(
            remote.requests.recv().now_or_never().is_none(),
            "{outcome}: extra relay request"
        );
        if outcome == "profile-changed" {
            assert!(
                draw(&telemetry).is_some(),
                "cancelled Move retains no capacity"
            );
        }
        cancellation.cancel();
        task.await.unwrap().unwrap();
        remote.shutdown.shutdown().await.unwrap();
    }
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
// Each answer below reads the lifecycle entry, the Move owner, and the setting.
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

    state
        .owner()
        .lifecycle
        .insert(session.into(), lifecycle_entry(LifecycleKind::Resume));
    assert_eq!(claim(&state, session), Claim::Blocked);

    state
        .owner()
        .lifecycle
        .insert(session.into(), lifecycle_entry(LifecycleKind::Move));
    assert_eq!(claim(&state, session), Claim::Move);
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
                last_good: None,
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
                last_good: None,
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
            child_switch_runtime: None,
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            start_quota_move: Arc::new(|_, _, _, _| {
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
            claim: {
                let permitted = permitted.clone();
                Arc::new(move |_| {
                    if permitted.load(Ordering::Acquire) {
                        Claim::Allowed
                    } else {
                        Claim::Blocked
                    }
                })
            },
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
                    last_good: None,
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
        child_switch_runtime: None,
        session_gate: gate,
        schedules: Default::default(),
        log: None,
        control: remote.control,
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        start_quota_move: Arc::new(|_, _, _, _| Box::pin(async { bail!("unexpected Move") })),
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
        child_switch_runtime: None,
        session_gate: session_gate.clone(),
        schedules: Default::default(),
        log: None,
        control: manager.control.clone(),
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&[ID])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(|_, _| Box::pin(async { bail!("unused resolver") })),
        start_quota_move: Arc::new(|_, _, _, _| {
            Box::pin(async { bail!("unexpected automatic Move") })
        }),
    };
    std::fs::write(root.path().join("block-quota-submit"), b"hold").unwrap();
    let recovery = recovery_for(&blocked, None);
    let scheduling = mj_core::runtime::spawn(async move {
        quota::wait(&environment, ID, &blocked, Some(recovery)).await
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
                let lease = handle.lease_connection(&admission).await?;
                lease.release();
                anyhow::Ok(())
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
        if let Err(error) = tokio::time::timeout(Duration::from_secs(5), moving)
            .await
            .unwrap()
            .unwrap()
        {
            panic!(
                "the Move could not lease the connection: {error:#}\nlogged:\n{}",
                log.at_or_above(tracing::Level::DEBUG).join("\n")
            );
        }
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
    for accepted in [true, false] {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![RelaySessionTarget {
            session_id: "one".into(),
            spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }]);
        let environment = Environment {
            child_switch_runtime: None,
            session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
            schedules: Default::default(),
            log: None,
            control: remote.control,
            claim: Arc::new(|_| Claim::Allowed),
            live: Arc::new(|| live_sessions(&["one"])),
            review: Arc::new(|_, _| {}),
            profile: Arc::new(|_| Some("test".into())),
            quota: Arc::new(|_, _| Box::pin(async { bail!("unused resolver") })),
            start_quota_move: Arc::new(|_, _, _, _| {
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
                quota::wait(&env, "one", &view, Some(recovery)).await
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
            async move { quota::wait(&env, "one", &view, None).await }
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
        if !accepted {
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
        second.await.unwrap().unwrap();
        let third = mj_core::runtime::spawn({
            let env = environment.clone();
            let view = blocked.clone();
            async move {
                let recovery = recovery_for(&view, None);
                quota::wait(&env, "one", &view, Some(recovery)).await
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
        quota::wait(&environment, "one", &blocked, Some(recovery))
            .await
            .unwrap();
        assert!(
            remote.requests.recv().now_or_never().is_none(),
            "an answered completion must not resubmit"
        );
        remote.shutdown.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn unanswered_wait_reconciles_even_when_the_snapshot_already_shows_it_is_due() {
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let (prepared, mut plans) = mpsc::unbounded_channel();
    let environment = Environment {
        child_switch_runtime: None,
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        log: None,
        control: remote.control,
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(move |_, view| {
            prepared.send(()).unwrap();
            Box::pin(async move {
                Ok(quota::Plan::Wait(recovery_for(
                    &view,
                    Some(mj_core::clock::epoch_millis() - 1000),
                )))
            })
        }),
        start_quota_move: Arc::new(|_, _, _, _| Box::pin(async { bail!("unexpected Move") })),
    };
    let cancellation = CancellationToken::new();
    let (mut updates, task) = spawn_with_gate(
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
    let RemoteSessionRequest::Submit {
        command_id,
        command,
        reply,
        ..
    } = remote.requests.recv().await.unwrap()
    else {
        panic!("expected Wait")
    };
    plans.recv().await.unwrap();
    let RelayCommand::SetQuotaRecovery {
        recovery: Some(recovery),
        ..
    } = &command
    else {
        panic!("expected recovery")
    };
    blocked
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .continuation
        .quota_recovery = Some((**recovery).clone());
    remote
        .publisher
        .publish("one".into(), blocked)
        .await
        .unwrap();
    reply
        .send(Err(mj_client::session::SubmitFailure {
            message: "lost acknowledgement".into(),
            unconfirmed: true,
            refused: false,
        }))
        .unwrap();
    // Drain the CheckingQuota update and let the scope settle before advancing.
    receive(&mut updates).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(30)).await;
    let RemoteSessionRequest::Submit {
        command_id: retry_id,
        command: retry,
        reply,
        ..
    } = tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected exact Wait exchange before any due resume")
    };
    assert_eq!(retry_id, command_id);
    assert_eq!(retry, command);
    reply.send(Ok(4)).unwrap();
    let RemoteSessionRequest::Sync { reply, .. } = remote.requests.recv().await.unwrap() else {
        panic!("expected sync before resume")
    };
    reply.send(Ok(())).unwrap();
    let RemoteSessionRequest::Submit { command, reply, .. } =
        tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("due resume follows sync")
    };
    assert!(
        matches!(command, RelayCommand::ResumeAfterQuota { .. }),
        "{command:?}"
    );
    assert!(
        plans.try_recv().is_err(),
        "reconciliation does not replan a Wait"
    );
    reply.send(Err("end due resume probe".into())).unwrap();
    cancellation.cancel();
    task.await.unwrap().unwrap();
    remote.shutdown.shutdown().await.unwrap();
}

#[tokio::test]
async fn quota_prepare_ignores_stale_profile_for_all_session_kinds() {
    use mj_controller::controller::test_support::{IsolatedTest, test_name};
    use mj_controller::quota::{
        ProfileQuota, QuotaRefreshRequest, QuotaReportMap, QuotaReportPublication, QuotaWindow,
    };
    use mj_core::config::{HarnessKind, HarnessProfile};

    const CHILD: &str = "MJ_QUOTA_PROVENANCE_TEST_CHILD";
    const NAME: &str = "quota_prepare_ignores_stale_profile_for_all_session_kinds";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), NAME))
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "quota-provenance-test")
            .isolated_store(root.path())
            .run();
        return;
    }
    let _writer = mj_controller::database::install_isolated_test_writer();
    let home = tempfile::tempdir().unwrap();
    let profile = HarnessProfile::new(HarnessKind::Claude, home.path().to_path_buf());
    let mut config = Config::default();
    config.profiles.insert("profile-a".into(), profile.clone());
    config.profiles.insert("profile-b".into(), profile.clone());
    let mut session = super::super::tests::runtime_test_session(
        "one",
        mj_core::workspace::DEFAULT_WORKSPACE_ID,
        mj_core::state::SessionState::Running,
    );
    session.last_profile = "profile-b".into();
    session.harness_kind = HarnessKind::Claude;
    let mut controller = Controller {
        config: config.clone(),
        state: Default::default(),
    };
    controller
        .state
        .sessions
        .insert("one".into(), session.clone());
    mj_controller::database::save_session(&session).unwrap();

    let state = super::super::tests::test_runtime_state_with(controller);
    let now = mj_core::clock::epoch_seconds() as i64;
    let reset = now + 2 * 3600;
    let request =
        QuotaRefreshRequest::for_profile("profile-b", &profile, home.path().to_path_buf());
    let observed = ProfileQuota {
        profile_id: "profile-b".into(),
        harness: HarnessKind::Claude,
        windows: vec![QuotaWindow {
            label: "5H".into(),
            model_scope: None,
            remaining_percent: Some(80),
            resets_at_epoch_seconds: Some(reset),
            resets: None,
            duration_seconds: Some(5 * 3600),
            used: None,
            limit: None,
        }],
        paid_credits: None,
        monthly_price_usd: None,
        banked_resets: None,
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: now as u64,
        rate_limited_until_epoch_seconds: None,
        next_check_at_epoch_seconds: None,
    };
    let reports = Arc::new(Mutex::new(QuotaReportMap::from([(
        "profile-b".into(),
        QuotaReportPublication {
            login_error: None,
            cache_identity: request.cache_identity(),
            last_good: None,
            report: Some(observed),
        },
    )])));
    let (_publications, quota_feed) = tokio::sync::watch::channel(reports.lock().unwrap().clone());
    let catalog = mj_controller::profile_catalog::ProfileCatalog::with_probe(
        mj_controller::profile_catalog::counting_probe(Arc::new(
            std::sync::atomic::AtomicUsize::new(0),
        )),
    );
    catalog.sync(&config);
    let (refresh, _refreshes) = mpsc::unbounded_channel();
    let backend = Arc::new(
        crate::server_runtime::api::ApiBackend::new(
            state.session_manager.client(),
            Arc::new(|_| None),
            state.clone(),
        )
        .with_quota_reports(reports)
        .with_quota_publications(quota_feed)
        .with_quota_refresh(refresh.clone())
        .with_profile_catalog(catalog),
    );
    state.install_report_backend(&backend);
    state.attach_quota_refresh(refresh);

    let mut blocked = view("one", true);
    quota_assessed(&mut blocked);
    let explicit_reset = now + 10 * 3600;
    let outcome = blocked
        .snapshot
        .as_mut()
        .unwrap()
        .materialized
        .last_turn_outcome
        .as_mut()
        .unwrap();
    outcome.completed_at_ms = now * 1000;
    outcome.diagnostic = Some(mj_core::diagnostic::TurnDiagnostic {
        message: "You've hit your usage limit.".into(),
        code: Some("usageLimitExceeded".into()),
        http_status: None,
        reset_at: Some(explicit_reset.to_string()),
        login_fingerprint: None,
    });

    // Without a selection row, provenance coalesces to current profile-b and
    // planning proceeds normally.
    let missing_row = quota::prepare(&state, "one", &blocked).await.unwrap();
    assert!(
        !matches!(missing_row, quota::Plan::Obsolete),
        "missing turn selection row must coalesce to current profile: {missing_row:?}"
    );

    let mut old_worker = blocked.clone();
    old_worker
        .snapshot
        .as_mut()
        .unwrap()
        .operational
        .relay_protocol_version = Some(mj_core::relay::RELAY_IN_PLACE_MOVE_PROTOCOL - 1);
    let quota::Plan::Wait(wait) = quota::prepare(&state, "one", &old_worker).await.unwrap() else {
        panic!("an old worker must wait on the source profile");
    };
    assert!(
        wait.notice
            .contains("worker predates credential-preserving in-place Move"),
        "{}",
        wait.notice
    );
    assert!(wait.retry_at_ms.is_some());
    assert!(
        mj_controller::database::load_move_operation("one")
            .unwrap()
            .is_none()
    );

    // Recording that the stopped turn ran on profile-a makes planning on
    // profile-b obsolete for a top-level session.
    rusqlite::Connection::open(mj_controller::database::database_path())
        .unwrap()
        .execute(
            "INSERT INTO session_turn_selections(session_id, command_id, profile, model) \
             VALUES ('one', 'request', 'profile-a', 'claude-opus')",
            [],
        )
        .unwrap();
    let stale = quota::prepare(&state, "one", &blocked).await.unwrap();
    assert!(
        matches!(stale, quota::Plan::Obsolete),
        "top-level turn from profile-a must be obsolete on profile-b: {stale:?}"
    );

    // A stale result belongs to profile-a for every child kind as well.
    for kind in [
        mj_core::subagent::SubagentKind::Worker,
        mj_core::subagent::SubagentKind::Reviewer,
        mj_core::subagent::SubagentKind::Oracle,
    ] {
        let mut child_controller = Controller {
            config: config.clone(),
            state: Default::default(),
        };
        child_controller
            .state
            .sessions
            .insert("one".into(), session.clone());
        child_controller.state.subagents.insert(
            "one".into(),
            mj_core::subagent::SubagentRecord {
                resume_with_parent: false,
                kind,
                reviewer_launch: None,
                child_session_id: "one".into(),
                parent_session_id: "parent".into(),
                task_name: "worker".into(),
                profile_id: "profile-b".into(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                initial_prompt: "work".into(),
                request_key: "req".into(),
                created_at: "2026-11-22T00:00:00Z".into(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: true,
                orchestrator: false,
            }
            .into(),
        );
        let child_state = super::super::tests::test_runtime_state_with(child_controller);
        child_state.install_report_backend(&backend);
        let child_plan = quota::prepare(&child_state, "one", &blocked).await.unwrap();
        assert!(
            matches!(child_plan, quota::Plan::Obsolete),
            "{kind:?} child result from profile-a must be obsolete on profile-b: {child_plan:?}"
        );
    }
}

#[tokio::test]
async fn failed_move_publishes_notice_on_disconnect_and_releases_refused_wait() {
    use futures::FutureExt;
    let mut remote = spawn_remote_session_manager().unwrap();
    remote.targets.send_replace(vec![RelaySessionTarget {
        session_id: "one".into(),
        spec: CommandSpec::new("unused", std::iter::empty::<&str>()),
        worker_recovery: None,
        project_memory: None,
    }]);
    let controller = Controller {
        config: Config::default(),
        state: Default::default(),
    };
    let runtime = super::super::tests::test_runtime_state_with(controller);
    let gate = Arc::new(mj_controller::upgrade::Gate::default());
    let environment = Environment {
        child_switch_runtime: Some(runtime.clone()),
        session_gate: Arc::new(mj_controller::recovery_gate::RecoveryGate::default()),
        schedules: Default::default(),
        log: None,
        control: remote.control,
        claim: Arc::new(|_| Claim::Allowed),
        live: Arc::new(|| live_sessions(&["one"])),
        review: Arc::new(|_, _| {}),
        profile: Arc::new(|_| Some("test".into())),
        quota: Arc::new(|_, _| unreachable!("test drives quota::schedule directly")),
        start_quota_move: Arc::new(|_, _, _, _| {
            Box::pin(async {
                Ok(mj_core::state::MoveOutcome {
                    session_id: "one".into(),
                    operation_id: "op-move-1".into(),
                    outcome: "failed".into(),
                    error: Some("internal path leaked".into()),
                    recovery: Some("Retry the Move to restore the checkpoint.".into()),
                    profile_id: "test".into(),
                    target_template_id: "localhost".into(),
                })
            })
        }),
    };

    // 1. Failed Move while source is disconnected must publish the sanitized
    // failure + recovery notice and return Err so retry stays pending until reconnect.
    let mut disconnected = view("one", true);
    quota_assessed(&mut disconnected);
    disconnected.connected = false;
    remote
        .publisher
        .publish("one".into(), disconnected.clone())
        .await
        .unwrap();
    let fallback = recovery_for(
        &disconnected,
        Some(mj_core::clock::epoch_millis() + 3_600_000),
    );
    let work = gate.enter_unless_draining("quota assessment").unwrap();
    let result = quota::schedule(
        &environment,
        "one",
        disconnected.clone(),
        quota::Plan::Move {
            request: Box::new(quota_move_request()),
            destination: "peer".into(),
            reservation: None,
            reason: "quota exhausted".into(),
            fallback: fallback.clone(),
        },
        &gate,
        Some(work),
    )
    .await;
    assert!(
        result.is_err(),
        "disconnected source must keep fallback pending until reconnect"
    );
    {
        let notices = runtime.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]
                .notice
                .text
                .contains("Retry the Move to restore the checkpoint."),
            "{}",
            notices[0].notice.text
        );
        assert!(
            !notices[0].notice.text.contains("internal path leaked"),
            "{}",
            notices[0].notice.text
        );
    }

    // 2. A stale-cursor refusal on the post-Move Wait releases admission
    // immediately (does not loop under the same upgrade hold) and leaves the
    // turn un-finished so the next attempt can run with a fresh cursor.
    let mut connected = view("one", true);
    quota_assessed(&mut connected);
    remote
        .publisher
        .publish("one".into(), connected.clone())
        .await
        .unwrap();
    let work = gate.enter_unless_draining("quota assessment").unwrap();
    let schedule_task = mj_core::runtime::spawn({
        let env = environment.clone();
        let view = connected.clone();
        let gate = gate.clone();
        async move {
            quota::schedule(
                &env,
                "one",
                view,
                quota::Plan::Move {
                    request: Box::new(quota_move_request()),
                    destination: "peer".into(),
                    reservation: None,
                    reason: "quota exhausted".into(),
                    fallback,
                },
                &gate,
                Some(work),
            )
            .await
        }
    });
    let RemoteSessionRequest::Submit { reply, .. } =
        tokio::time::timeout(Duration::from_secs(3), remote.requests.recv())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("expected initial Wait submit")
    };
    reply
        .send(Err(mj_client::session::SubmitFailure {
            message: "quota recovery evidence is stale".into(),
            unconfirmed: false,
            refused: true,
        }))
        .unwrap();
    assert!(
        schedule_task.await.unwrap().is_err(),
        "stale cursor refusal must end the admitted attempt and release upgrade work"
    );
    assert!(!quota::unanswered(&environment, "one"));
    assert!(!quota::finished(&environment, "one", &connected));
    assert!(
        remote.requests.recv().now_or_never().is_none(),
        "stale refusal must not loop under the same daemon admission"
    );
    remote.shutdown.shutdown().await.unwrap();
}
