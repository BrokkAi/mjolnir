use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::goal::GoalState;

/// Adapter settings and projected goal state have independent owners.
/// Keep the existing flat JSON shape for stores, clients and checkpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfiguration {
    #[serde(
        default,
        rename = "mj_goal_state",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal: Option<Box<GoalState>>,
    #[serde(flatten)]
    pub values: BTreeMap<String, Value>,
}

impl SessionConfiguration {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.goal.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_configuration_json_keeps_its_shape_and_separates_owners() {
        let goal: GoalState = serde_json::from_value(json!({
            "known": true,
            "capability": {"version": 1, "controlMethod": "_session/goal", "actions": ["pause", "resume", "clear"]},
            "snapshot": {"objective": "finish", "status": "paused", "tokensUsed": 42, "tokenBudget": 1000},
            "execution": {"version": 1, "status": "idle"}
        })).unwrap();
        for legacy in [
            json!({}),
            json!({"model": "astra", "adapter_extension": {"enabled": true}}),
            json!({"model": "astra", "mj_goal_state": goal}),
        ] {
            let configuration: SessionConfiguration =
                serde_json::from_value(legacy.clone()).unwrap();
            assert_eq!(serde_json::to_value(&configuration).unwrap(), legacy);
            assert!(
                !configuration
                    .values
                    .contains_key(crate::goal::PROJECTION_KEY)
            );
            assert_eq!(
                configuration.goal.is_some(),
                legacy.get("mj_goal_state").is_some()
            );
            let legacy_reader: BTreeMap<String, Value> =
                serde_json::from_str(&serde_json::to_string(&configuration).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(legacy_reader).unwrap(), legacy);
        }
    }
}
