//! Claude Code SDK `result` messages, which mark the end of one model cycle.
//!
//! The Claude adapter answers `session/prompt` when it decides the turn is
//! over, and it holds that reply while background subagents the turn started
//! are still alive. Claude Code reports the end of every model cycle with one
//! SDK `result` message, which the adapter forwards as `_claude/sdkMessage`
//! because the session asks for it at `session/new`. The worker uses these
//! results to end a prompt when its answer is complete, and to end a turn
//! Claude Code started on its own.

use agent_client_protocol::schema::v1::StopReason;
use serde::{Deserialize, Serialize};

use crate::config::HarnessKind;

/// The fields of one SDK `result` message that decide whether it ends a
/// prompt, and the usage it reports.
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
    /// Position of this result among the SDK results one ACP connection
    /// received, stamped by the worker when the notification arrived. The
    /// prompt loop compares it with the position at which it sent its
    /// `session/prompt`: a result received before that cannot answer it.
    #[serde(default)]
    pub received: u64,
}

/// Token counts of one result, in the adapter's tally shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeResultUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl ClaudeResultUsage {
    /// The usage the adapter reports on its prompt reply for a turn of one
    /// cycle: the four tallies, with the total counting all of them.
    #[must_use]
    pub fn token_usage(self) -> crate::usage::TokenUsage {
        let total = self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens;
        crate::usage::TokenUsage::from_acp(
            HarnessKind::Claude,
            agent_client_protocol::schema::v1::Usage::new(
                total,
                self.input_tokens,
                self.output_tokens,
            )
            .cached_read_tokens(self.cache_read_tokens)
            .cached_write_tokens(self.cache_write_tokens),
        )
    }
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
            received: 0,
        }))
    }

    /// Whether this cycle answered the user rather than a background task.
    ///
    /// Only `human` and an absent origin count. The adapter sends every origin
    /// it does not know to the user's lane; Mjolnir does the opposite, because
    /// ignoring a result here only means the adapter's reply ends the prompt,
    /// as it always did, while accepting a background cycle's result would end
    /// the user's prompt early.
    #[must_use]
    pub fn answers_user_prompt(&self) -> bool {
        self.origin_kind
            .as_deref()
            .is_none_or(|kind| kind == "human")
    }

    /// Whether Claude Code reports a cycle that the user's own input
    /// interrupted: a steer, an answer to a plan review, or a cancel. The
    /// adapter recognizes these reports by the same shape
    /// (`isEmptyUserInterruptionDiagnostic` and the plan-mode check in
    /// claude-agent-acp's `acp-agent.js`). The cycle after it decides the
    /// prompt.
    #[must_use]
    pub fn is_interruption_report(&self) -> bool {
        self.is_error
            && self.diagnostic.as_deref().is_some_and(|diagnostic| {
                diagnostic
                    .split_whitespace()
                    .any(|token| token == "result_type=user")
            })
    }

    /// The stop reason this result gives the user's prompt, or `None` when the
    /// result does not end the prompt on its own.
    ///
    /// A result does not end the prompt when a background task started its
    /// cycle, when Claude Code reports another user cycle queued behind it, or
    /// when it reports an interrupted cycle. Nor does a successful result
    /// whose cycle produced no model output: a local command such as
    /// `/context`, or a replayed answer. The adapter sends that text after the
    /// result and before its reply, and it never holds such a reply, because
    /// a cycle with no model output cannot start background work. Nor does a
    /// failure: the adapter fails the prompt at once through its reply, which
    /// carries the error that Mjolnir reports and that credential recovery
    /// reads.
    ///
    /// The mapping is the adapter's own (`case "result"` in claude-agent-acp's
    /// `acp-agent.js`): a refusal first, then a sign-in failure, then
    /// `max_tokens`, then errors, then the subtype. The values are spelled the
    /// way the prompt reply's stop reason is recorded.
    #[must_use]
    pub fn prompt_stop_reason(&self) -> Option<String> {
        if !self.answers_user_prompt()
            || self.queued_turn_count > 0
            || self.is_interruption_report()
            || self.num_turns == 0
        {
            return None;
        }
        if self.stop_reason.as_deref() == Some("refusal") {
            return Some(stop_reason_text(StopReason::Refusal));
        }
        let max_tokens = self.stop_reason.as_deref() == Some("max_tokens");
        let stop = match self.subtype.as_str() {
            "success" if self.signed_out => return None,
            "success" | "error_during_execution" if max_tokens => StopReason::MaxTokens,
            _ if self.is_error => return None,
            "success" if self.usage.output_tokens == 0 => return None,
            "success" | "error_during_execution" => StopReason::EndTurn,
            "error_max_turns" | "error_max_budget_usd" | "error_max_structured_output_retries" => {
                StopReason::MaxTurnRequests
            }
            _ => return None,
        };
        Some(stop_reason_text(stop))
    }
}

/// How a prompt reply's stop reason is recorded: the ACP variant's name.
fn stop_reason_text(stop: StopReason) -> String {
    format!("{stop:?}")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn success(origin: Option<&str>) -> serde_json::Value {
        let mut message = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "num_turns": 3,
            "stop_reason": "end_turn",
            "result": "All tests pass.",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_input_tokens": 300,
                "cache_creation_input_tokens": 4,
            },
            "total_cost_usd": 0.5,
            "uuid": "result-1",
            "session_id": "native",
        });
        if let Some(kind) = origin {
            message["origin"] = serde_json::json!({"kind": kind});
        }
        message
    }

    fn stop_reason(message: &serde_json::Value) -> Option<String> {
        ClaudeTurnResult::from_sdk_message(message)
            .expect("a result parses")
            .expect("a result is recognized")
            .prompt_stop_reason()
    }

    #[test]
    fn only_result_messages_are_read() {
        for message in [
            serde_json::json!({"type": "system", "subtype": "background_tasks_changed", "tasks": []}),
            serde_json::json!({"type": "assistant", "message": {}}),
            serde_json::json!({"subtype": "success"}),
        ] {
            assert_eq!(
                ClaudeTurnResult::from_sdk_message(&message).unwrap(),
                None,
                "{message}"
            );
        }
        assert!(
            ClaudeTurnResult::from_sdk_message(
                &serde_json::json!({"type": "result", "subtype": "success"})
            )
            .is_err(),
            "a result without is_error and num_turns is malformed"
        );
    }

    #[test]
    fn a_user_cycle_that_finished_ends_the_prompt_with_its_usage() {
        let result = ClaudeTurnResult::from_sdk_message(&success(Some("human")))
            .unwrap()
            .unwrap();
        assert_eq!(result.prompt_stop_reason().as_deref(), Some("EndTurn"));
        let usage = result.usage.token_usage();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cached_read_tokens, Some(300));
        assert_eq!(usage.cached_write_tokens, Some(4));
        assert_eq!(usage.total_tokens, 334);
        assert_eq!(usage.scope, crate::usage::UsageScope::Turn);
        assert_eq!(
            stop_reason(&success(None)).as_deref(),
            Some("EndTurn"),
            "older Claude Code reports no origin for the user's own cycle"
        );
    }

    #[test]
    fn background_cycles_never_end_the_prompt() {
        for kind in [
            "task-notification",
            "peer",
            "coordinator",
            "observer",
            "observer-activity",
            "channel",
            "a-kind-from-the-future",
        ] {
            assert_eq!(stop_reason(&success(Some(kind))), None, "{kind}");
        }
    }

    #[test]
    fn a_queued_user_cycle_or_a_cycle_without_model_output_leaves_the_prompt_open() {
        let mut queued = success(Some("human"));
        queued["queued_turn_count"] = serde_json::json!(1);
        assert_eq!(stop_reason(&queued), None, "a steered cycle follows");
        queued["queued_turn_count"] = serde_json::json!(0);
        assert_eq!(stop_reason(&queued).as_deref(), Some("EndTurn"));

        let mut local = success(Some("human"));
        local["num_turns"] = serde_json::json!(0);
        local["usage"]["output_tokens"] = serde_json::json!(0);
        local["local_command"] = serde_json::json!("/context");
        assert_eq!(
            stop_reason(&local),
            None,
            "a local command made no model call"
        );

        let mut replayed = success(Some("human"));
        replayed["usage"]["output_tokens"] = serde_json::json!(0);
        assert_eq!(
            stop_reason(&replayed),
            None,
            "the adapter sends a replayed answer's text after the result"
        );
    }

    #[test]
    fn stop_reasons_follow_the_adapter_mapping() {
        let mut refusal = success(Some("human"));
        refusal["stop_reason"] = serde_json::json!("refusal");
        refusal["is_error"] = serde_json::json!(true);
        refusal["usage"]["output_tokens"] = serde_json::json!(0);
        assert_eq!(stop_reason(&refusal).as_deref(), Some("Refusal"));

        let mut max_tokens = success(Some("human"));
        max_tokens["stop_reason"] = serde_json::json!("max_tokens");
        assert_eq!(stop_reason(&max_tokens).as_deref(), Some("MaxTokens"));

        let mut interrupted = success(Some("human"));
        interrupted["subtype"] = serde_json::json!("error_during_execution");
        interrupted["stop_reason"] = serde_json::Value::Null;
        interrupted["errors"] = serde_json::json!([]);
        assert_eq!(stop_reason(&interrupted).as_deref(), Some("EndTurn"));

        for subtype in [
            "error_max_turns",
            "error_max_budget_usd",
            "error_max_structured_output_retries",
        ] {
            let mut limited = interrupted.clone();
            limited["subtype"] = serde_json::json!(subtype);
            assert_eq!(
                stop_reason(&limited).as_deref(),
                Some("MaxTurnRequests"),
                "{subtype}"
            );
        }
        let mut unknown = interrupted.clone();
        unknown["subtype"] = serde_json::json!("error_from_the_future");
        assert_eq!(stop_reason(&unknown), None);
    }

    #[test]
    fn failures_and_interruption_reports_leave_the_prompt_to_the_adapter_reply() {
        let mut usage_limit = success(Some("human"));
        usage_limit["is_error"] = serde_json::json!(true);
        usage_limit["result"] = serde_json::json!("You've hit your session limit · resets 4:40am");
        assert_eq!(stop_reason(&usage_limit), None);

        let mut signed_out = success(Some("human"));
        signed_out["result"] = serde_json::json!("Invalid API key · Please run /login");
        assert_eq!(stop_reason(&signed_out), None);

        // The reports of a cycle a steer or a plan answer interrupted.
        for errors in [
            "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null",
            "[ede_diagnostic] result_type=user last_content_type=tool_result stop_reason=tool_use",
        ] {
            let interruption = serde_json::json!({
                "type": "result",
                "subtype": "error_during_execution",
                "is_error": true,
                "num_turns": 2,
                "stop_reason": null,
                "errors": [errors],
                "usage": {"input_tokens": 1, "output_tokens": 5},
                "origin": {"kind": "human"},
            });
            let parsed = ClaudeTurnResult::from_sdk_message(&interruption)
                .unwrap()
                .unwrap();
            assert!(parsed.is_interruption_report(), "{errors}");
            assert_eq!(parsed.prompt_stop_reason(), None, "{errors}");
        }
        let mut success_diagnostic = success(Some("human"));
        success_diagnostic["is_error"] = serde_json::json!(true);
        success_diagnostic["result"] =
            serde_json::json!("[ede_diagnostic] result_type=user last_content_type=n/a");
        let parsed = ClaudeTurnResult::from_sdk_message(&success_diagnostic)
            .unwrap()
            .unwrap();
        assert!(parsed.is_interruption_report());
        let mut other_diagnostic = success_diagnostic.clone();
        other_diagnostic["result"] =
            serde_json::json!("[ede_diagnostic] result_type=assistant_user stop_reason=null");
        assert!(
            !ClaudeTurnResult::from_sdk_message(&other_diagnostic)
                .unwrap()
                .unwrap()
                .is_interruption_report(),
            "the diagnostic token must match whole"
        );
    }

    #[test]
    fn missing_token_counts_read_as_zero() {
        let mut message = success(Some("human"));
        message["usage"] = serde_json::json!({"input_tokens": 1, "output_tokens": null});
        let parsed = ClaudeTurnResult::from_sdk_message(&message)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.usage,
            ClaudeResultUsage {
                input_tokens: 1,
                ..ClaudeResultUsage::default()
            }
        );
    }
}
