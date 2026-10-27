//! The settings draft: the stored shape of a configuration with every
//! optional field filled in, and the paths that address it.
use mj_core::config::Config;
use serde_json::{Value, json};

/// Where a page path is stored. The Interface page is synthetic: it gathers
/// root settings, and its prefix key lives under `keys`.
pub fn storage_path(path: &[String]) -> Vec<String> {
    match path {
        [section, key] if section == "interface" && key == "prefix" => {
            vec!["keys".to_owned(), "prefix".to_owned()]
        }
        [section, rest @ ..] if section == "interface" => rest.to_vec(),
        _ => path.to_vec(),
    }
}

pub fn pointer(path: &[String]) -> String {
    storage_path(path)
        .iter()
        .map(|key| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
        .collect()
}

pub fn only_unset_fields(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(fields) => fields.values().all(only_unset_fields),
        _ => false,
    }
}

/// The value an edit starts from and the draft it edits.
///
/// `original` is the stored shape exactly as the configuration serializes,
/// which is the base a save merges against. The draft adds this machine and
/// every optional field, so each page can show what is unset.
pub fn open(config: &Config) -> (Value, Value) {
    let original = serde_json::to_value(config).expect("configuration serializes");
    let mut draft = original.clone();
    ensure_local_machine(&mut draft);
    super::schema::expand(&mut draft, &mut Vec::new());
    (original, draft)
}

pub fn config_from_draft(mut draft: Value) -> Result<Config, serde_json::Error> {
    draft.as_object_mut().unwrap().remove("_subagent_options");
    for task in ["straightforward", "subtle"] {
        if draft["subagents"][task]["model"]
            .as_str()
            .is_none_or(|m| m.trim().is_empty())
        {
            draft["subagents"].as_object_mut().unwrap().remove(task);
        }
    }
    // The Machines page always shows this machine. Until it carries a setting
    // of its own it is the implied machine, not an entry, so it does not turn
    // an untouched draft into a change.
    if let Some(machines) = draft["machines"].as_object_mut()
        && machines
            .get(mj_core::config::LOCAL_MACHINE_ID)
            .and_then(|local| local["build_cache"].as_object())
            .is_some_and(|cache| cache.values().all(only_unset_fields))
    {
        machines.remove(mj_core::config::LOCAL_MACHINE_ID);
    }
    // A cleared number that has a default of its own is left out, so the
    // configuration's default applies instead of a null it cannot read.
    for (section, key) in [("notify", "delay_seconds"), ("subagents", "max_concurrent")] {
        if draft[section][key].is_null()
            && let Some(entries) = draft[section].as_object_mut()
        {
            entries.remove(key);
        }
    }
    // UI projections inspect sources only. Saving/warming serializes those
    // sources for credential resolution in the background request.
    mj_core::config::with_environment_sources_only(|| serde_json::from_value(draft))
}

/// A configuration with no machines of its own still has this machine, so the
/// Machines page always has an entry to open. The key is placed where the file
/// keeps it, before the runtimes that name it, so the page list reads in the
/// same order as the file.
pub fn ensure_local_machine(draft: &mut Value) {
    let root = draft.as_object_mut().expect("a configuration is an object");
    if !root.contains_key("machines") {
        let mut rebuilt = serde_json::Map::new();
        for (key, value) in std::mem::take(root) {
            if key == "targets" {
                rebuilt.insert("machines".to_owned(), json!({}));
            }
            rebuilt.insert(key, value);
        }
        rebuilt.entry("machines").or_insert_with(|| json!({}));
        *root = rebuilt;
    }
    root["machines"]
        .as_object_mut()
        .expect("machines is an object")
        .entry(mj_core::config::LOCAL_MACHINE_ID)
        .or_insert_with(|| json!({"kind": "local"}));
}
