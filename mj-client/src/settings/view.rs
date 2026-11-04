//! A settings page rendered as data, for a surface that draws its own
//! controls, and the one entry point that applies an edit such a surface
//! sends. An edit is accepted only through a control the page offers, so a
//! surface cannot change a setting the page shows as read only.
use super::draft::{pointer, storage_path};
use super::edit::{self, SettingInput};
use super::page::{self, PageRow, RowKind};
use super::{hidden, schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsPage {
    pub path: Vec<String>,
    /// The page names from the first page down to this one; segment `n`
    /// names the page at the first `n` keys of `path`.
    pub breadcrumb: Vec<String>,
    pub help: String,
    pub groups: Vec<SettingsGroup>,
    /// Adding an entry, on a page that lists entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<AddEntry>,
    /// Whether this page is an entry that may be removed.
    pub removable: bool,
    /// What this page cannot do here, and where it can be done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddEntry {
    pub label: String,
    /// Whether the new entry needs a name; a list grows without one.
    pub named: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    pub rows: Vec<SettingsRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsRow {
    pub path: Vec<String>,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub help: String,
    #[serde(flatten)]
    pub control: SettingsControl,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum SettingsControl {
    /// Opens the page at the row's path.
    Page {
        summary: String,
    },
    Checkbox {
        checked: bool,
    },
    Choice {
        value: Value,
        choices: Vec<SettingsChoice>,
    },
    Text {
        value: String,
        /// What an empty field means.
        placeholder: String,
        number: bool,
    },
    /// A literal value the surface may replace but is never sent.
    Hidden,
    ReadOnly {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsChoice {
    pub value: Value,
    pub label: String,
}

/// An edit a surface asks for, addressed by the path of the row or page it
/// came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsEdit {
    /// Type `text` into a text row, or pick `choice` on a choice row.
    Set {
        path: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<Value>,
    },
    Toggle {
        path: Vec<String>,
    },
    /// Add an entry to the page at `path`.
    Add {
        path: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Remove the entry whose page is `path`.
    Remove {
        path: Vec<String>,
    },
}

/// What an accepted edit did: the page to show next, and anything else it
/// changed that the person should be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditResult {
    pub page: Vec<String>,
    pub notice: Option<String>,
}

/// The theme as a surface that cannot see a terminal reports it.
fn theme_label(value: &Value, draft: &Value) -> String {
    schema::choice_label(&["theme".to_owned()], value, draft)
}

fn displayed(path: &[String], value: &Value, draft: &Value) -> String {
    match value {
        Value::Null => schema::null_label(&storage_path(path), draft),
        Value::Bool(true) => "On".to_owned(),
        Value::Bool(false) => "Off".to_owned(),
        _ if schema::is_build_cache_budget(path) => {
            format!(
                "{} GB",
                schema::build_cache_gigabytes_label(value.as_str().unwrap_or_default())
            )
        }
        _ => schema::choice_label(path, value, draft),
    }
}

/// Settings whose changes the terminal checks against something only it
/// measures or discovers, shown here as they stand.
fn read_only(path: &[String], draft: &Value) -> bool {
    match path {
        // The terminal inspects the machine's mbx installation first, which
        // may own these budgets itself.
        [section, _, page, ..] if section == "machines" && page == "build_cache" => true,
        // Offered models and efforts come from discovery.
        [section, key] if section == "review" && matches!(key.as_str(), "model" | "effort") => true,
        [section, task, key]
            if section == "subagents"
                && matches!(key.as_str(), "model" | "effort")
                && draft["_subagent_options"][task.as_str()].is_null() =>
        {
            true
        }
        _ => false,
    }
}

fn notices(path: &[String]) -> Vec<String> {
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        ["profiles"] => vec![
            "Detect profiles and managed setup run in the terminal: open Settings › Agent Profiles in mj.".to_owned(),
        ],
        ["profiles", id] => vec![format!(
            "To sign this profile in, save it, then run `mj login --profile {id}` in a terminal."
        )],
        ["targets"] => vec![
            "Detect runtimes runs in the terminal: open Settings › Runtimes in mj.".to_owned(),
        ],
        ["machines", _, "build_cache", ..] => vec![
            "Build cache settings change in the terminal Settings, which inspects this machine's mbx installation first.".to_owned(),
        ],
        ["review"] => vec![
            "The reviewer's model and effort are chosen in the terminal Settings, which discovers what the profile offers.".to_owned(),
        ],
        _ => Vec::new(),
    }
}

fn row(
    draft: &Value,
    page_path: &[String],
    parent: &Value,
    key: &str,
    path: Vec<String>,
) -> SettingsRow {
    let value = draft
        .pointer(&pointer(&path))
        .expect("a listed setting exists");
    let field_parent_path = path.get(..path.len().saturating_sub(1)).unwrap_or_default();
    let field_parent = draft.pointer(&pointer(field_parent_path)).unwrap_or(parent);
    let label = page::row_label(field_parent_path, field_parent, key, Some(value));
    let help = schema::specific_help(&path).unwrap_or_default().to_owned();
    let control = if hidden::is_marker(value) {
        if path == page_path {
            SettingsControl::Hidden
        } else {
            SettingsControl::Page {
                summary: "Set (hidden)".to_owned(),
            }
        }
    } else if page_path.is_empty() {
        SettingsControl::Page {
            summary: schema::section_summary(key, draft, &theme_label)
                .or_else(|| page::contents_summary(value))
                .unwrap_or_else(|| displayed(&path, value, draft)),
        }
    } else {
        match page::row_kind(draft, page_path, &path) {
            RowKind::Page => SettingsControl::Page {
                summary: page::machine_build_cache_summary(&path, value)
                    .or_else(|| page::contents_summary(value))
                    .unwrap_or_else(|| displayed(&path, value, draft)),
            },
            _ if read_only(&path, draft) => SettingsControl::ReadOnly {
                text: displayed(&path, value, draft),
            },
            RowKind::Checkbox => SettingsControl::Checkbox {
                checked: value.as_bool().unwrap_or_default(),
            },
            RowKind::Choices(choices) => SettingsControl::Choice {
                value: value.clone(),
                choices: choices
                    .into_iter()
                    .map(|choice| SettingsChoice {
                        label: schema::option_label(&path, &choice, draft),
                        value: choice,
                    })
                    .collect(),
            },
            RowKind::Field => SettingsControl::Text {
                value: schema::field_text(&path, value),
                placeholder: schema::null_label(&storage_path(&path), draft),
                number: schema::whole_number(&path).is_some()
                    || schema::is_build_cache_budget(&path),
            },
        }
    };
    SettingsRow {
        path,
        label,
        help,
        control,
    }
}

/// Render the page at `path` of `draft`.
pub fn render_page(draft: &Value, path: &[String]) -> Result<SettingsPage, String> {
    let current = draft
        .pointer(&pointer(path))
        .ok_or_else(|| "This settings page no longer exists.".to_owned())?;
    let mut groups: Vec<SettingsGroup> = Vec::new();
    if (current.is_object() && !hidden::is_marker(current)) || current.is_array() {
        let paths = page::visible_paths(path, current);
        for entry in page::page_plan(
            path,
            &paths
                .iter()
                .map(|p| p.last().cloned().unwrap_or_default())
                .collect::<Vec<_>>(),
        ) {
            match entry {
                PageRow::Heading(heading) => groups.push(SettingsGroup {
                    heading: Some(heading.to_owned()),
                    rows: Vec::new(),
                }),
                PageRow::Gap => {}
                PageRow::Setting(index) => {
                    let child = &paths[index];
                    let key = child.last().map(String::as_str).unwrap_or_default();
                    if groups.is_empty() {
                        groups.push(SettingsGroup {
                            heading: None,
                            rows: Vec::new(),
                        });
                    }
                    let group = groups.last_mut().expect("a group was just ensured");
                    let parent_path = page::row_page(child);
                    let parent = draft.pointer(&pointer(parent_path)).unwrap_or(current);
                    group
                        .rows
                        .push(row(draft, path, parent, key, child.clone()));
                }
            }
        }
    } else if let Some((key, parent_path)) = path.split_last() {
        // An entry holding one value, such as an environment variable, is a
        // page with that one value on it.
        let parent = draft
            .pointer(&pointer(parent_path))
            .expect("a page's parent exists");
        let mut single = row(draft, path, parent, key, path.to_vec());
        single.label = page::row_label(parent_path, parent, key, Some(current));
        groups.push(SettingsGroup {
            heading: None,
            rows: vec![single],
        });
    }
    let add = page::is_collection(path, current).then(|| AddEntry {
        label: if path == ["bundles"] { "Create" } else { "Add" }.to_owned(),
        named: current.is_object(),
    });
    Ok(SettingsPage {
        path: path.to_vec(),
        breadcrumb: page::breadcrumb_segments(path, draft),
        help: schema::page_specific_help(path, draft)
            .unwrap_or("Changes stay in this draft until you save.")
            .to_owned(),
        groups,
        add,
        removable: edit::removable(draft, path),
        notices: notices(path),
    })
}

/// Apply `change`, sent from the page at `page`, to `draft`.
///
/// The page is rendered first and the edit must match a control it offers:
/// text or a listed choice for a typed or chosen row, a toggle for a
/// checkbox, Add where the page lists entries, Remove on a removable entry.
pub fn apply(
    draft: &mut Value,
    page: &[String],
    change: SettingsEdit,
) -> Result<EditResult, String> {
    let rendered = render_page(draft, page)?;
    let offered = |path: &[String]| {
        rendered
            .groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.path == path)
            .map(|row| &row.control)
    };
    let stay = |notice| EditResult {
        page: page.to_vec(),
        notice,
    };
    match change {
        SettingsEdit::Set { path, text, choice } => match (offered(&path), text, choice) {
            (Some(SettingsControl::Text { .. } | SettingsControl::Hidden), Some(text), None) => {
                if schema::path_kind(&path) == Some(schema::PathKind::Target)
                    && mj_core::path_input::needs_home(std::path::Path::new(&text))
                        .map_err(|error| error.to_string())?
                {
                    return Err(
                        "Enter the full path on that machine. Only the terminal Settings resolves ~ on another machine."
                            .into(),
                    );
                }
                edit::apply_value(draft, &path, SettingInput::Text(text))?;
                Ok(stay(None))
            }
            (Some(SettingsControl::Choice { choices, .. }), None, Some(choice)) => {
                if !choices.iter().any(|offered| offered.value == choice) {
                    return Err("That is not one of this setting's choices.".into());
                }
                edit::apply_value(draft, &path, SettingInput::Choice(choice))?;
                Ok(stay(None))
            }
            _ => Err("This setting cannot be changed that way here.".into()),
        },
        SettingsEdit::Toggle { path } => match offered(&path) {
            Some(SettingsControl::Checkbox { .. }) => Ok(stay(edit::toggle(draft, &path)?)),
            _ => Err("This setting is not a checkbox here.".into()),
        },
        SettingsEdit::Add { path, name } => {
            if path != page || rendered.add.is_none() {
                return Err("Entries cannot be added here.".into());
            }
            let page = edit::add_entry(draft, &path, name.as_deref())?;
            Ok(EditResult { page, notice: None })
        }
        SettingsEdit::Remove { path } => {
            if path != page || !rendered.removable {
                return Err("This entry cannot be removed.".into());
            }
            let removal = edit::remove_entry(draft, &path)?;
            Ok(EditResult {
                page: path[..path.len() - 1].to_vec(),
                notice: removal.notice,
            })
        }
    }
}
