//! Read-only session-history requests shared by worker and controller.
use std::time::Duration;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_PENDING: usize = 8;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Caller-relative session spelling accepted by `search_history` and `read_session`.
pub const SELF_SESSION_ALIAS: &str = "self";

pub fn is_self_alias(id: &str) -> bool {
    id == SELF_SESSION_ALIAS
}

pub fn page_default() -> usize {
    20
}
pub fn text_default() -> usize {
    16_000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "tool",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HistoryQuery {
    /// Unified cross-session and scoped search. Needs relay protocol
    /// [`crate::relay::RELAY_SEARCH_HISTORY_PROTOCOL`].
    SearchHistory {
        query: String,
        #[serde(default)]
        session_ids: Option<Vec<String>>,
        /// Unadvertised singular alias for blind callers; unioned with `session_ids`.
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        order: Option<String>,
        #[serde(default = "page_default")]
        limit: usize,
        #[serde(default)]
        context_messages: usize,
        #[serde(default = "text_default")]
        max_chars: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Retained for resumed transcripts and older workers. Delete this variant,
    /// `SearchSession`, and `transcript_grep::grep_session` once
    /// `MINIMUM_WORKER_PROTOCOL > RELAY_SEARCH_HISTORY_PROTOCOL`.
    SearchSessions {
        query: String,
        #[serde(default = "page_default")]
        limit: usize,
    },
    /// No longer advertised by `mj-memory`, but older active workers can still
    /// send it inside `HistoryRequests`, where `deny_unknown_fields` would fail
    /// the entire poll if it were deleted. Delete once
    /// `RELAY_MIN_PROTOCOL_VERSION >= RELAY_READ_SESSION_V2_PROTOCOL` (the floor
    /// is currently 1, so this is effectively permanent).
    GetSessionBrief {
        session_id: String,
        #[serde(default = "text_default")]
        max_chars: usize,
    },
    /// Retained for resumed transcripts and older workers; see `SearchSessions`.
    SearchSession {
        session_id: String,
        query: String,
        #[serde(default)]
        start: usize,
        #[serde(default)]
        context: usize,
        #[serde(default = "page_default")]
        limit: usize,
        #[serde(default = "text_default")]
        max_chars: usize,
    },
    /// `tail`, `before`, `max_message_chars`, `role="dialogue"`, and `"self"` need
    /// [`crate::relay::RELAY_READ_SESSION_V2_PROTOCOL`]; the new fields are omitted
    /// from the wire when unset so plain reads still reach older controllers.
    ReadSession {
        session_id: String,
        #[serde(default)]
        start: usize,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        role: Option<String>,
        #[serde(default = "page_default")]
        limit: usize,
        #[serde(default = "text_default")]
        max_chars: usize,
        /// Read the last page, chronologically.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tail: bool,
        /// Read the page that ends before this message index (exclusive).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<usize>,
        /// Per-message character cap; zero means no cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_message_chars: Option<usize>,
    },
}

/// Deduplicated union of `session_ids` and the singular `session_id` alias,
/// preserving order with `session_ids` first.
pub fn merged_search_history_session_ids(
    session_ids: Option<&[String]>,
    session_id: Option<&str>,
) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for id in session_ids
        .into_iter()
        .flatten()
        .map(String::as_str)
        .chain(session_id)
    {
        if !merged.iter().any(|seen| seen == id) {
            merged.push(id.to_string());
        }
    }
    merged
}

impl HistoryQuery {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::SearchHistory {
                query,
                session_ids,
                session_id,
                role,
                order,
                ..
            } => {
                ensure!(
                    !query.trim().is_empty() && query.len() <= 4096,
                    "query must contain 1–4096 bytes"
                );
                if let Some(role) = role {
                    ensure_role(role)?;
                }
                if let Some(order) = order {
                    ensure!(
                        matches!(order.as_str(), "newest" | "oldest"),
                        "order must be newest or oldest"
                    );
                }
                let ids = merged_search_history_session_ids(
                    session_ids.as_deref(),
                    session_id.as_deref(),
                );
                ensure!(ids.len() <= 50, "session_ids accepts at most 50 sessions");
                ensure!(
                    ids.iter()
                        .all(|id| !id.trim().is_empty() && id.len() <= 256),
                    "each session id must contain 1–256 bytes"
                );
            }
            Self::SearchSessions { query, .. } | Self::SearchSession { query, .. } => {
                ensure!(
                    !query.trim().is_empty() && query.len() <= 4096,
                    "query must contain 1–4096 bytes"
                );
            }
            _ => {}
        }
        if let Self::ReadSession {
            role,
            start,
            offset,
            tail,
            before,
            ..
        } = self
        {
            if let Some(role) = role {
                ensure_role(role)?;
            }
            if *tail || before.is_some() {
                ensure!(*start == 0, "tail/before cannot be combined with start");
                ensure!(*offset == 0, "tail/before cannot be combined with offset");
            }
        }
        Ok(())
    }

    /// The lowest relay protocol that understands this query, and the feature
    /// name to report when the connected controller is older. The controller
    /// gate and the worker's local failure both ask here.
    pub fn protocol_requirement(&self) -> (u32, &'static str) {
        use crate::relay::{RELAY_READ_SESSION_V2_PROTOCOL, RELAY_SEARCH_HISTORY_PROTOCOL};
        match self {
            Self::SearchHistory { role, .. } if role.as_deref() == Some("dialogue") => (
                RELAY_READ_SESSION_V2_PROTOCOL,
                "search_history with role=\"dialogue\"",
            ),
            Self::SearchHistory { .. } => (RELAY_SEARCH_HISTORY_PROTOCOL, "search_history"),
            Self::ReadSession {
                session_id,
                role,
                tail,
                before,
                max_message_chars,
                ..
            } if *tail
                || before.is_some()
                || max_message_chars.is_some()
                || role.as_deref() == Some("dialogue")
                || is_self_alias(session_id) =>
            {
                (
                    RELAY_READ_SESSION_V2_PROTOCOL,
                    "read_session (tail/before/dialogue/max_message_chars/self)",
                )
            }
            Self::ReadSession { .. } => (16, "read_session"),
            _ => (16, "history query"),
        }
    }
}

fn ensure_role(role: &str) -> Result<()> {
    ensure!(
        matches!(role, "user" | "assistant" | "tool" | "dialogue"),
        "role must be user, assistant, tool, or dialogue"
    );
    Ok(())
}

// Older workers still send "blame": null on surviving history requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    pub request_id: String,
    pub query: HistoryQuery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryResult {
    pub request_id: String,
    pub value: serde_json::Value,
    pub is_error: bool,
}

impl HistoryResult {
    pub fn failed(request_id: String, error: impl std::fmt::Display) -> Self {
        Self {
            request_id,
            value: serde_json::json!({"error":error.to_string()}),
            is_error: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(
        query: &str,
        session_ids: Option<Vec<&str>>,
        session_id: Option<&str>,
    ) -> HistoryQuery {
        HistoryQuery::SearchHistory {
            query: query.into(),
            session_ids: session_ids.map(|ids| ids.into_iter().map(String::from).collect()),
            session_id: session_id.map(String::from),
            role: None,
            order: None,
            limit: 20,
            context_messages: 0,
            max_chars: 16_000,
            cursor: None,
        }
    }

    #[test]
    fn search_history_wire_form_accepts_singular_alias_and_unions_it() {
        let wire = serde_json::json!({
            "tool": "search_history",
            "arguments": {"query": "x", "session_ids": ["a", "b"], "session_id": "b"}
        });
        let query: HistoryQuery = serde_json::from_value(wire).unwrap();
        let HistoryQuery::SearchHistory {
            session_ids,
            session_id,
            limit,
            max_chars,
            context_messages,
            ..
        } = &query
        else {
            panic!("expected SearchHistory");
        };
        assert_eq!((*limit, *max_chars, *context_messages), (20, 16_000, 0));
        assert_eq!(
            merged_search_history_session_ids(session_ids.as_deref(), session_id.as_deref()),
            ["a", "b"]
        );
        query.validate().unwrap();
        assert_eq!(
            merged_search_history_session_ids(None, Some("only")),
            ["only"]
        );
    }

    #[test]
    fn read_session_wire_omits_new_fields_unless_used_and_legacy_shapes_parse() {
        use crate::relay::{RELAY_READ_SESSION_V2_PROTOCOL, RelayRequest};
        let plain: HistoryQuery = serde_json::from_value(serde_json::json!({
            "tool": "read_session",
            "arguments": {"session_id": "s", "start": 3}
        }))
        .unwrap();
        let wire = serde_json::to_value(&plain).unwrap();
        let arguments = wire["arguments"].as_object().unwrap();
        for key in ["tail", "before", "max_message_chars"] {
            assert!(!arguments.contains_key(key), "{key} leaked: {wire}");
        }
        assert_eq!(plain.protocol_requirement().0, 16);
        let tail: HistoryQuery = serde_json::from_value(serde_json::json!({
            "tool": "read_session",
            "arguments": {"session_id": "s", "tail": true}
        }))
        .unwrap();
        tail.validate().unwrap();
        let request = RelayRequest::HistoryQuery { query: tail };
        assert_eq!(request.minimum_protocol(), RELAY_READ_SESSION_V2_PROTOCOL);
        assert!(!request.supported_at(RELAY_READ_SESSION_V2_PROTOCOL - 1));
        // Older workers send the brief request unchanged.
        let brief: HistoryQuery = serde_json::from_value(serde_json::json!({
            "tool": "get_session_brief",
            "arguments": {"session_id": "s"}
        }))
        .unwrap();
        assert_eq!(brief.protocol_requirement().0, 16);
    }

    #[test]
    fn read_session_validation_rejects_forward_cursors_with_backward_paging_and_accepts_zero_cap() {
        let read = |arguments: serde_json::Value| -> Result<()> {
            let query: HistoryQuery = serde_json::from_value(
                serde_json::json!({"tool": "read_session", "arguments": arguments}),
            )
            .unwrap();
            query.validate()
        };
        assert!(read(serde_json::json!({"session_id": "s", "tail": true, "start": 1})).is_err());
        assert!(read(serde_json::json!({"session_id": "s", "before": 5, "offset": 1})).is_err());
        read(serde_json::json!({"session_id": "s", "before": 5, "start": 0, "offset": 0})).unwrap();
        read(serde_json::json!({"session_id": "s", "role": "dialogue", "max_message_chars": 0}))
            .unwrap();
        let mut dialogue = search("q", None, None);
        if let HistoryQuery::SearchHistory { role, .. } = &mut dialogue {
            *role = Some("dialogue".into());
        }
        dialogue.validate().unwrap();
    }

    #[test]
    fn history_request_accepts_older_worker_blame_null_field() {
        let request: HistoryRequest = serde_json::from_value(serde_json::json!({
            "request_id": "older-worker",
            "query": {
                "tool": "get_session_brief",
                "arguments": {"session_id": "session"}
            },
            "blame": null
        }))
        .unwrap();
        assert_eq!(request.request_id, "older-worker");
        assert!(matches!(
            request.query,
            HistoryQuery::GetSessionBrief { .. }
        ));
    }

    #[test]
    fn search_history_validation_bounds_query_role_order_and_scope() {
        assert!(search("  ", None, None).validate().is_err());
        assert!(search(&"q".repeat(4097), None, None).validate().is_err());
        let mut bad_role = search("q", None, None);
        if let HistoryQuery::SearchHistory { role, order, .. } = &mut bad_role {
            *role = Some("system".into());
            *order = None;
        }
        assert!(bad_role.validate().is_err());
        let mut bad_order = search("q", None, None);
        if let HistoryQuery::SearchHistory { order, .. } = &mut bad_order {
            *order = Some("random".into());
        }
        assert!(bad_order.validate().is_err());
        let many: Vec<String> = (0..50).map(|i| format!("s{i}")).collect();
        let mut at_limit = search("q", None, None);
        if let HistoryQuery::SearchHistory { session_ids, .. } = &mut at_limit {
            *session_ids = Some(many);
        }
        at_limit.validate().unwrap();
        // The singular alias counts toward the 50-session limit after the union.
        if let HistoryQuery::SearchHistory { session_id, .. } = &mut at_limit {
            *session_id = Some("extra".into());
        }
        assert!(at_limit.validate().is_err());
        assert!(search("q", Some(vec![" "]), None).validate().is_err());
        assert!(
            search("q", None, Some(&"x".repeat(257)))
                .validate()
                .is_err()
        );
        // Oversized context is clamped at execution, never rejected here.
        let mut context = search("q", None, None);
        if let HistoryQuery::SearchHistory {
            context_messages, ..
        } = &mut context
        {
            *context_messages = 200;
        }
        context.validate().unwrap();
    }

    #[test]
    fn search_history_needs_the_search_history_relay_protocol() {
        use crate::relay::{RELAY_SEARCH_HISTORY_PROTOCOL, RelayRequest};
        let request = RelayRequest::HistoryQuery {
            query: search("q", None, None),
        };
        assert_eq!(request.minimum_protocol(), RELAY_SEARCH_HISTORY_PROTOCOL);
        assert!(!request.supported_at(RELAY_SEARCH_HISTORY_PROTOCOL - 1));
        let legacy = RelayRequest::HistoryQuery {
            query: HistoryQuery::SearchSessions {
                query: "q".into(),
                limit: 20,
            },
        };
        assert_eq!(legacy.minimum_protocol(), 16);
    }

    #[test]
    fn workers_with_context_clear_but_without_history_do_not_receive_history_queries() {
        use crate::relay::{RelayCommand, RelayRequest, RelayVersionRange};
        let negotiated = RelayVersionRange::CURRENT
            .negotiate(RelayVersionRange { min: 1, max: 15 })
            .unwrap();
        assert!(
            RelayRequest::Submit {
                command_id: "clear".into(),
                command: RelayCommand::ClearContext
            }
            .supported_at(negotiated)
        );
        assert!(!RelayRequest::HistoryRequests.supported_at(negotiated));
        assert!(RelayRequest::HistoryRequests.supported_at(crate::relay::RELAY_PROTOCOL_VERSION));
    }
}
