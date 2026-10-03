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
            {"value": "default", "name": "Default"}, {"value": "plan", "name": "Plan"},
            {"value": "auto", "name": "Auto"}, {"value": "acceptEdits", "name": "Accept edits"},
            {"value": "bypassPermissions", "name": "Bypass"}
        ]
    }])
}

async fn set_claude_config_mode(probe: &mut PlanProbe, request_id: &str, mode: &str) {
    probe
        .commands
        .send(CommandRequest::SetConfig {
            request_id: request_id.into(),
            key: "mode".into(),
            value: mode.into(),
        })
        .await
        .unwrap();
    let request = probe.message().await;
    assert_eq!(request["method"], "session/set_config_option");
    assert_eq!(request["params"]["value"], mode);
    probe
        .result(&request, json!({"configOptions": mode_config(mode)}))
        .await;
    loop {
        if let RuntimeEvent::ConfigApplied { key, value, .. } = probe.event().await {
            assert_eq!(key, "mode");
            assert_eq!(value, mode);
            break;
        }
    }
}

#[tokio::test]
async fn claude_plan_mode_stays_active_and_exit_restores_the_pre_plan_mode() {
    for config in [false, true] {
        let mut probe = PlanProbe::with_config(ExecutionPolicy::Unconstrained, config).await;
        if config {
            set_claude_config_mode(&mut probe, "enter-plan", "plan").await;
        } else {
            probe
                .commands
                .send(CommandRequest::SetSessionMode {
                    request_id: "enter-plan".into(),
                    mode_id: "plan".into(),
                })
                .await
                .unwrap();
            let entering = probe.message().await;
            assert_eq!(entering["params"]["modeId"], "plan");
            probe.result(&entering, json!({})).await;
            loop {
                if let RuntimeEvent::SessionModeApplied { request_id, .. } = probe.event().await {
                    assert_eq!(request_id, "enter-plan");
                    break;
                }
            }
        }
        // Entering Plan must not immediately send another request that
        // restores the launch policy.
        probe.no_message().await;
        probe
            .commands
            .send(CommandRequest::RestoreExecutionMode {
                request_id: "exit-plan".into(),
            })
            .await
            .unwrap();
        let restoring = probe.message().await;
        if config {
            assert_eq!(restoring["method"], "session/set_config_option");
            assert_eq!(restoring["params"]["value"], "bypassPermissions");
        } else {
            assert_eq!(restoring["method"], "session/set_mode");
            assert_eq!(restoring["params"]["modeId"], "bypassPermissions");
        }
        while let Ok(event) = probe.events.try_recv() {
            assert!(!matches!(event, RuntimeEvent::ConfigApplied { .. }));
        }
        probe
            .result(
                &restoring,
                if config {
                    json!({"configOptions": mode_config("bypassPermissions")})
                } else {
                    json!({})
                },
            )
            .await;
        loop {
            if let RuntimeEvent::ConfigApplied {
                request_id,
                key,
                value,
                ..
            } = probe.event().await
            {
                assert_eq!(request_id, "exit-plan");
                assert_eq!(key, "mode");
                assert_eq!(value, "bypassPermissions");
                break;
            }
        }
        probe.no_message().await;
        probe.close().await;
    }
}

#[tokio::test]
async fn claude_plan_exit_does_not_force_bypass_when_the_prior_mode_was_lower() {
    let mut probe = PlanProbe::with_config(ExecutionPolicy::Unconstrained, true).await;
    set_claude_config_mode(&mut probe, "before-plan", "acceptEdits").await;
    set_claude_config_mode(&mut probe, "enter-plan", "plan").await;
    probe
        .commands
        .send(CommandRequest::RestoreExecutionMode {
            request_id: "exit-plan".into(),
        })
        .await
        .unwrap();
    let restoring = probe.message().await;
    assert_eq!(restoring["method"], "session/set_config_option");
    assert_eq!(restoring["params"]["value"], "acceptEdits");
    probe
        .result(
            &restoring,
            json!({"configOptions": mode_config("acceptEdits")}),
        )
        .await;
    loop {
        if let RuntimeEvent::ConfigApplied { value, .. } = probe.event().await {
            assert_eq!(value, "acceptEdits");
            break;
        }
    }
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn explicit_mode_change_during_plan_is_not_reverted_at_plan_exit() {
    let mut probe = PlanProbe::with_config(ExecutionPolicy::Unconstrained, true).await;
    set_claude_config_mode(&mut probe, "enter-plan", "plan").await;
    set_claude_config_mode(&mut probe, "user-mode", "acceptEdits").await;
    probe
        .commands
        .send(CommandRequest::RestoreExecutionMode {
            request_id: "exit-plan".into(),
        })
        .await
        .unwrap();
    loop {
        if let RuntimeEvent::ConfigApplied {
            request_id, value, ..
        } = probe.event().await
        {
            assert_eq!(request_id, "exit-plan");
            assert_eq!(value, "acceptEdits");
            break;
        }
    }
    probe.no_message().await;
    probe.close().await;
}

#[tokio::test]
async fn a_refused_claude_plan_exit_reports_failure_without_continuing() {
    let mut probe = PlanProbe::with_config(ExecutionPolicy::ConfiguredApprovals, true).await;
    set_claude_config_mode(&mut probe, "enter-plan", "plan").await;
    probe
        .commands
        .send(CommandRequest::RestoreExecutionMode {
            request_id: "exit-plan".into(),
        })
        .await
        .unwrap();
    let restoring = probe.message().await;
    // An acknowledgement that still reports Plan is not a successful exit.
    probe
        .result(&restoring, json!({"configOptions": mode_config("plan")}))
        .await;
    loop {
        match probe.event().await {
            RuntimeEvent::ConfigApplied { .. } => panic!("unconfirmed mode must not succeed"),
            RuntimeEvent::CommandRejected {
                request_id,
                message,
                ..
            } => {
                assert_eq!(request_id, "exit-plan");
                assert!(message.contains("reports"), "{message}");
                break;
            }
            _ => {}
        }
    }
    probe.no_message().await;
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
    driver: tokio::task::JoinHandle<Result<Option<SessionRestart>>>,
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
        Self::with_harness(policy, config, HarnessKind::Claude).await
    }

    async fn with_harness(policy: ExecutionPolicy, config: bool, harness: HarnessKind) -> Self {
        let (client, agent) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client);
        let (agent_read, agent_write) = tokio::io::split(agent);
        let (commands, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(128);
        let spec = LaunchSpec {
            bridge_spec_path: None,
            subagent_policy: mj_core::subagent::SubagentPolicy::Native,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "plan-probe".into(),
            args: vec![],
            environment: BTreeMap::new(),
            cwd: PathBuf::from("/workspace"),
            additional_directories: vec![],
            extra_mcp_servers: vec![],
            project_memory: None,
            resume_session: None,
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            initial_model: None,
            harness,
            execution_policy: policy,
            acp_activity: AcpActivityClock::default(),
            step_clock: StepClock::default(),
            tools_in_flight: Default::default(),
            turn_context: Default::default(),
            verdict: Some(crate::acp::VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            stall_policy: None,
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
        probe.result(&init, json!({
            "protocolVersion": 1,
            "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["nativeSubagentSessions"]}}}
        })).await;
        let new = probe.message().await;
        assert_eq!(new["method"], "session/new");
        probe
            .result(
                &new,
                json!({"sessionId": "plan-session", "configOptions": if config {mode_config("plan")} else {json!([])}, "modes": {
                    "currentModeId": "plan", "availableModes": [
                        {"id": "plan", "name": "Plan"}, {"id": "auto", "name": "Auto"},
                        {"id": "bypassPermissions", "name": "Bypass"},
                        {"id": "agent", "name": "Agent"},
                        {"id": "agent-full-access", "name": "Full access"},
                        {"id": "allowAll", "name": "Allow all"}
                    ]
                }}),
            )
            .await;
        let Some(enforced) = harness
            .execution_enforcement(policy)
            .and_then(|mode| mode.acp_mode())
        else {
            return probe;
        };
        let mode = probe.message().await;
        if config {
            assert_eq!(mode["method"], "session/set_config_option");
            assert_eq!(mode["params"]["value"], enforced);
            probe
                .result(&mode, json!({"configOptions": mode_config(enforced)}))
                .await;
        } else {
            assert_eq!(mode["method"], "session/set_mode");
            assert_eq!(mode["params"]["modeId"], enforced);
            probe.result(&mode, json!({})).await;
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

    /// Finish a prompt the way a working harness does: one line of answer,
    /// then the result. A turn that produces nothing at all is reported as
    /// unanswered rather than finished (#970).
    async fn answered(&mut self, request: &Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "plan-session",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "done"},
                },
            },
        }))
        .await;
        self.result(request, json!({"stopReason": "end_turn"}))
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
        self.answer_with(id, implement()).await
    }

    async fn answer_with(
        &mut self,
        id: &str,
        answer: ElicitationResponse,
    ) -> std::result::Result<(), String> {
        let (resolved, response) = oneshot::channel();
        self.commands
            .send(CommandRequest::ResolveElicitation {
                elicitation_id: id.into(),
                response: answer,
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
                ..
            } = self.event().await
            {
                assert_eq!(request_id, "original-prompt");
                return stop_reason;
            }
        }
    }

    async fn sdk_result(&mut self, message: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "_claude/sdkMessage",
            "params": {"sessionId": "plan-session", "message": message},
        }))
        .await;
    }

    async fn claude_result(&mut self) -> mj_core::acp::ClaudeTurnResult {
        loop {
            if let RuntimeEvent::ClaudeTurnResult(result) = self.event().await {
                return result;
            }
        }
    }

    /// What the relay coordinator does with a result that would end the
    /// running prompt: hand it to the prompt loop.
    async fn release(&mut self, result: &mj_core::acp::ClaudeTurnResult) {
        self.commands
            .send(CommandRequest::ReleasePrompt {
                request_id: "original-prompt".into(),
                received: result.received,
                stop_reason: result
                    .prompt_stop_reason()
                    .expect("the result would end a prompt"),
                usage: Some(result.usage.token_usage()),
            })
            .await
            .unwrap();
    }

    async fn no_finish(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            assert!(
                !matches!(event, RuntimeEvent::PromptFinished { .. }),
                "the prompt must keep running: {event:?}"
            );
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
    probe.answered(&continuation).await;
    assert_eq!(probe.finished().await, "EndTurn");
    probe.no_message().await;
    probe.close().await;
}

/// Claude answers a cancelled plan review with "Tool use aborted", so the
/// planning cycle can end with an ordinary result rather than an interruption
/// report. Neither that result nor a late relay of it may end the prompt the
/// approved plan continues; the implementation cycle's result ends it.
#[tokio::test]
async fn a_planning_result_never_ends_the_prompt_an_approved_plan_continues() {
    let result = json!({
        "type": "result", "subtype": "success", "is_error": false, "num_turns": 3,
        "stop_reason": "end_turn", "result": "done",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "origin": {"kind": "human"},
    });
    for relayed_late in [false, true] {
        let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
        let (prompt, _, _) = probe.approve("Plan", &["exit-plan-auto", "reject"]).await;
        probe.sdk_result(result.clone()).await;
        let planning = probe.claude_result().await;
        if !relayed_late {
            probe.release(&planning).await;
            probe.no_finish().await;
        }
        probe
            .result(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        let mode = probe.message().await;
        assert_eq!(mode["method"], "session/set_mode");
        probe.result(&mode, json!({})).await;
        let continuation = probe.message().await;
        assert_eq!(continuation["method"], "session/prompt");
        if relayed_late {
            probe.release(&planning).await;
            probe.no_finish().await;
        }
        probe
            .send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": "plan-session",
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "implemented"}},
            }}))
            .await;
        probe.sdk_result(result.clone()).await;
        let implementation = probe.claude_result().await;
        probe.release(&implementation).await;
        assert_eq!(probe.finished().await, "EndTurn", "{relayed_late}");
        probe.no_message().await;
        probe
            .result(&continuation, json!({"stopReason": "end_turn"}))
            .await;
        probe.no_finish().await;
        probe.close().await;
    }
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
                if message.contains("could not restore the pre-Plan mode") =>
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
            Some(super::SessionRestart::Resume("plan-session".into()))
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
            probe.answered(&continuation).await;
            assert_eq!(probe.finished().await, "EndTurn");
        } else {
            assert_eq!(probe.finished().await, "error");
        }
        probe.no_message().await;
        probe.close().await;
    }
}

#[tokio::test]
async fn close_is_applied_when_the_harness_lacks_session_close() {
    for (code, applied) in [(-32601, true), (-32603, false)] {
        let mut probe = PlanProbe::new(ExecutionPolicy::ConfiguredApprovals).await;
        probe
            .commands
            .send(CommandRequest::Close {
                request_id: "close".into(),
            })
            .await
            .unwrap();
        let mut close = probe.message().await;
        if close["method"] == "session/cancel" {
            close = probe.message().await;
        }
        assert_eq!(close["method"], "session/close");
        probe
            .send(json!({
                "jsonrpc": "2.0", "id": close["id"],
                "error": {"code": code, "message": "Method not found"}
            }))
            .await;

        let mut warned = false;
        let mut outcome = None;
        while outcome.is_none() {
            match probe.event().await {
                RuntimeEvent::Warning { message } if message.contains("session/close") => {
                    warned = true;
                }
                event @ (RuntimeEvent::CloseApplied { .. }
                | RuntimeEvent::CommandRejected { .. }) => outcome = Some(event),
                _ => {}
            }
        }
        match outcome.unwrap() {
            RuntimeEvent::CloseApplied { request_id } => {
                assert!(applied, "error {code} must not report the close as applied");
                assert_eq!(request_id, "close");
                assert!(warned, "an unimplemented session/close must be reported");
            }
            RuntimeEvent::CommandRejected { request_id, .. } => {
                assert!(!applied, "method not found must apply the close");
                assert_eq!(request_id, "close");
            }
            event => panic!("unexpected close outcome: {event:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), &mut probe.driver)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn unconstrained_tool_permissions_are_auto_approved_for_every_harness() {
    let once = json!({"optionId": "one-time-id", "name": "Approve", "kind": "allow_once"});
    let always = json!({"optionId": "persistent-id", "name": "Approve", "kind": "allow_always"});
    let reject = json!({"optionId": "reject", "name": "No", "kind": "reject_once"});
    for harness in HarnessKind::ALL {
        let mut probe =
            PlanProbe::with_harness(ExecutionPolicy::Unconstrained, false, harness).await;
        for (options, expected) in [
            (
                vec![always.clone(), reject.clone(), once.clone()],
                "one-time-id",
            ),
            (vec![once.clone(), always.clone()], "one-time-id"),
            (vec![reject.clone(), always.clone()], "persistent-id"),
        ] {
            // Claude's live requests both included wildcard deletion in /tmp/snip.
            // Display names deliberately agree: approval must use kind and ID.
            probe
                .send(json!({
                    "jsonrpc": "2.0", "id": "delete-ask", "method": "session/request_permission",
                    "params": {
                        "sessionId": "plan-session",
                        "toolCall": {
                            "toolCallId": "delete-tool", "kind": "execute",
                            "title": "cd /tmp/snip && rm -f *",
                            "rawInput": {"command": "cd /tmp/snip && rm -f *"}
                        },
                        "options": options
                    }
                }))
                .await;
            let answer = probe.message().await;
            assert_eq!(answer["id"], "delete-ask");
            assert_eq!(answer["result"]["outcome"]["outcome"], "selected");
            assert_eq!(answer["result"]["outcome"]["optionId"], expected);
        }
        assert!(
            probe.answer("tool-permission-1").await.is_err(),
            "auto-approval must not leave a pending form"
        );
        probe.close().await;
        while let Ok(event) = probe.events.try_recv() {
            assert!(
                !matches!(
                    event,
                    RuntimeEvent::ElicitationRequested { .. } | RuntimeEvent::Warning { .. }
                ),
                "unexpected permission event: {event:?}"
            );
        }
    }
}

#[tokio::test]
async fn unconstrained_permission_without_allow_reports_error_without_waiting() {
    for options in [
        json!([]),
        json!([{"optionId": "reject", "name": "No", "kind": "reject_once"}]),
    ] {
        let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
        probe.send(json!({
            "jsonrpc": "2.0", "id": "no-allow", "method": "session/request_permission",
            "params": {
                "sessionId": "plan-session",
                "toolCall": {"toolCallId": "no-allow-tool", "kind": "execute", "title": "Run command"},
                "options": options
            }
        })).await;
        let answer = probe.message().await;
        assert_eq!(answer["id"], "no-allow");
        assert_eq!(answer["error"]["code"], -32602);
        assert!(
            answer["error"]["data"]
                .as_str()
                .unwrap()
                .contains("without offering an allow response")
        );
        assert!(probe.answer("tool-permission-1").await.is_err());
        probe.close().await;
        let mut warned = false;
        while let Ok(event) = probe.events.try_recv() {
            match event {
                RuntimeEvent::Warning { message } => {
                    assert!(
                        message.contains("Claude Code")
                            && message.contains("plan-session")
                            && message.contains("no-allow-tool"),
                        "{message}"
                    );
                    warned = true;
                }
                RuntimeEvent::ElicitationRequested { .. } => {
                    panic!("an unanswerable request must not leave a form")
                }
                _ => {}
            }
        }
        assert!(warned, "the malformed request must be reported");
    }
}

#[tokio::test]
async fn unconstrained_native_child_tool_permissions_are_auto_approved() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    probe
        .send(json!({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "plan-session", "update": {
                "sessionUpdate": "subagent_spawned", "subagentSessionId": "child",
                "name": "worker", "task": "inspect", "capabilities": {}
            }}
        }))
        .await;
    loop {
        if let RuntimeEvent::NativeAgent { .. } = probe.event().await {
            break;
        }
    }
    probe
        .send(json!({
            "jsonrpc": "2.0", "id": "child-ask", "method": "session/request_permission",
            "params": {
                "sessionId": "child",
                "toolCall": {"toolCallId": "child-tool", "kind": "execute", "title": "Run command"},
                "options": [{"optionId": "allow-once", "name": "Yes", "kind": "allow_once"}]
            }
        }))
        .await;
    let answer = probe.message().await;
    assert_eq!(answer["id"], "child-ask");
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow-once");
    probe.close().await;
    while let Ok(event) = probe.events.try_recv() {
        assert!(
            !matches!(
                event,
                RuntimeEvent::ElicitationRequested { .. } | RuntimeEvent::Warning { .. }
            ),
            "unexpected child permission event: {event:?}"
        );
    }
}

#[tokio::test]
async fn unconstrained_plan_permissions_still_wait_for_approval_for_every_harness() {
    for harness in HarnessKind::ALL {
        let mut probe =
            PlanProbe::with_harness(ExecutionPolicy::Unconstrained, false, harness).await;
        probe
            .send(json!({
                "jsonrpc": "2.0", "id": "plan-ask", "method": "session/request_permission",
                "params": permission("Implement the parser.", &["bypassPermissions", "reject"])
            }))
            .await;
        let request = loop {
            match probe.event().await {
                RuntimeEvent::ElicitationRequested { request } => break request,
                RuntimeEvent::Warning { message } => {
                    panic!("plan approval must not warn: {message}")
                }
                _ => {}
            }
        };
        assert!(request.id.starts_with("plan-review-"));
        probe.no_message().await;
        probe
            .answer_with(&request.id, ElicitationResponse::Decline)
            .await
            .unwrap();
        let answer = probe.message().await;
        assert_eq!(answer["id"], "plan-ask");
        assert_eq!(answer["result"]["outcome"]["optionId"], "reject");
        probe.close().await;
    }
}

#[tokio::test]
async fn unconstrained_user_questions_still_wait_for_an_answer() {
    let mut probe = PlanProbe::new(ExecutionPolicy::Unconstrained).await;
    probe
        .send(json!({
            "jsonrpc": "2.0", "id": "question", "method": "elicitation/create",
            "params": {
                "sessionId": "plan-session", "mode": "form", "message": "Which file?",
                "requestedSchema": {"type": "object", "required": ["file"], "properties": {
                    "file": {"type": "string", "title": "File", "enum": ["a.c", "b.c"]}
                }}
            }
        }))
        .await;
    let request = loop {
        if let RuntimeEvent::ElicitationRequested { request } = probe.event().await {
            break request;
        }
    };
    assert_eq!(request.message, "Which file?");
    probe.no_message().await;
    probe
        .answer_with(
            &request.id,
            ElicitationResponse::Accept {
                content: BTreeMap::from([("file".into(), ElicitationValue::String("b.c".into()))]),
            },
        )
        .await
        .unwrap();
    let answer = probe.message().await;
    assert_eq!(answer["id"], "question");
    assert_eq!(answer["result"]["action"], "accept");
    assert_eq!(answer["result"]["content"]["file"], "b.c");
    probe.close().await;
}

/// A tool permission request that is not a plan review must reach the user as
/// a form instead of being cancelled, which the adapter reports to the agent
/// as "Tool use aborted".
#[tokio::test]
async fn a_non_plan_permission_request_is_answered_by_the_user() {
    let mut probe = PlanProbe::new(ExecutionPolicy::ConfiguredApprovals).await;
    probe
        .send(json!({
            "jsonrpc": "2.0", "id": "permission-2", "method": "session/request_permission",
            "params": {
                "sessionId": "plan-session",
                "toolCall": {
                    "toolCallId": "fetch-1", "kind": "fetch",
                    "title": "Fetch https://example.com/docs"
                },
                "options": [
                    {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
                    {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}
                ]
            }
        }))
        .await;
    let request = loop {
        if let RuntimeEvent::ElicitationRequested { request } = probe.event().await {
            break request;
        }
    };
    assert!(
        request.id.starts_with("tool-permission-"),
        "a generic permission form, not a plan review: {}",
        request.id
    );
    assert_eq!(
        request.message,
        "Claude Code requests permission:\nFetch https://example.com/docs"
    );
    let field = &request.fields[0];
    assert_eq!(field.id, "choice");
    let ElicitationFieldKind::SingleSelect { options, .. } = &field.kind else {
        panic!("a permission form offers the harness options as a select")
    };
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        ["allow_once", "allow_always", "reject_once"]
    );

    let (resolved, response) = oneshot::channel();
    probe
        .commands
        .send(CommandRequest::ResolveElicitation {
            elicitation_id: request.id,
            response: ElicitationResponse::Accept {
                content: BTreeMap::from([(
                    "choice".into(),
                    ElicitationValue::String("allow_once".into()),
                )]),
            },
            resolved,
        })
        .await
        .unwrap();
    assert_eq!(response.await.unwrap(), Ok(()));
    let answer = probe.message().await;
    assert_eq!(answer["id"], "permission-2");
    assert_eq!(answer["result"]["outcome"]["outcome"], "selected");
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow_once");
    probe.close().await;
}

/// Claude's ExitPlanMode request exactly as claude-agent-acp 0.84.0 sends it
/// to Mjolnir, an AIR client, when Claude wrote no plan file: the tool call has
/// no kind and the input no plan, so it is not a plan review. The Guardian
/// shape without a plan was captured from a live worker.log. The options are
/// what `buildExitPlanModePermissionOptions` builds, stably sorted by kind
/// (allow once, allow always, reject): the mode the session was in before
/// Plan leads the elevated ones (Auto for Guardian, bypass for YOLO), and a
/// plan text adds a clear-context option for that mode before them.
fn bridge_exit_plan_request(policy: ExecutionPolicy, plan: bool) -> Value {
    let mut options = vec![
        json!({"optionId": "exit-plan-default", "name": "Yes, manually approve edits", "kind": "allow_once"}),
    ];
    if plan {
        options.push(if policy.is_unconstrained() {
            json!({"optionId": "exit-plan-clear-bypass", "name": "Yes, clear context (12% used) and bypass permissions", "kind": "allow_always"})
        } else {
            json!({"optionId": "exit-plan-clear-auto", "name": "Yes, clear context (12% used) and use auto mode", "kind": "allow_always"})
        });
    }
    let auto = json!({"optionId": "exit-plan-auto", "name": "Yes, and use auto mode", "kind": "allow_always"});
    let bypass = json!({"optionId": "exit-plan-bypass", "name": "Yes, and bypass permissions", "kind": "allow_always"});
    if policy.is_unconstrained() {
        options.extend([bypass, auto]);
    } else {
        options.extend([auto, bypass]);
    }
    options.push(json!({"optionId": "reject", "name": "No, keep planning", "kind": "reject_once"}));
    json!({
        "sessionId": "plan-session",
        "toolCall": {
            "toolCallId": "toolu_exit_plan",
            "title": "Approve Plan",
            "rawInput": if plan {json!({"plan": "Add one line."})} else {json!({})}
        },
        "options": options,
        "_meta": {"jetbrains": {"air": {"version": 1, "permission": {"version": 1, "title": "Ready to code?"}}}}
    })
}

fn offered(request: &Value, policy: ExecutionPolicy) -> Vec<(String, String)> {
    let request: RequestPermissionRequest = serde_json::from_value(request.clone()).unwrap();
    permission_choices(&request, HarnessKind::Claude, policy)
        .into_iter()
        .map(|choice| (choice.option_id.to_string(), choice.title))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(id, title)| ((*id).to_owned(), (*title).to_owned()))
        .collect()
}

#[test]
fn claude_plan_approval_offers_yes_under_the_session_policy_and_no_bypass_in_guardian() {
    let guardian = ExecutionPolicy::ConfiguredApprovals;
    let yolo = ExecutionPolicy::Unconstrained;
    assert!(!is_plan_permission(
        &serde_json::from_value(bridge_exit_plan_request(guardian, false)).unwrap()
    ));
    assert_eq!(
        offered(&bridge_exit_plan_request(guardian, false), guardian),
        pairs(&[
            ("exit-plan-auto", "Yes"),
            ("exit-plan-default", "Yes, manually approve edits"),
            ("reject", "No, keep planning"),
        ])
    );
    assert_eq!(
        offered(&bridge_exit_plan_request(guardian, true), guardian),
        pairs(&[
            ("exit-plan-auto", "Yes"),
            ("exit-plan-default", "Yes, manually approve edits"),
            ("exit-plan-clear-auto", "Yes, clear context"),
            ("reject", "No, keep planning"),
        ])
    );
    // A YOLO session keeps Auto as a deliberate step down.
    assert_eq!(
        offered(&bridge_exit_plan_request(yolo, false), yolo),
        pairs(&[
            ("exit-plan-bypass", "Yes"),
            ("exit-plan-default", "Yes, manually approve edits"),
            ("exit-plan-auto", "Yes, and use auto mode"),
            ("reject", "No, keep planning"),
        ])
    );
    assert_eq!(
        offered(&bridge_exit_plan_request(yolo, true), yolo),
        pairs(&[
            ("exit-plan-bypass", "Yes"),
            ("exit-plan-default", "Yes, manually approve edits"),
            ("exit-plan-clear-bypass", "Yes, clear context"),
            ("exit-plan-auto", "Yes, and use auto mode"),
            ("reject", "No, keep planning"),
        ])
    );
    // A Guardian session whose bridge led with bypass (Plan entered from a
    // mode set by hand) still gets no option above its policy.
    assert_eq!(
        offered(&bridge_exit_plan_request(yolo, true), guardian),
        pairs(&[
            ("exit-plan-auto", "Yes"),
            ("exit-plan-default", "Yes, manually approve edits"),
            ("reject", "No, keep planning"),
        ])
    );
}

/// The published form and the accepted answers come from the same decision:
/// Yes selects the bridge option for the session's policy, and an answer
/// naming the hidden bypass option is refused without reaching Claude.
#[tokio::test]
async fn claude_plan_approval_form_selects_the_policy_mode_and_refuses_hidden_bypass() {
    for (policy, yes) in [
        (ExecutionPolicy::ConfiguredApprovals, "exit-plan-auto"),
        (ExecutionPolicy::Unconstrained, "exit-plan-bypass"),
    ] {
        let mut probe = PlanProbe::new(policy).await;
        probe
            .send(json!({
                "jsonrpc": "2.0", "id": "exit-plan", "method": "session/request_permission",
                "params": bridge_exit_plan_request(policy, false)
            }))
            .await;
        let request = loop {
            match probe.event().await {
                RuntimeEvent::ElicitationRequested { request } => break request,
                RuntimeEvent::Warning { message } => {
                    panic!("plan approval must not warn: {message}")
                }
                _ => {}
            }
        };
        let ElicitationFieldKind::SingleSelect { options, .. } = &request.fields[0].kind else {
            panic!("the approval is a select")
        };
        assert_eq!(options[0].value, yes);
        assert_eq!(options[0].title, "Yes");
        assert!(
            options
                .iter()
                .all(|option| !option.title.contains("bypass"))
        );
        let choose = |value: &str| ElicitationResponse::Accept {
            content: BTreeMap::from([("choice".into(), ElicitationValue::String(value.into()))]),
        };
        if !policy.is_unconstrained() {
            let (resolved, response) = oneshot::channel();
            probe
                .commands
                .send(CommandRequest::ResolveElicitation {
                    elicitation_id: request.id.clone(),
                    response: choose("exit-plan-bypass"),
                    resolved,
                })
                .await
                .unwrap();
            assert!(response.await.unwrap().is_err());
            probe.no_message().await;
        }
        let (resolved, response) = oneshot::channel();
        probe
            .commands
            .send(CommandRequest::ResolveElicitation {
                elicitation_id: request.id,
                response: choose(yes),
                resolved,
            })
            .await
            .unwrap();
        assert_eq!(response.await.unwrap(), Ok(()));
        let answer = probe.message().await;
        assert_eq!(answer["id"], "exit-plan");
        assert_eq!(answer["result"]["outcome"]["optionId"], yes);
        probe.close().await;
    }
}
