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
    (
        "Setup",
        &["profiles", "bundles", "targets", "machines", "daemon"],
    ),
    (
        "Sessions",
        &["review", "subagents", "quota", "sessionwiki", "phone"],
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

/// Every row `paths` draws on the page `path`, in order.
///
/// The first page groups its sections by heading. The Sub-agents page puts a
/// heading before its conditional YOLO task rows, so the two sets stay
/// visibly separate.
pub fn page_plan(path: &[String], paths: &[Vec<String>]) -> Vec<PageRow> {
    if !path.is_empty() {
        let mut plan = Vec::new();
        let mut yolo_heading = false;
        for (index, child) in paths.iter().enumerate() {
            if path == ["subagents"]
                && !yolo_heading
                && child.get(1).is_some_and(|key| key == "yolo")
            {
                plan.push(PageRow::Heading("YOLO Target Subagents"));
                yolo_heading = true;
            }
            plan.push(PageRow::Setting(index));
        }
        return plan;
    }
    let mut plan = Vec::new();
    let mut group = None;
    for (index, child) in paths.iter().enumerate() {
        let key = child.last().map(String::as_str).unwrap_or("");
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
            | "_reviewer_options"
            | "_review_action_reason"
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

/// Full setting paths shown on a page. Most pages list each visible child;
/// the global subagent page presents task model and effort fields inline.
pub fn visible_paths(path: &[String], value: &Value) -> Vec<Vec<String>> {
    let keys = visible_keys(path, value);
    if path == ["subagents"] {
        let mut fields = vec![vec!["subagents".to_owned(), "max_concurrent".to_owned()]];
        for task in ["straightforward", "subtle", "oracle"] {
            fields.push(vec![
                "subagents".to_owned(),
                task.to_owned(),
                "model".to_owned(),
            ]);
            let model = value[task]["model"].as_str().unwrap_or_default();
            if !model.is_empty() {
                fields.push(vec![
                    "subagents".to_owned(),
                    task.to_owned(),
                    "effort".to_owned(),
                ]);
            }
        }
        fields.push(vec![
            "subagents".to_owned(),
            "use_same_on_all_targets".to_owned(),
        ]);
        if !schema::shared_subagents(value) {
            for task in ["straightforward", "subtle", "oracle"] {
                if value["yolo"].get(task).is_none() {
                    continue;
                }
                fields.push(vec![
                    "subagents".to_owned(),
                    "yolo".to_owned(),
                    task.to_owned(),
                    "model".to_owned(),
                ]);
                let model = value["yolo"][task]["model"].as_str().unwrap_or_default();
                if !model.is_empty() {
                    fields.push(vec![
                        "subagents".to_owned(),
                        "yolo".to_owned(),
                        task.to_owned(),
                        "effort".to_owned(),
                    ]);
                }
            }
        }
        let extras = visible_keys(path, value)
            .into_iter()
            .filter(|child| {
                child != "max_concurrent"
                    && child != "use_same_on_all_targets"
                    && child != "yolo"
                    && !["straightforward", "subtle", "oracle"].contains(&child.as_str())
            })
            .map(|child| vec!["subagents".to_owned(), child])
            .collect::<Vec<_>>();
        fields.extend(extras);
        return fields;
    }
    if path.is_empty() {
        return keys.into_iter().map(|key| vec![key]).collect();
    }
    keys.into_iter()
        .map(|key| path.iter().cloned().chain([key]).collect())
        .collect()
}

/// Page that owns the row for a full setting path.
pub fn row_page(path: &[String]) -> &[String] {
    let subagent_field = matches!(
        path,
        [section, task, field]
            if section == "subagents"
                && ["straightforward", "subtle", "oracle"].contains(&task.as_str())
                && ["model", "effort"].contains(&field.as_str())
    ) || matches!(
        path,
        [section, yolo, task, field]
            if section == "subagents"
                && yolo == "yolo"
                && ["straightforward", "subtle", "oracle"].contains(&task.as_str())
                && ["model", "effort"].contains(&field.as_str())
    );
    if subagent_field {
        &path[..1]
    } else {
        path.get(..path.len().saturating_sub(1)).unwrap_or_default()
    }
}

/// Settings that exist in the stored shape but have no meaning on this entry,
/// so showing them would invite a value that does nothing.
pub fn hidden_key(path: &[String], value: &Value, key: &str) -> bool {
    if matches!(key, "_subagent_options" | "_reviewer_options") {
        return true;
    }
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        // A legacy per-profile table, read but never saved: not a setting.
        // Fields of other profile kinds never appear: each kind's defaults
        // list only its own (`schema::defaults`).
        ["profiles", _] if matches!(key, "subagents" | "muse_auto_review") => true,
        // This machine is always this machine; its type is not a choice.
        ["machines", mj_core::config::LOCAL_MACHINE_ID] => key == "kind",
        // Present only while reviewers still have to be chosen automatically:
        // it is not a setting. Review is on exactly when reviewers exist.
        ["review"] => key == "trigger",
        // Approvals are a remote-machine setting: a bare runtime here always
        // uses the configured approvals.
        ["targets", _] => {
            key == "permissions"
                && value["machine"]
                    .as_str()
                    .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
                    == mj_core::config::LOCAL_MACHINE_ID
        }
        // The YOLO set is retained while the shared set is active, but it is
        // not a setting the page offers and edits to it do nothing.
        ["subagents"] => key == "yolo" && schema::shared_subagents(value),
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
    breadcrumb_segments(path, draft).join(" › ")
}

/// [`breadcrumb`] as separate names: the first page's, then one per key of
/// `path`.
pub fn breadcrumb_segments(path: &[String], draft: &Value) -> Vec<String> {
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
    segments
}

/// A machine's build cache policy as configured. What the host actually
/// resolves an unset value to is only known once a surface inspects it.
pub fn machine_build_cache_summary(path: &[String], value: &Value) -> Option<String> {
    if !matches!(path, [section, _, page] if section == "machines" && page == "build_cache") {
        return None;
    }
    let enabled = match value["enabled"].as_bool() {
        Some(false) => "Disabled",
        Some(true) => "Enabled",
        None => "Enabled by default",
    };
    let budget = value["max_total_size"].as_str().map_or_else(
        || "automatic budget".to_owned(),
        |size| format!("{} GB budget", schema::build_cache_gigabytes_label(size)),
    );
    Some(format!("{enabled} · {budget}"))
}

/// How much the page at `path` holds, counted the way the page lists it:
/// hidden keys are left out, and synthetic pages such as `interface` count
/// the rows they show.
pub fn contents_summary(path: &[String], value: &Value) -> Option<String> {
    let count = visible_paths(path, value).len();
    match value {
        Value::Object(_) => Some(mj_core::text::counted(count, "setting", "settings")),
        Value::Array(_) => Some(mj_core::text::counted(count, "entry", "entries")),
        _ => None,
    }
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

#[cfg(test)]
mod target_tests {
    use super::*;
    use serde_json::json;

    /// The YOLO set is retained in the draft but absent from the page and
    /// from direct edits while the shared set is active. Unchecking reveals
    /// the same rows under their own heading.
    #[test]
    fn yolo_rows_are_hidden_refused_and_restored_by_the_checkbox() {
        let mut value = json!({"subagents": {"straightforward": {"model": "shared"}, "yolo": {"straightforward": {"model": "yolo"}}}});
        schema::expand(&mut value, &mut Vec::new());
        let subagents = &value["subagents"];
        let path = ["subagents".to_owned()];
        let yolo_model = vec![
            "subagents".to_owned(),
            "yolo".to_owned(),
            "straightforward".to_owned(),
            "model".to_owned(),
        ];
        assert!(!visible_paths(&path, subagents).contains(&yolo_model));
        assert!(hidden_key(&path, subagents, "yolo"));

        value["subagents"]["use_same_on_all_targets"] = json!(false);
        let subagents = &value["subagents"];
        let paths = visible_paths(&path, subagents);
        assert!(paths.contains(&yolo_model));
        assert!(!hidden_key(&path, subagents, "yolo"));
        assert!(page_plan(&path, &paths).contains(&PageRow::Heading("YOLO Target Subagents")));
        assert_eq!(subagents["yolo"]["straightforward"]["model"], "yolo");
    }
}
