//! A Claude prompt ends at the SDK result of the cycle that answered it, even
//! while the adapter holds its `session/prompt` reply. The agent side of the
//! ACP connection is driven by the test, and the test also stands in for the
//! relay coordinator: it hands every result that would end a prompt to the
//! prompt loop, which decides.

use super::*;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

const SESSION: &str = "claude-session";

struct ClaudeProbe {
    input: Lines<BufReader<ReadHalf<DuplexStream>>>,
    output: WriteHalf<DuplexStream>,
    commands: mpsc::Sender<CommandRequest>,
    events: mpsc::Receiver<RuntimeEvent>,
    driver: tokio::task::JoinHandle<Result<Option<SessionRestart>>>,
}

impl Drop for ClaudeProbe {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl ClaudeProbe {
    async fn new() -> Self {
        let (client, agent) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client);
        let (agent_read, agent_write) = tokio::io::split(agent);
        let (commands, mut requests) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(128);
        let spec = LaunchSpec {
            bridge_spec_path: None,
            subagent_mcp_socket: None,
            clear_context_request: None,
            context_restore: None,
            goal_recovery: Default::default(),
            command: "claude-probe".into(),
            args: vec![],
            environment: BTreeMap::new(),
            cwd: PathBuf::from("/workspace"),
            additional_directories: vec![],
            extra_mcp_servers: vec![],
            project_memory: None,
            resume_session: None,
            native_session_may_have_history: false,
            accepted_config: Default::default(),
            harness: HarnessKind::Claude,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
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
        probe
            .result(
                &init,
                json!({"protocolVersion": 1, "_meta": {"steering": {"supported": true}}}),
            )
            .await;
        let new = probe.message().await;
        assert_eq!(new["method"], "session/new");
        assert_eq!(
            new["params"]["_meta"]["claudeCode"]["emitRawSDKMessages"][1],
            json!({"type": "result"}),
            "the session must ask the adapter for SDK results"
        );
        probe
            .result(
                &new,
                json!({"sessionId": SESSION, "modes": {
                    "currentModeId": "default", "availableModes": [
                        {"id": "default", "name": "Default"}, {"id": "auto", "name": "Auto"},
                    ]
                }}),
            )
            .await;
        let mode = probe.message().await;
        assert_eq!(mode["method"], "session/set_mode");
        probe.result(&mode, json!({})).await;
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

    /// Nothing reaches the adapter for a moment. In particular, no
    /// `$/cancel_request` for a reply the adapter is holding.
    async fn no_message(&mut self) {
        let message = tokio::time::timeout(Duration::from_millis(50), self.input.next_line()).await;
        assert!(
            message.is_err(),
            "unexpected ACP message to the adapter: {message:?}"
        );
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

    async fn chunk(&mut self, text: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": SESSION, "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            }},
        }))
        .await;
    }

    async fn sdk_result(&mut self, message: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "_claude/sdkMessage",
            "params": {"sessionId": SESSION, "message": message},
        }))
        .await;
    }

    async fn prompt(&mut self, request_id: &str, text: &str) -> Value {
        self.commands
            .send(CommandRequest::Prompt {
                request_id: request_id.into(),
                prompt: vec![ContentBlock::Text(TextContent::new(text))],
            })
            .await
            .unwrap();
        let prompt = self.message().await;
        assert_eq!(prompt["method"], "session/prompt");
        prompt
    }

    async fn event(&mut self) -> RuntimeEvent {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("runtime event timed out")
            .expect("runtime stopped")
    }

    /// What the relay coordinator does with a result: hand one that would
    /// end a prompt to the prompt loop.
    async fn relay(&mut self, event: &RuntimeEvent, running_prompt: &str) {
        if let RuntimeEvent::ClaudeTurnResult(result) = event {
            self.release(running_prompt, result).await;
        }
    }

    async fn release(&mut self, request_id: &str, result: &mj_core::acp::ClaudeTurnResult) {
        if let Some(stop_reason) = result.prompt_stop_reason() {
            self.commands
                .send(CommandRequest::ReleasePrompt {
                    request_id: request_id.into(),
                    received: result.received,
                    stop_reason,
                    usage: Some(result.usage.token_usage()),
                })
                .await
                .unwrap();
        }
    }

    /// Relay results until `request_id` finishes, and return its stop reason
    /// and usage. Any other prompt finishing first fails the test.
    async fn finished(&mut self, request_id: &str) -> (String, Option<mj_core::usage::TokenUsage>) {
        loop {
            let event = self.event().await;
            if let RuntimeEvent::PromptFinished {
                request_id: finished,
                stop_reason,
                usage,
                ..
            } = &event
            {
                assert_eq!(finished, request_id, "the wrong prompt finished");
                return (stop_reason.clone(), usage.clone());
            }
            self.relay(&event, request_id).await;
        }
    }

    /// Relay results for a moment and require that no prompt finishes.
    async fn no_finish(&mut self, running_prompt: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            assert!(
                !matches!(event, RuntimeEvent::PromptFinished { .. }),
                "no prompt may finish yet: {event:?}"
            );
            self.relay(&event, running_prompt).await;
        }
    }

    async fn close(mut self) {
        self.commands
            .send(CommandRequest::Close {
                request_id: "close".into(),
            })
            .await
            .unwrap();
        let close = loop {
            let message = self.message().await;
            assert_ne!(message["method"], "$/cancel_request");
            if message["method"] == "session/close" {
                break message;
            }
        };
        self.result(&close, json!({})).await;
        let result = tokio::time::timeout(Duration::from_secs(5), &mut self.driver)
            .await
            .expect("the runtime exits after close")
            .expect("the runtime does not panic");
        assert!(result.expect("close succeeds").is_none());
    }
}

fn success(origin: &str) -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "num_turns": 2,
        "stop_reason": "end_turn",
        "result": "done",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 7,
            "cache_read_input_tokens": 1000,
            "cache_creation_input_tokens": 10,
        },
        "origin": {"kind": origin},
    })
}

/// The report of a cycle that a steer interrupted.
fn steer_interruption() -> Value {
    json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "num_turns": 1,
        "stop_reason": null,
        "errors": ["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],
        "usage": {"input_tokens": 50, "output_tokens": 3},
        "origin": {"kind": "human"},
        "queued_turn_count": 1,
    })
}

#[tokio::test]
async fn a_claude_prompt_ends_at_its_result_while_the_adapter_holds_the_reply() {
    let mut probe = ClaudeProbe::new().await;
    let first = probe.prompt("prompt-1", "start a background agent").await;
    probe.chunk("Started it; I will report back.").await;
    probe.sdk_result(success("human")).await;
    let (stop_reason, usage) = probe.finished("prompt-1").await;
    assert_eq!(stop_reason, "EndTurn");
    let usage = usage.expect("the result's usage is recorded");
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(usage.total_tokens, 1117);
    // The adapter still holds the reply. Keeping it alive means no
    // `$/cancel_request`, which the adapter would treat as a cancel.
    probe.no_message().await;

    // The loop left the prompt, so the next prompt goes out at once.
    let second = probe.prompt("prompt-2", "what did it find?").await;
    // The adapter settles the held turn when the next prompt arrives. That
    // late reply is discarded; it neither finishes prompt-1 again nor ends
    // prompt-2.
    probe
        .result(&first, json!({"stopReason": "end_turn"}))
        .await;
    probe.no_finish("prompt-2").await;
    probe.chunk("It found three files.").await;
    probe.sdk_result(success("human")).await;
    assert_eq!(probe.finished("prompt-2").await.0, "EndTurn");
    probe.no_message().await;
    probe
        .result(&second, json!({"stopReason": "end_turn"}))
        .await;
    probe.no_finish("none").await;
    probe.close().await;
}

#[tokio::test]
async fn the_adapter_reply_still_ends_a_prompt_that_no_result_ended() {
    let mut probe = ClaudeProbe::new().await;
    let first = probe.prompt("prompt-1", "/context").await;
    probe.chunk("Context: 12k tokens").await;
    probe
        .result(&first, json!({"stopReason": "end_turn"}))
        .await;
    assert_eq!(probe.finished("prompt-1").await.0, "EndTurn");
    // A result the coordinator relays after the reply won the race is for a
    // prompt that already ended.
    probe.sdk_result(success("human")).await;
    probe.no_finish("prompt-1").await;
    let second = probe.prompt("prompt-2", "carry on").await;
    probe.chunk("ok").await;
    probe
        .result(&second, json!({"stopReason": "end_turn"}))
        .await;
    assert_eq!(probe.finished("prompt-2").await.0, "EndTurn");
    probe.close().await;
}

#[tokio::test]
async fn a_result_that_arrived_before_the_prompt_was_sent_does_not_end_it() {
    let mut probe = ClaudeProbe::new().await;
    // A stray user cycle's result arrives while nothing is running. The
    // coordinator relays it late, after the next prompt went out.
    probe.sdk_result(success("human")).await;
    let stray = loop {
        if let RuntimeEvent::ClaudeTurnResult(result) = probe.event().await {
            break result;
        }
    };
    let prompt = probe.prompt("prompt-1", "go").await;
    probe.release("prompt-1", &stray).await;
    probe.no_finish("prompt-1").await;
    probe.chunk("done").await;
    probe.sdk_result(success("human")).await;
    assert_eq!(probe.finished("prompt-1").await.0, "EndTurn");
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    probe.close().await;
}

#[tokio::test]
async fn a_cancel_in_flight_lets_the_reply_end_the_prompt() {
    let mut probe = ClaudeProbe::new().await;
    let prompt = probe.prompt("prompt-1", "go").await;
    probe
        .commands
        .send(CommandRequest::Cancel {
            request_id: "cancel-1".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    assert_eq!(probe.message().await["method"], "session/cancel");
    probe.sdk_result(success("human")).await;
    probe.no_finish("prompt-1").await;
    probe
        .result(&prompt, json!({"stopReason": "cancelled"}))
        .await;
    assert_eq!(probe.finished("prompt-1").await.0, "Cancelled");
    probe.close().await;
}

#[tokio::test]
async fn background_and_interrupted_cycles_do_not_end_the_prompt() {
    let mut probe = ClaudeProbe::new().await;
    let prompt = probe.prompt("prompt-1", "go").await;
    probe.sdk_result(success("task-notification")).await;
    probe.sdk_result(steer_interruption()).await;
    probe.no_finish("prompt-1").await;
    probe.chunk("done").await;
    probe.sdk_result(success("human")).await;
    assert_eq!(probe.finished("prompt-1").await.0, "EndTurn");
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    probe.close().await;
}

async fn steer(probe: &mut ClaudeProbe) -> Value {
    probe
        .commands
        .send(CommandRequest::Steer {
            request_id: "steer-1".into(),
            active_prompt_id: "prompt-1".into(),
            steering_prompt: ClaimedSteeringPrompt {
                attachment_root: None,
                queued_command_id: "queued-1".into(),
                prompt: vec![ContentBlock::Text(TextContent::new("use the other file"))],
            },
        })
        .await
        .unwrap();
    let steering = probe.message().await;
    assert_eq!(steering["method"], SESSION_STEERING_METHOD);
    steering
}

async fn steer_applied(probe: &mut ClaudeProbe) {
    loop {
        match probe.event().await {
            RuntimeEvent::SteerApplied { request_id, .. } => {
                assert_eq!(request_id, "steer-1");
                return;
            }
            RuntimeEvent::PromptFinished { .. } => {
                panic!("the prompt finished before the steer was applied")
            }
            event => probe.relay(&event, "prompt-1").await,
        }
    }
}

#[tokio::test]
async fn the_steered_cycle_ends_the_prompt_whether_or_not_an_interruption_is_reported() {
    for interrupted in [true, false] {
        let mut probe = ClaudeProbe::new().await;
        let prompt = probe.prompt("prompt-1", "go").await;
        probe.chunk("Working on the first file").await;
        let steering = steer(&mut probe).await;
        probe
            .result(&steering, json!({"outcome": "injected"}))
            .await;
        steer_applied(&mut probe).await;
        if interrupted {
            probe.sdk_result(steer_interruption()).await;
        }
        probe.no_finish("prompt-1").await;
        probe.chunk("Switched to the other file.").await;
        probe.sdk_result(success("human")).await;
        assert_eq!(
            probe.finished("prompt-1").await.0,
            "EndTurn",
            "{interrupted}"
        );
        probe.no_message().await;
        probe
            .result(&prompt, json!({"stopReason": "end_turn"}))
            .await;
        probe.close().await;
    }
}

#[tokio::test]
async fn a_steer_still_waiting_for_its_acknowledgement_is_settled_before_the_prompt_ends() {
    let mut probe = ClaudeProbe::new().await;
    let prompt = probe.prompt("prompt-1", "go").await;
    let steering = steer(&mut probe).await;
    probe.chunk("done").await;
    probe.sdk_result(success("human")).await;
    let result = loop {
        if let RuntimeEvent::ClaudeTurnResult(result) = probe.event().await {
            break result;
        }
    };
    probe.release("prompt-1", &result).await;
    // The loop waits briefly for the steer's answer before ending the prompt.
    probe
        .result(&steering, json!({"outcome": "injected"}))
        .await;
    steer_applied(&mut probe).await;
    assert_eq!(probe.finished("prompt-1").await.0, "EndTurn");
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    probe.close().await;
}

/// Stop while Claude Code works on its own reaches the adapter as
/// `session/cancel`. The relay ends that turn at the interrupted cycle's
/// result, which the runtime forwards like any other.
#[tokio::test]
async fn stop_while_claude_works_on_its_own_sends_session_cancel() {
    let mut probe = ClaudeProbe::new().await;
    probe
        .chunk("Summarizing what the background agent found")
        .await;
    probe
        .commands
        .send(CommandRequest::Cancel {
            request_id: "stop-1".into(),
            steering_prompt: None,
        })
        .await
        .unwrap();
    assert_eq!(probe.message().await["method"], "session/cancel");
    loop {
        match probe.event().await {
            RuntimeEvent::CancelApplied { request_id } => {
                assert_eq!(request_id, "stop-1");
                break;
            }
            RuntimeEvent::CommandRejected { message, .. } => panic!("Stop was refused: {message}"),
            _ => {}
        }
    }
    let mut interrupted = success("task-notification");
    interrupted["subtype"] = json!("error_during_execution");
    interrupted["is_error"] = json!(true);
    probe.sdk_result(interrupted).await;
    loop {
        if let RuntimeEvent::ClaudeTurnResult(result) = probe.event().await {
            assert_eq!(result.origin_kind.as_deref(), Some("task-notification"));
            assert_eq!(result.prompt_stop_reason(), None);
            break;
        }
    }
    probe.close().await;
}

/// A local command such as `/context` makes no model call. The adapter sends
/// its text after the result and then answers the prompt, so the reply ends
/// the prompt and the text is recorded inside it.
#[tokio::test]
async fn a_local_command_ends_at_the_reply_after_its_text() {
    let mut probe = ClaudeProbe::new().await;
    let prompt = probe.prompt("prompt-1", "/context").await;
    let mut local = success("human");
    local["num_turns"] = json!(0);
    local["usage"]["output_tokens"] = json!(0);
    local["local_command"] = json!("/context");
    local["result"] = json!("Context: 12k of 200k tokens");
    probe.sdk_result(local).await;
    probe.chunk("Context: 12k of 200k tokens").await;
    probe
        .result(&prompt, json!({"stopReason": "end_turn"}))
        .await;
    let mut text_seen = false;
    loop {
        let event = probe.event().await;
        match &event {
            RuntimeEvent::SessionUpdate { update }
                if update["sessionUpdate"] == "agent_message_chunk" =>
            {
                text_seen = true;
            }
            RuntimeEvent::PromptFinished {
                request_id,
                stop_reason,
                ..
            } => {
                assert_eq!(request_id, "prompt-1");
                assert_eq!(stop_reason, "EndTurn");
                assert!(text_seen, "the command's text belongs to its prompt");
                break;
            }
            RuntimeEvent::ClaudeTurnResult(result) => {
                assert_eq!(
                    result.prompt_stop_reason(),
                    None,
                    "a cycle with no model call is left to the reply"
                );
            }
            _ => {}
        }
    }
    probe.close().await;
}
