//! Transcript data shared by the worker, controller state, and the chat UI.
//!
//! [`ChatEntry`] is what a worker snapshot carries in its transcript tail and
//! what the chat view renders, and [`TranscriptItem`] is the materialized
//! form controller state persists, so both live below the modules that use
//! them rather than inside any one of them.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const SESSION_RESTART_TEXT: &str = "[session restarted]";
pub const SESSION_RESTART_ITEM_PREFIX: &str = "system:session-restarted:";

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

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
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
