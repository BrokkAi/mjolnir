//! Runtime Brokk Observe policy decisions for the common ACP permission gate.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mj_core::config::HarnessKind;
use mj_core::observe::{ObserveEnforcementConfig, canonical_tool_name, tool_call_event_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    Allow,
    Deny,
    Ask,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub decision: PolicyAction,
    #[serde(default)]
    pub policy_id: Option<i64>,
    #[serde(default)]
    pub policy_name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

pub struct PolicyTool<'a> {
    pub native_session_id: &'a str,
    pub harness: HarnessKind,
    pub tool_call_id: &'a str,
    pub tool_title: &'a str,
    pub tool_kind: Option<&'a str>,
    pub tool_input: Option<&'a Value>,
    pub cwd: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDisposition {
    Continue,
    Deny(String),
    Ask(String),
    Warn(String),
    Unavailable { reason: String, deny: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyAcknowledgement {
    event_id: String,
    decision: PolicyAction,
    outcome: Option<String>,
}

impl PolicyAcknowledgement {
    pub fn record_outcome(&mut self, outcome: impl Into<String>) {
        self.outcome = Some(outcome.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyCheck {
    pub disposition: PolicyDisposition,
    pub acknowledgement: Option<PolicyAcknowledgement>,
}

impl PolicyDecision {
    pub fn reason(&self) -> String {
        match (&self.policy_name, &self.message) {
            (Some(name), Some(message)) => format!("[Brokk Observe policy: {name}] {message}"),
            (Some(name), None) => format!("[Brokk Observe policy: {name}]"),
            (None, Some(message)) => message.clone(),
            (None, None) => "Brokk Observe policy decision".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyClient {
    config: ObserveEnforcementConfig,
    session_id: String,
    http: reqwest::Client,
}

impl PolicyClient {
    pub fn new(config: ObserveEnforcementConfig, session_id: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .context("build Brokk Observe policy client")?;
        Ok(Self {
            config,
            session_id: session_id.into(),
            http,
        })
    }

    pub fn failure_action(&self) -> PolicyAction {
        if self.config.fail_closed {
            PolicyAction::Deny
        } else {
            PolicyAction::Allow
        }
    }

    pub async fn evaluate(&self, tool: PolicyTool<'_>) -> Result<PolicyDecision> {
        let event_id =
            tool_call_event_id(&self.session_id, tool.native_session_id, tool.tool_call_id);
        let tool_name = canonical_tool_name(tool.tool_kind.unwrap_or("other"), tool.tool_title);
        let body = json!({
            "session_id": self.session_id,
            "event_id": event_id,
            "tool_name": tool_name,
            "tool_input": tool.tool_input,
            "tool_use_id": tool.tool_call_id,
            "cwd": tool.cwd,
            "harness": tool.harness.id(),
            "native_session_id": tool.native_session_id,
        });
        let response = self
            .http
            .post(format!("{}/api/v1/policy/evaluate", self.config.url))
            .bearer_auth(&self.config.token)
            .json(&body)
            .send()
            .await
            .context("ask Brokk Observe for a policy decision")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "Brokk Observe policy request failed with HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let decision: PolicyDecision = response
            .json()
            .await
            .context("decode Brokk Observe policy decision")?;
        Ok(decision)
    }

    pub fn check(
        &self,
        result: Result<PolicyDecision>,
        native_session_id: &str,
        tool_call_id: &str,
    ) -> PolicyCheck {
        match result {
            Ok(decision) => {
                let action = decision.decision;
                let disposition = match action {
                    PolicyAction::Allow => PolicyDisposition::Continue,
                    PolicyAction::Deny => PolicyDisposition::Deny(decision.reason()),
                    PolicyAction::Ask => PolicyDisposition::Ask(decision.reason()),
                    PolicyAction::Warn => PolicyDisposition::Warn(decision.reason()),
                };
                let acknowledgement =
                    (action != PolicyAction::Allow).then(|| PolicyAcknowledgement {
                        event_id: tool_call_event_id(
                            &self.session_id,
                            native_session_id,
                            tool_call_id,
                        ),
                        decision: action,
                        outcome: match action {
                            PolicyAction::Deny => Some("rejected".into()),
                            PolicyAction::Warn => Some("warned".into()),
                            PolicyAction::Allow | PolicyAction::Ask => None,
                        },
                    });
                PolicyCheck {
                    disposition,
                    acknowledgement,
                }
            }
            Err(error) => PolicyCheck {
                disposition: PolicyDisposition::Unavailable {
                    reason: format!("{error:#}"),
                    deny: self.config.fail_closed,
                },
                acknowledgement: None,
            },
        }
    }

    /// Record that Mjolnir delivered the decision to the harness. This is a
    /// separate request from evaluation: Observe must not claim a decision
    /// was enforced when the response was lost before it could be applied.
    pub async fn acknowledge(&self, acknowledgement: &PolicyAcknowledgement) -> Result<()> {
        let outcome = acknowledgement
            .outcome
            .as_deref()
            .context("policy acknowledgement has no permission outcome")?;
        let response = self
            .http
            .post(format!("{}/api/v1/policy/acknowledge", self.config.url))
            .bearer_auth(&self.config.token)
            .json(&json!({
                "session_id": self.session_id,
                "event_id": acknowledgement.event_id,
                "decision": acknowledgement.decision,
                "outcome": outcome,
            }))
            .send()
            .await
            .context("acknowledge applied Brokk Observe policy decision")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "Brokk Observe acknowledgement failed with HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }

    pub fn acknowledge_applied(
        &self,
        acknowledgement: Option<PolicyAcknowledgement>,
        tool_call_id: String,
    ) {
        let Some(acknowledgement) = acknowledgement else {
            return;
        };
        let client = self.clone();
        tokio::spawn(async move {
            if let Err(error) = client.acknowledge(&acknowledgement).await {
                tracing::warn!(
                    tool_call_id,
                    event_id = acknowledgement.event_id,
                    "policy decision was applied but its Observe acknowledgement failed: {error:#}"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::observe::ObserveEnforcementConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn config_debug_never_prints_the_bearer_token() {
        let config = ObserveEnforcementConfig {
            url: "https://observe.example".into(),
            token: "bko_ingest_secret".into(),
            timeout_ms: 3_000,
            fail_closed: true,
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("bko_ingest_secret"));
    }

    #[test]
    fn policy_reason_keeps_the_policy_identity() {
        let decision = PolicyDecision {
            decision: PolicyAction::Deny,
            policy_id: Some(7),
            policy_name: Some("No secrets".into()),
            message: Some("Credential stores are blocked".into()),
        };
        assert_eq!(
            decision.reason(),
            "[Brokk Observe policy: No secrets] Credential stores are blocked"
        );
    }

    #[test]
    fn policy_failure_defaults_open_and_can_be_configured_closed() {
        let config = |fail_closed| ObserveEnforcementConfig {
            url: "http://127.0.0.1".into(),
            token: "token".into(),
            timeout_ms: 100,
            fail_closed,
        };
        assert_eq!(
            PolicyClient::new(config(false), "session-1")
                .unwrap()
                .failure_action(),
            PolicyAction::Allow
        );
        assert_eq!(
            PolicyClient::new(config(true), "session-1")
                .unwrap()
                .failure_action(),
            PolicyAction::Deny
        );
    }

    #[test]
    fn every_policy_action_has_one_handler_disposition() {
        let client = PolicyClient::new(
            ObserveEnforcementConfig {
                url: "http://127.0.0.1".into(),
                token: "token".into(),
                timeout_ms: 100,
                fail_closed: false,
            },
            "session-1",
        )
        .unwrap();
        for (action, expected) in [
            (PolicyAction::Allow, PolicyDisposition::Continue),
            (
                PolicyAction::Deny,
                PolicyDisposition::Deny("[Brokk Observe policy: P] deny".into()),
            ),
            (
                PolicyAction::Ask,
                PolicyDisposition::Ask("[Brokk Observe policy: P] ask".into()),
            ),
            (
                PolicyAction::Warn,
                PolicyDisposition::Warn("[Brokk Observe policy: P] warn".into()),
            ),
        ] {
            let check = client.check(
                Ok(PolicyDecision {
                    decision: action,
                    policy_id: Some(1),
                    policy_name: Some("P".into()),
                    message: Some(action_name(action).into()),
                }),
                "native-session",
                "call-1",
            );
            assert_eq!(check.disposition, expected);
            assert_eq!(
                check.acknowledgement.is_some(),
                action != PolicyAction::Allow
            );
        }
    }

    fn action_name(action: PolicyAction) -> &'static str {
        match action {
            PolicyAction::Allow => "allow",
            PolicyAction::Deny => "deny",
            PolicyAction::Ask => "ask",
            PolicyAction::Warn => "warn",
        }
    }

    #[tokio::test]
    async fn client_sends_the_normalized_tool_and_parses_the_decision() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk[..read]);
                let Some(headers_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap();
                if bytes.len() >= headers_end + 4 + content_length {
                    let body: Value = serde_json::from_slice(
                        &bytes[headers_end + 4..headers_end + 4 + content_length],
                    )
                    .unwrap();
                    assert!(headers.contains("/api/v1/policy/evaluate"));
                    assert!(headers.contains("authorization: Bearer test-token"));
                    assert_eq!(body["tool_name"], "Bash");
                    assert_eq!(
                        body["tool_input"]["command"],
                        "git push --force origin main"
                    );
                    assert_eq!(body["harness"], "dsh");
                    assert_eq!(body["session_id"], "session-1");
                    assert_eq!(body["native_session_id"], "native-session");
                    let reply = json!({
                        "decision": "deny",
                        "policy_id": 7,
                        "policy_name": "Protected branch",
                        "message": "Force-push denied"
                    })
                    .to_string();
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                                reply.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    break;
                }
            }
        });
        let client = PolicyClient::new(
            ObserveEnforcementConfig {
                url: format!("http://{address}"),
                token: "test-token".into(),
                timeout_ms: 3_000,
                fail_closed: false,
            },
            "session-1",
        )
        .unwrap();
        let input = json!({"command": "git push --force origin main"});
        let decision = client
            .evaluate(PolicyTool {
                native_session_id: "native-session",
                harness: HarnessKind::Dsh,
                tool_call_id: "call-1",
                tool_title: "Run command",
                tool_kind: Some("execute"),
                tool_input: Some(&input),
                cwd: "/work",
            })
            .await
            .unwrap();
        assert_eq!(decision.decision, PolicyAction::Deny);
        assert_eq!(decision.policy_name.as_deref(), Some("Protected branch"));
        server.await.unwrap();
    }
}
