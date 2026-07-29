//! First-startup host for the shared settings editor.

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::roster::Roster;
use crate::settings::{SettingsAction, SettingsEditor, draw_settings_panel};
use crate::term::TrackedBackend;

#[derive(Debug)]
pub enum Outcome {
    Accept(Box<Config>),
    Cancel,
}

pub async fn run(
    terminal: &mut Terminal<TrackedBackend<Stdout>>,
    config: Config,
    roster: Option<Roster>,
    notice: Option<String>,
    termination: CancellationToken,
) -> Result<Outcome> {
    let inventory = roster
        .as_ref()
        .map(|roster| roster.inventory.clone())
        .unwrap_or_else(|| crate::roster::discover_inventory(&config));
    let choices = roster
        .as_ref()
        .map(|roster| roster.choices.clone())
        .unwrap_or_default();
    let mut editor = SettingsEditor::new(config, choices, notice).with_inventory(inventory);
    if let Some(roster) = roster {
        editor = editor.with_active_models(crate::config::ModelsConfig {
            primary: roster.primary.model.model,
            primary_source: Some(roster.primary.launch.source_id),
            subagent: roster
                .subagent_default
                .as_ref()
                .map(|role| role.model.model.clone())
                .unwrap_or_else(|| "off".to_string()),
            subagent_source: roster.subagent_default.map(|role| role.launch.source_id),
        });
    }
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    terminal
        .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "Welcome to Mjolnir"))?;
    loop {
        tokio::select! {
            biased;
            _ = termination.cancelled() => return Ok(Outcome::Cancel),
            event = events.next() => {
                let Some(event) = event else {
                    return Ok(Outcome::Cancel);
                };
                if let Some(action) = handle_event(&mut editor, event.context("settings event")?) {
                    match action {
                        SettingsAction::Save => return Ok(Outcome::Accept(Box::new(editor.config.clone()))),
                        SettingsAction::Cancel => {
                            editor.cancel_background();
                            return Ok(Outcome::Cancel);
                        }
                        SettingsAction::Authenticate(vendor) => {
                            if vendor == crate::auth::AuthVendor::OpenAi
                                && crate::auth::executable(vendor).is_none()
                            {
                                editor.notice = Some(format!(
                                    "{} CLI is not installed; run `{}`",
                                    vendor.label(),
                                    crate::auth::install_hint(vendor)
                                ));
                            } else {
                                crate::ui::restore_terminal_for_auth(terminal, crate::ui::UiMode::FullscreenTui)?;
                                let login = crate::auth::run_login(vendor).await;
                                crate::ui::resume_terminal_after_auth(terminal, crate::ui::UiMode::FullscreenTui)?;
                                let notice = login.unwrap_or_else(|error| format!("Sign-in failed: {error:#}"));
                                editor.refresh_after_auth(notice);
                            }
                        }
                        SettingsAction::None | SettingsAction::Changed => {}
                    }
                }
            }
            _ = tick.tick() => editor.poll_background(),
        }
        terminal.draw(|frame| {
            draw_settings_panel(frame, frame.area(), &editor, "Welcome to Mjolnir")
        })?;
    }
}

fn handle_event(editor: &mut SettingsEditor, event: CtEvent) -> Option<SettingsAction> {
    let CtEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
        editor.cancel_background();
        return Some(SettingsAction::Cancel);
    }
    Some(editor.handle_key(key.code))
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
        Roster {
            primary: primary.clone(),
            subagent_default: None,
            available: vec![primary],
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
            inventory: crate::roster::AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
        }
    }

    #[test]
    fn startup_uses_shared_settings_panel() {
        let editor = SettingsEditor::new(Config::default(), roster().choices, None);
        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw_settings_panel(frame, frame.area(), &editor, "Welcome to Mjolnir"))
            .expect("draw");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Welcome to Mjolnir"));
        assert!(rendered.contains("Agents"));
        assert!(rendered.contains("primary model; plans, implements, and answers"));
        assert!(rendered.contains("Enter save"));
    }
}
