//! Thor advisor mode: a transcript-first, MCP-driven supervisor for normal
//! `mj` turns.
//!
//! Rust owns the ACP connection, bounded execution, transcript projection, and
//! completion guardrails. Thor owns the actual orchestration: choosing whether
//! to delegate, steering workers, judging reviews, and writing the answer.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, EnvVariable, McpServer, McpServerStdio, SessionUpdate, StopReason,
    TextContent, ToolCallContent, ToolCallId, Usage,
};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use tempfile::NamedTempFile;
use tokio::sync::{mpsc, watch};

use crate::acp;
use crate::config::Config;
use crate::event::{PromptImage, UiEvent, content_block_text};
use crate::ragnarok::{AgentHandle, Launch, TurnEvent};

const THOR_ORCHESTRATION_TIMEOUT: Duration = Duration::from_secs(40 * 60);
pub(crate) const ADVISOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

const MCP_PARENT_AGENT_SOURCE_ENV: &str = "MJ_MCP_PARENT_AGENT_SOURCE_ID";
const MCP_ADVISOR_MODE_ENV: &str = "MJ_MCP_ADVISOR_MODE";
const MCP_INHERITED_IMAGES_MANIFEST_ENV: &str = "MJ_MCP_INHERITED_IMAGES_MANIFEST";
const MCP_COMPLETION_MARKER_ENV: &str = "MJ_MCP_COMPLETION_MARKER";
const MCP_COMPLETION_TOKEN_ENV: &str = "MJ_MCP_COMPLETION_TOKEN";

#[derive(Debug, Clone)]
pub(crate) struct AdvisorConfig {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub config_path: PathBuf,
    pub thor_agent_source_id: String,
    pub thor_launch: Launch,
}

#[derive(Debug)]
pub(crate) struct AdvisorTurnResult {
    pub stop_reason: StopReason,
    pub usage: Option<Usage>,
}

/// Parent-owned proof that the attached MCP process, rather than a
/// JSON-looking transcript fragment from Thor, accepted completion.
struct CompletionMarker {
    file: NamedTempFile,
    token: String,
}

#[derive(Debug, Deserialize)]
struct CompletionReceipt {
    token: String,
    final_response: String,
}

impl CompletionMarker {
    fn new() -> Result<Self> {
        let file = NamedTempFile::new().context("create advisor completion marker")?;
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| {
            anyhow::anyhow!("generate advisor completion marker token: {error}")
        })?;
        Ok(Self {
            file,
            token: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        })
    }

    fn path(&self) -> &Path {
        self.file.path()
    }

    fn accepted_response(&self) -> Option<String> {
        let receipt: CompletionReceipt =
            serde_json::from_slice(&std::fs::read(self.path()).ok()?).ok()?;
        (receipt.token == self.token && !receipt.final_response.trim().is_empty())
            .then_some(receipt.final_response)
    }
}

/// Run one Thor-owned orchestration turn. The single Thor ACP prompt is free to
/// make arbitrarily shaped MCP calls within the server-side policy caps; Rust
/// does not impose a route → worker → review script of its own.
pub(crate) async fn run_turn(
    cfg: AdvisorConfig,
    user_prompt: String,
    images: Vec<PromptImage>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    abort: watch::Receiver<bool>,
) -> Result<AdvisorTurnResult> {
    emit_info(&ui_tx, "Thor advisor: supervising this turn");

    // The file remains alive until the MCP server has been torn down, so a
    // delegated worker can receive the exact attachments supplied to Thor.
    let image_manifest = write_image_manifest(&images)?;
    let completion_marker = CompletionMarker::new()?;
    let mcp_server = thor_mcp_server(
        std::env::current_exe().context("resolve current mj executable")?,
        &cfg.cwd,
        &cfg.additional_directories,
        &cfg.thor_agent_source_id,
        image_manifest.as_ref().map(NamedTempFile::path),
        completion_marker.path(),
        &completion_marker.token,
    )?;
    let saved_session_config = saved_session_config(&cfg.config_path, &cfg.thor_agent_source_id);

    let mut thor = AgentHandle::connect_with_saved_session_config(
        &cfg.thor_launch,
        &cfg.cwd,
        &cfg.additional_directories,
        abort,
        acp::RuntimeAccessMode::ReadOnly,
        saved_session_config,
        vec![mcp_server],
    )
    .await
    .context("Thor could not start")?;

    let mut bridge = AdvisorTranscriptBridge::default();
    let result = thor
        .prompt_with_images(
            thor_prompt(&user_prompt, !images.is_empty()),
            images,
            THOR_ORCHESTRATION_TIMEOUT,
            |event| forward_thor_event(&ui_tx, &mut bridge, &completion_marker, event),
        )
        .await;
    thor.dismiss().await;

    let result = result.context("Thor orchestration failed")?;
    if !turn_succeeded(result.stop) {
        bail!(
            "Thor stopped before completing the orchestration: {:?}",
            result.stop
        );
    }
    let Some(final_response) = completion_marker.accepted_response() else {
        bail!(
            "Thor ended without an MCP-verified completion receipt containing a user-facing answer; \
             delegated work is not accepted without a completed independent review"
        );
    };
    emit_agent_text(&ui_tx, final_response);

    Ok(AdvisorTurnResult {
        stop_reason: result.stop,
        usage: result.usage,
    })
}

fn saved_session_config(
    config_path: &Path,
    agent_source_id: &str,
) -> std::collections::HashMap<String, String> {
    Config::load(config_path)
        .ok()
        .and_then(|cfg| cfg.session_config.get(agent_source_id).cloned())
        .unwrap_or_default()
}

fn thor_mcp_server(
    mj_executable: PathBuf,
    cwd: &Path,
    additional_directories: &[PathBuf],
    thor_agent_source_id: &str,
    image_manifest: Option<&Path>,
    completion_marker: &Path,
    completion_token: &str,
) -> Result<McpServer> {
    let mut args = vec!["--cwd".to_string(), path_arg(cwd, "working directory")?];
    for directory in additional_directories {
        args.push("--additional-directory".to_string());
        args.push(path_arg(directory, "additional directory")?);
    }
    // `cwd` and `additional-directory` are parent CLI options, so they must
    // precede the `mcp` subcommand with the current Clap declaration.
    args.push("mcp".to_string());

    let mut env = vec![
        EnvVariable::new(MCP_PARENT_AGENT_SOURCE_ENV, thor_agent_source_id),
        EnvVariable::new(MCP_ADVISOR_MODE_ENV, "1"),
        EnvVariable::new(
            MCP_COMPLETION_MARKER_ENV,
            path_arg(completion_marker, "completion marker")?,
        ),
        EnvVariable::new(MCP_COMPLETION_TOKEN_ENV, completion_token),
    ];
    if let Some(image_manifest) = image_manifest {
        env.push(EnvVariable::new(
            MCP_INHERITED_IMAGES_MANIFEST_ENV,
            path_arg(image_manifest, "image manifest")?,
        ));
    }

    Ok(McpServer::Stdio(
        McpServerStdio::new("mj", mj_executable).args(args).env(env),
    ))
}

fn path_arg(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{label} is not valid UTF-8: {}", path.display()))
}

fn write_image_manifest(images: &[PromptImage]) -> Result<Option<NamedTempFile>> {
    if images.is_empty() {
        return Ok(None);
    }
    let mut file = NamedTempFile::new().context("create inherited image manifest")?;
    serde_json::to_writer(file.as_file_mut(), images).context("write inherited image manifest")?;
    file.flush().context("flush inherited image manifest")?;
    Ok(Some(file))
}

fn thor_prompt(user_prompt: &str, has_images: bool) -> String {
    let image_note = if has_images {
        "\nThe user attached images. Worker prompts inherit the original images by default. \
         Do not claim a reviewer saw them unless you deliberately attach them.\n"
    } else {
        ""
    };
    format!(
        "You are Thor, mjolnir's supervising advisor. You own this user turn from \
         decision through the final user-facing response. Rust provides bounded MCP \
         tools; you decide the actual workflow.\n\n\
         For a small factual, explanatory, or otherwise trivial request, prepare the exact \
         user-facing answer without opening an ACP worker connection. Do not send user-facing \
         prose before or after completion. Make complete_orchestration your final tool call with \
         mode `direct` and that exact answer in final_response. mj renders final_response after \
         server validation.\n\n\
         For implementation, edits, test repair, or substantial repository work:\n\
         1. Call select_ranked_agents with the original task. Use its recommended worker \
         and reviewer; never choose the Thor identity.\n\
         2. Connect the worker with purpose `worker`, then submit a precise implementation \
         prompt. Preserve unrelated changes, do not commit or push unless requested, and \
         run focused validation.\n\
         3. Poll from submit_prompt's since_seq and advance to each next_seq. Act on actual \
         progress; do not poll blindly.\n\
         4. Answer pending permissions promptly using only advertised option IDs and the \
         least privilege that safely permits the work.\n\
         5. If the worker drifts, stalls, exceeds scope, or claims completion without \
         evidence, cancel it, wait until its turn is terminal, then re-prompt or adjust \
         session configuration with a concrete correction.\n\
         6. After implementation, connect the distinct recommended candidate with purpose \
         `reviewer`. Submit its adversarial read-only review with `review_of` set to the exact \
         worker connection_id and turn_id you are auditing. The server binds that review to the \
         original task and current workspace; monitor it to completion.\n\
         7. Judge the review yourself. Reject speculative findings; send valid actionable \
         findings back to the worker and monitor any fix.\n\
         8. Make complete_orchestration your final tool call with mode `delegated` and the \
         exact user-facing answer in final_response; it refuses completion unless a worker and \
         independent reviewer completed successfully. Do not send user-facing prose before or \
         after the call. mj renders final_response after server validation and tears down nested \
         connections when this session closes.\n\n\
         Do not expose MCP JSON to the user. Give a concise final response describing the \
         result, validation, review/fixes, and any remaining risk. If bounded execution makes \
         completion unsafe, clean up and explain the concrete blocker instead of looping.\n\
         {image_note}\n\
         USER REQUEST:\n{user_prompt}"
    )
}

fn forward_thor_event(
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    bridge: &mut AdvisorTranscriptBridge,
    completion_marker: &CompletionMarker,
    event: TurnEvent,
) {
    match event {
        TurnEvent::RawSessionUpdate(update) => {
            // Preserve rich expandable MCP tool cards, then project structured
            // nested progress into visible worker/reviewer transcript data.
            let raw = *update;
            let completion_accepted = completion_marker.accepted_response().is_some();
            if should_forward_thor_session_update(&raw, completion_accepted) {
                let _ = ui_tx.send(UiEvent::SessionUpdate(namespace_tool_ids(raw.clone())));
            }
            bridge.observe_session_update(ui_tx, &raw);
        }
        TurnEvent::Permission { prompt, .. } => {
            let _ = ui_tx.send(UiEvent::PermissionRequest(*prompt));
        }
        TurnEvent::CancelPendingPermissions => {
            let _ = ui_tx.send(UiEvent::CancelPendingPermissions);
        }
        // Raw updates contain the complete versions of these values. Ignoring
        // their compact duplicates avoids duplicated transcript entries.
        TurnEvent::Message(_)
        | TurnEvent::Thought(_)
        | TurnEvent::Tool { .. }
        | TurnEvent::Note(_) => {}
    }
}

/// The verified receipt is the only user-facing delivery after completion.
/// Keep tool cards for observability and cleanup, but hide agent text/thoughts
/// generated after the terminal completion call so generic status boilerplate
/// cannot compete with the receipt-rendered answer.
fn should_forward_thor_session_update(update: &SessionUpdate, completion_accepted: bool) -> bool {
    !completion_accepted
        || !matches!(
            update,
            SessionUpdate::AgentMessageChunk(_) | SessionUpdate::AgentThoughtChunk(_)
        )
}

fn namespace_tool_ids(update: SessionUpdate) -> SessionUpdate {
    match update {
        SessionUpdate::ToolCall(mut tool_call) => {
            let old_id = tool_call.tool_call_id.to_string();
            tool_call.tool_call_id = ToolCallId::new(format!("thor-mcp-{old_id}"));
            tool_call.title = format!("Thor MCP · {}", tool_call.title);
            SessionUpdate::ToolCall(tool_call)
        }
        SessionUpdate::ToolCallUpdate(mut tool_call) => {
            let old_id = tool_call.tool_call_id.to_string();
            tool_call.tool_call_id = ToolCallId::new(format!("thor-mcp-{old_id}"));
            if let Some(title) = &mut tool_call.fields.title {
                *title = format!("Thor MCP · {title}");
            }
            SessionUpdate::ToolCallUpdate(tool_call)
        }
        update => update,
    }
}

#[derive(Default)]
struct AdvisorTranscriptBridge {
    seen_progress: HashSet<(String, u64, u64)>,
}

impl AdvisorTranscriptBridge {
    fn observe_session_update(
        &mut self,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        update: &SessionUpdate,
    ) {
        for value in tool_result_values(update) {
            self.observe_value(ui_tx, &value);
        }
    }

    fn observe_value(&mut self, ui_tx: &mpsc::UnboundedSender<UiEvent>, value: &Value) {
        if let Some(poll) = value_to_poll(value) {
            self.project_poll(ui_tx, poll);
        }
        for key in ["structuredContent", "structured_content", "result", "data"] {
            if let Some(nested) = value.get(key) {
                self.observe_value(ui_tx, nested);
            }
        }
    }

    fn project_poll(&mut self, ui_tx: &mpsc::UnboundedSender<UiEvent>, poll: NestedPoll) {
        let fresh: Vec<NestedProgressEntry> = poll
            .items
            .into_iter()
            .filter(|entry| {
                self.seen_progress
                    .insert((poll.connection_id.clone(), entry.turn_id, entry.seq))
            })
            .collect();
        if fresh.is_empty() {
            return;
        }

        let identity = poll
            .source_id
            .as_deref()
            .or(poll.candidate_id.as_deref())
            .unwrap_or(poll.connection_id.as_str());
        emit_info(ui_tx, format!("{} progress · {identity}", poll.purpose));
        for entry in fresh {
            match entry.item {
                NestedProgressItem::AgentMessage { text } => emit_agent_text(ui_tx, text),
                NestedProgressItem::AgentThought { text } => {
                    let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(
                        text_chunk(text),
                    )));
                }
                NestedProgressItem::ToolCall { title, status, .. }
                | NestedProgressItem::ToolCallUpdate {
                    title: Some(title),
                    status,
                    ..
                } => {
                    emit_info(
                        ui_tx,
                        format!(
                            "{} tool · {title} ({})",
                            poll.purpose,
                            status.unwrap_or_else(|| "updated".to_string())
                        ),
                    );
                }
                NestedProgressItem::ToolCallUpdate {
                    title: None,
                    status,
                    ..
                } => {
                    emit_info(
                        ui_tx,
                        format!(
                            "{} tool update ({})",
                            poll.purpose,
                            status.unwrap_or_else(|| "updated".to_string())
                        ),
                    );
                }
                NestedProgressItem::PermissionRequested { title, .. } => {
                    emit_info(
                        ui_tx,
                        format!("{} awaits permission · {title}", poll.purpose),
                    );
                }
                NestedProgressItem::Warning { message } => {
                    let _ = ui_tx.send(UiEvent::Warning(format!("{} · {message}", poll.purpose)));
                }
                NestedProgressItem::Info { message } => {
                    emit_info(ui_tx, format!("{} · {message}", poll.purpose));
                }
            }
        }
    }
}

fn tool_result_values(update: &SessionUpdate) -> Vec<Value> {
    let mut values = Vec::new();
    match update {
        SessionUpdate::ToolCall(call) => {
            if let Some(value) = call.raw_output.as_ref() {
                values.push(value.clone());
            }
            values.extend(content_json_values(&call.content));
        }
        SessionUpdate::ToolCallUpdate(update) => {
            if let Some(value) = update.fields.raw_output.as_ref() {
                values.push(value.clone());
            }
            if let Some(content) = update.fields.content.as_ref() {
                values.extend(content_json_values(content));
            }
        }
        _ => {}
    }
    values
}

fn content_json_values(content: &[ToolCallContent]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Content(block) => {
                serde_json::from_str(&content_block_text(&block.content)).ok()
            }
            _ => None,
        })
        .collect()
}

fn value_to_poll(value: &Value) -> Option<NestedPoll> {
    let poll: NestedPoll = serde_json::from_value(value.clone()).ok()?;
    (poll.schema == "mj.poll_progress.v1").then_some(poll)
}

#[derive(Debug, Deserialize)]
struct NestedPoll {
    schema: String,
    connection_id: String,
    purpose: String,
    source_id: Option<String>,
    candidate_id: Option<String>,
    #[serde(default)]
    items: Vec<NestedProgressEntry>,
}

#[derive(Debug, Deserialize)]
struct NestedProgressEntry {
    seq: u64,
    turn_id: u64,
    #[serde(flatten)]
    item: NestedProgressItem,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NestedProgressItem {
    AgentMessage {
        text: String,
    },
    AgentThought {
        text: String,
    },
    ToolCall {
        #[allow(dead_code)]
        id: String,
        title: String,
        #[allow(dead_code)]
        kind: String,
        status: Option<String>,
    },
    ToolCallUpdate {
        #[allow(dead_code)]
        id: String,
        title: Option<String>,
        #[allow(dead_code)]
        kind: Option<String>,
        status: Option<String>,
    },
    PermissionRequested {
        #[allow(dead_code)]
        perm_id: String,
        title: String,
    },
    Warning {
        message: String,
    },
    Info {
        message: String,
    },
}

fn emit_agent_text(ui_tx: &mpsc::UnboundedSender<UiEvent>, text: impl Into<String>) {
    let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
        text_chunk(text),
    )));
}

fn emit_info(ui_tx: &mpsc::UnboundedSender<UiEvent>, text: impl Into<String>) {
    let _ = ui_tx.send(UiEvent::Info(text.into()));
}

fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text.into())))
}

fn turn_succeeded(stop: StopReason) -> bool {
    matches!(
        stop,
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thor_prompt_describes_the_tool_control_loop_without_json_routing() {
        let prompt = thor_prompt("add a test", false);
        assert!(prompt.contains("select_ranked_agents"));
        assert!(prompt.contains("complete_orchestration"));
        assert!(prompt.contains("final_response"));
        assert!(prompt.contains("review_of"));
        assert!(prompt.contains("cancel it"));
        assert!(prompt.contains("final tool call"));
        assert!(prompt.contains("Do not send user-facing prose before or after"));
        assert!(!prompt.contains("Before your final answer"));
        assert!(!prompt.contains("Respond with ONLY one JSON"));
    }

    #[test]
    fn descriptor_keeps_parent_flags_before_mcp_and_sets_guard_environment() {
        let server = thor_mcp_server(
            PathBuf::from("/tmp/mj"),
            Path::new("/tmp/workspace"),
            &[PathBuf::from("/tmp/extra")],
            "thor-agent",
            None,
            Path::new("/tmp/completion-marker"),
            "marker-token",
        )
        .expect("descriptor");
        match server {
            McpServer::Stdio(server) => {
                assert_eq!(server.command, PathBuf::from("/tmp/mj"));
                assert_eq!(
                    server.args,
                    vec![
                        "--cwd",
                        "/tmp/workspace",
                        "--additional-directory",
                        "/tmp/extra",
                        "mcp",
                    ]
                );
                assert!(server.env.iter().any(|entry| {
                    entry.name == MCP_PARENT_AGENT_SOURCE_ENV && entry.value == "thor-agent"
                }));
                assert!(server.env.iter().any(|entry| {
                    entry.name == MCP_COMPLETION_MARKER_ENV
                        && entry.value == "/tmp/completion-marker"
                }));
                assert!(server.env.iter().any(|entry| {
                    entry.name == MCP_COMPLETION_TOKEN_ENV && entry.value == "marker-token"
                }));
            }
            _ => panic!("expected stdio MCP server"),
        }
    }

    #[test]
    fn image_manifest_round_trips_prompt_images() {
        let images = vec![PromptImage {
            data_base64: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
        }];
        let manifest = write_image_manifest(&images)
            .expect("manifest")
            .expect("nonempty image manifest");
        let loaded: Vec<PromptImage> =
            serde_json::from_slice(&std::fs::read(manifest.path()).expect("read manifest"))
                .expect("parse manifest");
        assert_eq!(loaded, images);
    }

    #[test]
    fn nested_poll_is_projected_once_per_connection_turn_and_sequence() {
        let mut bridge = AdvisorTranscriptBridge::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let value = serde_json::json!({
            "schema": "mj.poll_progress.v1",
            "connection_id": "conn-1",
            "purpose": "worker",
            "source_id": "worker-agent",
            "candidate_id": "candidate-1",
            "items": [{
                "seq": 1,
                "turn_id": 1,
                "type": "agent_message",
                "text": "working"
            }]
        });
        bridge.observe_value(&tx, &value);
        bridge.observe_value(&tx, &value);
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 2, "one header and one message only");
    }

    #[test]
    fn nested_poll_in_an_mcp_tool_result_reaches_the_transcript() {
        let mut bridge = AdvisorTranscriptBridge::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let poll = serde_json::json!({
            "schema": "mj.poll_progress.v1",
            "connection_id": "conn-1",
            "purpose": "worker",
            "source_id": "worker-agent",
            "items": [{
                "seq": 1,
                "turn_id": 1,
                "type": "agent_message",
                "text": "implemented the change"
            }]
        });
        let call = agent_client_protocol::schema::v1::ToolCall::new("tool-1", "poll_progress")
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(poll.to_string()),
            ))]);
        bridge.observe_session_update(&tx, &SessionUpdate::ToolCall(call));
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(events.len(), 2, "header and nested worker message");
    }

    #[test]
    fn post_completion_agent_text_is_hidden_but_tool_cards_remain_visible() {
        let message = SessionUpdate::AgentMessageChunk(text_chunk("No issue."));
        let thought = SessionUpdate::AgentThoughtChunk(text_chunk("I am done."));
        let tool = SessionUpdate::ToolCall(agent_client_protocol::schema::v1::ToolCall::new(
            "tool-1",
            "disconnect",
        ));

        assert!(!should_forward_thor_session_update(&message, true));
        assert!(!should_forward_thor_session_update(&thought, true));
        assert!(should_forward_thor_session_update(&tool, true));
        assert!(should_forward_thor_session_update(&message, false));
    }

    #[test]
    fn completion_receipt_requires_the_mcp_token_and_a_nonempty_response() {
        let marker = CompletionMarker::new().expect("marker");
        assert!(marker.accepted_response().is_none());
        std::fs::write(
            marker.path(),
            serde_json::json!({
                "token": &marker.token,
                "final_response": "The answer is four."
            })
            .to_string(),
        )
        .expect("write receipt");
        assert_eq!(
            marker.accepted_response().as_deref(),
            Some("The answer is four.")
        );
        std::fs::write(
            marker.path(),
            serde_json::json!({
                "token": "not-the-parent-token",
                "final_response": "forged"
            })
            .to_string(),
        )
        .expect("write forged receipt");
        assert!(marker.accepted_response().is_none());
    }
}
