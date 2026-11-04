//! Claude Code SDK `result` messages, which mark the end of one model cycle.
//!
//! Foreground prompts end only at the adapter's `session/prompt` response.
//! Raw results remain subscribed for autonomous turn completion until
//! claude-agent-acp#1453 is fixed.

use serde::{Deserialize, Serialize};

/// One raw SDK result, retained in runtime journals for diagnostics. Only
/// autonomous turn completion consumes it; foreground replies are authoritative.
/// Keep the historical payload fields for readers of runtime journal records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeTurnResult {
    /// `None` when Claude Code does not report an origin (older builds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_kind: Option<String>,
    pub subtype: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Model round trips in this cycle. Zero for a local command, which
    /// makes no model call, and for a request the provider refused.
    pub num_turns: u64,
    /// User messages still waiting in Claude Code's queue when the cycle
    /// ended. More than zero means another user cycle follows, for example
    /// the one a steer injected. Zero when Claude Code does not report it.
    #[serde(default)]
    pub queued_turn_count: u64,
    /// The result text asks the user to sign in again, which the adapter
    /// reports as an `auth_required` failure of the prompt.
    #[serde(default)]
    pub signed_out: bool,
    /// Claude Code's execution diagnostic, the text starting with
    /// `[ede_diagnostic]`, when the result carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    #[serde(default)]
    pub usage: ClaudeResultUsage,
}

/// Token counts of one result, in the adapter's tally shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeResultUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl ClaudeTurnResult {
    /// Read an SDK message the adapter forwarded. `None` means the message is
    /// not a `result`.
    pub fn from_sdk_message(
        message: &serde_json::Value,
    ) -> std::result::Result<Option<Self>, serde_json::Error> {
        if message.get("type").and_then(serde_json::Value::as_str) != Some("result") {
            return Ok(None);
        }
        let payload = ResultPayload::deserialize(message)?;
        let diagnostic = match payload.subtype.as_str() {
            "success" => payload
                .result
                .as_deref()
                .filter(|text| text.starts_with(DIAGNOSTIC_PREFIX))
                .map(str::to_owned),
            _ => payload
                .errors
                .iter()
                .find(|error| error.starts_with(DIAGNOSTIC_PREFIX))
                .cloned(),
        };
        let usage = payload
            .usage
            .map_or_else(ClaudeResultUsage::default, |usage| ClaudeResultUsage {
                input_tokens: usage.input_tokens.unwrap_or_default(),
                output_tokens: usage.output_tokens.unwrap_or_default(),
                cache_read_tokens: usage.cache_read_input_tokens.unwrap_or_default(),
                cache_write_tokens: usage.cache_creation_input_tokens.unwrap_or_default(),
            });
        Ok(Some(Self {
            origin_kind: payload.origin.map(|origin| origin.kind),
            signed_out: payload.subtype == "success"
                && payload
                    .result
                    .as_deref()
                    .is_some_and(|text| text.contains("Please run /login")),
            subtype: payload.subtype,
            is_error: payload.is_error,
            stop_reason: payload.stop_reason,
            num_turns: payload.num_turns,
            queued_turn_count: payload.queued_turn_count.unwrap_or_default(),
            diagnostic,
            usage,
        }))
    }
}

const DIAGNOSTIC_PREFIX: &str = "[ede_diagnostic]";

#[derive(Deserialize)]
struct ResultPayload {
    subtype: String,
    is_error: bool,
    num_turns: u64,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    queued_turn_count: Option<u64>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    origin: Option<OriginPayload>,
    #[serde(default)]
    usage: Option<UsagePayload>,
}

#[derive(Deserialize)]
struct OriginPayload {
    kind: String,
}

/// Third-party backends have been seen omitting token fields, so a missing or
/// null count reads as zero instead of discarding the result.
#[derive(Deserialize)]
struct UsagePayload {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}
