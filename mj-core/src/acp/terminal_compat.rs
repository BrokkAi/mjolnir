//! Transcript compatibility for agents that use client terminals without
//! publishing the ACP tool call that owns them.

use agent_client_protocol::schema::v1::ToolCall;
use agent_client_protocol::schema::v1::{Terminal, ToolCallContent, ToolCallStatus, ToolKind};

const FALLBACK_TERMINAL_META_KEY: &str = "hel.dev/fallback-terminal";
const FALLBACK_TERMINAL_TOOL_ID_PREFIX: &str = "hel-terminal:";

pub fn fallback_terminal_tool_call(terminal_id: &str, command: String) -> ToolCall {
    ToolCall::new(fallback_terminal_tool_call_id(terminal_id), command)
        .kind(ToolKind::Execute)
        .status(ToolCallStatus::InProgress)
        .content(vec![ToolCallContent::Terminal(Terminal::new(
            terminal_id.to_owned(),
        ))])
        .meta(serde_json::Map::from_iter([(
            FALLBACK_TERMINAL_META_KEY.into(),
            serde_json::Value::Bool(true),
        )]))
}

pub fn fallback_terminal_tool_call_id(terminal_id: &str) -> String {
    format!("{FALLBACK_TERMINAL_TOOL_ID_PREFIX}{terminal_id}")
}

pub fn is_fallback_terminal_tool_call(call: &ToolCall) -> bool {
    call.meta.as_ref().is_some_and(|meta| {
        meta.get(FALLBACK_TERMINAL_META_KEY)
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    })
}
