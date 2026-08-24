//! Shared first-startup and in-session settings editor.

use std::collections::HashSet;

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::{
    AcpServerPolicy, Config, InterfaceMode, ModelsConfig, PermissionPreset, TeamPreset,
    ThoughtOutput, VoiceAutoSend,
};
use crate::ink::InkStyle;
use crate::palette::TerminalTheme;
use crate::palette::TerminalThemeKindExt;
use crate::roster::{AcpInventory, ModelChoice};
use crate::spinner::SpinnerStyle;
use crate::theme::TerminalThemeKind;
pub(crate) use mj_core::settings::{SessionDefaultsSeat, SettingsTab, session_option_is_editable};

const ACCOUNT_COUNT: usize = crate::auth::AuthVendor::ALL.len();
pub(crate) const SETTINGS_PANEL_MIN_WIDTH: u16 = 28;
pub(crate) const SETTINGS_PANEL_MIN_HEIGHT: u16 = 12;
pub(crate) const SERVER_ROW_OFFSET: usize = ACCOUNT_COUNT;
pub(crate) use mj_core::settings::is_configurable_acp_server;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Changed,
    Authenticate(crate::auth::AuthVendor),
    Save,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsRow {
    PrimaryModel,
    ReviewModel,
    ReviewPermissions,
    SubagentModel,
    SubagentPermissions,
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
        config.apply_registered_external_team();
        let inventory = crate::roster::discover_inventory(&config);
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

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsAction {
        match code {
            KeyCode::Esc => SettingsAction::Cancel,
            KeyCode::Enter
                if self.tab == SettingsTab::AcpServers && self.selected < ACCOUNT_COUNT =>
            {
                SettingsAction::Authenticate(crate::auth::AuthVendor::ALL[self.selected])
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
            SettingsTab::Input => 1,
            SettingsTab::Appearance => 6,
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
                SettingsRow::ReviewPermissions => {
                    cycle_permission_preset(&mut self.config.review.permission, delta)
                }
                SettingsRow::SubagentPermissions => {
                    cycle_permission_preset(&mut self.config.subagents.permission, delta)
                }
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
            SettingsTab::Appearance if self.selected == 5 => {
                let current = InterfaceMode::ALL
                    .iter()
                    .position(|mode| *mode == self.config.interface)
                    .unwrap_or(0);
                let next =
                    (current as i32 + delta).rem_euclid(InterfaceMode::ALL.len() as i32) as usize;
                self.config.interface = InterfaceMode::ALL[next];
            }
            SettingsTab::Input if self.selected == 0 => {
                let current = VoiceAutoSend::ALL
                    .iter()
                    .position(|setting| *setting == self.config.voice_auto_send)
                    .unwrap_or(0);
                let next =
                    (current as i32 + delta).rem_euclid(VoiceAutoSend::ALL.len() as i32) as usize;
                self.config.voice_auto_send = VoiceAutoSend::ALL[next];
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
                let mut rows = vec![SettingsRow::ReviewModel, SettingsRow::ReviewPermissions];
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
                let mut rows = vec![SettingsRow::SubagentModel, SettingsRow::SubagentPermissions];
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

    pub(crate) fn selected_session_source(&self, seat: SessionDefaultsSeat) -> Option<String> {
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
        if model == crate::config::DISABLED_MODEL {
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

    pub(crate) fn session_option_rows(&self, seat: SessionDefaultsSeat) -> Vec<(usize, usize)> {
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

    pub(crate) fn saved_session_value(
        &self,
        seat: SessionDefaultsSeat,
        server_id: &str,
        option: &SessionConfigOption,
    ) -> String {
        self.session_defaults(seat)
            .get(server_id)
            .and_then(|defaults| defaults.get(&crate::acp::session_config_option_key(&option.id)))
            .or_else(|| {
                self.config.session_config.get(server_id).and_then(|saved| {
                    saved
                        .defaults
                        .get(&crate::acp::session_config_option_key(&option.id))
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
                crate::acp::session_config_option_key(&option.id),
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
            && model != crate::config::DISABLED_MODEL
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
        let tiers = crate::config::ReviewTier::ALL;
        let current = tiers
            .iter()
            .position(|tier| *tier == self.config.agent.review_tier)
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(tiers.len() as i32) as usize;
        self.config.agent.review_tier = tiers[next];
    }

    fn cycle_correction_threshold(&mut self, delta: i32) {
        let thresholds = crate::config::ReviewCorrectionThreshold::ALL;
        let current = thresholds
            .iter()
            .position(|threshold| *threshold == self.config.agent.correction_threshold)
            .unwrap_or(thresholds.len() - 1);
        let next = (current as i32 + delta).rem_euclid(thresholds.len() as i32) as usize;
        self.config.agent.correction_threshold = thresholds[next];
    }

    fn cycle_team(&mut self, delta: i32) {
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
        self.notice = Some("Team updated; save to apply it.".to_string());
    }

    pub(crate) fn model_choices(&self, role: usize) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut choices = vec!["auto".to_string()];
        seen.insert("auto".to_string());
        if role == 2 {
            choices.push(crate::config::DISABLED_MODEL.to_string());
            seen.insert(crate::config::DISABLED_MODEL.to_string());
        }
        for choice in self.choices.iter().filter(|choice| choice.available) {
            if seen.insert(choice.model.clone()) {
                choices.push(choice.model.clone());
            }
        }
        choices
    }

    pub(crate) fn staged_model_warning(&self, model: &str) -> Option<String> {
        if model == "auto" || model == crate::config::DISABLED_MODEL {
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

    pub(crate) fn active_model(&self, role: usize) -> Option<&str> {
        let models = self.active_models.as_ref()?;
        Some(match role {
            0 => models.primary.as_str(),
            1 => models.review.as_str(),
            _ => models.subagent.as_str(),
        })
    }

    pub(crate) fn active_model_detail(&self, role: usize) -> String {
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

    pub(crate) fn refresh_after_auth(&mut self, notice: String) {
        self.refresh_inventory();
        self.notice = Some(notice);
    }

    fn refresh_inventory(&mut self) {
        self.inventory = crate::roster::rediscover_inventory(&self.config, &self.inventory);
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    fn configurable_servers(&self) -> impl Iterator<Item = &crate::roster::AcpServerInfo> {
        self.inventory
            .servers
            .iter()
            .filter(|server| is_configurable_acp_server(&server.id))
    }
}

pub fn draw_settings_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    title: &str,
) {
    if area.width < SETTINGS_PANEL_MIN_WIDTH || area.height < SETTINGS_PANEL_MIN_HEIGHT {
        return;
    }
    let theme = editor.config.theme.palette();
    let rect = crate::term::centered_rect(area, 90, 24);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .style(Style::default().ink(theme.text));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(if editor.notice.is_some() { 2 } else { 0 }),
            Constraint::Length(1),
        ])
        .split(inner);
    draw_tabs(frame, rows[0], editor, theme);
    match editor.tab {
        SettingsTab::Team => draw_team(frame, rows[1], editor, theme),
        SettingsTab::Agents => draw_agents(frame, rows[1], editor, theme),
        SettingsTab::Reviewer => draw_reviewer(frame, rows[1], editor, theme),
        SettingsTab::Subagents => draw_subagents(frame, rows[1], editor, theme),
        SettingsTab::AcpServers => draw_servers(frame, rows[1], editor, theme),
        SettingsTab::Input => draw_input_settings(frame, rows[1], editor, theme),
        SettingsTab::Appearance => draw_appearance(frame, rows[1], editor, theme),
    }
    if let Some(notice) = &editor.notice {
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .style(Style::default().ink(theme.error))
                .wrap(Wrap { trim: false }),
            rows[2],
        );
    }
    let footer = if editor.tab == SettingsTab::AcpServers && editor.selected < ACCOUNT_COUNT {
        "Enter sign in · ↑/↓ select · Tab view · Esc cancel"
    } else {
        "Tab view · ↑/↓ select · ←/→ change · Space toggle · Enter save · Esc cancel"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().ink(theme.muted)),
        rows[3],
    );
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
        if model == "auto" || model == crate::config::DISABLED_MODEL {
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
            .or_else(|| crate::roster::native_source_id(&model));
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
        if crate::deepswe::model_provider(&model).is_empty()
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

pub(crate) fn session_option_choices(option: &SessionConfigOption) -> Vec<(String, String)> {
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

pub(crate) fn session_option_current_value(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => select.current_value.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn session_option_controls_reasoning_effort(option: &SessionConfigOption) -> bool {
    matches!(
        option.category,
        Some(agent_client_protocol::schema::v1::SessionConfigOptionCategory::ThoughtLevel)
    ) || option.id.to_string() == crate::acp::REASONING_EFFORT_CONFIG_ID
}

fn draw_tabs(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let tabs = SettingsTab::ALL.into_iter().flat_map(|tab| {
        let active = tab == editor.tab;
        let style = if active {
            Style::default()
                .ink(theme.selection_fg)
                .ink_bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().ink(theme.muted)
        };
        [
            Span::styled(format!(" {} ", tab.label()), style),
            Span::raw("  "),
        ]
    });
    frame.render_widget(Paragraph::new(Line::from(tabs.collect::<Vec<_>>())), area);
}

fn session_options_heading(
    editor: &SettingsEditor,
    source: Option<&str>,
    theme: TerminalTheme,
) -> Line<'static> {
    let label = source.map_or("ACP".to_string(), |source| {
        editor
            .inventory
            .servers
            .iter()
            .find(|server| server.id == source)
            .map_or_else(|| source.to_string(), |server| server.label.clone())
    });
    Line::styled(
        format!("Session options · {label}"),
        Style::default()
            .ink(theme.muted)
            .add_modifier(Modifier::BOLD),
    )
}

fn model_lines(
    selected: bool,
    label: &str,
    model: &str,
    role: usize,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    let warning = editor.staged_model_warning(model);
    let active = editor
        .active_model(role)
        .filter(|active| *active != model)
        .map(|_| format!("active: {}", editor.active_model_detail(role)));
    let (detail, trailing_active) = match (warning, active) {
        (Some(warning), Some(active)) => (Some(warning), Some(active)),
        (warning, active) => (warning.or(active), None),
    };
    let mut lines = vec![selected_line_with_detail(
        selected,
        format!("{label} < {model} >"),
        detail,
        theme,
    )];
    if let Some(active) = trailing_active {
        lines.push(Line::styled(
            format!("  {active}"),
            Style::default().ink(theme.muted),
        ));
    }
    lines
}

fn permission_lines(
    selected: bool,
    permission: PermissionPreset,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    vec![
        selected_line(selected, format!("Permissions < {permission} >"), theme),
        Line::styled(
            format!("  {}", permission.description()),
            Style::default().ink(theme.muted),
        ),
    ]
}

fn cycle_permission_preset(permission: &mut PermissionPreset, delta: i32) {
    let current = PermissionPreset::ALL
        .iter()
        .position(|candidate| *candidate == *permission)
        .unwrap_or(0);
    let next = (current as i32 + delta).rem_euclid(PermissionPreset::ALL.len() as i32) as usize;
    *permission = PermissionPreset::ALL[next];
}

fn draw_agents(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let rows = editor.settings_rows(SettingsTab::Agents);
    let source = editor.selected_session_source(SessionDefaultsSeat::Primary);
    let has_options = rows
        .iter()
        .any(|row| matches!(row, SettingsRow::SessionOption { .. }));
    let mut lines = Vec::new();
    let mut selected_line_index = 0;
    for (row_index, row) in rows.into_iter().enumerate() {
        if row_index == 1 && has_options {
            lines.push(Line::raw(""));
            lines.push(session_options_heading(editor, source.as_deref(), theme));
        }
        let selected = editor.selected == row_index;
        if selected {
            selected_line_index = lines.len();
        }
        match row {
            SettingsRow::PrimaryModel => {
                let model = &editor.config.agent.model;
                lines.extend(model_lines(
                    selected,
                    "Primary model",
                    model,
                    0,
                    editor,
                    theme,
                ));
            }
            SettingsRow::SessionOption {
                server_index,
                option_index,
                ..
            } => {
                let server = &editor.inventory.servers[server_index];
                let option = &server.session_config[option_index];
                let saved =
                    editor.saved_session_value(SessionDefaultsSeat::Primary, &server.id, option);
                let (saved_label, compatible) = session_option_value_label(option, &saved);
                let active = editor
                    .active_primary_session_value(option)
                    .map(|value| session_option_value_label(option, &value).0)
                    .filter(|active| active != &saved_label)
                    .map(|active| format!("active: {active}"));
                lines.push(selected_line_with_detail(
                    selected,
                    format!("{} < {saved_label} >", option.name),
                    active,
                    theme,
                ));
                if !compatible {
                    lines.push(Line::styled(
                        format!("  unavailable on {}", server.id),
                        Style::default().ink(theme.error),
                    ));
                }
            }
            SettingsRow::ReviewModel
            | SettingsRow::ReviewPermissions
            | SettingsRow::SubagentModel
            | SettingsRow::SubagentPermissions
            | SettingsRow::DiscreteReview
            | SettingsRow::ReviewTier
            | SettingsRow::CorrectionThreshold
            | SettingsRow::MaxParallelSubagents
            | SettingsRow::AutomaticQuotaFailover => {}
        }
    }
    draw_scrolling_settings_lines(frame, area, lines, selected_line_index);
}

fn draw_reviewer(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let rows = editor.settings_rows(SettingsTab::Reviewer);
    let source = editor.selected_session_source(SessionDefaultsSeat::Review);
    let has_options = rows
        .iter()
        .any(|row| matches!(row, SettingsRow::SessionOption { .. }));
    let mut lines = Vec::new();
    let mut selected_line_index = 0;
    let mut session_options_heading_drawn = false;
    for (row_index, row) in rows.into_iter().enumerate() {
        if matches!(row, SettingsRow::SessionOption { .. }) && !session_options_heading_drawn {
            lines.push(Line::raw(""));
            lines.push(session_options_heading(editor, source.as_deref(), theme));
            session_options_heading_drawn = true;
        }
        if has_options && matches!(row, SettingsRow::DiscreteReview) {
            lines.push(Line::raw(""));
        }
        let selected = editor.selected == row_index;
        if selected {
            selected_line_index = lines.len();
        }
        match row {
            SettingsRow::ReviewModel => {
                let model = &editor.config.review.model;
                lines.extend(model_lines(
                    selected,
                    "Review model",
                    model,
                    1,
                    editor,
                    theme,
                ));
            }
            SettingsRow::ReviewPermissions => {
                lines.extend(permission_lines(
                    selected,
                    editor.config.review.permission,
                    theme,
                ));
            }
            SettingsRow::SessionOption {
                server_index,
                option_index,
                ..
            } => {
                let server = &editor.inventory.servers[server_index];
                let option = &server.session_config[option_index];
                let saved =
                    editor.saved_session_value(SessionDefaultsSeat::Review, &server.id, option);
                let (saved_label, compatible) = session_option_value_label(option, &saved);
                lines.push(selected_line(
                    selected,
                    format!("{} < {saved_label} >", option.name),
                    theme,
                ));
                if !compatible {
                    lines.push(Line::styled(
                        format!("  unavailable on {}", server.id),
                        Style::default().ink(theme.error),
                    ));
                }
            }
            SettingsRow::DiscreteReview => lines.push(selected_line(
                selected,
                format!(
                    "Discrete review [{}]",
                    on_off(editor.config.agent.discrete_review)
                ),
                theme,
            )),
            SettingsRow::ReviewTier => {
                let tier = editor.config.agent.review_tier;
                lines.push(selected_line(
                    selected,
                    format!("Review depth < {} >", tier.label()),
                    theme,
                ));
                lines.push(Line::styled(
                    format!("  {}", tier.description()),
                    Style::default().ink(if editor.config.agent.discrete_review {
                        theme.muted
                    } else {
                        theme.warning
                    }),
                ));
                if !editor.config.agent.discrete_review {
                    lines.push(Line::styled(
                        "  discrete review is off, so no tier runs",
                        Style::default().ink(theme.warning),
                    ));
                }
            }
            SettingsRow::CorrectionThreshold => {
                let threshold = editor.config.agent.correction_threshold;
                lines.push(selected_line(
                    selected,
                    format!("Automatic correction through < {} >", threshold.label()),
                    theme,
                ));
                lines.push(Line::styled(
                    format!("  {}", threshold.description()),
                    Style::default().ink(if editor.config.agent.discrete_review {
                        theme.muted
                    } else {
                        theme.warning
                    }),
                ));
                if !editor.config.agent.discrete_review {
                    lines.push(Line::styled(
                        "  discrete review is off, so no finding is corrected automatically",
                        Style::default().ink(theme.warning),
                    ));
                }
            }
            SettingsRow::PrimaryModel
            | SettingsRow::SubagentModel
            | SettingsRow::SubagentPermissions
            | SettingsRow::MaxParallelSubagents
            | SettingsRow::AutomaticQuotaFailover => {}
        }
    }
    draw_scrolling_settings_lines(frame, area, lines, selected_line_index);
}

fn draw_subagents(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let rows = editor.settings_rows(SettingsTab::Subagents);
    let source = editor.selected_session_source(SessionDefaultsSeat::Subagents);
    let has_options = rows
        .iter()
        .any(|row| matches!(row, SettingsRow::SessionOption { .. }));
    let mut lines = Vec::new();
    let mut selected_line_index = 0;
    let mut session_options_heading_drawn = false;
    for (row_index, row) in rows.into_iter().enumerate() {
        if matches!(row, SettingsRow::SessionOption { .. }) && !session_options_heading_drawn {
            lines.push(Line::raw(""));
            lines.push(session_options_heading(editor, source.as_deref(), theme));
            session_options_heading_drawn = true;
        }
        if has_options && matches!(row, SettingsRow::MaxParallelSubagents) {
            lines.push(Line::raw(""));
        }
        let selected = editor.selected == row_index;
        if selected {
            selected_line_index = lines.len();
        }
        match row {
            SettingsRow::SubagentModel => {
                let model = &editor.config.subagents.model;
                lines.extend(model_lines(
                    selected,
                    "Subagent model",
                    model,
                    2,
                    editor,
                    theme,
                ));
            }
            SettingsRow::SubagentPermissions => {
                lines.extend(permission_lines(
                    selected,
                    editor.config.subagents.permission,
                    theme,
                ));
            }
            SettingsRow::SessionOption {
                server_index,
                option_index,
                ..
            } => {
                let server = &editor.inventory.servers[server_index];
                let option = &server.session_config[option_index];
                let saved =
                    editor.saved_session_value(SessionDefaultsSeat::Subagents, &server.id, option);
                let (saved_label, compatible) = session_option_value_label(option, &saved);
                lines.push(selected_line(
                    selected,
                    format!("{} < {saved_label} >", option.name),
                    theme,
                ));
                if !compatible {
                    lines.push(Line::styled(
                        format!("  unavailable on {}", server.id),
                        Style::default().ink(theme.error),
                    ));
                }
            }
            SettingsRow::MaxParallelSubagents => lines.push(selected_line(
                selected,
                format!(
                    "Parallel subagents < {} >",
                    editor.config.subagents.max_parallel
                ),
                theme,
            )),
            SettingsRow::AutomaticQuotaFailover => lines.push(selected_line(
                selected,
                format!(
                    "Automatic quota failover [{}]",
                    on_off(editor.config.subagents.auto_failover)
                ),
                theme,
            )),
            SettingsRow::PrimaryModel
            | SettingsRow::ReviewModel
            | SettingsRow::ReviewPermissions
            | SettingsRow::DiscreteReview
            | SettingsRow::ReviewTier
            | SettingsRow::CorrectionThreshold => {}
        }
    }
    draw_scrolling_settings_lines(frame, area, lines, selected_line_index);
}

fn session_option_value_label(option: &SessionConfigOption, value: &str) -> (String, bool) {
    session_option_choices(option)
        .into_iter()
        .find_map(|(candidate, label)| (candidate == value).then_some((label, true)))
        .unwrap_or_else(|| (format!("{value} (unavailable)"), false))
}

fn draw_scrolling_settings_lines(
    frame: &mut ratatui::Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    selected_line_index: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let selected_end =
        Paragraph::new(lines[..=selected_line_index.min(lines.len().saturating_sub(1))].to_vec())
            .wrap(Wrap { trim: false })
            .line_count(area.width);
    let scroll = selected_end
        .saturating_sub(usize::from(area.height))
        .min(usize::from(u16::MAX)) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_team(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    if let Some(external) = crate::roster::external_adapter() {
        let lines = vec![
            Line::styled(
                "Mjolnir automatically reviews generated code before returning the result.",
                Style::default().ink(theme.text),
            ),
            Line::styled(
                "This platform supplies one coding team.",
                Style::default().ink(theme.muted),
            ),
            Line::raw(""),
            selected_line(true, format!("Team  < {} >", external.label), theme),
            Line::raw(""),
            Line::styled(
                format!(
                    " * {:<31} handles primary, subagents, and review",
                    external.label
                ),
                Style::default().ink(theme.primary),
            ),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }
    let active = TeamPreset::from_config(&editor.config);
    let active_label = active.map_or("Custom routing", TeamPreset::label);
    let mut lines = vec![
        Line::styled(
            "Mjolnir automatically reviews generated code before returning the result.",
            Style::default().ink(theme.text),
        ),
        Line::styled(
            "Mix Codex and Claude, or use one provider. Auto models can reduce review cost.",
            Style::default().ink(theme.muted),
        ),
        Line::raw(""),
        selected_line(true, format!("Team  < {active_label} >"), theme),
        Line::raw(""),
        Line::styled(
            "Available configurations",
            Style::default().ink(theme.muted),
        ),
    ];
    for preset in TeamPreset::ALL {
        let marker = if Some(preset) == active { "*" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {marker} {:<31}", preset.label()),
                Style::default().ink(if Some(preset) == active {
                    theme.primary
                } else {
                    theme.text
                }),
            ),
            Span::styled(preset.description(), Style::default().ink(theme.muted)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Shift+Tab opens the same team switcher from a session.",
        Style::default().ink(theme.muted),
    ));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_servers(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let mut lines = vec![Line::styled(
        "Accounts",
        Style::default()
            .ink(theme.muted)
            .add_modifier(Modifier::BOLD),
    )];
    for (index, vendor) in crate::auth::AuthVendor::ALL.into_iter().enumerate() {
        let credentials = crate::auth::detect(vendor);
        lines.push(selected_line(
            editor.selected == index,
            format!(
                "{:<18} {} · enables {}",
                vendor.label(),
                credentials.status(),
                vendor.enables()
            ),
            theme,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Supported servers",
        Style::default()
            .ink(theme.muted)
            .add_modifier(Modifier::BOLD),
    ));
    let rows_available = area.height.saturating_sub(lines.len() as u16) as usize / 2;
    let selected_server = editor.selected.saturating_sub(SERVER_ROW_OFFSET);
    let start = selected_server.saturating_sub(rows_available.saturating_sub(1));
    for (index, server) in editor
        .configurable_servers()
        .enumerate()
        .skip(start)
        .take(rows_available)
    {
        let status = if server.policy == AcpServerPolicy::Disabled {
            "disabled".to_string()
        } else if let Some(error) = &server.error {
            format!("error: {error}")
        } else if server.model_count > 0 {
            format!(
                "ready; {} model{}",
                server.model_count,
                if server.model_count == 1 { "" } else { "s" }
            )
        } else if server.detected {
            "ready".to_string()
        } else {
            "not ready".to_string()
        };
        let status = match &server.subscription {
            Some(subscription) => format!("{status} · {subscription}"),
            None => status,
        };
        lines.push(selected_line(
            editor.selected == index + SERVER_ROW_OFFSET,
            format!("[{}] {:<16} {status}", server.policy, server.label),
            theme,
        ));
        let detail = {
            let args = server.launch.args.join(" ");
            let command = if args.is_empty() {
                server.launch.command.display().to_string()
            } else {
                format!("{} {args}", server.launch.command.display())
            };
            format!("{} · {command}", server.evidence)
        };
        lines.push(Line::styled(
            format!("      {detail}"),
            Style::default().ink(theme.muted),
        ));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_appearance(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    // Each selectable row with the description lines that belong to it, so
    // scrolling can keep the whole selected block on screen in short panels
    // instead of letting the bottom rows cycle blind.
    let sections: Vec<(Option<usize>, Vec<Line>)> = vec![
        (
            None,
            vec![
                Line::styled(
                    "Appearance changes preview immediately.",
                    Style::default().ink(theme.muted),
                ),
                Line::raw(""),
            ],
        ),
        (
            Some(0),
            vec![
                selected_line(
                    editor.selected == 0,
                    format!("Theme       < {} >", editor.config.theme),
                    theme,
                ),
                Line::styled(
                    format!("            {}", editor.config.theme.description()),
                    Style::default().ink(theme.muted),
                ),
                Line::styled(
                    format!("            {}", terminal_report()),
                    Style::default().ink(theme.muted),
                ),
            ],
        ),
        (
            Some(1),
            vec![spinner_preview_line(
                editor.selected == 1,
                editor.config.spinner,
                theme,
            )],
        ),
        (
            Some(2),
            vec![
                selected_line(
                    editor.selected == 2,
                    format!("Thought output < {} >", editor.config.thought_output),
                    theme,
                ),
                Line::styled(
                    format!(
                        "               {}",
                        editor.config.thought_output.description()
                    ),
                    Style::default().ink(theme.muted),
                ),
            ],
        ),
        (
            Some(3),
            vec![selected_line(
                editor.selected == 3,
                format!(
                    "Feature tips < {} >",
                    if editor.config.feature_hints {
                        "on"
                    } else {
                        "off"
                    }
                ),
                theme,
            )],
        ),
        (
            Some(4),
            vec![
                selected_line(
                    editor.selected == 4,
                    format!(
                        "Keep awake  < {} >",
                        if editor.config.keep_awake {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                    theme,
                ),
                Line::styled(
                    "            Prevent system sleep while the server runs or a turn is in flight.",
                    Style::default().ink(theme.muted),
                ),
            ],
        ),
        (
            Some(5),
            vec![
                selected_line(
                    editor.selected == 5,
                    format!("Interface   < {} >", editor.config.interface),
                    theme,
                ),
                Line::styled(
                    format!("            {}", editor.config.interface.description()),
                    Style::default().ink(theme.muted),
                ),
                Line::styled(
                    "            Applies when the next session starts; --fullscreen-tui overrides.",
                    Style::default().ink(theme.muted),
                ),
            ],
        ),
    ];
    let mut lines = Vec::new();
    let mut selected_span = (0, 0);
    for (row, section) in sections {
        if row == Some(editor.selected) {
            selected_span = (lines.len(), lines.len() + section.len() - 1);
        }
        lines.extend(section);
    }
    // Scroll just far enough to show the selected block's last line, but never
    // past its first line when the block itself outgrows the viewport.
    let scroll = (selected_span.1 + 1)
        .saturating_sub(area.height as usize)
        .min(selected_span.0);
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
}

fn draw_input_settings(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let lines = vec![
        Line::styled(
            "Input settings apply when saved.",
            Style::default().ink(theme.muted),
        ),
        Line::raw(""),
        selected_line(
            editor.selected == 0,
            format!("Voice auto-send < {} >", editor.config.voice_auto_send),
            theme,
        ),
        Line::styled(
            format!(
                "                {}",
                editor.config.voice_auto_send.description()
            ),
            Style::default().ink(theme.muted),
        ),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// What the startup probe learned, phrased for the appearance tab.
///
/// Worth surfacing because it explains an otherwise mysterious difference
/// between two machines: a terminal that answers the OSC queries gets tinted
/// diff rows, and one that stays silent gets foreground-only diffs.
fn terminal_report() -> String {
    let level = match crate::terminal_palette::stdout_color_level() {
        crate::terminal_palette::StdoutColorLevel::TrueColor => "truecolor",
        crate::terminal_palette::StdoutColorLevel::Ansi256 => "256 colors",
        crate::terminal_palette::StdoutColorLevel::Ansi16 => "16 colors",
        crate::terminal_palette::StdoutColorLevel::Unknown => "no color reported",
    };
    match crate::terminal_palette::default_colors() {
        Some(colors) => {
            let (r, g, b) = colors.bg;
            format!("terminal: {level}, background #{r:02x}{g:02x}{b:02x}")
        }
        None => format!("terminal: {level}, background unknown (diff fills off)"),
    }
}

/// The spinner row, with a live preview of the style in its real colors.
///
/// The preview trails the selection highlight rather than sitting inside it:
/// the highlight is a high-contrast fg/bg pair, and several inks (green, gray)
/// would be unreadable on it. Keeping the frame on the default background lets
/// the row you are actually editing show the colors you are choosing.
fn spinner_preview_line(
    selected: bool,
    style: crate::spinner::SpinnerStyle,
    theme: TerminalTheme,
) -> Line<'static> {
    let mut line = selected_line(selected, format!("Spinner     < {style} >  "), theme);
    line.spans
        .extend(style.current_frame().runs().iter().map(|(text, ink)| {
            Span::styled(text.as_str(), Style::default().ink(theme.spinner_ink(*ink)))
        }));
    line
}

fn selected_line(selected: bool, text: String, theme: TerminalTheme) -> Line<'static> {
    let style = if selected {
        Style::default()
            .ink(theme.selection_fg)
            .ink_bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().ink(theme.text)
    };
    Line::from(Span::styled(
        format!("{} {text}", if selected { ">" } else { " " }),
        style,
    ))
}

fn selected_line_with_detail(
    selected: bool,
    text: String,
    detail: Option<String>,
    theme: TerminalTheme,
) -> Line<'static> {
    let mut line = selected_line(selected, text, theme);
    if let Some(detail) = detail {
        line.spans.push(Span::styled(
            format!("  {detail}"),
            Style::default().ink(theme.muted),
        ));
    }
    line
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReviewTier;

    fn render(editor: &SettingsEditor, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), editor, "mj config"))
            .expect("draw");
        terminal.backend().to_string()
    }

    #[test]
    fn reset_unroutable_models_flips_seats_whose_route_is_disabled() {
        let mut config = Config::default();
        config.agent.model = "model-a".to_string();
        config.review.model = "model-b".to_string();
        config.subagents.model = crate::config::DISABLED_MODEL.to_string();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Disabled);
        let choices = vec![
            ModelChoice {
                model: "model-a".to_string(),
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
                available: true,
                disabled_reason: None,
                adapter: Some("codex-acp".to_string()),
                ranked: true,
            },
            ModelChoice {
                model: "model-b".to_string(),
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
                available: true,
                disabled_reason: None,
                adapter: Some("claude-acp".to_string()),
                ranked: true,
            },
        ];

        let notices = reset_unroutable_models(&mut config, &choices);

        // model-a lost its only route; model-b's adapter is still enabled and
        // the disabled subagent sentinel is never touched.
        assert_eq!(config.agent.model, "auto");
        assert_eq!(config.review.model, "model-b");
        assert_eq!(config.subagents.model, crate::config::DISABLED_MODEL);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("Agent model model-a"), "{}", notices[0]);
    }

    #[test]
    fn reset_unroutable_models_uses_native_adapter_when_catalog_lacks_the_model() {
        let mut config = Config::default();
        config.agent.model = "claude-opus-5".to_string();
        config.set_acp_server_policy("claude-acp", AcpServerPolicy::Disabled);

        // No catalog entry at all: the provider's native adapter decides.
        let notices = reset_unroutable_models(&mut config, &[]);

        assert_eq!(config.agent.model, "auto");
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("Agent model claude-opus-5"),
            "{}",
            notices[0]
        );
    }

    #[test]
    fn reset_unroutable_models_clears_a_source_pin_to_a_disabled_adapter() {
        // An adapter-advertised alias pinned with its source: disabling that
        // source must clear both, or roster assembly aborts on the stale
        // source pin ("no launchable models") despite the model reset.
        let mut config = Config::default();
        config.agent.model = "haiku".to_string();
        config.agent.acp_source = Some("claude-acp".to_string());
        config.set_acp_server_policy("claude-acp", AcpServerPolicy::Disabled);

        let notices = reset_unroutable_models(&mut config, &[]);

        assert_eq!(config.agent.model, "auto");
        assert_eq!(config.agent.acp_source, None);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("source pin was cleared"),
            "{}",
            notices[0]
        );
    }

    #[test]
    fn reset_unroutable_models_keeps_an_uncataloged_alias_pin() {
        // Same alias pin, but its source is merely undetected, not disabled:
        // the pin survives until the catalog can judge it.
        let mut config = Config::default();
        config.agent.model = "haiku".to_string();
        config.agent.acp_source = Some("claude-acp".to_string());

        let notices = reset_unroutable_models(&mut config, &[]);

        assert_eq!(config.agent.model, "haiku");
        assert_eq!(config.agent.acp_source.as_deref(), Some("claude-acp"));
        assert!(notices.is_empty());
    }

    #[test]
    fn selecting_explicit_model_updates_its_acp_source() {
        let mut config = Config::default();
        config.agent.model = "claude-fable-5".to_string();
        config.agent.acp_source = Some("claude-acp".to_string());
        let mut editor = SettingsEditor::new(
            config,
            vec![ModelChoice {
                model: "gpt-5-6-terra".to_string(),
                pass_at_1: 0.54,
                mean_cost_usd: 1.13,
                available: true,
                disabled_reason: None,
                adapter: Some("codex-acp".to_string()),
                ranked: true,
            }],
            None,
        );

        editor.cycle_model(0, 1);

        assert_eq!(editor.config.agent.model, "gpt-5-6-terra");
        assert_eq!(editor.config.agent.acp_source.as_deref(), Some("codex-acp"));
    }

    #[test]
    fn staged_model_warning_only_reports_missing_or_unavailable_models() {
        let editor = SettingsEditor::new(
            Config::default(),
            vec![ModelChoice {
                model: "unavailable-model".to_string(),
                pass_at_1: 0.0,
                mean_cost_usd: 0.0,
                available: false,
                disabled_reason: Some("sign-in required".to_string()),
                adapter: Some("codex-acp".to_string()),
                ranked: false,
            }],
            None,
        );

        assert_eq!(editor.staged_model_warning("auto"), None);
        assert_eq!(
            editor.staged_model_warning("missing-model").as_deref(),
            Some("not reported this session")
        );
        assert_eq!(
            editor.staged_model_warning("unavailable-model").as_deref(),
            Some("unavailable: sign-in required")
        );
    }

    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    };

    fn ensure_two_inventory_servers(editor: &mut SettingsEditor) {
        if editor.inventory.servers.len() >= 2 {
            return;
        }
        let mut second = editor
            .inventory
            .servers
            .first()
            .expect("at least one built-in ACP server")
            .clone();
        second.id = "test-second-adapter".to_string();
        second.label = "Test second adapter".to_string();
        second.launch.source_id.clone_from(&second.id);
        second.session_config.clear();
        editor.inventory.servers.push(second);
    }

    #[test]
    fn agent_panel_saves_primary_option_without_overwriting_live_route_cache() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        let server = editor
            .inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("priority", "Priority"),
            ],
        )];
        editor
            .config
            .session_config
            .entry(server_id.clone())
            .or_default()
            .models
            .entry("model-a".to_string())
            .or_default()
            .insert("config:service_tier".to_string(), "default".to_string());
        editor.config.agent.acp_source = Some(server_id.clone());
        editor.tab = SettingsTab::Agents;
        editor.selected = 1;

        assert_eq!(
            editor
                .session_option_rows(SessionDefaultsSeat::Primary)
                .len(),
            1
        );
        assert_eq!(
            session_option_choices(&editor.inventory.servers[0].session_config[0]).len(),
            2
        );
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.agent.session_defaults[&server_id]["config:service_tier"],
            "priority"
        );
        assert!(
            editor.config.session_config[&server_id].models["model-a"]
                .contains_key("config:service_tier")
        );
    }

    #[test]
    fn role_panels_edit_arbitrary_options_with_separate_scope() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        let server = editor
            .inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let choice = || vec![SessionConfigSelectOption::new("value", "Value")];
        let model = SessionConfigOption::select("model", "Model", "value", choice())
            .category(SessionConfigOptionCategory::Model);
        let thought =
            SessionConfigOption::select("thought_level", "Thought level", "value", choice())
                .category(SessionConfigOptionCategory::ThoughtLevel);
        let permission = SessionConfigOption::select("mode", "Permission", "value", choice());
        let reasoning = SessionConfigOption::select(
            crate::acp::REASONING_EFFORT_CONFIG_ID,
            "Reasoning effort",
            "value",
            choice(),
        );
        let fast = SessionConfigOption::select("fast_mode", "Fast mode", "value", choice())
            .category(SessionConfigOptionCategory::ModelConfig);
        let service =
            SessionConfigOption::select("service_tier", "Service tier", "value", choice());
        let server_id = server.id.clone();
        server.launch.kind = crate::roster::AdapterKind::Codex;
        server.session_config = vec![model, thought, permission, reasoning, fast, service];
        editor.config.agent.acp_source = Some(server_id.clone());
        editor.config.review.acp_source = Some(server_id.clone());
        editor.config.subagents.acp_source = Some(server_id.clone());
        editor.tab = SettingsTab::Agents;

        assert_eq!(
            editor
                .session_option_rows(SessionDefaultsSeat::Primary)
                .len(),
            5,
            "the dedicated model row owns the advertised model option"
        );
        for selected in 1..=5 {
            editor.selected = selected;
            assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        }
        assert_eq!(editor.config.agent.session_defaults[&server_id].len(), 5);
        assert_eq!(
            editor.config.agent.reasoning_effort.as_deref(),
            Some("value")
        );

        editor.tab = SettingsTab::Reviewer;
        assert_eq!(
            editor
                .session_option_rows(SessionDefaultsSeat::Review)
                .len(),
            4,
            "review permissions owns the provider mode option"
        );
        for selected in 2..=5 {
            editor.selected = selected;
            assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        }
        assert_eq!(editor.config.review.session_defaults[&server_id].len(), 4);
        assert!(!editor.config.review.session_defaults[&server_id].contains_key("config:mode"));
        assert_eq!(
            editor.config.review.reasoning_effort.as_deref(),
            Some("value")
        );

        editor.tab = SettingsTab::Subagents;
        assert_eq!(
            editor
                .session_option_rows(SessionDefaultsSeat::Subagents)
                .len(),
            4,
            "subagent permissions owns the provider mode option"
        );
        for selected in 2..=5 {
            editor.selected = selected;
            assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        }
        assert_eq!(
            editor.config.subagents.session_defaults[&server_id].len(),
            4
        );
        assert!(!editor.config.subagents.session_defaults[&server_id].contains_key("config:mode"));
        assert_eq!(
            editor.config.subagents.reasoning_effort.as_deref(),
            Some("value")
        );
    }

    #[test]
    fn primary_and_subagent_panels_use_their_selected_adapters_options() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        editor.inventory.servers.truncate(1);
        ensure_two_inventory_servers(&mut editor);
        let primary_index = 0;
        let subagent_index = 1;
        let primary_id = editor.inventory.servers[primary_index].id.clone();
        let subagent_id = editor.inventory.servers[subagent_index].id.clone();
        editor.inventory.servers[primary_index].session_config = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![SessionConfigSelectOption::new("default", "Default")],
        )];
        editor.inventory.servers[subagent_index].session_config =
            vec![SessionConfigOption::select(
                "permission_mode",
                "Permission mode",
                "ask",
                vec![SessionConfigSelectOption::new("ask", "Ask")],
            )];
        editor.config.agent.acp_source = Some(primary_id);
        editor.config.subagents.acp_source = Some(subagent_id);

        assert_eq!(
            editor.session_option_rows(SessionDefaultsSeat::Primary),
            vec![(primary_index, 0)]
        );
        assert_eq!(
            editor.session_option_rows(SessionDefaultsSeat::Subagents),
            vec![(subagent_index, 0)]
        );
    }

    #[test]
    fn active_source_wins_for_the_current_explicit_model() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        editor.inventory.servers.truncate(1);
        ensure_two_inventory_servers(&mut editor);
        let active_source = editor.inventory.servers[1].id.clone();
        editor.inventory.servers[0].session_config = vec![SessionConfigOption::select(
            "first",
            "First adapter option",
            "on",
            vec![SessionConfigSelectOption::new("on", "On")],
        )];
        editor.inventory.servers[1].session_config = vec![SessionConfigOption::select(
            "active",
            "Active adapter option",
            "on",
            vec![SessionConfigSelectOption::new("on", "On")],
        )];
        editor.config.agent.model = "model-a".to_string();
        editor.active_models = Some(ModelsConfig {
            primary: "model-a".to_string(),
            primary_source: Some(active_source),
            ..ModelsConfig::default()
        });

        assert_eq!(
            editor.session_option_rows(SessionDefaultsSeat::Primary),
            vec![(1, 0)]
        );
    }

    #[test]
    fn legacy_adapter_default_is_shown_until_a_scoped_value_is_chosen() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        let server = editor
            .inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![SessionConfigOption::select(
            "mode",
            "Mode",
            "ask",
            vec![
                SessionConfigSelectOption::new("ask", "Ask"),
                SessionConfigSelectOption::new("code", "Code"),
            ],
        )];
        editor.config.agent.acp_source = Some(server_id.clone());
        editor
            .config
            .session_config
            .entry(server_id.clone())
            .or_default()
            .defaults
            .insert("config:mode".to_string(), "code".to_string());
        let option = &editor.inventory.servers[0].session_config[0];
        assert_eq!(
            editor.saved_session_value(SessionDefaultsSeat::Primary, &server_id, option),
            "code"
        );

        editor.tab = SettingsTab::Agents;
        editor.selected = 1;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.agent.session_defaults[&server_id]["config:mode"],
            "ask"
        );
        assert_eq!(
            editor.config.session_config[&server_id].defaults["config:mode"],
            "code"
        );
    }

    #[test]
    fn stale_session_default_is_visible_and_cycles_to_an_advertised_value() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        let server = editor
            .inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![SessionConfigOption::select(
            "mode",
            "Mode",
            "ask",
            vec![
                SessionConfigSelectOption::new("ask", "Ask"),
                SessionConfigSelectOption::new("code", "Code"),
            ],
        )];
        editor.config.agent.acp_source = Some(server_id.clone());
        editor
            .config
            .agent
            .session_defaults
            .entry(server_id.clone())
            .or_default()
            .insert("config:mode".to_string(), "removed".to_string());
        let option = &editor.inventory.servers[0].session_config[0];

        assert_eq!(
            session_option_value_label(option, "removed"),
            ("removed (unavailable)".to_string(), false)
        );
        editor.tab = SettingsTab::Agents;
        editor.selected = 1;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.agent.session_defaults[&server_id]["config:mode"],
            "ask"
        );
    }

    #[test]
    fn agent_panel_scrolls_dynamic_options_into_view_at_narrow_width() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        let server = editor
            .inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = (0..12)
            .map(|index| {
                SessionConfigOption::select(
                    format!("option-{index}"),
                    format!("Dynamic option {index}"),
                    "off",
                    vec![
                        SessionConfigSelectOption::new("off", "Off"),
                        SessionConfigSelectOption::new("on", "On"),
                    ],
                )
            })
            .collect();
        editor.config.agent.acp_source = Some(server_id);
        editor.tab = SettingsTab::Agents;
        editor.selected = 12;
        let backend = ratatui::backend::TestBackend::new(48, 14);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Dynamic option 11"),
            "selected row must remain visible:\n{rendered}"
        );
    }

    #[test]
    fn tabs_share_one_editable_config() {
        let mut config = Config::default();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Enabled);
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::Reviewer;
        editor.selected = editor
            .settings_rows(SettingsTab::Reviewer)
            .iter()
            .position(|row| *row == SettingsRow::DiscreteReview)
            .expect("discrete review row");
        assert_eq!(
            editor.handle_key(KeyCode::Char(' ')),
            SettingsAction::Changed
        );
        assert!(!editor.config.agent.discrete_review);
        editor.handle_key(KeyCode::Tab);
        assert_eq!(editor.tab, SettingsTab::Subagents);
        editor.handle_key(KeyCode::Tab);
        assert_eq!(editor.tab, SettingsTab::AcpServers);
        editor.selected = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "codex-acp")
            .expect("codex")
            + SERVER_ROW_OFFSET;
        assert_eq!(
            editor.handle_key(KeyCode::Char(' ')),
            SettingsAction::Changed
        );
        assert_eq!(
            editor.config.acp.policy("codex-acp"),
            AcpServerPolicy::Disabled
        );
        editor.handle_key(KeyCode::Tab);
        assert_eq!(editor.tab, SettingsTab::Input);
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.voice_auto_send, VoiceAutoSend::TwoSeconds);
    }

    #[test]
    fn review_tier_cycles_next_to_the_discrete_review_switch() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Reviewer;
        let rows = editor.settings_rows(SettingsTab::Reviewer);
        let review = rows
            .iter()
            .position(|row| *row == SettingsRow::DiscreteReview)
            .expect("discrete review row");
        let tier = rows
            .iter()
            .position(|row| *row == SettingsRow::ReviewTier)
            .expect("review tier row");
        let threshold = rows
            .iter()
            .position(|row| *row == SettingsRow::CorrectionThreshold)
            .expect("correction threshold row");
        assert_eq!(tier, review + 1, "the tier belongs beside the switch");
        assert_eq!(
            threshold,
            tier + 1,
            "the correction policy belongs beside review depth"
        );

        editor.selected = tier;
        assert_eq!(editor.config.agent.review_tier, ReviewTier::Quick);
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.agent.review_tier, ReviewTier::Extended);

        editor.selected = threshold;
        assert_eq!(
            editor.config.agent.correction_threshold,
            crate::config::ReviewCorrectionThreshold::P3
        );
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(
            editor.config.agent.correction_threshold,
            crate::config::ReviewCorrectionThreshold::P2
        );
        assert_eq!(
            editor.handle_key(KeyCode::Char(' ')),
            SettingsAction::Changed
        );
        assert_eq!(
            editor.config.agent.correction_threshold,
            crate::config::ReviewCorrectionThreshold::P3
        );
        editor.selected = tier;
        // Two tiers, so left, right, and the toggle key all return to Quick.
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(editor.config.agent.review_tier, ReviewTier::Quick);
        assert_eq!(
            editor.handle_key(KeyCode::Char(' ')),
            SettingsAction::Changed
        );
        assert_eq!(editor.config.agent.review_tier, ReviewTier::Extended);
    }

    #[test]
    fn quota_failover_can_be_disabled() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Subagents;
        editor.selected = editor
            .settings_rows(SettingsTab::Subagents)
            .iter()
            .position(|row| *row == SettingsRow::AutomaticQuotaFailover)
            .expect("failover row");
        assert_eq!(
            editor.handle_key(KeyCode::Char(' ')),
            SettingsAction::Changed
        );
        assert!(!editor.config.subagents.auto_failover);
    }

    #[test]
    fn reviewer_and_subagent_permissions_are_configured_independently() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Reviewer;
        editor.selected = editor
            .settings_rows(SettingsTab::Reviewer)
            .iter()
            .position(|row| *row == SettingsRow::ReviewPermissions)
            .expect("review permissions row");
        assert_eq!(editor.config.review.permission, PermissionPreset::Auto);
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(editor.config.review.permission, PermissionPreset::Manual);
        assert_eq!(editor.config.subagents.permission, PermissionPreset::Auto);

        editor.tab = SettingsTab::Subagents;
        editor.selected = editor
            .settings_rows(SettingsTab::Subagents)
            .iter()
            .position(|row| *row == SettingsRow::SubagentPermissions)
            .expect("subagent permissions row");
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.subagents.permission, PermissionPreset::Yolo);
        assert_eq!(editor.config.review.permission, PermissionPreset::Manual);
    }

    #[test]
    fn disabled_is_only_available_for_optional_roles() {
        let editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        assert!(
            !editor
                .model_choices(0)
                .iter()
                .any(|choice| choice == "disabled")
        );
        assert!(
            !editor
                .model_choices(1)
                .iter()
                .any(|choice| choice == "disabled")
        );
        assert!(
            editor
                .model_choices(2)
                .iter()
                .any(|choice| choice == "disabled")
        );
    }

    #[test]
    fn optional_model_selection_can_disable_subagents() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Subagents;
        editor.selected = 0;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.subagents.model, crate::config::DISABLED_MODEL);
    }

    #[test]
    fn team_configuration_updates_all_three_routes() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Team;

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            TeamPreset::from_config(&editor.config),
            Some(TeamPreset::Codex)
        );
        assert_eq!(editor.config.agent.acp_source.as_deref(), Some("codex-acp"));
        assert_eq!(
            editor.config.subagents.acp_source.as_deref(),
            Some("codex-acp")
        );
        assert_eq!(
            editor.config.review.acp_source.as_deref(),
            Some("codex-acp")
        );
        assert_eq!(
            editor.notice.as_deref(),
            Some("Team updated; save to apply it.")
        );
    }

    #[test]
    fn team_configuration_cycles_through_all_four_options() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Team;

        for expected in TeamPreset::ALL {
            assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
            assert_eq!(TeamPreset::from_config(&editor.config), Some(expected));
        }
    }

    #[test]
    fn team_tab_exposes_all_four_configurations() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        TeamPreset::CodexWithClaudeReviewer.apply(&mut editor.config);
        editor.tab = SettingsTab::Team;
        let backend = ratatui::backend::TestBackend::new(90, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(!rendered.contains("ACP Priority"), "rendered:\n{rendered}");
        for preset in TeamPreset::ALL {
            assert!(rendered.contains(preset.label()), "rendered:\n{rendered}");
        }
    }

    #[test]
    fn auto_server_can_be_explicitly_enabled() {
        // An explicit policy keeps a built-in visible regardless of whether
        // this host actually has it installed.
        let mut config = Config::default();
        config.set_acp_server_policy("claude-acp", AcpServerPolicy::Disabled);
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        let server_index = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "claude-acp")
            .expect("claude-acp");
        editor.selected = server_index + SERVER_ROW_OFFSET;
        editor.inventory.servers[server_index].detected = false;
        editor.inventory.servers[server_index].policy = AcpServerPolicy::Auto;

        // Exercise the transition without refreshing host-specific discovery.
        assert_eq!(editor.toggle_selected(), SettingsAction::Changed);
        assert_eq!(
            editor.config.acp.policy("claude-acp"),
            AcpServerPolicy::Enabled
        );
    }

    #[test]
    fn only_builtin_servers_are_configurable() {
        // A platform adapter is the sole route on its build, so offering a
        // Disabled toggle for it would break every launch; it must never
        // appear as configurable.
        assert!(is_configurable_acp_server("codex-acp"));
        assert!(is_configurable_acp_server("claude-acp"));
        assert!(!is_configurable_acp_server("anvil"));
        assert!(!is_configurable_acp_server("sidecar"));
    }

    #[test]
    fn account_rows_are_direct_actions() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;

        editor.selected = 0;
        assert_eq!(
            editor.handle_key(KeyCode::Enter),
            SettingsAction::Authenticate(crate::auth::AuthVendor::OpenAi)
        );

        editor.selected = 1;
        assert_eq!(
            editor.handle_key(KeyCode::Enter),
            SettingsAction::Authenticate(crate::auth::AuthVendor::Anthropic)
        );
    }

    #[test]
    fn appearance_tab_toggles_feature_hints() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Appearance;
        editor.selected = 3;

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert!(!editor.config.feature_hints);
        assert_eq!(editor.handle_key(KeyCode::Char(' ')), SettingsAction::None);
        assert!(!editor.config.feature_hints);
    }

    #[test]
    fn appearance_tab_cycles_thought_output() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Appearance;
        editor.selected = 2;

        assert_eq!(editor.config.thought_output, ThoughtOutput::Default);
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.thought_output, ThoughtOutput::Full);
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(editor.config.thought_output, ThoughtOutput::Default);
    }

    #[test]
    fn appearance_tab_toggles_keep_awake() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Appearance;
        editor.selected = 4;

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert!(!editor.config.keep_awake);
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert!(editor.config.keep_awake);
    }

    #[test]
    fn appearance_scrolls_the_selected_bottom_row_into_view() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Appearance;

        let unscrolled = render(&editor, SETTINGS_PANEL_MIN_WIDTH, SETTINGS_PANEL_MIN_HEIGHT);
        assert!(unscrolled.contains("Theme"), "rendered:\n{unscrolled}");
        assert!(!unscrolled.contains("Interface"), "rendered:\n{unscrolled}");

        editor.selected = 5;
        let scrolled = render(&editor, SETTINGS_PANEL_MIN_WIDTH, SETTINGS_PANEL_MIN_HEIGHT);
        assert!(scrolled.contains("Interface"), "rendered:\n{scrolled}");
    }

    #[test]
    fn appearance_tab_cycles_interface_mode() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Appearance;
        editor.selected = 5;

        assert_eq!(editor.config.interface, InterfaceMode::Inline);
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.interface, InterfaceMode::Fullscreen);
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(editor.config.interface, InterfaceMode::Inline);
    }

    #[test]
    fn input_tab_cycles_voice_auto_send_delay() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Input;

        assert_eq!(editor.config.voice_auto_send, VoiceAutoSend::Off);
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.voice_auto_send, VoiceAutoSend::TwoSeconds);
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(editor.config.voice_auto_send, VoiceAutoSend::Off);
    }

    #[test]
    fn standard_tabs_render_their_controls() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );

        editor.tab = SettingsTab::Team;
        let team = render(&editor, 100, 30);
        for preset in TeamPreset::ALL {
            assert!(team.contains(preset.label()), "rendered:\n{team}");
        }

        editor.tab = SettingsTab::Agents;
        let agents = render(&editor, 100, 30);
        assert!(agents.contains("Primary model"), "rendered:\n{agents}");

        editor.tab = SettingsTab::Reviewer;
        let reviewer = render(&editor, 100, 30);
        assert!(reviewer.contains("Review model"), "rendered:\n{reviewer}");
        assert!(
            reviewer.contains("Discrete review"),
            "rendered:\n{reviewer}"
        );
        assert!(reviewer.contains("Review depth"), "rendered:\n{reviewer}");
        assert!(reviewer.contains("Quick"), "rendered:\n{reviewer}");

        editor.tab = SettingsTab::Subagents;
        let subagents = render(&editor, 100, 30);
        assert!(
            subagents.contains("Subagent model"),
            "rendered:\n{subagents}"
        );
        assert!(
            subagents.contains("Automatic quota failover"),
            "rendered:\n{subagents}"
        );

        editor.tab = SettingsTab::AcpServers;
        let servers = render(&editor, 100, 30);
        assert!(servers.contains("Accounts"), "rendered:\n{servers}");
        assert!(
            servers.contains("Supported servers"),
            "rendered:\n{servers}"
        );
        assert!(!servers.contains("+ Add server"), "rendered:\n{servers}");

        editor.tab = SettingsTab::Appearance;
        let appearance = render(&editor, 100, 30);
        assert!(appearance.contains("Theme"), "rendered:\n{appearance}");
        assert!(appearance.contains("Spinner"), "rendered:\n{appearance}");
        assert!(
            appearance.contains("Thought output"),
            "rendered:\n{appearance}"
        );
        assert!(
            appearance.contains("Feature tips"),
            "rendered:\n{appearance}"
        );
        assert!(appearance.contains("Keep awake"), "rendered:\n{appearance}");
        assert!(appearance.contains("Interface"), "rendered:\n{appearance}");
        assert!(appearance.contains("< inline >"), "rendered:\n{appearance}");
        assert!(
            !appearance.contains("Voice auto-send"),
            "rendered:\n{appearance}"
        );

        editor.tab = SettingsTab::Input;
        let input = render(&editor, 100, 30);
        assert!(input.contains("Voice auto-send"), "rendered:\n{input}");
        assert!(
            input.contains("Input settings apply when saved"),
            "rendered:\n{input}"
        );

        assert!(!render(&editor, 27, 11).contains("mj config"));
    }

    #[test]
    fn reviewer_panel_keeps_mode_under_the_permissions_control() {
        let mut editor = SettingsEditor::new(
            crate::roster::config_with_a_visible_builtin(),
            Vec::new(),
            None,
        );
        let server = editor
            .inventory
            .servers
            .first_mut()
            .expect("visible ACP server");
        let server_id = server.id.clone();
        server.session_config = vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "agent",
                vec![SessionConfigSelectOption::new("agent", "Agent")],
            ),
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning effort",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            ),
        ];
        editor.config.review.acp_source = Some(server_id.clone());
        editor.config.review.model = "missing-review-model".to_string();
        editor.active_models = Some(ModelsConfig {
            review: "active-review-model".to_string(),
            review_source: Some(server_id),
            ..ModelsConfig::default()
        });
        editor.tab = SettingsTab::Reviewer;

        let reviewer = render(&editor, 100, 30);

        assert!(
            reviewer.contains("Session options ·"),
            "rendered:\n{reviewer}"
        );
        assert!(
            reviewer.contains("Permissions < Auto >"),
            "rendered:\n{reviewer}"
        );
        assert!(
            !reviewer.contains("Mode < Agent >"),
            "rendered:\n{reviewer}"
        );
        assert!(
            reviewer.contains("Reasoning effort < High >"),
            "rendered:\n{reviewer}"
        );
        assert!(
            reviewer.contains("active: active-review-model via"),
            "rendered:\n{reviewer}"
        );
        assert!(
            reviewer.contains("not reported this session"),
            "rendered:\n{reviewer}"
        );
        for noise in [
            "Saved review-session defaults",
            "saved:",
            "saved default",
            "already-running reviews are unchanged",
        ] {
            assert!(!reviewer.contains(noise), "rendered:\n{reviewer}");
        }
    }

    #[test]
    fn acp_server_panel_lists_only_builtin_servers() {
        let config = crate::roster::config_with_a_visible_builtin();
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;

        let servers = render(&editor, 100, 30);

        assert!(servers.contains("Codex"), "rendered:\n{servers}");
        assert_eq!(
            editor.row_count(),
            ACCOUNT_COUNT + editor.configurable_servers().count()
        );
    }

    #[test]
    fn team_change_notice_and_footer_are_visible() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Team;
        editor.notice = Some("restart required".to_string());

        let rendered = render(&editor, 100, 30);

        assert!(
            rendered.contains("restart required"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("change"), "rendered:\n{rendered}");
        assert!(rendered.contains("Enter save"), "rendered:\n{rendered}");
    }
}
