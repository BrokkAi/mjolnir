//! Which rows a settings page lists, under which headings, and what each
//! row and page is called.
use super::draft::{pointer, storage_path};
use super::schema;
use serde_json::Value;

/// The first page's groups, in the order they are drawn.
///
/// A root key no group names is still listed, under `Other`, so a setting
/// added to the configuration can never go missing by being forgotten here.
pub const ROOT_GROUPS: &[(&str, &[&str])] = &[
    ("Setup", &["profiles", "bundles", "targets", "machines"]),
    (
        "Sessions",
        &[
            "review",
            "continuation",
            "subagents",
            "sessionwiki",
            "phone",
        ],
    ),
    ("Display", &["interface", "notify", "advanced"]),
    ("Privacy", &["jev"]),
];

/// A row of a settings page. The first page puts a heading above each group
/// and a blank line between them; every page below it is a plain list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRow {
    Heading(&'static str),
    Gap,
    /// An index into the page's keys.
    Setting(usize),
}

/// Every row `keys` draws on the page `path`, in order.
pub fn page_plan(path: &[String], keys: &[String]) -> Vec<PageRow> {
    if !path.is_empty() {
        return (0..keys.len()).map(PageRow::Setting).collect();
    }
    let mut plan = Vec::new();
    let mut group = None;
    for (index, key) in keys.iter().enumerate() {
        let heading = root_group(key);
        if group != Some(heading) {
            if group.is_some() {
                plan.push(PageRow::Gap);
            }
            plan.push(PageRow::Heading(heading));
            group = Some(heading);
        }
        plan.push(PageRow::Setting(index));
    }
    plan
}

/// The group heading a first-page row belongs under.
pub fn root_group(key: &str) -> &'static str {
    ROOT_GROUPS
        .iter()
        .find(|(_, keys)| keys.contains(&key))
        .map_or("Other", |(heading, _)| *heading)
}

/// Root keys the page never lists: the file's version, the settings the
/// synthetic Interface page gathers, the keybindings, and the deprecated
/// stopped-session flag that Advanced now owns.
pub fn hidden_root_key(key: &str) -> bool {
    matches!(
        key,
        "version"
            | "advanced"
            | "sessions_side"
            | "spinner"
            | "theme"
            | "keys"
            | "_subagent_options"
    )
}

pub fn visible_keys(path: &[String], value: &Value) -> Vec<String> {
    if path.is_empty() {
        let object = value.as_object();
        // `interface` is synthetic and `advanced` is always offered, so both
        // are listed whether or not the draft stores a key for them.
        let present = |key: &str| {
            matches!(key, "interface" | "advanced")
                || object.is_some_and(|entries| entries.contains_key(key))
        };
        let mut keys = ROOT_GROUPS
            .iter()
            .flat_map(|(_, keys)| keys.iter())
            .filter(|key| present(key))
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>();
        let ungrouped = object
            .into_iter()
            .flat_map(|entries| entries.keys())
            .filter(|key| {
                !hidden_root_key(key) && !keys.iter().any(|placed| placed == key.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        keys.extend(ungrouped);
        return keys;
    }
    if path == ["interface"] {
        return vec![
            "prefix".to_owned(),
            "sessions_side".to_owned(),
            "spinner".to_owned(),
            "theme".to_owned(),
        ];
    }
    match value {
        Value::Object(entries) => entries
            .keys()
            .filter(|key| key.as_str() != "version")
            .filter(|key| !hidden_key(path, value, key))
            .cloned()
            .collect(),
        Value::Array(entries) => (0..entries.len()).map(|i| i.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Settings that exist in the stored shape but have no meaning on this entry,
/// so showing them would invite a value that does nothing.
pub fn hidden_key(path: &[String], value: &Value, key: &str) -> bool {
    if key == "_subagent_options" {
        return true;
    }
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        // This machine is always this machine; its type is not a choice.
        ["profiles", _] if key == "subagents" => true,
        // These two settings are read only for a Codex profile; the profile's
        // own validation rejects them on any other kind.
        ["profiles", _] if matches!(key, "guardian_review_model" | "codex_profile") => {
            value["kind"].as_str() != Some("codex")
        }
        ["machines", mj_core::config::LOCAL_MACHINE_ID] => key == "kind",
        // Approvals are a remote-machine setting: a bare runtime here always
        // uses the configured approvals.
        ["targets", _] => {
            key == "permissions"
                && value["machine"]
                    .as_str()
                    .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
                    == mj_core::config::LOCAL_MACHINE_ID
        }
        _ => false,
    }
}

/// Whether `path` lists entries the user named rather than fixed settings.
pub fn is_collection(path: &[String], value: &Value) -> bool {
    value.is_array()
        || (path.len() == 1
            && matches!(
                path[0].as_str(),
                "profiles" | "machines" | "targets" | "bundles"
            ))
        || path.last().is_some_and(|key| key == "environment")
}

/// The name a row carries on the page `path`.
///
/// The label table is keyed by the last segment of a path, so a name the user
/// chose must never be looked up in it: a machine called `local` is that
/// machine, not the "Local repository directory" setting that shares the key.
pub fn row_label(path: &[String], parent: &Value, key: &str, value: Option<&Value>) -> String {
    if parent.is_array() {
        return value
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Item {}", key.parse::<usize>().unwrap_or(0) + 1));
    }
    if is_collection(path, parent) {
        return key.to_owned();
    }
    let mut field_path = path.to_vec();
    field_path.push(key.to_owned());
    schema::field_label(&field_path)
}

/// The trail of page names above `path`, starting at the first page.
///
/// Each segment is named the way the page above it names its rows, so a name
/// the user chose is shown as they wrote it and only schema keys reach the
/// label table: a machine called `local` is that machine, not the "Local
/// repository directory" setting that shares the key.
pub fn breadcrumb(path: &[String], draft: &Value) -> String {
    let mut segments = vec!["Settings".to_owned()];
    let mut parent: Vec<String> = Vec::new();
    for key in path {
        let mut child = parent.clone();
        child.push(key.clone());
        segments.push(match draft.pointer(&pointer(&parent)) {
            Some(value) => row_label(&parent, value, key, draft.pointer(&pointer(&child))),
            None => schema::label(key),
        });
        parent = child;
    }
    segments.join(" › ")
}

/// How a row of a settings page is edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// Opens a page of its own.
    Page,
    Checkbox,
    /// One of these values; `null` means unset.
    Choices(Vec<Value>),
    /// Text, a number or a path, typed.
    Field,
}

/// How the row at `path` on the page `page` is edited. A collection page
/// lists entries, and every entry opens its own page, even one that holds a
/// single value.
pub fn row_kind(draft: &Value, page: &[String], path: &[String]) -> RowKind {
    let value = draft
        .pointer(&pointer(path))
        .expect("a listed setting exists");
    let collection = draft
        .pointer(&pointer(page))
        .is_some_and(|current| is_collection(page, current));
    if value.is_object() || value.is_array() || collection {
        return RowKind::Page;
    }
    let mut choices = if schema::is_build_cache_field(path, "enabled") {
        vec![Value::Null, Value::Bool(true), Value::Bool(false)]
    } else {
        schema::choices(&storage_path(path), draft)
    };
    let parent = &path[..path.len() - 1];
    let defaults = schema::defaults(
        &storage_path(parent),
        draft.pointer(&pointer(parent)).unwrap(),
    );
    if !choices.is_empty()
        && defaults
            .get(path.last().unwrap())
            .is_some_and(Value::is_null)
        && !choices.contains(&Value::Null)
    {
        choices.insert(0, Value::Null);
    }
    if !choices.is_empty() {
        RowKind::Choices(choices)
    } else if value.is_boolean() {
        RowKind::Checkbox
    } else {
        RowKind::Field
    }
}
