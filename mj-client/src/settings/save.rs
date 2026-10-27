//! Writing a settings draft to the configuration file. Every surface saves
//! through here, so a save means the same thing wherever it was made.
use anyhow::{Context, Result};
use mj_core::config::Config;
use mj_core::state::State;
use serde_json::Value;
use std::collections::BTreeSet;

/// Save the edit from `original` to `updated` into the configuration at
/// `path`. Fields the edit left alone keep whatever the file holds now, so
/// another client's change to them survives; a field both changed is
/// refused.
pub fn save_at(
    path: &std::path::Path,
    original: &str,
    updated: &str,
    state: &State,
) -> Result<Config> {
    let original: serde_json::Value = serde_json::from_str(original)?;
    let updated_config: Config = serde_json::from_str(updated)?;
    updated_config.validate()?;
    let updated = serde_json::to_value(&updated_config)?;
    // A profile this edit adds or changes must be usable, so Setup reports a
    // missing key while it is open. Any other profile's readiness is not this
    // save's business: refusing over it would block every unrelated setting.
    for (id, profile) in updated_config.enabled_profiles() {
        if original["profiles"][id] != updated["profiles"][id] {
            profile.ensure_ready(id)?;
        }
    }
    Config::update_to(path, |config| {
        // The editor includes implicit local defaults. Treat those same defaults
        // as the merge base when they have not been written to disk yet.
        let defaults = Config::default().with_local_targets();
        // Compare in the stored shape the editor works in, where a runtime
        // names its machine, rather than in the fused in-memory shape.
        let stored_defaults = serde_json::to_value(&defaults)?;
        let mut implicit = Vec::new();
        for (id, target) in defaults.targets {
            if !config.targets.contains_key(&id)
                && original["targets"][&id] == stored_defaults["targets"][&id]
            {
                implicit.push((id.clone(), target.clone()));
                config.targets.insert(id, target);
            }
        }
        let current = serde_json::to_value(&*config)?;
        let merged = merge_edit(Some(&original), Some(&updated), Some(&current), "Setup")?
            .context("setup cannot remove the configuration")?;
        let mut next: Config = serde_json::from_value(merged)?;
        state.validate_setup_update(config, &next)?;
        // The defaults were only the merge base. A built-in target the edit
        // left as it was stays out of the file, as `Config::update` does, so
        // an unrelated save does not turn it into one the user configured.
        for (id, target) in implicit {
            // A built-in runtime the edit switched off is written with it.
            if next.targets.get(&id) == Some(&target) && !next.disabled_targets.contains(&id) {
                next.targets.remove(&id);
            }
        }
        *config = next;
        Ok(())
    })
    .map(|(config, ())| config)
}

/// Apply only fields the dialog changed; refuse conflicting concurrent edits.
pub fn merge_edit(
    original: Option<&serde_json::Value>,
    updated: Option<&serde_json::Value>,
    current: Option<&serde_json::Value>,
    path: &str,
) -> Result<Option<serde_json::Value>> {
    if original == updated || updated == current {
        return Ok(current.cloned());
    }
    if original == current {
        return Ok(updated.cloned());
    }
    if updated.is_some_and(Value::is_object)
        && original.is_none_or(Value::is_object)
        && current.is_some_and(Value::is_object)
    {
        let keys = original
            .into_iter()
            .chain(updated)
            .chain(current)
            .flat_map(|value| value.as_object().unwrap().keys())
            .collect::<BTreeSet<_>>();
        let mut merged = serde_json::Map::new();
        for key in keys {
            if let Some(value) = merge_edit(
                original.and_then(|v| v.get(key)),
                updated.and_then(|v| v.get(key)),
                current.and_then(|v| v.get(key)),
                &format!("{path} / {key}"),
            )? {
                merged.insert(key.clone(), value);
            }
        }
        return Ok(Some(Value::Object(merged)));
    }
    Err(EditConflict {
        path: path.to_owned(),
    }
    .into())
}

/// A field this save and another client both changed. Nothing is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditConflict {
    pub path: String,
}

impl std::fmt::Display for EditConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed in another client. Reopen Settings to edit the latest value.",
            self.path
        )
    }
}

impl std::error::Error for EditConflict {}
