//! Worker-facing types unrelated to the durable relay: legacy pre-relay
//! event/snapshot shapes still used to import old histories into the
//! controller-owned projection, plus small shared value types like
//! `Attachment` and `QueuedPrompt`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hel_transcript::is_false;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub media_type: String,
    /// A target-local path or opaque adapter reference. File contents do not
    /// travel in control messages.
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    Idle,
    Running,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSessionSummary {
    pub phase: WorkerPhase,
    pub latest_seq: u64,
    pub latest_completed_turn_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub session_title: Option<String>,
    pub unread_agent_messages: u64,
    pub agent_text_stream_open: bool,
    pub last_agent_message_id: Option<String>,
    pub transcript_tail: Vec<crate::hel_transcript::ChatEntry>,
    #[serde(default)]
    pub queued_prompts: Vec<QueuedPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePrompt {
    pub request_id: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedPrompt {
    pub id: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub created_at_ms: i64,
}

/// Minimal snapshot shape retained for importing pre-relay event histories
/// into the controller-owned materialized projection. It is not a relay wire
/// message and is never persisted by `DurableRelay`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub session_id: String,
    pub phase: WorkerPhase,
    pub latest_seq: u64,
    pub last_checkpoint_seq: Option<u64>,
    pub active_prompt: Option<ActivePrompt>,
    pub config: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<QueuedPrompt>,
}

impl WorkerSnapshot {
    pub fn summary(session_id: String, phase: WorkerPhase, latest_seq: u64) -> Self {
        Self {
            session_id,
            phase,
            latest_seq,
            last_checkpoint_seq: None,
            active_prompt: None,
            config: BTreeMap::new(),
            queued_prompts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequencedEvent {
    pub seq: u64,
    /// UTC receive/accept time recorded by the durable worker. Legacy and
    /// imported event streams may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at_ms: Option<i64>,
    /// Present for controller mutations. Persisting the id beside the event
    /// closes the crash window between appending the event and snapshotting
    /// the idempotency result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub event: WorkerEvent,
}

/// Event shape used only to decode/import histories produced before relay-v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkerEvent {
    PromptAccepted {
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    },
    QueuedPromptAdded {
        prompt: QueuedPrompt,
    },
    QueuedPromptRemoved {
        queue_id: String,
    },
    QueuedPromptPromoted {
        prompt: QueuedPrompt,
        request_id: String,
    },
    QueuedPromptsCleared,
    TurnCompleted,
    Cancelled,
    ConfigChanged {
        key: String,
        value: Value,
    },
    Checkpointed {
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        quiescent: bool,
    },
    Closing,
    Closed,
    Adapter {
        kind: String,
        payload: Value,
    },
}
