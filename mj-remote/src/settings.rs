//! Shared first-startup and in-session settings editor.

use std::collections::HashSet;

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
};
use crossterm::event::KeyCode;
use mj_core::config::{AcpServerPolicy, Config, ModelsConfig, TeamPreset, ThoughtOutput};
use mj_core::roster::{AcpInventory, ModelChoice};
use mj_core::spinner::SpinnerStyle;
use mj_core::theme::TerminalThemeKind;

const ACCOUNT_COUNT: usize = mj_core::auth::AuthVendor::ALL.len();
pub const SETTINGS_PANEL_MIN_WIDTH: u16 = 28;
pub const SETTINGS_PANEL_MIN_HEIGHT: u16 = 12;
pub const SERVER_ROW_OFFSET: usize = ACCOUNT_COUNT;
pub const CONFIGURABLE_ACP_SERVERS: [&str; 2] = ["codex-acp", "claude-acp"];

pub fn is_configurable_acp_server(id: &str) -> bool {
    CONFIGURABLE_ACP_SERVERS.contains(&id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Team,
    Agents,
    Reviewer,
    Subagents,
    AcpServers,
    Appearance,
}

impl SettingsTab {
    const ALL: [Self; 6] = [
        Self::Team,
        Self::Agents,
        Self::Reviewer,
        Self::Subagents,
        Self::AcpServers,
        Self::Appearance,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Team => "Team",
            Self::Agents => "Agent",
            Self::Reviewer => "Reviewer",
            Self::Subagents => "Subagents",
            Self::AcpServers => "ACP Servers",
            Self::Appearance => "Appearance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Changed,
    Authenticate(mj_core::auth::AuthVendor),
    Save,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDefaultsSeat {
    Primary,
    Review,
    Subagents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsRow {
    PrimaryModel,
    ReviewModel,
    SubagentModel,
    SessionOption {
        seat: SessionDefaultsSeat,
        server_index: usize,
        option_index: usize,
    },
    DiscreteReview,
    ReviewTier,
    CorrectionThreshold,
    MaxParallelSubagents,
    AutomaticQuotaFailover,
}

#[derive(Debug, Clone)]
pub struct SettingsEditor {
    pub config: Config,
    pub tab: SettingsTab,
    pub selected: usize,
    pub notice: Option<String>,
    choices: Vec<ModelChoice>,
    active_models: Option<ModelsConfig>,
    active_session_config: Vec<SessionConfigOption>,
    inventory: AcpInventory,
}

impl SettingsEditor {
    pub fn new(mut config: Config, choices: Vec<ModelChoice>, notice: Option<String>) -> Self {
        // Mirror brokk-mj-tui's editor: a registered platform adapter owns the
        // team, so the editor never shows or saves routes it would reject.
        config.apply_registered_external_team();
        let inventory = mj_core::roster::discover_inventory(&config);
        Self {
            config,
            tab: SettingsTab::Team,
            selected: 0,
            notice,
            choices,
            active_models: None,
            active_session_config: Vec::new(),
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

    pub fn with_active_session_config(mut self, options: Vec<SessionConfigOption>) -> Self {
        self.active_session_config = options;
        self
    }

    /// Replace the model and ACP catalog without discarding staged settings.
    /// Keep the same logical row selected when session options change.
    pub fn update_catalog(&mut self, choices: Vec<ModelChoice>, inventory: AcpInventory) {
        let selected_row = matches!(
            self.tab,
            SettingsTab::Agents | SettingsTab::Reviewer | SettingsTab::Subagents
        )
        .then(|| self.settings_rows(self.tab).get(self.selected).copied())
        .flatten();
        self.choices = choices;
        if !inventory.servers.is_empty() {
            self.inventory = inventory;
        }
        self.selected = selected_row
            .and_then(|row| {
                self.settings_rows(self.tab)
                    .iter()
                    .position(|candidate| *candidate == row)
            })
            .unwrap_or_else(|| self.selected.min(self.row_count().saturating_sub(1)));
    }

    /// Discovered ACP inventory backing the editor's server and session rows.
    /// The remote-control server projects it into the web `/mjconfig` panel so
    /// both UIs describe the same servers with the same status strings.
    pub fn inventory(&self) -> &AcpInventory {
        &self.inventory
    }

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsAction {
        match code {
            KeyCode::Esc => SettingsAction::Cancel,
            KeyCode::Enter
                if self.tab == SettingsTab::AcpServers && self.selected < ACCOUNT_COUNT =>
            {
                SettingsAction::Authenticate(mj_core::auth::AuthVendor::ALL[self.selected])
            }
            KeyCode::Enter => SettingsAction::Save,
            KeyCode::Tab => {
                self.change_tab(1);
                SettingsAction::None
            }
            KeyCode::BackTab => {
                self.change_tab(-1);
                SettingsAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                SettingsAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                SettingsAction::None
            }
            KeyCode::Left | KeyCode::Char('h') => self.change_selected(-1),
            KeyCode::Right | KeyCode::Char('l') => self.change_selected(1),
            KeyCode::Char(' ') => self.toggle_selected(),
            _ => SettingsAction::None,
        }
    }

    fn change_tab(&mut self, delta: i32) {
        let current = SettingsTab::ALL
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(SettingsTab::ALL.len() as i32) as usize;
        self.tab = SettingsTab::ALL[next];
        self.selected = 0;
        self.notice = None;
    }

    fn row_count(&self) -> usize {
        match self.tab {
            SettingsTab::Team => 1,
            SettingsTab::Agents | SettingsTab::Reviewer | SettingsTab::Subagents => {
                self.settings_rows(self.tab).len()
            }
            SettingsTab::AcpServers => self.configurable_servers().count() + SERVER_ROW_OFFSET,
            SettingsTab::Appearance => 5,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.row_count();
        if len > 0 {
            self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
        }
    }

    fn change_selected(&mut self, delta: i32) -> SettingsAction {
        if matches!(
            self.tab,
            SettingsTab::Agents | SettingsTab::Reviewer | SettingsTab::Subagents
        ) {
            let Some(row) = self.settings_rows(self.tab).get(self.selected).copied() else {
                return SettingsAction::None;
            };
            match row {
                SettingsRow::PrimaryModel => self.cycle_model(0, delta),
                SettingsRow::ReviewModel => self.cycle_model(1, delta),
                SettingsRow::SubagentModel => self.cycle_model(2, delta),
                SettingsRow::SessionOption {
                    seat,
                    server_index,
                    option_index,
                } => {
                    if !self.change_session_option(seat, server_index, option_index, delta) {
                        return SettingsAction::None;
                    }
                }
                SettingsRow::MaxParallelSubagents => {
                    self.config.subagents.max_parallel =
                        (self.config.subagents.max_parallel as i32 + delta).rem_euclid(17) as usize;
                }
                SettingsRow::ReviewTier => self.cycle_review_tier(delta),
                SettingsRow::CorrectionThreshold => self.cycle_correction_threshold(delta),
                SettingsRow::DiscreteReview | SettingsRow::AutomaticQuotaFailover => {
                    return SettingsAction::None;
                }
            }
            self.notice = None;
            return SettingsAction::Changed;
        }
        match self.tab {
            SettingsTab::Team => {
                self.cycle_team(delta);
                return SettingsAction::Changed;
            }
            SettingsTab::AcpServers => {
                let Some(index) = self.selected.checked_sub(SERVER_ROW_OFFSET) else {
                    return SettingsAction::None;
                };
                let Some(server) = self.configurable_servers().nth(index) else {
                    return SettingsAction::None;
                };
                let id = server.id.clone();
                let choices: &[AcpServerPolicy] = &[
                    AcpServerPolicy::Auto,
                    AcpServerPolicy::Enabled,
                    AcpServerPolicy::Disabled,
                ];
                let current = choices
                    .iter()
                    .position(|policy| *policy == server.policy)
                    .unwrap_or(0);
                let next = (current as i32 + delta).rem_euclid(choices.len() as i32) as usize;
                self.config.set_acp_server_policy(&id, choices[next]);
                self.refresh_inventory();
            }
            SettingsTab::Appearance if self.selected == 0 => {
                let current = TerminalThemeKind::ALL
                    .iter()
                    .position(|kind| *kind == self.config.theme)
                    .unwrap_or(0);
                let next = (current as i32 + delta).rem_euclid(TerminalThemeKind::ALL.len() as i32)
                    as usize;
                self.config.theme = TerminalThemeKind::ALL[next];
            }
            SettingsTab::Appearance if self.selected == 1 => {
                let current = SpinnerStyle::ALL
                    .iter()
                    .position(|style| *style == self.config.spinner)
                    .unwrap_or(0);
                let next =
                    (current as i32 + delta).rem_euclid(SpinnerStyle::ALL.len() as i32) as usize;
                self.config.spinner = SpinnerStyle::ALL[next];
            }
            SettingsTab::Appearance if self.selected == 2 => {
                let current = ThoughtOutput::ALL
                    .iter()
                    .position(|output| *output == self.config.thought_output)
                    .unwrap_or(0);
                let next =
                    (current as i32 + delta).rem_euclid(ThoughtOutput::ALL.len() as i32) as usize;
                self.config.thought_output = ThoughtOutput::ALL[next];
            }
            SettingsTab::Appearance if self.selected == 3 => {
                self.config.feature_hints = !self.config.feature_hints;
            }
            SettingsTab::Appearance if self.selected == 4 => {
                self.config.keep_awake = !self.config.keep_awake;
            }
            _ => return SettingsAction::None,
        }
        self.notice = None;
        SettingsAction::Changed
    }

    fn toggle_selected(&mut self) -> SettingsAction {
        if matches!(
            self.tab,
            SettingsTab::Agents | SettingsTab::Reviewer | SettingsTab::Subagents
        ) {
            let Some(row) = self.settings_rows(self.tab).get(self.selected).copied() else {
                return SettingsAction::None;
            };
            match row {
                SettingsRow::DiscreteReview => {
                    self.config.agent.discrete_review = !self.config.agent.discrete_review;
                }
                // Two tiers, so the toggle key advances the same way the
                // left/right keys do rather than doing nothing here.
                SettingsRow::ReviewTier => self.cycle_review_tier(1),
                SettingsRow::CorrectionThreshold => self.cycle_correction_threshold(1),
                SettingsRow::AutomaticQuotaFailover => {
                    self.config.subagents.auto_failover = !self.config.subagents.auto_failover;
                }
                _ => return SettingsAction::None,
            }
            self.notice = None;
            return SettingsAction::Changed;
        }
        match self.tab {
            SettingsTab::Team => {
                self.cycle_team(1);
                return SettingsAction::Changed;
            }
            SettingsTab::AcpServers => {
                let Some(index) = self.selected.checked_sub(SERVER_ROW_OFFSET) else {
                    return SettingsAction::None;
                };
                let Some(server) = self.configurable_servers().nth(index) else {
                    return SettingsAction::None;
                };
                let id = server.id.clone();
                let policy = if server.policy == AcpServerPolicy::Auto && !server.detected {
                    AcpServerPolicy::Enabled
                } else if server.policy == AcpServerPolicy::Disabled {
                    AcpServerPolicy::Auto
                } else {
                    AcpServerPolicy::Disabled
                };
                self.config.set_acp_server_policy(&id, policy);
                self.refresh_inventory();
            }
            _ => return SettingsAction::None,
        }
        self.notice = None;
        SettingsAction::Changed
    }

    fn settings_rows(&self, tab: SettingsTab) -> Vec<SettingsRow> {
        match tab {
            SettingsTab::Agents => {
                let mut rows = vec![SettingsRow::PrimaryModel];
                rows.extend(
                    self.session_option_rows(SessionDefaultsSeat::Primary)
                        .into_iter()
                        .map(|(server_index, option_index)| SettingsRow::SessionOption {
                            seat: SessionDefaultsSeat::Primary,
                            server_index,
                            option_index,
                        }),
                );
                rows
            }
            SettingsTab::Reviewer => {
                let mut rows = vec![SettingsRow::ReviewModel];
                rows.extend(
                    self.session_option_rows(SessionDefaultsSeat::Review)
                        .into_iter()
                        .map(|(server_index, option_index)| SettingsRow::SessionOption {
                            seat: SessionDefaultsSeat::Review,
                            server_index,
                            option_index,
                        }),
                );
                rows.push(SettingsRow::DiscreteReview);
                rows.push(SettingsRow::ReviewTier);
                rows.push(SettingsRow::CorrectionThreshold);
                rows
            }
            SettingsTab::Subagents => {
                let mut rows = vec![SettingsRow::SubagentModel];
                rows.extend(
                    self.session_option_rows(SessionDefaultsSeat::Subagents)
                        .into_iter()
                        .map(|(server_index, option_index)| SettingsRow::SessionOption {
                            seat: SessionDefaultsSeat::Subagents,
                            server_index,
                            option_index,
                        }),
                );
                rows.push(SettingsRow::MaxParallelSubagents);
                rows.push(SettingsRow::AutomaticQuotaFailover);
                rows
            }
            _ => Vec::new(),
        }
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
            .filter(|(_, option)| {
                matches!(option.kind, SessionConfigKind::Select(_))
                    && !matches!(
                        option.category,
                        Some(agent_client_protocol::schema::v1::SessionConfigOptionCategory::Model)
                    )
            })
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

    fn session_defaults_mut(
        &mut self,
        seat: SessionDefaultsSeat,
    ) -> &mut std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
        match seat {
            SessionDefaultsSeat::Primary => &mut self.config.agent.session_defaults,
            SessionDefaultsSeat::Review => &mut self.config.review.session_defaults,
            SessionDefaultsSeat::Subagents => &mut self.config.subagents.session_defaults,
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

    fn change_session_option(
        &mut self,
        seat: SessionDefaultsSeat,
        server_index: usize,
        option_index: usize,
        delta: i32,
    ) -> bool {
        let Some(server) = self.inventory.servers.get(server_index) else {
            return false;
        };
        let Some(option) = server.session_config.get(option_index).cloned() else {
            return false;
        };
        let server_id = server.id.clone();
        let choices = session_option_choices(&option);
        if choices.is_empty() {
            return false;
        }
        let current = self.saved_session_value(seat, &server_id, &option);
        let next = choices
            .iter()
            .position(|(value, _)| value == &current)
            .map(|index| (index as i32 + delta).rem_euclid(choices.len() as i32) as usize)
            .unwrap_or_else(|| if delta < 0 { choices.len() - 1 } else { 0 });
        let value = choices[next].0.clone();
        self.session_defaults_mut(seat)
            .entry(server_id)
            .or_default()
            .insert(
                mj_core::acp::session_config_option_key(&option.id),
                value.clone(),
            );
        if session_option_controls_reasoning_effort(&option) {
            match seat {
                SessionDefaultsSeat::Primary => {
                    self.config.agent.reasoning_effort = Some(value);
                }
                SessionDefaultsSeat::Review => {
                    self.config.review.reasoning_effort = Some(value);
                }
                SessionDefaultsSeat::Subagents => {
                    self.config.subagents.reasoning_effort = Some(value);
                }
            }
        }
        true
    }

    fn active_primary_session_value(&self, option: &SessionConfigOption) -> Option<String> {
        let selected_source = self.selected_session_source(SessionDefaultsSeat::Primary)?;
        let active_source = self.active_models.as_ref()?.primary_source.as_deref()?;
        if selected_source != active_source {
            return None;
        }
        self.active_session_config
            .iter()
            .find(|active| active.id == option.id)
            .map(session_option_current_value)
    }

    fn cycle_model(&mut self, role: usize, delta: i32) {
        let choices = self.model_choices(role);
        let current = match role {
            0 => &self.config.agent.model,
            1 => &self.config.review.model,
            2 => &self.config.subagents.model,
            _ => return,
        };
        let index = choices
            .iter()
            .position(|choice| choice == current)
            .unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(choices.len() as i32) as usize;
        let model = &choices[next];
        match role {
            0 => self.config.agent.model.clone_from(model),
            1 => self.config.review.model.clone_from(model),
            2 => self.config.subagents.model.clone_from(model),
            _ => {}
        }

        // An explicit model choice also selects the ACP adapter that
        // advertised it. Auto retains its source pin because Team presets use
        // that pin to constrain automatic selection.
        if model != "auto"
            && model != mj_core::config::DISABLED_MODEL
            && let Some(source) = self
                .choices
                .iter()
                .find(|choice| choice.available && choice.model == *model)
                .and_then(|choice| choice.adapter.clone())
        {
            match role {
                0 => self.config.agent.acp_source = Some(source),
                1 => self.config.review.acp_source = Some(source),
                2 => self.config.subagents.acp_source = Some(source),
                _ => {}
            }
        }
    }

    fn cycle_review_tier(&mut self, delta: i32) {
        let tiers = mj_core::config::ReviewTier::ALL;
        let current = tiers
            .iter()
            .position(|tier| *tier == self.config.agent.review_tier)
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(tiers.len() as i32) as usize;
        self.config.agent.review_tier = tiers[next];
    }

    fn cycle_correction_threshold(&mut self, delta: i32) {
        let thresholds = mj_core::config::ReviewCorrectionThreshold::ALL;
        let current = thresholds
            .iter()
            .position(|threshold| *threshold == self.config.agent.correction_threshold)
            .unwrap_or(thresholds.len() - 1);
        let next = (current as i32 + delta).rem_euclid(thresholds.len() as i32) as usize;
        self.config.agent.correction_threshold = thresholds[next];
    }

    fn cycle_team(&mut self, delta: i32) {
        // A registered platform adapter (e.g. Anvil on Android) is the only
        // team; applying a built-in preset would wipe seat model pins and
        // enable adapters that cannot run on this build.
        if self.config.apply_registered_external_team() {
            return;
        }
        let current = TeamPreset::from_config(&self.config)
            .and_then(|active| TeamPreset::ALL.iter().position(|preset| *preset == active))
            .unwrap_or_else(|| {
                if delta < 0 {
                    0
                } else {
                    TeamPreset::ALL.len() - 1
                }
            });
        let next = (current as i32 + delta).rem_euclid(TeamPreset::ALL.len() as i32) as usize;
        TeamPreset::ALL[next].apply(&mut self.config);
        self.refresh_inventory();
        self.notice =
            Some("Team updated; start a new session or restart Mjolnir to apply it.".to_string());
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

    pub fn refresh_after_auth(&mut self, notice: String) {
        self.refresh_inventory();
        self.notice = Some(notice);
    }

    fn refresh_inventory(&mut self) {
        self.inventory = mj_core::roster::rediscover_inventory(&self.config, &self.inventory);
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    fn configurable_servers(&self) -> impl Iterator<Item = &mj_core::roster::AcpServerInfo> {
        self.inventory
            .servers
            .iter()
            .filter(|server| is_configurable_acp_server(&server.id))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_input_cycles_the_builtin_preset_without_a_platform_adapter() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert!(TeamPreset::from_config(&editor.config).is_some());
    }
}
