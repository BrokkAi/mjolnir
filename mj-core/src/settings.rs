//! Frontend-neutral `/mjconfig` catalog rules.
//!
//! The terminal and web clients render their controls differently, but they
//! must agree on the panels and on which ACP options are safe to expose for a
//! seat. Keep those product rules here instead of copying them into a frontend.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOptions, SessionConfigValueId,
};

use crate::roster::{AcpInventory, AdapterKind, ModelChoice};

/// Built-in ACP servers whose enablement belongs in `/mjconfig`.
///
/// A platform adapter is deliberately absent: it can be the only launchable
/// route on that build, so treating it as disableable would be misleading.
pub const CONFIGURABLE_ACP_SERVERS: [&str; 2] = ["codex-acp", "claude-acp"];

pub fn is_configurable_acp_server(id: &str) -> bool {
    CONFIGURABLE_ACP_SERVERS.contains(&id)
}

/// Top-level `/mjconfig` panels shared by every interactive frontend.
///
/// The primary agent has no panel: its live model and reasoning effort are
/// driven by the `/model` and `/effort` commands and the session-config
/// shortcut row instead of saved `/mjconfig` defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Team,
    Reviewer,
    Subagents,
    AcpServers,
    Input,
    Appearance,
}

impl SettingsTab {
    pub const ALL: [Self; 6] = [
        Self::Team,
        Self::Reviewer,
        Self::Subagents,
        Self::AcpServers,
        Self::Input,
        Self::Appearance,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Reviewer => "reviewer",
            Self::Subagents => "subagents",
            Self::AcpServers => "servers",
            Self::Input => "input",
            Self::Appearance => "appearance",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Team => "Team",
            Self::Reviewer => "Reviewer",
            Self::Subagents => "Subagents",
            Self::AcpServers => "ACP Servers",
            Self::Input => "Input",
            Self::Appearance => "Appearance",
        }
    }
}

/// A role whose saved session defaults are being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDefaultsSeat {
    Primary,
    Review,
    Subagents,
}

/// Resolve the ACP source whose session options belong beside a seat model.
///
/// Concrete models route through the adapter that actually advertises them;
/// a Team source pin constrains only `auto`. This mirrors roster resolution so
/// `/mjconfig` never shows one provider's options beside another provider's
/// explicit model.
#[allow(clippy::too_many_arguments)]
pub fn session_source_for_model(
    model: &str,
    configured_source: Option<&str>,
    priority: &[String],
    active_model: Option<&str>,
    active_source: Option<&str>,
    choices: &[ModelChoice],
    inventory: &AcpInventory,
) -> Option<String> {
    if model == crate::config::DISABLED_MODEL {
        return None;
    }
    let source_exists = |source: &str| inventory.servers.iter().any(|server| server.id == source);
    let advertised_source = |source: &str| {
        choices.iter().any(|choice| {
            choice.available && choice.model == model && choice.adapter.as_deref() == Some(source)
        })
    };

    if model != "auto" {
        if active_model == Some(model)
            && let Some(source) = active_source
            && source_exists(source)
        {
            return Some(source.to_string());
        }
        if let Some(source) = priority
            .iter()
            .find(|source| advertised_source(source) && source_exists(source))
        {
            return Some(source.clone());
        }
        if let Some(source) = choices
            .iter()
            .find(|choice| choice.available && choice.model == model)
            .and_then(|choice| choice.adapter.as_deref())
            .filter(|source| source_exists(source))
        {
            return Some(source.to_string());
        }
        if let Some(source) = crate::roster::native_source_id(model)
            && source_exists(&source)
        {
            return Some(source);
        }
    }

    if let Some(source) = configured_source.filter(|source| source_exists(source)) {
        return Some(source.to_string());
    }
    if model == "auto"
        && let Some(source) = active_source
        && source_exists(source)
    {
        return Some(source.to_string());
    }
    priority
        .iter()
        .find(|source| {
            inventory.servers.iter().any(|server| {
                server.id == source.as_str()
                    && server.policy != crate::config::AcpServerPolicy::Disabled
                    && !server.session_config.is_empty()
            })
        })
        .cloned()
}

/// [`session_source_for_model`] with the seat's route read from the config.
///
/// Every frontend resolves a seat's provider through this one function so the
/// panel a user edits and the save that interprets the edit agree on the
/// route. `active_models` carries the live session's routing when the caller
/// has one; without it an `auto` seat can only fall back to the priority scan.
pub fn seat_session_source_for_model(
    config: &crate::config::Config,
    seat: SessionDefaultsSeat,
    model: &str,
    active_models: Option<&crate::config::ModelsConfig>,
    choices: &[ModelChoice],
    inventory: &AcpInventory,
) -> Option<String> {
    let (configured_source, priority, active_model, active_source) = match seat {
        SessionDefaultsSeat::Primary => (
            config.agent.acp_source.as_deref(),
            config.agent.acp_priority.as_slice(),
            active_models.map(|models| models.primary.as_str()),
            active_models.and_then(|models| models.primary_source.as_deref()),
        ),
        SessionDefaultsSeat::Review => (
            config.review.acp_source.as_deref(),
            config.review.acp_priority.as_slice(),
            active_models.map(|models| models.review.as_str()),
            active_models.and_then(|models| models.review_source.as_deref()),
        ),
        SessionDefaultsSeat::Subagents => (
            config.subagents.acp_source.as_deref(),
            config.subagents.acp_priority.as_slice(),
            active_models.map(|models| models.subagent.as_str()),
            active_models.and_then(|models| models.subagent_source.as_deref()),
        ),
    };
    session_source_for_model(
        model,
        configured_source,
        priority,
        active_model,
        active_source,
        choices,
        inventory,
    )
}

/// [`seat_session_source_for_model`] for the seat's configured model.
pub fn selected_seat_session_source(
    config: &crate::config::Config,
    seat: SessionDefaultsSeat,
    active_models: Option<&crate::config::ModelsConfig>,
    choices: &[ModelChoice],
    inventory: &AcpInventory,
) -> Option<String> {
    let model = match seat {
        SessionDefaultsSeat::Primary => config.agent.model.as_str(),
        SessionDefaultsSeat::Review => config.review.model.as_str(),
        SessionDefaultsSeat::Subagents => config.subagents.model.as_str(),
    };
    seat_session_source_for_model(config, seat, model, active_models, choices, inventory)
}

/// Whether a discovered ACP option belongs in this seat's `/mjconfig` panel.
///
/// The delegated Codex and Claude `mode` control is the provider's permission
/// mode. It is owned by the explicit reviewer/subagent Permissions setting,
/// rather than a low-level session-default override. The primary agent retains
/// the option because it has no separate permission preset.
pub fn session_option_is_editable(
    seat: SessionDefaultsSeat,
    adapter_kind: AdapterKind,
    option: &SessionConfigOption,
) -> bool {
    if !matches!(option.kind, SessionConfigKind::Select(_))
        || (matches!(option.category, Some(SessionConfigOptionCategory::Model))
            && option.id.to_string() != crate::acp::REASONING_EFFORT_CONFIG_ID)
    {
        return false;
    }

    let permissions_own_mode =
        matches!(
            seat,
            SessionDefaultsSeat::Review | SessionDefaultsSeat::Subagents
        ) && matches!(adapter_kind, AdapterKind::Codex | AdapterKind::Claude);
    !(permissions_own_mode && option.id.to_string() == "mode")
}

/// Whether an ACP session option controls the model's reasoning effort.
///
/// Thought-level is the protocol-native category. Codex ACP currently tags
/// its effort selector as a model option, so retain the stable config id as a
/// compatibility fallback.
pub fn session_option_controls_reasoning_effort(option: &SessionConfigOption) -> bool {
    matches!(
        option.category,
        Some(SessionConfigOptionCategory::ThoughtLevel)
    ) || option.id.to_string() == crate::acp::REASONING_EFFORT_CONFIG_ID
}

/// Read the effective reasoning effort reported by a live ACP session.
pub fn session_reasoning_effort(options: &[SessionConfigOption]) -> Option<String> {
    options
        .iter()
        .find(|option| session_option_controls_reasoning_effort(option))
        .and_then(|option| match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
            _ => None,
        })
}

/// The live ACP session option that selects the primary model. Adapters may
/// tag their reasoning-effort selector with the same `Model` category, so
/// that option is explicitly excluded.
pub fn session_model_option(options: &[SessionConfigOption]) -> Option<&SessionConfigOption> {
    options.iter().find(|option| {
        matches!(option.category, Some(SessionConfigOptionCategory::Model))
            && !session_option_controls_reasoning_effort(option)
    })
}

/// Wire value currently selected by the live session's model option. Callers
/// compare this across config snapshots to tell a moved selection apart from
/// an alias respelling of the model already shown.
pub fn session_model_value(options: &[SessionConfigOption]) -> Option<SessionConfigValueId> {
    session_model_option(options).and_then(|option| match &option.kind {
        SessionConfigKind::Select(select) => Some(select.current_value.clone()),
        _ => None,
    })
}

/// The model a live ACP session currently runs, as a seat display name.
///
/// `None` when the session exposes no model selector or when the advertised
/// selection still corresponds to `active_model`: the canonical configured id
/// is better display than the adapter-native alias for the same model. A
/// selection that moved is reverse-resolved through the roster back to a
/// canonical model id, falling back to the advertised choice label for models
/// the roster doesn't carry.
pub fn live_session_model(
    options: &[SessionConfigOption],
    source_id: &str,
    active_model: &str,
    choices: &[ModelChoice],
) -> Option<String> {
    let option = session_model_option(options)?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let current = select.current_value.clone();
    if crate::acp::session_config_model_value(option, source_id, active_model, None).as_ref()
        == Some(&current)
    {
        return None;
    }
    choices
        .iter()
        .filter(|choice| choice.adapter.as_deref() == Some(source_id))
        .find(|choice| {
            crate::acp::session_config_model_value(option, source_id, &choice.model, None).as_ref()
                == Some(&current)
        })
        .map(|choice| choice.model.clone())
        .or_else(|| Some(select_value_label(&select.options, &current)))
}

/// Display label for one advertised select value, preferring the choice name
/// over the wire id.
fn select_value_label(
    options: &SessionConfigSelectOptions,
    value: &SessionConfigValueId,
) -> String {
    let choices: Box<dyn Iterator<Item = _>> = match options {
        SessionConfigSelectOptions::Ungrouped(options) => Box::new(options.iter()),
        SessionConfigSelectOptions::Grouped(groups) => {
            Box::new(groups.iter().flat_map(|group| group.options.iter()))
        }
        _ => Box::new(std::iter::empty()),
    };
    choices
        .into_iter()
        .find(|choice| choice.value == *value)
        .map(|choice| choice.name.trim())
        .filter(|name| !name.is_empty())
        .map_or_else(|| value.to_string(), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionConfigSelectOption;

    #[test]
    fn catalog_includes_the_input_panel() {
        assert_eq!(SettingsTab::ALL[4], SettingsTab::Input);
        assert_eq!(SettingsTab::Input.id(), "input");
        assert_eq!(SettingsTab::Input.label(), "Input");
    }

    #[test]
    fn only_builtin_servers_are_configurable() {
        assert!(is_configurable_acp_server("codex-acp"));
        assert!(is_configurable_acp_server("claude-acp"));
        assert!(!is_configurable_acp_server("anvil"));
    }

    #[test]
    fn delegated_permissions_own_codex_and_claude_mode() {
        let mode = SessionConfigOption::select(
            "mode",
            "Mode",
            "agent",
            vec![SessionConfigSelectOption::new("agent", "Agent")],
        );
        let effort = SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        );

        assert!(session_option_is_editable(
            SessionDefaultsSeat::Primary,
            AdapterKind::Codex,
            &mode,
        ));
        for seat in [SessionDefaultsSeat::Review, SessionDefaultsSeat::Subagents] {
            assert!(!session_option_is_editable(seat, AdapterKind::Codex, &mode));
            assert!(!session_option_is_editable(
                seat,
                AdapterKind::Claude,
                &mode
            ));
            assert!(session_option_is_editable(
                seat,
                AdapterKind::Codex,
                &effort
            ));
        }
    }

    fn roster_choice(model: &str, adapter: &str) -> ModelChoice {
        ModelChoice {
            model: model.to_string(),
            pass_at_1: 0.5,
            mean_cost_usd: 1.0,
            available: true,
            disabled_reason: None,
            adapter: Some(adapter.to_string()),
            ranked: true,
        }
    }

    fn model_select(current: &str) -> SessionConfigOption {
        SessionConfigOption::select(
            "model",
            "Model",
            current.to_string(),
            vec![
                SessionConfigSelectOption::new("gpt-5-6-sol", "gpt-5-6-sol"),
                SessionConfigSelectOption::new("gpt-5-6-terra", "gpt-5-6-terra"),
                SessionConfigSelectOption::new("experimental", "Experimental preview"),
            ],
        )
        .category(SessionConfigOptionCategory::Model)
    }

    #[test]
    fn live_session_model_ignores_the_model_tagged_effort_selector() {
        let effort = SessionConfigOption::select(
            crate::acp::REASONING_EFFORT_CONFIG_ID,
            "Reasoning effort",
            "xhigh",
            vec![SessionConfigSelectOption::new("xhigh", "Xhigh")],
        )
        .category(SessionConfigOptionCategory::Model);
        assert!(session_model_option(std::slice::from_ref(&effort)).is_none());
        assert_eq!(
            session_model_option(&[effort, model_select("gpt-5-6-sol")])
                .map(|option| option.id.to_string()),
            Some("model".to_string())
        );
    }

    #[test]
    fn live_session_model_keeps_the_canonical_id_while_the_selection_matches() {
        let choices = vec![roster_choice("gpt-5-6-sol", "codex-acp")];
        assert_eq!(
            live_session_model(
                &[model_select("gpt-5-6-sol")],
                "codex-acp",
                "gpt-5-6-sol",
                &choices,
            ),
            None
        );
    }

    #[test]
    fn live_session_model_follows_a_moved_selection_to_the_roster_id() {
        let choices = vec![
            roster_choice("gpt-5-6-sol", "codex-acp"),
            roster_choice("gpt-5-6-terra", "codex-acp"),
        ];
        assert_eq!(
            live_session_model(
                &[model_select("gpt-5-6-terra")],
                "codex-acp",
                "gpt-5-6-sol",
                &choices,
            ),
            Some("gpt-5-6-terra".to_string())
        );
    }

    #[test]
    fn live_session_model_falls_back_to_the_choice_label_outside_the_roster() {
        let choices = vec![roster_choice("gpt-5-6-sol", "codex-acp")];
        assert_eq!(
            live_session_model(
                &[model_select("experimental")],
                "codex-acp",
                "gpt-5-6-sol",
                &choices,
            ),
            Some("Experimental preview".to_string())
        );
    }

    #[test]
    fn live_reasoning_effort_accepts_protocol_category_and_codex_config_id() {
        let thought_level = SessionConfigOption::select(
            "thinking",
            "Thinking",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);
        assert_eq!(
            session_reasoning_effort(&[thought_level]),
            Some("high".to_string())
        );

        let codex_effort = SessionConfigOption::select(
            crate::acp::REASONING_EFFORT_CONFIG_ID,
            "Reasoning effort",
            "xhigh",
            vec![SessionConfigSelectOption::new("xhigh", "Xhigh")],
        )
        .category(SessionConfigOptionCategory::Model);
        assert_eq!(
            session_reasoning_effort(&[codex_effort]),
            Some("xhigh".to_string())
        );
    }

    #[test]
    fn explicit_model_provider_beats_the_team_auto_source() {
        let mut config = crate::roster::config_with_a_visible_builtin();
        config.set_acp_server_policy("claude-acp", crate::config::AcpServerPolicy::Enabled);
        let inventory = crate::roster::discover_inventory(&config);
        let choices = vec![ModelChoice {
            model: "gpt-provider-model".to_string(),
            pass_at_1: 0.5,
            mean_cost_usd: 1.0,
            available: true,
            disabled_reason: None,
            adapter: Some("codex-acp".to_string()),
            ranked: true,
        }];

        assert_eq!(
            session_source_for_model(
                "gpt-provider-model",
                Some("claude-acp"),
                &["claude-acp".to_string(), "codex-acp".to_string()],
                None,
                None,
                &choices,
                &inventory,
            )
            .as_deref(),
            Some("codex-acp")
        );
        assert_eq!(
            session_source_for_model(
                "auto",
                Some("claude-acp"),
                &["claude-acp".to_string(), "codex-acp".to_string()],
                None,
                None,
                &choices,
                &inventory,
            )
            .as_deref(),
            Some("claude-acp")
        );
    }
}
