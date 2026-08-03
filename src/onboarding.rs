//! Guided first-run and major-upgrade product onboarding.

use std::io::Stdout;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ONBOARDING_CONTENT_VERSION};
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
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Welcome,
    WhatsNew,
    Connections,
    ChoosePath,
    Customize,
    Readiness,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Cancel,
    Resolve,
    Authenticate(crate::auth::AuthVendor),
    Finish,
}

struct State {
    kind: Kind,
    screen: Screen,
    editor: SettingsEditor,
    roster: Option<Roster>,
    inventory: AcpInventory,
    selected: usize,
    scroll: u16,
    notice: Option<String>,
}

impl State {
    fn new(kind: Kind, config: Config, roster: Option<Roster>, notice: Option<String>) -> Self {
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
        Self {
            kind,
            screen,
            editor,
            roster,
            inventory,
            selected: 0,
            scroll: 0,
            notice,
        }
    }

    fn config(&self) -> &Config {
        &self.editor.config
    }

    fn change_screen(&mut self, screen: Screen) {
        self.screen = screen;
        self.selected = 0;
        self.scroll = 0;
        self.notice = None;
    }

    fn connection_item_count(&self) -> usize {
        crate::auth::AuthVendor::ALL.len() + 1
    }

    fn move_selected(&mut self, delta: i32, len: usize) {
        if len > 0 {
            self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
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
                        self.change_screen(Screen::Readiness);
                        Action::None
                    } else {
                        Action::Resolve
                    }
                }
                KeyCode::Char('c' | 'C') => {
                    self.change_screen(Screen::ChoosePath);
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
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('n') => {
                    self.change_screen(Screen::ChoosePath);
                    Action::None
                }
                KeyCode::Char('r' | 'R') => Action::Resolve,
                KeyCode::Char('c' | 'C') => {
                    self.change_screen(Screen::Customize);
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
            Screen::ChoosePath => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_selected(-1, 2);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_selected(1, 2);
                    Action::None
                }
                KeyCode::Enter if self.selected == 0 => Action::Resolve,
                KeyCode::Enter => {
                    self.change_screen(Screen::Customize);
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
            Screen::Customize => match self.editor.handle_key(code) {
                SettingsAction::Save => Action::Resolve,
                SettingsAction::Cancel => {
                    self.change_screen(Screen::ChoosePath);
                    Action::None
                }
                SettingsAction::Authenticate(vendor) => Action::Authenticate(vendor),
                SettingsAction::None | SettingsAction::Changed => Action::None,
            },
            Screen::Readiness => match code {
                KeyCode::Enter => Action::Finish,
                KeyCode::Char('c' | 'C') => {
                    self.change_screen(Screen::Customize);
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
                    self.change_screen(Screen::ChoosePath);
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
        self.notice = Some(format!(
            "No launchable route yet: {error}. Sign in or repair a connection, then press R to retry."
        ));
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
                let CtEvent::Key(key) = event.context("onboarding event")? else {
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
                    Action::Finish => {
                        let Some(roster) = state.roster.take() else {
                            state.resolution_failed("readiness was not resolved");
                            terminal.draw(|frame| draw(frame, &mut state))?;
                            continue;
                        };
                        let mut config = state.editor.config.clone();
                        config.onboarding_version = ONBOARDING_CONTENT_VERSION;
                        return Ok(Outcome::Accept(Box::new(config), Box::new(roster)));
                    }
                    Action::Authenticate(vendor) => {
                        let notice = if crate::auth::executable(vendor).is_none() {
                            format!(
                                "{} CLI is not installed. Run `{}` and retry.",
                                vendor.label(),
                                crate::auth::install_hint(vendor)
                            )
                        } else {
                            crate::ui::restore_terminal_for_auth(terminal, crate::ui::UiMode::FullscreenTui)?;
                            let login = crate::auth::run_login(vendor).await;
                            crate::ui::resume_terminal_after_auth(terminal, crate::ui::UiMode::FullscreenTui)?;
                            login.unwrap_or_else(|error| format!("Sign-in failed: {error:#}"))
                        };
                        if state.screen == Screen::Customize {
                            state.editor.refresh_after_auth(notice);
                        } else {
                            state.notice = Some(notice);
                        }
                    }
                    Action::Resolve => {
                        state.notice = Some("Checking provider routes and role readiness…".to_string());
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

fn draw(frame: &mut ratatui::Frame, state: &mut State) {
    if state.screen == Screen::Customize {
        draw_settings_panel(frame, frame.area(), &state.editor, "Customize Mjolnir");
        return;
    }
    let theme = state.config().theme.palette();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(format!(" {} | guided setup ", mjolnir_version_label()))
            .style(Style::default().add_modifier(Modifier::REVERSED)),
        rows[0],
    );
    let (title, mut lines) = screen_lines(state, theme);
    if let Some(notice) = &state.notice {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Setup status",
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            notice.clone(),
            Style::default().fg(theme.warning),
        )));
    }
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(rows[1]);
    frame.render_widget(block.style(Style::default().fg(theme.primary)), rows[1]);
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height))
        .min(u16::MAX as usize) as u16;
    state.scroll = state.scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((state.scroll, 0)), inner);
    frame.render_widget(
        Paragraph::new(footer(state.screen)).style(Style::default().fg(theme.muted)),
        rows[2],
    );
}

fn heading(text: impl Into<String>, theme: TerminalTheme) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))
}

fn selected_line(selected: bool, label: &str, detail: &str, theme: TerminalTheme) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        Style::default()
            .fg(theme.selection_fg)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    Line::from(Span::styled(format!("{marker}{label} — {detail}"), style))
}

fn screen_lines(state: &State, theme: TerminalTheme) -> (&'static str, Vec<Line<'static>>) {
    match state.screen {
        Screen::Welcome => (" How Mjolnir works ", welcome_lines(theme)),
        Screen::WhatsNew => (" What changed ", whats_new_lines(theme)),
        Screen::Connections => (" Connect a provider ", connection_lines(state, theme)),
        Screen::ChoosePath => (" Choose your setup ", choose_path_lines(state, theme)),
        Screen::Readiness => (" Ready to start ", readiness_lines(state, theme)),
        Screen::Customize => unreachable!("custom settings draw separately"),
    }
}

fn welcome_lines(theme: TerminalTheme) -> Vec<Line<'static>> {
    vec![
        heading("1. The primary owns your request", theme),
        Line::raw(
            "The primary keeps the conversation, overall plan, verification, corrections, and final answer. It decides what to do directly and what bounded work to delegate, then integrates the evidence that comes back.",
        ),
        Line::raw(""),
        heading("2. Implementation subagents do bounded work", theme),
        Line::raw(
            "Subagents start fresh asynchronous sessions from standalone briefs. They can investigate, edit the authorized workspace, and test in parallel when tasks do not overlap. They may use different models or providers and report back; the primary must still verify their work.",
        ),
        Line::raw(""),
        heading("3. The review team checks every changed turn", theme),
        Line::raw(
            "When automatic review is enabled, any completed turn that changed the workspace is reviewable after writers drain—even when the primary made every edit. A visible intent analyst reconstructs the contract; a supervisor on the primary route selects useful read-only specialists, vets their reports, and returns one verdict. Surviving findings go to the primary for correction, and changed corrections receive a focused delta review.",
        ),
        Line::raw(""),
        heading("Routing and usage", theme),
        Line::raw(
            "Primary, subagent, and review work are accounted as separate seats. Delegation can preserve scarce primary context and quota by moving bounded work to cheaper or available workers; parallel work and independent review can also increase total work performed. Multiple providers expand capacity, choice, failover, cost options, and possible diversity, but do not guarantee that every role uses a different provider.",
        ),
    ]
}

fn whats_new_lines(theme: TerminalTheme) -> Vec<Line<'static>> {
    vec![
        heading("Mjolnir now uses three explicit roles", theme),
        Line::raw(
            "The former Council model is now a primary agent, a pool of write-capable implementation subagents, and a separate read-only automatic review team.",
        ),
        Line::raw(""),
        heading("Review is turn-based, not delegation-based", theme),
        Line::raw(
            "Every changed turn can be reviewed when review is enabled. The primary model supervises selective specialist sessions, verifies findings, corrects surviving problems, and receives a focused re-review when corrections change the workspace.",
        ),
        Line::raw(""),
        heading("Your existing providers and settings stay in place", theme),
        Line::raw(
            "This explanation has its own content version, separate from the config schema. Continue to review the resolved routes, or press C to customize them.",
        ),
    ]
}

fn connection_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(
            "One working connection is enough to start. Additional connections are optional and add model choice, workload capacity, quota failover, cost options, and possible provider diversity.",
        ),
        Line::raw(""),
        heading("Account actions", theme),
    ];
    for (index, vendor) in crate::auth::AuthVendor::ALL.iter().copied().enumerate() {
        lines.push(selected_line(
            state.selected == index,
            vendor.label(),
            &format!(
                "{}; provides {}",
                crate::auth::detect(vendor).status(),
                vendor.enables()
            ),
            theme,
        ));
    }
    lines.push(selected_line(
        state.selected == crate::auth::AuthVendor::ALL.len(),
        "Continue",
        "choose recommended setup or open full settings",
        theme,
    ));
    lines.push(Line::raw(""));
    lines.push(heading("Detected runtimes", theme));
    if state.inventory.servers.is_empty() {
        lines.push(Line::raw("No ACP runtimes detected yet."));
    } else {
        for server in &state.inventory.servers {
            let status = if server.installing {
                "installing".to_string()
            } else if let Some(error) = &server.error {
                format!("needs repair: {error}")
            } else if server.model_count > 0 {
                format!("ready; {} model route(s)", server.model_count)
            } else if server.detected {
                "detected; no launchable model route".to_string()
            } else {
                "not detected".to_string()
            };
            lines.push(Line::raw(format!(
                "  {} ({}) — {status}",
                server.label, server.id
            )));
        }
    }
    lines
}

fn choose_path_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    vec![
        Line::raw(
            "You can start from Mjolnir's automatic routing and current review/failover defaults, or inspect every model and connection setting.",
        ),
        Line::raw(""),
        selected_line(
            state.selected == 0,
            "Use recommended setup",
            "automatic primary and worker routing; review and failover enabled",
            theme,
        ),
        selected_line(
            state.selected == 1,
            "Customize",
            "open full model, provider, review, parallelism, and appearance settings",
            theme,
        ),
    ]
}

fn readiness_lines(state: &State, theme: TerminalTheme) -> Vec<Line<'static>> {
    let Some(roster) = state.roster.as_ref() else {
        return vec![Line::raw(
            "Routes have not been resolved. Press R to retry.",
        )];
    };
    let mut lines = vec![
        heading("Primary", theme),
        Line::raw(format!(
            "{} via {} — owns coordination, verification, correction, and the final answer",
            roster.primary.model.model, roster.primary.launch.source_id
        )),
        Line::raw(""),
        heading("Implementation subagents", theme),
    ];
    if let Some(worker) = &roster.subagent_default {
        let alternatives = roster.subagent_failover_roles().len().saturating_sub(1);
        lines.push(Line::raw(format!(
            "{} via {}; up to {} parallel; {} failover alternative(s)",
            worker.model.model,
            worker.launch.source_id,
            state.config().subagents.max_parallel,
            alternatives
        )));
    } else {
        lines.push(Line::raw(
            "disabled; the primary performs implementation work directly",
        ));
    }
    lines.push(Line::raw(""));
    lines.push(heading("Review supervisor", theme));
    if let Some(supervisor) = &roster.review_supervisor {
        lines.push(Line::raw(format!(
            "{} via {}; supervises intent-aware review",
            supervisor.model.model, supervisor.launch.source_id
        )));
    } else {
        lines.push(Line::raw(
            "no dedicated supervisor route; review uses the degraded primary-only path",
        ));
    }
    lines.push(Line::raw(""));
    lines.push(heading("Specialist reviewers", theme));
    lines.push(Line::raw(if roster.subagent_default.is_some() {
        "worker pool available for selective read-only specialists"
    } else {
        "no worker pool; review falls back without specialist fan-out"
    }));
    lines.push(Line::raw(""));
    lines.push(heading("Automatic review", theme));
    lines.push(Line::raw(if state.config().agent.discrete_review {
        "enabled for every changed turn after write-capable workers drain"
    } else {
        "disabled"
    }));
    lines.push(Line::raw(
        "Usage is reported separately for primary, subagent, and review seats.",
    ));
    for warning in &roster.warnings {
        lines.push(Line::raw(format!("Warning: {warning}")));
    }
    lines
}

fn footer(screen: Screen) -> &'static str {
    match screen {
        Screen::Welcome => "Enter/N continue | Up/Down/PgUp/PgDn scroll | Esc exit without saving",
        Screen::WhatsNew => {
            "Enter review readiness | C customize | scroll keys | Esc skip without saving"
        }
        Screen::Connections => {
            "Up/Down select | Enter sign in/continue | C full settings | R retry | Esc back"
        }
        Screen::ChoosePath => "Up/Down select | Enter choose | Esc back",
        Screen::Customize => "",
        Screen::Readiness => "Enter Start session | C customize | R retry | scroll keys | Esc back",
    }
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
        let worker = role("worker-test", "anvil");
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
        }
    }

    #[test]
    fn fresh_recommended_path_explains_roles_before_resolution() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        assert_eq!(state.screen, Screen::Welcome);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
        assert_eq!(state.screen, Screen::Connections);
        state.selected = crate::auth::AuthVendor::ALL.len();
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
        assert_eq!(state.screen, Screen::ChoosePath);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
        state.resolution_succeeded(roster());
        assert_eq!(state.screen, Screen::Readiness);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Finish);
    }

    #[test]
    fn customize_returns_to_readiness_without_losing_edits() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        state.change_screen(Screen::ChoosePath);
        state.selected = 1;
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
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
    fn upgrade_education_is_versioned_separately_from_provider_setup() {
        let mut state = State::new(Kind::Upgrade, Config::default(), Some(roster()), None);
        assert_eq!(state.screen, Screen::WhatsNew);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
        assert_eq!(state.screen, Screen::Readiness);
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
        assert!(rendered.contains("primary owns"), "rendered:\n{rendered}");
        assert_eq!(state.handle_key(KeyCode::PageDown), Action::None);
        terminal
            .draw(|frame| draw(frame, &mut state))
            .expect("redraw");
        assert!(state.scroll > 0);
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
    fn readiness_names_every_role_and_review_policy() {
        let state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        let text = readiness_lines(&state, state.config().theme.palette())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Primary",
            "Implementation subagents",
            "Review supervisor",
            "Specialist reviewers",
            "every changed turn",
            "primary, subagent, and review seats",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
    }
}
