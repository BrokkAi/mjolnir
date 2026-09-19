//! Read-only session-history requests shared by worker and controller.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_PENDING: usize = 8;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_BLAME_BYTES: usize = 512 * 1024;

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
    SearchSessions {
        query: String,
        #[serde(default = "page_default")]
        limit: usize,
    },
    GetSessionBrief {
        session_id: String,
        #[serde(default = "text_default")]
        max_chars: usize,
    },
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
    },
    TraceFile {
        path: String,
        #[serde(default = "page_default")]
        limit: usize,
    },
    SessionFiles {
        session_id: String,
        #[serde(default)]
        start: usize,
        #[serde(default = "page_default")]
        limit: usize,
    },
    BlameFile {
        path: PathBuf,
        start_line: usize,
        end_line: usize,
    },
}

impl HistoryQuery {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::SearchSessions { query, .. } | Self::SearchSession { query, .. } => {
                ensure!(
                    !query.trim().is_empty() && query.len() <= 4096,
                    "query must contain 1–4096 bytes"
                );
            }
            Self::TraceFile { path, .. } => ensure!(
                !path.trim().is_empty() && path.len() <= 4096,
                "path must contain 1–4096 bytes"
            ),
            Self::BlameFile {
                path,
                start_line,
                end_line,
            } => {
                ensure!(
                    !path.as_os_str().is_empty() && path.as_os_str().len() <= 4096,
                    "invalid file path"
                );
                ensure!(
                    *start_line > 0 && end_line >= start_line && end_line - start_line < 1000,
                    "request an inclusive range of 1–1000 lines"
                );
            }
            _ => {}
        }
        if let Self::ReadSession {
            role: Some(role), ..
        } = self
        {
            ensure!(
                matches!(role.as_str(), "user" | "assistant" | "tool"),
                "role must be user, assistant, or tool"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlameEvidence {
    pub repository: PathBuf,
    pub relative_path: PathBuf,
    pub porcelain: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub request_id: String,
    pub query: HistoryQuery,
    pub blame: Option<BlameEvidence>,
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
