//! Shared first-startup and in-session settings editor.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionConfigOption;
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::{AcpServerOrigin, AcpServerPolicy, Config, ConfiguredAcpServer, ModelsConfig};
use crate::install::Progress;
use crate::palette::TerminalTheme;
use crate::registry::{Agent, DistributionKind, Registry};
use crate::roster::{AcpInventory, ModelChoice};
use crate::spinner::SpinnerStyle;
use crate::theme::TerminalThemeKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Agents,
    Subagents,
    AcpServers,
    Appearance,
}

impl SettingsTab {
    const ALL: [Self; 4] = [
        Self::Agents,
        Self::Subagents,
        Self::AcpServers,
        Self::Appearance,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
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

#[derive(Debug, Clone)]
pub struct SettingsEditor {
    pub config: Config,
    pub tab: SettingsTab,
    pub selected: usize,
    pub notice: Option<String>,
    choices: Vec<ModelChoice>,
    active_models: Option<ModelsConfig>,
    inventory: AcpInventory,
    primary_session_config_options: Vec<SessionConfigOption>,
    subagent_session_config_options: Vec<SessionConfigOption>,
    primary_session_config_source_id: Option<String>,
    subagent_session_config_source_id: Option<String>,
    primary_session_config_model: Option<String>,
    subagent_session_config_model: Option<String>,
    acp_view: AcpView,
    registry: RegistryState,
    installing: Option<InstallingServer>,
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
            inventory,
            primary_session_config_options: Vec::new(),
            subagent_session_config_options: Vec::new(),
            primary_session_config_source_id: None,
            subagent_session_config_source_id: None,
            primary_session_config_model: None,
            subagent_session_config_model: None,
            acp_view: AcpView::Servers,
            registry: RegistryState::NotLoaded,
            installing: None,
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

    /// Options from the active primary session. They are authoritative for
    /// the live primary; cached discovery is used only when it is absent.
    pub fn with_session_config_options(mut self, options: Vec<SessionConfigOption>) -> Self {
        self.primary_session_config_options = options;
        self
    }

    pub fn with_primary_session_config_source_id(mut self, source_id: Option<String>) -> Self {
        self.primary_session_config_source_id = source_id;
        if self.primary_session_config_options.is_empty() {
            self.primary_session_config_options = Self::cached_options(
                &self.config,
                true,
                self.primary_session_config_source_id.as_deref(),
            );
        }
        self
    }

    pub fn with_primary_session_config_model(mut self, model: Option<String>) -> Self {
        self.primary_session_config_model = model;
        self.refresh_detached_options(true);
        self
    }

    pub fn with_subagent_session_config_options(
        mut self,
        options: Vec<SessionConfigOption>,
    ) -> Self {
        self.subagent_session_config_options = options;
        self
    }

    pub fn with_subagent_session_config_source_id(mut self, source_id: Option<String>) -> Self {
        self.subagent_session_config_source_id = source_id;
        if self.subagent_session_config_options.is_empty() {
            self.subagent_session_config_options = Self::cached_options(
                &self.config,
                false,
                self.subagent_session_config_source_id.as_deref(),
            );
        }
        self
    }

    pub fn with_subagent_session_config_model(mut self, model: Option<String>) -> Self {
        self.subagent_session_config_model = model;
        self.refresh_detached_options(false);
        self
    }

    /// Detached probe options are only usable for the exact adapter/model
    /// that produced them. Otherwise prefer the matching disk cache (or no
    /// controls) rather than exposing one model's values for another.
    fn refresh_detached_options(&mut self, primary: bool) {
        let (options, source_id, advertised_model, configured_model) = if primary {
            (
                &mut self.primary_session_config_options,
                self.primary_session_config_source_id.as_deref(),
                self.primary_session_config_model.as_deref(),
                self.config.agent.model.as_str(),
            )
        } else {
            (
                &mut self.subagent_session_config_options,
                self.subagent_session_config_source_id.as_deref(),
                self.subagent_session_config_model.as_deref(),
                self.config.subagents.model.as_str(),
            )
        };
        if advertised_model.is_some_and(|model| model != configured_model) || options.is_empty() {
            *options = Self::cached_options(&self.config, primary, source_id);
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsAction {
        self.poll_background();
        if self.tab == SettingsTab::AcpServers {
            match self.acp_view {
                AcpView::Catalog { .. } => return self.handle_catalog_key(code),
                AcpView::Custom { .. } => return self.handle_custom_key(code),
                AcpView::Servers
                    if code == KeyCode::Char('r')
                        && self
                            .selected
                            .checked_sub(4)
                            .and_then(|index| self.inventory.servers.get(index))
                            .is_some_and(|server| {
                                server.id == "anvil" && server.error.is_some()
                            }) =>
                {
                    crate::anvil::retry_background_install();
                    return SettingsAction::None;
                }
                AcpView::Servers => {}
            }
        }
        match code {
            KeyCode::Esc => SettingsAction::Cancel,
            KeyCode::Enter if self.tab == SettingsTab::AcpServers && self.selected < 3 => {
                SettingsAction::Authenticate(crate::auth::AuthVendor::ALL[self.selected])
            }
            KeyCode::Enter if self.tab == SettingsTab::AcpServers && self.selected == 3 => {
                self.open_catalog();
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
            SettingsTab::Agents => 3 + self.session_option_row_count(true),
            SettingsTab::Subagents => 4 + self.session_option_row_count(false),
            SettingsTab::AcpServers => self.inventory.servers.len() + 4,
            SettingsTab::Appearance => 2,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.row_count();
        if len > 0 {
            self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
        }
    }

    fn change_selected(&mut self, delta: i32) -> SettingsAction {
        match self.tab {
            SettingsTab::Agents if self.selected == 0 => self.cycle_model(0, delta),
            SettingsTab::Agents if self.selected == 1 => self.cycle_reasoning(true, delta),
            SettingsTab::Agents if self.selected >= 3 => self.cycle_session_option(true, delta),
            SettingsTab::Subagents if self.selected == 0 => self.cycle_model(1, delta),
            SettingsTab::Subagents if self.selected == 1 => self.cycle_reasoning(false, delta),
            SettingsTab::Subagents if self.selected >= 4 => self.cycle_session_option(false, delta),
            SettingsTab::Subagents if self.selected == 2 => {
                self.config.subagents.max_parallel =
                    (self.config.subagents.max_parallel as i32 + delta).rem_euclid(17) as usize;
            }
            SettingsTab::AcpServers => {
                let Some(index) = self.selected.checked_sub(4) else {
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
                if id == "anvil" && choices[next] == AcpServerPolicy::Enabled {
                    crate::anvil::retry_background_install();
                }
                self.inventory = crate::roster::discover_inventory(&self.config);
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
            _ => return SettingsAction::None,
        }
        self.notice = None;
        SettingsAction::Changed
    }

    fn toggle_selected(&mut self) -> SettingsAction {
        match self.tab {
            SettingsTab::Agents if self.selected == 2 => {
                self.config.agent.discrete_review = !self.config.agent.discrete_review;
            }
            SettingsTab::Subagents if self.selected == 3 => {
                self.config.subagents.auto_failover = !self.config.subagents.auto_failover;
            }
            SettingsTab::AcpServers => {
                let Some(index) = self.selected.checked_sub(4) else {
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
                if id == "anvil" && policy == AcpServerPolicy::Enabled {
                    crate::anvil::retry_background_install();
                }
                self.inventory = crate::roster::discover_inventory(&self.config);
            }
            _ => return SettingsAction::None,
        }
        self.notice = None;
        SettingsAction::Changed
    }

    fn cycle_model(&mut self, role: usize, delta: i32) {
        let choices = self.model_choices(role);
        let current = match role {
            0 => &self.config.agent.model,
            1 => &self.config.subagents.model,
            _ => return,
        };
        let index = choices
            .iter()
            .position(|choice| choice == current)
            .unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(choices.len() as i32) as usize;
        match role {
            0 => self.config.agent.model.clone_from(&choices[next]),
            1 => self.config.subagents.model.clone_from(&choices[next]),
            _ => {}
        }
        self.refresh_detached_options(role == 0);
    }

    fn cycle_reasoning(&mut self, primary: bool, delta: i32) {
        let values = ["default", "low", "medium", "high"];
        let effort = if primary {
            &mut self.config.agent.reasoning_effort
        } else {
            &mut self.config.subagents.reasoning_effort
        };
        let current = effort.as_deref().unwrap_or("default");
        let index = values
            .iter()
            .position(|value| *value == current)
            .unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(values.len() as i32) as usize;
        *effort = (values[next] != "default").then(|| values[next].to_string());
    }

    fn cycle_session_option(&mut self, primary: bool, delta: i32) {
        let option_index = self.selected.saturating_sub(if primary { 3 } else { 4 });
        let options = if primary {
            &self.primary_session_config_options
        } else {
            &self.subagent_session_config_options
        };
        let Some(option) = options.get(option_index) else {
            // Retained defaults that are no longer advertised still occupy a
            // selectable row, but deliberately have no editable values.
            return;
        };
        let Some(choices) = crate::app::config_option_choices(option) else {
            return;
        };
        if choices.is_empty() {
            return;
        }
        let key = format!("config:{}", option.id);
        let defaults = if primary {
            &mut self.config.agent.session_config
        } else {
            &mut self.config.subagents.session_config
        };
        // An adapter no longer advertising a saved value is not permission to
        // replace it. Keep the value visible and retain it until the adapter
        // advertises a safe replacement again.
        if defaults.get(&key).is_some_and(|saved| {
            !choices
                .iter()
                .any(|choice| choice.value.to_string() == *saved)
        }) {
            return;
        }
        let current = defaults
            .get(&key)
            .cloned()
            .or_else(|| {
                crate::app::config_option_current_value_id(option).map(|value| value.to_string())
            })
            .unwrap_or_default();
        let index = choices
            .iter()
            .position(|choice| choice.value.to_string() == current)
            .unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(choices.len() as i32) as usize;
        defaults.insert(key, choices[next].value.to_string());
        self.notice = None;
    }

    fn cached_options(
        config: &Config,
        primary: bool,
        source_id: Option<&str>,
    ) -> Vec<SessionConfigOption> {
        let cache = if primary {
            &config.agent.session_config_metadata
        } else {
            &config.subagents.session_config_metadata
        };
        let model = if primary {
            &config.agent.model
        } else {
            &config.subagents.model
        };
        source_id
            .and_then(|source_id| cache.get(&format!("{source_id}:{model}")))
            .cloned()
            .unwrap_or_default()
    }

    fn session_option_row_count(&self, primary: bool) -> usize {
        let (options, defaults) = if primary {
            (
                &self.primary_session_config_options,
                &self.config.agent.session_config,
            )
        } else {
            (
                &self.subagent_session_config_options,
                &self.config.subagents.session_config,
            )
        };
        options.len()
            + defaults
                .keys()
                .filter(|key| {
                    !options
                        .iter()
                        .any(|option| format!("config:{}", option.id) == ***key)
                })
                .count()
    }

    fn model_choices(&self, role: usize) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut choices = vec!["auto".to_string()];
        seen.insert("auto".to_string());
        if role != 0 {
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

    fn staged_model_detail(&self, model: &str) -> String {
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
        let adapter = choice.adapter.as_deref().unwrap_or("adapter unknown");
        if choice.ranked {
            format!(
                "{adapter}; Pass@1 {:.1}%; ${:.2}",
                choice.pass_at_1 * 100.0,
                choice.mean_cost_usd
            )
        } else {
            format!("{adapter}; unranked")
        }
    }

    fn active_model_detail(&self, role: usize) -> String {
        let Some(models) = self.active_models.as_ref() else {
            return "not running".to_string();
        };
        let model = match role {
            0 => &models.primary,
            _ => &models.subagent,
        };
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
        let mut refreshed = crate::roster::discover_inventory(&self.config);
        for server in &mut refreshed.servers {
            if let Some(previous) = self
                .inventory
                .servers
                .iter()
                .find(|previous| previous.id == server.id)
            {
                server.model_count = previous.model_count;
                if server.id != "anvil" {
                    server.error.clone_from(&previous.error);
                }
            }
        }
        self.inventory = refreshed;
    }

    pub(crate) fn refresh_after_auth(&mut self, notice: String) {
        self.inventory = crate::roster::discover_inventory(&self.config);
        self.notice = Some(notice);
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
        self.inventory = crate::roster::discover_inventory(&self.config);
        self.acp_view = AcpView::Servers;
        self.selected = self.inventory.servers.len().saturating_sub(1) + 4;
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
        .style(Style::default().fg(theme.text));
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
        SettingsTab::AcpServers => draw_servers(frame, rows[1], editor, theme),
        SettingsTab::Appearance => draw_appearance(frame, rows[1], editor, theme),
    }
    if let Some(notice) = &editor.notice {
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: false }),
            rows[2],
        );
    }
    let footer = match editor.acp_view {
        AcpView::Catalog { .. } if editor.installing.is_some() => "Esc cancel install view",
        AcpView::Catalog { .. } => "Type filter · ↑/↓ select · Enter add · Esc back",
        AcpView::Custom { .. } => "Tab field · Enter add · Esc back",
        AcpView::Servers if editor.tab == SettingsTab::AcpServers && editor.selected < 3 => {
            "Enter sign in · ↑/↓ select · Tab view · Esc cancel"
        }
        AcpView::Servers if editor.tab == SettingsTab::AcpServers && editor.selected == 3 => {
            "Enter add server · ↑/↓ select · Tab view · Esc cancel"
        }
        AcpView::Servers => {
            "Tab view · ↑/↓ select · ←/→ change · Space toggle · Enter save · Esc cancel"
        }
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(theme.muted)),
        rows[3],
    );
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
                .fg(theme.selection_fg)
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
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
    let mut lines = vec![
        Line::styled(
            "The running session keeps its models until /new or /clear reloads the saved selection.",
            Style::default().fg(theme.muted),
        ),
        Line::raw(""),
    ];
    lines.push(selected_line(
        editor.selected == 0,
        format!("Primary model < {} >", editor.config.agent.model),
        theme,
    ));
    lines.push(Line::styled(
        format!(
            "         saved: {} · active: {}",
            editor.staged_model_detail(&editor.config.agent.model),
            editor.active_model_detail(0)
        ),
        Style::default().fg(theme.muted),
    ));
    lines.push(selected_line(
        editor.selected == 1,
        format!(
            "Primary reasoning < {} >",
            editor
                .config
                .agent
                .reasoning_effort
                .as_deref()
                .unwrap_or("default")
        ),
        theme,
    ));
    lines.push(selected_line(
        editor.selected == 2,
        format!(
            "Discrete review [{}]",
            on_off(editor.config.agent.discrete_review)
        ),
        theme,
    ));
    lines.push(Line::styled(
        "ACP options · primary changes are also sent to the active primary.",
        Style::default().fg(theme.muted),
    ));
    append_option_lines(&mut lines, editor, true, theme);
    let selected_line = settings_selected_line(editor, true);
    let scroll = settings_scroll(&lines, selected_line, area.width, area.height);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_subagents(
    frame: &mut ratatui::Frame,
    area: Rect,
    editor: &SettingsEditor,
    theme: TerminalTheme,
) {
    let mut lines = vec![
        Line::styled(
            "Defaults apply only to newly launched workers; they never change running workers.",
            Style::default().fg(theme.muted),
        ),
        Line::raw(""),
        selected_line(
            editor.selected == 0,
            format!("Subagent model < {} >", editor.config.subagents.model),
            theme,
        ),
        Line::styled(
            format!(
                "         saved: {} · active: {}",
                editor.staged_model_detail(&editor.config.subagents.model),
                editor.active_model_detail(1)
            ),
            Style::default().fg(theme.muted),
        ),
        selected_line(
            editor.selected == 1,
            format!(
                "Subagent reasoning < {} >",
                editor
                    .config
                    .subagents
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("default")
            ),
            theme,
        ),
        selected_line(
            editor.selected == 2,
            format!("Max parallel < {} >", editor.config.subagents.max_parallel),
            theme,
        ),
        selected_line(
            editor.selected == 3,
            format!(
                "Automatic quota failover [{}]",
                on_off(editor.config.subagents.auto_failover)
            ),
            theme,
        ),
        Line::styled(
            "ACP options · saved for new workers only.",
            Style::default().fg(theme.muted),
        ),
    ];
    append_option_lines(&mut lines, editor, false, theme);
    let selected_line = settings_selected_line(editor, false);
    let scroll = settings_scroll(&lines, selected_line, area.width, area.height);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn settings_selected_line(editor: &SettingsEditor, primary: bool) -> usize {
    // These offsets are the actual logical lines built above, including the
    // descriptive and retained-stale rows. Visual height is calculated below
    // from those lines and the current panel width.
    if primary {
        match editor.selected {
            0 => 2,
            1 => 4,
            2 => 5,
            selected => 7 + selected.saturating_sub(3),
        }
    } else {
        match editor.selected {
            0 => 2,
            1 => 4,
            2 => 5,
            3 => 6,
            selected => 8 + selected.saturating_sub(4),
        }
    }
}

fn settings_scroll(
    lines: &[Line<'_>],
    selected_line: usize,
    width: u16,
    viewport_height: u16,
) -> u16 {
    // Delegate wrapping to the same Paragraph implementation used for the
    // panel. Counting character widths here is subtly wrong for word breaks,
    // hyphens, and styled spans. Leave one visual row below the selected
    // option, while keeping every visual row of a wrapped selection visible.
    let visual_rows_before_selected = Paragraph::new(lines[..selected_line].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    let selected_visual_rows = Paragraph::new(vec![lines[selected_line].clone()])
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    let viewport_height = usize::from(viewport_height);
    let scroll = visual_rows_before_selected
        .saturating_add(selected_visual_rows)
        .saturating_add(1)
        .saturating_sub(viewport_height);
    scroll.min(u16::MAX as usize) as u16
}

fn append_option_lines(
    lines: &mut Vec<Line<'static>>,
    editor: &SettingsEditor,
    primary: bool,
    theme: TerminalTheme,
) {
    let (options, defaults, start) = if primary {
        (
            &editor.primary_session_config_options,
            &editor.config.agent.session_config,
            3,
        )
    } else {
        (
            &editor.subagent_session_config_options,
            &editor.config.subagents.session_config,
            4,
        )
    };
    for (index, option) in options.iter().enumerate() {
        let key = format!("config:{}", option.id);
        let current = crate::app::config_option_current_value_label(option);
        let saved = defaults
            .get(&key)
            .cloned()
            .unwrap_or_else(|| current.clone());
        let stale = !crate::app::config_option_choices(option)
            .unwrap_or_default()
            .iter()
            .any(|choice| choice.value.to_string() == saved);
        let suffix = if stale { " (stale; retained)" } else { "" };
        lines.push(selected_line(
            editor.selected == start + index,
            format!("{} < {saved} >{suffix}", option.name),
            theme,
        ));
    }
    let stale_start = start + options.len();
    for (stale_index, (key, value)) in defaults
        .iter()
        .filter(|(key, _)| {
            !options
                .iter()
                .any(|option| format!("config:{}", option.id) == **key)
        })
        .enumerate()
    {
        lines.push(selected_line(
            editor.selected == stale_start + stale_index,
            format!("{key} < {value} > (stale; retained; not editable)"),
            theme,
        ));
    }
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
                    Style::default().fg(theme.muted),
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
            .fg(theme.muted)
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
        editor.selected == 3,
        "+ Add server".to_string(),
        theme,
    ));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Servers",
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    ));
    for (index, server) in editor.inventory.servers.iter().enumerate() {
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
        lines.push(selected_line(
            editor.selected == index + 4,
            format!(
                "[{}] {:<16} {status}",
                if server.installing {
                    "installing".to_string()
                } else if server.error.is_some() && server.id == "anvil" {
                    "failed".to_string()
                } else {
                    server.policy.to_string()
                },
                server.label
            ),
            theme,
        ));
        let detail = if server.id == "anvil" {
            server.evidence.clone()
        } else {
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
            Style::default().fg(theme.muted),
        ));
    }
    let selected_line = match editor.selected {
        0..=2 => editor.selected + 1,
        3 => 5,
        selected => 8 + 2 * selected.saturating_sub(4),
    };
    let scroll = settings_scroll(&lines, selected_line, area.width, area.height);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
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
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                Line::raw(""),
                Line::raw(status),
                Line::raw(""),
                Line::styled("Esc cancels this view", Style::default().fg(theme.muted)),
            ]),
            area,
        );
        return;
    }

    let mut lines = vec![
        Line::styled(
            format!("ACP registry · filter: {filter}"),
            Style::default().fg(theme.muted),
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
                Style::default().fg(theme.error),
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
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::styled(command, Style::default().fg(theme.muted)));
                lines.push(Line::raw(""));
            }
            lines.push(selected_line(
                editor.selected == 0,
                "Custom command...".to_string(),
                theme,
            ));
            let selected_catalog_line = if editor.selected == 0 {
                lines.len().saturating_sub(1)
            } else {
                lines.len().saturating_add(editor.selected - 1)
            };
            for (index, agent) in agents.iter().enumerate() {
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
            let scroll = settings_scroll(&lines, selected_catalog_line, area.width, area.height);
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0)),
                area,
            );
            return;
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
            Style::default().fg(theme.muted),
        ),
        Line::raw(""),
        selected_line(
            editor.selected == 0,
            format!("Theme       < {} >", editor.config.theme),
            theme,
        ),
        selected_line(
            editor.selected == 1,
            format!(
                "Spinner     < {} {} >",
                editor.config.spinner,
                editor.config.spinner.current_frame()
            ),
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn selected_line(selected: bool, text: String, theme: TerminalTheme) -> Line<'static> {
    let style = if selected {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
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
    fn tabs_share_one_editable_config() {
        let mut config = Config::default();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Enabled);
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.selected = 2;
        assert_eq!(
            editor.handle_key(KeyCode::Char(' ')),
            SettingsAction::Changed
        );
        assert!(!editor.config.agent.discrete_review);
        editor.handle_key(KeyCode::Tab);
        editor.handle_key(KeyCode::Tab);
        assert_eq!(editor.tab, SettingsTab::AcpServers);
        editor.selected = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "codex-acp")
            .expect("codex")
            + 4;
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
        editor.selected = 3;
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
            editor
                .model_choices(1)
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

    fn option(id: &str) -> SessionConfigOption {
        SessionConfigOption::select(
            id.to_string(),
            format!("Option {id}"),
            "one",
            vec![
                agent_client_protocol::schema::v1::SessionConfigSelectOption::new("one", "One"),
                agent_client_protocol::schema::v1::SessionConfigSelectOption::new("two", "Two"),
            ],
        )
    }

    #[test]
    fn separate_panels_keep_primary_and_subagent_options_isolated() {
        let primary = option("primary-mode");
        let subagent = option("worker-mode");
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None)
            .with_session_config_options(vec![primary])
            .with_subagent_session_config_options(vec![subagent]);

        editor.selected = 3;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.agent.session_config["config:primary-mode"],
            "two"
        );
        assert!(editor.config.subagents.session_config.is_empty());

        editor.handle_key(KeyCode::Tab);
        editor.selected = 4;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.subagents.session_config["config:worker-mode"],
            "two"
        );
        assert!(
            !editor
                .config
                .agent
                .session_config
                .contains_key("config:worker-mode")
        );
    }

    #[test]
    fn more_than_nine_dynamic_options_are_reachable_without_shortcuts() {
        let options = (0..12)
            .map(|index| option(&format!("option-{index}")))
            .collect();
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None)
            .with_session_config_options(options);
        editor.selected = 3 + 11;

        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(
            editor.config.agent.session_config["config:option-11"],
            "two"
        );
    }

    #[test]
    fn cached_metadata_populates_each_role_without_a_live_primary() {
        let mut config = Config::default();
        config
            .agent
            .session_config_metadata
            .insert("adapter-a:auto".to_string(), vec![option("primary")]);
        config
            .subagents
            .session_config_metadata
            .insert("adapter-b:auto".to_string(), vec![option("worker")]);
        let editor = SettingsEditor::new(config, Vec::new(), None)
            .with_primary_session_config_source_id(Some("adapter-a".to_string()))
            .with_subagent_session_config_source_id(Some("adapter-b".to_string()));

        assert_eq!(
            editor.primary_session_config_options[0].id.to_string(),
            "primary"
        );
        assert_eq!(
            editor.subagent_session_config_options[0].id.to_string(),
            "worker"
        );
    }

    #[test]
    fn cached_metadata_never_crosses_adapter_identities_for_the_same_model() {
        let mut config = Config::default();
        config.agent.session_config_metadata.insert(
            "adapter-a:auto".to_string(),
            vec![option("adapter-a-option")],
        );
        config.agent.session_config_metadata.insert(
            "adapter-b:auto".to_string(),
            vec![option("adapter-b-option")],
        );
        let editor = SettingsEditor::new(config, Vec::new(), None)
            .with_primary_session_config_source_id(Some("adapter-b".to_string()));

        assert_eq!(
            editor.primary_session_config_options[0].id.to_string(),
            "adapter-b-option"
        );
    }

    #[test]
    fn detached_subagent_metadata_never_crosses_models_for_one_adapter() {
        let mut config = Config::default();
        config.subagents.model = "model-y".to_string();
        config.subagents.session_config_metadata.insert(
            "adapter-a:model-y".to_string(),
            vec![option("model-y-option")],
        );
        let editor = SettingsEditor::new(config, Vec::new(), None)
            .with_subagent_session_config_options(vec![option("model-x-option")])
            .with_subagent_session_config_source_id(Some("adapter-a".to_string()))
            .with_subagent_session_config_model(Some("model-x".to_string()));

        assert_eq!(
            editor.subagent_session_config_options[0].id.to_string(),
            "model-y-option"
        );
    }

    #[test]
    fn stale_saved_options_remain_visible_and_are_not_editable() {
        let mut config = Config::default();
        config
            .agent
            .session_config
            .insert("config:removed".to_string(), "legacy".to_string());
        config
            .agent
            .session_config
            .insert("config:known".to_string(), "legacy".to_string());
        let mut editor = SettingsEditor::new(config, Vec::new(), None)
            .with_session_config_options(vec![option("known")]);
        let rendered = format!("{:?}", editor.config.agent.session_config);
        assert!(rendered.contains("removed"));
        editor.selected = 3;
        assert_eq!(editor.handle_key(KeyCode::Right), SettingsAction::Changed);
        assert_eq!(editor.config.agent.session_config["config:known"], "legacy");
    }

    #[test]
    fn removed_stale_option_is_selectable_in_a_narrow_panel() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut config = Config::default();
        config
            .agent
            .session_config
            .insert("config:removed".to_string(), "legacy".to_string());
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.selected = 3;
        let backend = TestBackend::new(42, 50);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("removed"), "{rendered}");
    }

    #[test]
    fn narrow_panel_scrolls_to_the_selected_dynamic_option() {
        use ratatui::{Terminal, backend::TestBackend};

        let options = (0..12)
            .map(|index| option(&format!("option-{index}")))
            .collect();
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None)
            .with_session_config_options(options);
        editor.selected = 14;
        let backend = TestBackend::new(42, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Option option-11"), "{rendered}");
    }

    #[test]
    fn down_moves_the_settings_highlight_before_scrolling_the_panel() {
        use ratatui::{Terminal, backend::TestBackend};

        fn selected_row(editor: &SettingsEditor) -> u16 {
            let backend = TestBackend::new(100, 30);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| draw_settings_panel(frame, frame.area(), editor, "mj config"))
                .expect("draw");
            let buffer = terminal.backend().buffer();
            buffer
                .content
                .iter()
                .enumerate()
                .find(|(index, cell)| {
                    *index as u16 % buffer.area.width == 6 && cell.symbol() == ">"
                })
                .map(|(index, _)| index)
                .map(|index| index as u16 / buffer.area.width)
                .expect("selected row")
        }

        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        let first_row = selected_row(&editor);

        assert_eq!(editor.handle_key(KeyCode::Down), SettingsAction::None);
        let second_row = selected_row(&editor);

        assert!(
            second_row > first_row,
            "Down should move the highlight down, not keep it at row {first_row}"
        );
    }

    #[test]
    fn narrow_panel_scrolls_to_selected_retained_stale_option_after_wrapping() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut config = Config::default();
        for index in 0..9 {
            config.subagents.session_config.insert(
                format!("config:very-long-retained-option-{index}-that-wraps"),
                "legacy-value".to_string(),
            );
        }
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::Subagents;
        editor.selected = 12;
        let selected_key = editor
            .config
            .subagents
            .session_config
            .keys()
            .nth(8)
            .expect("ninth stale row")
            .clone();
        let backend = TestBackend::new(42, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        let rendered = terminal.backend().to_string();

        let selected_option = selected_key
            .strip_prefix("config:very-long-retained-")
            .and_then(|key| key.split("-that").next())
            .expect("option label");
        assert!(rendered.contains(selected_option), "{rendered}");
    }

    #[test]
    fn acp_servers_scrolls_selected_server_one_line_above_the_panel_bottom() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut config = Config::default();
        for index in 0..12 {
            config.acp.servers.push(ConfiguredAcpServer {
                id: format!("server-{index}"),
                label: format!("Server {index}"),
                command: PathBuf::from("server"),
                args: Vec::new(),
                env: Default::default(),
                origin: AcpServerOrigin::Custom,
                policy: AcpServerPolicy::Enabled,
            });
        }
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        editor.selected = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "server-6")
            .expect("server")
            + 4;

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let selected_y = buffer
            .content
            .iter()
            .enumerate()
            .find(|(index, cell)| *index as u16 % buffer.area.width == 6 && cell.symbol() == ">")
            .map(|(index, _)| index as u16 / buffer.area.width)
            .expect("selected server marker");

        assert_eq!(
            selected_y, 20,
            "selected server should leave one guard line"
        );
    }

    #[test]
    fn acp_servers_keeps_a_wrapped_selected_server_fully_visible() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut config = Config::default();
        for index in 0..12 {
            let label = if index == 6 {
                "wrapped selection keeps every label word visible".to_string()
            } else {
                format!("Server {index}")
            };
            config.acp.servers.push(ConfiguredAcpServer {
                id: format!("server-{index}"),
                label,
                command: PathBuf::from("server"),
                args: Vec::new(),
                env: Default::default(),
                origin: AcpServerOrigin::Custom,
                policy: AcpServerPolicy::Enabled,
            });
        }
        let mut editor = SettingsEditor::new(config, Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        editor.selected = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "server-6")
            .expect("server")
            + 4;

        let backend = TestBackend::new(42, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        let rendered = terminal.backend().to_string();

        for word in [
            "wrapped",
            "selection",
            "keeps",
            "every",
            "label",
            "word",
            "visible",
        ] {
            assert!(rendered.contains(word), "missing {word}:\n{rendered}");
        }
    }

    #[test]
    fn catalog_scrolls_wrapped_selected_registry_entry_one_line_above_bottom() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut agents = (0..11)
            .map(|index| {
                format!(
                    r#"{{"id":"catalog-{index}","name":"Catalog Server {index:02}","distribution":{{"npx":{{"package":"catalog-{index}"}}}}}}"#
                )
            })
            .collect::<Vec<_>>();
        agents.push(
            r#"{"id":"codebuddy","name":"Codebuddy Code","description":"A deliberately long Codebuddy-like description that wraps across several visual rows and must remain fully readable when selected.","distribution":{"npx":{"package":"codebuddy-code"}}}"#.to_string(),
        );
        let registry = Registry::from_json(&format!(r#"{{"agents":[{}]}}"#, agents.join(",")))
            .expect("registry");
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        editor.acp_view = AcpView::Catalog {
            filter: String::new(),
        };
        editor.registry = RegistryState::Ready(registry);
        let codebuddy_selection = editor
            .filtered_agents()
            .iter()
            .position(|agent| agent.id == "codebuddy")
            .expect("Codebuddy catalog entry")
            + 1;
        for _ in 0..codebuddy_selection {
            editor.handle_key(KeyCode::Down);
        }
        assert_eq!(editor.selected, codebuddy_selection);

        let backend = TestBackend::new(46, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let selected_y = buffer
            .content
            .iter()
            .enumerate()
            .find(|(_, cell)| cell.symbol() == ">")
            .map(|(index, _)| index as u16 / buffer.area.width)
            .expect("selected registry marker");
        let rendered = terminal.backend().to_string();

        assert!(selected_y < 15, "selected marker should be visible");
        for word in ["deliberately", "several", "fully", "readable"] {
            assert!(rendered.contains(word), "missing {word}:\n{rendered}");
        }

        editor.handle_key(KeyCode::Up);
        assert_eq!(editor.selected, codebuddy_selection - 1);
        editor.handle_key(KeyCode::Down);
        assert_eq!(editor.selected, codebuddy_selection);

        let backend = TestBackend::new(46, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "mj config"))
            .expect("draw");
        assert!(
            terminal.backend().to_string().contains("Codebuddy Code"),
            "returning Down after Up should restore the selected catalog entry"
        );
    }

    #[test]
    fn auto_server_can_be_explicitly_enabled() {
        let mut editor = SettingsEditor::new(Config::default(), Vec::new(), None);
        editor.tab = SettingsTab::AcpServers;
        let server_index = editor
            .inventory
            .servers
            .iter()
            .position(|server| server.id == "anvil")
            .expect("anvil");
        editor.selected = server_index + 4;
        editor.inventory.servers[server_index].detected = false;
        editor.inventory.servers[server_index].policy = AcpServerPolicy::Auto;

        // Exercise the transition without refreshing host-specific discovery.
        assert_eq!(editor.toggle_selected(), SettingsAction::Changed);
        assert_eq!(editor.config.acp.policy("anvil"), AcpServerPolicy::Enabled);
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

        editor.selected = 2;
        assert_eq!(
            editor.handle_key(KeyCode::Enter),
            SettingsAction::Authenticate(crate::auth::AuthVendor::Kimi)
        );

        editor.selected = 3;
        assert_eq!(editor.handle_key(KeyCode::Enter), SettingsAction::None);
        assert!(matches!(editor.acp_view, AcpView::Catalog { .. }));
    }
}
