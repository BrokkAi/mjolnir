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
    ToolCallContent, ToolCallLocation, ToolCallStatus, ToolCallUpdateFields, ToolKind,
};
#[cfg(feature = "controller")]
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tree_sitter::{Node, Parser};

use crate::hel_acp::RuntimeEvent;

pub const SESSION_RESTART_TEXT: &str = "[session restarted]";
pub const SESSION_RESTART_ITEM_PREFIX: &str = "system:session-restarted:";
/// Marks the point where the harness resumed work with no prompt in flight.
pub const HARNESS_TURN_TEXT: &str = "Agent continued on its own";
pub const HARNESS_TURN_ITEM_PREFIX: &str = "harness-turn:";

const TOOL_SUMMARY_SOURCE_BYTES: usize = 64 * 1024;
/// Parser-rule version stored with cached tool summaries.
pub const TOOL_SUMMARY_VERSION: u8 = 1;

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

    #[cfg(feature = "controller")]
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
        ..
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolSummarySource {
    Shell(String),
    Argv {
        executable: String,
        arguments: Vec<String>,
    },
}

/// Whether a partial update changes inputs used to derive the tool summary.
pub fn tool_call_update_changes_presentation(
    call: &ToolCall,
    fields: &ToolCallUpdateFields,
) -> bool {
    fields
        .title
        .as_ref()
        .is_some_and(|title| title != &call.title)
        || fields.kind.is_some_and(|kind| kind != call.kind)
        || fields
            .raw_input
            .as_ref()
            .is_some_and(|input| Some(input) != call.raw_input.as_ref())
        || (call.kind == ToolKind::Execute
            && fields
                .raw_output
                .as_ref()
                .is_some_and(|output| Some(output) != call.raw_output.as_ref())
            && command_source(call.raw_input.as_ref()).is_none())
}

/// Compute the stable presentation metadata for one complete ACP call.
pub fn tool_call_presentation(call: &ToolCall) -> ToolCallPresentation {
    let kind = call.kind;
    if kind == ToolKind::Execute {
        if let Some(source) = command_source(call.raw_input.as_ref()) {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawInput,
                kind,
                &call.title,
            );
        }
        if let Some(source) = command_source(call.raw_output.as_ref()) {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawOutput,
                kind,
                &call.title,
            );
        }
    }
    presentation_from_title(&call.title, kind)
}

/// Use cached presentation data when it was produced by current parser rules,
/// otherwise rebuild it from the complete stored call. This lets parser fixes
/// repair existing transcripts while current summaries remain cheap to load.
pub fn materialized_tool_call_presentation(
    stored: Option<&ToolCallPresentation>,
    call: &ToolCall,
) -> ToolCallPresentation {
    stored
        .filter(|presentation| presentation.summary_version >= TOOL_SUMMARY_VERSION)
        .cloned()
        .unwrap_or_else(|| tool_call_presentation(call))
}

/// Apply the presentation-relevant portion of a partial ACP update to cached
/// metadata. ACP updates replace only fields that are present, so a title
/// update must not erase a summary selected from an earlier raw command.
pub fn update_tool_call_presentation(
    previous: Option<&ToolCallPresentation>,
    title: &str,
    kind: Option<ToolKind>,
    raw_input: Option<&Value>,
    raw_output: Option<&Value>,
) -> ToolCallPresentation {
    let next_kind = kind.unwrap_or_else(|| {
        previous
            .map(|presentation| presentation.tool_kind)
            .unwrap_or_default()
    });

    if next_kind == ToolKind::Execute {
        if let Some(source) = raw_input.and_then(|value| command_source(Some(value))) {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawInput,
                next_kind,
                title,
            );
        }
        if let Some(source) = raw_output.and_then(|value| command_source(Some(value)))
            && (raw_input.is_some()
                || !previous.is_some_and(|previous| {
                    previous.source_kind == ToolSummarySourceKind::RawInput
                }))
        {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawOutput,
                next_kind,
                title,
            );
        }
        if let Some(previous) = previous
            && previous.tool_kind == ToolKind::Execute
            && ((raw_input.is_none() && previous.source_kind == ToolSummarySourceKind::RawInput)
                || (raw_input.is_none()
                    && raw_output.is_none()
                    && previous.source_kind == ToolSummarySourceKind::RawOutput))
        {
            return ToolCallPresentation {
                tool_kind: next_kind,
                ..previous.clone()
            };
        }
    }

    presentation_from_title(title, next_kind)
}

fn command_source(raw: Option<&Value>) -> Option<ToolSummarySource> {
    let command = raw?.get("command")?;
    match command {
        Value::String(command) if !command.trim().is_empty() => {
            Some(ToolSummarySource::Shell(command.clone()))
        }
        Value::Array(argv) => {
            let argv = argv.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
            let first = argv.first()?.trim();
            if first.is_empty() {
                return None;
            }
            if is_shell_interpreter(first)
                && let Some(script) = shell_script_argument(&argv[1..])
            {
                return Some(ToolSummarySource::Shell(script.to_owned()));
            }
            Some(ToolSummarySource::Argv {
                executable: first.to_owned(),
                arguments: argv[1..]
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            })
        }
        _ => None,
    }
}

fn is_shell_interpreter(value: &str) -> bool {
    let executable = value.rsplit('/').next().unwrap_or(value);
    matches!(executable, "sh" | "bash" | "dash" | "zsh")
}

fn shell_script_argument<'a>(arguments: &'a [&'a str]) -> Option<&'a str> {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index];
        if argument == "--" {
            return None;
        }
        if argument == "-c" || argument == "--command" {
            return arguments.get(index + 1).copied();
        }
        if argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('c') {
            return arguments.get(index + 1).copied();
        }
        index += 1;
    }
    None
}

fn presentation_from_source(
    source: ToolSummarySource,
    source_kind: ToolSummarySourceKind,
    tool_kind: ToolKind,
    title: &str,
) -> ToolCallPresentation {
    let (source, summary) = match source {
        ToolSummarySource::Shell(source) => {
            let bounded = bound_summary_source(&source);
            let summary = summarize_shell(&bounded)
                .or_else(|| first_meaningful_token(title))
                .unwrap_or_else(|| "tool".to_owned());
            (bounded, summary)
        }
        ToolSummarySource::Argv {
            executable,
            arguments,
        } => {
            let source = std::iter::once(executable.as_str())
                .chain(arguments.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let bounded = bound_summary_source(&source);
            let summary = summarize_invocation(&executable, &arguments)
                .or_else(|| first_meaningful_token(title))
                .unwrap_or_else(|| "tool".to_owned());
            (bounded, summary)
        }
    };
    ToolCallPresentation {
        summary,
        source,
        source_kind,
        tool_kind,
        summary_version: TOOL_SUMMARY_VERSION,
    }
}

fn presentation_from_title(title: &str, tool_kind: ToolKind) -> ToolCallPresentation {
    let source = title_source(title);
    let bounded = bound_summary_source(&source);
    let summary = if tool_kind == ToolKind::Execute {
        summarize_shell(&bounded)
            .or_else(|| first_meaningful_token(&bounded))
            .unwrap_or_else(|| "tool".to_owned())
    } else {
        first_meaningful_token(&bounded).unwrap_or_else(|| "tool".to_owned())
    };
    ToolCallPresentation {
        summary,
        source: bounded,
        source_kind: ToolSummarySourceKind::Title,
        tool_kind,
        summary_version: TOOL_SUMMARY_VERSION,
    }
}

fn title_source(title: &str) -> String {
    let title = title.trim();
    let title = title
        .strip_prefix("Running:")
        .or_else(|| title.strip_prefix("Starting background:"))
        .map(str::trim)
        .unwrap_or(title);
    if let Some(inner) = title
        .strip_prefix("Execute `")
        .and_then(|value| value.strip_suffix('`'))
    {
        return inner.to_owned();
    }
    title.to_owned()
}

fn first_meaningful_token(value: &str) -> Option<String> {
    let token = value
        .split_whitespace()
        .next()?
        .trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '/' && character != '.' && character != '_'
        });
    if token.is_empty() {
        None
    } else {
        Some(token.trim_matches(['\'', '"', '`']).to_owned())
    }
}

fn bound_summary_source(source: &str) -> String {
    if source.len() <= TOOL_SUMMARY_SOURCE_BYTES {
        return source.to_owned();
    }
    let mut end = TOOL_SUMMARY_SOURCE_BYTES;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    source[..end].to_owned()
}

fn summarize_shell(source: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let mut commands = Vec::new();
    let mut operators = Vec::new();
    let mut subshells = Vec::new();
    if !collect_shell_tokens(root, source, &mut commands, &mut operators, &mut subshells) {
        return None;
    }
    if commands.is_empty() {
        return None;
    }
    commands.sort_by_key(|command| command.start);
    operators.sort_by_key(|operator| operator.start);
    subshells.sort_by_key(|subshell| subshell.start);

    let mut tokens = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        if index > 0 {
            let previous = &commands[index - 1];
            let separator = shell_separator_between(previous, command, &operators);
            tokens.push(ShellToken {
                start: separator.start,
                text: separator.kind,
                order: 1,
            });
        }
        tokens.push(ShellToken {
            start: command.start,
            text: command.summary.clone(),
            order: 2,
        });
    }

    // Parentheses are meaningful only for subshells that contain a command we
    // retained. Other punctuation, such as case arms and group delimiters,
    // is structural and must not leak into the compact summary.
    for subshell in subshells {
        if !commands
            .iter()
            .any(|command| command.start >= subshell.start && command.end <= subshell.end)
        {
            continue;
        }
        let close = subshell.end.saturating_sub(1);
        tokens.push(ShellToken {
            start: subshell.start,
            text: "(".to_owned(),
            order: 0,
        });
        tokens.push(ShellToken {
            start: close,
            text: ")".to_owned(),
            order: 3,
        });
    }

    tokens.sort_by_key(|token| (token.start, token.order));
    Some(join_shell_tokens(
        tokens.into_iter().map(|token| token.text).collect(),
    ))
}

#[derive(Debug, Clone)]
struct ShellCommandToken {
    start: usize,
    end: usize,
    summary: String,
}

#[derive(Debug, Clone)]
struct ShellOperatorToken {
    start: usize,
    kind: String,
}

#[derive(Debug, Clone)]
struct ShellSubshell {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct ShellToken {
    start: usize,
    text: String,
    order: u8,
}

fn collect_shell_tokens(
    node: Node<'_>,
    source: &str,
    commands: &mut Vec<ShellCommandToken>,
    operators: &mut Vec<ShellOperatorToken>,
    subshells: &mut Vec<ShellSubshell>,
) -> bool {
    let kind = node.kind();
    if matches!(kind, "command_substitution" | "process_substitution") {
        return true;
    }
    if kind == "command" {
        if let Some(summary) = summarize_command_node(node, source) {
            commands.push(ShellCommandToken {
                start: node.start_byte(),
                end: node.end_byte(),
                summary,
            });
            return true;
        }
        return false;
    }
    if is_shell_operator(node) {
        operators.push(ShellOperatorToken {
            start: node.start_byte(),
            kind: kind.to_owned(),
        });
        return true;
    }

    if kind == "subshell" {
        subshells.push(ShellSubshell {
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }

    let mut cursor = node.walk();
    node.children(&mut cursor)
        .all(|child| collect_shell_tokens(child, source, commands, operators, subshells))
}

fn shell_separator_between(
    previous: &ShellCommandToken,
    next: &ShellCommandToken,
    operators: &[ShellOperatorToken],
) -> ShellOperatorToken {
    let mut candidates = operators
        .iter()
        .filter(|operator| operator.start >= previous.end && operator.start < next.start);
    let structural = candidates.clone().find(|operator| operator.kind != ";");
    if let Some(operator) = structural {
        return operator.clone();
    }
    if let Some(operator) = candidates.find(|operator| operator.kind == ";") {
        return operator.clone();
    }
    ShellOperatorToken {
        start: previous.end,
        kind: ";".to_owned(),
    }
}

#[derive(Debug, Clone)]
struct InvocationArgument {
    value: String,
    literal: bool,
}

fn summarize_command_node(node: Node<'_>, source: &str) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let executable = shell_command_name(name, source)?;
    let mut cursor = node.walk();
    let arguments = node
        .children_by_field_name("argument", &mut cursor)
        .map(|argument| {
            let value = shell_argument_value(argument, source);
            InvocationArgument {
                value: value.clone().unwrap_or_default(),
                literal: value.is_some(),
            }
        })
        .collect::<Vec<_>>();
    summarize_invocation_with_literals(&executable, &arguments)
}

fn shell_command_name(node: Node<'_>, source: &str) -> Option<String> {
    if contains_dynamic_shell_node(node) {
        return None;
    }
    normalize_command_name(&source[node.byte_range()])
}

fn shell_argument_value(node: Node<'_>, source: &str) -> Option<String> {
    if contains_dynamic_shell_node(node) {
        return None;
    }
    let text = source[node.byte_range()].trim();
    if text.is_empty() {
        return None;
    }
    Some(strip_matching_quotes(text).to_owned())
}

fn contains_dynamic_shell_node(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "expansion"
            | "simple_expansion"
            | "command_substitution"
            | "process_substitution"
            | "arithmetic_expansion"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(contains_dynamic_shell_node)
}

fn summarize_invocation(executable: &str, arguments: &[String]) -> Option<String> {
    summarize_invocation_with_literals(
        executable,
        &arguments
            .iter()
            .map(|value| InvocationArgument {
                value: strip_matching_quotes(value).to_owned(),
                literal: true,
            })
            .collect::<Vec<_>>(),
    )
}

fn summarize_invocation_with_literals(
    executable: &str,
    arguments: &[InvocationArgument],
) -> Option<String> {
    let executable = normalize_command_name(executable)?;
    let basename = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&executable)
        .to_owned();
    let mut words = vec![executable];
    if !is_summary_executable(&basename) {
        return Some(words.remove(0));
    }
    let mut index = 0;

    if basename == "cargo"
        && arguments.first().is_some_and(|argument| {
            argument.literal && argument.value.starts_with('+') && argument.value.len() > 1
        })
    {
        index += 1;
    }

    let first_verb = loop {
        let Some(argument) = arguments.get(index) else {
            return Some(words.remove(0));
        };
        if !argument.literal {
            return Some(words.remove(0));
        }
        if argument.value == "--" || argument.value.starts_with('-') {
            if let Some(consumed) = known_leading_option_arguments(&basename, arguments, index) {
                index += consumed;
                continue;
            }
            return Some(words.remove(0));
        }
        break argument.value.clone();
    };
    words.push(first_verb.clone());

    if allows_second_verb(&basename, &first_verb) {
        let first = index + 1;
        if let Some(argument) = arguments.get(first)
            && argument.literal
            && !argument.value.starts_with('-')
            && argument.value != "--"
        {
            words.push(argument.value.clone());
        }
    }
    Some(words.join(" "))
}

fn is_summary_executable(basename: &str) -> bool {
    matches!(
        basename,
        "git"
            | "gh"
            | "cargo"
            | "rustup"
            | "npm"
            | "pnpm"
            | "yarn"
            | "bun"
            | "uv"
            | "pip"
            | "pip3"
            | "docker"
            | "podman"
            | "nice"
    )
}

fn allows_second_verb(basename: &str, first_verb: &str) -> bool {
    match basename {
        "gh" => matches!(
            first_verb,
            "alias"
                | "auth"
                | "cache"
                | "codespace"
                | "config"
                | "extension"
                | "gist"
                | "gpg-key"
                | "issue"
                | "label"
                | "org"
                | "pr"
                | "project"
                | "release"
                | "repo"
                | "ruleset"
                | "run"
                | "search"
                | "secret"
                | "ssh-key"
                | "variable"
                | "workflow"
        ),
        "docker" => matches!(
            first_verb,
            "buildx"
                | "compose"
                | "config"
                | "context"
                | "container"
                | "image"
                | "manifest"
                | "network"
                | "node"
                | "plugin"
                | "secret"
                | "service"
                | "stack"
                | "swarm"
                | "system"
                | "trust"
                | "volume"
        ),
        "podman" => matches!(
            first_verb,
            "artifact"
                | "container"
                | "farm"
                | "generate"
                | "image"
                | "machine"
                | "manifest"
                | "network"
                | "play"
                | "pod"
                | "secret"
                | "system"
                | "volume"
        ),
        "uv" => matches!(first_verb, "cache" | "pip" | "python" | "tool"),
        "rustup" => matches!(
            first_verb,
            "component" | "override" | "target" | "toolchain"
        ),
        _ => false,
    }
}

fn known_leading_option_arguments(
    basename: &str,
    arguments: &[InvocationArgument],
    index: usize,
) -> Option<usize> {
    let option = arguments.get(index)?.value.as_str();
    if option == "--" {
        if basename == "nice" {
            return Some(1);
        }
        return None;
    }
    let (option_name, attached_value) = option
        .split_once('=')
        .map_or((option, false), |(name, _)| (name, true));
    if basename == "nice"
        && option.starts_with('-')
        && option.len() > 1
        && option[1..].parse::<i32>().is_ok()
    {
        return Some(1);
    }
    if basename == "nice"
        && option
            .strip_prefix("-n")
            .is_some_and(|value| !value.is_empty() && value.parse::<i32>().is_ok())
    {
        return Some(1);
    }
    let attached_short_value = match basename {
        "git" => option.starts_with("-C") || option.starts_with("-c"),
        "gh" => option.starts_with("-R"),
        "docker" | "podman" => option.starts_with("-H"),
        _ => false,
    } && option.len() > 2;
    let takes_value = match basename {
        "git" => matches!(
            option_name,
            "-C" | "-c"
                | "--config-env"
                | "--exec-path"
                | "--git-dir"
                | "--namespace"
                | "--super-prefix"
                | "--work-tree"
        ),
        "gh" => matches!(
            option_name,
            "-R" | "--hostname" | "--repo" | "--jq" | "--template"
        ),
        "cargo" => matches!(
            option_name,
            "--manifest-path" | "--target-dir" | "--config" | "--color"
        ),
        "npm" | "pnpm" | "yarn" | "bun" => {
            matches!(
                option_name,
                "--cwd" | "--dir" | "--prefix" | "--registry" | "--userconfig"
            )
        }
        "uv" => matches!(option_name, "--directory" | "--project" | "--python"),
        "rustup" => matches!(option_name, "--toolchain"),
        "nice" => {
            matches!(option_name, "-n" | "--adjustment")
        }
        "docker" | "podman" => matches!(
            option_name,
            "-H" | "--config" | "--connection" | "--context" | "--host" | "--log-level"
        ),
        _ => false,
    };
    if attached_short_value {
        return Some(1);
    }
    if attached_value {
        return takes_value.then_some(1);
    }
    if takes_value {
        return arguments
            .get(index + 1)
            .filter(|argument| argument.literal)
            .map(|_| 2);
    }
    let known_flag = match basename {
        "git" => matches!(
            option_name,
            "-p" | "--paginate"
                | "-P"
                | "--no-pager"
                | "--bare"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
                | "--no-optional-locks"
                | "--no-advice"
        ),
        "gh" => false,
        "cargo" => matches!(
            option_name,
            "-q" | "--quiet" | "-v" | "--verbose" | "--locked" | "--offline" | "--frozen"
        ),
        "npm" | "pnpm" | "yarn" | "bun" => {
            matches!(option_name, "-g" | "--global" | "--silent")
        }
        "uv" => matches!(
            option_name,
            "-q" | "--quiet" | "-v" | "--verbose" | "--offline"
        ),
        "rustup" => matches!(option_name, "-q" | "--quiet" | "-v" | "--verbose"),
        "docker" | "podman" => matches!(option_name, "-D" | "--debug" | "--tls"),
        "nice" => false,
        _ => false,
    };
    known_flag.then_some(1)
}

fn strip_matching_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn is_shell_operator(node: Node<'_>) -> bool {
    match node.kind() {
        ";" => true,
        "&&" | "||" => node.parent().is_some_and(|parent| parent.kind() == "list"),
        "|" | "|&" => node
            .parent()
            .is_some_and(|parent| parent.kind() == "pipeline"),
        "&" => node.parent().is_none_or(|parent| {
            !matches!(
                parent.kind(),
                "binary_expression" | "unary_expression" | "postfix_expression"
            )
        }),
        _ => false,
    }
}

fn normalize_command_name(text: &str) -> Option<String> {
    let text = text.trim();
    if text.contains('$') || text.contains('`') {
        return None;
    }
    let text = text
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(text);
    (!text.is_empty()).then(|| text.to_owned())
}

fn join_shell_tokens(tokens: Vec<String>) -> String {
    let mut output = String::new();
    for token in tokens {
        match token.as_str() {
            "(" => {
                if !output.is_empty() && !output.ends_with(' ') {
                    output.push(' ');
                }
                output.push('(');
            }
            ")" => {
                output = output.trim_end().to_owned();
                output.push(')');
            }
            _ => {
                if !output.is_empty() && !output.ends_with(' ') && !output.ends_with('(') {
                    output.push(' ');
                }
                output.push_str(&token);
            }
        }
    }
    output
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

pub fn tool_content_details(
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
pub fn terminal_output_detail(record: &TerminalOutputRecord) -> String {
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

pub fn tool_diff_paths(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(diff.path.display().to_string()),
            _ => None,
        })
        .collect()
}

pub fn tool_location_details(locations: &[ToolCallLocation]) -> Vec<String> {
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
pub fn apply_session_update_to_entries(
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
            let presentation = tool_call_presentation(&call);
            let mut entry = ChatEntry::tool(
                seq,
                call.title,
                Some(call.tool_call_id.to_string()),
                tool_status(&call.status),
            );
            entry.tool_summary = Some(presentation.summary.clone());
            entry.tool_presentation = Some(presentation);
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
            let kind = update.fields.kind;
            let raw_input = update.fields.raw_input.clone();
            let raw_output = update.fields.raw_output.clone();
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
            let presentation = update_tool_call_presentation(
                entry.tool_presentation.as_ref(),
                &entry.text,
                kind,
                raw_input.as_ref(),
                raw_output.as_ref(),
            );
            entry.tool_summary = Some(presentation.summary.clone());
            entry.tool_presentation = Some(presentation);
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
pub fn apply_runtime_event_to_entries(
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
    crate::hel_worker::strip_hidden_prompt_context(&text).to_owned()
}

/// What produced a transcript item, as a stable wire name.
///
/// The chat view has its own role enum shaped around how it renders; this is
/// the name the HTTP API publishes, so it changes only when the transcript
/// model does.
pub fn transcript_item_role(body: &TranscriptBody) -> &'static str {
    match body {
        TranscriptBody::User { .. } => "user",
        TranscriptBody::Agent { .. } => "agent",
        TranscriptBody::Thought { .. } => "thought",
        TranscriptBody::Tool { .. } => "tool",
        TranscriptBody::TerminalOutput { .. } => "terminal",
        TranscriptBody::Plan { .. } => "plan",
        TranscriptBody::PlanProposal { .. } => "plan_proposal",
        TranscriptBody::System { .. } => "system",
    }
}

/// One transcript item flattened to the text a reader would see.
///
/// A caller that wants the structure reads the body itself; this is the plain
/// reading, built from the same flatteners every other surface uses so that a
/// tool call reads as the command it ran rather than as JSON.
pub fn transcript_item_text(item: &TranscriptItem) -> String {
    match &item.body {
        TranscriptBody::User { content } => materialized_content_text(content),
        TranscriptBody::Agent { chunks, .. } | TranscriptBody::Thought { chunks, .. } => {
            materialized_chunks_text(chunks)
        }
        TranscriptBody::Tool {
            call,
            terminal_outputs,
            presentation,
            ..
        } => {
            let Ok(call) = ToolCall::deserialize(call) else {
                return "[invalid tool call]".to_owned();
            };
            let mut text = materialized_tool_call_presentation(presentation.as_deref(), &call)
                .summary
                .clone();
            if text.trim().is_empty() {
                text = call.title.clone();
            }
            for record in terminal_outputs {
                text.push('\n');
                text.push_str(&terminal_output_detail(record));
            }
            sanitize_terminal_text(&text)
        }
        TranscriptBody::TerminalOutput { record } => {
            sanitize_terminal_text(&terminal_output_detail(record))
        }
        TranscriptBody::Plan { plan } => {
            let Ok(plan) = agent_client_protocol::schema::v1::Plan::deserialize(plan) else {
                return String::new();
            };
            plan.entries
                .iter()
                .map(|entry| {
                    let status = match plan_status(&entry.status) {
                        PlanStatus::Pending => "pending",
                        PlanStatus::Running => "running",
                        PlanStatus::Completed => "completed",
                    };
                    format!("[{status}] {}", sanitize_terminal_text(&entry.content))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        TranscriptBody::PlanProposal { plan, .. } => plan.clone(),
        TranscriptBody::System { text } => text.clone(),
    }
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
    use agent_client_protocol::schema::v1::ToolCall;
    use serde_json::json;

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

    #[test]
    fn execute_shell_summary_keeps_commands_and_control_operators() {
        let call = ToolCall::new("call-1", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "cd dir && python x.py | cat | wc ; print ok"
            }));

        let presentation = tool_call_presentation(&call);
        assert_eq!(presentation.summary, "cd && python | cat | wc ; print");
        assert_eq!(presentation.source_kind, ToolSummarySourceKind::RawInput);
    }

    #[test]
    fn execute_sources_handle_shell_argv_and_ordinary_argv() {
        let shell = ToolCall::new("shell", "Terminal")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": ["bash", "-lc", "cd dir && python x.py | cat"]
            }));
        assert_eq!(tool_call_presentation(&shell).summary, "cd && python | cat");

        let argv = ToolCall::new("argv", "Execute")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": ["python", "-c", "print(1)"]}));
        assert_eq!(tool_call_presentation(&argv).summary, "python");
    }

    #[test]
    fn output_updates_reuse_input_command_summaries_but_changed_commands_do_not() {
        let call = ToolCall::new("shell", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "cargo test"}));
        let mut output = ToolCallUpdateFields::default();
        output.raw_output = Some(json!({"output": "x".repeat(128 * 1024)}));
        assert!(!tool_call_update_changes_presentation(&call, &output));
        let mut status = ToolCallUpdateFields::default();
        status.status = Some(ToolCallStatus::Completed);
        assert!(!tool_call_update_changes_presentation(&call, &status));
        let mut changed = ToolCallUpdateFields::default();
        changed.raw_input = Some(json!({"command": "cargo check"}));
        assert!(tool_call_update_changes_presentation(&call, &changed));
        let output_call = ToolCall::new("output", "Bash").kind(ToolKind::Execute);
        assert!(tool_call_update_changes_presentation(&output_call, &output));
    }

    fn execute_summary(command: serde_json::Value) -> String {
        let call = ToolCall::new("argv", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(command);
        tool_call_presentation(&call).summary
    }

    #[test]
    fn argv_summary_keeps_registered_command_verbs() {
        assert_eq!(
            execute_summary(json!({
                "command": ["git", "--no-pager", "status", "--short"]
            })),
            "git status"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["cargo", "+nightly", "test", "--package", "hel"]
            })),
            "cargo test"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["gh", "--hostname", "github.example", "pr", "list"]
            })),
            "gh pr list"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["docker", "--context", "work", "compose", "up"]
            })),
            "docker compose up"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["podman", "machine", "list"]
            })),
            "podman machine list"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["uv", "--project", "app", "pip", "install", "ruff"]
            })),
            "uv pip install"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["rustup", "toolchain", "list"]
            })),
            "rustup toolchain list"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["npm", "--prefix", "web", "run", "build"]
            })),
            "npm run"
        );
    }

    #[test]
    fn string_shell_and_argv_summaries_have_the_same_invocation_depth() {
        let string = ToolCall::new("string", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "git --no-pager status --short"}));
        let argv = ToolCall::new("argv", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": ["git", "--no-pager", "status", "--short"]}));
        assert_eq!(
            tool_call_presentation(&string).summary,
            tool_call_presentation(&argv).summary
        );
    }

    #[test]
    fn unknown_leading_options_make_verb_position_ambiguous() {
        assert_eq!(
            execute_summary(json!({"command": ["git", "--mystery", "status"]})),
            "git"
        );
        assert_eq!(
            execute_summary(json!({"command": ["cargo", "--mystery", "test"]})),
            "cargo"
        );
    }

    #[test]
    fn non_whitelisted_commands_keep_only_the_executable() {
        assert_eq!(
            execute_summary(json!({"command": ["mytool", "build", "src"]})),
            "mytool"
        );
        assert_eq!(
            execute_summary(json!({"command": ["mytool", "./script.sh"]})),
            "mytool"
        );
        assert_eq!(
            execute_summary(json!({"command": ["python", "script.py"]})),
            "python"
        );
    }

    #[test]
    fn quoted_and_dynamic_shell_verbs_are_distinguished() {
        let quoted = ToolCall::new("quoted", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "git \"status\""}));
        assert_eq!(tool_call_presentation(&quoted).summary, "git status");

        let dynamic = ToolCall::new("dynamic", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "git \"$verb\""}));
        assert_eq!(tool_call_presentation(&dynamic).summary, "git");

        let dynamic_name = ToolCall::new("dynamic-name", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "$command status"}));
        assert_eq!(tool_call_presentation(&dynamic_name).summary, "Bash");
    }

    #[test]
    fn wrappers_remain_direct_invocations() {
        assert_eq!(
            tool_call_presentation(
                &ToolCall::new("sudo", "Bash")
                    .kind(ToolKind::Execute)
                    .raw_input(json!({"command": "sudo -n git status"}))
            )
            .summary,
            "sudo"
        );
        assert_eq!(
            tool_call_presentation(
                &ToolCall::new("env", "Bash")
                    .kind(ToolKind::Execute)
                    .raw_input(json!({"command": "env FOO=bar git status"}))
            )
            .summary,
            "env"
        );
        assert_eq!(
            tool_call_presentation(
                &ToolCall::new("command", "Bash")
                    .kind(ToolKind::Execute)
                    .raw_input(json!({"command": "command git status"}))
            )
            .summary,
            "command"
        );
    }

    #[test]
    fn second_verbs_require_a_registered_namespace() {
        assert_eq!(
            execute_summary(json!({"command": ["gh", "api", "graphql"]})),
            "gh api"
        );
        assert_eq!(
            execute_summary(json!({"command": ["docker", "run", "ubuntu"]})),
            "docker run"
        );
        assert_eq!(
            execute_summary(json!({"command": ["git", "future-verb"]})),
            "git future-verb"
        );
    }

    #[test]
    fn known_attached_and_short_global_options_are_skipped() {
        assert_eq!(
            execute_summary(json!({"command": ["/usr/bin/git", "-Crepo", "status"]})),
            "/usr/bin/git status"
        );
        assert_eq!(
            execute_summary(json!({"command": ["git", "-c", "core.pager=cat", "status"]})),
            "git status"
        );
        assert_eq!(
            execute_summary(json!({"command": ["gh", "-Rorg/repo", "pr", "list"]})),
            "gh pr list"
        );
        assert_eq!(
            execute_summary(json!({"command": ["cargo", "--color=always", "test"]})),
            "cargo test"
        );
        assert_eq!(
            execute_summary(json!({"command": ["cargo", "--config", "build.jobs=2", "test"]})),
            "cargo test"
        );
    }

    #[test]
    fn shell_summary_skips_assignments_arguments_and_nested_substitutions() {
        let call = ToolCall::new("call", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "FOO=bar env -i bash -c \"echo $(printf hi)\""
            }));
        assert_eq!(tool_call_presentation(&call).summary, "env");

        let subshell = ToolCall::new("subshell", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "(cd dir && python x.py) | cat"
            }));
        assert_eq!(
            tool_call_presentation(&subshell).summary,
            "(cd && python) | cat"
        );
    }

    #[test]
    fn shell_summary_keeps_list_pipeline_and_background_operators() {
        let call = ToolCall::new("operators", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "'printf' '%s' hi >out |& sed s/hi/bye/ || echo failed & wait; cat <in"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "printf |& sed || echo & wait ; cat"
        );
    }

    #[test]
    fn shell_summary_removes_structural_loop_and_group_separators() {
        let call = ToolCall::new("compound", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "for file in a b; do rm \"$file\"; done; mkdir -p out; nice -n 10 python3 script.py"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "rm ; mkdir ; nice python3"
        );

        let conditional = ToolCall::new("conditional", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "if test -f foo; then rm foo; fi; { mkdir bar; echo done; }"
            }));
        assert_eq!(
            tool_call_presentation(&conditional).summary,
            "test ; rm ; mkdir ; echo"
        );

        let case_statement = ToolCall::new("case", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "echo start; case x in a|b) echo branch;; esac; echo done"
            }));
        assert_eq!(
            tool_call_presentation(&case_statement).summary,
            "echo ; echo ; echo"
        );
    }

    #[test]
    fn shell_summary_handles_a_loop_with_a_leading_pipeline_and_nice() {
        let call = ToolCall::new("live-loop-shape", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "cd /tmp && for spec in a b; do set -- $spec; rm \"$spec\"; mkdir -p \"$spec\"; nice -n 10 ./bin/bifrost \"$spec\"; echo \"$spec\"; done; python3 script.py"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "cd && set ; rm ; mkdir ; nice ./bin/bifrost ; echo ; python3"
        );
    }

    #[test]
    fn shell_summary_inserts_a_separator_after_a_heredoc() {
        let call = ToolCall::new("heredoc", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "python3 <<'PYEOF'\nprint(\"x\")\nPYEOF\ngrep -n x file | head"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "python3 ; grep | head"
        );
    }

    #[test]
    fn materialized_summary_from_an_older_parser_is_repaired() {
        let source = "python3 <<'PYEOF'\nprint(\"x\")\nPYEOF\ngrep -n x file | head";
        let call = ToolCall::new("heredoc", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({ "command": source }));
        let stale = ToolCallPresentation {
            summary: "python3 grep | head".into(),
            source: source.into(),
            source_kind: ToolSummarySourceKind::RawInput,
            tool_kind: ToolKind::Execute,
            summary_version: 0,
        };

        let repaired = materialized_tool_call_presentation(Some(&stale), &call);
        assert_eq!(repaired.summary, "python3 ; grep | head");
        assert_eq!(repaired.summary_version, TOOL_SUMMARY_VERSION);

        let mut current = repaired;
        current.summary = "stored current summary".into();
        assert_eq!(
            materialized_tool_call_presentation(Some(&current), &call).summary,
            "stored current summary"
        );
    }

    #[test]
    fn nice_summary_skips_its_known_adjustment_options() {
        for command in [
            vec!["nice", "-n", "10", "python3", "script.py"],
            vec!["nice", "--adjustment", "10", "python3", "script.py"],
            vec!["nice", "--adjustment=10", "python3", "script.py"],
            vec!["nice", "-10", "python3", "script.py"],
            vec!["nice", "-n10", "python3", "script.py"],
            vec!["nice", "--", "python3", "script.py"],
        ] {
            assert_eq!(execute_summary(json!({"command": command})), "nice python3");
        }

        let string = ToolCall::new("nice-string", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "nice -n 10 python3 script.py"}));
        assert_eq!(tool_call_presentation(&string).summary, "nice python3");
    }

    #[test]
    fn execute_summary_bounds_the_retained_source_before_parsing() {
        let command = format!("echo {}", "argument".repeat(TOOL_SUMMARY_SOURCE_BYTES));
        let call = ToolCall::new("bounded", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({ "command": command }));

        let presentation = tool_call_presentation(&call);
        assert_eq!(presentation.source.len(), TOOL_SUMMARY_SOURCE_BYTES);
        assert_eq!(presentation.summary, "echo");
    }

    #[test]
    fn non_execute_titles_use_the_first_meaningful_token() {
        let call = ToolCall::new("read", "Read src/lib.rs").kind(ToolKind::Read);
        let presentation = tool_call_presentation(&call);
        assert_eq!(presentation.summary, "Read");
        assert_eq!(presentation.source_kind, ToolSummarySourceKind::Title);
    }

    #[test]
    fn title_wrappers_and_malformed_shell_fall_back_safely() {
        let wrapped = ToolCall::new("wrapped", "Running: ls -la").kind(ToolKind::Execute);
        assert_eq!(tool_call_presentation(&wrapped).summary, "ls");

        let malformed = ToolCall::new("bad", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "cd && ("}));
        assert_eq!(tool_call_presentation(&malformed).summary, "Bash");
    }

    #[test]
    fn explicit_empty_raw_input_drops_a_stale_raw_summary() {
        let initial = ToolCall::new("call", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "python script.py"}));
        let previous = tool_call_presentation(&initial);
        let empty_input = json!({"command": null});
        let updated = update_tool_call_presentation(
            Some(&previous),
            "Running: ls -la",
            None,
            Some(&empty_input),
            None,
        );
        assert_eq!(updated.summary, "ls");
        assert_eq!(updated.source_kind, ToolSummarySourceKind::Title);

        let output = json!({"command": "cat result.txt"});
        let updated = update_tool_call_presentation(
            Some(&previous),
            "Running: ls -la",
            None,
            Some(&empty_input),
            Some(&output),
        );
        assert_eq!(updated.summary, "cat");
        assert_eq!(updated.source_kind, ToolSummarySourceKind::RawOutput);

        let output_initial = ToolCall::new("output", "Bash")
            .kind(ToolKind::Execute)
            .raw_output(json!({"command": "python result.py"}));
        let output_previous = tool_call_presentation(&output_initial);
        let empty_output = json!({"command": null});
        let updated = update_tool_call_presentation(
            Some(&output_previous),
            "Running: ls -la",
            None,
            None,
            Some(&empty_output),
        );
        assert_eq!(updated.summary, "ls");
        assert_eq!(updated.source_kind, ToolSummarySourceKind::Title);
    }
}
