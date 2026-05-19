//! Interactive agent picker.
//!
//! Renders a ratatui screen that lists `anvil` + registry agents +
//! `Custom`, lets the user filter and select one, then resolves the
//! selection into a launch command (downloading a binary archive when
//! needed, with a progress spinner). Used both at first launch and from
//! the `/mj:agents` slash command.

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

/// Resolved launch command for the chosen agent.
#[derive(Debug, Clone)]
pub struct PickerOutcome {
    pub source_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// One row in the picker. `Anvil` and `Custom` are synthetic entries;
/// `Agent` indexes into the registry's agent list.
#[derive(Debug, Clone)]
enum Item {
    Anvil,
    Agent(usize),
    Custom,
}

enum Mode {
    Browse,
    CustomInput {
        input: String,
    },
    Installing {
        label: String,
        total_bytes: Option<u64>,
        downloaded_bytes: u64,
        extracting: bool,
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
    current_source_id: Option<String>,
}

impl<'a> PickerState<'a> {
    fn new(
        registry: &'a Registry,
        platform: String,
        install_root: PathBuf,
        current_source_id: Option<String>,
    ) -> Self {
        let mut items = vec![Item::Anvil];
        // Sort registry agents by display name for a predictable picker.
        let mut indices: Vec<usize> = (0..registry.agents.len()).collect();
        indices.sort_by(|&a, &b| {
            registry.agents[a]
                .name
                .to_lowercase()
                .cmp(&registry.agents[b].name.to_lowercase())
        });
        for i in indices {
            items.push(Item::Agent(i));
        }
        items.push(Item::Custom);

        let mut state = Self {
            registry,
            platform,
            install_root,
            items,
            filter: String::new(),
            filtered: Vec::new(),
            selected: 0,
            mode: Mode::Browse,
            current_source_id,
        };
        state.recompute_filter();
        state
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
            Item::Anvil => "anvil".to_string(),
            Item::Custom => "custom command".to_string(),
            Item::Agent(idx) => {
                let a = &self.registry.agents[*idx];
                format!("{} {} {}", a.name, a.id, a.description).to_lowercase()
            }
        }
    }

    fn item_hint(&self, item: &Item) -> String {
        match item {
            Item::Anvil => "default mj agent".to_string(),
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

    fn item_is_current(&self, item: &Item) -> bool {
        let Some(cur) = &self.current_source_id else {
            return false;
        };
        match item {
            Item::Anvil => cur == "anvil",
            Item::Custom => cur == "custom",
            Item::Agent(idx) => self.registry.agents[*idx].id == *cur,
        }
    }

    fn recompute_filter(&mut self) {
        let q = self.filter.to_lowercase();
        let prev_selected_label = self
            .filtered
            .get(self.selected)
            .map(|&i| self.item_label(&self.items[i]));

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
        self.selected = prev_selected_label
            .and_then(|label| {
                self.filtered
                    .iter()
                    .position(|&i| self.item_label(&self.items[i]) == label)
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
}

/// Run the picker until the user selects an agent or cancels with Esc.
/// Returns `Ok(None)` when the user cancels.
pub async fn run_picker(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    registry: &Registry,
    install_root: &Path,
    platform: &str,
    current_source_id: Option<String>,
) -> Result<Option<PickerOutcome>> {
    let mut state = PickerState::new(
        registry,
        platform.to_string(),
        install_root.to_path_buf(),
        current_source_id,
    );

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw(f, &state))?;

    loop {
        tokio::select! {
            biased;
            maybe_ev = events.next() => {
                let Some(ev) = maybe_ev else {
                    return Ok(None);
                };
                let ev = ev.context("crossterm event stream")?;
                if let Some(outcome) = handle_event(&mut state, ev).await? {
                    return Ok(Some(outcome));
                }
            }
            _ = tick.tick() => {
                if let Some(outcome) = pump_install(&mut state).await {
                    return Ok(Some(outcome));
                }
            }
        }
        terminal.draw(|f| draw(f, &state))?;
        if matches!(state.mode, Mode::Cancelled) {
            return Ok(None);
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
        if let Mode::Installing { task, .. } = prev {
            match task.await {
                Ok(Ok(outcome)) => return Some(outcome),
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
        Mode::CustomInput { input } => match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                state.mode = Mode::Browse;
            }
            (_, KeyCode::Enter) => {
                let raw = input.trim().to_string();
                if raw.is_empty() {
                    state.mode = Mode::Error("custom command cannot be empty".to_string());
                } else {
                    match parse_custom_command(&raw) {
                        Ok(outcome) => return Ok(Some(outcome)),
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
            (KeyModifiers::CONTROL, KeyCode::Char('c')) | (_, KeyCode::Esc) => {
                state.mode = Mode::Cancelled;
            }
            (_, KeyCode::Up) => state.move_selection(-1),
            (_, KeyCode::Down) => state.move_selection(1),
            (_, KeyCode::Enter) => {
                if let Some(item) = state.focused_item().cloned() {
                    return start_selection(state, &item).await;
                }
            }
            (_, KeyCode::Backspace) => {
                state.filter.pop();
                state.recompute_filter();
            }
            (_, KeyCode::Char(c)) => {
                state.filter.push(c);
                state.recompute_filter();
            }
            _ => {}
        },
    }
    Ok(None)
}

async fn start_selection(
    state: &mut PickerState<'_>,
    item: &Item,
) -> Result<Option<PickerOutcome>> {
    match item {
        Item::Anvil => Ok(Some(PickerOutcome {
            source_id: "anvil".to_string(),
            program: PathBuf::from("anvil"),
            args: Vec::new(),
            env: HashMap::new(),
        })),
        Item::Custom => {
            state.mode = Mode::CustomInput {
                input: String::new(),
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
                        rx,
                        task,
                    };
                    Ok(None)
                }
                DistributionKind::Npx => {
                    let pkg = agent.distribution.npx.as_ref().expect("npx checked");
                    let mut args = vec!["-y".to_string(), pkg.package.clone()];
                    args.extend(pkg.args.iter().cloned());
                    Ok(Some(PickerOutcome {
                        source_id: agent.id.clone(),
                        program: PathBuf::from("npx"),
                        args,
                        env: pkg.env.clone(),
                    }))
                }
                DistributionKind::Uvx => {
                    let pkg = agent.distribution.uvx.as_ref().expect("uvx checked");
                    let mut args = vec![pkg.package.clone()];
                    args.extend(pkg.args.iter().cloned());
                    Ok(Some(PickerOutcome {
                        source_id: agent.id.clone(),
                        program: PathBuf::from("uvx"),
                        args,
                        env: pkg.env.clone(),
                    }))
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
    draw_footer(f, chunks[3]);

    match &state.mode {
        Mode::CustomInput { input } => draw_custom_input_modal(f, f.area(), input),
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
    let p = Paragraph::new(" mj | choose an agent ")
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

    let visible = inner.height as usize;
    let total = state.filtered.len();
    let start = if total <= visible {
        0
    } else {
        let half = visible / 2;
        state.selected.saturating_sub(half).min(total - visible)
    };
    let end = (start + visible).min(total);

    let items: Vec<ListItem> = state.filtered[start..end]
        .iter()
        .enumerate()
        .map(|(offset, &i)| {
            let absolute = start + offset;
            let item = &state.items[i];
            let marker = if absolute == state.selected { ">" } else { " " };
            let badge = if state.item_is_current(item) {
                " [current]"
            } else {
                ""
            };
            let label = state.item_label(item);
            let hint = state.item_hint(item);
            let line = format!("{marker} {label}{badge}  -- {hint}");
            let style = if absolute == state.selected {
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
    let title = " filter (start typing) ";
    let block = Block::default().borders(Borders::ALL).title(title);
    let p = Paragraph::new(state.filter.as_str())
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect) {
    let p = Paragraph::new("Up/Down navigate | Enter select | Esc cancel")
        .style(Style::default().fg(Color::DarkGray));
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
            None,
        );
        // 1 anvil + 2 registry + 1 custom = 4 items
        assert_eq!(state.items.len(), 4);
        assert!(matches!(state.items[0], Item::Anvil));
        assert!(matches!(state.items.last(), Some(Item::Custom)));
    }

    #[test]
    fn picker_sorts_registry_agents_alphabetically() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            None,
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
            None,
        );
        state.filter = "binary".to_string();
        state.recompute_filter();
        let visible: Vec<String> = state
            .filtered
            .iter()
            .map(|&i| state.item_label(&state.items[i]))
            .collect();
        assert_eq!(visible, vec!["BinaryOnly".to_string()]);
    }

    #[test]
    fn picker_marks_current_selection() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            Some("anvil".to_string()),
        );
        assert!(state.item_is_current(&Item::Anvil));
        assert!(!state.item_is_current(&Item::Custom));
    }

    #[test]
    fn picker_hint_describes_distribution_choice() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            None,
        );
        // Find Claude (npx-only) and BinaryOnly entries.
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
    }

    #[test]
    fn picker_hint_warns_on_incompatible_binary_only() {
        let reg = fixture_registry();
        let state = PickerState::new(
            &reg,
            "windows-x86_64".to_string(),
            PathBuf::from("/tmp"),
            None,
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

    #[test]
    fn picker_move_selection_wraps() {
        let reg = fixture_registry();
        let mut state = PickerState::new(
            &reg,
            "darwin-aarch64".to_string(),
            PathBuf::from("/tmp"),
            None,
        );
        assert_eq!(state.selected, 0);
        state.move_selection(-1);
        assert_eq!(state.selected, state.filtered.len() - 1);
        state.move_selection(1);
        assert_eq!(state.selected, 0);
    }
}
