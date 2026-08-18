//! Provider quota values shared by runtimes and frontends.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeUsageStatus {
    Available(ClaudeUsageReport),
    Unavailable(String),
}

impl ClaudeUsageStatus {
    pub fn compact_label(&self) -> String {
        match self {
            Self::Available(report) => report.compact_label(),
            Self::Unavailable(reason) => format!("Claude usage unavailable: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeUsageReport {
    pub five_hour: Option<ClaudeUsageWindow>,
    pub week: Option<ClaudeUsageWindow>,
}

impl ClaudeUsageReport {
    pub fn compact_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(window) = &self.five_hour {
            parts.push(window.compact_label("5H"));
        }
        if let Some(window) = &self.week {
            parts.push(window.compact_label("week"));
        }
        if parts.is_empty() {
            "Claude usage: unavailable".to_string()
        } else {
            format!("Claude usage: {}", parts.join(" · "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeUsageWindow {
    pub remaining_percent: u8,
    pub reset_context: Option<String>,
}

impl ClaudeUsageWindow {
    fn compact_label(&self, label: &str) -> String {
        let mut text = format!("{label} {}% left", self.remaining_percent);
        if let Some(reset_context) = &self.reset_context {
            text.push_str(" · resets ");
            text.push_str(reset_context);
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Available(CodexUsageReport),
    Unavailable(String),
}

impl CodexUsageStatus {
    pub fn compact_label(&self) -> String {
        match self {
            Self::Available(report) => report.compact_label(),
            Self::Unavailable(reason) => format!("Codex usage unavailable: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageReport {
    pub primary: Option<CodexUsageWindow>,
    pub secondary: Option<CodexUsageWindow>,
}

impl CodexUsageReport {
    fn compact_label(&self) -> String {
        let parts = [&self.primary, &self.secondary]
            .into_iter()
            .flatten()
            .map(CodexUsageWindow::compact_label)
            .collect::<Vec<_>>();
        format!("Codex usage: {}", parts.join(" · "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageWindow {
    pub label: String,
    pub remaining_percent: u8,
    pub resets_at: Option<i64>,
}

impl CodexUsageWindow {
    fn compact_label(&self) -> String {
        let mut label = format!("{} {}% left", self.label, self.remaining_percent);
        if let Some(reset) = self
            .resets_at
            .and_then(crate::usage_format::format_reset_local_seconds)
        {
            label.push_str(" · resets ");
            label.push_str(&reset);
        }
        label
    }
}
