//! Bounded turn evidence and conservative decisions for the optional Jev classifier.
use serde::Serialize;

use crate::config::HarnessKind;

pub const USER_PROMPT_BYTES: usize = 1024;
pub const ASSISTANT_TEXT_BYTES: usize = 2048;
pub const TOOL_TITLE_BYTES: usize = 128;
pub const IN_FLIGHT_TOOLS: usize = 16;

/// A tool or command title as every classifier sees it: its first
/// [`TOOL_TITLE_BYTES`], where the program and its operation are.
pub fn tool_title(text: &str) -> String {
    text[..text.floor_char_boundary(TOOL_TITLE_BYTES)].to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    Replied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ToolEvidence {
    pub title: String,
    pub running_s: u64,
}

/// One background command the agent left running, as the quiet judgment sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct BackgroundEvidence {
    pub id: String,
    /// The command line, cut to [`TOOL_TITLE_BYTES`].
    pub command: String,
    pub started_s_ago: u64,
}

/// A tool call the agent made after its last text, by name and outcome only.
/// A `handback` or `spawn` here says what the reply's silence does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ToolOutcome {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TurnEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<crate::assessment::ContextHistory>,
    /// Tool calls made after the last assistant text of the turn, oldest
    /// first, at most [`IN_FLIGHT_TOOLS`]. Empty on older workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub final_tool_calls: Vec<ToolOutcome>,
    /// The background commands behind `background_commands`, sent so Jev can
    /// judge whether anyone still depends on them. Empty on older workers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background: Vec<BackgroundEvidence>,
    pub harness: HarnessKind,
    pub phase: TurnPhase,
    pub silent_for_s: u64,
    pub tools_in_flight: Vec<ToolEvidence>,
    pub transcript_summary: String,
    pub background_commands: usize,
    pub queued_commands: usize,
    pub user_prompt_tail: String,
    pub assistant_text_tail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<CompletionEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct CompletionEvidence {
    pub stop_reason: String,
    pub diagnostic: Option<crate::diagnostic::TurnDiagnostic>,
}

impl CompletionEvidence {
    pub fn bounded(
        stop_reason: &str,
        diagnostic: Option<&crate::diagnostic::TurnDiagnostic>,
    ) -> Self {
        let mut stop_reason = stop_reason.to_owned();
        stop_reason.truncate(stop_reason.floor_char_boundary(128));
        let diagnostic = diagnostic.cloned().map(|mut d| {
            if d.message.len() > 4096 {
                d.message = "[Provider diagnostic omitted: exceeds 4096 bytes]".into();
            }
            if d.code.as_ref().is_some_and(|v| v.len() > 128) {
                d.code = None;
            }
            if d.reset_at.as_ref().is_some_and(|v| v.len() > 256) {
                d.reset_at = None;
            }
            d
        });
        Self {
            stop_reason,
            diagnostic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AwaitingInput,
    ExpectContinuation,
    InferIdle,
    KeepCurrent,
}

/// The latest prompt or harness turn that ended: its command and the ordinal of its
/// completion event. `HandbackReminder` names a turn by this identity, and its
/// sender takes `completed_ordinal` from the projected turn outcome, so the two
/// must stay equal.
///
/// Jev's verdict on a turn lives in the worker's assessment record
/// (`mj_core::assessment::TurnAssessment`), and nothing reads `decision`.
/// It is still written, always as `KeepCurrent`, because the relay protocol
/// lets a newer worker serve an older daemon: daemons before #1304 require the
/// field, and read anything but `KeepCurrent` as a verdict on this turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TurnCompletion {
    pub command_id: String,
    pub completed_ordinal: u64,
    pub decision: Decision,
}
