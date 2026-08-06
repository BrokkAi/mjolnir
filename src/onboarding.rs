//! Guided first-run and major-upgrade product onboarding.

use std::io::Stdout;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ONBOARDING_CONTENT_VERSION};
use crate::ink::{Ink, InkStyle};
use crate::palette::TerminalTheme;
use crate::roster::{AcpInventory, Roster};
use crate::settings::{SettingsAction, SettingsEditor, draw_settings_panel};
use crate::term::TrackedBackend;
use crate::version::mjolnir_version_label;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fresh,
    Upgrade,
}

#[derive(Debug)]
pub enum Outcome {
    Accept(Box<Config>, Box<Roster>),
    Skip(Box<Config>),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Welcome,
    WhatsNew,
    Connections,
    Customize,
    Readiness,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Cancel,
    Resolve,
    Authenticate(crate::auth::AuthVendor),
    UseRecommended,
    Skip,
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeTone {
    Info,
    Success,
    Warning,
}

struct State {
    kind: Kind,
    screen: Screen,
    original_config: Config,
    editor: SettingsEditor,
    roster: Option<Roster>,
    inventory: AcpInventory,
    selected: usize,
    scroll: u16,
    notice: Option<String>,
    notice_tone: NoticeTone,
    customize_return: Screen,
    customize_snapshot: Option<SettingsEditor>,
    reveal_selection: bool,
}

impl State {
    fn new(kind: Kind, config: Config, roster: Option<Roster>, notice: Option<String>) -> Self {
        let original_config = config.clone();
        let inventory = roster
            .as_ref()
            .map(|roster| roster.inventory.clone())
            .unwrap_or_else(|| crate::roster::discover_inventory(&config));
        let choices = roster
            .as_ref()
            .map(|roster| roster.choices.clone())
            .unwrap_or_default();
        let mut editor =
            SettingsEditor::new(config, choices, None).with_inventory(inventory.clone());
        if let Some(roster) = &roster {
            editor = editor.with_active_models(crate::config::ModelsConfig {
                primary: roster.primary.model.model.clone(),
                primary_source: Some(roster.primary.launch.source_id.clone()),
                review: roster
                    .review_supervisor
                    .as_ref()
                    .map(|role| role.model.model.clone())
                    .unwrap_or_else(|| "off".to_string()),
                review_source: roster
                    .review_supervisor
                    .as_ref()
                    .map(|role| role.launch.source_id.clone()),
                subagent: roster
                    .subagent_default
                    .as_ref()
                    .map(|role| role.model.model.clone())
                    .unwrap_or_else(|| "off".to_string()),
                subagent_source: roster
                    .subagent_default
                    .as_ref()
                    .map(|role| role.launch.source_id.clone()),
            });
        }
        let screen = if notice.is_some() {
            Screen::Connections
        } else if kind == Kind::Upgrade {
            Screen::WhatsNew
        } else {
            Screen::Welcome
        };
        let mut state = Self {
            kind,
            screen,
            original_config,
            editor,
            roster,
            inventory,
            selected: 0,
            scroll: 0,
            notice,
            notice_tone: NoticeTone::Warning,
            customize_return: screen,
            customize_snapshot: None,
            reveal_selection: screen == Screen::Connections,
        };
        if screen == Screen::Connections {
            state.selected = Self::default_connection_selection();
        }
        state
    }

    fn config(&self) -> &Config {
        &self.editor.config
    }

    fn apply_recommended_setup(&mut self) {
        self.editor.config.agent.model = "auto".to_string();
        self.editor.config.agent.acp_source = Some("codex-acp".to_string());
        self.editor.config.agent.discrete_review = true;
        self.editor.config.review.model = "auto".to_string();
        self.editor.config.review.acp_source = Some("codex-acp".to_string());
        self.editor.config.subagents.model = "auto".to_string();
        self.editor.config.subagents.acp_source = Some("codex-acp".to_string());
        self.editor.config.subagents.auto_failover = true;
        self.editor
            .config
            .set_acp_server_policy("codex-acp", crate::config::AcpServerPolicy::Enabled);
    }

    fn visited_config(&self) -> Config {
        let mut config = self.editor.config.clone();
        config.onboarding_version = ONBOARDING_CONTENT_VERSION;
        config
    }

    fn skipped_config(&self) -> Config {
        let mut config = self.original_config.clone();
        config.onboarding_version = ONBOARDING_CONTENT_VERSION;
        config
    }

    fn change_screen(&mut self, screen: Screen) {
        self.screen = screen;
        self.selected = if screen == Screen::Connections {
            Self::default_connection_selection()
        } else {
            0
        };
        self.scroll = 0;
        self.notice = None;
        self.notice_tone = NoticeTone::Info;
        self.reveal_selection = screen == Screen::Connections;
    }

    fn connection_item_count(&self) -> usize {
        crate::auth::AuthVendor::ALL.len() + 2
    }

    fn recommended_index() -> usize {
        crate::auth::AuthVendor::ALL.len()
    }

    fn customize_index() -> usize {
        crate::auth::AuthVendor::ALL.len() + 1
    }

    fn default_connection_selection() -> usize {
        Self::connection_selection_for_openai(
            crate::auth::detect(crate::auth::AuthVendor::OpenAi).available(),
        )
    }

    fn connection_selection_for_openai(available: bool) -> usize {
        if available {
            Self::recommended_index()
        } else {
            crate::auth::AuthVendor::ALL
                .iter()
                .position(|vendor| *vendor == crate::auth::AuthVendor::OpenAi)
                .unwrap_or(0)
        }
    }

    fn open_customize(&mut self, return_to: Screen) {
        self.customize_snapshot = Some(self.editor.clone());
        self.customize_return = return_to;
        self.change_screen(Screen::Customize);
    }

    fn terminal_resized(&mut self) {
        self.reveal_selection = self.screen == Screen::Connections;
    }

    fn move_selected(&mut self, delta: i32, len: usize) {
        if len > 0 {
            self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
            self.reveal_selection = true;
        }
    }

    fn scroll(&mut self, delta: i32) {
        self.scroll = if delta.is_negative() {
            self.scroll.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            self.scroll.saturating_add(delta as u16)
        };
    }

    fn handle_key(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::PageUp => {
                self.scroll(-5);
                return Action::None;
            }
            KeyCode::PageDown => {
                self.scroll(5);
                return Action::None;
            }
            KeyCode::Home => {
                self.scroll = 0;
                return Action::None;
            }
            KeyCode::End => {
                self.scroll = u16::MAX;
                return Action::None;
            }
            _ => {}
        }
        match self.screen {
            Screen::Welcome => match code {
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('n') => {
                    self.change_screen(Screen::Connections);
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll(1);
                    Action::None
                }
                KeyCode::Esc => Action::Cancel,
                _ => Action::None,
            },
            Screen::WhatsNew => match code {
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('n') => {
                    if self.roster.is_some() {
                        Action::Finish
                    } else {
                        Action::Resolve
                    }
                }
                KeyCode::Char('c' | 'C') => {
                    self.open_customize(Screen::WhatsNew);
                    Action::None
                }
                KeyCode::Char('s' | 'S') | KeyCode::Esc => Action::Skip,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll(1);
                    Action::None
                }
                _ => Action::None,
            },
            Screen::Connections => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_selected(-1, self.connection_item_count());
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_selected(1, self.connection_item_count());
                    Action::None
                }
                KeyCode::Enter if self.selected < crate::auth::AuthVendor::ALL.len() => {
                    Action::Authenticate(crate::auth::AuthVendor::ALL[self.selected])
                }
                KeyCode::Enter if self.selected == Self::recommended_index() => {
                    Action::UseRecommended
                }
                KeyCode::Enter => {
                    self.open_customize(Screen::Connections);
                    Action::None
                }
                KeyCode::Right | KeyCode::Char('n') => Action::UseRecommended,
                KeyCode::Char('r' | 'R') => Action::Resolve,
                KeyCode::Char('c' | 'C') => {
                    self.open_customize(Screen::Connections);
                    Action::None
                }
                KeyCode::Esc | KeyCode::Left => {
                    self.change_screen(if self.kind == Kind::Fresh {
                        Screen::Welcome
                    } else {
                        Screen::WhatsNew
                    });
                    Action::None
                }
                _ => Action::None,
            },
            Screen::Customize => match self.editor.handle_key(code) {
                SettingsAction::Save => {
                    self.customize_snapshot = None;
                    Action::Resolve
                }
                SettingsAction::Cancel => {
                    if let Some(editor) = self.customize_snapshot.take() {
                        let mut changed = std::mem::replace(&mut self.editor, editor);
                        changed.cancel_background();
                    }
                    self.change_screen(self.customize_return);
                    Action::None
                }
                SettingsAction::Authenticate(vendor) => Action::Authenticate(vendor),
                SettingsAction::None | SettingsAction::Changed => Action::None,
            },
            Screen::Readiness => match code {
                KeyCode::Enter => Action::Finish,
                KeyCode::Char('c' | 'C') => {
                    self.open_customize(Screen::Readiness);
                    Action::None
                }
                KeyCode::Char('r' | 'R') => Action::Resolve,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll(1);
                    Action::None
                }
                KeyCode::Esc | KeyCode::Left => {
                    self.change_screen(if self.kind == Kind::Fresh {
                        Screen::Connections
                    } else {
                        Screen::WhatsNew
                    });
                    Action::None
                }
                _ => Action::None,
            },
        }
    }

    fn resolution_succeeded(&mut self, roster: Roster) {
        self.inventory = roster.inventory.clone();
        let mut editor =
            SettingsEditor::new(self.editor.config.clone(), roster.choices.clone(), None)
                .with_inventory(roster.inventory.clone())
                .with_active_models(crate::config::ModelsConfig {
                    primary: roster.primary.model.model.clone(),
                    primary_source: Some(roster.primary.launch.source_id.clone()),
                    review: roster
                        .review_supervisor
                        .as_ref()
                        .map(|role| role.model.model.clone())
                        .unwrap_or_else(|| "off".to_string()),
                    review_source: roster
                        .review_supervisor
                        .as_ref()
                        .map(|role| role.launch.source_id.clone()),
                    subagent: roster
                        .subagent_default
                        .as_ref()
                        .map(|role| role.model.model.clone())
                        .unwrap_or_else(|| "off".to_string()),
                    subagent_source: roster
                        .subagent_default
                        .as_ref()
                        .map(|role| role.launch.source_id.clone()),
                });
        std::mem::swap(&mut self.editor, &mut editor);
        editor.cancel_background();
        self.roster = Some(roster);
        self.change_screen(Screen::Readiness);
    }

    fn resolution_failed(&mut self, error: impl std::fmt::Display) {
        let error = error.to_string();
        self.roster = None;
        self.inventory = crate::roster::discover_inventory(&self.editor.config);
        self.screen = Screen::Connections;
        self.selected = if error.to_ascii_lowercase().contains("kimi") {
            crate::auth::AuthVendor::ALL
                .iter()
                .position(|vendor| *vendor == crate::auth::AuthVendor::Kimi)
                .unwrap_or(0)
        } else {
            crate::auth::AuthVendor::ALL
                .iter()
                .position(|vendor| !crate::auth::detect(*vendor).available())
                .unwrap_or(0)
        };
        self.scroll = 0;
        self.reveal_selection = true;
        self.notice = Some(format!(
            "No launchable route yet: {error}. Sign in or repair a connection, then press R to retry."
        ));
        self.notice_tone = NoticeTone::Warning;
    }
}

pub async fn run(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    kind: Kind,
    config: Config,
    roster: Option<Roster>,
    notice: Option<String>,
    cwd: &Path,
    termination: CancellationToken,
) -> Result<Outcome> {
    let mut state = State::new(kind, config, roster, notice);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    terminal.draw(|frame| draw(frame, &mut state))?;
    loop {
        tokio::select! {
            biased;
            _ = termination.cancelled() => {
                state.editor.cancel_background();
                return Ok(Outcome::Cancel);
            },
            event = events.next() => {
                let Some(event) = event else {
                    return Ok(Outcome::Cancel);
                };
                let event = event.context("onboarding event")?;
                if matches!(&event, CtEvent::Resize(_, _)) {
                    state.terminal_resized();
                }
                let CtEvent::Key(key) = event else {
                    terminal.draw(|frame| draw(frame, &mut state))?;
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    state.editor.cancel_background();
                    return Ok(Outcome::Cancel);
                }
                match state.handle_key(key.code) {
                    Action::None => {}
                    Action::Cancel => {
                        state.editor.cancel_background();
                        return Ok(Outcome::Cancel);
                    }
                    Action::Skip => {
                        state.editor.cancel_background();
                        return Ok(Outcome::Skip(Box::new(state.skipped_config())));
                    }
                    Action::Finish => {
                        let Some(roster) = state.roster.take() else {
                            state.resolution_failed("readiness was not resolved");
                            terminal.draw(|frame| draw(frame, &mut state))?;
                            continue;
                        };
                        return Ok(Outcome::Accept(
                            Box::new(state.visited_config()),
                            Box::new(roster),
                        ));
                    }
                    Action::Authenticate(vendor) => {
                        let (notice, login_succeeded) = if crate::auth::executable(vendor).is_none() {
                            (
                                format!(
                                    "{} CLI is not installed. Run `{}` and retry.",
                                    vendor.label(),
                                    crate::auth::install_hint(vendor)
                                ),
                                false,
                            )
                        } else {
                            crate::ui::restore_terminal_for_auth(terminal, crate::ui::UiMode::FullscreenTui)?;
                            let login = crate::auth::run_login(vendor).await;
                            crate::ui::resume_terminal_after_auth(terminal, crate::ui::UiMode::FullscreenTui)?;
                            match login {
                                Ok(outcome) => {
                                    let succeeded = outcome.succeeded();
                                    (outcome.into_message(), succeeded)
                                }
                                Err(error) => (format!("Sign-in failed: {error:#}"), false),
                            }
                        };
                        let signed_in = login_succeeded && crate::auth::detect(vendor).available();
                        if state.screen == Screen::Customize {
                            state.editor.refresh_after_auth(notice);
                        } else {
                            state.notice = Some(notice);
                            state.notice_tone = if signed_in {
                                NoticeTone::Success
                            } else {
                                NoticeTone::Warning
                            };
                            if signed_in && vendor == crate::auth::AuthVendor::OpenAi {
                                state.selected = State::recommended_index();
                                state.reveal_selection = true;
                            }
                        }
                    }
                    action @ (Action::Resolve | Action::UseRecommended) => {
                        if action == Action::UseRecommended {
                            state.apply_recommended_setup();
                        }
                        state.notice = Some("Checking provider routes and role readiness…".to_string());
                        state.notice_tone = NoticeTone::Info;
                        terminal.draw(|frame| draw(frame, &mut state))?;
                        match crate::roster::resolve_waiting_for_installs(&state.editor.config, cwd).await {
                            Ok(roster) => state.resolution_succeeded(roster),
                            Err(error) => state.resolution_failed(format!("{error:#}")),
                        }
                    }
                }
            }
            _ = tick.tick() => state.editor.poll_background(),
        }
        terminal.draw(|frame| draw(frame, &mut state))?;
    }
}

const PANEL_MAX_WIDTH: u16 = 104;
const PANEL_MAX_HEIGHT: u16 = 30;
const PANEL_MIN_HEIGHT: u16 = 14;

fn draw(frame: &mut ratatui::Frame, state: &mut State) {
    if state.screen == Screen::Customize {
        draw_settings_panel(frame, frame.area(), &state.editor, "Customize Mjolnir");
        return;
    }

    let theme = state.config().theme.palette();
    let panel_width = onboarding_panel_width(frame.area());
    let content_width = panel_width.saturating_sub(2).max(1);
    let mut lines = screen_lines(state, theme);
    if let Some(notice) = &state.notice {
        let (label, ink) = match state.notice_tone {
            NoticeTone::Info => ("SETUP STATUS", theme.primary),
            NoticeTone::Success => ("CONNECTED", theme.success),
            NoticeTone::Warning => ("NEEDS ATTENTION", theme.warning),
        };
        lines.push(Line::raw(""));
        lines.push(section_heading(label, ink));
        lines.push(Line::styled(notice.clone(), Style::default().ink(ink)));
    }
    let measured_lines = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .line_count(content_width);
    let base_footer = footer_line(state.screen, content_width, false, theme);
    let base_footer_height = wrapped_line_height(&base_footer, content_width);
    let available_height = if frame.area().height > 2 {
        frame.area().height - 2
    } else {
        frame.area().height
    };
    let available_height = available_height.min(PANEL_MAX_HEIGHT);
    let required_height = u16::try_from(measured_lines)
        .unwrap_or(PANEL_MAX_HEIGHT)
        .saturating_add(5 + base_footer_height);
    let scrollable = required_height > available_height;
    let footer = footer_line(state.screen, content_width, scrollable, theme);
    let footer_height = wrapped_line_height(&footer, content_width).clamp(1, 4);
    let preferred_height = u16::try_from(measured_lines)
        .unwrap_or(PANEL_MAX_HEIGHT)
        .saturating_add(7 + footer_height)
        .clamp(PANEL_MIN_HEIGHT, PANEL_MAX_HEIGHT);
    let panel_area = onboarding_panel_area(frame.area(), preferred_height);
    let panel = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().ink(theme.primary));
    let inner = panel.inner(panel_area);
    frame.render_widget(panel, panel_area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let footer_height = footer_height.min(inner.height.saturating_sub(4).max(1));

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(header_line(state, inner.width, theme)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(progress_line(state, inner.width, theme)),
        rows[1],
    );

    let selected_bounds = (state.screen == Screen::Connections && state.reveal_selection)
        .then(|| connection_selection_bounds(&lines, rows[2].width));
    let paragraph = Paragraph::new(lines)
        .style(Style::default().ink(theme.text))
        .wrap(Wrap { trim: false });
    let line_count = paragraph.line_count(rows[2].width);
    let max_scroll = line_count
        .saturating_sub(usize::from(rows[2].height))
        .min(u16::MAX as usize) as u16;
    if let Some((start, end)) = selected_bounds {
        let visible = usize::from(rows[2].height).max(1);
        let current = usize::from(state.scroll);
        let next = if end.saturating_sub(start) >= visible || start < current {
            start
        } else if end > current + visible {
            end.saturating_sub(visible)
        } else {
            current
        };
        state.scroll = next.min(u16::MAX as usize) as u16;
        state.reveal_selection = false;
    }
    state.scroll = state.scroll.min(max_scroll);
    let body_area = if max_scroll == 0 && line_count < usize::from(rows[2].height) {
        let height = u16::try_from(line_count).unwrap_or(rows[2].height).max(1);
        Rect {
            x: rows[2].x,
            y: rows[2].y + (rows[2].height.saturating_sub(height) / 2),
            width: rows[2].width,
            height,
        }
    } else {
        rows[2]
    };
    frame.render_widget(paragraph.scroll((state.scroll, 0)), body_area);
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), rows[3]);
}

fn wrapped_line_height(line: &Line<'static>, width: u16) -> u16 {
    u16::try_from(
        Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width.max(1)),
    )
    .unwrap_or(u16::MAX)
}

fn onboarding_panel_width(area: Rect) -> u16 {
    if area.width > PANEL_MAX_WIDTH + 2 {
        PANEL_MAX_WIDTH
    } else if area.width > 2 {
        area.width - 2
    } else {
        area.width
    }
}

fn onboarding_panel_area(area: Rect, preferred_height: u16) -> Rect {
    let width = onboarding_panel_width(area);
    let available_height = if area.height > 2 {
        area.height - 2
    } else {
        area.height
    };
    let height = preferred_height.min(PANEL_MAX_HEIGHT).min(available_height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn header_line(state: &State, width: u16, theme: TerminalTheme) -> Line<'static> {
    let brand = format!(" {} ", mjolnir_version_label().to_uppercase());
    let context = match state.kind {
        Kind::Fresh => "GUIDED SETUP",
        Kind::Upgrade => "PRODUCT UPDATE",
    };
    let gap = usize::from(width).saturating_sub(brand.chars().count() + context.len());
    let mut spans = vec![Span::styled(
        brand,
        Style::default()
            .ink(theme.primary)
            .add_modifier(Modifier::BOLD),
    )];
    if gap > 1 {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(
            context,
            Style::default()
                .ink(theme.muted)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn progress_line(state: &State, width: u16, theme: TerminalTheme) -> Line<'static> {
    if state.kind == Kind::Upgrade {
        let label = match state.screen {
            Screen::WhatsNew => "WHAT'S NEW",
            Screen::Connections => "CHECK CONNECTIONS",
            Screen::Readiness => "CHANGES READY",
            Screen::Welcome | Screen::Customize => "PRODUCT UPDATE",
        };
        return Line::styled(
            label,
            Style::default()
                .ink(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
        .centered();
    }

    let steps = &["WELCOME", "CONNECT", "READY"];
    let current = match state.screen {
        Screen::Welcome => 0,
        Screen::Connections => 1,
        Screen::Readiness => 2,
        Screen::WhatsNew | Screen::Customize => 0,
    };
    if width < 58 {
        return Line::styled(
            format!(
                "STEP {} OF {}  ·  {}",
                current + 1,
                steps.len(),
                steps[current]
            ),
            Style::default()
                .ink(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
        .centered();
    }

    let mut spans = Vec::new();
    for (index, label) in steps.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" ─── ", Style::default().ink(theme.subtle)));
        }
        let (marker, style) = if index < current {
            (
                "✓".to_string(),
                Style::default()
                    .ink(theme.success)
                    .add_modifier(Modifier::BOLD),
            )
        } else if index == current {
            (
                (index + 1).to_string(),
                Style::default()
                    .ink(theme.selection_fg)
                    .ink_bg(theme.selection_bg)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ((index + 1).to_string(), Style::default().ink(theme.muted))
        };
        spans.push(Span::styled(format!(" {marker} {label} "), style));
    }
    Line::from(spans).centered()
}

fn hero(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    theme: TerminalTheme,
) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            title.into(),
            Style::default()
                .ink(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
        .centered(),
        Line::styled(subtitle.into(), Style::default().ink(theme.muted)).centered(),
        Line::raw(""),
    ]
}

fn section_heading(text: impl Into<String>, ink: Ink) -> Line<'static> {
    Line::styled(
        format!("  {}", text.into()),
        Style::default().ink(ink).add_modifier(Modifier::BOLD),
    )
}

fn role_line(label: &str, detail: impl Into<String>, ink: Ink) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {label:<12}"),
            Style::default().ink(ink).add_modifier(Modifier::BOLD),
        ),
        Span::raw(detail.into()),
    ])
}

fn choice_line(selected: bool, label: &str, status: &str, theme: TerminalTheme) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let text = format!(" {marker} {label:<28} {status}");
    let style = if selected {
        Style::default()
            .ink(theme.selection_fg)
            .ink_bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().ink(theme.text)
    };
    Line::styled(text, style)
}

fn connection_selection_bounds(lines: &[Line<'static>], width: u16) -> (usize, usize) {
    let logical_line = lines
        .iter()
        .position(|line| line.to_string().trim_start().starts_with('>'))
        .unwrap_or(0);
    let width = width.max(1);
    let start = Paragraph::new(lines[..logical_line].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width);
    let end = Paragraph::new(lines[..=logical_line].to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width);
    (start, end)
}

fn detail_line(detail: impl Into<String>, theme: TerminalTheme) -> Line<'static> {
    Line::styled(
        format!("     {}", detail.into()),
        Style::default().ink(theme.muted),
    )
}

fn screen_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    match state.screen {
        Screen::Welcome => welcome_lines(theme),
        Screen::WhatsNew => whats_new_lines(state, theme),
        Screen::Connections => connection_lines(state, theme),
        Screen::Readiness => readiness_lines(state, theme),
        Screen::Customize => unreachable!("custom settings draw separately"),
    }
}

fn welcome_lines(theme: TerminalTheme) -> Vec<Line<'static>> {
    let mut lines = hero(
        "ONE REQUEST. A COORDINATED TEAM.",
        "Mjolnir gives Codex a team without splitting your conversation.",
        theme,
    );
    lines.push(
        Line::from(vec![
            Span::styled("YOU", Style::default().ink(theme.text)),
            Span::styled("  ──►  ", Style::default().ink(theme.subtle)),
            Span::styled(
                "PRIMARY",
                Style::default()
                    .ink(theme.primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ──►  ", Style::default().ink(theme.subtle)),
            Span::styled(
                "VERIFIED RESULT",
                Style::default()
                    .ink(theme.success)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
        .centered(),
    );
    lines.push(Line::raw(""));
    lines.push(role_line(
        "LEAD",
        "Plans the work, coordinates the team, and owns the final answer",
        theme.primary,
    ));
    lines.push(role_line(
        "BUILD",
        "Subagents investigate, implement, and test in parallel",
        theme.secondary,
    ));
    lines.push(role_line(
        "CHECK",
        "Independent review challenges every changed turn",
        theme.success,
    ));
    lines.push(Line::raw(""));
    lines.push(
        Line::styled(
            "One worktree  ·  visible delegation  ·  one verified result",
            Style::default().ink(theme.muted),
        )
        .centered(),
    );
    lines
}

fn whats_new_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    let mut lines = hero(
        "CODEX-FIRST, END TO END",
        "New setups now use Codex for every role by default.",
        theme,
    );
    lines.push(
        Line::from(vec![
            Span::styled("PRIMARY  CODEX", Style::default().ink(theme.primary)),
            Span::styled("   ·   ", Style::default().ink(theme.subtle)),
            Span::styled("BUILD  CODEX", Style::default().ink(theme.secondary)),
            Span::styled("   ·   ", Style::default().ink(theme.subtle)),
            Span::styled("CHECK  CODEX", Style::default().ink(theme.success)),
        ])
        .centered(),
    );
    lines.push(Line::raw(""));
    lines.push(role_line(
        "STILL HERE",
        "Remote control, worktrees, local voice, and adversarial review",
        theme.text,
    ));
    lines.push(Line::styled(
        "  ✓  Your saved providers, routes, and settings will not change.",
        Style::default()
            .ink(theme.success)
            .add_modifier(Modifier::BOLD),
    ));
    if let Some(roster) = &state.roster {
        lines.push(Line::raw(""));
        lines.push(section_heading("YOUR CURRENT SETUP", theme.muted));
        lines.push(role_line(
            "PRIMARY",
            format!(
                "{}  ·  {}",
                roster.primary.model.model, roster.primary.launch.source_id
            ),
            theme.primary,
        ));
        lines.push(role_line(
            "BUILD",
            roster.subagent_default.as_ref().map_or_else(
                || "Primary only".to_string(),
                |worker| {
                    format!(
                        "{}  ·  {}  ·  {} parallel",
                        worker.model.model,
                        worker.launch.source_id,
                        state.config().subagents.max_parallel
                    )
                },
            ),
            theme.secondary,
        ));
        lines.push(role_line(
            "CHECK",
            roster.review_supervisor.as_ref().map_or_else(
                || "No dedicated review supervisor".to_string(),
                |reviewer| format!("{}  ·  {}", reviewer.model.model, reviewer.launch.source_id),
            ),
            theme.success,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(
        Line::styled(
            "Continue as-is, review the setup, or dismiss this update.",
            Style::default().ink(theme.muted),
        )
        .centered(),
    );
    lines
}

fn connection_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    let mut lines = hero(
        "START WITH CODEX",
        "Use your existing ChatGPT or Codex sign-in. Other providers are optional.",
        theme,
    );
    lines.push(section_heading("ACCOUNTS", theme.muted));
    for (index, vendor) in crate::auth::AuthVendor::ALL.iter().copied().enumerate() {
        let credential = crate::auth::detect(vendor);
        let status = if credential.available() {
            "✓ CONNECTED"
        } else if vendor == crate::auth::AuthVendor::OpenAi {
            "SIGN IN REQUIRED"
        } else {
            "OPTIONAL"
        };
        lines.push(choice_line(
            state.selected == index,
            vendor.label(),
            status,
            theme,
        ));
        lines.push(detail_line(
            if credential.available() {
                format!(
                    "{}  ·  Enter to reconnect  ·  enables {}",
                    credential.status(),
                    vendor.enables()
                )
            } else {
                format!("{}  ·  enables {}", credential.status(), vendor.enables())
            },
            theme,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(section_heading("START", theme.muted));
    lines.push(choice_line(
        state.selected == State::recommended_index(),
        "Use Codex defaults",
        "RECOMMENDED",
        theme,
    ));
    lines.push(detail_line(
        format!(
            "Automatic primary, builders, and review  ·  up to {} parallel",
            state.config().subagents.max_parallel
        ),
        theme,
    ));
    lines.push(choice_line(
        state.selected == State::customize_index(),
        "Customize every route",
        "ADVANCED",
        theme,
    ));
    lines.push(detail_line(
        "Models, providers, review, parallelism, and appearance",
        theme,
    ));
    lines.push(Line::raw(""));
    lines.push(section_heading("ROUTE CHECK", theme.muted));
    if state.inventory.servers.is_empty() {
        lines.push(detail_line("No ACP runtimes detected yet", theme));
    } else {
        let ready = state
            .inventory
            .servers
            .iter()
            .filter(|server| server.error.is_none() && server.model_count > 0)
            .count();
        let models: usize = state
            .inventory
            .servers
            .iter()
            .map(|server| server.model_count)
            .sum();
        if ready == 0 {
            lines.push(Line::styled(
                "     No launchable ACP runtime is ready yet",
                Style::default().ink(theme.warning),
            ));
        } else {
            lines.push(Line::styled(
                format!("     ✓ {ready} runtime(s) ready  ·  {models} model route(s)"),
                Style::default().ink(theme.success),
            ));
        }
        for server in &state.inventory.servers {
            let issue = if server.installing {
                Some("installing".to_string())
            } else if let Some(error) = &server.error {
                Some(format!("needs repair: {error}"))
            } else if server.selected && !server.detected {
                Some(format!("not detected: {}", server.evidence))
            } else if server.detected && server.model_count == 0 {
                Some("detected, but has no launchable model route".to_string())
            } else {
                None
            };
            if let Some(issue) = issue {
                lines.push(Line::styled(
                    format!("     {} ({})  ·  {issue}", server.label, server.id),
                    Style::default().ink(theme.warning),
                ));
            }
        }
    }
    lines
}

fn readiness_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    let Some(roster) = state.roster.as_ref() else {
        return vec![
            Line::styled(
                "ROUTES NOT READY",
                Style::default()
                    .ink(theme.warning)
                    .add_modifier(Modifier::BOLD),
            )
            .centered(),
            Line::styled(
                "Press R to check the configured routes again.",
                Style::default().ink(theme.muted),
            )
            .centered(),
        ];
    };
    let mut lines = hero(
        "✓  YOUR CODING TEAM IS READY",
        "Routes resolved. Nothing starts until you press Enter.",
        theme,
    );
    lines.push(role_line(
        "PRIMARY",
        format!(
            "{}  ·  {}",
            roster.primary.model.model, roster.primary.launch.source_id
        ),
        theme.primary,
    ));
    lines.push(detail_line(
        "Coordinates, verifies, corrects, and owns the final answer",
        theme,
    ));
    lines.push(Line::raw(""));
    if let Some(worker) = &roster.subagent_default {
        let alternatives = roster.subagent_failover_roles().len().saturating_sub(1);
        lines.push(role_line(
            "BUILD",
            format!(
                "{}  ·  {}  ·  {} parallel",
                worker.model.model,
                worker.launch.source_id,
                state.config().subagents.max_parallel
            ),
            theme.secondary,
        ));
        lines.push(detail_line(
            format!("Implementation subagents  ·  {alternatives} failover route(s)",),
            theme,
        ));
    } else {
        lines.push(role_line(
            "BUILD",
            "Disabled  ·  the primary implements directly",
            theme.secondary,
        ));
    }
    lines.push(Line::raw(""));
    if let Some(supervisor) = &roster.review_supervisor {
        lines.push(role_line(
            "CHECK",
            format!(
                "{}  ·  {}",
                supervisor.model.model, supervisor.launch.source_id
            ),
            theme.success,
        ));
        lines.push(detail_line(
            if state.config().agent.discrete_review {
                let tier = state.config().agent.review_tier;
                if roster.subagent_default.is_some() {
                    format!(
                        "Automatic {} review every changed turn  ·  reviewers available",
                        tier.as_str()
                    )
                } else {
                    format!(
                        "Automatic {} review every changed turn  ·  no reviewer worker pool",
                        tier.as_str()
                    )
                }
            } else {
                "Review supervisor resolved, but automatic review is disabled".to_string()
            },
            theme,
        ));
    } else {
        lines.push(role_line(
            "CHECK",
            "No dedicated review supervisor  ·  degraded primary-only review",
            theme.warning,
        ));
        lines.push(detail_line(
            if state.config().agent.discrete_review {
                "Automatic review uses the degraded path"
            } else {
                "Automatic review is disabled"
            },
            theme,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(
        Line::styled(
            "Usage is reported separately for primary, subagent, and review roles.",
            Style::default().ink(theme.muted),
        )
        .centered(),
    );
    for warning in &roster.warnings {
        lines.push(Line::styled(
            format!("Warning: {warning}"),
            Style::default().ink(theme.warning),
        ));
    }
    lines
}

fn footer_line(
    screen: Screen,
    width: u16,
    scrollable: bool,
    theme: TerminalTheme,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut action = |key: &'static str, label: &'static str| {
        if !spans.is_empty() {
            spans.push(Span::styled("   ", Style::default()));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default()
                .ink(theme.selection_fg)
                .ink_bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().ink(theme.muted),
        ));
    };
    match screen {
        Screen::Welcome => {
            action("Enter", "Set up Mjolnir");
            action("Esc", "Quit");
        }
        Screen::WhatsNew => {
            action("Enter", "Continue");
            action("C", "Review setup");
            action("S", "Dismiss");
        }
        Screen::Connections => {
            action("↑↓", "Select");
            action("Enter", "Choose");
            if width >= 52 {
                action("C", "Customize");
            }
            if width >= 68 {
                action("R", "Recheck");
            }
            action("Esc", "Back");
        }
        Screen::Readiness => {
            action("Enter", "Start session");
            action("C", "Customize");
            if width >= 68 {
                action("R", "Recheck");
            }
            action("Esc", "Back");
        }
        Screen::Customize => {}
    }
    if scrollable {
        action("PgUp/PgDn", "Scroll");
    }
    Line::from(spans).centered()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::deepswe::Row;
    use crate::roster::{AdapterKind, AdapterLaunch, ModelChoice, ResolvedAgent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn role(model: &str, source_id: &str) -> ResolvedAgent {
        ResolvedAgent {
            model: Row {
                model: model.to_string(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: model.to_string(),
            launch: AdapterLaunch {
                kind: AdapterKind::Custom,
                source_id: source_id.to_string(),
                command: PathBuf::from(source_id),
                args: Vec::new(),
                env: Default::default(),
            },
            ranked: true,
            reasoning_effort: None,
        }
    }

    fn roster() -> Roster {
        let primary = role("gpt-test", "codex-acp");
        let worker = role("worker-test", "kimi");
        Roster {
            primary: primary.clone(),
            review_supervisor: Some(primary.clone()),
            subagent_default: Some(worker.clone()),
            available: vec![primary, worker],
            choices: vec![ModelChoice {
                model: "gpt-test".to_string(),
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
                available: true,
                disabled_reason: None,
                adapter: Some("codex-acp".to_string()),
                ranked: true,
            }],
            warnings: Vec::new(),
            inventory: AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
        }
    }

    #[test]
    fn fresh_recommended_path_constrains_every_seat_to_codex() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        assert_eq!(state.screen, Screen::Welcome);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
        assert_eq!(state.screen, Screen::Connections);
        state.selected = State::recommended_index();
        assert_eq!(state.handle_key(KeyCode::Enter), Action::UseRecommended);
        state.apply_recommended_setup();
        assert_eq!(
            state.config().agent.acp_source.as_deref(),
            Some("codex-acp")
        );
        assert_eq!(
            state.config().review.acp_source.as_deref(),
            Some("codex-acp")
        );
        assert_eq!(
            state.config().subagents.acp_source.as_deref(),
            Some("codex-acp")
        );
        assert!(state.config().agent.discrete_review);
        assert!(state.config().subagents.auto_failover);
        state.resolution_succeeded(roster());
        assert_eq!(state.screen, Screen::Readiness);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Finish);
    }

    #[test]
    fn customize_returns_to_readiness_without_losing_edits() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::Readiness);
        assert_eq!(state.handle_key(KeyCode::Char('c')), Action::None);
        assert_eq!(state.screen, Screen::Customize);
        state.editor.config.agent.discrete_review = false;
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
        state.resolution_succeeded(roster());
        assert!(!state.config().agent.discrete_review);
    }

    #[test]
    fn failed_validation_focuses_connection_recovery_and_retries() {
        let mut state = State::new(Kind::Fresh, Config::default(), None, None);
        state.resolution_failed("adapter missing");
        assert_eq!(state.screen, Screen::Connections);
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("repair"))
        );
        assert_eq!(state.handle_key(KeyCode::Char('r')), Action::Resolve);
    }

    #[test]
    fn failed_customization_invalidates_the_stale_upgrade_roster() {
        let mut state = State::new(Kind::Upgrade, Config::default(), Some(roster()), None);
        assert_eq!(state.handle_key(KeyCode::Char('c')), Action::None);
        state.editor.config.agent.acp_source = Some("missing-acp".to_string());
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);

        state.resolution_failed("adapter missing");

        assert!(state.roster.is_none());
        assert_eq!(state.screen, Screen::Connections);
        assert_eq!(state.handle_key(KeyCode::Esc), Action::None);
        assert_eq!(state.screen, Screen::WhatsNew);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
    }

    #[test]
    fn upgrade_education_is_versioned_separately_from_provider_setup() {
        let mut state = State::new(Kind::Upgrade, Config::default(), Some(roster()), None);
        assert_eq!(state.screen, Screen::WhatsNew);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Finish);
        assert_eq!(state.screen, Screen::WhatsNew);
    }

    #[test]
    fn fresh_connection_defaults_to_the_next_required_action() {
        assert_eq!(State::connection_selection_for_openai(false), 0);
        assert_eq!(
            State::connection_selection_for_openai(true),
            State::recommended_index()
        );
    }

    #[test]
    fn setup_notice_opens_connection_recovery_with_warning_focus() {
        let state = State::new(
            Kind::Fresh,
            Config::default(),
            Some(roster()),
            Some("provider route needs repair".to_string()),
        );

        assert_eq!(state.screen, Screen::Connections);
        assert_eq!(state.selected, State::default_connection_selection());
        assert!(state.reveal_selection);
        assert_eq!(state.notice_tone, NoticeTone::Warning);
    }

    #[test]
    fn visited_config_accepts_candidate_edits_and_marks_onboarding_complete() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.editor.config.agent.acp_source = Some("claude-acp".to_string());

        let visited = state.visited_config();

        assert_eq!(visited.onboarding_version, ONBOARDING_CONTENT_VERSION);
        assert_eq!(visited.agent.acp_source.as_deref(), Some("claude-acp"));
    }

    #[test]
    fn page_navigation_saturates_at_both_scroll_boundaries() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);

        assert_eq!(state.handle_key(KeyCode::PageUp), Action::None);
        assert_eq!(state.scroll, 0);
        assert_eq!(state.handle_key(KeyCode::End), Action::None);
        assert_eq!(state.scroll, u16::MAX);
        assert_eq!(state.handle_key(KeyCode::PageDown), Action::None);
        assert_eq!(state.scroll, u16::MAX);
        assert_eq!(state.handle_key(KeyCode::Home), Action::None);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn customize_cancel_restores_the_candidate_and_returns_to_its_origin() {
        let mut state = State::new(Kind::Upgrade, Config::default(), Some(roster()), None);
        let original = state.config().clone();
        assert_eq!(state.handle_key(KeyCode::Char('c')), Action::None);
        assert_eq!(state.screen, Screen::Customize);
        state.editor.config.agent.acp_source = Some("claude-acp".to_string());
        state.editor.config.agent.discrete_review = false;
        assert_eq!(state.handle_key(KeyCode::Esc), Action::None);
        assert_eq!(state.screen, Screen::WhatsNew);
        assert_eq!(state.config(), &original);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Finish);
    }

    #[test]
    fn upgrade_skip_records_latest_onboarding_without_changing_routes() {
        let mut config = Config {
            onboarding_version: 1,
            ..Config::default()
        };
        config.agent.acp_source = Some("claude-acp".to_string());
        let mut state = State::new(Kind::Upgrade, config, Some(roster()), None);

        assert_eq!(state.handle_key(KeyCode::Esc), Action::Skip);
        let visited = state.skipped_config();

        assert_eq!(visited.onboarding_version, ONBOARDING_CONTENT_VERSION);
        assert_eq!(visited.agent.acp_source.as_deref(), Some("claude-acp"));
    }

    #[test]
    fn upgrade_dismiss_after_failed_customization_preserves_original_routes() {
        let mut config = Config {
            onboarding_version: 1,
            ..Config::default()
        };
        config.agent.acp_source = Some("claude-acp".to_string());
        let mut state = State::new(Kind::Upgrade, config, Some(roster()), None);
        assert_eq!(state.handle_key(KeyCode::Char('c')), Action::None);
        state.editor.config.agent.acp_source = Some("missing-acp".to_string());
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
        state.resolution_failed("adapter missing");
        assert_eq!(state.handle_key(KeyCode::Esc), Action::None);
        assert_eq!(state.handle_key(KeyCode::Char('s')), Action::Skip);

        let skipped = state.skipped_config();
        assert_eq!(skipped.onboarding_version, ONBOARDING_CONTENT_VERSION);
        assert_eq!(skipped.agent.acp_source.as_deref(), Some("claude-acp"));
    }

    #[test]
    fn cancel_does_not_mutate_or_accept_the_config() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        assert_eq!(state.handle_key(KeyCode::Esc), Action::Cancel);
        assert_eq!(state.config().onboarding_version, 0);
    }

    #[test]
    fn narrow_welcome_wraps_and_remains_scrollable() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("draw");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("ONE REQUEST"), "rendered:\n{rendered}");
        assert!(rendered.contains("PgUp/PgDn"), "rendered:\n{rendered}");
        assert_eq!(state.handle_key(KeyCode::PageDown), Action::None);
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("redraw");
        assert!(state.scroll > 0);
    }

    #[test]
    fn narrow_connection_keeps_the_selected_action_and_scroll_help_visible() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::Connections);
        state.selected = State::recommended_index();
        state.reveal_selection = true;
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("draw");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Use Codex defaults"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("PgUp/PgDn"), "rendered:\n{rendered}");
        assert!(rendered.contains("Select"), "rendered:\n{rendered}");

        assert_eq!(state.handle_key(KeyCode::Down), Action::None);
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("redraw");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Customize every route"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn resize_reveals_the_selected_connection_action_again() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::Connections);
        state.selected = State::recommended_index();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("wide draw");
        assert!(!state.reveal_selection);

        terminal.backend_mut().resize(40, 12);
        state.terminal_resized();
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("narrow redraw");

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Use Codex defaults"),
            "rendered:\n{rendered}"
        );
    }

    #[test]
    fn common_width_connection_card_keeps_selection_and_scroll_help_visible() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::Connections);
        state.notice = Some(format!(
            "{} notice-tail",
            "provider validation failed with detailed context; ".repeat(8)
        ));
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Select"), "rendered:\n{rendered}");
        assert!(rendered.contains("PgUp/PgDn"), "rendered:\n{rendered}");
    }

    #[test]
    fn minimum_width_connection_footer_keeps_navigation_visible() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::Connections);
        state.notice = Some("route check needs attention".to_string());
        let backend = TestBackend::new(28, 14);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        for expected in ["Select", "Choose", "Back", "PgUp/PgDn"] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
    }

    #[test]
    fn long_setup_notice_remains_reachable_at_narrow_width() {
        let mut state = State::new(Kind::Fresh, Config::default(), None, None);
        state.screen = Screen::Connections;
        state.notice = Some(format!(
            "{} diagnostic-tail",
            "provider validation failed with detailed context; ".repeat(8)
        ));
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");

        state.handle_key(KeyCode::End);
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("diagnostic-tail"),
            "rendered:\n{rendered}"
        );
        assert!(state.scroll > 0);
    }

    #[test]
    fn zero_ready_inventory_is_not_rendered_as_success() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::Connections);
        let launch = role("gpt-test", "codex-acp").launch;
        state.inventory.servers = vec![crate::roster::AcpServerInfo {
            id: "codex-acp".to_string(),
            label: "Codex".to_string(),
            policy: crate::config::AcpServerPolicy::Enabled,
            detected: false,
            selected: true,
            evidence: "credentials missing".to_string(),
            launch,
            model_count: 0,
            error: None,
            installing: false,
            origin: None,
            session_config: Vec::new(),
            subscription: None,
        }];

        let text = connection_lines(&state, state.config().theme.palette())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("No launchable ACP runtime is ready yet"));
        assert!(text.contains("not detected: credentials missing"));
        assert!(!text.contains("✓ 0 runtime"));
    }

    #[test]
    fn readiness_names_every_role_and_review_policy() {
        let state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        let text = readiness_lines(&state, state.config().theme.palette())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "PRIMARY",
            "Implementation subagents",
            "CHECK",
            "reviewers available",
            // The default tier is named, so the readiness screen never implies
            // the expensive review is what will run.
            "Automatic quick review",
            "every changed turn",
            "primary, subagent, and review roles",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
    }

    #[test]
    fn wide_layout_caps_and_centers_the_setup_panel() {
        let panel = onboarding_panel_area(Rect::new(0, 0, 160, 40), 42);
        assert_eq!(panel.width, PANEL_MAX_WIDTH);
        assert_eq!(panel.height, PANEL_MAX_HEIGHT);
        assert_eq!(panel.x, 28);
        assert_eq!(panel.y, 5);
    }

    #[test]
    fn standard_layout_keeps_each_primary_action_visible() {
        for (kind, screen, expected) in [
            (Kind::Fresh, Screen::Welcome, "Set up Mjolnir"),
            (Kind::Fresh, Screen::Connections, "Use Codex defaults"),
            (Kind::Fresh, Screen::Readiness, "Start session"),
            (Kind::Upgrade, Screen::WhatsNew, "Continue"),
        ] {
            let mut state = State::new(kind, Config::default(), Some(roster()), None);
            state.screen = screen;
            let backend = TestBackend::new(100, 30);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| draw(frame, &mut state))
                .expect("draw");
            let rendered = terminal.backend().to_string();
            assert!(
                rendered.contains(expected),
                "missing {expected:?} on {screen:?}:\n{rendered}"
            );
            assert_eq!(state.scroll, 0, "{screen:?} should fit without scrolling");
        }
    }

    #[test]
    fn long_readiness_warning_remains_reachable_at_narrow_width() {
        let mut ready = roster();
        ready.warnings.push(format!(
            "{} warning-tail",
            "route fallback has detailed context; ".repeat(8)
        ));
        let mut state = State::new(Kind::Fresh, Config::default(), Some(ready), None);
        state.screen = Screen::Readiness;
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");

        state.handle_key(KeyCode::End);
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("draw");

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("warning-tail"), "rendered:\n{rendered}");
        assert!(state.scroll > 0);
    }

    #[test]
    fn upgrade_card_shows_the_current_routes() {
        let state = State::new(Kind::Upgrade, Config::default(), Some(roster()), None);
        let text = whats_new_lines(&state, state.config().theme.palette())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in ["YOUR CURRENT SETUP", "gpt-test", "worker-test", "codex-acp"] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
    }
}
