//! Convert Mjolnir's canonical transcript into Brokk Observe envelopes and
//! retain envelopes that have not yet been delivered.
//!
//! This module is intentionally independent of Observe's HTTP API and of any
//! live worker. A caller can export a historical transcript, and the daemon
//! can later use the same conversion and spool after turns. No function here
//! evaluates policy or delays a tool call.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use mj_core::observe::{
    OBSERVE_COLLECTOR, OBSERVE_ENVELOPE_VERSION, ObserveCommit, ObserveEnvelope, ObserveGitMeta,
    ObserveMeta, canonical_tool_name, event_id, session_start_event,
};
use mj_core::state::{MaterializedExecutionState, MaterializedSession, TranscriptBody};
use mj_core::storage::UsagePage;
use mj_core::usage::UsageScope;

use crate::server::api::{TranscriptItemView, TranscriptResponse};

/// Session metadata that is stable across all events in one export.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObserveSession {
    pub id: String,
    pub parent_id: Option<String>,
    pub native_session_id: Option<String>,
    pub worker_id: Option<String>,
    pub harness: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub started_at_ms: i64,
    pub workspace_id: Option<String>,
    pub profile_id: Option<String>,
    pub target_id: Option<String>,
    pub runtime: Option<String>,
    pub git: Option<ObserveGitMeta>,
    pub commits: Vec<ObserveCommit>,
    pub historical: bool,
}

/// Convert all supported transcript items into an ordered Observe stream.
///
/// Tool calls are represented as a synthetic `PreToolUse` followed by the
/// result that the stored call shows. A turn end follows the last agent text
/// before the next user prompt. These are reconstructed events, not a claim
/// that Mjolnir observed a harness-native hook at those exact instants.
pub fn envelopes_for_transcript(
    session: &ObserveSession,
    transcript: &TranscriptResponse,
) -> Result<Vec<ObserveEnvelope>> {
    ensure!(
        !session.id.is_empty(),
        "Observe session id must not be empty"
    );
    ensure!(
        transcript.session_id == session.id,
        "transcript session {} does not match Observe session {}",
        transcript.session_id,
        session.id
    );
    ensure!(
        !session.harness.trim().is_empty(),
        "Observe session harness must not be empty"
    );

    let mut items = transcript.items.iter().collect::<Vec<_>>();
    items.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });

    let mut envelopes = Vec::new();
    let started_at = if session.started_at_ms > 0 {
        session.started_at_ms
    } else {
        items.first().map_or(0, |item| item.created_at_ms.max(0))
    };
    let mut start = session_start_event(
        &session.id,
        &session.harness,
        session.model.as_deref(),
        session.cwd.as_deref(),
    );
    if session.historical {
        start["source"] = "mjolnir-backfill".into();
    }
    envelopes.push(envelope(session, started_at, 0, "SessionStart", 0, start));

    let mut pending_agent: Option<(String, i64, u64)> = None;
    let mut last_activity = started_at;
    let mut turn_activity = started_at;
    for item in items {
        let timestamp = item.created_at_ms.max(0);
        last_activity = last_activity.max(timestamp.max(item.last_changed_at_ms.max(0)));
        match &item.body {
            TranscriptBody::User { .. } if auxiliary_user_item(&item.stable_id) => {}
            TranscriptBody::User { .. } => {
                emit_turn_end(&mut envelopes, session, &mut pending_agent, turn_activity);
                envelopes.push(envelope(
                    session,
                    timestamp,
                    item.position,
                    "UserPromptSubmit",
                    0,
                    json!({
                        "hook_event_name": "UserPromptSubmit",
                        "session_id": session.id,
                        "prompt": item.text,
                        "cwd": session.cwd,
                        "model": session.model,
                        "stable_id": item.stable_id,
                    }),
                ));
                turn_activity = timestamp;
            }
            TranscriptBody::Agent { .. } if !item.text.trim().is_empty() => {
                pending_agent = Some((
                    item.text.clone(),
                    item.last_changed_at_ms.max(item.created_at_ms).max(0),
                    item.position,
                ));
            }
            TranscriptBody::Tool {
                call,
                terminal_outputs,
                ..
            } => {
                emit_tool(
                    &mut envelopes,
                    session,
                    item,
                    call.value(),
                    terminal_outputs,
                )?;
                turn_activity = turn_activity.max(item.last_changed_at_ms.max(timestamp));
            }
            TranscriptBody::Message { .. }
            | TranscriptBody::Thought { .. }
            | TranscriptBody::Plan { .. }
            | TranscriptBody::PlanProposal { .. }
            | TranscriptBody::System { .. }
            | TranscriptBody::TerminalOutput { .. }
            | TranscriptBody::Agent { .. } => {}
        }
    }
    emit_turn_end(&mut envelopes, session, &mut pending_agent, turn_activity);

    if transcript.execution == MaterializedExecutionState::Closed {
        envelopes.push(envelope(
            session,
            last_activity,
            transcript.latest_seq,
            "SessionEnd",
            0,
            json!({
                "hook_event_name": "SessionEnd",
                "session_id": session.id,
                "reason": "mjolnir-stopped",
                "cwd": session.cwd,
            }),
        ));
    }

    for envelope in &envelopes {
        envelope.validate()?;
    }
    Ok(envelopes)
}

fn emit_turn_end(
    envelopes: &mut Vec<ObserveEnvelope>,
    session: &ObserveSession,
    pending_agent: &mut Option<(String, i64, u64)>,
    fallback_ts: i64,
) {
    let Some((message, timestamp, position)) = pending_agent.take() else {
        return;
    };
    envelopes.push(envelope(
        session,
        timestamp.max(fallback_ts),
        position,
        "Stop",
        0,
        json!({
            "hook_event_name": "Stop",
            "session_id": session.id,
            "last_assistant_message": message,
            "cwd": session.cwd,
            "model": session.model,
        }),
    ));
}

fn emit_tool(
    envelopes: &mut Vec<ObserveEnvelope>,
    session: &ObserveSession,
    item: &TranscriptItemView,
    call: Value,
    terminal_outputs: &[mj_core::state::TerminalOutputRecord],
) -> Result<()> {
    let call_id = string_field(&call, &["toolCallId", "tool_call_id"])
        .unwrap_or_else(|| item.stable_id.clone());
    let kind = string_field(&call, &["kind"]).unwrap_or_else(|| "other".into());
    let title = string_field(&call, &["title"]).unwrap_or_else(|| kind.clone());
    let tool_name = canonical_tool_name(&kind, &title);
    let input = call
        .get("rawInput")
        .or_else(|| call.get("raw_input"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut pre = Map::new();
    pre.insert("hook_event_name".into(), "PreToolUse".into());
    pre.insert("session_id".into(), session.id.clone().into());
    pre.insert("tool_name".into(), tool_name.clone().into());
    pre.insert("tool_use_id".into(), call_id.clone().into());
    pre.insert("tool_input".into(), input);
    pre.insert("cwd".into(), session.cwd.clone().into());
    pre.insert("mjolnir_tool_kind".into(), kind.into());
    pre.insert("mjolnir_tool_title".into(), title.into());
    pre.insert("mjolnir_tool_call".into(), call.clone());
    envelopes.push(envelope(
        session,
        item.created_at_ms.max(0),
        item.position,
        "PreToolUse",
        0,
        Value::Object(pre),
    ));

    let status = string_field(&call, &["status"]).unwrap_or_default();
    if !matches!(status.as_str(), "completed" | "failed") {
        return Ok(());
    }
    let failed = status == "failed";
    let mut response = Map::new();
    if let Some(output) = call.get("rawOutput").or_else(|| call.get("raw_output")) {
        response.insert("output".into(), output.clone());
    }
    if !terminal_outputs.is_empty() {
        response.insert(
            "terminal_outputs".into(),
            serde_json::to_value(terminal_outputs)?,
        );
    }
    if response.is_empty()
        && let Some(content) = call.get("content")
    {
        response.insert("content".into(), content.clone());
    }
    let duration_ms = (item.last_changed_at_ms > item.created_at_ms)
        .then_some(item.last_changed_at_ms - item.created_at_ms);
    envelopes.push(envelope(
        session,
        item.last_changed_at_ms.max(item.created_at_ms).max(0),
        item.position,
        if failed {
            "PostToolUseFailure"
        } else {
            "PostToolUse"
        },
        1,
        json!({
            "hook_event_name": if failed { "PostToolUseFailure" } else { "PostToolUse" },
            "session_id": session.id,
            "tool_name": tool_name,
            "tool_use_id": call_id,
            "tool_input": call.get("rawInput").or_else(|| call.get("raw_input")).cloned().unwrap_or(Value::Null),
            "tool_response": Value::Object(response),
            "cwd": session.cwd,
            "duration_ms": duration_ms,
            "status": status,
        }),
    ));
    Ok(())
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::to_owned)
}

fn auxiliary_user_item(stable_id: &str) -> bool {
    stable_id.starts_with(mj_core::transcript::ELICITATION_REPLY_ITEM_PREFIX)
        || stable_id.starts_with(mj_core::transcript::HARNESS_TURN_ITEM_PREFIX)
}

fn envelope(
    session: &ObserveSession,
    ts: i64,
    position: u64,
    kind: &str,
    ordinal: u8,
    event: Value,
) -> ObserveEnvelope {
    ObserveEnvelope {
        v: OBSERVE_ENVELOPE_VERSION,
        meta: ObserveMeta {
            event_id: event_id(&session.id, position, kind, ordinal),
            ts,
            event_position: position,
            event_ordinal: ordinal,
            agent: OBSERVE_COLLECTOR.into(),
            collector: OBSERVE_COLLECTOR.into(),
            historical: session.historical,
            hostname: session.hostname.clone(),
            os: session.os.clone(),
            orchestrator: Some(OBSERVE_COLLECTOR.into()),
            harness: Some(session.harness.clone()),
            native_session_id: session.native_session_id.clone(),
            worker_id: session.worker_id.clone(),
            parent_session_id: session.parent_id.clone(),
            workspace_id: session.workspace_id.clone(),
            profile_id: session.profile_id.clone(),
            target_id: session.target_id.clone(),
            runtime: session.runtime.clone(),
            git: session.git.clone(),
            commits: session.commits.clone(),
            collector_version: Some(env!("CARGO_PKG_VERSION").into()),
            client_version: Some(env!("CARGO_PKG_VERSION").into()),
            ..Default::default()
        },
        event,
    }
}

/// Rebuild the public transcript view from the daemon's canonical durable
/// projection. Automatic publication uses the same item shape as the CLI
/// export, so both paths derive identical Observe event identities.
pub fn transcript_response_from_materialized(
    session_id: &str,
    materialized: &MaterializedSession,
) -> Result<TranscriptResponse> {
    ensure!(
        materialized.session_id == session_id,
        "materialized session {} does not match requested session {session_id}",
        materialized.session_id
    );
    let mut items = materialized.transcript.to_vec();
    items.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });
    let latest_seq = items
        .iter()
        .map(|item| item.latest_content_event_ordinal.unwrap_or(item.position))
        .max()
        .unwrap_or(0);
    Ok(TranscriptResponse {
        next_after_seq: latest_seq,
        session_id: session_id.to_owned(),
        latest_seq,
        execution: materialized.execution,
        items: items
            .into_iter()
            .map(|item| TranscriptItemView {
                stable_id: item.stable_id.clone(),
                position: item.position,
                seq: item.seq(),
                role: mj_core::transcript::transcript_item_role(&item.body).to_owned(),
                text: mj_transcript::transcript::transcript_item_text(&item),
                created_at_ms: item.created_at_ms,
                last_changed_at_ms: item.last_changed_at_ms,
                body: item.body.clone(),
            })
            .collect(),
    })
}

/// Convert whole-turn usage into the generic request-usage event Observe
/// already understands. Partial and unspecified reports are deliberately
/// omitted rather than presented as complete turn totals.
pub fn usage_envelopes(
    session: &ObserveSession,
    usage: &UsagePage,
) -> Result<Vec<ObserveEnvelope>> {
    ensure!(
        usage.session_id == session.id,
        "usage session {} does not match Observe session {}",
        usage.session_id,
        session.id
    );
    let mut envelopes = Vec::new();
    for turn in &usage.turns {
        let Some(report) = turn
            .usage
            .as_ref()
            .filter(|report| report.scope == UsageScope::Turn)
        else {
            continue;
        };
        let selection = usage.turn_selections.get(&turn.command_id);
        let model = selection
            .and_then(|selection| selection.model.as_deref())
            .or(session.model.as_deref())
            .unwrap_or("unknown");
        let effort = selection.and_then(|selection| selection.effort.as_deref());
        let event = json!({
            "hook_event_name": "PostModelRequest",
            "session_id": session.id,
            "status": "completed",
            "timestamp": turn.completed_at_ms,
            "turn_id": turn.command_id,
            "turn_ordinal": turn.completed_ordinal,
            "model": model,
            "effort": effort,
            "input_tokens": report.input_tokens,
            "output_tokens": report.output_tokens,
            "cached_input_tokens": report.cached_read_tokens.unwrap_or(0),
            "cache_write_input_tokens": report.cached_write_tokens.unwrap_or(0),
            "thought_tokens": report.thought_tokens.unwrap_or(0),
            "total_tokens": report.total_tokens,
            "cwd": session.cwd,
        });
        let envelope = envelope(
            session,
            turn.completed_at_ms,
            turn.completed_ordinal,
            "PostModelRequest",
            0,
            event,
        );
        envelope.validate()?;
        envelopes.push(envelope);
    }
    Ok(envelopes)
}

/// A directory of newline-delimited Observe envelopes waiting for delivery.
///
/// Files are named by timestamp, transcript position, ordinal, and the digest
/// of the envelope's session and event ids. This preserves event order when a
/// harness emits several events at one timestamp and prevents a caller-supplied
/// id from becoming a path component.
#[derive(Debug, Clone)]
pub struct ObserveSpool {
    root: PathBuf,
}

impl ObserveSpool {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        fs::create_dir_all(&requested)
            .with_context(|| format!("create Observe spool {}", requested.display()))?;
        let root = fs::canonicalize(&requested)
            .with_context(|| format!("resolve Observe spool {}", requested.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn enqueue(&self, envelope: &ObserveEnvelope) -> Result<PathBuf> {
        let line = envelope.to_ndjson()?;
        let session_id = envelope.event["session_id"]
            .as_str()
            .context("Observe envelope lost its session id")?;
        let digest = Sha256::digest(format!("{session_id}\0{}", envelope.meta.event_id).as_bytes());
        let digest = mj_core::hex::lower_hex(digest);
        let name = format!(
            "{:020}-{:020}-{:03}-{digest}.json",
            envelope.meta.ts.max(0),
            envelope.meta.event_position,
            envelope.meta.event_ordinal
        );
        let path = self.root.join(name);
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).context("generate Observe spool temporary name")?;
        let temporary = self
            .root
            .join(format!(".{digest}.{}.tmp", mj_core::hex::lower_hex(nonce)));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create Observe spool entry {}", temporary.display()))?;
        if let Err(error) = file
            .write_all(line.as_bytes())
            .and_then(|()| file.sync_all())
        {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("write Observe spool entry");
        }
        drop(file);
        if let Err(error) = fs::hard_link(&temporary, &path) {
            let collision = error.kind() == std::io::ErrorKind::AlreadyExists;
            let matching =
                collision && fs::read(&path).is_ok_and(|existing| existing == line.as_bytes());
            let _ = fs::remove_file(&temporary);
            if !matching {
                return if collision {
                    bail!(
                        "Observe spool collision at {} for a different envelope",
                        path.display()
                    );
                } else {
                    Err(error).context("publish Observe spool entry")
                };
            }
            return Ok(path);
        }
        fs::remove_file(&temporary)
            .with_context(|| format!("remove Observe spool link {}", temporary.display()))?;
        Ok(path)
    }

    pub fn pending(&self) -> Result<Vec<PathBuf>> {
        let mut paths = fs::read_dir(&self.root)
            .with_context(|| format!("list Observe spool {}", self.root.display()))?
            .filter_map(|entry| match entry {
                Ok(entry)
                    if entry.path().extension().and_then(|value| value.to_str())
                        == Some("json") =>
                {
                    Some(Ok(entry.path()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<std::io::Result<Vec<_>>>()
            .context("read Observe spool entries")?;
        paths.sort();
        Ok(paths)
    }

    pub fn remove(&self, path: &Path) -> Result<()> {
        let path = fs::canonicalize(path)
            .with_context(|| format!("resolve Observe spool entry {}", path.display()))?;
        ensure!(
            path.parent() == Some(self.root.as_path()),
            "refusing to remove an Observe spool file outside {}",
            self.root.display()
        );
        fs::remove_file(&path)
            .with_context(|| format!("remove delivered Observe spool entry {}", path.display()))
    }
}

/// Filesystem locations owned by asynchronous Observe publication.
#[derive(Debug, Clone)]
pub struct ObservePaths {
    pub root: PathBuf,
    pub spool: PathBuf,
    pub state: PathBuf,
}

impl ObservePaths {
    pub fn current() -> Self {
        let root = mj_core::config::data_dir().join("observe");
        Self {
            spool: root.join("spool"),
            state: root.join("delivery.json"),
            root,
        }
    }
}

/// Delivery progress for one Mjolnir session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveSessionDelivery {
    /// Stable Observe event ids already committed to the local spool. Replaying
    /// the projection is harmless because entries are filtered through this
    /// set before the spool's own idempotent filename check.
    pub emitted_event_ids: BTreeSet<String>,
    /// The session record revision whose transcript was last examined.
    pub record_updated_at: String,
    /// Materialized event frontier whose transcript was last examined.
    #[serde(default)]
    pub materialized_event_ordinal: u64,
    /// Execution state at that frontier, needed because a close can change the
    /// published session boundary without adding transcript content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<MaterializedExecutionState>,
    /// A completed historical import records when it was published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_imported_at_ms: Option<i64>,
    /// SessionWiki can outlive Mjolnir's full checkpoint. Remember that there
    /// is nothing richer left to publish instead of rescanning on every event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_source_unavailable_at_ms: Option<i64>,
}

/// Durable automatic-delivery state. The daemon is the only writer; the CLI
/// reads the same atomically replaced file for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveDeliveryState {
    pub version: u32,
    pub initialized_at_ms: i64,
    #[serde(default)]
    pub sessions: BTreeMap<String, ObserveSessionDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

impl ObserveDeliveryState {
    pub const VERSION: u32 = 1;

    pub fn load(path: &Path, now_ms: i64) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: Self::VERSION,
                initialized_at_ms: now_ms,
                sessions: BTreeMap::new(),
                last_success_ms: None,
                last_failure_ms: None,
                last_failure: None,
            });
        }
        let body = fs::read(path)
            .with_context(|| format!("read Observe delivery state {}", path.display()))?;
        let state: Self = serde_json::from_slice(&body)
            .with_context(|| format!("parse Observe delivery state {}", path.display()))?;
        ensure!(
            state.version == Self::VERSION,
            "unsupported Observe delivery state version {}",
            state.version
        );
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        mj_core::config::atomic_write(path, &body)
            .with_context(|| format!("write Observe delivery state {}", path.display()))
    }

    pub fn mark_success(&mut self, at_ms: i64) {
        self.last_success_ms = Some(at_ms);
        self.last_failure = None;
    }

    pub fn mark_failure(&mut self, at_ms: i64, error: impl Into<String>) {
        self.last_failure_ms = Some(at_ms);
        self.last_failure = Some(error.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use agent_client_protocol::v1::schema::{
        ContentBlock, ContentChunk, TextContent, ToolCall, ToolCallStatus, ToolKind,
    };
    use mj_core::state::TranscriptItem;

    fn session() -> ObserveSession {
        ObserveSession {
            id: "session-1".into(),
            parent_id: Some("parent-1".into()),
            native_session_id: Some("native-1".into()),
            worker_id: Some("worker-1".into()),
            harness: "muse".into(),
            model: Some("muse-wonder".into()),
            cwd: Some("/work/app".into()),
            hostname: Some("build-host".into()),
            os: Some("linux".into()),
            started_at_ms: 1_000,
            workspace_id: Some("workspace".into()),
            profile_id: Some("muse".into()),
            target_id: Some("localhost".into()),
            runtime: Some("local".into()),
            git: Some(ObserveGitMeta {
                remote: Some("https://github.com/acme/app".into()),
                branch: Some("main".into()),
                head: Some("0123456789abcdef".into()),
            }),
            commits: Vec::new(),
            historical: false,
        }
    }

    fn item(position: u64, body: TranscriptBody) -> TranscriptItemView {
        let item = TranscriptItem {
            stable_id: format!("item-{position}"),
            position,
            latest_content_event_ordinal: matches!(body, TranscriptBody::Agent { .. })
                .then_some(position),
            created_at_ms: 1_000 + position as i64,
            last_changed_at_ms: 1_100 + position as i64,
            body,
        };
        let seq = item.seq();
        TranscriptItemView {
            stable_id: item.stable_id,
            position: item.position,
            seq,
            role: "test".into(),
            text: "hello".into(),
            created_at_ms: item.created_at_ms,
            last_changed_at_ms: item.last_changed_at_ms,
            body: item.body,
        }
    }

    fn agent(position: u64, text: &str) -> TranscriptItemView {
        let mut item = item(
            position,
            TranscriptBody::Agent {
                chunks: vec![
                    serde_json::to_value(ContentChunk::new(ContentBlock::Text(TextContent::new(
                        text,
                    ))))
                    .unwrap(),
                ],
                streaming: false,
            },
        );
        item.text = text.into();
        item
    }

    #[test]
    fn transcript_becomes_deterministic_turn_and_tool_events() {
        let tool = ToolCall::new("call-1", "cargo test")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Completed)
            .raw_input(json!({"command": "cargo test"}))
            .raw_output(json!({"output": "all tests passed", "exit_code": 0}));
        let tool = serde_json::to_value(tool).unwrap();
        let transcript = TranscriptResponse {
            next_after_seq: 4,
            session_id: "session-1".into(),
            latest_seq: 4,
            execution: MaterializedExecutionState::Closed,
            items: vec![
                item(
                    1,
                    TranscriptBody::User {
                        content: vec![json!({"type": "text", "text": "run tests"})],
                    },
                ),
                item(
                    2,
                    TranscriptBody::Tool {
                        call: tool.into(),
                        terminal_outputs: Vec::new(),
                        terminal_refs: Vec::new(),
                        presentation: None,
                    },
                ),
                agent(3, "Tests pass."),
            ],
        };
        let envelopes = envelopes_for_transcript(&session(), &transcript).unwrap();
        let kinds = envelopes
            .iter()
            .map(|event| event.event["hook_event_name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
                "SessionEnd"
            ]
        );
        assert_eq!(envelopes[2].event["tool_name"], "Bash");
        assert_eq!(envelopes[2].event["tool_input"]["command"], "cargo test");
        assert_eq!(
            envelopes[3].event["tool_response"]["output"]["output"],
            "all tests passed"
        );
        assert_eq!(envelopes[0].meta.harness.as_deref(), Some("muse"));
        assert_eq!(
            envelopes,
            envelopes_for_transcript(&session(), &transcript).unwrap()
        );
    }

    #[test]
    fn failed_tool_emits_a_failure_result() {
        let tool = ToolCall::new("call-1", "bad command")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Failed)
            .raw_input(json!({"command": "false"}))
            .raw_output(json!({"output": "failed", "exit_code": 1}));
        let transcript = TranscriptResponse {
            next_after_seq: 1,
            session_id: "session-1".into(),
            latest_seq: 1,
            execution: MaterializedExecutionState::Closed,
            items: vec![item(
                1,
                TranscriptBody::Tool {
                    call: serde_json::to_value(tool).unwrap().into(),
                    terminal_outputs: Vec::new(),
                    terminal_refs: Vec::new(),
                    presentation: None,
                },
            )],
        };
        let envelopes = envelopes_for_transcript(&session(), &transcript).unwrap();
        assert_eq!(envelopes[2].event["hook_event_name"], "PostToolUseFailure");
    }

    #[test]
    fn form_replies_and_harness_turns_are_not_counted_as_user_prompts() {
        let mut reply = item(
            1,
            TranscriptBody::User {
                content: vec![json!({"type": "text", "text": "yes"})],
            },
        );
        reply.stable_id = format!(
            "{}reply",
            mj_core::transcript::ELICITATION_REPLY_ITEM_PREFIX
        );
        let mut continued = item(
            2,
            TranscriptBody::User {
                content: vec![json!({"type": "text", "text": "continue"})],
            },
        );
        continued.stable_id = format!("{}1", mj_core::transcript::HARNESS_TURN_ITEM_PREFIX);
        let transcript = TranscriptResponse {
            next_after_seq: 2,
            session_id: "session-1".into(),
            latest_seq: 2,
            execution: MaterializedExecutionState::Idle,
            items: vec![reply, continued],
        };
        let envelopes = envelopes_for_transcript(&session(), &transcript).unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].event["hook_event_name"], "SessionStart");
    }

    #[test]
    fn closing_is_not_published_as_a_finished_session() {
        let transcript = TranscriptResponse {
            next_after_seq: 1,
            session_id: "session-1".into(),
            latest_seq: 1,
            execution: MaterializedExecutionState::Closing,
            items: vec![item(
                1,
                TranscriptBody::User {
                    content: vec![json!({"type": "text", "text": "hello"})],
                },
            )],
        };
        let envelopes = envelopes_for_transcript(&session(), &transcript).unwrap();
        assert!(
            envelopes
                .iter()
                .all(|envelope| envelope.event["hook_event_name"] != "SessionEnd")
        );
    }

    #[test]
    fn each_turn_gets_a_distinct_stop_event_id() {
        let transcript = TranscriptResponse {
            next_after_seq: 5,
            session_id: "session-1".into(),
            latest_seq: 5,
            execution: MaterializedExecutionState::Closed,
            items: vec![
                item(
                    1,
                    TranscriptBody::User {
                        content: vec![json!({"type": "text", "text": "first"})],
                    },
                ),
                agent(2, "first answer"),
                item(
                    3,
                    TranscriptBody::User {
                        content: vec![json!({"type": "text", "text": "second"})],
                    },
                ),
                agent(4, "second answer"),
            ],
        };
        let envelopes = envelopes_for_transcript(&session(), &transcript).unwrap();
        let ids = envelopes
            .iter()
            .map(|envelope| envelope.meta.event_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), envelopes.len());
        assert_eq!(
            envelopes
                .iter()
                .filter(|envelope| envelope.event["hook_event_name"] == "Stop")
                .count(),
            2
        );
        let stops = envelopes
            .iter()
            .filter(|envelope| envelope.event["hook_event_name"] == "Stop")
            .map(|envelope| envelope.meta.ts)
            .collect::<Vec<_>>();
        assert_eq!(stops, [1_102, 1_104]);
    }

    #[test]
    fn whole_turn_usage_becomes_a_stable_request_event() {
        let usage: UsagePage = serde_json::from_value(json!({
            "session_id": "session-1",
            "turns": [{
                "command_id": "turn-1",
                "completed_ordinal": 7,
                "completed_at_ms": 1_700,
                "outcome": {"kind": "completed", "stop_reason": "end_turn"},
                "usage": {
                    "scope": "turn",
                    "total_tokens": 270,
                    "input_tokens": 150,
                    "output_tokens": 70,
                    "thought_tokens": 50,
                    "cached_read_tokens": 1000,
                    "cached_write_tokens": 50
                }
            }],
            "next_after_seq": 7,
            "latest_seq": 7,
            "totals": {},
            "coverage": {
                "recorded_turns": 1,
                "full_turn_reports": 1,
                "last_request_reports": 0,
                "unspecified_reports": 0,
                "missing_reports": 0
            },
            "turn_selections": {
                "turn-1": {"model": "muse-wonder", "effort": "high"}
            }
        }))
        .unwrap();
        let envelopes = usage_envelopes(&session(), &usage).unwrap();
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].event["hook_event_name"], "PostModelRequest");
        assert_eq!(envelopes[0].event["input_tokens"], 150);
        assert_eq!(envelopes[0].event["cached_input_tokens"], 1000);
        assert_eq!(envelopes[0].event["model"], "muse-wonder");
        assert_eq!(envelopes[0].event["turn_ordinal"], 7);
    }

    #[test]
    fn delivery_state_is_durable_and_preserves_the_latest_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("delivery.json");
        let mut state = ObserveDeliveryState::load(&path, 10).unwrap();
        state.mark_failure(20, "server unavailable");
        state.save(&path).unwrap();
        let mut state = ObserveDeliveryState::load(&path, 30).unwrap();
        assert_eq!(
            (state.initialized_at_ms, state.last_failure.as_deref()),
            (10, Some("server unavailable"))
        );
        state.mark_success(40);
        assert_eq!(state.last_failure, None);
    }

    #[test]
    fn spool_is_atomic_ordered_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let spool = ObserveSpool::open(directory.path()).unwrap();
        let transcript = TranscriptResponse {
            next_after_seq: 1,
            session_id: "session-1".into(),
            latest_seq: 1,
            execution: MaterializedExecutionState::Idle,
            items: vec![item(
                1,
                TranscriptBody::User {
                    content: vec![json!({"type": "text", "text": "hello"})],
                },
            )],
        };
        let mut envelopes = envelopes_for_transcript(&session(), &transcript).unwrap();
        envelopes[1].meta.ts = envelopes[0].meta.ts;
        let first = spool.enqueue(&envelopes[0]).unwrap();
        assert_eq!(spool.enqueue(&envelopes[0]).unwrap(), first);
        let second = spool.enqueue(&envelopes[1]).unwrap();
        assert_eq!(
            spool.pending().unwrap(),
            vec![first.clone(), second.clone()]
        );
        spool.remove(&first).unwrap();
        assert_eq!(spool.pending().unwrap(), vec![second]);
    }

    #[test]
    fn spool_refuses_to_remove_outside_its_root() {
        let spool = ObserveSpool::open(tempfile::tempdir().unwrap().path()).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(spool.remove(outside.path()).is_err());
    }

    #[test]
    fn concurrent_enqueue_of_one_event_is_one_spool_file() {
        let directory = tempfile::tempdir().unwrap();
        let spool = ObserveSpool::open(directory.path()).unwrap();
        let envelope = ObserveEnvelope {
            v: mj_core::observe::OBSERVE_ENVELOPE_VERSION,
            meta: ObserveMeta {
                event_id: event_id("session-1", 1, "SessionStart", 0),
                ts: 1,
                agent: OBSERVE_COLLECTOR.into(),
                collector: OBSERVE_COLLECTOR.into(),
                ..Default::default()
            },
            event: session_start_event("session-1", "muse", None, None),
        };
        let threads = (0..8)
            .map(|_| {
                let spool = spool.clone();
                let envelope = envelope.clone();
                std::thread::spawn(move || spool.enqueue(&envelope))
            })
            .collect::<Vec<_>>();
        let paths = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(paths.len(), 1);
        assert_eq!(
            spool.pending().unwrap(),
            paths.into_iter().collect::<Vec<_>>()
        );
    }
}
