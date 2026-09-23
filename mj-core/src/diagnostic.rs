//! Provider failure details attached to a single completed turn.
use serde::{Deserialize, Serialize};

pub const QUOTA_STOP_REASON: &str = "QuotaLimit";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDiagnostic {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Provider-supplied reset value, without guessing a timezone or deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

impl TurnDiagnostic {
    pub fn from_acp(error: &agent_client_protocol::Error) -> Self {
        let mut diagnostic = Self {
            message: error.message.clone(),
            code: Some(error.code.to_string()),
            http_status: None,
            reset_at: None,
        };
        if let Some(data) = &error.data {
            diagnostic.enrich(data);
        }
        diagnostic
    }

    pub fn from_provider(error: &serde_json::Value) -> Option<Self> {
        let message = error.get("message")?.as_str()?.to_owned();
        let mut diagnostic = Self {
            message,
            code: None,
            http_status: None,
            reset_at: None,
        };
        diagnostic.enrich(error);
        Some(diagnostic)
    }

    fn enrich(&mut self, data: &serde_json::Value) {
        if let Some(message) = data.get("message").and_then(serde_json::Value::as_str) {
            self.message = message.to_owned();
        }
        if let Some(code) = data.get("code").and_then(serde_json::Value::as_str) {
            self.code = Some(code.to_owned());
        } else if let Some(code) = data
            .get("codex_error_info")
            .and_then(serde_json::Value::as_str)
        {
            // Preserve provider metadata as evidence; retryability is Jev's decision.
            self.code = Some(code.to_owned());
        }
        let details = data.get("details").unwrap_or(data);
        self.http_status = details
            .get("statusCode")
            .or_else(|| details.get("status"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        self.reset_at = details
            .get("resetAt")
            .or_else(|| details.get("reset_at"))
            .and_then(|value| match value {
                serde_json::Value::String(value) => Some(value.clone()),
                serde_json::Value::Number(value) => Some(value.to_string()),
                _ => None,
            });
    }

    /// Only apply to trusted provider failures, never arbitrary chat or tool text.
    pub fn is_usage_limit(&self) -> bool {
        if matches!(
            self.code.as_deref(),
            Some("usage_limit" | "quota_exceeded" | "provider.quota_exceeded")
        ) {
            return true;
        }
        let message = self.message.to_ascii_lowercase();
        (message.contains("usage limit")
            || message.contains("usage-limit")
            || message.contains("quota"))
            && ["exceeded", "exhausted", "reached", "used up"]
                .iter()
                .any(|word| message.contains(word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_exhaustion_is_quota_and_details_survive() {
        let diagnostic = TurnDiagnostic::from_provider(&serde_json::json!({
            "code":"provider.auth_error", "message":"You have reached your five-hour usage limit. Resets at 23:00 UTC.",
            "details":{"statusCode":403,"resetAt":"23:00 UTC"}
        })).unwrap();
        assert!(diagnostic.is_usage_limit());
        assert_eq!(diagnostic.http_status, Some(403));
        assert_eq!(diagnostic.reset_at.as_deref(), Some("23:00 UTC"));
        for message in [
            "Forbidden",
            "Invalid API key",
            "Unable to read quota",
            "Usage limit service unavailable",
        ] {
            let ordinary = TurnDiagnostic {
                message: message.into(),
                ..diagnostic.clone()
            };
            assert!(!ordinary.is_usage_limit(), "{message}");
        }
    }
}
