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
    ContentBlock, ContentChunk, EmbeddedResourceResource, PlanEntryStatus, ToolCallStatus, ToolKind,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const SESSION_RESTART_TEXT: &str = "[session restarted]";
pub const SESSION_RESTART_ITEM_PREFIX: &str = "system:session-restarted:";
/// Marks the point where the harness resumed work with no prompt in flight.
pub const HARNESS_TURN_TEXT: &str = "Agent continued on its own";
pub const HARNESS_TURN_ITEM_PREFIX: &str = "harness-turn:";

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
    pub source: String,
    pub source_kind: ToolSummarySourceKind,
    pub tool_kind: ToolKind,
    /// Version of the parser rules that produced `summary`. A missing value
    /// identifies presentation metadata written before parser versioning.
    #[serde(default)]
    pub summary_version: u8,
}

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
            && content.get("text").is_some_and(serde_json::Value::is_string)
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
    pub revision: u64,
    pub message_id: Option<String>,
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

    pub fn plan(seq: u64, plan: Vec<PlanLine>) -> Self {
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
            tool_summary: None,
            tool_presentation: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan,
            leading_omitted: false,
            raw_only: false,
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
            truncate_string_start(&mut presentation.source, TEXT_BYTES);
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
            revision: 0,
            message_id: None,
            tool_call_id: None,
            tool_status: None,
            tool_summary: None,
            tool_presentation: None,
            tool_content: Vec::new(),
            tool_diffstats: Vec::new(),
            tool_locations: Vec::new(),
            plan: Vec::new(),
            leading_omitted: false,
            raw_only: false,
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
            revision: 0,
            message_id: None,
            tool_call_id,
            tool_status: Some(tool_status),
            tool_summary: None,
            tool_presentation: None,
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
        TranscriptBody::Thought { .. } => TranscriptRole::Thought,
        TranscriptBody::Tool { .. } => TranscriptRole::Tool,
        TranscriptBody::TerminalOutput { .. } => TranscriptRole::Terminal,
        TranscriptBody::Plan { .. } => TranscriptRole::Plan,
        TranscriptBody::PlanProposal { .. } => TranscriptRole::PlanProposal,
        TranscriptBody::System { .. } => TranscriptRole::System,
    };
    role.as_str()
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

    #[test]
    fn push_content_chunk_merges_adjacent_text_for_the_same_message_id() {
        let mut chunks = vec![text_chunk("The", Some("m1"))];
        push_content_chunk(&mut chunks, text_chunk(" quick", Some("m1")));
        push_content_chunk(&mut chunks, text_chunk(" fox", Some("m1")));
        assert_eq!(chunks, vec![text_chunk("The quick fox", Some("m1"))]);
    }

    #[test]
    fn push_content_chunk_merges_adjacent_text_without_message_ids() {
        let mut chunks = vec![text_chunk("one", None)];
        push_content_chunk(&mut chunks, text_chunk(" two", None));
        assert_eq!(chunks, vec![text_chunk("one two", None)]);
    }

    #[test]
    fn push_content_chunk_keeps_chunks_from_different_message_ids_apart() {
        let mut chunks = vec![text_chunk("first", Some("m1"))];
        push_content_chunk(&mut chunks, text_chunk("second", Some("m2")));
        push_content_chunk(&mut chunks, text_chunk("third", None));
        assert_eq!(
            chunks,
            vec![
                text_chunk("first", Some("m1")),
                text_chunk("second", Some("m2")),
                text_chunk("third", None),
            ]
        );
    }

    #[test]
    fn push_content_chunk_keeps_non_text_content_separate() {
        let image = json!({"content": {"type": "image", "data": "abc", "mimeType": "image/png"}});
        let mut chunks = vec![text_chunk("before", None)];
        push_content_chunk(&mut chunks, image.clone());
        push_content_chunk(&mut chunks, image.clone());
        push_content_chunk(&mut chunks, text_chunk("after", None));
        assert_eq!(
            chunks,
            vec![
                text_chunk("before", None),
                image.clone(),
                image,
                text_chunk("after", None),
            ]
        );
    }

    #[test]
    fn push_content_chunk_keeps_chunks_with_differing_metadata_apart() {
        let mut chunks =
            vec![json!({"content": {"type": "text", "text": "a"}, "meta": {"source": "one"}})];
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "b"}, "meta": {"source": "two"}}),
        );
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "c"}, "meta": {"source": "two"}}),
        );
        assert_eq!(
            chunks,
            vec![
                json!({"content": {"type": "text", "text": "a"}, "meta": {"source": "one"}}),
                json!({"content": {"type": "text", "text": "bc"}, "meta": {"source": "two"}}),
            ]
        );
    }

    #[test]
    fn push_content_chunk_keeps_chunks_with_differing_annotations_apart() {
        let mut chunks = vec![
            json!({"content": {"type": "text", "text": "a", "annotations": {"audience": ["user"]}}}),
        ];
        push_content_chunk(
            &mut chunks,
            json!({"content": {"type": "text", "text": "b"}}),
        );
        assert_eq!(
            chunks,
            vec![
                json!({"content": {"type": "text", "text": "a", "annotations": {"audience": ["user"]}}}),
                json!({"content": {"type": "text", "text": "b"}}),
            ]
        );
    }

    #[test]
    fn coalesce_content_chunks_collapses_runs_and_keeps_segment_boundaries() {
        let mut chunks = vec![
            text_chunk("He", Some("m1")),
            text_chunk("llo", Some("m1")),
            text_chunk("!", Some("m1")),
            text_chunk("next", Some("m2")),
            text_chunk(" turn", Some("m2")),
        ];
        coalesce_content_chunks(&mut chunks);
        assert_eq!(
            chunks,
            vec![
                text_chunk("Hello!", Some("m1")),
                text_chunk("next turn", Some("m2")),
            ]
        );
    }

    #[test]
    fn coalesce_content_chunks_leaves_unmergeable_chunks_alone() {
        let original = vec![
            text_chunk("a", Some("m1")),
            text_chunk("b", Some("m2")),
            json!({"content": {"type": "image", "data": "x", "mimeType": "image/png"}}),
        ];
        let mut chunks = original.clone();
        coalesce_content_chunks(&mut chunks);
        assert_eq!(chunks, original);
    }
}
