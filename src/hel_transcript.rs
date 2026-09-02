//! Transcript data shared by the worker, controller state, and the chat UI.
//!
//! [`ChatEntry`] is what a worker snapshot carries in its transcript tail and
//! what the chat view renders, and [`TranscriptItem`] is the materialized
//! form controller state persists, so both live below the modules that use
//! them rather than inside any one of them.
//!
//! The text helpers that read one of those shapes live here for the same
//! reason: the database, the projection, controller state, the compactor and
//! the review host all need the plain text of a stored message, and none of
//! them should have to reach up into the chat view to get it.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, EmbeddedResourceResource, PlanEntryStatus, SessionUpdate, ToolCall,
    ToolCallContent, ToolCallLocation, ToolCallStatus,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::hel_acp::RuntimeEvent;

pub const SESSION_RESTART_TEXT: &str = "[session restarted]";
pub const SESSION_RESTART_ITEM_PREFIX: &str = "system:session-restarted:";
/// Marks the point where the harness resumed work with no prompt in flight.
pub const HARNESS_TURN_TEXT: &str = "Agent continued on its own";
pub const HARNESS_TURN_ITEM_PREFIX: &str = "harness-turn:";

/// The current value of one logical transcript item. ACP structures whose
/// schemas can grow are kept as JSON values, while logical item identity and
/// lifecycle remain controller-owned and stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptBody {
    User {
        content: Vec<serde_json::Value>,
    },
    Agent {
        /// Complete ACP `ContentChunk` values, including message IDs, content
        /// metadata, and non-text content blocks.
        chunks: Vec<serde_json::Value>,
        streaming: bool,
    },
    Thought {
        /// Complete ACP `ContentChunk` values, including message IDs, content
        /// metadata, and non-text content blocks.
        chunks: Vec<serde_json::Value>,
        streaming: bool,
    },
    Tool {
        /// Complete current ACP `ToolCall`, updated field-for-field as
        /// `ToolCallUpdate` notifications arrive.
        call: serde_json::Value,
        /// Output of the terminals this call's content refers to. It is a
        /// sibling of `call` rather than part of it because `ToolCall::update`
        /// replaces `content` wholesale, which would discard anything injected
        /// into the stored ACP value.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        terminal_outputs: Vec<TerminalOutputRecord>,
        /// Every terminal this call has ever referred to. Agents that replace
        /// `content` wholesale can drop a terminal reference before the
        /// terminal is reaped, so the current call is not enough to decide
        /// where a terminal's output belongs.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        terminal_refs: Vec<String>,
    },
    /// Terminal output that no tool call refers to yet. It becomes a
    /// `Tool` item's `terminal_outputs` entry as soon as a call naming the
    /// terminal arrives, and stays here permanently otherwise so output is
    /// never dropped.
    TerminalOutput {
        record: TerminalOutputRecord,
    },
    Plan {
        /// Complete current ACP `Plan`, including entry priorities and all
        /// plan- and entry-level metadata.
        plan: serde_json::Value,
    },
    /// A plan the harness asked the user to approve, captured where the
    /// decision happened so it renders inline and survives restart and export.
    ///
    /// It is a record of the proposal, not conversation input: Hel never
    /// replays it to a model as a user or agent message.
    PlanProposal {
        /// Identity of the plan review that carried this proposal.
        proposal_id: String,
        /// Exact proposal text the harness sent.
        plan: String,
    },
    System {
        text: String,
    },
}

/// What one client-run terminal produced, as hel recorded it when the child
/// was reaped. `exit_code` and `signal` mirror ACP `TerminalExitStatus`; both
/// are `None` when the terminal was released before a status was observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutputRecord {
    pub terminal_id: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

impl TerminalOutputRecord {
    /// Whether the command ended the way a caller asked for: exit status zero
    /// and no signal. Anything else — a nonzero exit, a signal, or no status at
    /// all because the terminal was released before one was observed — is
    /// abnormal, and stays visible in every render mode.
    pub(crate) fn exited_cleanly(&self) -> bool {
        self.exit_code == Some(0) && self.signal.is_none()
    }

    /// Whether a completed ACP tool's provider-specific raw result is this
    /// child result. Kimi reports shell output as a byte array beside its exit
    /// status but omits the ACP terminal reference, so the exact result is the
    /// only ownership information it publishes.
    pub(crate) fn matches_tool_raw_result(&self, call: &serde_json::Value) -> bool {
        if !matches!(
            call.get("status").and_then(serde_json::Value::as_str),
            Some("completed" | "failed")
        ) {
            return false;
        }
        let Some(raw) = call.get("rawOutput") else {
            return false;
        };
        let Some(exit_code) = raw
            .get("exit_code")
            .and_then(serde_json::Value::as_u64)
            .and_then(|code| u32::try_from(code).ok())
        else {
            return false;
        };
        if self.exit_code != Some(exit_code) || self.signal.is_some() {
            return false;
        }
        match raw.get("output") {
            Some(serde_json::Value::Array(bytes)) => {
                bytes.len() == self.output.len()
                    && bytes
                        .iter()
                        .zip(self.output.as_bytes())
                        .all(|(value, byte)| value.as_u64() == Some(u64::from(*byte)))
            }
            Some(serde_json::Value::String(output)) => output == &self.output,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptItem {
    pub stable_id: String,
    /// Ordinal of the relay event that first created this logical item.
    pub position: u64,
    /// Ordinal of the most recent content chunk for an agent message. This is
    /// `None` for every other logical item.
    pub latest_content_event_ordinal: Option<u64>,
    pub created_at_ms: i64,
    pub last_changed_at_ms: i64,
    pub body: TranscriptBody,
}

impl TranscriptItem {
    pub fn is_session_restart(&self) -> bool {
        self.stable_id.starts_with(SESSION_RESTART_ITEM_PREFIX)
    }

    /// Whether this item begins a turn: a user message, or the marker for a
    /// turn the harness started on its own. The recovery boundary and the
    /// scope of a plan update both key on the newest of these.
    pub fn is_turn_start(&self) -> bool {
        matches!(self.body, TranscriptBody::User { .. })
            || self.stable_id.starts_with(HARNESS_TURN_ITEM_PREFIX)
    }

    pub fn is_nonempty_agent_message(&self) -> bool {
        let TranscriptBody::Agent { chunks, .. } = &self.body else {
            return false;
        };
        chunks.iter().any(|chunk| {
            let Some(content) = chunk.get("content") else {
                return false;
            };
            match content.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty()),
                Some(_) => true,
                None => false,
            }
        })
    }

    pub(crate) fn validate(&self, through: u64) -> Result<()> {
        if self.stable_id.trim().is_empty() {
            bail!("materialized transcript item has an empty stable id");
        }
        if self.position == 0 || self.position > through {
            bail!(
                "materialized transcript item {:?} has invalid position {} at frontier {through}",
                self.stable_id,
                self.position
            );
        }
        match (&self.body, self.latest_content_event_ordinal) {
            (TranscriptBody::Agent { .. }, Some(ordinal))
                if ordinal >= self.position && ordinal <= through => {}
            (TranscriptBody::Agent { .. }, Some(ordinal)) => bail!(
                "materialized agent message {:?} has invalid latest content ordinal {ordinal} at position {} and frontier {through}",
                self.stable_id,
                self.position
            ),
            (TranscriptBody::Agent { .. }, None) => bail!(
                "materialized agent message {:?} has no latest content ordinal",
                self.stable_id
            ),
            (_, Some(ordinal)) => bail!(
                "non-agent transcript item {:?} has latest content ordinal {ordinal}",
                self.stable_id
            ),
            (_, None) => {}
        }
        if self.last_changed_at_ms < self.created_at_ms {
            bail!(
                "materialized transcript item {:?} changed before it was created",
                self.stable_id
            );
        }
        Ok(())
    }
}

/// Reduce a tool call to what a reader still needs, once a verified checkpoint
/// holds the whole of it.
///
/// Tool output is where a projection's bytes are: on one measured session,
/// 561 MB of 635 MiB. Behind a checkpoint nothing reads it — the checkpoint
/// archive carries the complete transcript, and restoring it brings the output
/// back — so what stays here is what the transcript still shows: which tool
/// ran, on what, with what result, and how many lines each edit changed.
///
/// Returns whether anything changed, so a caller can skip the write.
pub fn compact_tool_call_for_retention(body: &mut TranscriptBody) -> bool {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        terminal_refs,
    } = body
    else {
        return false;
    };
    let Some(object) = call.as_object_mut() else {
        return false;
    };
    let mut changed = !terminal_outputs.is_empty() || !terminal_refs.is_empty();
    terminal_outputs.clear();
    terminal_refs.clear();
    for field in ["rawInput", "rawOutput", "_meta"] {
        changed |= object.remove(field).is_some();
    }
    let Some(content) = object
        .get_mut("content")
        .and_then(|value| value.as_array_mut())
    else {
        return changed;
    };
    let before = content.len();
    // Diffs stay, because the transcript still shows their stat. Their patch
    // text does not, and neither do the two file copies an older record holds
    // instead of a patch: `hel_diff::drop_patch_text` turns those into the
    // counts `format_diffstat` reads before dropping them.
    content.retain(|item| item.get("type").and_then(|kind| kind.as_str()) == Some("diff"));
    changed |= content.len() != before;
    for item in content.iter_mut() {
        changed |= drop_diff_body(item);
    }
    changed
}

fn drop_diff_body(item: &mut serde_json::Value) -> bool {
    use agent_client_protocol::schema::v1::ToolCallContent;

    // Round-trip through `ToolCallContent`, not `Diff`: the variant tag lives
    // on the enum, and writing back a bare `Diff` would strip it and make the
    // whole tool call unreadable.
    let mut content = match serde_json::from_value::<ToolCallContent>(item.clone()) {
        Ok(content) => content,
        // Content this cannot read is content it must not rewrite.
        Err(error) => {
            tracing::warn!(%error, "skipping unreadable tool content during retention");
            return false;
        }
    };
    let ToolCallContent::Diff(diff) = &mut content else {
        return false;
    };
    if !crate::hel_diff::drop_patch_text(diff) {
        return false;
    }
    match serde_json::to_value(&content) {
        Ok(value) => {
            *item = value;
            true
        }
        Err(error) => {
            tracing::warn!(%error, "could not rewrite a diff during retention");
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChatRole {
    User,
    Agent,
    /// Agent reasoning stream, rendered dimmed.
    Thought,
    /// Tool invocation titles.
    Tool,
    /// Current agent plan.
    Plan,
    /// A plan proposal awaiting, or already given, a decision.
    PlanProposal,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatEntry {
    #[serde(default)]
    pub(crate) start_seq: u64,
    pub seq: u64,
    pub role: ChatRole,
    pub text: String,
    pub(crate) recorded_at_ms: Option<i64>,
    pub(crate) revision: u64,
    pub(crate) message_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) tool_status: Option<ToolStatus>,
    pub(crate) tool_content: Vec<String>,
    pub(crate) tool_diffstats: Vec<String>,
    pub(crate) tool_locations: Vec<String>,
    pub(crate) plan: Vec<PlanLine>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) leading_omitted: bool,
    /// Detail the decluttered feed leaves out: the entry renders only in the
    /// raw transcript mode. Set once, when the entry is built, because Alt-T
    /// switches render mode without rebuilding entries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) raw_only: bool,
    /// The materialized transcript item this entry was derived from, when it
    /// came from the controller's projection. Provenance only, so it is
    /// neither serialized nor part of the entry's value.
    #[serde(skip)]
    pub(crate) source: TranscriptSource,
}

/// Handle on the transcript item an entry was derived from. Unchanged items
/// keep the same `Arc` from one projection to the next, so a pointer
/// comparison replaces re-reading the item and re-parsing its JSON.
///
/// The handle records where an entry came from, not what it says, so two
/// entries with equal content are equal whatever they were derived from.
#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptSource(pub(crate) Option<Arc<TranscriptItem>>);

impl TranscriptSource {
    pub(crate) fn is(&self, item: &Arc<TranscriptItem>) -> bool {
        self.0
            .as_ref()
            .is_some_and(|source| Arc::ptr_eq(source, item))
    }
}

impl PartialEq for TranscriptSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for TranscriptSource {}

impl ChatEntry {
    pub(crate) fn plan(seq: u64, plan: Vec<PlanLine>) -> Self {
        Self {
            start_seq: seq,
            seq,
            role: ChatRole::Plan,
            text: String::new(),
            recorded_at_ms: None,
            revision: 0,
            message_id: None,
            tool_call_id: None,
            tool_status: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan,
            leading_omitted: false,
            raw_only: false,
            source: TranscriptSource::default(),
        }
    }

    pub(crate) fn touch(&mut self, seq: u64) {
        self.seq = seq;
        self.revision = self.revision.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn bounded_for_dashboard(mut self) -> Self {
        self.bound_dashboard_content();
        self
    }

    #[cfg(test)]
    fn bound_dashboard_content(&mut self) {
        const TEXT_BYTES: usize = 64 * 1024;
        const DETAIL_BYTES: usize = 2 * 1024;
        const DETAIL_COUNT: usize = 8;

        self.leading_omitted |= truncate_string_start(&mut self.text, TEXT_BYTES);
        for values in [
            &mut self.tool_content,
            &mut self.tool_diffstats,
            &mut self.tool_locations,
        ] {
            values.truncate(DETAIL_COUNT);
            for value in values {
                truncate_string_start(value, DETAIL_BYTES);
            }
        }
        self.plan.truncate(DETAIL_COUNT);
        for line in &mut self.plan {
            truncate_string_start(&mut line.text, DETAIL_BYTES);
        }
    }

    pub(crate) fn with_recorded_at(mut self, recorded_at_ms: Option<i64>) -> Self {
        self.recorded_at_ms = recorded_at_ms;
        self
    }
}

/// Constructors that sanitize the text they are given, so terminal escape
/// sequences from a harness never reach a transcript entry.
impl ChatEntry {
    pub(crate) fn plain(seq: u64, role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            start_seq: seq,
            seq,
            role,
            text: sanitize_terminal_text(&text.into()),
            recorded_at_ms: None,
            revision: 0,
            message_id: None,
            tool_call_id: None,
            tool_status: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan: Vec::new(),
            leading_omitted: false,
            raw_only: false,
            source: TranscriptSource::default(),
        }
    }

    pub(crate) fn tool(
        seq: u64,
        title: impl Into<String>,
        tool_call_id: Option<String>,
        tool_status: ToolStatus,
    ) -> Self {
        Self {
            start_seq: seq,
            seq,
            role: ChatRole::Tool,
            text: sanitize_terminal_text(&title.into()),
            recorded_at_ms: None,
            revision: 0,
            message_id: None,
            tool_call_id,
            tool_status: Some(tool_status),
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan: Vec::new(),
            leading_omitted: false,
            raw_only: false,
            source: TranscriptSource::default(),
        }
    }
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn plan_status(status: &PlanEntryStatus) -> PlanStatus {
    match status {
        PlanEntryStatus::InProgress => PlanStatus::Running,
        PlanEntryStatus::Completed => PlanStatus::Completed,
        _ => PlanStatus::Pending,
    }
}

pub(crate) fn tool_content_details(
    content: &[ToolCallContent],
    terminal_outputs: &[TerminalOutputRecord],
    raw_output: Option<&serde_json::Value>,
) -> Vec<String> {
    let mut details = Vec::new();
    let mut referenced: Vec<&str> = Vec::new();
    for item in content {
        let detail = match item {
            ToolCallContent::Content(content) => content_block_text(&content.content),
            ToolCallContent::Diff(_) => None,
            // Kimi-style agents send a terminal reference and no textual copy
            // of the output, so the record hel captured is the only thing a
            // reader ever sees. Until the terminal is reaped there is none.
            ToolCallContent::Terminal(terminal) => {
                let terminal_id = terminal.terminal_id.0.as_ref();
                referenced.push(terminal_id);
                Some(
                    terminal_outputs
                        .iter()
                        .find(|record| record.terminal_id.as_str() == terminal_id)
                        .map(terminal_output_detail)
                        .or_else(|| raw_output.and_then(raw_output_terminal_detail))
                        .unwrap_or_else(|| format!("terminal {}", terminal.terminal_id)),
                )
            }
            _ => None,
        };
        if let Some(detail) = detail {
            details.push(sanitize_terminal_text(&detail));
        }
    }
    // Grok-style agents name the terminal on a mid-flight update and then
    // replace `content` wholesale without it, so the output hel captured has
    // nothing in the final call pointing at it. Show it rather than lose it.
    for record in terminal_outputs {
        if referenced.contains(&record.terminal_id.as_str()) {
            continue;
        }
        let output = sanitize_terminal_text(&record.output);
        if !output.is_empty() && details.iter().any(|detail| detail == &output) {
            // Kimi sends the captured stdout as ordinary tool content and in
            // its raw result. Keep the exit summary without printing those
            // same bytes a second time in Raw mode.
            details.push(terminal_exit_summary(record));
        } else {
            details.push(sanitize_terminal_text(&terminal_output_detail(record)));
        }
    }
    details
}

/// The output codex reports for a terminal it ran itself. Codex names its own
/// server-side terminal, which hel never opened and has no record for, and
/// puts the text in `rawOutput`; reading it here keeps such a call from
/// rendering as a bare terminal id.
fn raw_output_terminal_detail(raw_output: &serde_json::Value) -> Option<String> {
    let output = raw_output.get("formatted_output")?.as_str()?;
    let Some(exit_code) = raw_output
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
    else {
        return Some(output.to_owned());
    };
    let summary = format!("exited {exit_code}");
    if output.is_empty() {
        return Some(summary);
    }
    Some(format!("{output}\n{summary}"))
}

/// One terminal's output followed by how it ended.
pub(crate) fn terminal_output_detail(record: &TerminalOutputRecord) -> String {
    let summary = terminal_exit_summary(record);
    if record.output.is_empty() {
        return summary;
    }
    format!("{}\n{summary}", record.output)
}

/// How a terminal ended, in one line.
fn terminal_exit_summary(record: &TerminalOutputRecord) -> String {
    let mut summary = match (record.exit_code, &record.signal) {
        (_, Some(signal)) => format!("killed by {signal}"),
        (Some(code), None) => format!("exited {code}"),
        (None, None) => "released before exit".to_owned(),
    };
    if record.truncated {
        summary.push_str(" · output truncated");
    }
    summary
}

pub(crate) fn tool_diff_paths(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(diff.path.display().to_string()),
            _ => None,
        })
        .collect()
}

pub(crate) fn tool_location_details(locations: &[ToolCallLocation]) -> Vec<String> {
    locations
        .iter()
        .map(|location| match location.line {
            Some(line) => format!("{}:{line}", location.path.display()),
            None => location.path.display().to_string(),
        })
        .collect()
}

/// Append streamed agent or thought text to the transcript, merging it into
/// the entry it continues so a message arrives as one entry rather than one
/// per chunk.
pub(crate) fn push_streamed_entry(
    entries: &mut Vec<ChatEntry>,
    seq: u64,
    recorded_at_ms: Option<i64>,
    role: ChatRole,
    message_id: Option<String>,
    text: &str,
) {
    let text = sanitize_terminal_text(text);
    if let Some(last) = entries.last_mut()
        && last.role == role
        && (role == ChatRole::Thought || last.message_id == message_id)
    {
        last.touch(seq);
        if role == ChatRole::Thought
            && last.message_id != message_id
            && !last.text.is_empty()
            && !text.is_empty()
        {
            while last.text.ends_with('\n') {
                last.text.pop();
            }
            last.text.push('\n');
            last.text.push_str(text.trim_start_matches('\n'));
        } else {
            last.text.push_str(&text);
        }
        return;
    }
    let mut entry = ChatEntry::plain(seq, role, text).with_recorded_at(recorded_at_ms);
    entry.message_id = message_id;
    entries.push(entry);
}

/// Apply the transcript-visible part of one ACP session update. Returns the
/// update again when it changes the session surface rather than the
/// transcript, so the chat view handles those without decoding twice.
pub(crate) fn apply_session_update_to_entries(
    entries: &mut Vec<ChatEntry>,
    seq: u64,
    recorded_at_ms: Option<i64>,
    update: SessionUpdate,
) -> Option<SessionUpdate> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let message_id = chunk.message_id.map(|id| id.to_string());
            if let Some(text) = content_block_text(&chunk.content) {
                push_streamed_entry(
                    entries,
                    seq,
                    recorded_at_ms,
                    ChatRole::Agent,
                    message_id,
                    &text,
                );
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            let message_id = chunk.message_id.map(|id| id.to_string());
            if let Some(text) = content_block_text(&chunk.content) {
                push_streamed_entry(
                    entries,
                    seq,
                    recorded_at_ms,
                    ChatRole::Thought,
                    message_id,
                    &text,
                );
            }
        }
        // PromptAccepted is the canonical local user-message event. ACP
        // user chunks would duplicate it during replay.
        SessionUpdate::UserMessageChunk(_) => {}
        SessionUpdate::ToolCall(call) => {
            let mut entry = ChatEntry::tool(
                seq,
                call.title,
                Some(call.tool_call_id.to_string()),
                tool_status(&call.status),
            );
            entry.tool_content = tool_content_details(&call.content, &[], call.raw_output.as_ref());
            entry.tool_diffstats = tool_diff_paths(&call.content);
            entry.tool_locations = tool_location_details(&call.locations);
            entries.push(entry);
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let tool_call_id = update.tool_call_id.to_string();
            let entry = entries.iter_mut().rev().find(|entry| {
                entry.role == ChatRole::Tool
                    && entry.tool_call_id.as_deref() == Some(tool_call_id.as_str())
            })?;
            entry.touch(seq);
            if let Some(title) = update.fields.title {
                entry.text = sanitize_terminal_text(&title);
            }
            if let Some(status) = update.fields.status {
                entry.tool_status = Some(tool_status(&status));
            }
            if let Some(content) = update.fields.content {
                entry.tool_content =
                    tool_content_details(&content, &[], update.fields.raw_output.as_ref());
                entry.tool_diffstats = tool_diff_paths(&content);
            }
            if let Some(locations) = update.fields.locations {
                entry.tool_locations = tool_location_details(&locations);
            }
        }
        SessionUpdate::Plan(plan) => {
            let lines = plan
                .entries
                .into_iter()
                .map(|entry| PlanLine {
                    text: sanitize_terminal_text(&entry.content),
                    status: plan_status(&entry.status),
                })
                .collect();
            let latest_user_seq = entries
                .iter()
                .rev()
                .find(|entry| entry.role == ChatRole::User)
                .map_or(0, |entry| entry.seq);
            if let Some(entry) = entries
                .iter_mut()
                .rev()
                .find(|entry| entry.role == ChatRole::Plan && entry.seq > latest_user_seq)
            {
                entry.touch(seq);
                entry.plan = lines;
            } else {
                entries.push(ChatEntry::plan(seq, lines));
            }
        }
        other => return Some(other),
    }
    None
}

/// Apply the transcript-visible part of one persisted runtime event. Returns
/// the event again when it only configures the session surface, which is the
/// chat view's business rather than the transcript's.
pub(crate) fn apply_runtime_event_to_entries(
    entries: &mut Vec<ChatEntry>,
    seq: u64,
    recorded_at_ms: Option<i64>,
    runtime: RuntimeEvent,
) -> Option<RuntimeEvent> {
    match runtime {
        RuntimeEvent::SessionUpdate { update } => {
            let parsed = match serde_json::from_value::<SessionUpdate>(update.clone()) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::debug!(%error, "ignoring invalid ACP session update");
                    return None;
                }
            };
            apply_session_update_to_entries(entries, seq, recorded_at_ms, parsed)
                .map(|_| RuntimeEvent::SessionUpdate { update })
        }
        RuntimeEvent::Warning { message } => {
            entries.push(ChatEntry::plain(
                seq,
                ChatRole::System,
                format!("warning: {message}"),
            ));
            None
        }
        RuntimeEvent::ConfigApplied { key, value, .. } => {
            entries.push(ChatEntry::plain(
                seq,
                ChatRole::System,
                format!("{key} set to {value}"),
            ));
            None
        }
        RuntimeEvent::SessionStarted { resumed: false, .. } => {
            entries.push(ChatEntry::plain(
                seq,
                ChatRole::System,
                "harness session started",
            ));
            None
        }
        RuntimeEvent::SessionStarted { resumed: true, .. } => None,
        other => Some(other),
    }
}

/// Remove terminal controls while preserving user-visible whitespace.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // One escape can end at the ESC introducing the next one, so keep
            // consuming rather than recursing: transcript text is untrusted and
            // may nest these arbitrarily deep.
            while consume_escape_body(&mut chars) {}
        } else if ch == '\r' {
            if chars.peek() != Some(&'\n') {
                sanitized.push('\n');
            }
        } else if matches!(ch, '\n' | '\t') || !ch.is_control() {
            sanitized.push(ch);
        }
    }
    sanitized
}

/// Consume one escape sequence's body, after its introducing ESC. Returns
/// whether the body ended at another ESC, which introduces the next sequence.
///
/// Dropping the ESC alone is not enough: an OSC payload (a build tool setting
/// the window title) or the second byte of a charset selection would otherwise
/// reach the transcript as visible text.
fn consume_escape_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    match chars.next() {
        // CSI: parameter and intermediate bytes up to a final byte.
        Some('[') => {
            let _ = chars.find(|ch| ('@'..='~').contains(ch));
            false
        }
        // OSC, DCS, SOS, PM, and APC all carry a string payload.
        Some(']' | 'P' | 'X' | '^' | '_') => consume_string_body(chars),
        // Two-byte sequences: charset selection (ESC ( B), ESC # 8, ESC SP F.
        Some('(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%' | ' ') => {
            chars.next();
            false
        }
        // Everything else is a complete one-byte escape: ESC 7, ESC 8, ESC M,
        // ESC =, and a trailing ESC with nothing after it.
        _ => false,
    }
}

/// Consume a string payload, which ends at BEL or at ST (ESC \). A line break
/// or a cancel control aborts it instead, so one malformed OSC cannot swallow
/// the rest of a transcript.
fn consume_string_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    while let Some(&ch) = chars.peek() {
        match ch {
            '\n' | '\r' | '\x18' | '\x1a' => return false,
            '\x07' => {
                chars.next();
                return false;
            }
            '\x1b' => {
                chars.next();
                return true;
            }
            _ => {
                chars.next();
            }
        }
    }
    false
}

pub fn materialized_content_text(content: &[serde_json::Value]) -> String {
    let text = content
        .iter()
        .map(materialized_value_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    crate::hel_worker::strip_hidden_prompt_context(&text).to_owned()
}

pub fn materialized_chunks_text(chunks: &[serde_json::Value]) -> String {
    chunks
        .iter()
        .filter_map(|value| match ContentChunk::deserialize(value) {
            Ok(chunk) => Some(chunk),
            Err(error) => {
                tracing::warn!(%error, "could not decode a stored content chunk");
                None
            }
        })
        .filter_map(|chunk| content_block_text(&chunk.content))
        .map(|text| sanitize_terminal_text(&text))
        .collect::<Vec<_>>()
        .join("")
}

fn materialized_value_text(value: &serde_json::Value) -> String {
    if let Ok(block) = ContentBlock::deserialize(value)
        && let Some(text) = content_block_text(&block)
    {
        return sanitize_terminal_text(&text);
    }
    if let Some(text) = value.as_str() {
        return sanitize_terminal_text(text);
    }
    sanitize_terminal_text(&serde_json::to_string(value).unwrap_or_else(|_| "[content]".into()))
}

pub(crate) fn tool_status(status: &ToolCallStatus) -> ToolStatus {
    match status {
        ToolCallStatus::InProgress => ToolStatus::Running,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

pub(crate) fn content_block_text(content: &ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text.clone()),
        ContentBlock::Image(_) => Some("[image]".into()),
        ContentBlock::Audio(_) => Some("[audio]".into()),
        ContentBlock::ResourceLink(link) => Some(format!("[{}]({})", link.name, link.uri)),
        ContentBlock::Resource(resource) => Some(match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(resource) => resource.text.clone(),
            EmbeddedResourceResource::BlobResourceContents(resource) => {
                format!("[embedded resource: {}]", resource.uri)
            }
            _ => "[embedded resource]".into(),
        }),
        _ => None,
    }
}

pub(crate) fn compute_tool_diffstats(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(format_diffstat(diff)),
            _ => None,
        })
        .collect()
}

pub fn materialized_tool_diffstats(item: &TranscriptItem) -> Option<Vec<String>> {
    let TranscriptBody::Tool { call, .. } = &item.body else {
        return None;
    };
    let call = match ToolCall::deserialize(call) {
        Ok(call) => call,
        Err(error) => {
            tracing::warn!(
                stable_id = %item.stable_id,
                %error,
                "could not decode a stored tool call while reading diff summary"
            );
            return None;
        }
    };
    if !matches!(
        tool_status(&call.status),
        ToolStatus::Completed | ToolStatus::Failed
    ) {
        return None;
    }
    let diffstats = compute_tool_diffstats(&call.content);
    (!diffstats.is_empty()).then_some(diffstats)
}

fn format_diffstat(diff: &agent_client_protocol::schema::v1::Diff) -> String {
    // A diff recorded since `hel_diff` landed already carries its counts, so
    // this is a lookup. An older record still holds both file copies and is
    // diffed here on demand.
    let patch = crate::hel_diff::patch_of(diff);
    format!(
        "{}  +{} −{}",
        diff.path.display(),
        patch.insertions,
        patch.deletions
    )
}

#[cfg(test)]
fn truncate_string_start(value: &mut String, maximum_bytes: usize) -> bool {
    if value.len() <= maximum_bytes {
        return false;
    }
    let mut start = value.len() - maximum_bytes;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
    true
}

/// The ACP tool states needed to keep a compact tool block visually useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum PlanStatus {
    Pending,
    Running,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlanLine {
    pub(crate) text: String,
    pub(crate) status: PlanStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_new_file_diff_counts_each_inserted_line() {
        let diff = agent_client_protocol::schema::v1::Diff::new("/workspace/new.txt", "one\ntwo\n");

        assert_eq!(format_diffstat(&diff), "/workspace/new.txt  +2 \u{2212}0");
    }

    #[test]
    fn terminal_exit_summary_names_signal_release_and_truncation() {
        let record = |exit_code, signal: Option<&str>, truncated| TerminalOutputRecord {
            terminal_id: "term-1".into(),
            output: "out".into(),
            truncated,
            exit_code,
            signal: signal.map(str::to_owned),
        };

        assert_eq!(
            terminal_exit_summary(&record(Some(0), None, false)),
            "exited 0"
        );
        assert_eq!(
            terminal_exit_summary(&record(Some(1), None, true)),
            "exited 1 · output truncated"
        );
        assert_eq!(
            terminal_exit_summary(&record(None, Some("SIGKILL"), false)),
            "killed by SIGKILL"
        );
        assert_eq!(
            terminal_exit_summary(&record(None, None, false)),
            "released before exit"
        );

        // A terminal that produced nothing is still worth a line: the summary
        // is all a reader has to go on.
        let mut silent = record(None, Some("SIGTERM"), false);
        silent.output.clear();
        assert_eq!(terminal_output_detail(&silent), "killed by SIGTERM");
    }
}
