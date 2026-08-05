//! Shared first-startup and in-session settings editor.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions,
};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::{AcpServerOrigin, AcpServerPolicy, Config, ConfiguredAcpServer, ModelsConfig};
use crate::ink::InkStyle;
use crate::install::Progress;
use crate::palette::TerminalTheme;
use crate::registry::{Agent, DistributionKind, Registry};
use crate::roster::{AcpInventory, ModelChoice};
use crate::spinner::SpinnerStyle;
use crate::theme::TerminalThemeKind;

const ACCOUNT_COUNT: usize = crate::auth::AuthVendor::ALL.len();
const ADD_SERVER_INDEX: usize = ACCOUNT_COUNT;
pub(crate) const SERVER_ROW_OFFSET: usize = ACCOUNT_COUNT + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Agents,
    Subagents,
    AcpPriority,
    AcpServers,
    Appearance,
}

impl SettingsTab {
    const ALL: [Self; 5] = [
        Self::Agents,
        Self::Subagents,
        Self::AcpServers,
        Self::AcpPriority,
        Self::Appearance,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::Subagents => "Subagents",
            Self::AcpPriority => "ACP Priority",
            Self::AcpServers => "ACP Servers",
            Self::Appearance => "Appearance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Changed,
    Authenticate(crate::auth::AuthVendor),
    Save,
    Cancel,
}

#[derive(Debug, Clone)]
enum AcpView {
    Servers,
    Catalog {
        filter: String,
    },
    Custom {
        name: String,
        command: String,
        field: usize,
    },
}

#[derive(Debug, Clone)]
enum RegistryState {
    NotLoaded,
    Loading(Arc<Mutex<Option<Result<Registry, String>>>>),
    Ready(Registry),
    Error(String),
}

#[derive(Debug, Clone, Default)]
struct InstallSnapshot {
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
    extracting: bool,
    result: Option<Result<(PathBuf, Vec<String>), String>>,
}

#[derive(Debug, Clone)]
struct InstallingServer {
    agent: Agent,
    snapshot: Arc<Mutex<InstallSnapshot>>,
    abort: tokio::task::AbortHandle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrioritySeat {
    Primary,
    Review,
    Subagents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDefaultsSeat {
    Primary,
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
    acp_view: AcpView,
    registry: RegistryState,
    installing: Option<InstallingServer>,
    priority_editor: Option<PrioritySeat>,
    priority_selected: usize,
}

impl SettingsEditor {
    pub fn new(config: Config, choices: Vec<ModelChoice>, notice: Option<String>) -> Self {
        let inventory = crate::roster::discover_inventory(&config);
        Self {
            config,
            tab: SettingsTab::Agents,
            selected: 0,
            notice,
            choices,
            active_models: None,
            active_session_config: Vec::new(),
            inventory,
            acp_view: AcpView::Servers,
            registry: RegistryState::NotLoaded,
            installing: None,
            priority_editor: None,
            priority_selected: 0,
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

    /// Discovered ACP inventory backing the editor's server and session rows.
    /// The remote-control server projects it into the web `/mjconfig` panel so
    /// both UIs describe the same servers with the same status strings.
    pub(crate) fn inventory(&self) -> &AcpInventory {
        &self.inventory
    }

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsAction {
        self.poll_background();
        if self.priority_editor.is_some() {
            return self.handle_priority_key(code);
        }
        if self.tab == SettingsTab::AcpServers {
            match self.acp_view {
                AcpView::Catalog { .. } => return self.handle_catalog_key(code),
                AcpView::Custom { .. } => return self.handle_custom_key(code),
                AcpView::Servers => {}
            }
        }
        match code {
            KeyCode::Esc => SettingsAction::Cancel,
            KeyCode::Enter
                if self.tab == SettingsTab::AcpServers && self.selected < ACCOUNT_COUNT =>
            {
                SettingsAction::Authenticate(crate::auth::AuthVendor::ALL[self.selected])
            }
            KeyCode::Enter
                if self.tab == SettingsTab::AcpServers && self.selected == ADD_SERVER_INDEX =>
            {
                self.open_catalog();
                SettingsAction::None
            }
            KeyCode::Enter if self.tab == SettingsTab::AcpPriority && self.selected == 0 => {
                self.open_priority_editor(PrioritySeat::Primary);
                SettingsAction::None
            }
            KeyCode::Enter if self.tab == SettingsTab::AcpPriority && self.selected == 1 => {
                self.open_priority_editor(PrioritySeat::Review);
                SettingsAction::None
            }
            KeyCode::Enter if self.tab == SettingsTab::AcpPriority && self.selected == 2 => {
                self.open_priority_editor(PrioritySeat::Subagents);
                SettingsAction::None
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
            SettingsTab::Agents | SettingsTab::Subagents => self.settings_rows(self.tab).len(),
            SettingsTab::AcpPriority => 3,
            SettingsTab::AcpServers => self.inventory.servers.len() + SERVER_ROW_OFFSET,
            SettingsTab::Appearance => 3,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.row_count();
        if len > 0 {
            self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
        }
    }

    fn change_selected(&mut self, delta: i32) -> SettingsAction {
        if matches!(self.tab, SettingsTab::Agents | SettingsTab::Subagents) {
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
                SettingsRow::DiscreteReview | SettingsRow::AutomaticQuotaFailover => {
                    return SettingsAction::None;
                }
            }
            self.notice = None;
            return SettingsAction::Changed;
        }
        match self.tab {
            SettingsTab::AcpPriority => {
                let seat = match self.selected {
                    0 => PrioritySeat::Primary,
                    1 => PrioritySeat::Review,
                    2 => PrioritySeat::Subagents,
                    _ => return SettingsAction::None,
                };
                self.cycle_source(seat, delta);
            }
            SettingsTab::AcpServers => {
                let Some(index) = self.selected.checked_sub(SERVER_ROW_OFFSET) else {
                    return SettingsAction::None;
                };
                let Some(server) = self.inventory.servers.get(index) else {
                    return SettingsAction::None;
                };
                let id = server.id.clone();
                let choices: &[AcpServerPolicy] = if server.origin.is_some() {
                    &[AcpServerPolicy::Enabled, AcpServerPolicy::Disabled]
                } else {
                    &[
                        AcpServerPolicy::Auto,
                        AcpServerPolicy::Enabled,
                        AcpServerPolicy::Disabled,
                    ]
                };
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
                self.config.feature_hints = !self.config.feature_hints;
            }
            _ => return SettingsAction::None,
        }
        self.notice = None;
        SettingsAction::Changed
    }

    fn toggle_selected(&mut self) -> SettingsAction {
        if matches!(self.tab, SettingsTab::Agents | SettingsTab::Subagents) {
            let Some(row) = self.settings_rows(self.tab).get(self.selected).copied() else {
                return SettingsAction::None;
            };
            match row {
                SettingsRow::DiscreteReview => {
                    self.config.agent.discrete_review = !self.config.agent.discrete_review;
                }
                SettingsRow::AutomaticQuotaFailover => {
                    self.config.subagents.auto_failover = !self.config.subagents.auto_failover;
                }
                _ => return SettingsAction::None,
            }
            self.notice = None;
            return SettingsAction::Changed;
        }
        match self.tab {
            SettingsTab::AcpPriority => return SettingsAction::None,
            SettingsTab::AcpServers => {
                let Some(index) = self.selected.checked_sub(SERVER_ROW_OFFSET) else {
                    return SettingsAction::None;
                };
                let Some(server) = self.inventory.servers.get(index) else {
                    return SettingsAction::None;
                };
                let id = server.id.clone();
                let policy = if server.origin.is_some() {
                    if server.policy == AcpServerPolicy::Enabled {
                        AcpServerPolicy::Disabled
                    } else {
                        AcpServerPolicy::Enabled
                    }
                } else if server.policy == AcpServerPolicy::Auto && !server.detected {
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
                rows.push(SettingsRow::ReviewModel);
                rows.push(SettingsRow::DiscreteReview);
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
            SessionDefaultsSeat::Subagents => &self.config.subagents.session_defaults,
        }
    }

    fn session_defaults_mut(
        &mut self,
        seat: SessionDefaultsSeat,
    ) -> &mut std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
        match seat {
            SessionDefaultsSeat::Primary => &mut self.config.agent.session_defaults,
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
        if matches!(
            option.category,
            Some(agent_client_protocol::schema::v1::SessionConfigOptionCategory::ThoughtLevel)
        ) || option.id.to_string() == crate::acp::REASONING_EFFORT_CONFIG_ID
        {
            match seat {
                SessionDefaultsSeat::Primary => {
                    self.config.agent.reasoning_effort = Some(value);
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
        match role {
            0 => self.config.agent.model.clone_from(&choices[next]),
            1 => self.config.review.model.clone_from(&choices[next]),
            2 => self.config.subagents.model.clone_from(&choices[next]),
            _ => {}
        }
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

    fn priority(&self, seat: PrioritySeat) -> &Vec<String> {
        match seat {
            PrioritySeat::Primary => &self.config.agent.acp_priority,
            PrioritySeat::Review => &self.config.review.acp_priority,
            PrioritySeat::Subagents => &self.config.subagents.acp_priority,
        }
    }

    pub(crate) fn source(&self, seat: PrioritySeat) -> &Option<String> {
        match seat {
            PrioritySeat::Primary => &self.config.agent.acp_source,
            PrioritySeat::Review => &self.config.review.acp_source,
            PrioritySeat::Subagents => &self.config.subagents.acp_source,
        }
    }

    fn source_mut(&mut self, seat: PrioritySeat) -> &mut Option<String> {
        match seat {
            PrioritySeat::Primary => &mut self.config.agent.acp_source,
            PrioritySeat::Review => &mut self.config.review.acp_source,
            PrioritySeat::Subagents => &mut self.config.subagents.acp_source,
        }
    }

    fn cycle_source(&mut self, seat: PrioritySeat, delta: i32) {
        let choices = std::iter::once(None)
            .chain(self.effective_priority(seat).into_iter().map(Some))
            .collect::<Vec<_>>();
        let current = choices
            .iter()
            .position(|choice| choice == self.source(seat))
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(choices.len() as i32) as usize;
        *self.source_mut(seat) = choices[next].clone();
        self.notice = Some(
            "Source constraint updated; start a new session or restart Mjolnir to apply it."
                .to_string(),
        );
    }

    fn priority_mut(&mut self, seat: PrioritySeat) -> &mut Vec<String> {
        match seat {
            PrioritySeat::Primary => &mut self.config.agent.acp_priority,
            PrioritySeat::Review => &mut self.config.review.acp_priority,
            PrioritySeat::Subagents => &mut self.config.subagents.acp_priority,
        }
    }

    pub(crate) fn effective_priority(&self, seat: PrioritySeat) -> Vec<String> {
        let mut priority = self.priority(seat).clone();
        for server in &self.inventory.servers {
            if !priority.contains(&server.id) {
                priority.push(server.id.clone());
            }
        }
        priority
    }

    fn open_priority_editor(&mut self, seat: PrioritySeat) {
        let priority = self.effective_priority(seat);
        *self.priority_mut(seat) = priority;
        self.priority_editor = Some(seat);
        self.priority_selected = 0;
        self.notice = None;
    }

    fn handle_priority_key(&mut self, code: KeyCode) -> SettingsAction {
        let Some(seat) = self.priority_editor else {
            return SettingsAction::None;
        };
        let len = self.priority(seat).len();
        match code {
            KeyCode::Esc | KeyCode::Enter => {
                self.priority_editor = None;
                self.notice = None;
            }
            KeyCode::Up | KeyCode::Char('k') if len > 0 => {
                self.priority_selected = self
                    .priority_selected
                    .checked_sub(1)
                    .unwrap_or(len.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                self.priority_selected = (self.priority_selected + 1) % len;
            }
            KeyCode::Left | KeyCode::Char('h') if self.priority_selected > 0 => {
                let selected = self.priority_selected;
                self.priority_mut(seat).swap(selected, selected - 1);
                self.priority_selected -= 1;
                self.notice = Some(
                    "Priority updated; start a new session or restart Mjolnir to apply it."
                        .to_string(),
                );
            }
            KeyCode::Right | KeyCode::Char('l') if self.priority_selected + 1 < len => {
                let selected = self.priority_selected;
                self.priority_mut(seat).swap(selected, selected + 1);
                self.priority_selected += 1;
                self.notice = Some(
                    "Priority updated; start a new session or restart Mjolnir to apply it."
                        .to_string(),
                );
            }
            KeyCode::Char('r') => {
                *self.priority_mut(seat) = crate::config::DEFAULT_ACP_PRIORITY
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                self.priority_selected = 0;
                self.notice = Some(
                    "Priority reset; start a new session or restart Mjolnir to apply it."
                        .to_string(),
                );
            }
            _ => return SettingsAction::None,
        }
        SettingsAction::Changed
    }

    fn priority_summary(&self, seat: PrioritySeat) -> String {
        self.effective_priority(seat)
            .iter()
            .map(|id| {
                self.inventory
                    .servers
                    .iter()
                    .find(|server| server.id == *id)
                    .map_or_else(|| id.clone(), |server| server.label.clone())
            })
            .collect::<Vec<_>>()
            .join(" → ")
    }

    fn source_summary(&self, seat: PrioritySeat) -> String {
        self.source(seat).as_ref().map_or_else(
            || "any enabled source".to_string(),
            |id| {
                self.inventory
                    .servers
                    .iter()
                    .find(|server| server.id == *id)
                    .map_or_else(|| id.clone(), |server| format!("{} only", server.label))
            },
        )
    }

    pub(crate) fn staged_model_detail(&self, model: &str) -> String {
        if model == "auto" {
            return "automatic selection".to_string();
        }
        if model == crate::config::DISABLED_MODEL {
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

    fn open_catalog(&mut self) {
        self.acp_view = AcpView::Catalog {
            filter: String::new(),
        };
        self.selected = 0;
        if !matches!(
            self.registry,
            RegistryState::NotLoaded | RegistryState::Error(_)
        ) {
            return;
        }
        let shared = Arc::new(Mutex::new(None));
        let output = Arc::clone(&shared);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let result = crate::registry::load()
                        .await
                        .map_err(|error| format!("{error:#}"));
                    if let Ok(mut slot) = output.lock() {
                        *slot = Some(result);
                    }
                });
                self.registry = RegistryState::Loading(shared);
            }
            Err(error) => self.registry = RegistryState::Error(error.to_string()),
        }
    }

    pub(crate) fn poll_background(&mut self) {
        if let RegistryState::Loading(shared) = &self.registry {
            let completed = shared.lock().ok().and_then(|mut slot| slot.take());
            if let Some(result) = completed {
                self.registry = match result {
                    Ok(registry) => RegistryState::Ready(registry),
                    Err(error) => RegistryState::Error(error),
                };
                self.selected = 0;
            }
        }
        let completed = self.installing.as_ref().and_then(|installing| {
            installing
                .snapshot
                .lock()
                .ok()
                .and_then(|mut snapshot| snapshot.result.take())
                .map(|result| (installing.agent.clone(), result))
        });
        if let Some((agent, result)) = completed {
            self.installing = None;
            match result {
                Ok((command, args)) => self.add_server(ConfiguredAcpServer {
                    env: agent
                        .distribution
                        .binary
                        .as_ref()
                        .and_then(|targets| targets.get(&crate::registry::current_platform()))
                        .map(|target| target.env.clone())
                        .unwrap_or_default(),
                    id: agent.id,
                    label: agent.name,
                    command,
                    args,
                    origin: AcpServerOrigin::Registry,
                    policy: AcpServerPolicy::Enabled,
                }),
                Err(error) => self.notice = Some(format!("Install failed: {error}")),
            }
        }
        self.refresh_inventory();
    }

    pub(crate) fn refresh_after_auth(&mut self, notice: String) {
        self.refresh_inventory();
        self.notice = Some(notice);
    }

    fn refresh_inventory(&mut self) {
        self.inventory = crate::roster::rediscover_inventory(&self.config, &self.inventory);
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    fn filtered_agents(&self) -> Vec<&Agent> {
        let RegistryState::Ready(registry) = &self.registry else {
            return Vec::new();
        };
        let filter = match &self.acp_view {
            AcpView::Catalog { filter } => filter.to_ascii_lowercase(),
            _ => String::new(),
        };
        let configured = self
            .inventory
            .servers
            .iter()
            .map(|server| server.id.as_str())
            .collect::<HashSet<_>>();
        let platform = crate::registry::current_platform();
        let mut agents = registry
            .agents
            .iter()
            .filter(|agent| !configured.contains(agent.id.as_str()))
            .filter(|agent| agent.preferred_kind(&platform).is_some())
            .filter(|agent| {
                filter.is_empty()
                    || agent.name.to_ascii_lowercase().contains(&filter)
                    || agent.id.to_ascii_lowercase().contains(&filter)
            })
            .collect::<Vec<_>>();
        agents.sort_by_key(|agent| agent.name.to_ascii_lowercase());
        agents
    }

    fn handle_catalog_key(&mut self, code: KeyCode) -> SettingsAction {
        if self.installing.is_some() {
            if code == KeyCode::Esc {
                if let Some(installing) = &self.installing {
                    installing.abort.abort();
                }
                self.installing = None;
            }
            return SettingsAction::None;
        }
        match code {
            KeyCode::Esc => {
                self.acp_view = AcpView::Servers;
                self.selected = 0;
            }
            KeyCode::Up => {
                let count = self.filtered_agents().len() + 1;
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or(count.saturating_sub(1));
            }
            KeyCode::Down => {
                let count = self.filtered_agents().len() + 1;
                self.selected = (self.selected + 1) % count.max(1);
            }
            KeyCode::Backspace => {
                if let AcpView::Catalog { filter } = &mut self.acp_view {
                    filter.pop();
                }
                self.selected = 0;
            }
            KeyCode::Char(c) => {
                if let AcpView::Catalog { filter } = &mut self.acp_view {
                    filter.push(c);
                }
                self.selected = 0;
            }
            KeyCode::Enter => {
                let agents = self.filtered_agents();
                if self.selected == 0 {
                    self.acp_view = AcpView::Custom {
                        name: String::new(),
                        command: String::new(),
                        field: 0,
                    };
                    self.selected = 0;
                } else if let Some(agent) = agents.get(self.selected - 1).cloned().cloned() {
                    self.select_registry_agent(agent);
                }
            }
            _ => {}
        }
        SettingsAction::None
    }

    fn select_registry_agent(&mut self, agent: Agent) {
        let platform = crate::registry::current_platform();
        match agent.preferred_kind(&platform) {
            Some(DistributionKind::Binary) => {
                let Some(target) = agent
                    .distribution
                    .binary
                    .as_ref()
                    .and_then(|targets| targets.get(&platform))
                    .cloned()
                else {
                    return;
                };
                let snapshot = Arc::new(Mutex::new(InstallSnapshot::default()));
                let output = Arc::clone(&snapshot);
                let id = agent.id.clone();
                let version = agent.version.clone();
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
                let progress_output = Arc::clone(&snapshot);
                tokio::spawn(async move {
                    while let Some(progress) = progress_rx.recv().await {
                        let Ok(mut state) = progress_output.lock() else {
                            break;
                        };
                        match progress {
                            Progress::Started { total_bytes } => state.total_bytes = total_bytes,
                            Progress::Downloaded { downloaded_bytes } => {
                                state.downloaded_bytes = downloaded_bytes;
                            }
                            Progress::Extracting => state.extracting = true,
                            Progress::Done => {}
                        }
                    }
                });
                let task = tokio::spawn(async move {
                    let result =
                        crate::install::install_or_resolve(&id, &version, &target, progress_tx)
                            .await
                            .map_err(|error| format!("{error:#}"));
                    if let Ok(mut state) = output.lock() {
                        state.result = Some(result);
                    }
                });
                self.installing = Some(InstallingServer {
                    agent,
                    snapshot,
                    abort: task.abort_handle(),
                });
            }
            Some(DistributionKind::Npx) => {
                let package = agent.distribution.npx.as_ref().expect("npx selected");
                let mut args = vec!["-y".to_string(), package.package.clone()];
                args.extend(package.args.clone());
                self.add_server(ConfiguredAcpServer {
                    id: agent.id,
                    label: agent.name,
                    command: PathBuf::from("npx"),
                    args,
                    env: package.env.clone(),
                    origin: AcpServerOrigin::Registry,
                    policy: AcpServerPolicy::Enabled,
                });
            }
            Some(DistributionKind::Uvx) => {
                let package = agent.distribution.uvx.as_ref().expect("uvx selected");
                let mut args = vec![package.package.clone()];
                args.extend(package.args.clone());
                self.add_server(ConfiguredAcpServer {
                    id: agent.id,
                    label: agent.name,
                    command: PathBuf::from("uvx"),
                    args,
                    env: package.env.clone(),
                    origin: AcpServerOrigin::Registry,
                    policy: AcpServerPolicy::Enabled,
                });
            }
            None => self.notice = Some("No supported distribution for this platform".to_string()),
        }
    }

    fn add_server(&mut self, server: ConfiguredAcpServer) {
        self.config
            .acp
            .servers
            .retain(|existing| existing.id != server.id);
        self.config.acp.policies.remove(&server.id);
        self.config.acp.servers.push(server);
        self.refresh_inventory();
        self.acp_view = AcpView::Servers;
        self.selected = self.inventory.servers.len().saturating_sub(1) + SERVER_ROW_OFFSET;
        self.notice = None;
    }

    fn handle_custom_key(&mut self, code: KeyCode) -> SettingsAction {
        match code {
            KeyCode::Esc => {
                self.acp_view = AcpView::Catalog {
                    filter: String::new(),
                }
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                if let AcpView::Custom { field, .. } = &mut self.acp_view {
                    *field = (*field + 1) % 2;
                }
            }
            KeyCode::Backspace => {
                if let AcpView::Custom {
                    name,
                    command,
                    field,
                } = &mut self.acp_view
                {
                    if *field == 0 {
                        name.pop();
                    } else {
                        command.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if let AcpView::Custom {
                    name,
                    command,
                    field,
                } = &mut self.acp_view
                {
                    if *field == 0 {
                        name.push(c);
                    } else {
                        command.push(c);
                    }
                }
            }
            KeyCode::Enter => {
                let AcpView::Custom { name, command, .. } = &self.acp_view else {
                    return SettingsAction::None;
                };
                let name = name.trim();
                let parts = match shell_words::split(command) {
                    Ok(parts) if !parts.is_empty() => parts,
                    Ok(_) => {
                        self.notice = Some("Command is required".to_string());
                        return SettingsAction::None;
                    }
                    Err(error) => {
                        self.notice = Some(format!("Invalid command: {error}"));
                        return SettingsAction::None;
                    }
                };
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    self.notice =
                        Some("Name must contain only letters, digits, '-' or '_'".to_string());
                    return SettingsAction::None;
                }
                self.add_server(ConfiguredAcpServer {
                    id: format!("custom:{name}"),
                    label: name.to_string(),
                    command: PathBuf::from(&parts[0]),
                    args: parts[1..].to_vec(),
                    env: Default::default(),
                    origin: AcpServerOrigin::Custom,
                    policy: AcpServerPolicy::Enabled,
                });
            }
            _ => {}
        }
        SettingsAction::None
    }

    pub(crate) fn cancel_background(&mut self) {
        if let Some(installing) = self.installing.take() {
            installing.abort.abort();
        }
    }
}

pub fn draw_settings_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    title: &str,
) {
    if area.width < 28 || area.height < 12 {
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
        SettingsTab::Agents => draw_agents(frame, rows[1], editor, theme),
        SettingsTab::Subagents => draw_subagents(frame, rows[1], editor, theme),
        SettingsTab::AcpPriority => draw_acp_priority(frame, rows[1], editor, theme),
        SettingsTab::AcpServers => draw_servers(frame, rows[1], editor, theme),
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
    let footer = if editor.priority_editor.is_some() {
        "↑/↓ select · ←/→ move · r reset default · Enter done · Esc back"
    } else {
        match editor.acp_view {
            AcpView::Catalog { .. } if editor.installing.is_some() => "Esc cancel install view",
            AcpView::Catalog { .. } => "Type filter · ↑/↓ select · Enter add · Esc back",
            AcpView::Custom { .. } => "Tab field · Enter add · Esc back",
            AcpView::Servers
                if editor.tab == SettingsTab::AcpServers && editor.selected < ACCOUNT_COUNT =>
            {
                "Enter sign in · ↑/↓ select · Tab view · Esc cancel"
            }
            AcpView::Servers
                if editor.tab == SettingsTab::AcpServers && editor.selected == ADD_SERVER_INDEX =>
            {
                "Enter add server · ↑/↓ select · Tab view · Esc cancel"
            }
            AcpView::Servers => {
                "Tab view · ↑/↓ select · ←/→ change · Space toggle · Enter save · Esc cancel"
            }
        }
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
    let source_disabled = |config: &Config, source: &str| {
        config
            .acp
            .servers
            .iter()
            .find(|server| server.id == source)
            .map_or_else(|| config.acp.policy(source), |server| server.policy)
            == AcpServerPolicy::Disabled
    };
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
        if model == "auto"
            || model == crate::config::DISABLED_MODEL
            // Custom-selector models have no native built-in adapter; only
            // their catalog entry (when present) can judge them.
            || (model.starts_with("custom/")
                && !choices.iter().any(|choice| choice.model == model))
        {
            continue;
        }
        // Judge the route from the model catalog when it knows the model,
        // falling back to the provider's native adapter — the catalog may
        // have been resolved while that vendor was disabled and lack the
        // entry entirely. Custom-server models resolve to their own id.
        let route = choices
            .iter()
            .find(|choice| choice.model == model)
            .and_then(|choice| choice.adapter.clone())
            .or_else(|| crate::roster::native_source_id(&model));
        // No catalog entry and no built-in adapter for the model's provider
        // means nothing enabled can serve the pin either.
        if route
            .as_deref()
            .is_none_or(|route| source_disabled(config, route))
        {
            let slot = match seat {
                Seat::Agent => &mut config.agent.model,
                Seat::Review => &mut config.review.model,
                Seat::Subagents => &mut config.subagents.model,
            };
            "auto".clone_into(slot);
            notices.push(format!(
                "{label} model {model} is not provided by any enabled ACP server; switched to automatic selection"
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
    let mut lines = vec![Line::styled(
        match source.as_deref() {
            Some(source) => format!(
                "Saved primary defaults via {source}. Compatible options update the active primary on save; models apply on /new or /clear."
            ),
            None => "No primary ACP route is resolved; choose a model or source to discover its session options."
                .to_string(),
        },
        Style::default().ink(theme.muted),
    )];
    if !has_options {
        lines.push(Line::styled(
            "No additional selectable session options were reported for this primary route.",
            Style::default().ink(theme.muted),
        ));
    }
    lines.push(Line::raw(""));
    let mut selected_line_index = 0;
    for (row_index, row) in rows.into_iter().enumerate() {
        let selected = editor.selected == row_index;
        if selected {
            selected_line_index = lines.len();
        }
        match row {
            SettingsRow::PrimaryModel => {
                let model = &editor.config.agent.model;
                lines.push(selected_line(
                    selected,
                    format!("Primary model < {model} >"),
                    theme,
                ));
                lines.push(Line::styled(
                    format!(
                        "  saved: {} · active: {}",
                        editor.staged_model_detail(model),
                        editor.active_model_detail(0)
                    ),
                    Style::default().ink(theme.muted),
                ));
            }
            SettingsRow::ReviewModel => {
                let model = &editor.config.review.model;
                lines.push(selected_line(
                    selected,
                    format!("Review model < {model} >"),
                    theme,
                ));
                lines.push(Line::styled(
                    format!(
                        "  saved: {} · active: {}",
                        editor.staged_model_detail(model),
                        editor.active_model_detail(1)
                    ),
                    Style::default().ink(theme.muted),
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
                    .unwrap_or_else(|| "next session only".to_string());
                lines.push(selected_line(
                    selected,
                    format!("{} < {saved_label} >", option.name),
                    theme,
                ));
                lines.push(Line::styled(
                    if compatible {
                        format!("  saved default · active: {active}")
                    } else {
                        format!(
                            "  saved value is unavailable on {server_id}",
                            server_id = server.id
                        )
                    },
                    Style::default().ink(if compatible { theme.muted } else { theme.error }),
                ));
            }
            SettingsRow::DiscreteReview => lines.push(selected_line(
                selected,
                format!(
                    "Discrete review [{}]",
                    on_off(editor.config.agent.discrete_review)
                ),
                theme,
            )),
            SettingsRow::SubagentModel
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
    let mut lines = vec![Line::styled(
        match source.as_deref() {
            Some(source) => format!(
                "Saved delegated-session defaults via {source}. Changes apply only to subagents started later."
            ),
            None => "No subagent ACP route is resolved; choose a model or source to discover its session options."
                .to_string(),
        },
        Style::default().ink(theme.muted),
    )];
    if !has_options && editor.config.subagents.model != crate::config::DISABLED_MODEL {
        lines.push(Line::styled(
            "No additional selectable session options were reported for this subagent route.",
            Style::default().ink(theme.muted),
        ));
    }
    lines.push(Line::raw(""));
    let mut selected_line_index = 0;
    for (row_index, row) in rows.into_iter().enumerate() {
        let selected = editor.selected == row_index;
        if selected {
            selected_line_index = lines.len();
        }
        match row {
            SettingsRow::SubagentModel => {
                let model = &editor.config.subagents.model;
                lines.push(selected_line(
                    selected,
                    format!("Subagent model < {model} >"),
                    theme,
                ));
                lines.push(Line::styled(
                    format!(
                        "  saved: {} · active pool: {}",
                        editor.staged_model_detail(model),
                        editor.active_model_detail(2)
                    ),
                    Style::default().ink(theme.muted),
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
                lines.push(Line::styled(
                    if compatible {
                        "  saved default · already-running subagents are unchanged".to_string()
                    } else {
                        format!(
                            "  saved value is unavailable on {server_id}",
                            server_id = server.id
                        )
                    },
                    Style::default().ink(if compatible { theme.muted } else { theme.error }),
                ));
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
            SettingsRow::PrimaryModel | SettingsRow::ReviewModel | SettingsRow::DiscreteReview => {}
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

fn draw_acp_priority(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    if let Some(seat) = editor.priority_editor {
        let title = match seat {
            PrioritySeat::Primary => "Primary ACP priority",
            PrioritySeat::Review => "Review ACP priority",
            PrioritySeat::Subagents => "Subagent ACP priority",
        };
        let mut lines = vec![
            Line::styled(
                format!("{title} · first matching adapter wins"),
                Style::default().ink(theme.muted),
            ),
            Line::styled(
                "r resets to Codex → Claude → Kimi",
                Style::default().ink(theme.muted),
            ),
            Line::raw(""),
        ];
        for (index, id) in editor.priority(seat).iter().enumerate() {
            let label = editor
                .inventory
                .servers
                .iter()
                .find(|server| server.id == *id)
                .map_or(id.as_str(), |server| server.label.as_str());
            lines.push(selected_line(
                editor.priority_selected == index,
                format!("{}. {label} ({id})", index + 1),
                theme,
            ));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    let lines = vec![
        Line::styled(
            "Left/Right constrains a seat to one source; Enter edits fallback priority.",
            Style::default().ink(theme.muted),
        ),
        Line::raw(""),
        selected_line(
            editor.selected == 0,
            format!(
                "Primary   < {} >  [Enter] {}",
                editor.source_summary(PrioritySeat::Primary),
                editor.priority_summary(PrioritySeat::Primary)
            ),
            theme,
        ),
        selected_line(
            editor.selected == 1,
            format!(
                "Review    < {} >  [Enter] {}",
                editor.source_summary(PrioritySeat::Review),
                editor.priority_summary(PrioritySeat::Review)
            ),
            theme,
        ),
        selected_line(
            editor.selected == 2,
            format!(
                "Subagents < {} >  [Enter] {}",
                editor.source_summary(PrioritySeat::Subagents),
                editor.priority_summary(PrioritySeat::Subagents)
            ),
            theme,
        ),
        Line::raw(""),
        Line::styled(
            "ACP Servers controls eligibility; source constraints preserve Auto within one route.",
            Style::default().ink(theme.muted),
        ),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_servers(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    match &editor.acp_view {
        AcpView::Catalog { filter } => {
            draw_catalog(frame, area, editor, filter, theme);
            return;
        }
        AcpView::Custom {
            name,
            command,
            field,
        } => {
            let lines = vec![
                Line::styled(
                    "Add a custom ACP server command.",
                    Style::default().ink(theme.muted),
                ),
                Line::raw(""),
                selected_line(*field == 0, format!("Name     {name}"), theme),
                selected_line(*field == 1, format!("Command  {command}"), theme),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }
        AcpView::Servers => {}
    }
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
    lines.push(selected_line(
        editor.selected == ADD_SERVER_INDEX,
        "+ Add server".to_string(),
        theme,
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Servers",
        Style::default()
            .ink(theme.muted)
            .add_modifier(Modifier::BOLD),
    ));
    let rows_available = area.height.saturating_sub(lines.len() as u16) as usize / 2;
    let selected_server = editor.selected.saturating_sub(SERVER_ROW_OFFSET);
    let start = selected_server.saturating_sub(rows_available.saturating_sub(1));
    for (index, server) in editor
        .inventory
        .servers
        .iter()
        .enumerate()
        .skip(start)
        .take(rows_available)
    {
        let status = if server.installing {
            "installing".to_string()
        } else if server.policy == AcpServerPolicy::Disabled {
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
            format!(
                "[{}] {:<16} {status}",
                if server.installing {
                    "installing".to_string()
                } else {
                    server.policy.to_string()
                },
                server.label
            ),
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

fn draw_catalog(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    filter: &str,
    theme: TerminalTheme,
) {
    if let Some(installing) = &editor.installing {
        let snapshot = installing.snapshot.lock().ok();
        let status = snapshot.as_ref().map_or_else(
            || "installing".to_string(),
            |snapshot| {
                if snapshot.extracting {
                    "extracting".to_string()
                } else if let Some(total) = snapshot.total_bytes {
                    let percent = snapshot.downloaded_bytes.saturating_mul(100) / total.max(1);
                    format!("downloading {percent}%")
                } else {
                    format!("downloading {} bytes", snapshot.downloaded_bytes)
                }
            },
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("Installing {}", installing.agent.name),
                    Style::default()
                        .ink(theme.text)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::raw(""),
                Line::raw(status),
                Line::raw(""),
                Line::styled("Esc cancels this view", Style::default().ink(theme.muted)),
            ]),
            area,
        );
        return;
    }

    let mut lines = vec![
        Line::styled(
            format!("ACP registry · filter: {filter}"),
            Style::default().ink(theme.muted),
        ),
        Line::raw(""),
    ];
    match &editor.registry {
        RegistryState::NotLoaded | RegistryState::Loading(_) => {
            lines.push(Line::raw("Loading registry..."));
            lines.push(selected_line(
                editor.selected == 0,
                "Custom command...".to_string(),
                theme,
            ));
        }
        RegistryState::Error(error) => {
            lines.push(Line::styled(
                format!("Registry unavailable: {error}"),
                Style::default().ink(theme.error),
            ));
            lines.push(selected_line(
                editor.selected == 0,
                "Custom command...".to_string(),
                theme,
            ));
        }
        RegistryState::Ready(_) => {
            let agents = editor.filtered_agents();
            if let Some(agent) = editor.selected.checked_sub(1).and_then(|i| agents.get(i)) {
                let platform = crate::registry::current_platform();
                let kind = agent.preferred_kind(&platform);
                let (command, download) = match kind {
                    Some(DistributionKind::Binary) => {
                        let command = agent
                            .distribution
                            .binary
                            .as_ref()
                            .and_then(|targets| targets.get(&platform))
                            .map(|target| target.cmd.as_str())
                            .unwrap_or("binary");
                        (command.to_string(), "downloads into Mjolnir data")
                    }
                    Some(DistributionKind::Npx) => {
                        let package = agent
                            .distribution
                            .npx
                            .as_ref()
                            .map(|package| package.package.as_str())
                            .unwrap_or("package");
                        (format!("npx -y {package}"), "downloads on first launch")
                    }
                    Some(DistributionKind::Uvx) => {
                        let package = agent
                            .distribution
                            .uvx
                            .as_ref()
                            .map(|package| package.package.as_str())
                            .unwrap_or("package");
                        (format!("uvx {package}"), "downloads on first launch")
                    }
                    None => ("unsupported".to_string(), "not installable"),
                };
                lines.push(Line::styled(
                    format!("{} · v{} · {download}", agent.name, agent.version),
                    Style::default()
                        .ink(theme.text)
                        .add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::styled(command, Style::default().ink(theme.muted)));
                lines.push(Line::raw(""));
            }
            lines.push(selected_line(
                editor.selected == 0,
                "Custom command...".to_string(),
                theme,
            ));
            let visible = area.height.saturating_sub(lines.len() as u16) as usize;
            let start = editor.selected.saturating_sub(visible.saturating_sub(1));
            for (index, agent) in agents
                .iter()
                .enumerate()
                .skip(start.saturating_sub(1))
                .take(visible)
            {
                let kind = agent
                    .preferred_kind(&crate::registry::current_platform())
                    .map(DistributionKind::label)
                    .unwrap_or("unsupported");
                lines.push(selected_line(
                    editor.selected == index + 1,
                    format!("{:<24} {kind} · {}", agent.name, agent.description),
                    theme,
                ));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_appearance(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let lines = vec![
        Line::styled(
            "Appearance changes preview immediately.",
            Style::default().ink(theme.muted),
        ),
        Line::raw(""),
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
        spinner_preview_line(editor.selected == 1, editor.config.spinner, theme),
        selected_line(
            editor.selected == 2,
            format!(
                "Feature tips < {} >",
                if editor.config.feature_hints {
                    "on"
                } else {
                    "off"
                }
            ),
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
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

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn primary_and_subagent_panels_edit_arbitrary_options_with_separate_scope() {
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

        editor.tab = SettingsTab::Subagents;
        for selected in 1..=5 {
            editor.selected = selected;
            assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        }
        assert_eq!(
            editor.config.subagents.session_defaults[&server_id].len(),
            5
        );
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
        editor.selected = editor
            .settings_rows(SettingsTab::Agents)
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
    fn primary_and_subagent_acp_priorities_reorder_independently() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpPriority;
        editor.selected = 0;
        assert_eq!(editor.handle_key(KeyCode::Enter), SettingsAction::None);
        assert_eq!(editor.priority_editor, Some(PrioritySeat::Primary));
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.agent.acp_priority[0], "claude-acp");
        assert_eq!(editor.config.subagents.acp_priority[0], "codex-acp");
        assert_eq!(
            editor.notice.as_deref(),
            Some("Priority updated; start a new session or restart Mjolnir to apply it.")
        );
        editor.handle_key(KeyCode::Enter);

        editor.selected = 2;
        editor.handle_key(KeyCode::Enter);
        editor.handle_key(KeyCode::Right);
        assert_eq!(editor.config.subagents.acp_priority[0], "claude-acp");
        assert_eq!(editor.config.agent.acp_priority[0], "claude-acp");
        assert_eq!(editor.config.agent.acp_priority[1], "codex-acp");
        assert_eq!(
            editor.handle_key(KeyCode::Char('r')),
            SettingsAction::Changed
        );
        assert_eq!(
            editor.config.subagents.acp_priority,
            crate::config::DEFAULT_ACP_PRIORITY.map(str::to_string)
        );
        assert_eq!(
            editor.notice.as_deref(),
            Some("Priority reset; start a new session or restart Mjolnir to apply it.")
        );
    }

    #[test]
    fn acp_source_constraints_cycle_independently() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpPriority;

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.agent.acp_source.as_deref(), Some("codex-acp"));
        assert_eq!(editor.config.review.acp_source, None);
        assert_eq!(editor.config.subagents.acp_source, None);

        editor.selected = 2;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.subagents.acp_source.as_deref(),
            Some("codex-acp")
        );
        assert_eq!(editor.handle_key(KeyCode::Left), SettingsAction::Changed);
        assert_eq!(editor.config.subagents.acp_source, None);
    }

    #[test]
    fn acp_priority_tab_exposes_both_seat_editors() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpPriority;
        let backend = ratatui::backend::TestBackend::new(90, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("ACP Priority"), "rendered:\n{rendered}");
        assert!(
            rendered.contains("Primary   < any enabled source >"),
            "rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("Subagents < any enabled source >"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn auto_server_can_be_explicitly_enabled() {
        // An explicit policy keeps a built-in visible regardless of whether
        // this host actually has it installed.
        let mut config = Config::default();
        config.set_acp_server_policy("kimi", AcpServerPolicy::Disabled);
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        let server_index = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "kimi")
            .expect("kimi");
        editor.selected = server_index + SERVER_ROW_OFFSET;
        editor.inventory.servers[server_index].detected = false;
        editor.inventory.servers[server_index].policy = AcpServerPolicy::Auto;

        // Exercise the transition without refreshing host-specific discovery.
        assert_eq!(editor.toggle_selected(), SettingsAction::Changed);
        assert_eq!(editor.config.acp.policy("kimi"), AcpServerPolicy::Enabled);
    }

    #[test]
    fn registry_npx_selection_adds_an_explicit_server() {
        let registry = Registry::from_json(
            r#"{"agents":[{"id":"gemini","name":"Gemini","version":"1","distribution":{"npx":{"package":"@google/gemini-cli","args":["--acp"]}}}]}"#,
        )
        .expect("registry");
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        editor.acp_view = AcpView::Catalog {
            filter: String::new(),
        };
        editor.registry = RegistryState::Ready(registry);
        editor.selected = 1;

        editor.handle_key(KeyCode::Enter);

        let server = editor
            .config
            .acp
            .servers
            .iter()
            .find(|server| server.id == "gemini")
            .expect("configured registry server");
        assert_eq!(server.command, PathBuf::from("npx"));
        assert_eq!(server.args, vec!["-y", "@google/gemini-cli", "--acp"]);
        assert_eq!(server.policy, AcpServerPolicy::Enabled);
    }

    #[test]
    fn accounts_and_add_server_are_direct_actions() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;

        editor.selected = 1;
        assert_eq!(
            editor.handle_key(KeyCode::Enter),
            SettingsAction::Authenticate(crate::auth::AuthVendor::Kimi)
        );

        editor.selected = ADD_SERVER_INDEX;
        assert_eq!(editor.handle_key(KeyCode::Enter), SettingsAction::None);
        assert!(matches!(editor.acp_view, AcpView::Catalog { .. }));
    }

    #[test]
    fn appearance_tab_toggles_feature_hints() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::Appearance;
        editor.selected = 2;

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert!(!editor.config.feature_hints);
        assert_eq!(editor.handle_key(KeyCode::Char(' ')), SettingsAction::None);
        assert!(!editor.config.feature_hints);
    }
}
