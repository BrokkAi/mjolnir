//! Derived `/mjconfig` catalog for the remote viewer.

use std::collections::HashSet;

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
};
use mj_core::config::{AcpServerPolicy, Config, ModelsConfig};
use mj_core::roster::{AcpInventory, ModelChoice};
use mj_core::settings::session_option_is_editable;
pub use mj_core::settings::{SessionDefaultsSeat, is_configurable_acp_server};

#[derive(Debug, Clone)]
pub struct MjConfigCatalog {
    pub config: Config,
    choices: Vec<ModelChoice>,
    active_models: Option<ModelsConfig>,
    inventory: AcpInventory,
}

impl MjConfigCatalog {
    pub fn new(mut config: Config, choices: Vec<ModelChoice>) -> Self {
        config.apply_registered_external_team();
        let inventory = mj_core::roster::discover_inventory(&config);
        Self {
            config,
            choices,
            active_models: None,
            inventory,
        }
    }

    pub fn with_inventory(mut self, inventory: AcpInventory) -> Self {
        if !inventory.servers.is_empty() {
            self.inventory = inventory;
        }
        self
    }

    pub fn with_active_models(mut self, active_models: ModelsConfig) -> Self {
        self.active_models = Some(active_models);
        self
    }

    /// Whether the catalog holds any model this machine can actually launch.
    pub fn any_model_launchable(&self) -> bool {
        self.choices.iter().any(|choice| choice.available)
    }

    pub fn inventory(&self) -> &AcpInventory {
        &self.inventory
    }

    pub fn selected_session_source(&self, seat: SessionDefaultsSeat) -> Option<String> {
        let (model, configured_source, priority, active_model, active_source) = match seat {
            SessionDefaultsSeat::Primary => (
                self.config.agent.model.as_str(),
                self.config.agent.acp_source.as_deref(),
                self.config.agent.acp_priority.as_slice(),
                self.active_models
                    .as_ref()
                    .map(|models| models.primary.as_str()),
                self.active_models
                    .as_ref()
                    .and_then(|models| models.primary_source.as_deref()),
            ),
            SessionDefaultsSeat::Review => (
                self.config.review.model.as_str(),
                self.config.review.acp_source.as_deref(),
                self.config.review.acp_priority.as_slice(),
                self.active_models
                    .as_ref()
                    .map(|models| models.review.as_str()),
                self.active_models
                    .as_ref()
                    .and_then(|models| models.review_source.as_deref()),
            ),
            SessionDefaultsSeat::Subagents => (
                self.config.subagents.model.as_str(),
                self.config.subagents.acp_source.as_deref(),
                self.config.subagents.acp_priority.as_slice(),
                self.active_models
                    .as_ref()
                    .map(|models| models.subagent.as_str()),
                self.active_models
                    .as_ref()
                    .and_then(|models| models.subagent_source.as_deref()),
            ),
        };
        if model == mj_core::config::DISABLED_MODEL {
            return None;
        }
        if let Some(source) = configured_source
            && self
                .inventory
                .servers
                .iter()
                .any(|server| server.id == source)
        {
            return Some(source.to_string());
        }
        if (model == "auto" || active_model == Some(model))
            && let Some(source) = active_source
            && self
                .inventory
                .servers
                .iter()
                .any(|server| server.id == source)
        {
            return Some(source.to_string());
        }
        if model != "auto"
            && let Some(source) = priority.iter().find(|source| {
                self.choices.iter().any(|choice| {
                    choice.available
                        && choice.model == model
                        && choice.adapter.as_deref() == Some(source.as_str())
                })
            })
            && self
                .inventory
                .servers
                .iter()
                .any(|server| server.id == source.as_str())
        {
            return Some(source.clone());
        }
        if model != "auto"
            && let Some(source) = self
                .choices
                .iter()
                .find(|choice| choice.available && choice.model == model)
                .and_then(|choice| choice.adapter.as_deref())
            && self
                .inventory
                .servers
                .iter()
                .any(|server| server.id == source)
        {
            return Some(source.to_string());
        }
        priority
            .iter()
            .find(|source| {
                self.inventory.servers.iter().any(|server| {
                    server.id == source.as_str()
                        && server.policy != AcpServerPolicy::Disabled
                        && !server.session_config.is_empty()
                })
            })
            .cloned()
    }

    pub fn session_option_rows(&self, seat: SessionDefaultsSeat) -> Vec<(usize, usize)> {
        let Some(source) = self.selected_session_source(seat) else {
            return Vec::new();
        };
        let Some((server_index, server)) = self
            .inventory
            .servers
            .iter()
            .enumerate()
            .find(|(_, server)| server.id == source)
        else {
            return Vec::new();
        };
        server
            .session_config
            .iter()
            .enumerate()
            .filter(|(_, option)| session_option_is_editable(seat, server.launch.kind, option))
            .map(|(option_index, _)| (server_index, option_index))
            .collect()
    }

    fn session_defaults(
        &self,
        seat: SessionDefaultsSeat,
    ) -> &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
        match seat {
            SessionDefaultsSeat::Primary => &self.config.agent.session_defaults,
            SessionDefaultsSeat::Review => &self.config.review.session_defaults,
            SessionDefaultsSeat::Subagents => &self.config.subagents.session_defaults,
        }
    }

    pub fn saved_session_value(
        &self,
        seat: SessionDefaultsSeat,
        server_id: &str,
        option: &SessionConfigOption,
    ) -> String {
        self.session_defaults(seat)
            .get(server_id)
            .and_then(|defaults| defaults.get(&mj_core::acp::session_config_option_key(&option.id)))
            .or_else(|| {
                self.config.session_config.get(server_id).and_then(|saved| {
                    saved
                        .defaults
                        .get(&mj_core::acp::session_config_option_key(&option.id))
                })
            })
            .cloned()
            .unwrap_or_else(|| session_option_current_value(option))
    }

    pub fn model_choices(&self, role: usize) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut choices = vec!["auto".to_string()];
        seen.insert("auto".to_string());
        if role == 2 {
            choices.push(mj_core::config::DISABLED_MODEL.to_string());
            seen.insert(mj_core::config::DISABLED_MODEL.to_string());
        }
        for choice in self.choices.iter().filter(|choice| choice.available) {
            if seen.insert(choice.model.clone()) {
                choices.push(choice.model.clone());
            }
        }
        choices
    }

    pub fn staged_model_detail(&self, model: &str) -> String {
        if model == "auto" {
            return "automatic selection".to_string();
        }
        if model == mj_core::config::DISABLED_MODEL {
            return "role disabled".to_string();
        }
        let Some(choice) = self.choices.iter().find(|choice| choice.model == model) else {
            return "saved model; not reported this session".to_string();
        };
        if !choice.available {
            return format!(
                "unavailable: {}",
                choice
                    .disabled_reason
                    .as_deref()
                    .unwrap_or("no launchable ACP route")
            );
        }
        if choice.ranked {
            format!(
                "Pass@1 {:.1}%; ${:.2}",
                choice.pass_at_1 * 100.0,
                choice.mean_cost_usd
            )
        } else {
            "unranked".to_string()
        }
    }

    pub fn staged_model_warning(&self, model: &str) -> Option<String> {
        if model == "auto" || model == mj_core::config::DISABLED_MODEL {
            return None;
        }
        match self.choices.iter().find(|choice| choice.model == model) {
            Some(choice) if !choice.available => Some(format!(
                "unavailable: {}",
                choice
                    .disabled_reason
                    .as_deref()
                    .unwrap_or("no launchable ACP route")
            )),
            None => Some("not reported this session".to_string()),
            _ => None,
        }
    }

    pub fn active_model(&self, role: usize) -> Option<&str> {
        let models = self.active_models.as_ref()?;
        Some(match role {
            0 => models.primary.as_str(),
            1 => models.review.as_str(),
            _ => models.subagent.as_str(),
        })
    }

    pub fn active_model_detail(&self, role: usize) -> String {
        let Some(models) = self.active_models.as_ref() else {
            return "not running".to_string();
        };
        let (model, source) = match role {
            0 => (&models.primary, models.primary_source.as_deref()),
            1 => (&models.review, models.review_source.as_deref()),
            _ => (&models.subagent, models.subagent_source.as_deref()),
        };
        if let Some(source) = source {
            return format!("{model} via {source}");
        }
        let adapter = self
            .choices
            .iter()
            .find(|choice| choice.available && choice.model == *model)
            .and_then(|choice| choice.adapter.as_deref());
        adapter.map_or_else(|| model.clone(), |adapter| format!("{model} via {adapter}"))
    }
}
/// After a save disables an ACP server, a pinned seat model can lose its only
/// known route. Rather than letting the next session (or server restart) fail
/// to resolve, flip that seat back to automatic selection and explain why.
/// The inventory must already reflect the edited config's policies. Returns
/// one human-readable notice per changed seat.
pub fn reset_unroutable_models(config: &mut Config, choices: &[ModelChoice]) -> Vec<String> {
    // Only an explicit `disabled` policy strands a route. Absence from the
    // discovered inventory is not proof: an undetected server (not signed in,
    // not probed yet) may still serve the model once it comes back.
    let source_disabled =
        |config: &Config, source: &str| config.acp.policy(source) == AcpServerPolicy::Disabled;
    let mut notices = Vec::new();
    enum Seat {
        Agent,
        Review,
        Subagents,
    }
    for (label, seat) in [
        ("Agent", Seat::Agent),
        ("Review", Seat::Review),
        ("Subagents", Seat::Subagents),
    ] {
        let model = match seat {
            Seat::Agent => config.agent.model.clone(),
            Seat::Review => config.review.model.clone(),
            Seat::Subagents => config.subagents.model.clone(),
        };
        if model == "auto" || model == mj_core::config::DISABLED_MODEL {
            continue;
        }
        // Judge the route from the model catalog when it knows the model,
        // falling back to the provider's native adapter — the catalog may
        // have been resolved while that vendor was disabled and lack the
        // entry entirely.
        let route = choices
            .iter()
            .find(|choice| choice.model == model)
            .and_then(|choice| choice.adapter.clone())
            .or_else(|| mj_core::roster::native_source_id(&model));
        // An explicit model choice with a source pin to another adapter could
        // never resolve; move the pin to the route that serves the model.
        if let Some(route) = route.as_deref() {
            let source_slot = match seat {
                Seat::Agent => &mut config.agent.acp_source,
                Seat::Review => &mut config.review.acp_source,
                Seat::Subagents => &mut config.subagents.acp_source,
            };
            if source_slot.as_deref().is_some_and(|source| source != route) {
                *source_slot = Some(route.to_string());
                notices.push(format!(
                    "{label} ACP source moved to {route}, which serves the selected model {model}"
                ));
            }
        }
        let seat_source = match seat {
            Seat::Agent => config.agent.acp_source.clone(),
            Seat::Review => config.review.acp_source.clone(),
            Seat::Subagents => config.subagents.acp_source.clone(),
        };
        let seat_source_is_disabled = seat_source
            .as_deref()
            .is_some_and(|source| source_disabled(config, source));
        // Adapter-advertised aliases (e.g. claude-acp's `haiku`) have no
        // derivable provider; only their catalog entry can judge them. When
        // absent, keep the pin — an undetected server may still serve it —
        // unless the seat's own source is explicitly disabled, in which case
        // the alias can never resolve and falls through to the reset below.
        if mj_core::deepswe::model_provider(&model).is_empty()
            && !choices.iter().any(|choice| choice.model == model)
            && !seat_source_is_disabled
        {
            continue;
        }
        // No catalog entry and no built-in adapter for the model's provider
        // means nothing enabled can serve the pin either.
        if route
            .as_deref()
            .is_none_or(|route| source_disabled(config, route))
        {
            let (model_slot, source_slot) = match seat {
                Seat::Agent => (&mut config.agent.model, &mut config.agent.acp_source),
                Seat::Review => (&mut config.review.model, &mut config.review.acp_source),
                Seat::Subagents => (
                    &mut config.subagents.model,
                    &mut config.subagents.acp_source,
                ),
            };
            "auto".clone_into(model_slot);
            // A source pin to a disabled adapter would still abort roster
            // assembly ("no launchable models"); clear it with the model.
            let source_note = if seat_source_is_disabled {
                *source_slot = None;
                " and its disabled ACP source pin was cleared"
            } else {
                ""
            };
            notices.push(format!(
                "{label} model {model} is not provided by any enabled ACP server; switched to automatic selection{source_note}"
            ));
        }
    }
    notices
}

pub fn session_option_choices(option: &SessionConfigOption) -> Vec<(String, String)> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return Vec::new();
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options
            .iter()
            .map(|choice| (choice.value.to_string(), choice.name.clone()))
            .collect(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .map(|choice| (choice.value.to_string(), choice.name.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn session_option_current_value(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => select.current_value.to_string(),
        _ => String::new(),
    }
}

pub fn session_option_controls_reasoning_effort(option: &SessionConfigOption) -> bool {
    matches!(
        option.category,
        Some(agent_client_protocol::schema::v1::SessionConfigOptionCategory::ThoughtLevel)
    ) || option.id.to_string() == mj_core::acp::REASONING_EFFORT_CONFIG_ID
}
