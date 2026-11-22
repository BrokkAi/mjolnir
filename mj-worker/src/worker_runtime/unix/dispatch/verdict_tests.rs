use super::*;
use crate::acp::verdict_client::{VerdictClient, VerdictSource};
use crate::test_support::{scoped_log_capture_sees_every_callsite, wait_for};
use mj_core::activity::ActivityState;
use mj_core::assessment::{Action, Status};
use mj_core::diagnostic::{QUOTA_STOP_REASON, TurnDiagnostic};
use mj_core::relay::RelayCursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tracing::instrument::WithSubscriber;

const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

async fn read_glide_request(stream: &mut TcpStream) -> serde_json::Value {
    let mut stream = BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[derive(Clone, Copy)]
enum WhileClassifying {
    Child,
    Wait,
    Stop,
    KeepCurrent,
    /// A checkpoint barrier becomes ready while the classifier is asked.
    Checkpoint,
}

async fn completed_turn_verdict(action: WhileClassifying) {
    completed_turn_with_background(action, "background_work", 0).await;
}

async fn completed_turn_with_background(
    action: WhileClassifying,
    choice: &'static str,
    tasks: usize,
) {
    completed_turn_response(action, choice, tasks, 0.95, 0.95, 200).await;
}

async fn completed_turn_response(
    action: WhileClassifying,
    choice: &'static str,
    tasks: usize,
    confidence: f32,
    work_probability: f64,
    status: u16,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requested_tx, mut requested_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = mj_core::runtime::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_glide_request(&mut stream).await;
        assert_eq!(request["phase"], "replied");
        assert_eq!(
            request["assistant_text_tail"],
            "Waiting for my agent to finish."
        );
        assert_eq!(request["background_commands"], tasks);
        requested_tx.send(()).unwrap();
        if release_rx.await.is_err() {
            return;
        }
        let work = if choice == "user" || choice == "background_work" {
            "waiting"
        } else {
            choice
        };
        let input = if choice == "user" { "required" } else { "none" };
        let questions: serde_json::Value =
            serde_json::from_str(mj_core::activity::eot_glide::QUESTIONS).unwrap();
        let mut answers = serde_json::json!({});
        for (axis, question) in questions.as_object().unwrap() {
            let winner = match axis.as_str() {
                "work" => work,
                "input" => input,
                "failure" | "attention" => "none",
                "background" => "unclear",
                "permission_veto" => "unknown",
                "turn_state" => {
                    if work == "finished" {
                        "finished"
                    } else {
                        "wait"
                    }
                }
                _ => unreachable!(),
            };
            let criteria = question["criteria"].as_object().unwrap();
            let probability = if axis == "work" || axis == "turn_state" {
                work_probability
            } else {
                1.0
            };
            let mut scores = serde_json::Map::new();
            for name in criteria.keys() {
                scores.insert(
                    name.clone(),
                    serde_json::json!(if name == winner {
                        probability
                    } else {
                        (1.0 - probability) / (criteria.len() - 1) as f64
                    }),
                );
            }
            answers[axis] = serde_json::json!({"probabilities":scores});
        }
        let body = serde_json::json!({"answers":answers}).to_string();
        let _ = confidence;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "test").unwrap();
    durable
        .set_managed_subagent(matches!(action, WhileClassifying::Child))
        .unwrap();
    durable.set_turn_verdict_harness(HarnessKind::Claude);
    durable.set_background_work_policy(crate::relay::BackgroundWorkPolicy::AirTasks);
    durable
        .set_air_tasks(
            (0..tasks)
                .map(|id| crate::relay::AirTaskFixture {
                    task_id: id.to_string(),
                    description: "Old provisioning wait".into(),
                })
                .collect(),
        )
        .unwrap();
    durable.set_harness_turn_policy(crate::relay::HarnessTurnPolicy::ClaudeAdapter);
    let submit = |durable: &mut DurableRelay, id: &str| {
        let response = durable.handle(RelayRequestEnvelope {
            request_id: format!("submit-{id}"),
            protocol_version: crate::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Submit {
                command_id: id.into(),
                command: RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::v1::schema::ContentBlock::Text(
                        agent_client_protocol::v1::schema::TextContent::new("Do the work"),
                    )],
                },
            },
        });
        assert!(matches!(response.body, RelayResponseBody::Ok { .. }));
    };
    submit(&mut durable, "prompt-1");
    let relay = Arc::new(crate::relay::RelayCell::new(durable));
    let (events_tx, events_rx) = mpsc::channel(16);
    let (wakes_tx, wakes_rx) = mpsc::channel(1);
    let (commands_tx, mut commands_rx) = mpsc::channel(4);
    let user_shells = crate::user_shell::UserShellRegistry::new(
        temp.path().to_owned(),
        BTreeMap::new(),
        events_tx.clone(),
    );
    let client = VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap();
    scoped_log_capture_sees_every_callsite();
    let log_path = temp.path().join("worker.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(crate::DEFAULT_WORKER_LOG_FILTER)
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    let coordinator = mj_core::runtime::spawn(
        run_relay_coordinator_with_verdict(
            relay.clone(),
            events_rx,
            wakes_rx,
            commands_tx,
            user_shells,
            None,
            Some(client),
            None,
        )
        .with_subscriber(subscriber),
    );
    events_tx
        .send(RuntimeEvent::SessionConfigured {
            config_options: vec![],
        })
        .await
        .unwrap();
    assert!(matches!(
        commands_rx.recv().await.unwrap(),
        CommandRequest::Prompt { .. }
    ));
    if matches!(action, WhileClassifying::Child) {
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(requested_rx.try_recv().is_err());
        assert!(
            relay
                .lock()
                .unwrap()
                .operational_state()
                .active_prompt
                .is_some()
        );
    }
    events_tx.send(RuntimeEvent::SessionUpdate { update: serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"Waiting for my agent to finish."}}) }).await.unwrap();
    events_tx
        .send(RuntimeEvent::PromptFinished {
            request_id: "prompt-1".into(),
            stop_reason: "end_turn".into(),
            usage: None,
            diagnostic: None,
        })
        .await
        .unwrap();
    if matches!(action, WhileClassifying::Child) {
        wait_for("child completion", async {
            while relay
                .lock()
                .unwrap()
                .operational_state()
                .active_prompt
                .is_some()
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(requested_rx.try_recv().is_err());
        let state = relay.lock().unwrap().operational_state();
        assert!(!state.retry_assessment_pending);
        assert!(!state.assessment.as_ref().unwrap().current());
        assert_eq!(state.is_quiet(), tasks == 0);
        assert!(relay.lock().unwrap().handback_verdict_client.is_some());
        events_tx.send(RuntimeEvent::Stopped).await.unwrap();
        wait_for("child coordinator shutdown", coordinator)
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        return;
    }
    wait_for("the local classifier request", requested_rx)
        .await
        .unwrap();
    // Completion is published even while the classifier HTTP response is blocked.
    assert!(
        relay
            .lock()
            .unwrap()
            .operational_state()
            .active_prompt
            .is_none()
    );
    assert!(!matches!(
        relay.lock().unwrap().operational_state().activity_state(),
        ActivityState::Expecting { .. }
    ));
    if matches!(action, WhileClassifying::Stop) {
        events_tx.send(RuntimeEvent::Stopped).await.unwrap();
        wait_for("classifier coordinator shutdown", coordinator)
            .await
            .unwrap()
            .unwrap();
        drop(release_tx);
        server.await.unwrap();
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("GLiDE classification requested"), "{log}");
        assert!(log.contains("cancelled"), "{log}");
        return;
    }
    let barrier = |relay: &mut DurableRelay, id: &str, command: RelayCommand| {
        let response = relay.handle(RelayRequestEnvelope {
            request_id: format!("request-{id}"),
            protocol_version: crate::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Submit {
                command_id: id.into(),
                command,
            },
        });
        assert!(
            matches!(response.body, RelayResponseBody::Ok { .. }),
            "{response:?}"
        );
    };
    let cut = if matches!(action, WhileClassifying::Checkpoint) {
        barrier(
            &mut relay.lock().unwrap(),
            "checkpoint-barrier",
            RelayCommand::BeginCheckpoint { reason: None },
        );
        wakes_tx.send(()).await.unwrap();
        wait_for("the checkpoint cut", async {
            while relay
                .lock()
                .unwrap()
                .operational_state()
                .checkpoint_ready
                .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        relay.lock().unwrap().operational_state().checkpoint_ready
    } else {
        None
    };
    release_tx.send(()).unwrap();
    wait_for("the local classifier response", server)
        .await
        .unwrap();
    if let Some(cut) = cut {
        // The answer is back, but the barrier holds its cut: the answer waits.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let state = relay.lock().unwrap().operational_state();
        assert_eq!(state.latest_ordinal, cut.ordinal);
        assert_eq!(
            state.assessment.as_ref().map(|a| a.status),
            Some(mj_core::assessment::Status::Pending)
        );
        barrier(
            &mut relay.lock().unwrap(),
            "checkpoint-release",
            RelayCommand::ReleaseCheckpoint {
                barrier_command_id: "checkpoint-barrier".into(),
            },
        );
        wakes_tx.send(()).await.unwrap();
        wait_for(
            "the held classifier answer after checkpoint release",
            async {
                while relay
                    .lock()
                    .unwrap()
                    .operational_state()
                    .assessment
                    .as_ref()
                    .is_none_or(|a| a.status == mj_core::assessment::Status::Pending)
                {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            },
        )
        .await;
        assert!(relay.lock().unwrap().operational_state().latest_ordinal > cut.ordinal);
    } else if matches!(action, WhileClassifying::Wait) {
        wait_for("the classifier's activity decision", async {
            loop {
                let state = relay.lock().unwrap().operational_state();
                if if choice == "background_work" {
                    state.expected_continuation.is_some()
                } else {
                    state.inferred_idle_since_ms.is_some()
                } {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        let state = relay.lock().unwrap().operational_state();
        assert_eq!(state.background_commands.len(), tasks);
        if choice == "background_work" {
            assert_eq!(state.activity_state().is_working(), tasks > 0);
        } else {
            assert!(state.activity_state().is_idle());
        }
        if tasks > 0 {
            assert!(!state.is_quiet());
        }
        // Autonomous output starts a real harness turn and clears the guess.
        events_tx.send(RuntimeEvent::SessionUpdate { update: serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"The agent finished."}}) }).await.unwrap();
        wait_for("the new autonomous harness turn", async {
            loop {
                if relay
                    .lock()
                    .unwrap()
                    .operational_state()
                    .harness_turn
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
    } else if matches!(action, WhileClassifying::KeepCurrent) {
        wait_for("the classifier assessment result", async {
            while relay
                .lock()
                .unwrap()
                .operational_state()
                .assessment
                .as_ref()
                .is_none_or(|a| {
                    a.status
                        != if status == 200 {
                            mj_core::assessment::Status::Assessed
                        } else {
                            mj_core::assessment::Status::Failed
                        }
                })
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        let state = relay.lock().unwrap().operational_state();
        assert!(matches!(
            state.activity_state(),
            ActivityState::Background { .. }
        ));
        assert_eq!(state.inferred_idle_since_ms, None);
    }
    assert!(!matches!(
        relay.lock().unwrap().operational_state().activity_state(),
        ActivityState::Expecting { .. }
    ));
    events_tx.send(RuntimeEvent::Stopped).await.unwrap();
    wait_for("classifier coordinator shutdown", coordinator)
        .await
        .unwrap()
        .unwrap();
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("GLiDE classification requested"), "{log}");
    assert!(log.contains("summary_bytes="), "{log}");
    assert!(
        !log.contains("Waiting for my agent to finish."),
        "payloads must not be logged: {log}"
    );
    assert!(log.contains(SESSION_ID), "{log}");
    assert!(!log.contains("fake-key"));
    if matches!(action, WhileClassifying::Wait) {
        assert!(log.contains("GLiDE classification received"), "{log}");
        assert!(log.contains("probabilities="), "{log}");
        assert!(log.contains("applied"), "{log}");
    }
}

#[tokio::test]
async fn replied_verdict_publishes_without_delaying_completion_and_clears_on_output() {
    completed_turn_verdict(WhileClassifying::Wait).await;
    completed_turn_verdict(WhileClassifying::Child).await;
    completed_turn_with_background(WhileClassifying::Child, "finished", 1).await;
}

/// I2-3: the classifier's answer landed after a close's checkpoint cut and
/// made the worker refuse the Close. The answer now waits for the barrier.
// Hard-won: 634af0a: a classifier journal write moved the checkpoint cut and made suspend fail.
#[tokio::test]
async fn a_classifier_answer_waits_for_a_ready_checkpoint_barrier() {
    completed_turn_with_background(WhileClassifying::Checkpoint, "finished", 0).await;
}

#[tokio::test]
async fn shutdown_cancels_pending_replied_verdict() {
    completed_turn_verdict(WhileClassifying::Stop).await;
}

#[tokio::test]
async fn probability_qualified_completion_and_input_reach_activity_at_low_confidence() {
    for choice in ["finished", "user"] {
        completed_turn_response(WhileClassifying::Wait, choice, 4, 0.2, 0.95, 200).await;
    }
}

#[tokio::test]
async fn uncertain_verdicts_are_cached_and_transport_failures_back_off() {
    completed_turn_response(WhileClassifying::KeepCurrent, "finished", 4, 0.5, 0.5, 200).await;
    completed_turn_response(WhileClassifying::KeepCurrent, "unclear", 4, 0.95, 0.95, 200).await;
    completed_turn_response(
        WhileClassifying::KeepCurrent,
        "finished",
        4,
        0.95,
        0.95,
        503,
    )
    .await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuotaCase {
    UntimedRecovery,
    FailedBackoff,
    ProseOnly,
    PausedGoal,
}

async fn completed_quota_turn(case: QuotaCase) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = requests.clone();
    let server = mj_core::runtime::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_glide_request(&mut stream).await;
            request_count.fetch_add(1, Ordering::Relaxed);
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
    });
    let diagnostic = if case == QuotaCase::ProseOnly {
        serde_json::json!({"message": "RPM quota exceeded"})
    } else {
        serde_json::json!({"message": "Plan usage exhausted", "code": "usageLimitExceeded"})
    };
    let diagnostic = TurnDiagnostic::from_provider(&diagnostic).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut durable = DurableRelay::open(temp.path(), SESSION_ID, "test").unwrap();
    let submit = |relay: &mut DurableRelay, id: &str, command| {
        let response = relay.handle(RelayRequestEnvelope {
            request_id: format!("submit-{id}"),
            protocol_version: crate::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Submit {
                command_id: id.into(),
                command,
            },
        });
        assert!(
            matches!(response.body, RelayResponseBody::Ok { .. }),
            "{response:?}"
        );
    };
    submit(
        &mut durable,
        "prompt-1",
        RelayCommand::Prompt {
            prompt: mj_core::continuation::prompt_blocks(),
        },
    );
    let failed = if case == QuotaCase::FailedBackoff {
        // Write and reopen the durable state left by a worker that asked GLiDE.
        // Install the harness on the reopened relay so only it settles locally.
        durable
            .record_observation(RelayObservation::SessionConfigured {
                config_options: vec![],
            })
            .unwrap();
        assert_eq!(durable.claim_pending_commands(true).unwrap().len(), 1);
        durable
            .record_command_completed(
                "prompt-1",
                RelayCommandOutcome::Prompt {
                    work_facts: None,
                    stop_reason: QUOTA_STOP_REASON.into(),
                    usage: None,
                    diagnostic: Some(diagnostic.clone()),
                },
            )
            .unwrap();
        let pending = durable.operational_state().assessment.unwrap();
        durable
            .fail_turn_assessment(pending.revision, "classifier_failed")
            .unwrap();
        drop(durable);
        durable = DurableRelay::open(temp.path(), SESSION_ID, "test").unwrap();
        let failed = durable.operational_state().assessment.unwrap();
        assert_eq!(failed.status, Status::Failed);
        assert!(failed.retry_at_ms.unwrap() > mj_core::clock::epoch_millis());
        Some(failed)
    } else {
        None
    };
    durable.set_turn_verdict_harness(HarnessKind::Codex);
    if case == QuotaCase::UntimedRecovery {
        durable.set_background_work_policy(crate::relay::BackgroundWorkPolicy::AirTasks);
        durable
            .set_air_tasks(vec![crate::relay::AirTaskFixture {
                task_id: "background-task".into(),
                description: "Waiting for a build".into(),
            }])
            .unwrap();
    }
    let relay = Arc::new(crate::relay::RelayCell::new(durable));
    let (events_tx, events_rx) = mpsc::channel(4);
    let (wakes_tx, wakes_rx) = mpsc::channel(1);
    let (commands_tx, mut commands_rx) = mpsc::channel(4);
    let user_shells = crate::user_shell::UserShellRegistry::new(
        temp.path().to_owned(),
        BTreeMap::new(),
        events_tx.clone(),
    );
    let client = VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap();
    let coordinator = mj_core::runtime::spawn(run_relay_coordinator_with_verdict(
        relay.clone(),
        events_rx,
        wakes_rx,
        commands_tx,
        user_shells,
        None,
        Some(client),
        None,
    ));
    if failed.is_none() {
        events_tx
            .send(RuntimeEvent::SessionConfigured {
                config_options: vec![],
            })
            .await
            .unwrap();
        assert!(matches!(
            wait_for("the prompt dispatch", commands_rx.recv())
                .await
                .unwrap(),
            CommandRequest::Prompt { .. }
        ));
        if case == QuotaCase::PausedGoal {
            events_tx
                .send(RuntimeEvent::SessionUpdate {
                    update: serde_json::json!({
                        "sessionUpdate": "session_info_update",
                        "_meta": {"goal": {"objective": "Build it", "createdAt": 1, "status": "paused"}}
                    }),
                })
                .await
                .unwrap();
        }
        events_tx
            .send(RuntimeEvent::PromptFinished {
                request_id: "prompt-1".into(),
                // The harness can use QuotaLimit even for a prose-only diagnosis.
                stop_reason: QUOTA_STOP_REASON.into(),
                usage: None,
                diagnostic: Some(diagnostic),
            })
            .await
            .unwrap();
    }
    let expected_status = match case {
        QuotaCase::ProseOnly => Status::Failed,
        QuotaCase::PausedGoal => Status::Superseded,
        _ => Status::Deferred,
    };
    let settled = wait_for("the completed quota turn assessment", async {
        loop {
            let state = relay.lock().unwrap().operational_state();
            if let Some(a) = state.assessment.filter(|a| a.status == expected_status)
                && (case != QuotaCase::ProseOnly || requests.load(Ordering::Relaxed) == 1)
            {
                break a;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if case == QuotaCase::ProseOnly {
        assert_eq!(settled.action, None);
    } else {
        assert_eq!(settled.action, Some(Action::RecoverQuota));
        assert!(settled.verdict.is_none());
        assert!(settled.glide.is_none());
        assert_eq!(settled.retry_at_ms, None);
        assert_eq!(
            settled.reason,
            if case == QuotaCase::PausedGoal {
                "automatic_action_suppressed"
            } else {
                "quota_resolution_required"
            }
        );
    }
    if let Some(failed) = failed {
        assert_eq!(settled.turn_id, failed.turn_id);
        assert_eq!(settled.completed_ordinal, failed.completed_ordinal);
        assert_eq!(settled.revision, failed.revision);
        assert_eq!(settled.completion, failed.completion);
        assert_eq!(settled.failures, failed.failures);
    }
    if case == QuotaCase::UntimedRecovery {
        {
            let mut relay = relay.lock().unwrap();
            let state = relay.operational_state();
            assert_eq!(state.background_commands.len(), 1);
            submit(
                &mut relay,
                "record-quota-recovery",
                RelayCommand::SetQuotaRecovery {
                    expected: RelayCursor {
                        ordinal: state.latest_ordinal,
                        digest: state.latest_digest,
                    },
                    recovery: Some(Box::new(mj_core::continuation::QuotaRecovery {
                        user_command_id: "prompt-1".into(),
                        completed_command_id: "prompt-1".into(),
                        profile_id: "isolated-quota-test".into(),
                        reset_at_ms: None,
                        retry_at_ms: None,
                        notice: "Quota deadline unavailable".into(),
                        submitted: false,
                    })),
                },
            );
        }
        // With a one-slot wake channel, the third send proves the coordinator
        // returned to its assessment loop after processing the first wake.
        for _ in 0..3 {
            wait_for("the coordinator to process recovery", wakes_tx.send(()))
                .await
                .unwrap();
        }
        let recovered = relay
            .lock()
            .unwrap()
            .operational_state()
            .assessment
            .unwrap();
        assert_eq!(recovered.status, Status::Assessed);
        assert_eq!(recovered.action, Some(Action::RecoverQuota));
        assert_eq!(recovered.reason, "quota_deadline_unavailable");
        assert_eq!(recovered.revision, settled.revision);
        assert!(recovered.glide.is_none());
    }
    events_tx.send(RuntimeEvent::Stopped).await.unwrap();
    wait_for("quota coordinator shutdown", coordinator)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    assert_eq!(
        requests.load(Ordering::Relaxed),
        usize::from(case == QuotaCase::ProseOnly)
    );
}

// Hard-won: 2026-11-22 live GLiDE 502 outage blocked top-level typed QuotaLimit automove
#[tokio::test]
async fn typed_quota_settles_locally_and_untimed_recovery_does_not_reask_glide() {
    completed_quota_turn(QuotaCase::UntimedRecovery).await;
}

#[tokio::test]
async fn typed_quota_settles_during_a_persisted_glide_backoff() {
    completed_quota_turn(QuotaCase::FailedBackoff).await;
}

#[tokio::test]
async fn prose_only_quota_still_asks_glide() {
    completed_quota_turn(QuotaCase::ProseOnly).await;
}

#[tokio::test]
async fn typed_quota_respects_a_paused_goal_without_asking_glide() {
    completed_quota_turn(QuotaCase::PausedGoal).await;
}

/// Sends one classification to a closed port, logging through whatever
/// dispatcher is current on the calling thread.
fn ask_once_without_a_classifier() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let client = VerdictClient::new(VerdictSource::Hosted {
            endpoint: "http://127.0.0.1:1".into(),
        })
        .unwrap();
        let evidence = mj_core::activity::verdict::TurnEvidence {
            background: Vec::new(),
            final_tool_calls: Vec::new(),
            authorization: None,
            completion: None,
            harness: mj_core::config::HarnessKind::Codex,
            phase: mj_core::activity::verdict::TurnPhase::Replied,
            silent_for_s: 0,
            tools_in_flight: vec![],
            transcript_summary: String::new(),
            background_commands: 0,
            queued_commands: 0,
            user_prompt_tail: String::new(),
            assistant_text_tail: String::new(),
        };
        let _ = client
            .ask_glide_logged("s", 1, &evidence, &Default::default(), 0)
            .await;
    });
}

/// The CI flake in order: this test's dispatcher is the only one registered,
/// then a thread with no subscriber reaches the Jev callsite first.
// Hard-won: ccd520d: parallel callsite initialization made CI log captures miss Jev events.
#[test]
fn scoped_log_capture_keeps_a_callsite_first_reached_without_a_subscriber() {
    scoped_log_capture_sees_every_callsite();
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("worker.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(crate::DEFAULT_WORKER_LOG_FILTER)
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    std::thread::spawn(ask_once_without_a_classifier)
        .join()
        .unwrap();
    tracing::dispatcher::with_default(&dispatch, ask_once_without_a_classifier);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("GLiDE classification requested"), "{log}");
}
