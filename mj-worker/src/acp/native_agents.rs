//! Interpret the negotiated native child-session extension before ACP v1 decoding.
use anyhow::{Context, Result, ensure};
use mj_core::native_agent::{NativeAgentCapabilities, NativeAgentEvent};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

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
