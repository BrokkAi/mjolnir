//! How an edit changes a settings draft. Every surface applies edits through
//! these functions, so a value means the same thing whichever one typed it.
use super::draft::{config_from_draft, page_path, pointer, storage_path};
use super::page::hidden_key;
use super::page::is_collection;
use super::schema;
use serde_json::{Value, json};

/// What a person gave a setting: text typed into a field, or one of the
/// values the setting offers.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingInput {
    Text(String),
    Choice(Value),
}

/// Set the scalar at `path` from what was typed or chosen. Returns the text
/// the field now holds when applying it rewrote the text, as expanding `~`
/// in a local path does.
pub fn apply_value(
    draft: &mut Value,
    path: &[String],
    input: SettingInput,
) -> Result<Option<String>, String> {
    let (text, choice) = match input {
        SettingInput::Text(text) => (text, None),
        SettingInput::Choice(choice) => (String::new(), Some(choice)),
    };
    let mut text = text;
    let mut rewritten = None;
    let parent = &path[..path.len() - 1];
    let parent_value = draft
        .pointer(&pointer(parent))
        .ok_or_else(|| "This setting no longer exists.".to_owned())?;
    let defaults = schema::defaults(parent, parent_value);
    let clear = choice.is_none()
        && text.is_empty()
        && (defaults
            .get(path.last().unwrap())
            .is_some_and(Value::is_null)
            || path == ["interface", "prefix"]
            || schema::whole_number(path).is_some_and(|n| n.defaulted));
    if !clear && !text.is_empty() && choice.is_none() {
        match schema::path_kind(path) {
            Some(schema::PathKind::Local) => {
                let expanded = mj_core::path_input::expand_local(std::path::Path::new(&text))
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .into_owned();
                if expanded != text {
                    rewritten = Some(expanded.clone());
                }
                text = expanded;
            }
            Some(schema::PathKind::RelativeDestination) => {
                let path = std::path::Path::new(&text);
                mj_core::config::validate_relative_destination(path)
                    .map_err(|error| error.to_string())?;
                if mj_core::path_input::needs_home(path).map_err(|error| error.to_string())? {
                    return Err(
                        "Repository destinations must be safe relative paths; ~ is not supported."
                            .into(),
                    );
                }
            }
            _ => {}
        }
    }
    let old = draft
        .pointer(&pointer(path))
        .ok_or_else(|| "This setting no longer exists.".to_owned())?
        .clone();
    let value = if clear && path == ["interface", "prefix"] {
        Value::String(mj_core::config::DEFAULT_PREFIX.to_owned())
    } else if clear {
        Value::Null
    } else if let Some(choice) = choice {
        choice
    } else if let Some(number) = schema::whole_number(path) {
        schema::parse_whole_number(number, &text)?
    } else if schema::is_build_cache_budget(path) {
        let text = text.trim();
        if text.is_empty() {
            Value::Null
        } else {
            let gigabytes = text
                .parse::<u64>()
                .map_err(|_| "Enter a whole number of gigabytes.".to_owned())?;
            if gigabytes == 0 {
                return Err("Enter at least 1 GB, or clear the field to use the default.".into());
            }
            Value::String(mj_core::config::build_cache_size_from_gigabytes(gigabytes))
        }
    } else if text.trim().is_empty() && old.is_null() {
        Value::Null
    } else {
        Value::String(text)
    };
    // An empty optional field means the account/runtime default.
    let mut parent_path = path.to_vec();
    let key = parent_path.pop().unwrap();
    let defaults = schema::defaults(&parent_path, draft.pointer(&pointer(&parent_path)).unwrap());
    let value = if value == "" && defaults.get(&key).is_some_and(Value::is_null) {
        Value::Null
    } else {
        value
    };
    if path == ["interface", "prefix"] {
        let mut keys = serde_json::from_value::<mj_core::config::KeysConfig>(draft["keys"].clone())
            .map_err(|error| error.to_string())?;
        keys.prefix = value.as_str().unwrap_or_default().to_owned();
        keys.resolve().map_err(|error| format!("{error:#}"))?;
    }
    if clear
        && !defaults.get(&key).is_some_and(Value::is_null)
        && path != ["interface", "prefix"]
        && !schema::whole_number(path).is_some_and(|number| number.defaulted)
    {
        return Err("This setting is required. Choose a value instead of clearing it.".into());
    }
    let changed = old != value;
    if key == "kind" {
        let old_parent = draft.pointer(&pointer(&parent_path)).unwrap().clone();
        let mut replacement = schema::defaults(&parent_path, &json!({"kind":value}));
        for (key, target) in replacement.as_object_mut().unwrap() {
            if key != "kind"
                && let Some(old) = old_parent.get(key)
            {
                *target = old.clone();
            }
        }
        replacement["kind"] = value;
        *draft.pointer_mut(&pointer(&parent_path)).unwrap() = replacement;
    } else {
        *draft.pointer_mut(&pointer(path)).unwrap() = value;
    }
    if changed && path.len() == 3 && path[0] == "subagents" && key == "model" {
        draft["subagents"][&path[1]]["effort"] = Value::Null;
    }
    Ok(rewritten)
}

/// Set the setting at page `path` to `value` the way a person would in
/// Settings: a checkbox is ticked or cleared, a listed value is chosen, and
/// anything else is typed, so it is checked and rewritten exactly as typing
/// it would be. `null` clears the field.
pub fn stage(draft: &mut Value, path: &[String], value: &Value) -> Result<(), String> {
    let Some(current) = draft.pointer(&pointer(path)) else {
        return Err("there is no such setting".into());
    };
    if current.is_object() || current.is_array() {
        return Err("this is a page of settings; name one of the settings on it".into());
    }
    for depth in 1..path.len() {
        let parent = &path[..depth];
        let parent_value = draft
            .pointer(&pointer(parent))
            .expect("parent of a setting");
        if hidden_key(parent, parent_value, &path[depth]) {
            return Err("Settings does not offer this setting here".into());
        }
    }
    if let Some(wanted) = value.as_bool()
        && let Some(held) = current.as_bool()
    {
        if held != wanted {
            toggle(draft, path)?;
        }
        return Ok(());
    }
    let choices = schema::choices(&storage_path(path), draft);
    if choices.len() > 1 && !value.is_null() {
        if !choices.contains(value) {
            let listed = choices
                .iter()
                .filter(|choice| !choice.is_null())
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!("{value} is not one of {listed}"));
        }
        apply_value(draft, path, SettingInput::Choice(value.clone()))?;
        return Ok(());
    }
    let text = match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::Array(_) | Value::Object(_) => {
            return Err("give one value; set a list or table one setting at a time".into());
        }
    };
    apply_value(draft, path, SettingInput::Text(text))?;
    Ok(())
}

/// Stage every edit, or none: the draft changes only when each edit applies
/// and the result is a configuration that could be saved. Paths may use the
/// stored spelling of an Interface setting (`theme`, `keys.prefix`). Returns
/// the page path of each edit, in order.
pub fn stage_all(
    draft: &mut Value,
    edits: &[crate::ui_request::SettingEdit],
) -> Result<Vec<Vec<String>>, String> {
    if edits.is_empty() {
        return Err("name at least one setting to change".into());
    }
    let mut staged = draft.clone();
    let mut paths = Vec::with_capacity(edits.len());
    for edit in edits {
        let path = page_path(&edit.path);
        stage(&mut staged, &path, &edit.value)
            .map_err(|error| format!("{}: {error}", edit.dotted()))?;
        paths.push(path);
    }
    config_from_draft(staged.clone())
        .map_err(anyhow::Error::from)
        .and_then(|config| config.validate())
        .map_err(|error| format!("these settings would not save: {error:#}"))?;
    *draft = staged;
    Ok(paths)
}

/// Flip the checkbox at `path`. Disabling a profile also clears the places
/// that name it, so the saved configuration never points at a profile that
/// cannot run; the returned notice says what was cleared.
pub fn toggle(draft: &mut Value, path: &[String]) -> Result<Option<String>, String> {
    let value = draft
        .pointer_mut(&pointer(path))
        .ok_or_else(|| "This setting no longer exists.".to_owned())?;
    let enabled = !value
        .as_bool()
        .ok_or_else(|| "This setting is not a checkbox.".to_owned())?;
    *value = Value::Bool(enabled);
    let [section, profile_id, field] = path else {
        return Ok(None);
    };
    if section != "profiles" || field != "enabled" || enabled {
        return Ok(None);
    }
    Ok(Some(format!("Disabled profile {profile_id:?}.")))
}

/// Add an entry to the collection at `collection`. A list grows by one
/// default entry; a named collection needs the new entry's name. Returns the
/// new entry's path.
pub fn add_entry(
    draft: &mut Value,
    collection: &[String],
    name: Option<&str>,
) -> Result<Vec<String>, String> {
    let value = draft
        .pointer_mut(&pointer(collection))
        .ok_or_else(|| "This list no longer exists.".to_owned())?;
    let mut path = collection.to_vec();
    if let Some(array) = value.as_array_mut() {
        array.push(
            if collection.last().is_some_and(|key| key == "repositories") {
                schema::repository_default()
            } else {
                Value::String(String::new())
            },
        );
        path.push((array.len() - 1).to_string());
        return Ok(path);
    }
    let object = value
        .as_object_mut()
        .ok_or_else(|| "Entries cannot be added here.".to_owned())?;
    let name = name.unwrap_or_default().trim();
    if name.is_empty() {
        return Err("Choose a name for the new entry.".into());
    }
    if object.contains_key(name) {
        return Err(format!("An entry named {name:?} already exists."));
    }
    path.push(name.to_owned());
    let entry = if collection.last().is_some_and(|key| key == "environment") {
        Value::String(String::new())
    } else {
        schema::defaults(&path, &json!({}))
    };
    object.insert(name.to_owned(), entry);
    Ok(path)
}

/// What removing an entry took with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// Every path that no longer exists: the entry, and the runtimes of a
    /// removed machine.
    pub removed: Vec<Vec<String>>,
    /// The removed entry's index when it was in a list, whose later entries
    /// have each moved up by one.
    pub list_index: Option<usize>,
    pub notice: Option<String>,
}

/// The runtimes that run on `machine_id`.
pub fn runtime_ids_on_machine(draft: &Value, machine_id: &str) -> Vec<String> {
    draft["targets"]
        .as_object()
        .map(|targets| {
            targets
                .iter()
                .filter(|(_, target)| {
                    target
                        .get("machine")
                        .and_then(Value::as_str)
                        .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
                        == machine_id
                })
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the entry at `path` is one a person may remove.
pub fn removable(draft: &Value, path: &[String]) -> bool {
    let Some((key, parent)) = path.split_last() else {
        return false;
    };
    draft
        .pointer(&pointer(parent))
        .is_some_and(|value| is_collection(parent, value))
        && !(parent == ["machines"] && key == mj_core::config::LOCAL_MACHINE_ID)
}

/// Remove the collection entry at `path`.
pub fn remove_entry(draft: &mut Value, path: &[String]) -> Result<Removal, String> {
    let Some((key, parent)) = path.split_last() else {
        return Err("The settings themselves cannot be removed.".into());
    };
    if !draft
        .pointer(&pointer(parent))
        .is_some_and(|value| is_collection(parent, value))
    {
        return Err("Only entries of a list can be removed.".into());
    }
    if parent == ["machines"] && key == mj_core::config::LOCAL_MACHINE_ID {
        return Err("This machine is always available.".into());
    }
    // A runtime cannot outlive the machine it names, so removing a machine
    // takes its runtimes with it. Left behind, they would make the next save
    // fail on a machine the file no longer defines.
    let runtimes = if parent == ["machines"] {
        runtime_ids_on_machine(draft, key)
    } else {
        Vec::new()
    };
    let mut removed = vec![path.to_vec()];
    let mut list_index = None;
    match draft.pointer_mut(&pointer(parent)).unwrap() {
        Value::Object(object) => {
            object.remove(key);
        }
        Value::Array(array) => {
            let index = key
                .parse::<usize>()
                .map_err(|_| "This list entry does not exist.".to_owned())?;
            if index >= array.len() {
                return Err("This list entry does not exist.".into());
            }
            array.remove(index);
            list_index = Some(index);
        }
        _ => {}
    }
    let mut notice = None;
    if !runtimes.is_empty()
        && let Some(targets) = draft["targets"].as_object_mut()
    {
        for runtime in &runtimes {
            targets.remove(runtime);
            removed.push(vec!["targets".to_owned(), runtime.clone()]);
        }
        notice = Some(match runtimes.as_slice() {
            [only] => format!("Removed machine {key:?} and its runtime {only:?}."),
            _ => format!(
                "Removed machine {key:?} and its {} runtimes {}.",
                runtimes.len(),
                runtimes
                    .iter()
                    .map(|runtime| format!("{runtime:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(Removal {
        removed,
        list_index,
        notice,
    })
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use serde_json::json;

    /// Reviewers are load-balanced across enabled profiles, so disabling one
    /// leaves `[review]` as it was.
    #[test]
    fn disabling_a_profile_leaves_review_unchanged() {
        let review =
            json!({"trigger": "push", "reviewers": [{"model": "gpt-6-terra", "effort": "xhigh"}]});
        let mut draft = json!({"profiles": {"codex": {"enabled": true}}, "review": review.clone()});
        let path = ["profiles", "codex", "enabled"].map(str::to_owned);
        assert_eq!(
            toggle(&mut draft, &path).unwrap().as_deref(),
            Some("Disabled profile \"codex\".")
        );
        assert_eq!(draft["review"], review);
    }
}
