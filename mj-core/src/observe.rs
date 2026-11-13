//! The versioned wire contract Mjolnir uses to publish sessions to Brokk
//! Observe.
//!
//! Observe accepts one JSON envelope at a time or newline-delimited batches.
//! `event` is retained as JSON because ACP harnesses expose tool data in a
//! common shape whose individual fields can grow without changing this
//! transport contract. The stable identity lives in `event.session_id` and
//! `meta.event_id`; Observe de-duplicates on that pair.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use std::fmt;

use crate::hex::lower_hex;

/// The transport version understood by this module.
pub const OBSERVE_ENVELOPE_VERSION: u8 = 1;
/// The collector name visible to Brokk Observe.
pub const OBSERVE_COLLECTOR: &str = "mjolnir";
const DEFAULT_POLICY_TIMEOUT_SECS: u64 = 3;

/// Configuration for synchronous Brokk Observe policy decisions.
///
/// The bearer token stays in the worker launch configuration. It is never
/// added to the harness environment.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveEnforcementConfig {
    pub url: String,
    pub token: String,
    #[serde(default = "default_policy_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub fail_closed: bool,
}

impl fmt::Debug for ObserveEnforcementConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObserveEnforcementConfig")
            .field("url", &self.url)
            .field("token", &"[REDACTED]")
            .field("timeout_ms", &self.timeout_ms)
            .field("fail_closed", &self.fail_closed)
            .finish()
    }
}

const fn default_policy_timeout_ms() -> u64 {
    DEFAULT_POLICY_TIMEOUT_SECS * 1_000
}

impl ObserveEnforcementConfig {
    /// Read the same variables the Phase 1 collector and vendor plugin use.
    /// Enforcement is absent unless `OBSERVE_ENFORCE` is true.
    pub fn from_environment() -> Result<Option<Self>> {
        if !environment_true("OBSERVE_ENFORCE") {
            return Ok(None);
        }
        let url = std::env::var("OBSERVE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("OBSERVE_ENFORCE=true requires OBSERVE_URL"))?;
        let parsed =
            url::Url::parse(&url).map_err(|error| anyhow::anyhow!("parse OBSERVE_URL: {error}"))?;
        ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "OBSERVE_URL must use http or https"
        );
        let token = std::env::var("OBSERVE_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("OBSERVE_ENFORCE=true requires OBSERVE_TOKEN"))?;
        let timeout_ms = match std::env::var("OBSERVE_TIMEOUT") {
            Ok(value) => {
                let seconds = value.parse::<u64>().map_err(|_| {
                    anyhow::anyhow!("OBSERVE_TIMEOUT must be a positive whole number of seconds")
                })?;
                ensure!(seconds > 0, "OBSERVE_TIMEOUT must be at least one second");
                seconds * 1_000
            }
            Err(_) => default_policy_timeout_ms(),
        };
        Ok(Some(Self {
            url: url.trim_end_matches('/').to_owned(),
            token,
            timeout_ms,
            fail_closed: environment_true("OBSERVE_FAIL_CLOSED"),
        }))
    }

    /// The only harness that currently asks Mjolnir before every command,
    /// write, and MCP call. Other harnesses may auto-approve tools inside
    /// their own sandbox, which leaves no pre-execution veto for Mjolnir.
    pub fn supports_harness(self, harness: crate::config::HarnessKind) -> Result<Self> {
        ensure!(
            harness == crate::config::HarnessKind::Dsh,
            "Observe enforcement currently supports DSH only; {} does not expose a pre-execution permission request for every tool",
            harness.display_name()
        );
        Ok(self)
    }
}

fn environment_true(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// One event sent from Mjolnir to Brokk Observe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveEnvelope {
    pub v: u8,
    pub meta: ObserveMeta,
    pub event: Value,
}

/// Metadata that belongs to the sender rather than one agent-specific hook.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveMeta {
    /// Stable within the session. A retried event reuses this id.
    pub event_id: String,
    /// Event creation time in Unix epoch milliseconds.
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// The logical agent product. This is Mjolnir even when a harness runs
    /// underneath it.
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// The component that produced the envelopes.
    #[serde(default = "default_collector")]
    pub collector: String,
    /// Explicit extension used by newer Observe servers to distinguish the
    /// orchestration layer from the selected harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
    /// The selected ACP product, such as `claude`, `muse`, or `kimi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// `local`, `ci`, `cloud`, `container`, or a future runtime label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collector_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<ObserveGitMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<ObserveCommit>,
}

/// Repository context attached to session boundaries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveGitMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// One commit selected for Observe's session-to-source linkage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveCommit {
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

fn default_agent() -> String {
    OBSERVE_COLLECTOR.to_owned()
}

fn default_collector() -> String {
    OBSERVE_COLLECTOR.to_owned()
}

impl ObserveEnvelope {
    /// Check the fields Observe uses as keys before an event enters a spool.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.v == OBSERVE_ENVELOPE_VERSION,
            "unsupported Observe envelope version {}",
            self.v
        );
        validate_id("meta.event_id", &self.meta.event_id)?;
        ensure!(
            !self.meta.collector.trim().is_empty(),
            "meta.collector must not be empty"
        );
        let session_id = self
            .event
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        validate_id("event.session_id", session_id)?;
        let event_name = self
            .event
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            !event_name.trim().is_empty(),
            "event.hook_event_name must not be empty"
        );
        Ok(())
    }

    /// One newline-terminated line, suitable for Observe's NDJSON batch
    /// endpoint.
    pub fn to_ndjson(&self) -> Result<String> {
        self.validate()?;
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

fn validate_id(field: &str, value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{field} must not be empty");
    ensure!(value.len() <= 128, "{field} is longer than 128 bytes");
    Ok(())
}

/// Derive the event id for one stable transcript position. The ordinal
/// separates multiple Observe events built from the same logical item, such
/// as a tool call and its result.
pub fn event_id(session_id: &str, position: u64, kind: &str, ordinal: u8) -> String {
    let digest = lower_hex(sha2::Sha256::digest(
        format!("mjolnir-observe-v1\0{session_id}\0{position}\0{kind}\0{ordinal}").as_bytes(),
    ));
    format!("mj-{position}-{digest}")
}

/// Stable identity for the policy question about one ACP tool call.
///
/// The native session is part of the identity because a harness can run
/// native child sessions under one Mjolnir session, and their tool-call ids
/// occupy separate namespaces.
pub fn tool_call_event_id(session_id: &str, native_session_id: &str, tool_call_id: &str) -> String {
    let digest = lower_hex(sha2::Sha256::digest(
        format!("mjolnir-observe-policy-v1\0{session_id}\0{native_session_id}\0{tool_call_id}")
            .as_bytes(),
    ));
    format!("mj-policy-{digest}")
}

/// Normalize ACP's portable tool categories to the names Observe's existing
/// detectors and policies understand.
pub fn canonical_tool_name(kind: &str, title: &str) -> String {
    match kind {
        "execute" => "Bash".into(),
        "read" => "Read".into(),
        "edit" => {
            if title.to_ascii_lowercase().contains("write") {
                "Write".into()
            } else {
                "Edit".into()
            }
        }
        "fetch" => "WebFetch".into(),
        "search" => "Grep".into(),
        other if title.is_empty() => other.into(),
        _ => title.into(),
    }
}

/// Create the `SessionStart` payload shared by every harness.
pub fn session_start_event(
    session_id: &str,
    harness: &str,
    model: Option<&str>,
    cwd: Option<&str>,
) -> Value {
    json!({
        "hook_event_name": "SessionStart",
        "session_id": session_id,
        "source": OBSERVE_COLLECTOR,
        "harness": harness,
        "model": model,
        "cwd": cwd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> ObserveEnvelope {
        ObserveEnvelope {
            v: OBSERVE_ENVELOPE_VERSION,
            meta: ObserveMeta {
                event_id: event_id("session-1", 1, "SessionStart", 0),
                ts: 10,
                agent: OBSERVE_COLLECTOR.into(),
                collector: OBSERVE_COLLECTOR.into(),
                orchestrator: Some(OBSERVE_COLLECTOR.into()),
                harness: Some("muse".into()),
                collector_version: Some(env!("CARGO_PKG_VERSION").into()),
                ..Default::default()
            },
            event: session_start_event("session-1", "muse", Some("muse-wonder"), Some("/work")),
        }
    }

    #[test]
    fn envelope_serializes_as_one_complete_line() {
        let line = envelope().to_ndjson().unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let decoded: ObserveEnvelope = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(decoded.meta.harness.as_deref(), Some("muse"));
        assert_eq!(decoded.event["session_id"], "session-1");
    }

    #[test]
    fn event_ids_are_stable_and_distinguish_event_kinds() {
        let first = event_id("session-1", 7, "PreToolUse", 0);
        assert_eq!(first, event_id("session-1", 7, "PreToolUse", 0));
        assert_ne!(first, event_id("session-1", 7, "PostToolUse", 0));
        assert_ne!(first, event_id("session-2", 7, "PreToolUse", 0));
        assert!(first.len() <= 128);
    }

    #[test]
    fn policy_ids_distinguish_native_children_with_the_same_tool_call_id() {
        let parent = tool_call_event_id("logical", "native-parent", "call-1");
        assert_eq!(
            parent,
            tool_call_event_id("logical", "native-parent", "call-1")
        );
        assert_ne!(
            parent,
            tool_call_event_id("logical", "native-child", "call-1")
        );
        assert_ne!(
            parent,
            tool_call_event_id("other-logical", "native-parent", "call-1")
        );
    }

    #[test]
    fn validation_rejects_missing_keys() {
        let mut value = envelope();
        value.meta.event_id.clear();
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("event_id")
        );

        let mut value = envelope();
        value.event["session_id"] = String::new().into();
        assert!(
            value
                .validate()
                .unwrap_err()
                .to_string()
                .contains("session_id")
        );
    }

    #[test]
    fn enforcement_is_limited_to_harnesses_with_a_complete_permission_gate() {
        let config = ObserveEnforcementConfig {
            url: "https://observe.example".into(),
            token: "token".into(),
            timeout_ms: 3_000,
            fail_closed: true,
        };
        assert!(
            config
                .clone()
                .supports_harness(crate::config::HarnessKind::Dsh)
                .is_ok()
        );
        for harness in [
            crate::config::HarnessKind::Codex,
            crate::config::HarnessKind::Claude,
            crate::config::HarnessKind::Kimi,
            crate::config::HarnessKind::Grok,
            crate::config::HarnessKind::Muse,
            crate::config::HarnessKind::OpenCode,
            crate::config::HarnessKind::Antigravity,
        ] {
            let error = config
                .clone()
                .supports_harness(harness)
                .unwrap_err()
                .to_string();
            assert!(error.contains("supports DSH only"), "{harness:?}: {error}");
            assert!(
                error.contains(harness.display_name()),
                "{harness:?}: {error}"
            );
        }
    }
}
