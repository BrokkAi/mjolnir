//! Literal environment values never leave the daemon for a browser.
//!
//! A profile or container runtime may hold an API key as a literal
//! environment value. The browser's copy of a draft carries a marker in its
//! place; the browser may replace the value but never reads it. Saving puts
//! the stored value back first. A reference (`from_env`, `from_secret`) names
//! where a value comes from rather than holding it, so it is shown as is.
use serde_json::{Map, Value, json};

const MARKER_KEY: &str = "mj_hidden_value";

/// Whether `value` stands in for a hidden literal.
pub fn is_marker(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 1 && object.get(MARKER_KEY) == Some(&Value::Bool(true))
    })
}

/// The environment maps of a configuration value, with what owns each.
fn environments(
    value: &mut Value,
) -> impl Iterator<Item = (&'static str, &String, &mut Map<String, Value>)> {
    let root = value.as_object_mut();
    root.into_iter().flat_map(|root| {
        root.iter_mut()
            .filter_map(|(section, entries)| {
                let section = match section.as_str() {
                    "profiles" => "profile",
                    "targets" => "runtime",
                    _ => return None,
                };
                Some((section, entries.as_object_mut()?))
            })
            .flat_map(|(section, entries)| {
                entries.iter_mut().filter_map(move |(id, entry)| {
                    Some((section, id, entry.get_mut("environment")?.as_object_mut()?))
                })
            })
    })
}

/// Replace every literal environment value in a configuration or draft value
/// with the marker.
pub fn hide(value: &mut Value) {
    for (_, _, environment) in environments(value) {
        for entry in environment.values_mut() {
            if entry.is_string() {
                *entry = json!({ MARKER_KEY: true });
            }
        }
    }
}

/// Put back each hidden value from `stored`, the stored shape of the
/// configuration as it is now, and return the values put back, so a caller
/// can keep them out of anything else it sends. A marker whose value the file
/// no longer holds is refused: saving it would write a value nobody entered.
pub fn restore(value: &mut Value, stored: &Value) -> Result<Vec<String>, String> {
    let mut restored = Vec::new();
    for (section, id, environment) in environments(value) {
        let owner = if section == "profile" {
            "profiles"
        } else {
            "targets"
        };
        for (key, entry) in environment.iter_mut() {
            if !is_marker(entry) {
                continue;
            }
            match stored[owner][id.as_str()]["environment"][key.as_str()].as_str() {
                Some(literal) => {
                    restored.push(literal.to_owned());
                    *entry = Value::String(literal.to_owned());
                }
                None => {
                    return Err(format!(
                        "The hidden value of {key} in {section} {id:?} was changed or removed in another client. Reopen Settings, then enter it again."
                    ));
                }
            }
        }
    }
    if contains_marker(value) {
        return Err("A hidden value is not where Settings left it. Reopen Settings.".into());
    }
    Ok(restored)
}

/// `message` with every one of `secrets` replaced, for a failure that might
/// quote a value it was given.
pub fn scrub(message: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(message.to_owned(), |message, secret| {
            message.replace(secret.as_str(), "(hidden)")
        })
}

fn contains_marker(value: &Value) -> bool {
    is_marker(value)
        || match value {
            Value::Object(entries) => entries.values().any(contains_marker),
            Value::Array(entries) => entries.iter().any(contains_marker),
            _ => false,
        }
}
