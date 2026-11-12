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

use crate::packed_text::PackedText;
use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, EmbeddedResourceResource, PlanEntryStatus, ToolCallStatus, ToolKind,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const SESSION_RESTART_TEXT: &str = "[session restarted]";
pub const SESSION_RESTART_ITEM_PREFIX: &str = "system:session-restarted:";
pub const WORK_INTERRUPTED_ITEM_PREFIX: &str = "system:work-interrupted:";
/// Marks where a turn the user cancelled stopped.
pub const TURN_INTERRUPTED_TEXT: &str = "Interrupted";
pub const TURN_INTERRUPTED_ITEM_PREFIX: &str = "system:turn-interrupted:";
/// Marks the point where the harness resumed work with no prompt in flight.
pub const HARNESS_TURN_TEXT: &str = "Agent continued on its own";
pub const HARNESS_TURN_ITEM_PREFIX: &str = "harness-turn:";
/// User replies within an existing turn, rather than prompts that start one.
pub const ELICITATION_REPLY_ITEM_PREFIX: &str = "elicitation-reply:";

/// Shared read-time join of embedded and parked terminal output.
/// Standalone close-time records override embedded streaming records.
pub struct TerminalOutputs<'a, R = TerminalOutputRecord> {
    standalone: std::collections::HashMap<&'a str, &'a R>,
    tools: std::collections::HashMap<&'a str, (&'a [R], &'a [String])>,
    referenced: std::collections::HashSet<&'a str>,
}

pub trait TerminalRecord: Clone {
    fn terminal_id(&self) -> &str;
}
impl TerminalRecord for TerminalOutputRecord {
    fn terminal_id(&self) -> &str {
        &self.terminal_id
    }
}
impl TerminalRecord for crate::archive::CanonicalTerminalOutput {
    fn terminal_id(&self) -> &str {
        &self.terminal_id
    }
}
impl<'a, R: TerminalRecord> TerminalOutputs<'a, R> {
    fn new() -> Self {
        Self {
            standalone: Default::default(),
            tools: Default::default(),
            referenced: Default::default(),
        }
    }
    fn tool(&mut self, id: &'a str, embedded: &'a [R], refs: &'a [String]) {
        self.tools.insert(id, (embedded, refs));
        self.referenced.extend(refs.iter().map(String::as_str));
        self.referenced
            .extend(embedded.iter().map(TerminalRecord::terminal_id));
    }
    pub fn for_tool(&self, stable_id: &str) -> std::borrow::Cow<'a, [R]> {
        let Some((embedded, refs)) = self.tools.get(stable_id) else {
            return std::borrow::Cow::Borrowed(&[]);
        };
        let mut seen = std::collections::HashSet::new();
        if embedded.iter().all(|record| {
            seen.insert(record.terminal_id()) && !self.standalone.contains_key(record.terminal_id())
        }) && !refs
            .iter()
            .any(|id| self.standalone.contains_key(id.as_str()))
        {
            return std::borrow::Cow::Borrowed(embedded);
        }
        seen.clear();
        let mut records = Vec::new();
        for record in *embedded {
            let id = record.terminal_id();
            if seen.insert(id) {
                records.push((*self.standalone.get(id).unwrap_or(&record)).clone());
            }
        }
        for id in *refs {
            if seen.insert(id.as_str())
                && let Some(record) = self.standalone.get(id.as_str())
            {
                records.push((*record).clone());
            }
        }
        std::borrow::Cow::Owned(records)
    }
    pub fn embedded_for_tool(&self, stable_id: &str) -> &[R] {
        self.tools
            .get(stable_id)
            .map_or(&[], |(records, _)| *records)
    }
    pub fn referenced_terminal_ids(&self) -> impl Iterator<Item = &str> {
        self.referenced.iter().copied()
    }
    pub fn has_standalone(&self) -> bool {
        !self.standalone.is_empty()
    }
    pub fn shown_under_tool(&self, terminal_id: &str) -> bool {
        self.referenced.contains(terminal_id)
    }
}
impl<'a> TerminalOutputs<'a> {
    pub fn of(items: impl IntoIterator<Item = &'a TranscriptItem>) -> Self {
        let mut outputs = Self::new();
        for item in items {
            match &item.body {
                TranscriptBody::Tool {
                    terminal_outputs,
                    terminal_refs,
                    ..
                } => outputs.tool(&item.stable_id, terminal_outputs, terminal_refs),
                TranscriptBody::TerminalOutput { record } => {
                    outputs.standalone.insert(&record.terminal_id, record);
                }
                _ => {}
            }
        }
        outputs
    }
}
impl<'a> TerminalOutputs<'a, crate::archive::CanonicalTerminalOutput> {
    pub fn of_canonical(
        items: impl IntoIterator<Item = &'a crate::archive::CanonicalTranscriptItem>,
    ) -> Self {
        let mut outputs = Self::new();
        for item in items {
            match &item.body {
                crate::archive::CanonicalTranscriptBody::Tool {
                    terminal_outputs,
                    terminal_refs,
                    ..
                } => outputs.tool(&item.stable_id, terminal_outputs, terminal_refs),
                crate::archive::CanonicalTranscriptBody::TerminalOutput { record } => {
                    outputs.standalone.insert(&record.terminal_id, record);
                }
                _ => {}
            }
        }
        outputs
    }
}

/// Where a tool's compact presentation source came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSummarySourceKind {
    RawInput,
    RawOutput,
    Title,
}

/// Bounded presentation metadata derived from an ACP tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallPresentation {
    pub summary: String,
    pub source: PackedText,
    pub source_kind: ToolSummarySourceKind,
    pub tool_kind: ToolKind,
    /// Version of the parser rules that produced `summary`. A missing value
    /// identifies presentation metadata written before parser versioning.
    #[serde(default)]
    pub summary_version: u8,
}

/// An ACP call retained as JSON text. All cached interpretations are built
/// together; replacing the call cannot leave metadata from an older update.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct OpaqueToolCall {
    json: PackedText,
    metadata: Box<ToolCallMetadata>,
}

#[derive(Debug, Clone)]
struct ToolCallMetadata {
    raw_status: Option<Box<str>>,
    typed_status: Option<ToolCallStatus>,
    id: Option<Box<str>>,
    terminal_ids: Vec<String>,
    fallback: bool,
    raw_result: Option<(u32, usize, [u8; 32])>,
}

impl Serialize for OpaqueToolCall {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::value::RawValue::from_string(self.json.text().into_owned())
            .expect("opaque JSON")
            .serialize(serializer)
    }
}
impl PartialEq for OpaqueToolCall {
    fn eq(&self, other: &Self) -> bool {
        self.json == other.json || self.value() == other.value()
    }
}
impl Eq for OpaqueToolCall {}

impl From<serde_json::Value> for OpaqueToolCall {
    fn from(value: serde_json::Value) -> Self {
        use agent_client_protocol::v1::schema::ToolCall;
        use sha2::{Digest, Sha256};
        let decoded = ToolCall::deserialize(&value).ok();
        let raw_result = (|| {
            if !matches!(
                value.get("status").and_then(serde_json::Value::as_str),
                Some("completed" | "failed")
            ) {
                return None;
            }
            let raw = value.get("rawOutput")?;
            let exit = u32::try_from(raw.get("exit_code")?.as_u64()?).ok()?;
            let output = raw.get("output")?;
            let (len, digest) = match output {
                serde_json::Value::String(s) => (s.len(), Sha256::digest(s.as_bytes()).into()),
                serde_json::Value::Array(values) => {
                    let mut hash = Sha256::new();
                    for value in values {
                        hash.update([u8::try_from(value.as_u64()?).ok()?]);
                    }
                    (values.len(), hash.finalize().into())
                }
                _ => return None,
            };
            Some((exit, len, digest))
        })();
        let metadata = ToolCallMetadata {
            raw_status: value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .map(Into::into),
            typed_status: decoded.as_ref().map(|call| call.status),
            id: value
                .get("toolCallId")
                .and_then(serde_json::Value::as_str)
                .map(Into::into),
            terminal_ids: tool_call_terminal_ids(&value),
            fallback: decoded
                .as_ref()
                .is_some_and(crate::acp::is_fallback_terminal_tool_call),
            raw_result,
        };
        Self {
            json: serde_json::to_string(&value)
                .expect("JSON Value serializes")
                .into(),
            metadata: Box::new(metadata),
        }
    }
}

impl OpaqueToolCall {
    pub fn edit<R>(&mut self, edit: impl FnOnce(&mut serde_json::Value) -> R) -> R {
        let mut value = self.value();
        let result = edit(&mut value);
        *self = value.into();
        result
    }
    pub fn value(&self) -> serde_json::Value {
        serde_json::from_str(&self.json.text()).expect("opaque call contains valid JSON")
    }
    pub fn decode(&self) -> serde_json::Result<agent_client_protocol::v1::schema::ToolCall> {
        serde_json::from_str(&self.json.text())
    }
    pub fn raw_status(&self) -> Option<&str> {
        self.metadata.raw_status.as_deref()
    }
    pub fn typed_status(&self) -> ToolCallStatus {
        self.metadata
            .typed_status
            .unwrap_or(ToolCallStatus::Pending)
    }
    pub fn typed_valid(&self) -> bool {
        self.metadata.typed_status.is_some()
    }
    pub fn id(&self) -> Option<&str> {
        self.metadata.id.as_deref()
    }
    pub fn terminal_ids(&self) -> &[String] {
        &self.metadata.terminal_ids
    }
    pub fn is_fallback(&self) -> bool {
        self.metadata.fallback
    }
    pub fn matches_raw_result(&self, record: &TerminalOutputRecord, digest: &[u8; 32]) -> bool {
        self.metadata.raw_result.is_some_and(|(exit, len, expected)| {
            record.exit_code == Some(exit) && record.signal.is_none() && record.output.len() == len
                && &expected == digest
                // Keep exact ownership even in the event of a digest collision.
                && record.matches_tool_raw_result(&self.value())
        })
    }
}

/// One interpretation of ACP terminal references for opaque metadata and
/// transient JSON readers (for example summaries and relay update fields).
pub fn tool_call_terminal_ids(call: &serde_json::Value) -> Vec<String> {
    use agent_client_protocol::v1::schema::ToolCallContent;
    let Some(serde_json::Value::Array(content)) = call.get("content") else {
        return Vec::new();
    };
    content.iter().filter_map(|value| match ToolCallContent::deserialize(value) {
        Ok(ToolCallContent::Terminal(terminal)) => Some(terminal.terminal_id.0.to_string()),
        Ok(_) => None,
        Err(error) => { tracing::warn!(%error, "ignoring malformed tool-call content while locating terminal output"); None }
    }).collect()
}

/// The current value of one logical transcript item. ACP structures whose
/// schemas can grow are kept as JSON values, while logical item identity and
/// lifecycle remain controller-owned and stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptBody {
    /// A complete message at the worker's durable delivery boundary.
    Message {
        event: Box<crate::mailbox::MailboxEvent>,
    },
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
        call: OpaqueToolCall,
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
        /// Cached label data used by Rich and browser projections. Older
        /// transcript items omit this and derive it from `call` when read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presentation: Option<Box<ToolCallPresentation>>,
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

/// Append one ACP `ContentChunk` value to a transcript item's chunk list,
/// merging it into the previous chunk when the two are the same text stream.
///
/// Agents stream text one token at a time, so a long turn arrives as
/// thousands of chunks that differ only in `content.text`. Stored separately
/// they cost far more memory than the text they carry: each chunk is its own
/// pair of nested `serde_json::Value` maps. Merging them keeps one chunk per
/// run of text, which is what every reader of `chunks` already reconstructs.
///
/// Two chunks merge only when nothing but the text differs: both are objects
/// whose `content.type` is `"text"` with a string `content.text`, their
/// `messageId` values are equal (both absent counts as equal), and every
/// other top-level key (such as `meta`) and every other `content` key (such
/// as `annotations`) is identical. Anything else is pushed as its own chunk.
pub fn push_content_chunk(chunks: &mut Vec<serde_json::Value>, chunk: serde_json::Value) {
    if chunks
        .last()
        .is_some_and(|last| text_chunks_mergeable(last, &chunk))
    {
        let addition = chunk
            .get("content")
            .and_then(|content| content.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(serde_json::Value::Object(last)) = chunks.last_mut()
            && let Some(serde_json::Value::Object(content)) = last.get_mut("content")
            && let Some(serde_json::Value::String(text)) = content.get_mut("text")
        {
            text.push_str(&addition);
            return;
        }
    }
    chunks.push(chunk);
}

/// Collapse runs of per-token text chunks that were stored before they were
/// merged on the way in. Rebuilds the list through [`push_content_chunk`], so
/// it applies exactly the same merge rule and leaves chunk boundaries that
/// carry real differences (a new message ID, non-text content, differing
/// metadata) where they are.
pub fn coalesce_content_chunks(chunks: &mut Vec<serde_json::Value>) {
    if chunks.len() < 2 {
        return;
    }
    let mut merged = Vec::with_capacity(chunks.len());
    for chunk in std::mem::take(chunks) {
        push_content_chunk(&mut merged, chunk);
    }
    merged.shrink_to_fit();
    *chunks = merged;
}

/// Whether `next` carries only more text for the same stream as `last`, so
/// the two can share one chunk. The single definition of the merge rule used
/// by both [`push_content_chunk`] and [`coalesce_content_chunks`].
fn text_chunks_mergeable(last: &serde_json::Value, next: &serde_json::Value) -> bool {
    let (serde_json::Value::Object(last), serde_json::Value::Object(next)) = (last, next) else {
        return false;
    };
    let (
        Some(serde_json::Value::Object(last_content)),
        Some(serde_json::Value::Object(next_content)),
    ) = (last.get("content"), next.get("content"))
    else {
        return false;
    };
    let is_text = |content: &serde_json::Map<String, serde_json::Value>| {
        content.get("type").and_then(serde_json::Value::as_str) == Some("text")
            && content
                .get("text")
                .is_some_and(serde_json::Value::is_string)
    };
    if !is_text(last_content) || !is_text(next_content) {
        return false;
    }
    // Every other top-level key, `messageId` and `meta` included, must match.
    if last.len() != next.len()
        || !last
            .iter()
            .all(|(key, value)| key == "content" || next.get(key) == Some(value))
    {
        return false;
    }
    // ... as must every other content key, such as `annotations`.
    last_content.len() == next_content.len()
        && last_content
            .iter()
            .all(|(key, value)| key == "text" || next_content.get(key) == Some(value))
}

/// What one client-run terminal produced, as hel recorded it when the child
/// was reaped. `exit_code` and `signal` mirror ACP `TerminalExitStatus`; both
/// are `None` when the terminal was released before a status was observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOutputRecord {
    pub terminal_id: String,
    pub output: PackedText,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

impl TerminalOutputRecord {
    /// Compute once before comparing one terminal event with the tool window.
    pub fn raw_result_digest(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(self.output.text().as_bytes()).into()
    }

    /// Whether the command ended the way a caller asked for: exit status zero
    /// and no signal. Anything else — a nonzero exit, a signal, or no status at
    /// all because the terminal was released before one was observed — is
    /// abnormal, and stays visible in every render mode.
    pub fn exited_cleanly(&self) -> bool {
        self.exit_code == Some(0) && self.signal.is_none()
    }

    /// Whether a completed ACP tool's provider-specific raw result is this
    /// child result. Kimi reports shell output as a byte array beside its exit
    /// status but omits the ACP terminal reference, so the exact result is the
    /// only ownership information it publishes.
    pub fn matches_tool_raw_result(&self, call: &serde_json::Value) -> bool {
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
                        .zip(self.output.text().as_bytes())
                        .all(|(value, byte)| value.as_u64() == Some(u64::from(*byte)))
            }
            Some(serde_json::Value::String(output)) => output == self.output.text().as_ref(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn is_work_interruption(&self) -> bool {
        self.stable_id.starts_with(WORK_INTERRUPTED_ITEM_PREFIX)
    }

    pub fn is_session_restart(&self) -> bool {
        self.stable_id.starts_with(SESSION_RESTART_ITEM_PREFIX)
    }

    /// The relay ordinal a reader pages by.
    ///
    /// An agent message is rewritten as its content streams in, and its
    /// `latest_content_event_ordinal` is where that stopped, so paging by it
    /// hands a caller the finished message once instead of the partial one it
    /// was created with. Every other body is created once, so its position is
    /// its sequence.
    pub fn seq(&self) -> u64 {
        self.latest_content_event_ordinal.unwrap_or(self.position)
    }

    /// A submitted prompt, excluding replies within an existing turn.
    pub fn is_user_prompt(&self) -> bool {
        matches!(self.body, TranscriptBody::User { .. })
            && !self.stable_id.starts_with(ELICITATION_REPLY_ITEM_PREFIX)
    }

    /// Whether this item begins a turn: a user prompt, or the marker for a
    /// turn the harness started on its own. The recovery boundary and the
    /// scope of a plan update both key on the newest of these.
    pub fn is_turn_start(&self) -> bool {
        self.is_user_prompt() || self.stable_id.starts_with(HARNESS_TURN_ITEM_PREFIX)
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

    pub fn validate(&self, through: u64) -> Result<()> {
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
        if let TranscriptBody::Message { event } = &self.body {
            anyhow::ensure!(
                !event.key.trim().is_empty()
                    && event.message_text().is_some()
                    && self.stable_id == message_item_id(&event.key),
                "invalid message transcript entry {:?}",
                self.stable_id
            );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChatRole {
    User,
    Agent,
    Message,
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
    pub start_seq: u64,
    pub seq: u64,
    pub role: ChatRole,
    pub text: String,
    pub recorded_at_ms: Option<i64>,
    /// When a finished tool call last changed, which is when it ended.
    /// `None` for a call still running and for every other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_ended_at_ms: Option<i64>,
    pub revision: u64,
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_message: Option<Box<crate::mailbox::MailboxEvent>>,
    pub tool_call_id: Option<String>,
    pub tool_status: Option<ToolStatus>,
    /// Compact label used by Rich and browser projections. `text` remains the
    /// original provider title for Raw mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_summary: Option<String>,
    /// Selected bounded source retained so partial ACP updates can preserve a
    /// raw-command-derived summary without retaining arbitrary raw JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_presentation: Option<ToolCallPresentation>,
    /// ACP arguments, kept separately so result updates cannot erase them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    /// A diagnostic reported in ACP `rawOutput.error`, kept separate from
    /// textual tool content so partial result updates can replace it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_error: Option<String>,
    pub tool_content: Vec<String>,
    pub tool_diffstats: Vec<String>,
    pub tool_locations: Vec<String>,
    pub plan: Vec<PlanLine>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub leading_omitted: bool,
    /// Detail the decluttered feed leaves out: the entry renders only in the
    /// raw transcript mode. Set once, when the entry is built, because Alt-T
    /// switches render mode without rebuilding entries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub raw_only: bool,
    /// A completed tool call whose turn was interrupted while it ran. The
    /// harness kept the command going and it ended later on its own, so its
    /// completion is not the agent's finished work.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ended_after_interrupt: bool,
    /// The materialized transcript item this entry was derived from, when it
    /// came from the controller's projection. Provenance only, so it is
    /// neither serialized nor part of the entry's value.
    #[serde(skip)]
    pub source: TranscriptSource,
}

/// Handle on the transcript item an entry was derived from. Unchanged items
/// keep the same `Arc` from one projection to the next, so a pointer
/// comparison replaces re-reading the item and re-parsing its JSON.
///
/// The handle records where an entry came from, not what it says, so two
/// entries with equal content are equal whatever they were derived from.
#[derive(Debug, Clone, Default)]
pub struct TranscriptSource(pub Option<Arc<TranscriptItem>>);

impl TranscriptSource {
    pub fn is(&self, item: &Arc<TranscriptItem>) -> bool {
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
    pub fn received_message(seq: u64, event: &crate::mailbox::MailboxEvent) -> Self {
        let mut entry = Self::plain(
            seq,
            ChatRole::Message,
            event
                .message_text()
                .expect("message entry carries a message event"),
        );
        entry.message_id = Some(message_item_id(&event.key));
        entry.incoming_message = Some(Box::new(event.clone()));
        entry
    }

    /// Whether this entry is the durable marker emitted when a session's
    /// control plane restarts. The source identity is authoritative for
    /// materialized entries; the role/text check also covers entries built
    /// from older worker snapshots that have no materialized source handle.
    pub fn is_session_restart(&self) -> bool {
        self.source
            .0
            .as_ref()
            .is_some_and(|item| item.is_session_restart())
            || (self.role == ChatRole::System && self.text == SESSION_RESTART_TEXT)
    }

    /// Whether this entry is a message the user wrote, including an answer
    /// to an agent's question. Prompts that Mjolnir submits on the user's
    /// behalf are excluded: every [`crate::continuation::GeneratedPrompt`]
    /// and text marked as generated. Authorization evidence asks the same
    /// two predicates, so the two cannot disagree.
    pub fn is_user_authored(&self) -> bool {
        // An entry without a source is a prompt submitted from this client
        // that the controller has not yet projected.
        self.role == ChatRole::User
            && crate::continuation::is_user_authored_message(
                self.source.0.as_ref().map(|item| item.stable_id.as_str()),
                &self.text,
            )
    }

    pub fn plan(seq: u64, plan: Vec<PlanLine>) -> Self {
        Self {
            start_seq: seq,
            seq,
            role: ChatRole::Plan,
            text: String::new(),
            recorded_at_ms: None,
            tool_ended_at_ms: None,
            revision: 0,
            message_id: None,
            incoming_message: None,
            tool_call_id: None,
            tool_status: None,
            tool_summary: None,
            tool_presentation: None,
            tool_input: None,
            tool_error: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan,
            leading_omitted: false,
            raw_only: false,
            ended_after_interrupt: false,
            source: TranscriptSource::default(),
        }
    }

    pub fn touch(&mut self, seq: u64) {
        self.seq = seq;
        self.revision = self.revision.wrapping_add(1);
    }

    /// Bound one entry to the sizes the dashboard's summary tolerates.
    ///
    /// Compiled unconditionally and hidden from the documentation because the
    /// chat crate's tests need it, and a `#[cfg(test)]` item is invisible to
    /// another crate.
    #[doc(hidden)]
    pub fn bounded_for_dashboard(mut self) -> Self {
        self.bound_dashboard_content();
        self
    }

    fn bound_dashboard_content(&mut self) {
        const TEXT_BYTES: usize = 64 * 1024;
        const DETAIL_BYTES: usize = 2 * 1024;
        const DETAIL_COUNT: usize = 8;

        self.leading_omitted |= truncate_string_start(&mut self.text, TEXT_BYTES);
        // Dashboard summaries do not expand tool arguments.
        self.tool_input = None;
        if let Some(error) = &mut self.tool_error {
            truncate_string_start(error, DETAIL_BYTES);
        }
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
        if let Some(summary) = &mut self.tool_summary {
            truncate_string_start(summary, DETAIL_BYTES);
        }
        if let Some(presentation) = &mut self.tool_presentation {
            truncate_string_start(&mut presentation.summary, DETAIL_BYTES);
            let mut source = presentation.source.text().into_owned();
            truncate_string_start(&mut source, TEXT_BYTES);
            presentation.source = source.into();
        }
        self.plan.truncate(DETAIL_COUNT);
        for line in &mut self.plan {
            truncate_string_start(&mut line.text, DETAIL_BYTES);
        }
    }

    pub fn with_recorded_at(mut self, recorded_at_ms: Option<i64>) -> Self {
        self.recorded_at_ms = recorded_at_ms;
        self
    }
}

/// Constructors that sanitize the text they are given, so terminal escape
/// sequences from a harness never reach a transcript entry.
impl ChatEntry {
    pub fn plain(seq: u64, role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            start_seq: seq,
            seq,
            role,
            text: sanitize_terminal_text(&text.into()),
            recorded_at_ms: None,
            tool_ended_at_ms: None,
            revision: 0,
            message_id: None,
            incoming_message: None,
            tool_call_id: None,
            tool_status: None,
            tool_summary: None,
            tool_presentation: None,
            tool_input: None,
            tool_error: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan: Vec::new(),
            leading_omitted: false,
            raw_only: false,
            ended_after_interrupt: false,
            source: TranscriptSource::default(),
        }
    }

    pub fn tool(
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
            tool_ended_at_ms: None,
            revision: 0,
            message_id: None,
            incoming_message: None,
            tool_call_id,
            tool_status: Some(tool_status),
            tool_summary: None,
            tool_presentation: None,
            tool_input: None,
            tool_error: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan: Vec::new(),
            leading_omitted: false,
            raw_only: false,
            ended_after_interrupt: false,
            source: TranscriptSource::default(),
        }
    }
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

pub fn plan_status(status: &PlanEntryStatus) -> PlanStatus {
    match status {
        PlanEntryStatus::InProgress => PlanStatus::Running,
        PlanEntryStatus::Completed => PlanStatus::Completed,
        _ => PlanStatus::Pending,
    }
}

/// Remove terminal controls while preserving user-visible whitespace.
pub fn sanitize_terminal_text(text: &str) -> String {
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
    crate::relay::strip_hidden_prompt_context(&text).to_owned()
}

/// What produced a transcript item, as a stable wire name.
///
/// The chat view has its own role enum shaped around how it renders; this is
/// the name the HTTP API publishes, so it changes only when the transcript
/// model does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRole {
    User,
    Agent,
    Message,
    Thought,
    Tool,
    Terminal,
    Plan,
    PlanProposal,
    System,
}

impl TranscriptRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Message => "message",
            Self::Thought => "thought",
            Self::Tool => "tool",
            Self::Terminal => "terminal",
            Self::Plan => "plan",
            Self::PlanProposal => "plan_proposal",
            Self::System => "system",
        }
    }
    pub fn storage_kind(self) -> &'static str {
        match self {
            Self::Terminal => "terminal_output",
            other => other.as_str(),
        }
    }
}

pub fn transcript_item_role(body: &TranscriptBody) -> &'static str {
    let role = match body {
        TranscriptBody::User { .. } => TranscriptRole::User,
        TranscriptBody::Agent { .. } => TranscriptRole::Agent,
        TranscriptBody::Message { .. } => TranscriptRole::Message,
        TranscriptBody::Thought { .. } => TranscriptRole::Thought,
        TranscriptBody::Tool { .. } => TranscriptRole::Tool,
        TranscriptBody::TerminalOutput { .. } => TranscriptRole::Terminal,
        TranscriptBody::Plan { .. } => TranscriptRole::Plan,
        TranscriptBody::PlanProposal { .. } => TranscriptRole::PlanProposal,
        TranscriptBody::System { .. } => TranscriptRole::System,
    };
    role.as_str()
}

pub fn message_item_id(event_key: &str) -> String {
    format!("message:{event_key}")
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

pub fn tool_status(status: &ToolCallStatus) -> ToolStatus {
    match status {
        ToolCallStatus::InProgress => ToolStatus::Running,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

pub fn content_block_text(content: &ContentBlock) -> Option<String> {
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

pub fn truncate_string_start(value: &mut String, maximum_bytes: usize) -> bool {
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
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PlanStatus {
    Pending,
    Running,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanLine {
    pub text: String,
    pub status: PlanStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_chunk(text: &str, message_id: Option<&str>) -> serde_json::Value {
        match message_id {
            Some(id) => json!({"content": {"type": "text", "text": text}, "messageId": id}),
            None => json!({"content": {"type": "text", "text": text}}),
        }
    }

    fn sourced_user_entry(stable_id: &str, text: &str) -> ChatEntry {
        let mut entry = ChatEntry::plain(1, ChatRole::User, text);
        entry.source = TranscriptSource(Some(Arc::new(TranscriptItem {
            stable_id: stable_id.into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: TranscriptBody::User {
                content: vec![json!({"type": "text", "text": text})],
            },
        })));
        entry
    }

    #[test]
    fn only_messages_the_user_wrote_are_user_authored() {
        for (stable_id, text) in [
            ("user:prompt-7", "fix the build"),
            ("user:2", "a prompt recovered by position"),
            ("elicitation-reply:9", "Yes"),
        ] {
            assert!(
                sourced_user_entry(stable_id, text).is_user_authored(),
                "{stable_id}"
            );
        }
        assert!(ChatEntry::plain(1, ChatRole::User, "not yet projected").is_user_authored());
        for (stable_id, text) in [
            ("user:auto-continue-1-1", "Continue."),
            ("user:quota-retry-4", "Continue."),
            ("user:server-retry-5", "Continue."),
            ("user:review-forward-2", "Review findings"),
            // Hard-won: 8b08e8bc1: handback reminders and peer-session messages counted as user authorization
            ("user:handback-reminder-36", "Please also update the docs"),
            ("user:subagent-input-3", "A sub-agent finished."),
            (
                "user:session-message-turn-8",
                "Message from another session",
            ),
            ("user:prompt-9", "[HARNESS NOTE: summarize the request]"),
            (
                "user:prompt-10",
                crate::continuation::PARENT_WAIT_PROMPT_TEXT,
            ),
        ] {
            assert!(
                !sourced_user_entry(stable_id, text).is_user_authored(),
                "{stable_id}"
            );
        }
        assert!(!ChatEntry::plain(1, ChatRole::Agent, "a reply").is_user_authored());
    }

    // Hard-won: ff088d42: A 15,000-chunk thought made daemon RSS grow from gigabytes of stored JSON into many gigabytes.
    #[test]
    fn push_content_chunk_merges_adjacent_text_for_the_same_message_id() {
        let mut chunks = vec![text_chunk("x", Some("m1"))];
        for _ in 1..15_001 {
            push_content_chunk(&mut chunks, text_chunk("x", Some("m1")));
        }
        assert_eq!(chunks, vec![text_chunk(&"x".repeat(15_001), Some("m1"))]);
    }

    #[test]
    fn push_content_chunk_keeps_chunks_from_different_message_ids_apart() {
        let image = json!({"content": {"type": "image", "data": "abc", "mimeType": "image/png"}});
        let mut chunks = vec![text_chunk("first", Some("m1"))];
        push_content_chunk(&mut chunks, text_chunk(" more", Some("m1")));
        push_content_chunk(&mut chunks, text_chunk("second", Some("m2")));
        push_content_chunk(&mut chunks, text_chunk("third", None));
        push_content_chunk(&mut chunks, text_chunk(" more", None));
        push_content_chunk(&mut chunks, image.clone());
        push_content_chunk(&mut chunks, image.clone());
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "a"}, "meta": {"source": "one"}}),
        );
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "b"}, "meta": {"source": "two"}}),
        );
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "c"}, "meta": {"source": "two"}}),
        );
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "annotated", "annotations": {"audience": ["user"]}}}),
        );
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "plain"}}),
        );
        assert_eq!(
            chunks,
            vec![
                text_chunk("first more", Some("m1")),
                text_chunk("second", Some("m2")),
                text_chunk("third more", None),
                image.clone(),
                image,
                json!({"content": {"type": "text", "text": "a"}, "meta": {"source": "one"}}),
                json!({"content": {"type": "text", "text": "bc"}, "meta": {"source": "two"}}),
                json!({"content": {"type": "text", "text": "annotated", "annotations": {"audience": ["user"]}}}),
                json!({"content": {"type": "text", "text": "plain"}}),
            ]
        );
    }

    // Hard-won: ff088d42: Stored 15,000-chunk transcript rows ballooned daemon RSS after restart.
    #[test]
    fn coalesce_content_chunks_collapses_runs_and_keeps_segment_boundaries() {
        let image = json!({"content": {"type": "image", "data": "x", "mimeType": "image/png"}});
        let mut chunks = (0..15_001)
            .map(|_| text_chunk("x", Some("m1")))
            .collect::<Vec<_>>();
        chunks.extend([
            text_chunk("next", Some("m2")),
            text_chunk(" turn", Some("m2")),
            image.clone(),
        ]);
        coalesce_content_chunks(&mut chunks);
        assert_eq!(
            chunks,
            vec![
                text_chunk(&"x".repeat(15_001), Some("m1")),
                text_chunk("next turn", Some("m2")),
                image
            ]
        );
    }
}
