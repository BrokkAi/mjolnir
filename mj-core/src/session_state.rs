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
