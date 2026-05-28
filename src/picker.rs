//! Interactive agent picker.
//!
//! Renders a ratatui screen that lists `anvil` + registry agents +
//! `Custom`, lets the user filter and select one, then resolves the
//! selection into a launch command (downloading a binary archive when
//! needed, with a progress spinner). Used whenever a new session starts.

use std::collections::HashMap;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use tokio::sync::mpsc;

use crate::install::{self, Progress};
use crate::registry::{DistributionKind, Registry};
use crate::version::mjolnir_version_label;

/// Resolved launch command for the chosen agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerOutcome {
    pub source_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Persistent picker preferences owned by the caller's global config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerPreferences {
    pub default_agent: Option<PickerOutcome>,
    pub favorite_source_ids: Vec<String>,
}

/// Picker completion state. `outcome` is `None` only when the user cancels.
#[derive(Debug, Clone)]
pub struct PickerResult {
    pub outcome: Option<PickerOutcome>,
    pub preferences: PickerPreferences,
}

/// One row in the picker. `Anvil` and `Custom` are synthetic entries;
/// `Agent` indexes into the registry's agent list.
#[derive(Debug, Clone)]
enum Item {
    Anvil,
    Agent(usize),
    Custom,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ItemAction {
    Select,
    SetDefault,
}

enum Mode {
    Browse,
    CustomInput {
        input: String,
        action: ItemAction,
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
            notice: None,
            preferences,
        };
        let default_source_id = state.default_source_id().map(ToOwned::to_owned);
        state.rebuild_items(default_source_id.as_deref());
        state
    }

    fn rebuild_items(&mut self, preferred_source_id: Option<&str>) {
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
        self.recompute_filter();
        if let Some(source_id) = preferred_source_id {
            self.select_source_id(source_id);
        }
    }

    fn item_label(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "anvil".to_string(),
            Item::Custom => "Custom command...".to_string(),
            Item::Agent(idx) => self.registry.agents[*idx].name.clone(),
        }
    }

    fn item_search_key(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "anvil brokk acp uvx".to_string(),
            Item::Custom => "custom command".to_string(),
            Item::Agent(idx) => {
                let a = &self.registry.agents[*idx];
                format!("{} {} {}", a.name, a.id, a.description).to_lowercase()
            }
        }
    }

    fn item_source_id(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "anvil".to_string(),
            Item::Custom => "custom".to_string(),
            Item::Agent(idx) => self.registry.agents[*idx].id.clone(),
        }
    }

    fn item_hint(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "uvx brokk acp".to_string(),
            Item::Custom => "type your own command".to_string(),
            Item::Agent(idx) => {
                let a = &self.registry.agents[*idx];
                match a.preferred_kind(&self.platform) {
                    Some(kind) => format!("{} v{}", kind.label(), a.version),
                    None => "no compatible distribution".to_string(),
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
        let Some(cur) = self.default_source_id() else {
            return false;
        };
        self.item_source_id(item) == cur
    }

    fn item_is_favorite(&self, item: &Item) -> bool {
        let source_id = self.item_source_id(item);
        self.preferences
            .favorite_source_ids
            .iter()
            .any(|id| id == &source_id)
    }

    fn recompute_filter(&mut self) {
        let q = self.filter.to_lowercase();
        let prev_selected_source_id = self
            .filtered
            .get(self.selected)
            .map(|&i| self.item_source_id(&self.items[i]));

        if q.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            self.filtered = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| self.item_search_key(item).contains(&q))
                .map(|(i, _)| i)
                .collect();
        }

        // Preserve selection on the same row when possible; otherwise top.
        self.selected = prev_selected_source_id
            .and_then(|source_id| {
                self.filtered
                    .iter()
                    .position(|&i| self.item_source_id(&self.items[i]) == source_id)
            })
            .unwrap_or(0);
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
}

/// Run the picker until the user selects an agent or cancels with Esc.
/// Returns a result with `outcome: None` when the user cancels.
pub async fn run_picker(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    registry: &Registry,
    install_root: &Path,
    platform: &str,
    preferences: PickerPreferences,
) -> Result<PickerResult> {
    let mut state = PickerState::new(
        registry,
        platform.to_string(),
        install_root.to_path_buf(),
        preferences,
    );

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw(f, &state))?;

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
        terminal.draw(|f| draw(f, &state))?;
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
        Mode::CustomInput { input, action } => match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                state.mode = Mode::Browse;
            }
            (_, KeyCode::Enter) => {
                let raw = input.trim().to_string();
                if raw.is_empty() {
                    state.mode = Mode::Error("custom command cannot be empty".to_string());
                } else {
                    match parse_custom_command(&raw) {
                        Ok(outcome) => match *action {
                            ItemAction::Select => return Ok(Some(outcome)),
                            ItemAction::SetDefault => {
                                state.set_default_outcome(outcome, "custom command".to_string());
                            }
                        },
                        Err(e) => state.mode = Mode::Error(format!("{e:#}")),
                    }
                }
            }
            (_, KeyCode::Backspace) => {
                input.pop();
            }
            (_, KeyCode::Char(c)) => {
                input.push(c);
            }
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
            if matches!(action, ItemAction::Select)
                && let Some(default_agent) = state.preferences.default_agent.as_ref()
                && default_agent.source_id == "custom"
            {
                return Ok(Some(default_agent.clone()));
            }
            state.mode = Mode::CustomInput {
                input: String::new(),
                action,
            };
            Ok(None)
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
                        program: PathBuf::from("npx"),
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

fn parse_custom_command(s: &str) -> Result<PickerOutcome> {
    let parts = shell_words::split(s).context("split command")?;
    let mut iter = parts.into_iter();
    let program = iter.next().context("empty command")?;
    Ok(PickerOutcome {
        source_id: "custom".to_string(),
        program: PathBuf::from(program),
        args: iter.collect(),
        env: HashMap::new(),
    })
}

fn draw(f: &mut ratatui::Frame, state: &PickerState<'_>) {
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
    draw_list(f, chunks[1], state);
    draw_filter(f, chunks[2], state);
    draw_footer(f, chunks[3], state);

    match &state.mode {
        Mode::CustomInput { input, .. } => draw_custom_input_modal(f, f.area(), input),
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
        ),
        Mode::Error(msg) => draw_error_modal(f, f.area(), msg),
        Mode::Browse | Mode::Cancelled => {}
    }
}

fn draw_header(f: &mut ratatui::Frame, area: Rect) {
    let p = Paragraph::new(format!(" {} | choose an agent ", mjolnir_version_label()))
        .style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(p, area);
}

fn draw_list(f: &mut ratatui::Frame, area: Rect, state: &PickerState<'_>) {
    let block = Block::default().borders(Borders::ALL).title(" agents ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if state.filtered.is_empty() {
        let p = Paragraph::new("no matches").style(Style::default().fg(Color::DarkGray));
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
                return ListItem::new(sep_line).style(Style::default().fg(Color::DarkGray));
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
                    .fg(Color::Black)
                    .bg(Color::Cyan)
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

fn draw_filter(f: &mut ratatui::Frame, area: Rect, state: &PickerState<'_>) {
    let title = if state.search_focused {
        " search (typing) "
    } else {
        " search (press /) "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let text = if state.filter.is_empty() && !state.search_focused {
        Line::from(vec![Span::styled(
            "press / to filter agents",
            Style::default().fg(Color::DarkGray),
        )])
    } else {
        Line::from(state.filter.clone())
    };
    let p = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, state: &PickerState<'_>) {
    let text = if let Some(notice) = state.notice.as_ref() {
        notice.as_str()
    } else if state.search_focused {
        "typing filters | Up/Down navigate | Enter select | Esc stop search"
    } else {
        "Up/Down navigate | Enter select | / search | f favorite | d default | Esc cancel"
    };
    let p = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(p, area);
}

fn draw_custom_input_modal(f: &mut ratatui::Frame, area: Rect, input: &str) {
    let width = area.width.saturating_sub(8).min(80);
    let height = 7.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" custom command ")
        .style(Style::default().fg(Color::Cyan));
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

    let help = Paragraph::new("type a shell command (e.g. `/path/to/agent --flag`)")
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: false });
    f.render_widget(help, layout[0]);

    let body = Paragraph::new(Line::from(vec![Span::raw("> "), Span::raw(input)]));
    f.render_widget(body, layout[1]);

    let footer = Paragraph::new("Enter to confirm | Esc to cancel")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, layout[2]);
}

fn draw_install_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
    extracting: bool,
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
        .style(Style::default().fg(Color::Yellow));
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

    let footer = Paragraph::new("Esc to cancel").style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, layout[2]);
}

fn draw_error_modal(f: &mut ratatui::Frame, area: Rect, msg: &str) {
    let width = area.width.saturating_sub(8).min(80);
    let height = 7.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" error ")
        .style(Style::default().fg(Color::Red));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let body = Paragraph::new(msg).wrap(Wrap { trim: false });
    f.render_widget(body, layout[0]);

    let footer =
        Paragraph::new("Enter or Esc to dismiss").style(Style::default().fg(Color::DarkGray));
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

    #[test]
    fn picker_lists_anvil_registry_and_custom() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            PickerPreferences::default(),
        );
        // 1 anvil + 3 registry + 1 custom = 5 items
        assert_eq!(state.items.len(), 5);
        assert!(matches!(state.items[0], Item::Anvil));
        assert!(state.items.iter().any(|item| matches!(item, Item::Custom)));
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
        let visible: Vec<String> = state
            .filtered
            .iter()
            .map(|&i| state.item_label(&state.items[i]))
            .collect();
        assert_eq!(visible, vec!["BinaryOnly".to_string()]);
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
                favorite_source_ids: Vec::new(),
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
                favorite_source_ids: Vec::new(),
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
                default_agent: None,
                favorite_source_ids: vec!["claude-acp".to_string()],
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
        assert_eq!(state.item_source_id(&state.items[0]), "anvil");
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
