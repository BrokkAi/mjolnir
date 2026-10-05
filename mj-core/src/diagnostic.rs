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
        } else if let Some(code) = codex_error_kind(data) {
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
            Some(
                "usage_limit"
                    | "quota_exceeded"
                    | "provider.quota_exceeded"
                    // Codex's `codexErrorInfo` for an exhausted plan (J-25).
                    | "usageLimitExceeded"
                    | "usage_limit_exceeded"
            )
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

/// A failure record a bridge attaches under `_meta.jetbrains.air.sessionFailure`
/// when the client lists `sessionFailure` among its AIR capabilities: on the
/// prompt response of a turn that failed (whose stop reason still says
/// `end_turn`), and on `session_info_update` for warnings and retries. Records
/// that share an `id` are revisions of one notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFailure {
    pub id: String,
    /// `connection`, `access`, `limit`, `request`, `service` or `unknown`.
    pub category: String,
    /// `error` or `warning`; a record without one is an error.
    pub severity: String,
    pub title: String,
    pub details: Option<String>,
    /// What the bridge says may help: `retry`, `login`, `new_session`.
    pub actions: Vec<String>,
}

impl SessionFailure {
    pub fn from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<Self> {
        let record = meta?.get("jetbrains")?.get("air")?.get("sessionFailure")?;
        let text = |key: &str| {
            record
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Some(Self {
            id: text("id").unwrap_or_default(),
            category: text("category").unwrap_or_default(),
            severity: text("severity").unwrap_or_else(|| "error".to_owned()),
            title: text("title")?,
            details: text("details").filter(|details| !details.trim().is_empty()),
            actions: record
                .get("actions")
                .and_then(serde_json::Value::as_array)
                .map(|actions| {
                    actions
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    pub fn is_error(&self) -> bool {
        self.severity != "warning"
    }
}

impl TurnDiagnostic {
    /// The diagnostic of a turn the bridge reported as failed with a typed
    /// record. The record leaves out Codex's error kind, which quota handling
    /// and credential sync read from the code, so the two kinds they need are
    /// recovered from the bridge's policy table, where each maps to exactly
    /// one category and action set: only an exhausted plan is `limit` with no
    /// actions (a rate limit offers `retry`, a full context `new_session`), and
    /// only a rejected login is `access`. Any other failure keeps its category.
    ///
    /// Codex often passes the provider's error body through as the title
    /// (`{"type":"error","error":{"message":...},"status":400}`); the
    /// diagnostic then takes the provider's sentence and status from it.
    pub fn from_session_failure(failure: &SessionFailure) -> Self {
        let code = match failure.category.as_str() {
            "limit" if failure.actions.is_empty() => "usageLimitExceeded",
            "access" => AUTH_FAILURE_KIND,
            "" => "session_failure",
            category => category,
        };
        let mut diagnostic = Self {
            message: failure.title.clone(),
            code: Some(code.to_owned()),
            http_status: None,
            reset_at: None,
        };
        if let Ok(body) = serde_json::from_str::<serde_json::Value>(&failure.title)
            && let Some(message) = body
                .pointer("/error/message")
                .or_else(|| body.get("message"))
                .and_then(serde_json::Value::as_str)
        {
            diagnostic.message = message.to_owned();
            diagnostic.http_status = body
                .get("status")
                .and_then(serde_json::Value::as_u64)
                .and_then(|status| u16::try_from(status).ok());
        }
        if let Some(details) = &failure.details {
            diagnostic.message = format!("{}: {details}", diagnostic.message);
        }
        diagnostic
    }
}

/// Codex's error kind for a rejected login, which credential sync matches.
pub const AUTH_FAILURE_KIND: &str = "unauthorized";

/// The kind of error the Codex bridge names beside the JSON-RPC code, in
/// either spelling its versions use: a string such as `usageLimitExceeded`,
/// or an object whose single key is the kind.
pub fn codex_error_kind(data: &serde_json::Value) -> Option<&str> {
    match data
        .get("codexErrorInfo")
        .or_else(|| data.get("codex_error_info"))?
    {
        serde_json::Value::String(kind) => Some(kind),
        serde_json::Value::Object(kind) if kind.len() == 1 => {
            kind.keys().next().map(String::as_str)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Launch finding J-25: Codex reports an exhausted plan as a JSON-RPC
    /// internal error whose data names `codexErrorInfo: usageLimitExceeded`.
    /// That is a usage limit, whatever its sentence says, and the sentence is
    /// the diagnostic's message.
    // Hard-won: cd2f1b6d: Codex quota failures appeared as raw JSON and repeated warnings
    #[test]
    fn a_codex_usage_limit_error_is_a_usage_limit() {
        let error = agent_client_protocol::Error::internal_error().data(serde_json::json!({
            "message": "You’ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 29th, 2026 10:20 PM.",
            "codexErrorInfo": "usageLimitExceeded"
        }));
        let diagnostic = TurnDiagnostic::from_acp(&error);
        assert!(diagnostic.is_usage_limit(), "{diagnostic:?}");
        assert!(
            diagnostic
                .message
                .starts_with("You’ve hit your usage limit")
        );
        assert_eq!(diagnostic.code.as_deref(), Some("usageLimitExceeded"));

        let other = agent_client_protocol::Error::internal_error().data(serde_json::json!({
            "message": "stream disconnected before completion",
            "codexErrorInfo": "responseStreamDisconnected"
        }));
        assert!(!TurnDiagnostic::from_acp(&other).is_usage_limit());
    }
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

    fn record(category: &str, actions: &[&str], title: &str) -> SessionFailure {
        let meta = serde_json::json!({"quota": {}, "jetbrains": {"air": {"version": 1,
            "sessionFailure": {"id": "turn:error", "revision": 1, "category": category,
                "severity": "error", "title": title, "actions": actions}}}});
        SessionFailure::from_meta(meta.as_object()).expect("a failure record")
    }

    /// Issue 1217: the failed turn's record, as codex-acp sends it.
    // Hard-won: 03e1b5ac: Issue 1217 made a failed Codex sub-agent turn count as finished
    #[test]
    fn a_typed_provider_failure_keeps_the_providers_sentence_and_status() {
        let failure = record(
            "service",
            &["retry"],
            r#"{"type":"error","error":{"message":"model 'gpt-6-luna' is not enabled in rustponsesapi","type":"invalid_request_error","param":null,"code":null},"status":400}"#,
        );
        assert!(failure.is_error());
        let diagnostic = TurnDiagnostic::from_session_failure(&failure);
        assert_eq!(
            diagnostic.message,
            "model 'gpt-6-luna' is not enabled in rustponsesapi"
        );
        assert_eq!(diagnostic.http_status, Some(400));
        assert_eq!(diagnostic.code.as_deref(), Some("service"));
        assert!(!diagnostic.is_usage_limit());
    }

    /// The record drops Codex's error kind; the two kinds Mjolnir acts on
    /// come back from the bridge's category and actions.
    // Hard-won: 03e1b5ac: Issue 1217 made a failed Codex sub-agent turn count as finished
    #[test]
    fn a_typed_usage_limit_or_login_failure_keeps_the_kind_mjolnir_acts_on() {
        let exhausted = TurnDiagnostic::from_session_failure(&record(
            "limit",
            &[],
            "You've hit your usage limit.",
        ));
        assert!(exhausted.is_usage_limit());
        let throttled = TurnDiagnostic::from_session_failure(&record(
            "limit",
            &["retry"],
            "Rate limit reached for requests.",
        ));
        assert!(!throttled.is_usage_limit());
        let login = TurnDiagnostic::from_session_failure(&record(
            "access",
            &["login"],
            "Your access token could not be refreshed.",
        ));
        assert!(crate::credentials::turn_diagnostic_reports_auth_failure(
            &login
        ));
    }

    #[test]
    fn a_warning_record_is_not_a_failed_turn_and_other_meta_is_not_a_record() {
        let meta = serde_json::json!({"jetbrains": {"air": {"sessionFailure": {
            "id": "turn:error", "category": "connection", "severity": "warning",
            "title": "Reconnecting... 1/5", "actions": []}}}});
        assert!(
            !SessionFailure::from_meta(meta.as_object())
                .unwrap()
                .is_error()
        );
        let goal = serde_json::json!({"jetbrains": {"air": {"goal": null}}});
        assert!(SessionFailure::from_meta(goal.as_object()).is_none());
        assert!(SessionFailure::from_meta(None).is_none());
    }
}
