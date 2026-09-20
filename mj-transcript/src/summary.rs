//! A deterministic model-facing view of history. Source transcripts stay lossless.
use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, ToolCall};
use mj_core::archive::{CanonicalSessionSnapshot, CanonicalTranscriptBody};
use mj_core::state::{MaterializedSession, TranscriptBody};
use mj_core::transcript::{
    ToolCallPresentation, materialized_chunks_text, materialized_content_text,
};
use serde_json::{Value, json};

use crate::transcript::materialized_tool_call_presentation;

/// Opening sentence of every handoff this module writes. Generation and
/// detection share it so a later resume can always recognize its own prior
/// handoff turns.
pub const HANDOFF_PREAMBLE: &str =
    "You are continuing a coding session previously run by another ACP harness.";
/// Opening sentence of a hand-off written when an archived session is restored
/// from the SessionWiki index. The restored session has no workspace from the
/// old one, so it is marked apart from a cross-harness resume.
pub const ARCHIVE_HANDOFF_PREAMBLE: &str = "Archived session restored from SessionWiki.";
/// Opening sentence of the byte-truncating handoff this pipeline replaced.
/// Sessions resumed by that build still carry it in their transcripts.
pub const LEGACY_HANDOFF_PREAMBLE: &str =
    "Continue this coding session from the portable transcript below.";
/// What a prior handoff turn contributes to a new compaction. The transcript
/// already carries the pre-resume lineage as ordinary turns, so repeating the
/// handoff body would only spend budget on a summary of a summary.
pub const HANDOFF_PLACEHOLDER: &str =
    "[cross-harness resume handoff: continuing work from a prior harness]";
/// Prior handoffs repeat history already represented in the lineage.
fn user_text(text: String) -> String {
    let trimmed = text.trim_start();
    if [
        HANDOFF_PREAMBLE,
        ARCHIVE_HANDOFF_PREAMBLE,
        LEGACY_HANDOFF_PREAMBLE,
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
    {
        HANDOFF_PLACEHOLDER.into()
    } else {
        text
    }
}

pub const FULL_TOOL_CALLS: usize = 8;
/// Change when derived indexing content changes, even without a source update.
pub const SUMMARY_VERSION: u32 = 1;
pub const DEFAULT_SUMMARY_BYTES: usize = 256 * 1024;
const LIVE_ITEMS: usize = 512;
const LIVE_ITEM_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryRole {
    User,
    Assistant,
    Tool,
    Plan,
}

#[derive(Debug, Clone)]
pub struct SummaryEntry {
    pub id: String,
    pub position: u64,
    pub created_at_ms: i64,
    pub role: SummaryRole,
    pub text: String,
    /// Full retained calls or a compact, outcome-only object. Never a presentation sidecar.
    pub tool: Option<Value>,
    terminal_refs: Vec<String>,
}

impl SummaryEntry {
    pub fn body(&self) -> String {
        self.tool
            .as_ref()
            .map_or_else(|| self.text.clone(), Value::to_string)
    }

    fn render(&self, body_limit: usize) -> String {
        let role = match self.role {
            SummaryRole::User => "user",
            SummaryRole::Assistant => "assistant",
            SummaryRole::Tool => "tool",
            SummaryRole::Plan => "plan",
        };
        let body = self.body();
        // Names and outcomes are outside the potentially large raw-body excerpt.
        let label = if self.role == SummaryRole::Tool {
            format!(" {}", self.text)
        } else {
            String::new()
        };
        format!(
            "<{role}{label}>\n{}\n</{role}>\n",
            excerpt(&body, body_limit)
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct TranscriptSummary {
    pub entries: Vec<SummaryEntry>,
    next_position: u64,
    open_message: Option<String>,
    leading_omitted: bool,
}

/// A head/tail excerpt with an explicit byte omission count, never broken UTF-8.
pub fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let marker_budget = format!("\n[{} bytes omitted]\n", text.len()).len();
    if limit < marker_budget {
        return "[omitted]".chars().take(limit).collect();
    }
    let available = limit - marker_budget;
    let mut head = available / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (available - available / 2);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[{} bytes omitted]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn tool_value(call: &Value, presentation: Option<&ToolCallPresentation>, full: bool) -> Value {
    let name = match serde_json::from_value::<ToolCall>(call.clone()) {
        Ok(call) => materialized_tool_call_presentation(presentation, &call).summary,
        Err(error) => {
            tracing::warn!(%error, "could not decode tool for transcript summary");
            "[invalid tool call]".into()
        }
    };
    let mut compact = json!({
        "toolCallId": call.get("toolCallId").and_then(Value::as_str).unwrap_or("unknown"),
        "name": excerpt(&name, 256),
        "kind": call.get("kind").and_then(Value::as_str).unwrap_or("other"),
        "status": call.get("status").and_then(Value::as_str).unwrap_or("pending"),
    });
    if let Some(code) = call.pointer("/rawOutput/exit_code").and_then(Value::as_i64) {
        compact["exit_code"] = json!(code);
    }
    if let Some(signal) = call.pointer("/rawOutput/signal").and_then(Value::as_str) {
        compact["signal"] = json!(signal);
    }
    if full {
        compact["call"] = call.clone();
    }
    compact
}

/// Reconstruct indexed summary content without treating serialized detail as a tool name.
/// Legacy indexes contain only titles and have no recoverable tool payload.
pub fn indexed_tool_call(
    text: &str,
    fallback_id: &str,
) -> (Value, Vec<mj_core::archive::CanonicalTerminalOutput>) {
    if let Ok(value) = serde_json::from_str::<Value>(text)
        && value.get("name").is_some_and(Value::is_string)
        && value.get("status").is_some_and(Value::is_string)
        && value.get("toolCallId").is_some_and(Value::is_string)
    {
        if let Some(call) = value.get("call")
            && serde_json::from_value::<ToolCall>(call.clone()).is_ok()
        {
            let mut terminals = Vec::new();
            if let Some(records) = value.get("terminals").and_then(Value::as_object) {
                for (id, record) in records {
                    let mut record = record.clone();
                    if !record.is_object() {
                        tracing::warn!("invalid indexed terminal summary");
                        continue;
                    }
                    record["terminal_id"] = json!(id);
                    match serde_json::from_value(record) {
                        Ok(record) => terminals.push(record),
                        Err(error) => tracing::warn!(%error, "invalid indexed terminal summary"),
                    }
                }
            }
            return (call.clone(), terminals);
        }
        let mut call = json!({"toolCallId":value["toolCallId"],"title":value["name"],"status":value["status"],"kind":value.get("kind").cloned().unwrap_or(json!("other"))});
        for key in ["exit_code", "signal"] {
            if let Some(outcome) = value.get(key) {
                if !call["rawOutput"].is_object() {
                    call["rawOutput"] = json!({});
                }
                call["rawOutput"][key] = outcome.clone();
            }
        }
        return (call, Vec::new());
    }
    (
        json!({"toolCallId":fallback_id,"title":text,"status":"completed"}),
        Vec::new(),
    )
}

fn tool_label(value: &Value) -> String {
    let mut label = format!(
        "{} [{}]",
        value["name"].as_str().unwrap_or("tool"),
        value["status"].as_str().unwrap_or("unknown")
    );
    if let Some(code) = value.get("exit_code") {
        label.push_str(&format!(" exit={code}"));
    }
    if let Some(signal) = value.get("signal").and_then(Value::as_str) {
        label.push_str(&format!(" signal={signal}"));
    }
    label
}

impl TranscriptSummary {
    pub fn from_snapshot(snapshot: &CanonicalSessionSnapshot) -> Self {
        let mut summary = Self::default();
        let start = snapshot.current_context_start();
        let cutoff = snapshot
            .transcript
            .iter()
            .filter(|i| start == 0 || i.position > start)
            .filter(|i| matches!(i.body, CanonicalTranscriptBody::Tool { .. }))
            .rev()
            .take(FULL_TOOL_CALLS)
            .map(|i| i.position)
            .min()
            .unwrap_or(u64::MAX);
        for item in snapshot
            .transcript
            .iter()
            .filter(|i| start == 0 || i.position > start)
        {
            let (role, text, tool) = match &item.body {
                CanonicalTranscriptBody::User { content } => (
                    SummaryRole::User,
                    user_text(materialized_content_text(content)),
                    None,
                ),
                CanonicalTranscriptBody::Agent { chunks, .. } => (
                    SummaryRole::Assistant,
                    materialized_chunks_text(chunks),
                    None,
                ),
                CanonicalTranscriptBody::Tool {
                    call,
                    terminal_outputs,
                    presentation,
                    ..
                } => {
                    let mut value =
                        tool_value(call, presentation.as_ref(), item.position >= cutoff);
                    attach_terminals(
                        &mut value,
                        terminal_outputs.iter().map(|t| {
                            (
                                &t.terminal_id,
                                &t.output,
                                t.exit_code,
                                t.signal.as_deref(),
                                t.truncated,
                            )
                        }),
                    );
                    (SummaryRole::Tool, tool_label(&value), Some(value))
                }
                CanonicalTranscriptBody::Plan { plan } => {
                    (SummaryRole::Plan, plan.to_string(), None)
                }
                _ => continue,
            };
            summary.entries.push(SummaryEntry {
                id: item.stable_id.clone(),
                position: item.position,
                created_at_ms: item.created_at_ms,
                role,
                text,
                terminal_refs: tool
                    .as_ref()
                    .and_then(|v| v.get("call"))
                    .map(crate::projection::tool_call_terminal_ids)
                    .unwrap_or_default(),
                tool,
            });
        }
        summary.next_position = snapshot.event_frontier.saturating_add(1);
        summary
    }

    pub fn from_materialized(session: &MaterializedSession) -> Self {
        let start = session
            .transcript
            .iter()
            .filter(|i| mj_core::archive::is_context_boundary(&i.stable_id))
            .map(|i| i.position)
            .max()
            .unwrap_or(0);
        let cutoff = session
            .transcript
            .iter()
            .filter(|i| start == 0 || i.position > start)
            .filter(|i| matches!(i.body, TranscriptBody::Tool { .. }))
            .rev()
            .take(FULL_TOOL_CALLS)
            .map(|i| i.position)
            .min()
            .unwrap_or(u64::MAX);
        let mut summary = Self::default();
        for item in session
            .transcript
            .iter()
            .filter(|i| start == 0 || i.position > start)
        {
            let (role, text, tool) = match &item.body {
                TranscriptBody::User { content } => (
                    SummaryRole::User,
                    user_text(materialized_content_text(content)),
                    None,
                ),
                TranscriptBody::Agent { chunks, .. } => (
                    SummaryRole::Assistant,
                    materialized_chunks_text(chunks),
                    None,
                ),
                TranscriptBody::Tool {
                    call,
                    terminal_outputs,
                    presentation,
                    ..
                } => {
                    let mut value =
                        tool_value(call, presentation.as_deref(), item.position >= cutoff);
                    attach_terminals(
                        &mut value,
                        terminal_outputs.iter().map(|t| {
                            (
                                &t.terminal_id,
                                &t.output,
                                t.exit_code,
                                t.signal.as_deref(),
                                t.truncated,
                            )
                        }),
                    );
                    (SummaryRole::Tool, tool_label(&value), Some(value))
                }
                TranscriptBody::Plan { plan } => (SummaryRole::Plan, plan.to_string(), None),
                _ => continue,
            };
            summary.entries.push(SummaryEntry {
                id: item.stable_id.clone(),
                position: item.position,
                created_at_ms: item.created_at_ms,
                role,
                text,
                terminal_refs: tool
                    .as_ref()
                    .and_then(|v| v.get("call"))
                    .map(crate::projection::tool_call_terminal_ids)
                    .unwrap_or_default(),
                tool,
            });
        }
        summary.next_position = session.applied_event_ordinal.saturating_add(1);
        summary
    }

    pub fn mark_earlier_history_omitted(&mut self) {
        self.leading_omitted = true;
    }

    pub fn push_user(&mut self, text: &str) {
        self.open_message = None;
        self.push(
            SummaryRole::User,
            String::new(),
            user_text(text.to_owned()),
            None,
        );
    }

    fn push(&mut self, role: SummaryRole, id: String, text: String, tool: Option<Value>) {
        self.next_position = self.next_position.saturating_add(1);
        self.entries.push(SummaryEntry {
            id,
            position: self.next_position,
            created_at_ms: 0,
            role,
            text: excerpt(&text, LIVE_ITEM_BYTES),
            terminal_refs: tool
                .as_ref()
                .and_then(|v| v.get("call"))
                .map(crate::projection::tool_call_terminal_ids)
                .unwrap_or_default(),
            tool,
        });
        if self.entries.len() > LIVE_ITEMS {
            let full_tools = self
                .entries
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, e)| e.role == SummaryRole::Tool)
                .take(FULL_TOOL_CALLS)
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
            let latest_user = self
                .entries
                .iter()
                .rposition(|e| e.role == SummaryRole::User);
            let remove = (0..self.entries.len())
                .find(|i| !full_tools.contains(i) && Some(*i) != latest_user)
                .unwrap_or(0);
            self.entries.remove(remove);
            self.leading_omitted = true;
        }
    }

    pub fn observe(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let id = chunk
                    .message_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                if let ContentBlock::Text(text) = &chunk.content {
                    if self.open_message.as_ref() == Some(&id) {
                        if let Some(entry) = self.entries.last_mut() {
                            entry.text.push_str(&excerpt(&text.text, LIVE_ITEM_BYTES));
                            if entry.text.len() > LIVE_ITEM_BYTES {
                                entry.text = excerpt(&entry.text, LIVE_ITEM_BYTES);
                            }
                        }
                    } else {
                        self.push(
                            SummaryRole::Assistant,
                            id.clone(),
                            excerpt(&text.text, LIVE_ITEM_BYTES),
                            None,
                        );
                    }
                    self.open_message = Some(id);
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.open_message = None;
                let id = call.tool_call_id.to_string();
                let raw = serde_json::to_value(call).expect("serialize ACP call");
                let full = self
                    .entries
                    .iter()
                    .find(|e| e.id == id)
                    .is_none_or(|e| e.tool.as_ref().is_some_and(|v| v.get("call").is_some()));
                let value = tool_value(&raw, None, full);
                if let Some(entry) = self
                    .entries
                    .iter_mut()
                    .find(|e| e.role == SummaryRole::Tool && e.id == id)
                {
                    for id in value
                        .get("call")
                        .map(crate::projection::tool_call_terminal_ids)
                        .unwrap_or_default()
                    {
                        if !entry.terminal_refs.contains(&id) {
                            entry.terminal_refs.push(id);
                        }
                    }
                    entry.text = tool_label(&value);
                    entry.tool = Some(value);
                } else {
                    self.push(SummaryRole::Tool, id, tool_label(&value), Some(value));
                }
                self.demote_tools();
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.open_message = None;
                if let Some(entry) = self.entries.iter_mut().find(|e| {
                    e.role == SummaryRole::Tool && e.id == update.tool_call_id.to_string()
                }) {
                    let old = entry.tool.as_ref().expect("tool entry");
                    let full = old.get("call").is_some();
                    let mut call: ToolCall = old
                        .get("call")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_else(|| {
                            ToolCall::new(
                                update.tool_call_id.clone(),
                                old["name"].as_str().unwrap_or("tool"),
                            )
                        });
                    let previous_name = old["name"].clone();
                    let previous_status = old["status"].clone();
                    call.update(update.fields.clone());
                    let raw = serde_json::to_value(call).expect("serialize ACP call");
                    let mut value = tool_value(&raw, None, full);
                    if !full && update.fields.raw_input.is_none() {
                        value["name"] = previous_name;
                    }
                    if update.fields.kind.is_none() {
                        value["kind"] = old["kind"].clone();
                    }
                    if update.fields.status.is_none() {
                        value["status"] = previous_status;
                    }
                    for key in ["exit_code", "signal", "terminals"] {
                        if value.get(key).is_none()
                            && let Some(old_value) = old.get(key)
                        {
                            value[key] = old_value.clone();
                        }
                    }
                    for id in value
                        .get("call")
                        .map(crate::projection::tool_call_terminal_ids)
                        .unwrap_or_default()
                    {
                        if !entry.terminal_refs.contains(&id) {
                            entry.terminal_refs.push(id);
                        }
                    }
                    entry.text = tool_label(&value);
                    entry.tool = Some(value);
                }
            }
            SessionUpdate::AgentThoughtChunk(_) => self.open_message = None,
            SessionUpdate::Plan(plan) => {
                self.open_message = None;
                self.push(
                    SummaryRole::Plan,
                    String::new(),
                    serde_json::to_string(plan).expect("serialize plan"),
                    None,
                );
            }
            _ => {}
        }
        // Bound retained live payloads, including strings nested in raw ACP records.
        if matches!(
            update,
            SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)
        ) {
            for entry in &mut self.entries {
                if let Some(value) = &mut entry.tool {
                    bound_value(value, LIVE_ITEM_BYTES);
                }
            }
        }
    }

    pub fn observe_terminal(&mut self, terminal: &mj_core::transcript::TerminalOutputRecord) {
        for entry in &mut self.entries {
            let Some(value) = &mut entry.tool else {
                continue;
            };
            let refers = entry.terminal_refs.contains(&terminal.terminal_id);
            if refers {
                attach_terminals(
                    value,
                    std::iter::once((
                        &terminal.terminal_id,
                        &terminal.output,
                        terminal.exit_code,
                        terminal.signal.as_deref(),
                        terminal.truncated,
                    )),
                );
                bound_value(value, LIVE_ITEM_BYTES);
                entry.text = tool_label(value);
            }
        }
    }

    fn demote_tools(&mut self) {
        for entry in self
            .entries
            .iter_mut()
            .rev()
            .filter(|e| e.role == SummaryRole::Tool)
            .skip(FULL_TOOL_CALLS)
        {
            if let Some(value) = entry.tool.as_mut().and_then(Value::as_object_mut) {
                value.remove("call");
                value.remove("terminals");
            }
        }
    }

    /// Exact retained context alongside a model-produced summary, newest-first selection.
    pub fn retained(&self) -> Self {
        let last_assistant = self
            .entries
            .iter()
            .rposition(|e| e.role == SummaryRole::Assistant);
        Self {
            entries: self
                .entries
                .iter()
                .enumerate()
                .filter(|(i, e)| {
                    e.role == SummaryRole::User
                        || Some(*i) == last_assistant
                        || e.tool.as_ref().is_some_and(|v| v.get("call").is_some())
                })
                .map(|(_, e)| e.clone())
                .collect(),
            ..Default::default()
        }
    }

    /// Preserve recent context first, then spend spare space on newest history.
    /// Only after optional history is removed do oversized retained bodies shrink.
    pub fn render(&self, limit: usize) -> String {
        let history_marker = if self.leading_omitted {
            "[Earlier transcript entries omitted]\n"
        } else {
            ""
        };
        if limit < history_marker.len() {
            return excerpt(history_marker, limit);
        }
        let all = history_marker.to_owned()
            + &self
                .entries
                .iter()
                .map(|e| e.render(usize::MAX))
                .collect::<String>();
        if all.len() <= limit {
            return all;
        }

        let mut selected = std::collections::BTreeSet::new();
        for role in [SummaryRole::User, SummaryRole::Assistant] {
            if let Some(index) = self.entries.iter().rposition(|e| e.role == role) {
                selected.insert(index);
            }
        }
        selected.extend(
            self.entries
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, e)| e.role == SummaryRole::Tool)
                .take(FULL_TOOL_CALLS)
                .map(|(i, _)| i),
        );
        let mut used = selected
            .iter()
            .map(|i| self.entries[*i].render(usize::MAX).len())
            .sum::<usize>();
        // Reserve enough room for both coverage markers before adding optional items.
        let available = limit.saturating_sub(history_marker.len() + 64);
        if used < available {
            // Codex-style retention: recent user messages take precedence over old activity.
            for users in [true, false] {
                for (index, entry) in self.entries.iter().enumerate().rev() {
                    if selected.contains(&index) || (entry.role == SummaryRole::User) != users {
                        continue;
                    }
                    let size = entry.render(usize::MAX).len();
                    if used.saturating_add(size) <= available {
                        selected.insert(index);
                        used += size;
                    }
                }
            }
        }
        let omitted = self.entries.len() - selected.len();
        let prefix = history_marker.to_owned()
            + &if omitted > 0 {
                format!("[{omitted} transcript entries omitted]\n")
            } else {
                String::new()
            };
        let render = |cap| {
            prefix.clone()
                + &selected
                    .iter()
                    .map(|i| self.entries[*i].render(cap))
                    .collect::<String>()
        };
        let full = render(usize::MAX);
        if full.len() <= limit {
            return full;
        }
        let minimum = render(0);
        if minimum.len() > limit {
            return excerpt(&minimum, limit);
        }
        // A common cap leaves short messages intact and shares remaining space among long bodies.
        let (mut low, mut high) = (0, limit);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if render(middle).len() <= limit {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        render(low)
    }
}

fn attach_terminals<'a>(
    value: &mut Value,
    terminals: impl Iterator<Item = (&'a String, &'a String, Option<u32>, Option<&'a str>, bool)>,
) {
    for (id, output, code, signal, truncated) in terminals {
        if let Some(code) = code {
            value["exit_code"] = json!(code);
        }
        if let Some(signal) = signal {
            value["signal"] = json!(signal);
        }
        if value.get("call").is_some() {
            if !value["terminals"].is_object() {
                value["terminals"] = json!({});
            }
            value["terminals"][id] =
                json!({"output":output,"exit_code":code,"signal":signal,"truncated":truncated});
        }
    }
}

fn bound_value(value: &mut Value, limit: usize) {
    match value {
        Value::String(text) if text.len() > limit => *text = excerpt(text, limit),
        Value::Array(values) => {
            for value in values {
                bound_value(value, limit);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                bound_value(value, limit);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    };
    use mj_core::state::TranscriptItem;
    use std::sync::Arc;

    fn call(n: usize) -> ToolCall {
        ToolCall::new(format!("call-{n}"), "Run command")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Completed)
            .raw_input(json!({"command": format!("cargo test --marker=ARGUMENT_{n}")}))
            .raw_output(json!({"exit_code": n, "formatted_output": format!("OUTPUT_{n}")}))
    }

    #[test]
    fn ninth_call_demotes_first_and_late_updates_do_not_promote_it() {
        let mut summary = TranscriptSummary::default();
        summary.push_user("test the change");
        for n in 0..9 {
            summary.observe(&SessionUpdate::ToolCall(call(n)));
        }
        summary.observe(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "call-0",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .raw_output(json!({"exit_code": 42,"output":"LATE_PAYLOAD"})),
        )));
        let output = summary.render(DEFAULT_SUMMARY_BYTES);
        assert!(!output.contains("ARGUMENT_0"));
        assert!(!output.contains("OUTPUT_0"));
        assert!(!output.contains("LATE_PAYLOAD"));
        assert!(output.contains("cargo test [failed] exit=42"));
        for n in 1..9 {
            assert!(output.contains(&format!("ARGUMENT_{n}")));
            assert!(output.contains(&format!("OUTPUT_{n}")));
        }
        assert_eq!(
            summary
                .entries
                .iter()
                .filter(|e| e.tool.as_ref().is_some_and(|v| v.get("call").is_some()))
                .count(),
            FULL_TOOL_CALLS
        );
    }

    #[test]
    fn canonical_materialized_and_live_views_match() {
        let mut session = MaterializedSession::empty("summary-test");
        let mut live = TranscriptSummary::default();
        for n in 0..12 {
            let call = call(n);
            live.observe(&SessionUpdate::ToolCall(call.clone()));
            session.transcript.push(Arc::new(TranscriptItem {
                stable_id: format!("tool:{}", call.tool_call_id),
                position: n as u64 + 1,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body: TranscriptBody::Tool {
                    call: serde_json::to_value(call).unwrap(),
                    terminal_outputs: Vec::new(),
                    terminal_refs: Vec::new(),
                    presentation: None,
                },
            }));
        }
        let canonical = crate::projection::canonical_session_from_materialized(&session).unwrap();
        let from_live = live.render(DEFAULT_SUMMARY_BYTES);
        assert_eq!(
            from_live,
            TranscriptSummary::from_snapshot(&canonical).render(DEFAULT_SUMMARY_BYTES)
        );
        assert_eq!(
            from_live,
            TranscriptSummary::from_materialized(&session).render(DEFAULT_SUMMARY_BYTES)
        );
    }

    #[test]
    fn huge_unicode_bodies_are_marked_and_preserve_recent_outcomes() {
        let mut summary = TranscriptSummary::default();
        summary.push_user("latest user requirement");
        for n in 0..8 {
            summary.observe(&SessionUpdate::ToolCall(
                call(n).raw_output(json!({"exit_code":n,"output":"🦀\"\\\n".repeat(70_000)})),
            ));
        }
        let output = summary.render(16 * 1024);
        assert!(output.len() <= 16 * 1024);
        assert!(output.contains("bytes omitted"));
        assert!(output.contains("latest user requirement"));
        for n in 0..8 {
            assert!(output.contains(&format!("exit={n}")));
        }
        for limit in 0..100 {
            let excerpt = excerpt("🦀".repeat(100).as_str(), limit);
            assert!(excerpt.len() <= limit);
        }
    }

    #[test]
    fn context_boundary_discards_previous_conversation() {
        let mut session = MaterializedSession::empty("summary-test");
        for (position, id, body) in [
            (
                1,
                "user:old",
                TranscriptBody::User {
                    content: vec![json!({"type":"text","text":"OLD_CONTEXT"})],
                },
            ),
            (
                2,
                "context-clear:test",
                TranscriptBody::System {
                    text: "cleared".into(),
                },
            ),
            (
                3,
                "user:new",
                TranscriptBody::User {
                    content: vec![json!({"type":"text","text":"NEW_CONTEXT"})],
                },
            ),
        ] {
            let id = if position == 2 {
                format!("{}test", mj_core::archive::CONTEXT_BOUNDARY_PREFIX)
            } else {
                id.into()
            };
            session.transcript.push(Arc::new(TranscriptItem {
                stable_id: id,
                position,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body,
            }));
        }
        let output = TranscriptSummary::from_materialized(&session).render(4096);
        assert!(!output.contains("OLD_CONTEXT"));
        assert!(output.contains("NEW_CONTEXT"));
    }

    #[test]
    fn terminal_output_and_exit_are_attached_and_partial_updates_keep_name() {
        let mut summary = TranscriptSummary::default();
        let call: ToolCall = serde_json::from_value(json!({"toolCallId":"terminal","title":"Run command","kind":"execute","status":"in_progress","rawInput":{"command":"cargo test --secret argument"},"content":[{"type":"terminal","terminalId":"term-1"}]})).unwrap();
        summary.observe(&SessionUpdate::ToolCall(call));
        summary.observe_terminal(&mj_core::transcript::TerminalOutputRecord {
            terminal_id: "term-1".into(),
            output: "terminal result".into(),
            truncated: false,
            exit_code: Some(17),
            signal: None,
        });
        summary.observe(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "terminal",
            ToolCallUpdateFields::new()
                .title("Done")
                .status(ToolCallStatus::Failed),
        )));
        let output = summary.render(4096);
        assert!(output.contains("cargo test [failed] exit=17"));
        assert!(output.contains("terminal result"));
    }
    #[test]
    fn demoted_terminal_keeps_late_outcome_without_its_body() {
        let mut summary = TranscriptSummary::default();
        let terminal: ToolCall = serde_json::from_value(json!({"toolCallId":"terminal","title":"cargo test","kind":"execute","status":"in_progress","content":[{"type":"terminal","terminalId":"term"}]})).unwrap();
        summary.observe(&SessionUpdate::ToolCall(terminal));
        for n in 0..8 {
            summary.observe(&SessionUpdate::ToolCall(call(n)));
        }
        summary.observe_terminal(&mj_core::transcript::TerminalOutputRecord {
            terminal_id: "term".into(),
            output: "OLD_TERMINAL_PAYLOAD".into(),
            exit_code: Some(23),
            signal: None,
            truncated: false,
        });
        let output = summary.render(DEFAULT_SUMMARY_BYTES);
        assert!(output.contains("exit=23"));
        assert!(!output.contains("OLD_TERMINAL_PAYLOAD"));
    }

    #[test]
    fn indexed_tool_round_trip_keeps_recent_results_and_older_names() {
        let mut summary = TranscriptSummary::default();
        for n in 0..9 {
            summary.observe(&SessionUpdate::ToolCall(call(n)));
        }
        for entry in summary.entries {
            let (call, _) = indexed_tool_call(&entry.body(), "fallback");
            let restored = tool_value(&call, None, false);
            assert_eq!(restored["name"], "cargo test");
            assert_eq!(restored["status"], "completed");
            if entry.id == "call-0" {
                assert!(!call.to_string().contains("ARGUMENT_0"));
            } else {
                assert!(call["rawInput"].is_object());
                assert!(call["rawOutput"]["formatted_output"].is_string());
            }
        }
    }

    #[test]
    fn malformed_older_calls_do_not_leak_their_titles_or_payloads() {
        let invalid =
            json!({"title":"SECRET_TITLE", "rawInput":"SECRET_INPUT", "status":"completed"});
        let compact = tool_value(&invalid, None, false).to_string();
        assert!(compact.contains("invalid tool call"));
        assert!(!compact.contains("SECRET"));
    }

    #[test]
    fn a_title_update_can_supply_a_previously_unknown_operation() {
        let mut summary = TranscriptSummary::default();
        summary.observe(&SessionUpdate::ToolCall(
            ToolCall::new("tool", "Execute").kind(ToolKind::Execute),
        ));
        summary.observe(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tool",
            ToolCallUpdateFields::new().title("cargo test --nocapture"),
        )));
        assert!(summary.render(4096).contains("cargo test [pending]"));
    }
    #[test]
    fn indexed_terminals_restore_as_records_with_known_outcomes() {
        let indexed = json!({"toolCallId":"call", "name":"cargo test", "status":"failed", "call":{"toolCallId":"call","title":"cargo test","kind":"execute","status":"failed"},"terminals":{"terminal":{"output":"test failure","exit_code":17,"signal":null,"truncated":false}}});
        let (call, terminals) = indexed_tool_call(&indexed.to_string(), "unused");
        assert_eq!(call["title"], "cargo test");
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].exit_code, Some(17));
        assert_eq!(terminals[0].output, "test failure");
    }
}
