//! Frontend-neutral ACP session-state helpers.

use agent_client_protocol::schema::v1::{
    ElicitationMode, ElicitationPropertySchema, EnumOption, MultiSelectItems, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOptions, SessionConfigValueId,
};

use crate::event::ElicitationPrompt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warning,
    Fatal,
}

pub fn status_transcript_text(kind: StatusKind, text: &str) -> String {
    match kind {
        StatusKind::Info => text.to_string(),
        StatusKind::Warning => format!("warning: {text}"),
        StatusKind::Fatal => format!("fatal: {text}"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationFormField {
    pub property_name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub required: bool,
    pub kind: ElicitationFormFieldKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationFormFieldKind {
    SingleSelect {
        options: Vec<EnumOption>,
    },
    MultiSelect {
        options: Vec<EnumOption>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
    Text,
    Number {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
    Boolean,
}

/// How a pending elicitation should be rendered and resolved, derived once
/// from its mode + schema so the renderer and the key handler agree on the
/// interpretation. Owned data keeps both call sites borrow-free.
#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationView {
    /// Single-select form: exactly one property, a `StringPropertySchema`
    /// with a non-empty `oneOf` or `enum`. Accept maps `{ property => String(value) }`.
    SingleSelect {
        property_name: String,
        title: Option<String>,
        options: Vec<EnumOption>,
    },
    /// URL/QR step (e.g. OAuth login). Accept carries no content.
    Url { url: String },
    /// Free-text form: exactly one property, a `StringPropertySchema` with no
    /// `oneOf`/`enum` (e.g. an API-key entry). Accept maps
    /// `{ property => String(typed_value) }`.
    Text {
        property_name: String,
        title: Option<String>,
        description: Option<String>,
    },
    /// A form with multiple properties, or a single multi-select property.
    /// Fields are presented in schema order and accumulated into one Accept.
    Form {
        title: Option<String>,
        fields: Vec<ElicitationFormField>,
    },
    /// Any shape the UI cannot render (an enum with no options or a future
    /// schema variant). The modal shows an informational message and resolves
    /// to `decline` on dismiss.
    Unsupported,
}

/// Classify an elicitation prompt into the renderable/resolvable view. Never
/// panics on an unexpected schema: unsupported primitive or future variants
/// become [`ElicitationView::Unsupported`].
pub fn classify_elicitation(prompt: &ElicitationPrompt) -> ElicitationView {
    match &prompt.mode {
        ElicitationMode::Url(url_mode) => ElicitationView::Url {
            url: url_mode.url.clone(),
        },
        ElicitationMode::Form(form) => {
            let schema = &form.requested_schema;
            if schema.properties.is_empty() {
                return ElicitationView::Unsupported;
            }
            if schema.properties.len() > 1
                || matches!(
                    schema.properties.values().next(),
                    Some(
                        ElicitationPropertySchema::Array(_)
                            | ElicitationPropertySchema::Number(_)
                            | ElicitationPropertySchema::Integer(_)
                            | ElicitationPropertySchema::Boolean(_)
                    )
                )
            {
                let required = schema.required.as_deref().unwrap_or_default();
                let mut fields = Vec::with_capacity(schema.properties.len());
                for (property_name, property) in &schema.properties {
                    let field = match property {
                        ElicitationPropertySchema::String(string_schema) => {
                            let options = string_schema
                                .one_of
                                .clone()
                                .filter(|options| !options.is_empty())
                                .or_else(|| {
                                    string_schema.enum_values.as_ref().and_then(|values| {
                                        (!values.is_empty()).then(|| {
                                            values
                                                .iter()
                                                .map(|value| {
                                                    EnumOption::new(value.clone(), value.clone())
                                                })
                                                .collect()
                                        })
                                    })
                                });
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: string_schema.title.clone(),
                                description: string_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: options.map_or(ElicitationFormFieldKind::Text, |options| {
                                    ElicitationFormFieldKind::SingleSelect { options }
                                }),
                            }
                        }
                        ElicitationPropertySchema::Array(array_schema) => {
                            let options = match &array_schema.items {
                                MultiSelectItems::Titled(items) => items.options.clone(),
                                MultiSelectItems::Untitled(items) => items
                                    .values
                                    .iter()
                                    .map(|value| EnumOption::new(value.clone(), value.clone()))
                                    .collect(),
                                _ => return ElicitationView::Unsupported,
                            };
                            if options.is_empty() {
                                return ElicitationView::Unsupported;
                            }
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: array_schema.title.clone(),
                                description: array_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: ElicitationFormFieldKind::MultiSelect {
                                    options,
                                    min_items: array_schema.min_items,
                                    max_items: array_schema.max_items,
                                },
                            }
                        }
                        ElicitationPropertySchema::Number(number_schema) => ElicitationFormField {
                            property_name: property_name.clone(),
                            title: number_schema.title.clone(),
                            description: number_schema.description.clone(),
                            required: required.contains(property_name),
                            kind: ElicitationFormFieldKind::Number {
                                minimum: number_schema.minimum,
                                maximum: number_schema.maximum,
                            },
                        },
                        ElicitationPropertySchema::Integer(integer_schema) => {
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: integer_schema.title.clone(),
                                description: integer_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: ElicitationFormFieldKind::Integer {
                                    minimum: integer_schema.minimum,
                                    maximum: integer_schema.maximum,
                                },
                            }
                        }
                        ElicitationPropertySchema::Boolean(boolean_schema) => {
                            ElicitationFormField {
                                property_name: property_name.clone(),
                                title: boolean_schema.title.clone(),
                                description: boolean_schema.description.clone(),
                                required: required.contains(property_name),
                                kind: ElicitationFormFieldKind::Boolean,
                            }
                        }
                        _ => return ElicitationView::Unsupported,
                    };
                    fields.push(field);
                }
                return ElicitationView::Form {
                    title: schema.title.clone(),
                    fields,
                };
            }
            let Some((property_name, property)) = schema.properties.iter().next() else {
                return ElicitationView::Unsupported;
            };
            match property {
                ElicitationPropertySchema::String(string_schema) => {
                    let one_of_options = string_schema
                        .one_of
                        .as_ref()
                        .filter(|opts| !opts.is_empty());
                    let enum_options = string_schema
                        .enum_values
                        .as_ref()
                        .filter(|opts| !opts.is_empty());
                    match (one_of_options, enum_options) {
                        (Some(options), _) => ElicitationView::SingleSelect {
                            property_name: property_name.clone(),
                            // Prefer the per-property title, falling back to the
                            // schema-level title for the modal heading.
                            title: string_schema.title.clone().or_else(|| schema.title.clone()),
                            options: options.clone(),
                        },
                        (None, Some(values)) => ElicitationView::SingleSelect {
                            property_name: property_name.clone(),
                            title: string_schema.title.clone().or_else(|| schema.title.clone()),
                            options: values
                                .iter()
                                .map(|value| EnumOption::new(value.clone(), value.clone()))
                                .collect(),
                        },
                        // A string field without `oneOf` or `enum` is free
                        // text: render an input field (e.g. API-key entry).
                        _ => ElicitationView::Text {
                            property_name: property_name.clone(),
                            title: string_schema.title.clone().or_else(|| schema.title.clone()),
                            description: string_schema.description.clone(),
                        },
                    }
                }
                _ => ElicitationView::Unsupported,
            }
        }
        // `ElicitationMode` is `#[non_exhaustive]`; future modes degrade safely.
        _ => ElicitationView::Unsupported,
    }
}
/// One displayed value for a select-style session config option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValueChoice {
    pub value: SessionConfigValueId,
    pub name: String,
    pub description: Option<String>,
    pub group: Option<String>,
}

/// Return the current value identifier for a select-style session config option.
pub fn config_option_current_value_id(
    option: &SessionConfigOption,
) -> Option<&SessionConfigValueId> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(&select.current_value),
        _ => None,
    }
}

/// Return the value choices for a select-style config option.
pub fn config_option_choices(option: &SessionConfigOption) -> Option<Vec<ConfigValueChoice>> {
    match &option.kind {
        SessionConfigKind::Select(select) => Some(config_select_choices(select)),
        _ => None,
    }
}

/// Whether a session config option selects a model.
pub fn is_model_config_option(option: &SessionConfigOption) -> bool {
    matches!(option.category, Some(SessionConfigOptionCategory::Model))
}

fn config_select_choices(select: &SessionConfigSelect) -> Vec<ConfigValueChoice> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|option| ConfigValueChoice {
                value: option.value.clone(),
                name: option.name.clone(),
                description: option.description.clone(),
                group: None,
            })
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| {
                group.options.iter().map(move |option| ConfigValueChoice {
                    value: option.value.clone(),
                    name: option.name.clone(),
                    description: option.description.clone(),
                    group: Some(group.name.clone()),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

use agent_client_protocol::schema::v1::{ToolCall, ToolCallUpdate};

/// Whether a tool call is the transport wrapper for a Mjolnir subagent command.
pub fn is_subagent_transport_call(tool_call: &ToolCall) -> bool {
    subagent_identity_from_raw_input(tool_call.raw_input.as_ref())
        || subagent_identity_from_name(&tool_call.title)
        || subagent_identity_from_meta(tool_call.meta.as_ref())
}

/// Whether a tool update is the transport wrapper for a Mjolnir subagent command.
pub fn is_subagent_transport_update(update: &ToolCallUpdate) -> bool {
    subagent_identity_from_raw_input(update.fields.raw_input.as_ref())
        || update
            .fields
            .title
            .as_deref()
            .is_some_and(subagent_identity_from_name)
        || subagent_identity_from_meta(update.meta.as_ref())
}

fn subagent_identity_from_raw_input(raw_input: Option<&serde_json::Value>) -> bool {
    let Some(object) = raw_input.and_then(serde_json::Value::as_object) else {
        return false;
    };
    object
        .get("server")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|server| server == "mj-subagents")
        && object
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|tool| matches!(tool, "create_subagent" | "subagent_cancel"))
}

fn subagent_identity_from_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("mj-subagents")
        && ["create_subagent", "subagent_cancel"]
            .into_iter()
            .any(|tool| contains_tool_identifier(&name, tool))
}

fn contains_tool_identifier(name: &str, tool: &str) -> bool {
    name.match_indices(tool).any(|(start, _)| {
        let before = name[..start].chars().next_back();
        let suffix = &name[start + tool.len()..];
        let after = suffix.chars().next();
        (!before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            || name[..start].ends_with("__"))
            && (!after
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
                || suffix.starts_with("__"))
    })
}

fn subagent_identity_from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> bool {
    let Some(meta) = meta else {
        return false;
    };
    meta.get("toolName")
        .and_then(serde_json::Value::as_str)
        .is_some_and(subagent_identity_from_name)
        || meta
            .get("claudeCode")
            .and_then(serde_json::Value::as_object)
            .and_then(|claude| claude.get("toolName"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(subagent_identity_from_name)
}
#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        SessionConfigOptionCategory, SessionConfigSelectOption,
    };

    #[test]
    fn model_select_helpers_expose_current_value_and_choices() {
        let option = SessionConfigOption::select(
            "model",
            "Model",
            "sonnet",
            vec![SessionConfigSelectOption::new("sonnet", "Sonnet")],
        )
        .category(SessionConfigOptionCategory::Model);

        assert!(is_model_config_option(&option));
        assert_eq!(
            config_option_current_value_id(&option).unwrap().to_string(),
            "sonnet"
        );
        assert_eq!(config_option_choices(&option).unwrap()[0].name, "Sonnet");
    }
}
