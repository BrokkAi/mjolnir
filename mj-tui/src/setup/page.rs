use super::*;

/// The first page's groups, in the order they are drawn.
///
/// A root key no group names is still listed, under `Other`, so a setting
/// added to the configuration can never go missing by being forgotten here.
pub(super) const ROOT_GROUPS: &[(&str, &[&str])] = &[
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
pub(super) enum PageRow {
    Heading(&'static str),
    Gap,
    /// An index into the page's keys.
    Setting(usize),
}

/// Every row `keys` draws on the page `path`, in order.
pub(super) fn page_plan(path: &[String], keys: &[String]) -> Vec<PageRow> {
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
pub(super) fn root_group(key: &str) -> &'static str {
    ROOT_GROUPS
        .iter()
        .find(|(_, keys)| keys.contains(&key))
        .map_or("Other", |(heading, _)| *heading)
}

/// Root keys the page never lists: the file's version, the settings the
/// synthetic Interface page gathers, the keybindings, and the deprecated
/// stopped-session flag that Advanced now owns.
pub(super) fn hidden_root_key(key: &str) -> bool {
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

pub(super) fn visible_keys(path: &[String], value: &Value) -> Vec<String> {
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
pub(super) fn hidden_key(path: &[String], value: &Value, key: &str) -> bool {
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

/// One row of a settings page: its name on the left and what it is set to on
/// the right, against the far edge, so the column reads down the page.
///
/// Nothing here may depend on whether the row is selected. A list identifies
/// its contents by the text it draws (`Form::set_list_contents`), so a caret
/// in the gutter would read as a different list the moment the selection
/// moved, cancelling the gesture a double-click is halfway through.
///
/// When both do not fit, the label gives way first: the value keeps its
/// width, less a few cells of label, and the two are always separated by at
/// least one space.
/// Whether `path` lists entries the user named rather than fixed settings.
pub(super) fn is_collection(path: &[String], value: &Value) -> bool {
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
pub(super) fn row_label(
    path: &[String],
    parent: &Value,
    key: &str,
    value: Option<&Value>,
) -> String {
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
pub(super) fn breadcrumb(path: &[String], draft: &Value) -> String {
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

/// A size string as the whole number of GB the field is measured in, or the
/// string itself when it is not a size at all.
/// Compiler time the cache avoided, in units somebody reads at a glance. The
/// number is mbx's own estimate, so more precision than this would be false.
/// What one row reports, which on the first page is the state of a whole
/// section rather than the size of it.
pub(super) fn row_summary(
    path: &[String],
    key: &str,
    value: &Value,
    draft: &Value,
    automatic: Option<String>,
) -> String {
    if path.is_empty()
        && let Some(summary) = schema::section_summary(key, draft)
    {
        return summary;
    }
    let mut child_path = path.to_vec();
    child_path.push(key.to_owned());
    machine_build_cache_summary(&child_path, value, automatic.as_deref())
        .unwrap_or_else(|| value_summary(path, key, value, draft, automatic))
}

/// Configured policy only: opening the page resolves the host's actual defaults.
/// A host the preview found unable to share the cache reports `unavailable`,
/// which is the same fact the build cache page shows as an unchecked switch.
pub(super) fn machine_build_cache_summary(
    path: &[String],
    value: &Value,
    unavailable: Option<&str>,
) -> Option<String> {
    if !matches!(path, [section, _, page] if section == "machines" && page == "build_cache") {
        return None;
    }
    if let Some(reason) = unavailable {
        return Some(format!("{reason}  ›"));
    }
    let enabled = match value["enabled"].as_bool() {
        Some(false) => "Disabled",
        Some(true) => "Enabled",
        None => "Enabled by default",
    };
    let budget = value["max_total_size"].as_str().map_or_else(
        || "automatic budget".to_owned(),
        |size| format!("{} GB budget", build_cache_gigabytes_label(size)),
    );
    Some(format!("{enabled} · {budget}  ›"))
}

/// `automatic` replaces the placeholder for an unset value when its resolved
/// default is known.
pub(super) fn value_summary(
    path: &[String],
    key: &str,
    value: &Value,
    draft: &Value,
    automatic: Option<String>,
) -> String {
    let mut child_path = path.to_vec();
    child_path.push(key.to_owned());
    let summary = match value {
        // The eligible profiles are a page of checkboxes, one per profile, so
        // the row counts the checked ones instead of counting settings.
        Value::Object(entries) if child_path == ["subagents", "eligible_profiles"] => {
            let selected = entries.values().filter(|value| **value == true).count();
            format!("{selected} of {} selected  ›", entries.len())
        }
        Value::Object(entries) => {
            format!("{}  ›", counted(entries.len(), "setting", "settings"))
        }
        Value::Array(entries) => format!("{}  ›", counted(entries.len(), "entry", "entries")),
        // The machine's own switch is a checkbox, and an unset value means on.
        // A host that cannot support the cache reports an unchecked box
        // through `automatic`, whatever the machine asks for.
        Value::Bool(_) | Value::Null if is_build_cache_field(&child_path, "enabled") => {
            if value.as_bool().unwrap_or(true) {
                automatic.unwrap_or_else(|| Checkbox::marker(true).to_owned())
            } else {
                Checkbox::marker(false).to_owned()
            }
        }
        // Every on/off setting is the same checkbox, whichever section holds it.
        Value::Bool(value) => Checkbox::marker(*value).to_owned(),
        // The cache size is measured in whole GB, whatever unit the file
        // spells it in. Only a hand-edited invalid value keeps its own text.
        Value::String(size) if is_build_cache_budget(&child_path) => automatic
            .filter(|label| label != "Resolving…" && label != "Unknown")
            .unwrap_or_else(|| build_cache_gigabytes_label(size)),
        Value::Null => automatic.unwrap_or_else(|| schema::null_label(&child_path, draft)),
        // The archive window's live estimate carries the value itself, so it
        // replaces the number as well as the "Never" placeholder.
        _ => match automatic.filter(|_| key == "archive_after_days") {
            Some(label) => label,
            None if storage_path(&child_path) == ["theme"] => {
                schema::draft_theme_report(value, draft)
            }
            None => schema::choice_label(&child_path, value, draft),
        },
    };
    if !value.is_object()
        && !value.is_array()
        && !value.is_boolean()
        && !schema::choices(&storage_path(&child_path), draft).is_empty()
    {
        ComboBox::display_value(&summary)
    } else {
        summary
    }
}
