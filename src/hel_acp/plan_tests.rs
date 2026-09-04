use super::*;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

fn implement() -> ElicitationResponse {
    ElicitationResponse::Accept {
        content: BTreeMap::from([(
            PLAN_REVIEW_ACTION.into(),
            ElicitationValue::String("implement".into()),
        )]),
    }
}

fn permission(plan: &str, options: &[&str]) -> Value {
    json!({
        "sessionId": "plan-session",
        "toolCall": {
            "toolCallId": "plan-tool", "kind": "switch_mode", "title": "Ready to code?",
            "rawInput": {"plan": plan, "planFilePath": "/workspace/plan.md"}
        },
        "options": options.iter().map(|id| json!({
            "optionId": id, "name": id,
            "kind": if *id == "reject" {"reject_once"} else {"allow_always"}
        })).collect::<Vec<_>>()
    })
}

fn mode_config(current: &str) -> Value {
    json!([{
        "id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": current,
        "options": [
            {"value": "plan", "name": "Plan"}, {"value": "auto", "name": "Auto"},
            {"value": "bypassPermissions", "name": "Bypass"}
        ]
    }])
}

#[test]
fn claude_plan_approval_selects_the_deployment_mode_without_clearing_context() {
    for ids in [
        ["auto", "bypassPermissions", "default"],
        ["exit-plan-auto", "exit-plan-bypass", "exit-plan-default"],
    ] {
        for reverse in [false, true] {
            let mut options = vec![
                "exit-plan-clear-auto",
                "exit-plan-clear-bypass",
                ids[2],
                ids[0],
                ids[1],
            ];
            if reverse {
                options.reverse();
            }
            let request = serde_json::from_value(permission("Approved plan", &options)).unwrap();
            for (policy, expected) in [
                (ExecutionPolicy::ConfiguredApprovals, ids[0]),
                (ExecutionPolicy::Unconstrained, ids[1]),
            ] {
                let PlanPermissionAnswer::Native(answer) = policy_plan_permission_answer(
                    &request,
                    implement(),
                    HarnessKind::Claude,
                    policy,
                )
                .unwrap() else {
                    panic!("offered mode must be selected directly");
                };
                assert_eq!(
                    serde_json::to_value(answer).unwrap()["outcome"]["optionId"],
                    expected
                );
            }
        }
    }
}

#[test]
fn claude_missing_auto_is_an_error_and_missing_bypass_requires_a_continuation() {
    let request = serde_json::from_value(permission(
        "Plan",
        &["exit-plan-clear-auto", "default", "acceptEdits", "reject"],
    ))
    .unwrap();
    assert!(
        policy_plan_permission_answer(
            &request,
            implement(),
            HarnessKind::Claude,
            ExecutionPolicy::ConfiguredApprovals
        )
        .err()
        .unwrap()
        .to_string()
        .contains("required auto")
    );
    assert!(matches!(
        policy_plan_permission_answer(
            &request,
            implement(),
            HarnessKind::Claude,
            ExecutionPolicy::Unconstrained
        )
        .unwrap(),
        PlanPermissionAnswer::ContinueInBypass
    ));
    let PlanPermissionAnswer::Native(answer) = policy_plan_permission_answer(
        &request,
        ElicitationResponse::Decline,
        HarnessKind::Claude,
        ExecutionPolicy::Unconstrained,
    )
    .unwrap() else {
        panic!("decline is native")
    };
    assert_eq!(
        serde_json::to_value(answer).unwrap()["outcome"]["optionId"],
        "reject"
    );
}

/// A real ACP transport with the agent side controlled by the test. No fake
/// depends on runtime implementation order beyond the protocol under test.
struct PlanProbe {
    input: Lines<BufReader<ReadHalf<DuplexStream>>>,
    output: WriteHalf<DuplexStream>,
    commands: mpsc::Sender<CommandRequest>,
    events: mpsc::Receiver<RuntimeEvent>,
    driver: tokio::task::JoinHandle<Result<Option<String>>>,
}

impl Drop for PlanProbe {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl PlanProbe {
    async fn new(policy: ExecutionPolicy) -> Self {
        Self::with_config(policy, false).await
    }

    async fn with_config(policy: ExecutionPolicy, config: bool) -> Self {
        let (client, agent) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client);
        let (agent_read, agent_write) = tokio::io::split(agent);
        let (commands, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(128);
        let spec = LaunchSpec {
            command: "plan-probe".into(),
            args: vec![],
            environment: BTreeMap::new(),
            cwd: PathBuf::from("/workspace"),
            additional_directories: vec![],
            extra_mcp_servers: vec![],
            project_memory: None,
            resume_session: None,
            harness: HarnessKind::Claude,
            execution_policy: policy,
            acp_activity: AcpActivityClock::default(),
            step_clock: StepClock::default(),
        };
        let driver = tokio::spawn(async move {
            drive(
                ByteStreams::new(client_write.compat_write(), client_read.compat()),
                spec,
                &mut requests,
                event_tx,
                Arc::new(Mutex::new(None)),
                false,
            )
            .await
        });
        let mut probe = Self {
            input: BufReader::new(agent_read).lines(),
            output: agent_write,
            commands,
            events,
            driver,
        };
        let init = probe.message().await;
        assert_eq!(init["method"], "initialize");
        probe.result(&init, json!({"protocolVersion": 1})).await;
        let new = probe.message().await;
        assert_eq!(new["method"], "session/new");
        probe
            .result(
                &new,
                json!({"sessionId": "plan-session", "configOptions": if config {mode_config("plan")} else {json!([])}, "modes": {
                    "currentModeId": "plan", "availableModes": [
                        {"id": "plan", "name": "Plan"}, {"id": "auto", "name": "Auto"},
                        {"id": "bypassPermissions", "name": "Bypass"}
                    ]
                }}),
            )
            .await;
        if policy.is_unconstrained() {
            let mode = probe.message().await;
            if config {
                assert_eq!(mode["method"], "session/set_config_option");
                probe
                    .result(
                        &mode,
                        json!({"configOptions": mode_config("bypassPermissions")}),
                    )
                    .await;
            } else {
                assert_eq!(mode["method"], "session/set_mode");
                probe.result(&mode, json!({})).await;
            }
        }
        probe
    }

    async fn message(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.input.next_line())
            .await
            .expect("ACP message timed out")
            .unwrap()
            .expect("ACP closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn send(&mut self, message: Value) {
        self.output
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn result(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn event(&mut self) -> RuntimeEvent {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("runtime event timed out")
            .expect("runtime stopped")
    }

    async fn approve(&mut self, plan: &str, options: &[&str]) -> (Value, String, Value) {
        self.commands
            .send(CommandRequest::Prompt {
                request_id: "original-prompt".into(),
                prompt: vec![ContentBlock::Text(TextContent::new("Make a plan"))],
            })
            .await
            .unwrap();
        let prompt = self.message().await;
        assert_eq!(prompt["method"], "session/prompt");
        self.send(json!({"jsonrpc": "2.0", "id": "permission-1", "method": "session/request_permission", "params": permission(plan, options)})).await;
        let id = loop {
            if let RuntimeEvent::ElicitationRequested { request } = self.event().await {
                break request.id;
            }
        };
        self.answer(&id).await.unwrap();
        let answer = self.message().await;
        assert_eq!(answer["id"], "permission-1");
        (prompt, id, answer)
    }

    async fn answer(&mut self, id: &str) -> std::result::Result<(), String> {
        let (resolved, response) = oneshot::channel();
        self.commands
            .send(CommandRequest::ResolveElicitation {
                elicitation_id: id.into(),
                response: implement(),
                resolved,
            })
            .await
            .unwrap();
        response.await.unwrap()
    }

    async fn no_message(&mut self) {
        let message = tokio::time::timeout(Duration::from_millis(30), self.input.next_line()).await;
        assert!(
            message.is_err(),
            "unexpected ACP request before its prerequisite: {message:?}"
        );
    }

    async fn finished(&mut self) -> String {
        loop {
            if let RuntimeEvent::PromptFinished {
                request_id,
                stop_reason,
            } = self.event().await
            {
                assert_eq!(request_id, "original-prompt");
                return stop_reason;
            }
        }
    }

    async fn close(&mut self) {
        self.commands
            .send(CommandRequest::Close {
                request_id: "close".into(),
            })
            .await
            .unwrap();
        let mut close = self.message().await;
        if close["method"] == "session/cancel" {
            close = self.message().await;
        }
        assert_eq!(close["method"], "session/close");
        self.result(&close, json!({})).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(5), &mut self.driver)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn approved_plan_waits_for_turn_and_mode_ack_then_continues_once_in_same_session() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let plan = "Implement the parser and its behavior tests.\n".repeat(2048);
    assert!(plan.len() > 65536);
    let (prompt, id, answer) = probe
        .approve(
            &plan,
            &[
                "exit-plan-clear-auto",
                "exit-plan-default",
                "exit-plan-auto",
                "reject",
            ],
        )
        .await;
    assert_eq!(answer["result"]["outcome"]["outcome"], "cancelled");
    assert!(
        probe.answer(&id).await.is_err(),
        "a duplicate answer must not schedule another continuation"
    );
    probe.no_message().await;
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    let mode = probe.message().await;
    assert_eq!(mode["method"], "session/set_mode");
    assert_eq!(mode["params"]["modeId"], "bypassPermissions");
    probe.no_message().await;
    while let Ok(event) = probe.events.try_recv() {
        assert!(
            !matches!(event, RuntimeEvent::PromptFinished { .. }),
            "the relay command must stay active through the transition"
        );
    }
    probe.result(&mode, json!({})).await;
    let continuation = probe.message().await;
    assert_eq!(continuation["method"], "session/prompt");
    assert_eq!(continuation["params"]["sessionId"], "plan-session");
    let text = continuation["params"]["prompt"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("The user approved"));
    assert!(text.ends_with(&plan));
    probe
        .result(&continuation, json!({"stopReason": "end_turn"}))
        .await;
    assert_eq!(probe.finished().await, "EndTurn");
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn guardian_approval_uses_auto_without_a_followup_prompt() {
    let mut probe = PlanProbe::new(ExecutionPolicy::ConfiguredApprovals).await;
    let (prompt, _, answer) = probe
        .approve(
            "Plan",
            &[
                "exit-plan-default",
                "exit-plan-clear-auto",
                "exit-plan-auto",
                "reject",
            ],
        )
        .await;
    assert_eq!(answer["result"]["outcome"]["optionId"], "exit-plan-auto");
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    assert_eq!(probe.finished().await, "EndTurn");
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn rejected_mode_change_never_submits_the_approved_plan() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
    probe
        .result(&prompt, json!({"stopReason": "cancelled"}))
        .await;
    let mode = probe.message().await;
    probe.send(json!({"jsonrpc":"2.0", "id": mode["id"], "error": {"code": -32603, "message": "mode unavailable"}})).await;
    let mut warned = false;
    loop {
        match probe.event().await {
            RuntimeEvent::Warning { message }
                if message.contains("could not restore bypassPermissions") =>
            {
                warned = true
            }
            RuntimeEvent::PromptFinished { stop_reason, .. } => {
                assert_eq!(stop_reason, "error");
                break;
            }
            _ => {}
        }
    }
    assert!(warned);
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn cancelling_during_mode_restoration_discards_the_continuation() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    let mode = probe.message().await;
    probe
        .commands
        .send(CommandRequest::Cancel {
            request_id: "cancel".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    assert_eq!(probe.message().await["method"], "session/cancel");
    assert_eq!(probe.finished().await, "Cancelled");
    let cancelled_request = probe.message().await;
    assert_eq!(cancelled_request["method"], "$/cancel_request");
    assert_eq!(cancelled_request["params"]["requestId"], mode["id"]);
    probe.result(&mode, json!({})).await;
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn closing_during_mode_restoration_discards_the_continuation() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    assert_eq!(probe.message().await["method"], "session/set_mode");
    probe.close().await;
}

#[tokio::test]
async fn failed_planning_turn_does_not_restore_mode_or_continue() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
    probe.send(json!({"jsonrpc":"2.0", "id": prompt["id"], "error": {"code": -32603, "message": "planning failed"}})).await;
    assert_eq!(probe.finished().await, "error");
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn cancelling_while_waiting_for_the_planning_turn_prevents_mode_restoration() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
    probe
        .commands
        .send(CommandRequest::Cancel {
            request_id: "cancel".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    assert_eq!(probe.message().await["method"], "session/cancel");
    probe
        .result(&prompt, json!({"stopReason": "cancelled"}))
        .await;
    assert_eq!(probe.finished().await, "Cancelled");
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn plan_transition_timeout_requests_a_restart_without_replaying_implementation() {
    for awaiting_mode in [false, true] {
        let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
        let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
        if awaiting_mode {
            probe
                .result(&prompt, json!({"stopReason": "end_turn"}))
                .await;
            assert_eq!(probe.message().await["method"], "session/set_mode");
        }
        loop {
            if let RuntimeEvent::Warning { message } = probe.event().await
                && message.contains("waiting for Claude")
            {
                break;
            }
        }
        tokio::time::pause();
        tokio::time::advance(CANCEL_ACK_TIMEOUT + Duration::from_secs(1)).await;
        let mut warned = false;
        loop {
            match probe.event().await {
                RuntimeEvent::Warning { message }
                    if message.contains("Plan implementation timed out") =>
                {
                    warned = true
                }
                RuntimeEvent::CommandInterrupted { request_id, .. } => {
                    assert_eq!(request_id, "original-prompt");
                    break;
                }
                RuntimeEvent::PromptFinished { .. } => panic!("timeout must interrupt the command"),
                _ => {}
            }
        }
        assert!(warned);
        assert_eq!(
            (&mut probe.driver).await.unwrap().unwrap(),
            Some("plan-session".into())
        );
        tokio::time::resume();
        while let Some(line) = probe.input.next_line().await.unwrap() {
            let message: Value = serde_json::from_str(&line).unwrap();
            assert_ne!(message["method"], "session/prompt");
        }
    }
}

#[tokio::test]
async fn dropping_bridge_connection_discards_a_pending_implementation() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    assert_eq!(probe.message().await["method"], "session/set_mode");
    // run_bridge drops the driver when its child exits; exercise that same
    // teardown while the mode acknowledgement is still outstanding.
    probe.driver.abort();
    tokio::time::timeout(Duration::from_secs(5), &mut probe.driver)
        .await
        .expect("transport loss must stop the connection")
        .expect_err("the driver must be cancelled");
    while let Some(line) = probe.input.next_line().await.unwrap() {
        let message: Value = serde_json::from_str(&line).unwrap();
        assert_ne!(message["method"], "session/prompt");
    }
}

#[tokio::test]
async fn guardian_without_auto_reports_failure_instead_of_selecting_manual_mode() {
    let mut probe = PlanProbe::new(ExecutionPolicy::ConfiguredApprovals).await;
    let (prompt, _, answer) = probe
        .approve(
            "Plan",
            &["exit-plan-default", "exit-plan-clear-auto", "reject"],
        )
        .await;
    assert_eq!(answer["result"]["outcome"]["outcome"], "cancelled");
    loop {
        if let RuntimeEvent::Warning { message } = probe.event().await
            && message.contains("required auto")
        {
            break;
        }
    }
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    probe.finished().await;
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn offered_bypass_is_selected_without_cancelling_the_plan_turn() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    let (prompt, _, answer) = probe
        .approve(
            "Plan",
            &[
                "exit-plan-clear-bypass",
                "exit-plan-default",
                "exit-plan-bypass",
                "exit-plan-auto",
                "reject",
            ],
        )
        .await;
    assert_eq!(answer["result"]["outcome"]["optionId"], "exit-plan-bypass");
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    probe.finished().await;
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn config_mode_restoration_checks_the_mode_returned_by_claude() {
    for returned_mode in ["bypassPermissions", "auto"] {
        let mut probe = PlanProbe::with_config(ExecutionPolicy::Unconstrained, true).await;
        let (prompt, _, _) = probe
            .approve("The approved plan", &["exit-plan-auto", "reject"])
            .await;
        probe
            .result(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        let mode = probe.message().await;
        assert_eq!(mode["method"], "session/set_config_option");
        assert_eq!(mode["params"]["value"], "bypassPermissions");
        probe.no_message().await;
        probe
            .result(&mode, json!({"configOptions": mode_config(returned_mode)}))
            .await;
        if returned_mode == "bypassPermissions" {
            let continuation = probe.message().await;
            assert_eq!(continuation["method"], "session/prompt");
            probe
                .result(&continuation, json!({"stopReason": "end_turn"}))
                .await;
            assert_eq!(probe.finished().await, "EndTurn");
        } else {
            assert_eq!(probe.finished().await, "error");
        }
        probe.no_message().await;
        probe.close().await;
    }
}
