//! Startup configuration using the same editor available from a session.

use std::io::Stdout;
use std::path::Path;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::layout::Alignment;
use ratatui::widgets::{Paragraph, Wrap};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::config::TeamPreset;
use crate::config::{Config, ONBOARDING_CONTENT_VERSION};
use crate::roster::Roster;
use crate::settings::{
    SETTINGS_PANEL_MIN_HEIGHT, SETTINGS_PANEL_MIN_WIDTH, SettingsAction, SettingsEditor,
    SettingsTab, draw_settings_panel,
};
use crate::term::TrackedBackend;

const TEAM_SELECTION_REQUIRED: &str = "Choose one of the four Mjolnir Teams before saving.";

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

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Cancel,
    Resolve,
    Authenticate(crate::auth::AuthVendor),
}

struct State {
    kind: Kind,
    editor: SettingsEditor,
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
        let opens_recovery = notice.is_some();
        let notice = notice.or_else(|| {
            (!crate::config::has_valid_team(&config)).then(|| TEAM_SELECTION_REQUIRED.to_string())
        });
        let mut editor = SettingsEditor::new(config, choices, notice).with_inventory(inventory);
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
        if opens_recovery {
            editor.tab = SettingsTab::AcpServers;
            editor.selected = 0;
        }
        Self { kind, editor }
    }

    fn visited_config(&self) -> Config {
        let mut config = self.editor.config.clone();
        config.onboarding_version = ONBOARDING_CONTENT_VERSION;
        config
    }

    fn handle_key(&mut self, code: KeyCode) -> Action {
        match self.editor.handle_key(code) {
            SettingsAction::Save if !crate::config::has_valid_team(&self.editor.config) => {
                self.editor.tab = SettingsTab::Team;
                self.editor.selected = 0;
                self.editor.notice = Some(TEAM_SELECTION_REQUIRED.to_string());
                Action::None
            }
            SettingsAction::Save => Action::Resolve,
            SettingsAction::Cancel => Action::Cancel,
            SettingsAction::Authenticate(vendor) => Action::Authenticate(vendor),
            SettingsAction::None | SettingsAction::Changed => Action::None,
        }
    }

    fn resolution_failed(&mut self, error: impl std::fmt::Display) {
        self.editor.tab = SettingsTab::AcpServers;
        self.editor.selected = 0;
        self.editor.refresh_after_auth(format!(
            "No launchable route yet: {error}. Sign in or adjust ACP server settings, then save again."
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
    terminal.draw(|frame| draw(frame, &state))?;
    loop {
        tokio::select! {
            biased;
            _ = termination.cancelled() => {
                return Ok(Outcome::Cancel);
            },
            event = events.next() => {
                let Some(event) = event else {
                    return Ok(Outcome::Cancel);
                };
                let event = event.context("onboarding event")?;
                let CtEvent::Key(key) = event else {
                    terminal.draw(|frame| draw(frame, &state))?;
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
                    return Ok(Outcome::Cancel);
                }
                match state.handle_key(key.code) {
                    Action::None => {}
                    Action::Cancel => return Ok(Outcome::Cancel),
                    Action::Authenticate(vendor) => {
                        crate::ui::restore_terminal_for_auth(terminal)?;
                        let login = crate::auth::run_login(vendor).await;
                        crate::ui::resume_terminal_after_auth(terminal)?;
                        let notice = match login {
                            Ok(outcome) => outcome.into_message(),
                            Err(error) => format!("Sign-in failed: {error:#}"),
                        };
                        state.editor.refresh_after_auth(notice);
                    }
                    Action::Resolve => {
                        state.editor.notice = Some("Checking provider routes and models...".to_string());
                        terminal.draw(|frame| draw(frame, &state))?;
                        match crate::roster::resolve(&state.editor.config, cwd).await {
                            Ok(roster) => {
                                return Ok(Outcome::Accept(
                                    Box::new(state.visited_config()),
                                    Box::new(roster),
                                ));
                            }
                            Err(error) => state.resolution_failed(format!("{error:#}")),
                        }
                    }
                }
            }
        }
        terminal.draw(|frame| draw(frame, &state))?;
    }
}

fn draw(frame: &mut ratatui::Frame, state: &State) {
    let title = match state.kind {
        Kind::Fresh => "Set up Mjolnir Teams",
        Kind::Upgrade => "Configure Mjolnir Teams",
    };
    let area = frame.area();
    if area.width < SETTINGS_PANEL_MIN_WIDTH || area.height < SETTINGS_PANEL_MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "{title}\n\nTerminal too small\nResize to at least {SETTINGS_PANEL_MIN_WIDTH} x {SETTINGS_PANEL_MIN_HEIGHT}\n\nEsc cancel"
            ))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    draw_settings_panel(frame, frame.area(), &state.editor, title);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::deepswe::Row;
    use crate::roster::{AcpInventory, AdapterKind, AdapterLaunch, ModelChoice, ResolvedAgent};

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
                kind: AdapterKind::from_source_id(source_id).unwrap_or(AdapterKind::Claude),
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
        let worker = role("worker-test", "opencode");
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

    fn render(state: &State) -> String {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, state))
            .expect("draw onboarding");
        terminal.backend().to_string()
    }

    #[test]
    fn onboarding_opens_the_standard_configuration_editor_on_the_team_tab() {
        let state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);

        assert_eq!(state.editor.tab, SettingsTab::Team);
        let rendered = render(&state);
        for expected in [
            "Set up Mjolnir Teams",
            "Team",
            "Reviewer",
            "Subagents",
            "ACP Servers",
            "Appearance",
            "automatically reviews generated code",
            "Mix Codex and Claude",
            "Auto models can reduce review cost",
            "Recommended team",
            "Extended review",
            "Luna xhigh",
            "Choose one of the four Mjolnir Teams before saving",
            "Enter save",
            "Esc cancel",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
        for removed in ["Dismiss", "Review setup", "Keep current", "Start session"] {
            assert!(
                !rendered.contains(removed),
                "obsolete action {removed:?} remains:\n{rendered}"
            );
        }
    }

    #[test]
    fn team_selection_uses_the_standard_settings_controls() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);

        assert_eq!(TeamPreset::from_config(&state.editor.config), None);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
        assert_eq!(state.editor.tab, SettingsTab::Team);
        assert_eq!(state.handle_key(KeyCode::Right), Action::None);
        assert_eq!(
            TeamPreset::from_config(&state.editor.config),
            Some(TeamPreset::Codex)
        );
        assert_eq!(state.handle_key(KeyCode::Right), Action::None);
        assert_eq!(
            TeamPreset::from_config(&state.editor.config),
            Some(TeamPreset::Claude)
        );
        assert_eq!(state.handle_key(KeyCode::Tab), Action::None);
        assert_eq!(state.editor.tab, SettingsTab::Reviewer);
    }

    #[test]
    fn a_defaulted_team_is_preselected_without_demanding_a_choice() {
        // Startup applies the default team on a machine signed in to both
        // providers, so setup opens on that team with nothing to answer.
        let mut config = Config::default();
        TeamPreset::ClaudeWithCodexReviewer.apply(&mut config);
        let mut state = State::new(Kind::Fresh, config, Some(roster()), None);

        assert_eq!(state.editor.notice, None);
        let rendered = render(&state);
        assert!(
            rendered.contains("Team  < Claude coder + Codex reviewer >"),
            "{rendered}"
        );
        assert!(!rendered.contains(TEAM_SELECTION_REQUIRED), "{rendered}");
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
    }

    #[test]
    fn enter_saves_and_validates_the_complete_configuration() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        TeamPreset::Claude.apply(&mut state.editor.config);

        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
        let visited = state.visited_config();
        assert_eq!(visited.onboarding_version, ONBOARDING_CONTENT_VERSION);
        assert_eq!(visited.agent.acp_source.as_deref(), Some("claude-acp"));
    }

    #[test]
    fn upgrade_with_custom_routing_requires_a_new_team_selection() {
        let mut config = Config {
            onboarding_version: 1,
            ..Config::default()
        };
        config.agent.acp_source = Some("custom-agent".to_string());
        config.review.acp_source = Some("custom-reviewer".to_string());
        config.subagents.acp_source = Some("custom-agent".to_string());
        let mut state = State::new(Kind::Upgrade, config, Some(roster()), None);

        assert_eq!(TeamPreset::from_config(&state.editor.config), None);
        assert_eq!(state.handle_key(KeyCode::Enter), Action::None);
        assert_eq!(state.editor.tab, SettingsTab::Team);
        assert_eq!(state.handle_key(KeyCode::Right), Action::None);
        assert_eq!(
            TeamPreset::from_config(&state.editor.config),
            Some(TeamPreset::Codex)
        );
        assert_eq!(state.handle_key(KeyCode::Enter), Action::Resolve);
    }

    #[test]
    fn fresh_cancel_aborts_without_accepting_the_configuration() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);

        assert_eq!(state.handle_key(KeyCode::Esc), Action::Cancel);
        assert_eq!(state.editor.config.onboarding_version, 0);
    }

    #[test]
    fn upgrade_cancel_does_not_accept_or_mark_the_update_complete() {
        let mut config = Config {
            onboarding_version: 1,
            ..Config::default()
        };
        config.agent.acp_source = Some("claude-acp".to_string());
        let mut state = State::new(Kind::Upgrade, config, Some(roster()), None);
        state.editor.config.agent.acp_source = Some("codex-acp".to_string());

        assert_eq!(state.handle_key(KeyCode::Esc), Action::Cancel);
        assert_eq!(state.editor.config.onboarding_version, 1);
    }

    #[test]
    fn undersized_terminal_shows_resize_instructions_and_cancel_action() {
        let state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);
        let backend = TestBackend::new(SETTINGS_PANEL_MIN_WIDTH - 1, SETTINGS_PANEL_MIN_HEIGHT - 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &state))
            .expect("draw onboarding fallback");

        let rendered = terminal.backend().to_string();
        for expected in [
            "Terminal too small",
            "Resize to at least 28 x 12",
            "Esc cancel",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
    }

    #[test]
    fn setup_notice_opens_standard_connection_recovery() {
        let state = State::new(
            Kind::Fresh,
            Config::default(),
            Some(roster()),
            Some("provider route needs repair".to_string()),
        );

        assert_eq!(state.editor.tab, SettingsTab::AcpServers);
        assert_eq!(state.editor.selected, 0);
        assert_eq!(
            state.editor.notice.as_deref(),
            Some("provider route needs repair")
        );
    }

    #[test]
    fn failed_validation_stays_in_the_editor_on_connection_recovery() {
        let mut state = State::new(Kind::Fresh, Config::default(), Some(roster()), None);

        state.resolution_failed("adapter missing");

        assert_eq!(state.editor.tab, SettingsTab::AcpServers);
        assert_eq!(state.editor.selected, 0);
        assert!(
            state
                .editor
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("adapter missing"))
        );
    }

    #[test]
    fn account_row_uses_the_standard_sign_in_action() {
        let mut state = State::new(
            Kind::Fresh,
            Config::default(),
            Some(roster()),
            Some("sign in".to_string()),
        );

        assert_eq!(
            state.handle_key(KeyCode::Enter),
            Action::Authenticate(crate::auth::AuthVendor::OpenAi)
        );
    }
}
