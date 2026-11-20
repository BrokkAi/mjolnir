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

/// Replace literal credentials in a configuration or draft value
/// with the marker.
pub fn hide(value: &mut Value) {
    if let Some(key) = value
        .pointer_mut("/jev/fastino_key")
        .filter(|key| key.is_string())
    {
        *key = json!({ MARKER_KEY: true });
    }
    for (_, _, environment) in environments(value) {
        for entry in environment.values_mut() {
            if entry.is_string() {
                *entry = json!({ MARKER_KEY: true });
            }
        }
    }
}

/// Hide only literals that were markers in the browser's incoming draft.
/// Values newly entered by the user must survive a server-side page action.
pub fn rehide_restored(value: &mut Value, incoming: &Value) {
    if is_marker(&incoming["jev"]["fastino_key"])
        && let Some(key) = value
            .pointer_mut("/jev/fastino_key")
            .filter(|key| key.is_string())
    {
        *key = json!({ MARKER_KEY: true });
    }
    for (section, id, environment) in environments(value) {
        let owner = if section == "profile" {
            "profiles"
        } else {
            "targets"
        };
        for (key, entry) in environment.iter_mut() {
            if is_marker(&incoming[owner][id.as_str()]["environment"][key.as_str()]) {
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
    if let Some(key) = value
        .pointer_mut("/jev/fastino_key")
        .filter(|key| is_marker(key))
    {
        let literal = stored["jev"]["fastino_key"].as_str().ok_or(
            "The hidden Fastino key was changed or removed in another client. Reopen Settings, then enter it again.",
        )?;
        restored.push(literal.to_owned());
        *key = literal.into();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fastino_key_is_hidden_and_typed_references_survive_a_settings_round_trip() {
        use crate::settings::{draft, save, view};
        use mj_core::config::Config;

        let config = serde_json::from_value::<Config>(json!({
            "version": mj_core::config::CONFIG_VERSION, "jev": { "fastino_key": "personal-key" }
        }))
        .unwrap();
        let (stored, mut shown) = draft::open(&config);
        hide(&mut shown);
        assert!(is_marker(&shown["jev"]["fastino_key"]));
        let mut restored = shown.clone();
        assert_eq!(restore(&mut restored, &stored).unwrap(), ["personal-key"]);
        rehide_restored(&mut restored, &shown);
        assert_eq!(restored, shown);

        let path = vec!["jev".into(), "fastino_key".into()];
        let mut cleared = shown.clone();
        let result = view::apply(
            &mut cleared,
            &path,
            view::SettingsEdit::Remove { path: path.clone() },
        )
        .unwrap();
        assert_eq!(result.page, ["jev"]);
        assert!(
            draft::config_from_draft(cleared)
                .unwrap()
                .jev
                .fastino_key
                .is_none()
        );
        view::apply(
            &mut shown,
            &path,
            view::SettingsEdit::Set {
                path: path.clone(),
                text: Some(r#"{ from_secret = "FASTINO" }"#.into()),
                choice: None,
            },
        )
        .unwrap();
        let page = view::render_page(&shown, &path).unwrap();
        assert!(matches!(
            page.groups[0].rows[0].control,
            view::SettingsControl::Text { .. }
        ));
        let updated = draft::config_from_draft(shown).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        config.save_to(&config_path).unwrap();
        let original = stored.to_string();
        let updated = serde_json::to_string(&updated).unwrap();
        let state = mj_core::state::State::default();
        let error = save::save_at(&config_path, &original, &updated, &state).unwrap_err();
        assert!(error.to_string().contains("jev.fastino_key"));
        let secrets_path = directory.path().join("secrets.toml");
        std::fs::write(&secrets_path, "FASTINO = \"private-marker\" invalid").unwrap();
        let error = save::save_at(&config_path, &original, &updated, &state).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("secrets.toml"));
        assert!(chain.contains("at byte"));
        assert!(!chain.contains("private-marker"));
        std::fs::write(&secrets_path, "FASTINO = \"my-provider-key\"").unwrap();
        let mut saved = save::save_at(&config_path, &original, &updated, &state).unwrap();
        assert_eq!(
            serde_json::to_value(&saved).unwrap()["jev"]["fastino_key"],
            json!({ "from_secret": "FASTINO" })
        );

        saved.jev.fastino_key = Some(mj_core::config::EnvironmentValue::Literal(
            "old-key ".into(),
        ));
        saved.save_to(&config_path).unwrap();
        let before = serde_json::to_string(&saved).unwrap();
        saved.jev.enabled = false;
        let disabled = serde_json::to_string(&saved).unwrap();
        let disabled = save::save_at(&config_path, &before, &disabled, &state).unwrap();
        assert_eq!(disabled.jev, saved.jev);
    }

    #[test]
    fn page_action_preserves_typed_environment_values_through_save() {
        let stored = json!({
            "profiles": {"old": {"environment": {"TOKEN": "old-secret"}}},
            "targets": {"box": {"environment": {"KEY": "old-runtime"}}}
        });
        let mut incoming = stored.clone();
        hide(&mut incoming);
        incoming["profiles"]["old"]["environment"]["TOKEN"] = json!("edited-secret");
        incoming["profiles"]["old"]["environment"]["NEW"] = json!("new-secret");
        incoming["targets"]["box"]["environment"]["NEW"] = json!("new-runtime");
        let mut action_result = incoming.clone();
        restore(&mut action_result, &stored).unwrap();
        rehide_restored(&mut action_result, &incoming);
        assert_eq!(
            action_result["profiles"]["old"]["environment"]["TOKEN"],
            "edited-secret"
        );
        assert_eq!(
            action_result["profiles"]["old"]["environment"]["NEW"],
            "new-secret"
        );
        assert!(is_marker(
            &action_result["targets"]["box"]["environment"]["KEY"]
        ));
        restore(&mut action_result, &stored).unwrap();
        assert_eq!(
            action_result["targets"]["box"]["environment"]["NEW"],
            "new-runtime"
        );
        assert_eq!(
            action_result["targets"]["box"]["environment"]["KEY"],
            "old-runtime"
        );
    }
}
