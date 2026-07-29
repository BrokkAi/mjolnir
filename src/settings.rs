//! Shared first-startup and in-session settings editor.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

pub const ROLE_DESCRIPTIONS: [(&str, &str); 2] = [
    ("Agent", "primary model; plans, implements, and answers"),
    ("Subagents", "default model for create_subagent delegations"),
];
const ACCOUNT_COUNT: usize = crate::auth::AuthVendor::ALL.len();
const ADD_SERVER_INDEX: usize = ACCOUNT_COUNT;
pub(crate) const SERVER_ROW_OFFSET: usize = ACCOUNT_COUNT + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Agents,
    AcpPriority,
    AcpServers,
    Appearance,
}

impl SettingsTab {
    const ALL: [Self; 4] = [
        Self::Agents,
        Self::AcpServers,
        Self::AcpPriority,
        Self::Appearance,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
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
enum PrioritySeat {
    Primary,
    Subagents,
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

    pub fn handle_key(&mut self, code: KeyCode) -> SettingsAction {
        self.poll_background();
        if self.priority_editor.is_some() {
            return self.handle_priority_key(code);
        }
        if self.tab == SettingsTab::AcpServers {
            match self.acp_view {
                AcpView::Catalog { .. } => return self.handle_catalog_key(code),
                AcpView::Custom { .. } => return self.handle_custom_key(code),
                AcpView::Servers
                    if code == KeyCode::Char('r')
                        && self
                            .selected
                            .checked_sub(SERVER_ROW_OFFSET)
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
            SettingsTab::Agents => 5,
            SettingsTab::AcpPriority => 2,
            SettingsTab::AcpServers => self.inventory.servers.len() + SERVER_ROW_OFFSET,
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
            SettingsTab::Agents if self.selected < 2 => self.cycle_model(self.selected, delta),
            SettingsTab::Agents if self.selected == 3 => {
                self.config.subagents.max_parallel =
                    (self.config.subagents.max_parallel as i32 + delta).rem_euclid(17) as usize;
            }
            SettingsTab::AcpPriority => return SettingsAction::None,
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
            SettingsTab::Agents if self.selected == 4 => {
                self.config.subagents.auto_failover = !self.config.subagents.auto_failover;
            }
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

    fn priority(&self, seat: PrioritySeat) -> &Vec<String> {
        match seat {
            PrioritySeat::Primary => &self.config.agent.acp_priority,
            PrioritySeat::Subagents => &self.config.subagents.acp_priority,
        }
    }

    fn priority_mut(&mut self, seat: PrioritySeat) -> &mut Vec<String> {
        match seat {
            PrioritySeat::Primary => &mut self.config.agent.acp_priority,
            PrioritySeat::Subagents => &mut self.config.subagents.acp_priority,
        }
    }

    fn effective_priority(&self, seat: PrioritySeat) -> Vec<String> {
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

    fn active_model_detail(&self, role: usize) -> String {
        let Some(models) = self.active_models.as_ref() else {
            return "not running".to_string();
        };
        let (model, source) = match role {
            0 => (&models.primary, models.primary_source.as_deref()),
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
        SettingsTab::AcpPriority => draw_acp_priority(frame, rows[1], editor, theme),
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
    for (index, (role, description)) in ROLE_DESCRIPTIONS.iter().enumerate() {
        let model = match index {
            0 => &editor.config.agent.model,
            _ => &editor.config.subagents.model,
        };
        lines.push(selected_line(
            editor.selected == index,
            format!("{role:<9} < {model} >"),
            theme,
        ));
        lines.push(Line::from(vec![
            Span::raw("            "),
            Span::styled(*description, Style::default().fg(theme.muted)),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "            saved: {} · active: {}",
                editor.staged_model_detail(model),
                editor.active_model_detail(index)
            ),
            Style::default().fg(theme.muted),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(selected_line(
        editor.selected == 2,
        format!(
            "Discrete review [{}]",
            on_off(editor.config.agent.discrete_review)
        ),
        theme,
    ));
    lines.push(selected_line(
        editor.selected == 3,
        format!(
            "Parallel subagents < {} >",
            editor.config.subagents.max_parallel
        ),
        theme,
    ));
    lines.push(selected_line(
        editor.selected == 4,
        format!(
            "Automatic quota failover [{}]",
            on_off(editor.config.subagents.auto_failover)
        ),
        theme,
    ));
    lines.push(Line::from(Span::styled(
        "         quota changes reload with /new or /clear",
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
            PrioritySeat::Subagents => "Subagent ACP priority",
        };
        let mut lines = vec![
            Line::styled(
                format!("{title} · first matching adapter wins"),
                Style::default().fg(theme.muted),
            ),
            Line::styled(
                "r resets to Codex → Claude → Kimi → Anvil",
                Style::default().fg(theme.muted),
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
            "Choose which enabled ACP source supplies a model when several match.",
            Style::default().fg(theme.muted),
        ),
        Line::raw(""),
        selected_line(
            editor.selected == 0,
            format!(
                "Primary   [Enter] {}",
                editor.priority_summary(PrioritySeat::Primary)
            ),
            theme,
        ),
        selected_line(
            editor.selected == 1,
            format!(
                "Subagents [Enter] {}",
                editor.priority_summary(PrioritySeat::Subagents)
            ),
            theme,
        ),
        Line::raw(""),
        Line::styled(
            "ACP Servers controls eligibility; this tab controls preference order.",
            Style::default().fg(theme.muted),
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
        editor.selected == ADD_SERVER_INDEX,
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
        lines.push(selected_line(
            editor.selected == index + SERVER_ROW_OFFSET,
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
    fn role_descriptions_match_product_language() {
        assert_eq!(
            ROLE_DESCRIPTIONS[0].1,
            "primary model; plans, implements, and answers"
        );
        assert_eq!(
            ROLE_DESCRIPTIONS[1].1,
            "default model for create_subagent delegations"
        );
    }

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
        editor.selected = 4;
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
        editor.selected = 1;
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

        editor.selected = 1;
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
            rendered.contains("Primary   [Enter]"),
            "rendered:\n{rendered}"
        );
        assert!(
            rendered.contains("Subagents [Enter]"),
            "rendered:\n{rendered}"
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
        editor.selected = server_index + SERVER_ROW_OFFSET;
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

        editor.selected = 1;
        assert_eq!(
            editor.handle_key(KeyCode::Enter),
            SettingsAction::Authenticate(crate::auth::AuthVendor::Kimi)
        );

        editor.selected = ADD_SERVER_INDEX;
        assert_eq!(editor.handle_key(KeyCode::Enter), SettingsAction::None);
        assert!(matches!(editor.acp_view, AcpView::Catalog { .. }));
    }
}
