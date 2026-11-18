//! Reading the sessions Mjolnir manages out of a container engine's listing.

use super::*;

/// Managed session IDs in a container listing, each with the instance label
/// that created it when the container carries one.
pub fn managed_sessions_from_container_json(
    stdout: &[u8],
) -> Result<Vec<(String, Option<String>)>> {
    let values = serde_json::Deserializer::from_slice(stdout)
        .into_iter::<serde_json::Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse container list JSON")?;
    let mut sessions = Vec::new();
    for value in &values {
        collect_managed_sessions(value, &mut sessions);
    }
    sessions.sort();
    sessions.dedup();
    Ok(sessions)
}

fn collect_managed_sessions(
    value: &serde_json::Value,
    sessions: &mut Vec<(String, Option<String>)>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_managed_sessions(value, sessions);
            }
        }
        serde_json::Value::Object(object) => {
            for label_key in ["Labels", "labels"] {
                if let Some(labels) = object.get(label_key) {
                    let managed =
                        label_value(labels, MANAGED_LABEL).is_some_and(|value| value == "true");
                    if managed && let Some(session) = label_value(labels, SESSION_LABEL) {
                        sessions.push((session, label_value(labels, INSTANCE_LABEL)));
                    }
                }
            }
            for value in object.values() {
                collect_managed_sessions(value, sessions);
            }
        }
        _ => {}
    }
}

fn label_value(labels: &serde_json::Value, key: &str) -> Option<String> {
    match labels {
        serde_json::Value::Object(object) => object.get(key)?.as_str().map(str::to_owned),
        serde_json::Value::String(text) => text
            .split(',')
            .find_map(|label| {
                label
                    .trim()
                    .split_once('=')
                    .filter(|(name, _)| *name == key)
            })
            .map(|(_, value)| value.to_owned()),
        _ => None,
    }
}
