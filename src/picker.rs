//! Interactive agent picker.
//!
//! Renders a ratatui screen that lists `anvil` + registry agents +
//! `Custom`, lets the user filter and select one, then resolves the
//! selection into a launch command (downloading a binary archive when
//! needed, with a progress spinner). Used for first-run setup, explicit
//! new-session requests, and agent selection before interactive resume flows.

use std::collections::HashMap;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::term::TrackedBackend;
use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use tokio::sync::mpsc;

use crate::install::{self, Progress};
use crate::palette::TerminalTheme;
use crate::paths::{expand_home_shortcut, normalize_spawn_program};
use crate::registry::{Agent, DistributionKind, Registry};
use crate::version::mjolnir_version_label;

const CURATED_AGENT_IDS: &[&str] = &["claude-acp", "codex-acp", "opencode-acp", "pi-acp"];

/// Resolved launch command for the chosen agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOutcome {
    pub source_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// A user-defined custom agent surfaced as a first-class picker row.
/// Mirrors `config::CustomAgent`, but kept here so the picker module
/// stays decoupled from the on-disk config types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAgent {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub description: String,
}

impl CustomAgent {
    pub fn source_id(&self) -> String {
        format!("custom:{}", self.name)
    }

    fn to_outcome(&self) -> PickerOutcome {
        PickerOutcome {
            source_id: self.source_id(),
            program: self.program.clone(),
            args: self.args.clone(),
            env: HashMap::new(),
        }
    }
}

/// Persistent picker preferences owned by the caller's global config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerPreferences {
    pub default_agent: Option<PickerOutcome>,
    pub favorite_source_ids: Vec<String>,
    pub custom_agents: Vec<CustomAgent>,
}

/// Picker completion state. `outcome` is `None` only when the user cancels.
#[derive(Debug, Clone)]
pub struct PickerResult {
    pub outcome: Option<PickerOutcome>,
    pub preferences: PickerPreferences,
}

/// One row in the picker. `Anvil` and `Custom` are synthetic entries
/// (`Custom` is the "Add custom agent..." row); `Agent` indexes into
/// the registry's agent list and `CustomAgent` indexes into the
/// caller's persisted custom agents.
#[derive(Debug, Clone)]
enum Item {
    Anvil,
    Agent(usize),
    CustomAgent(usize),
    Other,
    Custom,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ItemAction {
    Select,
    SetDefault,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AddCustomFocus {
    Name,
    Command,
}

enum Mode {
    Browse,
    AddCustomAgent {
        name: String,
        command: String,
        focus: AddCustomFocus,
        action: ItemAction,
        error: Option<String>,
        /// `Some(idx)` when editing the existing custom agent at that index
        /// in `preferences.custom_agents`; `None` when adding a new one.
        edit_index: Option<usize>,
    },
    Installing {
        label: String,
        total_bytes: Option<u64>,
        downloaded_bytes: u64,
        extracting: bool,
        action: ItemAction,
        rx: mpsc::UnboundedReceiver<Progress>,
        task: tokio::task::JoinHandle<Result<PickerOutcome>>,
    },
    Error(String),
    Cancelled,
}

struct PickerState<'a> {
    registry: &'a Registry,
    platform: String,
    install_root: PathBuf,
    items: Vec<Item>,
    filter: String,
    filtered: Vec<usize>,
    selected: usize,
    mode: Mode,
    search_focused: bool,
    expanded: bool,
    notice: Option<String>,
    preferences: PickerPreferences,
}

impl<'a> PickerState<'a> {
    fn new(
        registry: &'a Registry,
        platform: String,
        install_root: PathBuf,
        preferences: PickerPreferences,
    ) -> Self {
        let mut state = Self {
            registry,
            platform,
            install_root,
            items: Vec::new(),
            filter: String::new(),
            filtered: Vec::new(),
            selected: 0,
            mode: Mode::Browse,
            search_focused: false,
            expanded: false,
            notice: None,
            preferences,
        };
        let default_source_id = state.default_source_id().map(ToOwned::to_owned);
        state.rebuild_items(default_source_id.as_deref());
        state
    }

    fn rebuild_items(&mut self, preferred_source_id: Option<&str>) {
        let previous_source_id = preferred_source_id
            .map(str::to_string)
            .or_else(|| self.focused_item().map(|item| self.item_source_id(item)));
        let fallback_selected = self.selected;
        let mut items = vec![Item::Anvil];
        let mut indices: Vec<usize> = (0..self.registry.agents.len()).collect();
        indices.sort_by(|&a, &b| {
            self.registry.agents[a]
                .name
                .to_lowercase()
                .cmp(&self.registry.agents[b].name.to_lowercase())
        });
        for i in indices {
            items.push(Item::Agent(i));
        }
        let mut custom_indices: Vec<usize> = (0..self.preferences.custom_agents.len()).collect();
        custom_indices.sort_by(|&a, &b| {
            self.preferences.custom_agents[a]
                .name
                .to_lowercase()
                .cmp(&self.preferences.custom_agents[b].name.to_lowercase())
        });
        for i in custom_indices {
            items.push(Item::CustomAgent(i));
        }
        items.push(Item::Other);
        items.push(Item::Custom);

        items.sort_by(|a, b| {
            let a_fav = self.item_is_favorite(a);
            let b_fav = self.item_is_favorite(b);
            b_fav.cmp(&a_fav).then_with(|| {
                self.item_label(a)
                    .to_lowercase()
                    .cmp(&self.item_label(b).to_lowercase())
            })
        });

        self.items = items;
        self.recompute_filter_preserving(previous_source_id, fallback_selected);
    }

    fn item_label(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "anvil".to_string(),
            Item::Other => "Other...".to_string(),
            Item::Custom => "Add custom agent...".to_string(),
            Item::Agent(idx) => self.registry.agents[*idx].name.clone(),
            Item::CustomAgent(idx) => self.preferences.custom_agents[*idx].name.clone(),
        }
    }

    fn item_search_key(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "anvil brokk acp uvx".to_string(),
            Item::Other => "other agents browse all".to_string(),
            Item::Custom => "add custom agent command".to_string(),
            Item::Agent(idx) => {
                let a = &self.registry.agents[*idx];
                format!("{} {} {}", a.name, a.id, a.description).to_lowercase()
            }
            Item::CustomAgent(idx) => {
                let a = &self.preferences.custom_agents[*idx];
                format!(
                    "{} {} {}",
                    a.name,
                    a.program.to_string_lossy(),
                    a.description
                )
                .to_lowercase()
            }
        }
    }

    fn item_source_id(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "anvil".to_string(),
            Item::Other => "other".to_string(),
            Item::Custom => "custom".to_string(),
            Item::Agent(idx) => self.registry.agents[*idx].id.clone(),
            Item::CustomAgent(idx) => self.preferences.custom_agents[*idx].source_id(),
        }
    }

    fn item_hint(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "uvx brokk acp".to_string(),
            Item::Other => "show all agents".to_string(),
            Item::Custom => "save a named command for next time".to_string(),
            Item::Agent(idx) => {
                let a = &self.registry.agents[*idx];
                match a.preferred_kind(&self.platform) {
                    Some(kind) => format!("{} v{}", kind.label(), a.version),
                    None => "no compatible distribution".to_string(),
                }
            }
            Item::CustomAgent(idx) => {
                let a = &self.preferences.custom_agents[*idx];
                if !a.description.is_empty() {
                    a.description.clone()
                } else {
                    let mut parts = vec![a.program.to_string_lossy().into_owned()];
                    parts.extend(a.args.iter().cloned());
                    parts.join(" ")
                }
            }
        }
    }

    fn default_source_id(&self) -> Option<&str> {
        self.preferences
            .default_agent
            .as_ref()
            .map(|agent| agent.source_id.as_str())
    }

    fn item_is_default(&self, item: &Item) -> bool {
        if matches!(item, Item::Other) {
            return false;
        }
        let Some(cur) = self.default_source_id() else {
            return false;
        };
        self.item_source_id(item) == cur
    }

    fn item_is_favorite(&self, item: &Item) -> bool {
        if matches!(item, Item::Other) {
            return false;
        }
        let source_id = self.item_source_id(item);
        self.preferences
            .favorite_source_ids
            .iter()
            .any(|id| id == &source_id)
    }

    fn item_is_curated(&self, item: &Item) -> bool {
        match item {
            Item::Anvil | Item::Custom => true,
            Item::Agent(idx) => registry_agent_initially_visible(&self.registry.agents[*idx]),
            Item::CustomAgent(_) | Item::Other => false,
        }
    }

    fn item_is_visible_when_collapsed(&self, item: &Item) -> bool {
        matches!(item, Item::Custom)
            || self.item_is_curated(item)
            || self.item_is_favorite(item)
            || self.item_is_default(item)
    }

    fn recompute_filter(&mut self) {
        let prev_selected_source_id = self.focused_item().map(|item| self.item_source_id(item));
        self.recompute_filter_preserving(prev_selected_source_id, 0);
    }

    fn recompute_filter_preserving(
        &mut self,
        prev_selected_source_id: Option<String>,
        fallback_selected: usize,
    ) {
        let q = self.filter.to_lowercase();

        if q.is_empty() {
            if self.expanded {
                self.filtered = self
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| !matches!(item, Item::Other))
                    .map(|(i, _)| i)
                    .collect();
            } else {
                let hidden_count = self
                    .items
                    .iter()
                    .filter(|item| {
                        !matches!(item, Item::Other) && !self.item_is_visible_when_collapsed(item)
                    })
                    .count();
                let other_idx = self
                    .items
                    .iter()
                    .position(|item| matches!(item, Item::Other));
                let custom_idx = self
                    .items
                    .iter()
                    .position(|item| matches!(item, Item::Custom));
                self.filtered = self
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        !matches!(item, Item::Other | Item::Custom)
                            && self.item_is_visible_when_collapsed(item)
                    })
                    .map(|(i, _)| i)
                    .collect();
                if hidden_count > 0
                    && let Some(other_idx) = other_idx
                {
                    self.filtered.push(other_idx);
                }
                if let Some(custom_idx) = custom_idx {
                    self.filtered.push(custom_idx);
                }
            }
        } else {
            self.filtered = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    !matches!(item, Item::Other) && self.item_search_key(item).contains(&q)
                })
                .map(|(i, _)| i)
                .collect();
        }

        // Preserve selection on the same row when possible; otherwise keep the
        // closest stable position chosen by the caller.
        self.selected = prev_selected_source_id
            .as_deref()
            .and_then(|source_id| {
                self.filtered
                    .iter()
                    .position(|&i| self.item_source_id(&self.items[i]) == source_id)
            })
            .unwrap_or_else(|| fallback_selected.min(self.filtered.len().saturating_sub(1)));
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected as i32;
        self.selected = (cur + delta).rem_euclid(len as i32) as usize;
    }

    fn focused_item(&self) -> Option<&Item> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    #[cfg(test)]
    fn select_source_id(&mut self, source_id: &str) {
        if let Some(pos) = self
            .filtered
            .iter()
            .position(|&i| self.item_source_id(&self.items[i]) == source_id)
        {
            self.selected = pos;
        }
    }

    fn toggle_favorite(&mut self, item: &Item) {
        if matches!(item, Item::Other) {
            self.notice = Some("Other expands the full agent list".to_string());
            return;
        }
        let source_id = self.item_source_id(item);
        let label = self.item_label(item);
        if let Some(pos) = self
            .preferences
            .favorite_source_ids
            .iter()
            .position(|id| id == &source_id)
        {
            self.preferences.favorite_source_ids.remove(pos);
            self.notice = Some(format!("removed favorite: {label}"));
        } else {
            self.preferences.favorite_source_ids.push(source_id.clone());
            self.notice = Some(format!("added favorite: {label}"));
        }
        self.rebuild_items(Some(&source_id));
    }

    fn set_default_outcome(&mut self, outcome: PickerOutcome, label: String) {
        let source_id = outcome.source_id.clone();
        self.preferences.default_agent = Some(outcome);
        self.notice = Some(format!("default set to {label}"));
        self.rebuild_items(Some(&source_id));
    }

    /// Replace the custom agent at `idx` with an edited name/program/args,
    /// preserving its description. When the rename changes the agent's
    /// `source_id`, any favorite or default reference is migrated so it keeps
    /// pointing at the same row.
    fn update_custom_agent(
        &mut self,
        idx: usize,
        name: String,
        program: PathBuf,
        args: Vec<String>,
    ) {
        let old_source_id = self.preferences.custom_agents[idx].source_id();
        let description = self.preferences.custom_agents[idx].description.clone();
        let updated = CustomAgent {
            name,
            program,
            args,
            description,
        };
        let new_source_id = updated.source_id();
        let label = updated.name.clone();
        let outcome = updated.to_outcome();
        self.preferences.custom_agents[idx] = updated;

        if old_source_id != new_source_id
            && let Some(pos) = self
                .preferences
                .favorite_source_ids
                .iter()
                .position(|id| id == &old_source_id)
        {
            self.preferences.favorite_source_ids[pos] = new_source_id.clone();
        }
        if self
            .preferences
            .default_agent
            .as_ref()
            .is_some_and(|d| d.source_id == old_source_id)
        {
            self.preferences.default_agent = Some(outcome);
        }

        self.rebuild_items(Some(&new_source_id));
        self.notice = Some(format!("updated custom agent: {label}"));
        self.mode = Mode::Browse;
    }
}

/// Run the picker until the user selects an agent or cancels with Esc.
/// Returns a result with `outcome: None` when the user cancels.
pub async fn run_picker(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    registry: &Registry,
    install_root: &Path,
    platform: &str,
    preferences: PickerPreferences,
    theme: TerminalTheme,
) -> Result<PickerResult> {
    let mut state = PickerState::new(
        registry,
        platform.to_string(),
        install_root.to_path_buf(),
        preferences,
    );

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw(f, &state, theme))?;

    loop {
        tokio::select! {
            biased;
            maybe_ev = events.next() => {
                let Some(ev) = maybe_ev else {
                    return Ok(PickerResult {
                        outcome: None,
                        preferences: state.preferences,
                    });
                };
                let ev = ev.context("crossterm event stream")?;
                if let Some(outcome) = handle_event(&mut state, ev).await? {
                    return Ok(PickerResult {
                        outcome: Some(outcome),
                        preferences: state.preferences,
                    });
                }
            }
            _ = tick.tick() => {
                if let Some(outcome) = pump_install(&mut state).await {
                    return Ok(PickerResult {
                        outcome: Some(outcome),
                        preferences: state.preferences,
                    });
                }
            }
        }
        terminal.draw(|f| draw(f, &state, theme))?;
        if matches!(state.mode, Mode::Cancelled) {
            return Ok(PickerResult {
                outcome: None,
                preferences: state.preferences,
            });
        }
    }
}

async fn pump_install(state: &mut PickerState<'_>) -> Option<PickerOutcome> {
    let Mode::Installing {
        rx,
        task,
        total_bytes,
        downloaded_bytes,
        extracting,
        ..
    } = &mut state.mode
    else {
        return None;
    };

    // Drain any progress events that have arrived.
    while let Ok(p) = rx.try_recv() {
        match p {
            Progress::Started { total_bytes: t } => {
                *total_bytes = t;
                *downloaded_bytes = 0;
            }
            Progress::Downloaded {
                downloaded_bytes: d,
            } => {
                *downloaded_bytes = d;
            }
            Progress::Extracting => {
                *extracting = true;
            }
            Progress::Done => {}
        }
    }

    if task.is_finished() {
        // Move the task out so we can await it.
        let prev = std::mem::replace(&mut state.mode, Mode::Browse);
        if let Mode::Installing {
            task,
            action,
            label,
            ..
        } = prev
        {
            match task.await {
                Ok(Ok(outcome)) => match action {
                    ItemAction::Select => return Some(outcome),
                    ItemAction::SetDefault => state.set_default_outcome(outcome, label),
                },
                Ok(Err(e)) => {
                    state.mode = Mode::Error(format!("install failed: {e:#}"));
                }
                Err(e) => {
                    state.mode = Mode::Error(format!("install task panicked: {e}"));
                }
            }
        }
    }
    None
}

async fn handle_event(state: &mut PickerState<'_>, ev: CtEvent) -> Result<Option<PickerOutcome>> {
    let CtEvent::Key(key) = ev else {
        return Ok(None);
    };
    if key.kind != KeyEventKind::Press {
        return Ok(None);
    }

    match &mut state.mode {
        Mode::Error(_) => match (key.modifiers, key.code) {
            (_, KeyCode::Enter) | (_, KeyCode::Esc) => {
                state.mode = Mode::Browse;
            }
            _ => {}
        },
        Mode::Installing { .. } => {
            // Allow Esc to cancel an in-flight install (best-effort: we
            // abort the task; the partial extract gets left behind, the
            // next attempt re-creates the dir).
            if matches!(key.code, KeyCode::Esc)
                || (key.modifiers == KeyModifiers::CONTROL
                    && matches!(key.code, KeyCode::Char('c')))
            {
                if let Mode::Installing { task, .. } = &state.mode {
                    task.abort();
                }
                state.mode = Mode::Browse;
            }
        }
        Mode::AddCustomAgent {
            name,
            command,
            focus,
            action,
            error,
            edit_index,
        } => match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                state.mode = Mode::Browse;
            }
            (_, KeyCode::Tab) | (KeyModifiers::SHIFT, KeyCode::BackTab) => {
                *focus = match focus {
                    AddCustomFocus::Name => AddCustomFocus::Command,
                    AddCustomFocus::Command => AddCustomFocus::Name,
                };
            }
            (_, KeyCode::Enter) => {
                if matches!(focus, AddCustomFocus::Name) {
                    // Enter on the name field advances to the command field rather
                    // than committing; the user explicitly confirms from Command.
                    *focus = AddCustomFocus::Command;
                } else {
                    let trimmed_name = name.trim().to_string();
                    let trimmed_command = command.trim().to_string();
                    if trimmed_name.is_empty() {
                        *error = Some("name cannot be empty".to_string());
                        *focus = AddCustomFocus::Name;
                    } else if trimmed_command.is_empty() {
                        *error = Some("command cannot be empty".to_string());
                    } else if state
                        .preferences
                        .custom_agents
                        .iter()
                        .enumerate()
                        .any(|(i, a)| Some(i) != *edit_index && a.name == trimmed_name)
                    {
                        *error = Some(format!(
                            "a custom agent named '{trimmed_name}' already exists"
                        ));
                        *focus = AddCustomFocus::Name;
                    } else {
                        match parse_custom_command(&trimmed_command) {
                            Ok(parsed) => {
                                if let Some(idx) = *edit_index {
                                    state.update_custom_agent(
                                        idx,
                                        trimmed_name.clone(),
                                        parsed.program,
                                        parsed.args,
                                    );
                                } else {
                                    let custom = CustomAgent {
                                        name: trimmed_name.clone(),
                                        program: parsed.program,
                                        args: parsed.args,
                                        description: String::new(),
                                    };
                                    let outcome = custom.to_outcome();
                                    let label = custom.name.clone();
                                    state.preferences.custom_agents.push(custom);
                                    let act = *action;
                                    let source_id = outcome.source_id.clone();
                                    state.rebuild_items(Some(&source_id));
                                    state.notice = Some(format!("added custom agent: {label}"));
                                    state.mode = Mode::Browse;
                                    match act {
                                        ItemAction::Select => return Ok(Some(outcome)),
                                        ItemAction::SetDefault => {
                                            state.set_default_outcome(outcome, label);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                *error = Some(format!("{e:#}"));
                                *focus = AddCustomFocus::Command;
                            }
                        }
                    }
                }
            }
            (_, KeyCode::Backspace) => match focus {
                AddCustomFocus::Name => {
                    name.pop();
                }
                AddCustomFocus::Command => {
                    command.pop();
                }
            },
            (_, KeyCode::Char(c)) => match focus {
                AddCustomFocus::Name => name.push(c),
                AddCustomFocus::Command => command.push(c),
            },
            _ => {}
        },
        Mode::Cancelled => {}
        Mode::Browse => match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                state.mode = Mode::Cancelled;
            }
            (_, KeyCode::Esc) => {
                if state.search_focused {
                    state.search_focused = false;
                } else {
                    state.mode = Mode::Cancelled;
                }
            }
            (_, KeyCode::Up) => state.move_selection(-1),
            (_, KeyCode::Down) => state.move_selection(1),
            (_, KeyCode::Enter) => {
                if let Some(item) = state.focused_item().cloned() {
                    return start_item_action(state, &item, ItemAction::Select).await;
                }
            }
            (_, KeyCode::Backspace) if state.search_focused => {
                state.filter.pop();
                state.recompute_filter();
            }
            (KeyModifiers::NONE, KeyCode::Char('/')) if !state.search_focused => {
                state.search_focused = true;
                state.notice = None;
            }
            (KeyModifiers::NONE, KeyCode::Char('f')) if !state.search_focused => {
                if let Some(item) = state.focused_item().cloned() {
                    state.toggle_favorite(&item);
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('d')) if !state.search_focused => {
                if let Some(item) = state.focused_item().cloned() {
                    return start_item_action(state, &item, ItemAction::SetDefault).await;
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('e')) if !state.search_focused => {
                match state.focused_item() {
                    Some(Item::CustomAgent(idx)) => {
                        let idx = *idx;
                        let agent = &state.preferences.custom_agents[idx];
                        state.mode = Mode::AddCustomAgent {
                            name: agent.name.clone(),
                            command: custom_agent_command_string(agent),
                            focus: AddCustomFocus::Name,
                            action: ItemAction::Select,
                            error: None,
                            edit_index: Some(idx),
                        };
                        state.notice = None;
                    }
                    _ => {
                        state.notice = Some("only custom agents can be edited".to_string());
                    }
                }
            }
            (_, KeyCode::Char(c)) if state.search_focused => {
                state.filter.push(c);
                state.recompute_filter();
            }
            _ => {}
        },
    }
    Ok(None)
}

async fn start_item_action(
    state: &mut PickerState<'_>,
    item: &Item,
    action: ItemAction,
) -> Result<Option<PickerOutcome>> {
    match item {
        Item::Anvil => {
            let outcome = PickerOutcome {
                source_id: "anvil".to_string(),
                program: PathBuf::from("uvx"),
                args: vec!["brokk".to_string(), "acp".to_string()],
                env: HashMap::new(),
            };
            match action {
                ItemAction::Select => Ok(Some(outcome)),
                ItemAction::SetDefault => {
                    state.set_default_outcome(outcome, "anvil".to_string());
                    Ok(None)
                }
            }
        }
        Item::Custom => {
            state.mode = Mode::AddCustomAgent {
                name: String::new(),
                command: String::new(),
                focus: AddCustomFocus::Name,
                action,
                error: None,
                edit_index: None,
            };
            Ok(None)
        }
        Item::Other => {
            state.expanded = true;
            state.notice = Some("showing all agents".to_string());
            let previous = state.selected;
            state.recompute_filter();
            state.selected = previous.min(state.filtered.len().saturating_sub(1));
            Ok(None)
        }
        Item::CustomAgent(idx) => {
            let custom = state.preferences.custom_agents[*idx].clone();
            let outcome = custom.to_outcome();
            match action {
                ItemAction::Select => Ok(Some(outcome)),
                ItemAction::SetDefault => {
                    state.set_default_outcome(outcome, custom.name);
                    Ok(None)
                }
            }
        }
        Item::Agent(idx) => {
            let agent = state.registry.agents[*idx].clone();
            let kind = match agent.preferred_kind(&state.platform) {
                Some(k) => k,
                None => {
                    state.mode = Mode::Error(format!(
                        "{} has no distribution for {}",
                        agent.name, state.platform
                    ));
                    return Ok(None);
                }
            };
            match kind {
                DistributionKind::Binary => {
                    let Some(target) = agent
                        .distribution
                        .binary
                        .as_ref()
                        .and_then(|m| m.get(&state.platform))
                        .cloned()
                    else {
                        state.mode =
                            Mode::Error("binary target missing after preferred_kind".to_string());
                        return Ok(None);
                    };
                    let install_root = state.install_root.clone();
                    let agent_id = agent.id.clone();
                    let version = agent.version.clone();
                    let label = agent.name.clone();
                    let (tx, rx) = mpsc::unbounded_channel::<Progress>();
                    let task = tokio::spawn({
                        let tx = tx.clone();
                        async move {
                            let (program, args) = install::install_or_resolve(
                                &agent_id,
                                &version,
                                &target,
                                &install_root,
                                tx,
                            )
                            .await?;
                            Ok(PickerOutcome {
                                source_id: agent_id,
                                program,
                                args,
                                env: HashMap::new(),
                            })
                        }
                    });
                    state.mode = Mode::Installing {
                        label,
                        total_bytes: None,
                        downloaded_bytes: 0,
                        extracting: false,
                        action,
                        rx,
                        task,
                    };
                    Ok(None)
                }
                DistributionKind::Npx => {
                    let pkg = agent.distribution.npx.as_ref().expect("npx checked");
                    let mut args = vec!["-y".to_string(), pkg.package.clone()];
                    args.extend(pkg.args.iter().cloned());
                    let outcome = PickerOutcome {
                        source_id: agent.id.clone(),
                        program: normalize_spawn_program(PathBuf::from("npx")),
                        args,
                        env: pkg.env.clone(),
                    };
                    match action {
                        ItemAction::Select => Ok(Some(outcome)),
                        ItemAction::SetDefault => {
                            state.set_default_outcome(outcome, agent.name);
                            Ok(None)
                        }
                    }
                }
                DistributionKind::Uvx => {
                    let pkg = agent.distribution.uvx.as_ref().expect("uvx checked");
                    let mut args = vec![pkg.package.clone()];
                    args.extend(pkg.args.iter().cloned());
                    let outcome = PickerOutcome {
                        source_id: agent.id.clone(),
                        program: PathBuf::from("uvx"),
                        args,
                        env: pkg.env.clone(),
                    };
                    match action {
                        ItemAction::Select => Ok(Some(outcome)),
                        ItemAction::SetDefault => {
                            state.set_default_outcome(outcome, agent.name);
                            Ok(None)
                        }
                    }
                }
            }
        }
    }
}

fn registry_agent_initially_visible(agent: &Agent) -> bool {
    CURATED_AGENT_IDS.contains(&agent.id.as_str())
}

/// Render a custom agent's stored program + args back into a single editable
/// command string, shell-quoting parts that need it. Inverse of
/// `parse_custom_command` (modulo the home-shortcut/program-normalization that
/// the parse step applies).
fn custom_agent_command_string(agent: &CustomAgent) -> String {
    let mut parts = vec![agent.program.to_string_lossy().into_owned()];
    parts.extend(agent.args.iter().cloned());
    shell_words::join(&parts)
}

fn parse_custom_command(s: &str) -> Result<PickerOutcome> {
    let parts = shell_words::split(s).context("split command")?;
    let mut iter = parts.into_iter();
    let program = iter.next().context("empty command")?;
    Ok(PickerOutcome {
        source_id: "custom".to_string(),
        program: normalize_spawn_program(expand_home_shortcut(&program)),
        args: iter
            .map(|part| expand_home_shortcut(&part).to_string_lossy().into_owned())
            .collect(),
        env: HashMap::new(),
    })
}

fn draw(f: &mut ratatui::Frame, state: &PickerState<'_>, theme: TerminalTheme) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0]);
    draw_list(f, chunks[1], state, theme);
    draw_filter(f, chunks[2], state, theme);
    draw_footer(f, chunks[3], state, theme);

    match &state.mode {
        Mode::AddCustomAgent {
            name,
            command,
            focus,
            error,
            edit_index,
            ..
        } => draw_add_custom_agent_modal(
            f,
            f.area(),
            name,
            command,
            *focus,
            error.as_deref(),
            edit_index.is_some(),
            theme,
        ),
        Mode::Installing {
            label,
            total_bytes,
            downloaded_bytes,
            extracting,
            ..
        } => draw_install_modal(
            f,
            f.area(),
            label,
            *total_bytes,
            *downloaded_bytes,
            *extracting,
            theme,
        ),
        Mode::Error(msg) => draw_error_modal(f, f.area(), msg, theme),
        Mode::Browse | Mode::Cancelled => {}
    }
}

fn draw_header(f: &mut ratatui::Frame, area: Rect) {
    let p = Paragraph::new(format!(" {} | choose an agent ", mjolnir_version_label()))
        .style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(p, area);
}

fn draw_list(f: &mut ratatui::Frame, area: Rect, state: &PickerState<'_>, theme: TerminalTheme) {
    let block = Block::default().borders(Borders::ALL).title(" agents ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.filtered.is_empty() {
        let p = Paragraph::new("no matches").style(Style::default().fg(theme.muted));
        f.render_widget(p, inner);
        return;
    }

    // Partition filtered items into favorites and others.
    let (favorite_slots, other_slots): (Vec<Option<usize>>, Vec<Option<usize>>) = state
        .filtered
        .iter()
        .map(|&i| Some(i))
        .partition(|slot| state.item_is_favorite(&state.items[slot.unwrap()]));
    let has_separator = !favorite_slots.is_empty() && !other_slots.is_empty();

    // Build slots: favorites, optional separator, then others.
    let mut slots = favorite_slots;
    if has_separator {
        slots.push(None);
    }
    slots.extend(other_slots);

    // Map the selected filtered index to its slot position for centering.
    let selected_item = state.filtered.get(state.selected).copied();
    let selected_slot = slots.iter().position(|&s| s == selected_item).unwrap_or(0);

    let visible = inner.height as usize;
    let total = slots.len();
    let start = if total <= visible {
        0
    } else {
        let half = visible / 2;
        selected_slot.saturating_sub(half).min(total - visible)
    };
    let end = (start + visible).min(total);

    let items: Vec<ListItem> = slots[start..end]
        .iter()
        .map(|slot| {
            if slot.is_none() {
                let label = " other ";
                let width = inner.width as usize;
                let sep_line = if width >= label.len() {
                    let extra = width - label.len();
                    let left = extra / 2;
                    let right = extra - left;
                    format!("{}{}{}", "─".repeat(left), label, "─".repeat(right))
                } else {
                    "─ other ─".to_string()
                };
                return ListItem::new(sep_line).style(Style::default().fg(theme.muted));
            }

            let i = slot.unwrap();
            let item = &state.items[i];
            let is_selected = Some(i) == selected_item;

            let marker = if is_selected { ">" } else { " " };
            let mut badges = Vec::new();
            if state.item_is_default(item) {
                badges.push("default");
            }
            if state.item_is_favorite(item) {
                badges.push("favorite");
            }
            let badge = if badges.is_empty() {
                String::new()
            } else {
                format!(" [{}]", badges.join(", "))
            };
            let label = state.item_label(item);
            let hint = state.item_hint(item);
            let line = format!("{marker} {label}{badge}  -- {hint}");
            let style = if is_selected {
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);
}

fn draw_filter(f: &mut ratatui::Frame, area: Rect, state: &PickerState<'_>, theme: TerminalTheme) {
    let title = if state.search_focused {
        " search (typing) "
    } else {
        " search (press /) "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let text = if state.filter.is_empty() && !state.search_focused {
        Line::from(vec![Span::styled(
            "press / to filter agents",
            Style::default().fg(theme.muted),
        )])
    } else {
        Line::from(state.filter.clone())
    };
    let p = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, state: &PickerState<'_>, theme: TerminalTheme) {
    let text = if let Some(notice) = state.notice.as_ref() {
        notice.as_str()
    } else if state.search_focused {
        "typing filters | Up/Down navigate | Enter select | Esc stop search"
    } else {
        "Up/Down navigate | Enter select | / search | f favorite | d default | e edit | Esc cancel"
    };
    let p = Paragraph::new(text).style(Style::default().fg(theme.muted));
    f.render_widget(p, area);
}

#[expect(clippy::too_many_arguments)]
fn draw_add_custom_agent_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    name: &str,
    command: &str,
    focus: AddCustomFocus,
    error: Option<&str>,
    editing: bool,
    theme: TerminalTheme,
) {
    let width = area.width.saturating_sub(8).min(80);
    let height = 11.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let title = if editing {
        " edit custom agent "
    } else {
        " add custom agent "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(theme.primary));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let name_label = if matches!(focus, AddCustomFocus::Name) {
        Span::styled(
            "name (focused)",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("name", Style::default().fg(theme.muted))
    };
    f.render_widget(Paragraph::new(Line::from(vec![name_label])), layout[0]);

    let name_cursor = if matches!(focus, AddCustomFocus::Name) {
        "_"
    } else {
        ""
    };
    let name_body = Paragraph::new(Line::from(vec![
        Span::raw("> "),
        Span::raw(name.to_string()),
        Span::raw(name_cursor),
    ]));
    f.render_widget(name_body, layout[1]);

    let cmd_label = if matches!(focus, AddCustomFocus::Command) {
        Span::styled(
            "command (focused)",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("command", Style::default().fg(theme.muted))
    };
    f.render_widget(Paragraph::new(Line::from(vec![cmd_label])), layout[2]);

    let cmd_cursor = if matches!(focus, AddCustomFocus::Command) {
        "_"
    } else {
        ""
    };
    let cmd_body = Paragraph::new(Line::from(vec![
        Span::raw("> "),
        Span::raw(command.to_string()),
        Span::raw(cmd_cursor),
    ]));
    f.render_widget(cmd_body, layout[3]);

    let help = Paragraph::new("e.g. `/path/to/agent --flag` — saved for next time")
        .style(Style::default().fg(theme.muted))
        .wrap(Wrap { trim: false });
    f.render_widget(help, layout[4]);

    if let Some(err) = error {
        let err_p = Paragraph::new(err)
            .style(Style::default().fg(theme.error))
            .wrap(Wrap { trim: false });
        f.render_widget(err_p, layout[5]);
    }

    let footer = Paragraph::new("Tab switches fields | Enter confirms | Esc cancels")
        .style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[6]);
}

fn draw_install_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
    extracting: bool,
    theme: TerminalTheme,
) {
    let width = area.width.saturating_sub(8).min(70);
    let height = 7.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" installing {label} "))
        .style(Style::default().fg(theme.warning));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let status = if extracting {
        "extracting...".to_string()
    } else if let Some(total) = total_bytes {
        let pct = downloaded_bytes
            .checked_mul(100)
            .and_then(|n| n.checked_div(total))
            .unwrap_or(0);
        format!("downloading: {downloaded_bytes} / {total} bytes ({pct}%)")
    } else if downloaded_bytes > 0 {
        format!("downloading: {} bytes", downloaded_bytes)
    } else {
        "connecting...".to_string()
    };
    let p = Paragraph::new(status).wrap(Wrap { trim: false });
    f.render_widget(p, layout[0]);

    let bar_width = layout[1].width.saturating_sub(2) as usize;
    let bar = if let Some(total) = total_bytes
        && total > 0
        && !extracting
    {
        let filled = ((downloaded_bytes as usize) * bar_width / total as usize).min(bar_width);
        let empty = bar_width.saturating_sub(filled);
        format!("[{}{}]", "#".repeat(filled), " ".repeat(empty))
    } else if extracting {
        format!("[{}]", "=".repeat(bar_width))
    } else {
        format!("[{}]", " ".repeat(bar_width))
    };
    f.render_widget(Paragraph::new(bar), layout[1]);

    let footer = Paragraph::new("Esc to cancel").style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[2]);
}

fn draw_error_modal(f: &mut ratatui::Frame, area: Rect, msg: &str, theme: TerminalTheme) {
    let width = area.width.saturating_sub(8).min(80);
    let height = 7.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" error ")
        .style(Style::default().fg(theme.error));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let body = Paragraph::new(msg).wrap(Wrap { trim: false });
    f.render_widget(body, layout[0]);

    let footer = Paragraph::new("Enter or Esc to dismiss").style(Style::default().fg(theme.muted));
    f.render_widget(footer, layout[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_registry() -> Registry {
        let json = r#"{
            "version": "1.0.0",
            "agents": [
                {
                    "id": "claude-acp",
                    "name": "Claude",
                    "version": "0.36.1",
                    "description": "Claude ACP",
                    "distribution": {
                        "npx": {
                            "package": "@x/claude@0.36.1",
                            "env": {"NO_UPDATE": "1"}
                        }
                    }
                },
                {
                    "id": "binary-only",
                    "name": "BinaryOnly",
                    "version": "1.0.0",
                    "description": "binary distribution only",
                    "distribution": {
                        "binary": {
                            "darwin-aarch64": {
                                "archive": "https://example.com/bin.tar.gz",
                                "cmd": "./bin"
                            }
                        }
                    }
                },
                {
                    "id": "uvx-binary",
                    "name": "UvxBinary",
                    "version": "2.0.0",
                    "description": "uvx and binary distributions",
                    "distribution": {
                        "uvx": {
                            "package": "uvx-binary==2.0.0"
                        },
                        "binary": {
                            "darwin-aarch64": {
                                "archive": "https://example.com/uvx-bin.tar.gz",
                                "cmd": "./uvx-bin"
                            }
                        }
                    }
                }
            ]
        }"#;
        Registry::from_json(json).expect("parse")
    }

    fn visible_labels(state: &PickerState<'_>) -> Vec<String> {
        state
            .filtered
            .iter()
            .map(|&i| state.item_label(&state.items[i]))
            .collect()
    }

    #[test]
    fn picker_lists_anvil_registry_and_custom() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        // 1 anvil + 3 registry + 1 Other + 1 custom-add = 6 items.
        assert_eq!(state.items.len(), 6);
        assert!(state.items.iter().any(|item| matches!(item, Item::Anvil)));
        assert!(state.items.iter().any(|item| matches!(item, Item::Other)));
        assert!(state.items.iter().any(|item| matches!(item, Item::Custom)));
    }

    #[test]
    fn picker_collapses_empty_view_to_curated_agents_and_other() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );

        assert_eq!(
            visible_labels(&state),
            vec!["anvil", "Claude", "Other...", "Add custom agent..."]
        );
    }

    #[test]
    fn picker_collapsed_view_includes_curated_registry_agents() {
        let reg = Registry::from_json(
            r#"{
                "version": "1.0.0",
                "agents": [
                    {
                        "id": "claude-acp",
                        "name": "Claude",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@x/claude"}}
                    },
                    {
                        "id": "codex-acp",
                        "name": "Codex",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@x/codex"}}
                    },
                    {
                        "id": "opencode-acp",
                        "name": "opencode",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@x/opencode"}}
                    },
                    {
                        "id": "pi-acp",
                        "name": "pi",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@x/pi"}}
                    },
                    {
                        "id": "zebra",
                        "name": "Zebra",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@x/zebra"}}
                    }
                ]
            }"#,
        )
        .expect("parse registry");
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );

        assert_eq!(
            visible_labels(&state),
            vec![
                "anvil",
                "Claude",
                "Codex",
                "opencode",
                "pi",
                "Other...",
                "Add custom agent..."
            ]
        );
    }

    #[test]
    fn picker_collapsed_view_uses_stable_ids_not_spoofable_names() {
        let reg = Registry::from_json(
            r#"{
                "version": "1.0.0",
                "agents": [
                    {
                        "id": "evil-acp",
                        "name": "Claude",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@attacker/acp"}}
                    },
                    {
                        "id": "codex-acp",
                        "name": "Renamed Codex",
                        "version": "1.0.0",
                        "distribution": {"npx": {"package": "@x/codex"}}
                    }
                ]
            }"#,
        )
        .expect("parse registry");
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );

        assert_eq!(
            visible_labels(&state),
            vec!["anvil", "Renamed Codex", "Other...", "Add custom agent..."]
        );
    }

    #[test]
    fn picker_collapsed_view_keeps_favorites_visible() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                favorite_source_ids: vec!["uvx-binary".to_string()],
                ..Default::default()
            },
        );

        let labels = visible_labels(&state);
        assert!(labels.contains(&"UvxBinary".to_string()));
        assert_eq!(
            state.item_source_id(state.focused_item().expect("focused")),
            "uvx-binary"
        );
    }

    #[test]
    fn picker_unfavoriting_hidden_agent_keeps_focus_on_visible_row() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                favorite_source_ids: vec!["uvx-binary".to_string()],
                ..Default::default()
            },
        );
        let focused = state.focused_item().expect("focused").clone();

        state.toggle_favorite(&focused);

        assert!(!visible_labels(&state).contains(&"UvxBinary".to_string()));
        assert_eq!(
            state.item_source_id(state.focused_item().expect("focused")),
            "anvil"
        );
    }

    #[test]
    fn picker_collapsed_view_keeps_saved_default_visible_and_selected() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                default_agent: Some(PickerOutcome {
                    source_id: "binary-only".to_string(),
                    program: PathBuf::from("/tmp/bin"),
                    args: Vec::new(),
                    env: HashMap::new(),
                }),
                ..Default::default()
            },
        );

        assert!(visible_labels(&state).contains(&"BinaryOnly".to_string()));
        assert_eq!(
            state.item_source_id(state.focused_item().expect("focused")),
            "binary-only"
        );
    }

    #[test]
    fn picker_other_row_is_never_default() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                default_agent: Some(PickerOutcome {
                    source_id: "other".to_string(),
                    program: PathBuf::from("/tmp/other"),
                    args: Vec::new(),
                    env: HashMap::new(),
                }),
                ..Default::default()
            },
        );
        let other = state
            .items
            .iter()
            .find(|item| matches!(item, Item::Other))
            .expect("other row");

        assert!(!state.item_is_default(other));
    }

    #[test]
    fn picker_sorts_registry_agents_alphabetically() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        let registry_labels: Vec<String> = state.items[1..state.items.len() - 1]
            .iter()
            .map(|i| state.item_label(i))
            .collect();
        let mut sorted = registry_labels.clone();
        sorted.sort_by_key(|s| s.to_lowercase());
        assert_eq!(registry_labels, sorted);
    }

    #[test]
    fn picker_filter_matches_by_name() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        state.filter = "binary-only".to_string();
        state.recompute_filter();
        assert_eq!(visible_labels(&state), vec!["BinaryOnly".to_string()]);
    }

    #[test]
    fn picker_search_matches_hidden_custom_agents() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        state.filter = "experiment".to_string();
        state.recompute_filter();

        assert_eq!(visible_labels(&state), vec!["experiment".to_string()]);
    }

    #[tokio::test]
    async fn selecting_other_expands_full_list_in_place() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        let other = state
            .items
            .iter()
            .find(|item| matches!(item, Item::Other))
            .expect("other row")
            .clone();

        let result = start_item_action(&mut state, &other, ItemAction::Select)
            .await
            .expect("expand");

        assert!(result.is_none());
        assert!(state.expanded);
        let labels = visible_labels(&state);
        assert!(labels.contains(&"BinaryOnly".to_string()));
        assert!(labels.contains(&"UvxBinary".to_string()));
        assert!(!labels.contains(&"Other...".to_string()));
    }

    #[test]
    fn picker_marks_default_selection() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                default_agent: Some(PickerOutcome {
                    source_id: "anvil".to_string(),
                    program: PathBuf::from("uvx"),
                    args: vec!["brokk".to_string(), "acp".to_string()],
                    env: HashMap::new(),
                }),
                ..Default::default()
            },
        );
        assert!(state.item_is_default(&Item::Anvil));
        assert!(!state.item_is_default(&Item::Custom));
    }

    #[test]
    fn picker_anvil_entry_uses_brokk_uvx_command() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );

        assert_eq!(state.item_hint(&Item::Anvil), "uvx brokk acp");
    }

    #[tokio::test]
    async fn selecting_anvil_returns_brokk_uvx_command() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );

        let outcome = start_item_action(&mut state, &Item::Anvil, ItemAction::Select)
            .await
            .expect("select")
            .expect("outcome");

        assert_eq!(outcome.source_id, "anvil");
        assert_eq!(outcome.program, PathBuf::from("uvx"));
        assert_eq!(outcome.args, vec!["brokk", "acp"]);
    }

    #[tokio::test]
    async fn selecting_uvx_agent_prefers_uvx_over_binary() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        let item = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "uvx-binary")
            .expect("uvx-binary")
            .clone();

        let outcome = start_item_action(&mut state, &item, ItemAction::Select)
            .await
            .expect("select")
            .expect("outcome");

        assert_eq!(outcome.source_id, "uvx-binary");
        assert_eq!(outcome.program, PathBuf::from("uvx"));
        assert_eq!(outcome.args, vec!["uvx-binary==2.0.0"]);
    }

    #[tokio::test]
    async fn selecting_npx_agent_uses_spawnable_program() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        let item = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "claude-acp")
            .expect("claude")
            .clone();

        let outcome = start_item_action(&mut state, &item, ItemAction::Select)
            .await
            .expect("select")
            .expect("outcome");

        assert_eq!(outcome.source_id, "claude-acp");
        if cfg!(windows) {
            assert_eq!(outcome.program, PathBuf::from("npx.cmd"));
        } else {
            assert_eq!(outcome.program, PathBuf::from("npx"));
        }
        assert_eq!(outcome.args, vec!["-y", "@x/claude@0.36.1"]);
    }

    #[test]
    fn picker_initial_selection_uses_default_agent() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                default_agent: Some(PickerOutcome {
                    source_id: "claude-acp".to_string(),
                    program: PathBuf::from("npx"),
                    args: vec!["-y".to_string(), "@x/claude@0.36.1".to_string()],
                    env: HashMap::new(),
                }),
                ..Default::default()
            },
        );

        let focused = state.focused_item().expect("focused");
        assert_eq!(state.item_source_id(focused), "claude-acp");
    }

    #[test]
    fn picker_pins_favorites_first() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                favorite_source_ids: vec!["claude-acp".to_string()],
                ..Default::default()
            },
        );

        assert_eq!(state.item_source_id(&state.items[0]), "claude-acp");
        assert!(state.item_is_favorite(&state.items[0]));
    }

    #[test]
    fn picker_toggle_favorite_updates_preferences_and_order() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        let claude = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "claude-acp")
            .expect("claude")
            .clone();

        state.toggle_favorite(&claude);
        assert_eq!(
            state.preferences.favorite_source_ids,
            vec!["claude-acp".to_string()]
        );
        assert_eq!(state.item_source_id(&state.items[0]), "claude-acp");

        state.toggle_favorite(&claude);
        assert!(state.preferences.favorite_source_ids.is_empty());
        // After un-favoriting, items are sorted alphabetically by label
        // (case-insensitive). The "Add custom agent..." row sorts ahead of
        // "anvil" by that ordering.
        let labels: Vec<String> = state
            .items
            .iter()
            .map(|item| state.item_label(item).to_lowercase())
            .collect();
        let mut sorted = labels.clone();
        sorted.sort();
        assert_eq!(labels, sorted);
    }

    #[test]
    fn picker_hint_describes_distribution_choice() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        // Find Claude (npx-only), UvxBinary, and BinaryOnly entries.
        let labels_and_hints: Vec<(String, String)> = state
            .items
            .iter()
            .map(|i| (state.item_label(i), state.item_hint(i)))
            .collect();
        let claude = labels_and_hints
            .iter()
            .find(|(l, _)| l == "Claude")
            .expect("claude");
        assert!(claude.1.starts_with("npx"), "hint: {}", claude.1);
        let bin = labels_and_hints
            .iter()
            .find(|(l, _)| l == "BinaryOnly")
            .expect("binonly");
        assert!(bin.1.starts_with("binary"), "hint: {}", bin.1);
        let uvx = labels_and_hints
            .iter()
            .find(|(l, _)| l == "UvxBinary")
            .expect("uvx");
        assert!(uvx.1.starts_with("uvx"), "hint: {}", uvx.1);
    }

    #[test]
    fn picker_hint_warns_on_incompatible_binary_only() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "windows-x86_64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        let bin_hint = state
            .items
            .iter()
            .find_map(|i| {
                if state.item_label(i) == "BinaryOnly" {
                    Some(state.item_hint(i))
                } else {
                    None
                }
            })
            .expect("bin hint");
        assert!(bin_hint.contains("no compatible"), "hint: {bin_hint}");
    }

    #[test]
    fn parse_custom_command_splits_with_shell_words() {
        let outcome =
            parse_custom_command("/usr/local/bin/agent --flag \"with space\"").expect("parse");
        assert_eq!(outcome.source_id, "custom");
        assert_eq!(outcome.program, PathBuf::from("/usr/local/bin/agent"));
        assert_eq!(outcome.args, vec!["--flag", "with space"]);
    }

    #[test]
    fn parse_custom_command_expands_home_shortcuts_in_program_and_args() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let outcome =
            parse_custom_command("~/bin/agent --config $HOME/.config/agent.toml").expect("parse");
        assert_eq!(outcome.program, home.join("bin/agent"));
        assert_eq!(
            outcome.args,
            vec![
                "--config".to_string(),
                home.join(".config/agent.toml").display().to_string()
            ]
        );
    }

    #[test]
    fn parse_custom_command_leaves_non_supported_home_syntax_literal() {
        let outcome = parse_custom_command("agent ${HOME}/config.toml").expect("parse");
        assert_eq!(outcome.program, PathBuf::from("agent"));
        assert_eq!(outcome.args, vec!["${HOME}/config.toml"]);
    }

    #[test]
    fn parse_custom_command_normalizes_npx_for_spawn() {
        let outcome = parse_custom_command("npx -y @example/agent").expect("parse");
        if cfg!(windows) {
            assert_eq!(outcome.program, PathBuf::from("npx.cmd"));
        } else {
            assert_eq!(outcome.program, PathBuf::from("npx"));
        }
        assert_eq!(outcome.args, vec!["-y", "@example/agent"]);
    }

    #[test]
    fn parse_custom_command_rejects_empty() {
        let err = parse_custom_command("   ").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("empty"), "msg: {msg}");
    }

    #[tokio::test]
    async fn backspace_does_not_mutate_filter_when_search_unfocused() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        state.filter.push_str("hidden");
        state.recompute_filter();
        assert!(!state.search_focused);

        let filtered_before = state.filtered.clone();
        let key = crossterm::event::KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let _ = handle_event(&mut state, CtEvent::Key(key)).await.unwrap();

        assert_eq!(
            state.filter, "hidden",
            "backspace must not mutate filter while unfocused"
        );
        assert_eq!(state.filtered, filtered_before);
    }

    #[tokio::test]
    async fn backspace_mutates_filter_when_search_focused() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        state.filter.push_str("hi");
        state.search_focused = true;
        state.recompute_filter();

        let key = crossterm::event::KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let _ = handle_event(&mut state, CtEvent::Key(key)).await.unwrap();

        assert_eq!(
            state.filter, "h",
            "backspace must pop one char while search is focused"
        );
    }

    fn fixture_custom_agents() -> Vec<CustomAgent> {
        vec![
            CustomAgent {
                name: "local-claude".to_string(),
                program: PathBuf::from("/usr/local/bin/claude-acp"),
                args: vec!["--debug".to_string()],
                description: "claude with debug logging".to_string(),
            },
            CustomAgent {
                name: "experiment".to_string(),
                program: PathBuf::from("/tmp/agent"),
                args: vec![],
                description: String::new(),
            },
        ]
    }

    #[test]
    fn picker_lists_persisted_custom_agents_as_rows() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        // 1 anvil + 3 registry + 2 custom-agent + 1 Other + 1 "Add custom" = 8.
        assert_eq!(state.items.len(), 8);
        let sources: Vec<String> = state
            .items
            .iter()
            .map(|i| state.item_source_id(i))
            .collect();
        assert!(sources.contains(&"custom:local-claude".to_string()));
        assert!(sources.contains(&"custom:experiment".to_string()));
        assert!(sources.contains(&"custom".to_string()));
    }

    #[tokio::test]
    async fn selecting_persisted_custom_agent_returns_its_command() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        let item = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "custom:local-claude")
            .expect("custom row")
            .clone();

        let outcome = start_item_action(&mut state, &item, ItemAction::Select)
            .await
            .expect("select")
            .expect("outcome");

        assert_eq!(outcome.source_id, "custom:local-claude");
        assert_eq!(outcome.program, PathBuf::from("/usr/local/bin/claude-acp"));
        assert_eq!(outcome.args, vec!["--debug"]);
    }

    #[tokio::test]
    async fn setting_custom_agent_as_default_records_it_in_preferences() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        let item = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "custom:experiment")
            .expect("custom row")
            .clone();

        let result = start_item_action(&mut state, &item, ItemAction::SetDefault)
            .await
            .expect("set default");
        assert!(result.is_none(), "set-default does not return an outcome");

        let default = state
            .preferences
            .default_agent
            .as_ref()
            .expect("default set");
        assert_eq!(default.source_id, "custom:experiment");
        assert_eq!(default.program, PathBuf::from("/tmp/agent"));
    }

    #[test]
    fn picker_custom_agent_hint_prefers_description_then_command() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        let local_claude = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "custom:local-claude")
            .expect("local-claude");
        assert_eq!(state.item_hint(local_claude), "claude with debug logging");

        let experiment = state
            .items
            .iter()
            .find(|item| state.item_source_id(item) == "custom:experiment")
            .expect("experiment");
        assert_eq!(state.item_hint(experiment), "/tmp/agent");
    }

    #[tokio::test]
    async fn add_custom_agent_flow_persists_and_returns_outcome_on_select() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );

        // Trigger the "Add custom agent..." row -> opens the modal.
        let custom_item = Item::Custom;
        let res = start_item_action(&mut state, &custom_item, ItemAction::Select)
            .await
            .expect("trigger");
        assert!(res.is_none());
        assert!(matches!(state.mode, Mode::AddCustomAgent { .. }));

        // Type a name + command and confirm.
        for c in "my-agent".chars() {
            let key = crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            handle_event(&mut state, CtEvent::Key(key)).await.unwrap();
        }
        // Tab to command field.
        let tab = crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        handle_event(&mut state, CtEvent::Key(tab)).await.unwrap();
        for c in "/usr/local/bin/agent --flag".chars() {
            let key = crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            handle_event(&mut state, CtEvent::Key(key)).await.unwrap();
        }
        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = handle_event(&mut state, CtEvent::Key(enter))
            .await
            .unwrap()
            .expect("outcome on confirm");

        assert_eq!(outcome.source_id, "custom:my-agent");
        assert_eq!(outcome.program, PathBuf::from("/usr/local/bin/agent"));
        assert_eq!(outcome.args, vec!["--flag"]);
        assert_eq!(state.preferences.custom_agents.len(), 1);
        assert_eq!(state.preferences.custom_agents[0].name, "my-agent");
    }

    #[tokio::test]
    async fn add_custom_agent_rejects_duplicate_name() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        state.mode = Mode::AddCustomAgent {
            name: "local-claude".to_string(),
            command: "/bin/agent".to_string(),
            focus: AddCustomFocus::Command,
            action: ItemAction::Select,
            error: None,
            edit_index: None,
        };

        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_event(&mut state, CtEvent::Key(enter)).await.unwrap();
        assert!(result.is_none());
        let Mode::AddCustomAgent { error, .. } = &state.mode else {
            panic!("expected AddCustomAgent mode, got something else");
        };
        let err = error.as_ref().expect("error set");
        assert!(err.contains("already exists"), "err: {err}");
        assert_eq!(
            state.preferences.custom_agents.len(),
            2,
            "no new agent added"
        );
    }

    #[tokio::test]
    async fn add_custom_agent_rejects_empty_name() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        state.mode = Mode::AddCustomAgent {
            name: "  ".to_string(),
            command: "/bin/agent".to_string(),
            focus: AddCustomFocus::Command,
            action: ItemAction::Select,
            error: None,
            edit_index: None,
        };

        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_event(&mut state, CtEvent::Key(enter)).await.unwrap();
        assert!(result.is_none());
        let Mode::AddCustomAgent { error, focus, .. } = &state.mode else {
            panic!("expected AddCustomAgent mode");
        };
        assert!(error.as_ref().unwrap().contains("name"));
        assert!(matches!(focus, AddCustomFocus::Name));
    }

    #[tokio::test]
    async fn pressing_e_on_custom_agent_opens_prefilled_edit_modal() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        state.expanded = true;
        state.recompute_filter();
        state.select_source_id("custom:local-claude");

        let key = crossterm::event::KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        handle_event(&mut state, CtEvent::Key(key)).await.unwrap();

        let Mode::AddCustomAgent {
            name,
            command,
            edit_index,
            ..
        } = &state.mode
        else {
            panic!("expected edit modal");
        };
        assert_eq!(name, "local-claude");
        assert_eq!(command, "/usr/local/bin/claude-acp --debug");
        let idx = edit_index.expect("editing an existing agent");
        assert_eq!(state.preferences.custom_agents[idx].name, "local-claude");
    }

    #[tokio::test]
    async fn editing_custom_agent_updates_in_place_without_adding() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        // Edit the second agent's command, keeping its name.
        state.mode = Mode::AddCustomAgent {
            name: "experiment".to_string(),
            command: "/tmp/agent --verbose".to_string(),
            focus: AddCustomFocus::Command,
            action: ItemAction::Select,
            error: None,
            edit_index: Some(1),
        };

        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let result = handle_event(&mut state, CtEvent::Key(enter)).await.unwrap();
        assert!(result.is_none(), "editing returns to browse, not a launch");
        assert!(matches!(state.mode, Mode::Browse));

        assert_eq!(state.preferences.custom_agents.len(), 2, "no new agent");
        let edited = &state.preferences.custom_agents[1];
        assert_eq!(edited.name, "experiment");
        assert_eq!(edited.program, PathBuf::from("/tmp/agent"));
        assert_eq!(edited.args, vec!["--verbose"]);
    }

    #[tokio::test]
    async fn editing_keeps_same_name_without_duplicate_error() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        state.mode = Mode::AddCustomAgent {
            name: "local-claude".to_string(),
            command: "/usr/local/bin/claude-acp --debug --extra".to_string(),
            focus: AddCustomFocus::Command,
            action: ItemAction::Select,
            error: None,
            edit_index: Some(0),
        };

        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_event(&mut state, CtEvent::Key(enter)).await.unwrap();

        assert!(
            matches!(state.mode, Mode::Browse),
            "keeping the same name must not be treated as a duplicate"
        );
        assert_eq!(
            state.preferences.custom_agents[0].args,
            vec!["--debug", "--extra"]
        );
    }

    #[tokio::test]
    async fn editing_rejects_name_colliding_with_other_agent() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                ..Default::default()
            },
        );
        // Try to rename agent 1 ("experiment") to "local-claude" (agent 0).
        state.mode = Mode::AddCustomAgent {
            name: "local-claude".to_string(),
            command: "/tmp/agent".to_string(),
            focus: AddCustomFocus::Command,
            action: ItemAction::Select,
            error: None,
            edit_index: Some(1),
        };

        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_event(&mut state, CtEvent::Key(enter)).await.unwrap();

        let Mode::AddCustomAgent { error, .. } = &state.mode else {
            panic!("expected modal to stay open on collision");
        };
        assert!(error.as_ref().unwrap().contains("already exists"));
        // Original name preserved.
        assert_eq!(state.preferences.custom_agents[1].name, "experiment");
    }

    #[tokio::test]
    async fn renaming_custom_agent_migrates_favorite_and_default() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences {
                custom_agents: fixture_custom_agents(),
                favorite_source_ids: vec!["custom:experiment".to_string()],
                default_agent: Some(PickerOutcome {
                    source_id: "custom:experiment".to_string(),
                    program: PathBuf::from("/tmp/agent"),
                    args: vec![],
                    env: HashMap::new(),
                }),
            },
        );
        state.mode = Mode::AddCustomAgent {
            name: "renamed".to_string(),
            command: "/tmp/agent --new".to_string(),
            focus: AddCustomFocus::Command,
            action: ItemAction::Select,
            error: None,
            edit_index: Some(1),
        };

        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_event(&mut state, CtEvent::Key(enter)).await.unwrap();

        assert_eq!(
            state.preferences.favorite_source_ids,
            vec!["custom:renamed".to_string()],
            "favorite reference follows the rename"
        );
        let default = state.preferences.default_agent.as_ref().expect("default");
        assert_eq!(default.source_id, "custom:renamed");
        assert_eq!(default.args, vec!["--new"], "default command also updated");
    }

    #[tokio::test]
    async fn pressing_e_on_non_custom_row_sets_notice() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        state.select_source_id("anvil");

        let key = crossterm::event::KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        handle_event(&mut state, CtEvent::Key(key)).await.unwrap();

        assert!(matches!(state.mode, Mode::Browse));
        assert_eq!(
            state.notice.as_deref(),
            Some("only custom agents can be edited")
        );
    }

    #[test]
    fn picker_move_selection_wraps() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, state.filtered.len() - 1);
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }
}
