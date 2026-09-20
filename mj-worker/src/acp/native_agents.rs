//! Interpret the negotiated native child-session extension before ACP v1 decoding.
use anyhow::{Context, Result, ensure};
use mj_core::native_agent::{NativeAgentCapabilities, NativeAgentEvent};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Inventory {
    agents: Vec<mj_core::native_agent::NativeAgentAvailabilityReport>,
    complete: bool,
}

fn decode_inventory(value: Value) -> Result<Inventory> {
    let inventory: Inventory = serde_json::from_value(value)?;
    let mut identities = BTreeSet::new();
    for agent in &inventory.agents {
        ensure!(
            !agent.session_id.is_empty() && identities.insert(&agent.session_id),
            "availability inventory has empty or duplicate identities"
        );
        ensure!(
            agent.stable_id.as_ref().is_none_or(|id| !id.is_empty()),
            "availability inventory has an empty stable identity"
        );
    }
    Ok(inventory)
}

/// Only called after the adapter explicitly negotiates this read-only extension.
pub(super) async fn refresh_availability(
    connection: &super::ConnectionTo<super::Agent>,
    session_id: &super::SessionId,
    events: &super::mpsc::Sender<super::RuntimeEvent>,
) -> Result<()> {
    let request = super::UntypedMessage {
        method: "_session/subagents/availability".into(),
        params: serde_json::json!({ "sessionId": session_id }),
    };
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connection.send_request(request).block_task(),
    )
    .await;
    let inventory = match response {
        Ok(Ok(value)) => decode_inventory(value).map_err(|e| e.to_string()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("availability query timed out".into()),
    };
    let event = match inventory {
        Ok(inventory) => super::RuntimeEvent::NativeAgent {
            event: NativeAgentEvent::Availability {
                reports: inventory.agents,
                complete: inventory.complete,
            },
        },
        Err(error) => super::RuntimeEvent::Warning {
            message: format!("Subagent availability remains unknown: {error}"),
        },
    };
    super::emit_runtime_event(events, event).await
}

#[derive(Default)]
pub(super) struct NativeAgentRouter {
    children: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(tag = "sessionUpdate")]
enum Lifecycle {
    #[serde(rename = "subagent_spawned", rename_all = "camelCase")]
    Spawned {
        subagent_session_id: String,
        name: String,
        task: String,
        #[serde(default)]
        capabilities: NativeAgentCapabilities,
    },
    #[serde(rename = "subagent_state_update", rename_all = "camelCase")]
    State {
        subagent_session_id: String,
        state: mj_core::native_agent::NativeAgentState,
    },
}

impl NativeAgentRouter {
    pub fn is_child(&self, id: &str) -> bool {
        self.children.contains(id)
    }
    pub fn route(&mut self, addressed: &str, update: &Value) -> Result<Option<NativeAgentEvent>> {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("subagent_spawned" | "subagent_state_update") => {
                let event: Lifecycle = serde_json::from_value(update.clone())
                    .context("decode native agent lifecycle")?;
                Ok(Some(match event {
                    Lifecycle::Spawned {
                        subagent_session_id,
                        name,
                        task,
                        capabilities,
                    } => {
                        ensure!(
                            !subagent_session_id.is_empty() && subagent_session_id != addressed,
                            "native agent has an empty or self-referential identity"
                        );
                        let parent_session_id = self
                            .children
                            .contains(addressed)
                            .then(|| addressed.to_owned());
                        self.children.insert(subagent_session_id.clone());
                        NativeAgentEvent::Spawned {
                            session_id: subagent_session_id,
                            parent_session_id,
                            name,
                            task,
                            capabilities,
                        }
                    }
                    Lifecycle::State {
                        subagent_session_id,
                        state,
                    } => {
                        ensure!(
                            self.children.contains(&subagent_session_id),
                            "native agent state precedes its spawn"
                        );
                        NativeAgentEvent::State {
                            session_id: subagent_session_id,
                            state,
                        }
                    }
                }))
            }
            _ if self.children.contains(addressed) => Ok(Some(NativeAgentEvent::Update {
                session_id: addressed.to_owned(),
                update: Box::new(
                    serde_json::from_value(update.clone()).context("decode native child update")?,
                ),
            })),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn availability_inventory_requires_explicit_evidence_and_unique_identities() {
        let agent = json!({"session_id":"child", "availability":"available", "stable_id":"stable"});
        assert!(decode_inventory(json!({"agents":[agent.clone()]})).is_err());
        assert!(
            decode_inventory(json!({"agents":[agent.clone(), agent.clone()], "complete":true}))
                .is_err()
        );
        let inventory = decode_inventory(json!({"agents":[agent], "complete":false})).unwrap();
        assert!(!inventory.complete);
        assert_eq!(inventory.agents[0].stable_id.as_deref(), Some("stable"));
    }

    #[test]
    fn routes_nested_children_without_consuming_parent_output() {
        let mut router = NativeAgentRouter::default();
        let spawn = |id| {
            json!({"sessionUpdate":"subagent_spawned", "subagentSessionId":id,
            "name":"review", "task":"review source", "capabilities":{}})
        };
        assert!(matches!(
            router.route("root", &spawn("child")).unwrap(),
            Some(NativeAgentEvent::Spawned {
                parent_session_id: None,
                ..
            })
        ));
        assert!(
            matches!(router.route("child", &spawn("grandchild")).unwrap(), Some(NativeAgentEvent::Spawned { parent_session_id: Some(parent), .. }) if parent == "child")
        );
        let message = json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"hello"}});
        assert!(router.route("root", &message).unwrap().is_none());
        assert!(
            matches!(router.route("grandchild", &message).unwrap(), Some(NativeAgentEvent::Update { session_id, .. }) if session_id == "grandchild")
        );
    }
}
