//! Frontend-neutral ACP session-state helpers.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOptions, SessionConfigValueId,
};

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
