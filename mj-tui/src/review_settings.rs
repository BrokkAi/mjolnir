//! The global review settings editor.
//!
//! This dialog owns only the in-memory draft of [`HelConfig::review`]. The
//! controller performs discovery and persistence off the event loop; replies
//! carry a generation so a slow probe can never replace a newer choice.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hel::hel_acp::SessionConfigChoice;
use hel::hel_config::{HelConfig, ReviewConfig};
use hel::hel_review::lanes::ReviewTier;
use mj_chat::hel_selection::FrameSurfaces;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::widgets::{centered_modal, focused_buttons, popup_height};
use crate::{DashboardAction, DashboardState, Mode, cycle_control};

/// A readiness observation for one actual review execution target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTargetReadiness {
    pub target: String,
    pub ready: bool,
    pub message: String,
}

/// Capabilities returned by a background review probe.
///
/// The choices come from the harness adapter. The UI adds the explicit
/// profile-default row while rendering, so it never invents a model or effort
/// accepted by a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSettingsProbeResult {
    pub model_choices: Vec<SessionConfigChoice>,
    pub effort_choices: Vec<SessionConfigChoice>,
    pub targets: Vec<ReviewTargetReadiness>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewSettingsFocus {
    Enabled,
    Tier,
    Profile,
    Model,
    Effort,
    Cancel,
    Save,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewProbeRefresh {
    Profile,
    Model,
    Effort,
}

const REVIEW_SETTINGS_FOCUS: [ReviewSettingsFocus; 7] = [
    ReviewSettingsFocus::Enabled,
    ReviewSettingsFocus::Tier,
    ReviewSettingsFocus::Profile,
    ReviewSettingsFocus::Model,
    ReviewSettingsFocus::Effort,
    ReviewSettingsFocus::Cancel,
    ReviewSettingsFocus::Save,
];
const REVIEW_SETTINGS_BUTTONS: &[&str] = &["Cancel", "Save"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewReadiness {
    /// No probe has run for the current profile and selector draft.
    Unknown(String),
    Loading,
    Verified(Vec<ReviewTargetReadiness>),
    /// There is no connected target to verify. This is a valid save state, but
    /// the dialog tells the user how to obtain a concrete result.
    Unverified(String),
    Invalid(String),
    Failed(String),
}

impl ReviewReadiness {
    fn label(&self) -> String {
        match self {
            Self::Unknown(message) => format!("unverified: {message}"),
            Self::Loading => "checking actual targets…".to_owned(),
            Self::Verified(targets) => {
                let ready = targets.iter().filter(|target| target.ready).count();
                format!("verified ({ready}/{} targets ready)", targets.len())
            }
            Self::Unverified(message) => format!("unverified: {message}"),
            Self::Invalid(message) => format!("cannot use these settings: {message}"),
            Self::Failed(message) => format!("check failed: {message}"),
        }
    }
}

/// The editable global review form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewSettingsDialog {
    pub(crate) review: ReviewConfig,
    pub(crate) profiles: Vec<Option<String>>,
    pub(crate) focus: ReviewSettingsFocus,
    pub(crate) model_choices: Vec<SessionConfigChoice>,
    pub(crate) effort_choices: Vec<SessionConfigChoice>,
    /// Whether a successful adapter probe supplied the choices currently
    /// shown. An empty list after a completed probe still differs from a
    /// selector that has not been checked yet.
    pub(crate) model_capabilities_discovered: bool,
    pub(crate) effort_capabilities_discovered: bool,
    pub(crate) target_readiness: Vec<ReviewTargetReadiness>,
    pub(crate) readiness: ReviewReadiness,
    pub(crate) generation: u64,
    pub(crate) probing: bool,
    pub(crate) saving: bool,
    pub(crate) save_error: Option<String>,
    pub(crate) read_only_reason: Option<String>,
}

impl ReviewSettingsDialog {
    fn new(config: &HelConfig) -> Self {
        let mut profiles = vec![None];
        profiles.extend(config.profiles.keys().cloned().map(Some));
        let profile = config.review.profile.as_deref();
        let readiness = if profile.is_none() {
            ReviewReadiness::Unverified(
                "select a reviewer profile, then start a session to verify it".to_owned(),
            )
        } else {
            ReviewReadiness::Unknown("checking the selected reviewer profile…".to_owned())
        };
        Self {
            review: config.review.clone(),
            profiles,
            focus: ReviewSettingsFocus::Enabled,
            model_choices: Vec::new(),
            effort_choices: Vec::new(),
            model_capabilities_discovered: false,
            effort_capabilities_discovered: false,
            target_readiness: Vec::new(),
            readiness,
            generation: 0,
            probing: false,
            saving: false,
            save_error: None,
            read_only_reason: config.newer_build_notice(),
        }
    }

    fn profile_index(&self) -> usize {
        self.profiles
            .iter()
            .position(|profile| profile.as_deref() == self.review.profile.as_deref())
            .unwrap_or(0)
    }

    fn profile_label(&self) -> String {
        self.review
            .profile
            .clone()
            .unwrap_or_else(|| "No reviewer profile".to_owned())
    }

    fn value_label(
        value: Option<&str>,
        choices: &[SessionConfigChoice],
        capabilities_discovered: bool,
    ) -> String {
        let Some(value) = value else {
            return "Profile default".to_owned();
        };
        choices
            .iter()
            .find(|choice| choice.value == value)
            .map(|choice| choice.name.clone())
            .unwrap_or_else(|| {
                let state = if capabilities_discovered {
                    "unavailable"
                } else {
                    "unverified"
                };
                format!("{value} ({state})")
            })
    }

    fn choice_values(value: Option<&str>, choices: &[SessionConfigChoice]) -> Vec<Option<String>> {
        let mut values = vec![None];
        values.extend(choices.iter().map(|choice| Some(choice.value.clone())));
        if let Some(value) = value
            && !values
                .iter()
                .any(|candidate| candidate.as_deref() == Some(value))
        {
            // Keep an invalid value in the form until the user explicitly
            // changes it. A refresh must never silently pick a new model.
            values.push(Some(value.to_owned()));
        }
        values
    }

    fn select_value(
        current: Option<&str>,
        choices: &[SessionConfigChoice],
        delta: isize,
    ) -> Option<String> {
        let values = Self::choice_values(current, choices);
        let index = values
            .iter()
            .position(|candidate| candidate.as_deref() == current)
            .unwrap_or(0);
        values
            .get(cycle_index(index, values.len(), delta))
            .cloned()
            .flatten()
    }

    fn button_index(focus: ReviewSettingsFocus) -> usize {
        match focus {
            ReviewSettingsFocus::Cancel => 0,
            ReviewSettingsFocus::Save
            | ReviewSettingsFocus::Enabled
            | ReviewSettingsFocus::Tier
            | ReviewSettingsFocus::Profile
            | ReviewSettingsFocus::Model
            | ReviewSettingsFocus::Effort => 1,
        }
    }

    fn can_save(&self) -> bool {
        if self.saving || self.read_only_reason.is_some() {
            return false;
        }
        // A disabled review is an ordinary place to keep a reviewer profile,
        // including a selector that an unavailable target cannot currently
        // validate. It must remain possible to save those values for a later
        // session.
        if !self.review.enabled {
            return true;
        }
        if self.review.profile.is_none() {
            return false;
        }
        matches!(
            self.readiness,
            ReviewReadiness::Verified(_) | ReviewReadiness::Unverified(_)
        )
    }

    fn probe_action(
        &mut self,
        dashboard: &mut DashboardState,
        refresh: ReviewProbeRefresh,
    ) -> DashboardAction {
        self.generation = dashboard.next_review_settings_generation();
        self.save_error = None;
        self.target_readiness.clear();
        match refresh {
            ReviewProbeRefresh::Profile => {
                self.model_choices.clear();
                self.effort_choices.clear();
                self.model_capabilities_discovered = false;
                self.effort_capabilities_discovered = false;
            }
            ReviewProbeRefresh::Model => {
                self.effort_choices.clear();
                self.effort_capabilities_discovered = false;
            }
            ReviewProbeRefresh::Effort => {}
        }
        let Some(profile) = self.review.profile.clone() else {
            self.probing = false;
            self.model_choices.clear();
            self.effort_choices.clear();
            self.model_capabilities_discovered = false;
            self.effort_capabilities_discovered = false;
            self.readiness = ReviewReadiness::Unverified(
                "select a reviewer profile, then start a session to verify it".to_owned(),
            );
            return DashboardAction::CancelReviewSettingsProbe;
        };
        self.probing = true;
        self.readiness = ReviewReadiness::Loading;
        DashboardAction::ProbeReviewSettings {
            generation: self.generation,
            profile_id: profile,
            model: self.review.model.clone(),
            effort: self.review.effort.clone(),
        }
    }

    pub(crate) fn apply_probe(
        &mut self,
        generation: u64,
        profile_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        result: Result<ReviewSettingsProbeResult, String>,
    ) -> bool {
        if generation != self.generation
            || self.review.profile.as_deref() != Some(profile_id)
            || self.review.model.as_deref() != model
            || self.review.effort.as_deref() != effort
        {
            return false;
        }
        self.probing = false;
        match result {
            Ok(result) => {
                // A failed target can still produce a report, but it did not
                // prove that an empty selector list came from a successful
                // adapter discovery. A non-empty list or a ready target is
                // the conservative evidence that an explicit value may be
                // called unavailable.
                let target_ready = result.targets.iter().any(|target| target.ready);
                self.model_capabilities_discovered =
                    !result.model_choices.is_empty() || target_ready;
                self.effort_capabilities_discovered =
                    !result.effort_choices.is_empty() || target_ready;
                self.target_readiness = result.targets.clone();
                self.model_choices = result.model_choices;
                self.effort_choices = result.effort_choices;
                if result.targets.is_empty() {
                    self.readiness = ReviewReadiness::Unverified(
                        "no active target is available; start a session to verify review readiness"
                            .to_owned(),
                    );
                } else if result.targets.iter().all(|target| target.ready) {
                    self.readiness = ReviewReadiness::Verified(result.targets);
                } else {
                    let failures = result
                        .targets
                        .iter()
                        .filter(|target| !target.ready)
                        .map(|target| format!("{}: {}", target.target, target.message))
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.readiness = ReviewReadiness::Invalid(failures);
                }
            }
            Err(error) => {
                self.target_readiness.clear();
                self.readiness = ReviewReadiness::Failed(error);
            }
        }
        true
    }

    pub(crate) fn apply_save_result(&mut self, result: Result<(), String>) {
        self.saving = false;
        self.save_error = result.err();
    }

    fn handle_key(&mut self, dashboard: &mut DashboardState, key: KeyEvent) -> DashboardAction {
        if key.code == KeyCode::Esc {
            dashboard.cancel_modal();
            return DashboardAction::CancelReviewSettingsProbe;
        }
        if self.saving {
            return DashboardAction::None;
        }
        let reverse = key.modifiers.contains(KeyModifiers::SHIFT);
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let next = cycle_control(self.focus, &REVIEW_SETTINGS_FOCUS, reverse);
            self.focus = next;
            return DashboardAction::None;
        }

        let delta = match key.code {
            KeyCode::Up | KeyCode::Left => Some(-1),
            KeyCode::Down | KeyCode::Right => Some(1),
            _ => None,
        };
        if let Some(delta) = delta {
            match self.focus {
                ReviewSettingsFocus::Enabled => self.review.enabled = !self.review.enabled,
                ReviewSettingsFocus::Tier => {
                    self.review.tier = cycle_control(
                        self.review.tier,
                        &[ReviewTier::Quick, ReviewTier::Extended],
                        delta < 0,
                    );
                }
                ReviewSettingsFocus::Profile => {
                    let index = cycle_index(self.profile_index(), self.profiles.len(), delta);
                    self.review.profile = self.profiles.get(index).cloned().flatten();
                    return self.probe_action(dashboard, ReviewProbeRefresh::Profile);
                }
                ReviewSettingsFocus::Model => {
                    self.review.model = Self::select_value(
                        self.review.model.as_deref(),
                        &self.model_choices,
                        delta,
                    );
                    return self.probe_action(dashboard, ReviewProbeRefresh::Model);
                }
                ReviewSettingsFocus::Effort => {
                    self.review.effort = Self::select_value(
                        self.review.effort.as_deref(),
                        &self.effort_choices,
                        delta,
                    );
                    return self.probe_action(dashboard, ReviewProbeRefresh::Effort);
                }
                ReviewSettingsFocus::Cancel | ReviewSettingsFocus::Save => {}
            }
            return DashboardAction::None;
        }

        if key.code == KeyCode::Char(' ') && self.focus == ReviewSettingsFocus::Enabled {
            self.review.enabled = !self.review.enabled;
            return DashboardAction::None;
        }

        if key.code != KeyCode::Enter {
            return DashboardAction::None;
        }
        match self.focus {
            ReviewSettingsFocus::Cancel => {
                dashboard.cancel_modal();
                DashboardAction::CancelReviewSettingsProbe
            }
            ReviewSettingsFocus::Save => {
                if !self.can_save() {
                    if self.read_only_reason.is_some() {
                        dashboard.set_notice(
                            self.read_only_reason
                                .clone()
                                .unwrap_or_else(|| "Configuration is read-only.".to_owned()),
                        );
                    } else if self.review.enabled && self.review.profile.is_none() {
                        dashboard.set_notice(
                            "Choose a reviewer profile before enabling automatic review.",
                        );
                    } else if let ReviewReadiness::Invalid(message) = &self.readiness {
                        dashboard.set_notice(format!("Review settings are not ready: {message}"));
                    } else if let ReviewReadiness::Failed(message) = &self.readiness {
                        dashboard.set_notice(format!(
                            "Could not verify review readiness: {message}. Retry or disable automatic review."
                        ));
                    } else if self.probing {
                        dashboard.set_notice(
                            "Wait for review readiness to finish before enabling automatic review.",
                        );
                    }
                    return DashboardAction::None;
                }
                self.saving = true;
                self.save_error = None;
                DashboardAction::SaveReviewSettings {
                    review: self.review.clone(),
                }
            }
            ReviewSettingsFocus::Enabled => {
                self.review.enabled = !self.review.enabled;
                DashboardAction::None
            }
            ReviewSettingsFocus::Tier => {
                self.review.tier = cycle_control(
                    self.review.tier,
                    &[ReviewTier::Quick, ReviewTier::Extended],
                    false,
                );
                DashboardAction::None
            }
            ReviewSettingsFocus::Profile => {
                self.probe_action(dashboard, ReviewProbeRefresh::Profile)
            }
            ReviewSettingsFocus::Model => self.probe_action(dashboard, ReviewProbeRefresh::Model),
            ReviewSettingsFocus::Effort => {
                self.review.effort =
                    Self::select_value(self.review.effort.as_deref(), &self.effort_choices, 1);
                self.probe_action(dashboard, ReviewProbeRefresh::Effort)
            }
        }
    }
}

fn cycle_index(index: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        index.checked_sub(delta.unsigned_abs()).unwrap_or(len - 1)
    } else {
        index.saturating_add(delta as usize) % len
    }
}

impl DashboardState {
    fn next_review_settings_generation(&mut self) -> u64 {
        self.review_settings_generation = self.review_settings_generation.wrapping_add(1);
        self.review_settings_generation
    }

    pub(crate) fn begin_review_settings(&mut self) -> DashboardAction {
        let mut dialog = ReviewSettingsDialog::new(&self.config);
        let action = if dialog.review.profile.is_some() {
            dialog.probe_action(self, ReviewProbeRefresh::Profile)
        } else {
            DashboardAction::None
        };
        self.mode = Mode::ReviewSettings(dialog);
        action
    }

    pub(crate) fn handle_review_settings_key(
        &mut self,
        key: KeyEvent,
        mut dialog: ReviewSettingsDialog,
    ) -> DashboardAction {
        let action = dialog.handle_key(self, key);
        if !matches!(self.mode, Mode::Dashboard) {
            self.mode = Mode::ReviewSettings(dialog);
        }
        action
    }

    pub fn apply_review_settings_probe(
        &mut self,
        generation: u64,
        profile_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        result: Result<ReviewSettingsProbeResult, String>,
    ) -> bool {
        let Mode::ReviewSettings(dialog) = &mut self.mode else {
            return false;
        };
        dialog.apply_probe(generation, profile_id, model, effort, result)
    }

    pub fn review_settings_save_failed(&mut self, error: String) {
        let Mode::ReviewSettings(dialog) = &mut self.mode else {
            return;
        };
        dialog.apply_save_result(Err(error));
    }
}

pub(crate) fn render_review_settings(
    frame: &mut Frame,
    area: Rect,
    dialog: &ReviewSettingsDialog,
    surfaces: &mut FrameSurfaces,
) {
    let field = |focused| {
        if focused {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };
    let row = |label: &str, value: String, focused: bool| {
        Line::from(vec![
            Span::styled(format!("{label:<18}"), Style::default().fg(Color::DarkGray)),
            Span::styled(value, field(focused)),
        ])
    };
    let mut lines = vec![
        Line::styled(
            "Global settings; changes apply to subsequent reviews.",
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
        row(
            "Automatic review",
            if dialog.review.enabled { "On" } else { "Off" }.to_owned(),
            dialog.focus == ReviewSettingsFocus::Enabled,
        ),
        row(
            "Tier",
            dialog.review.tier.label().to_owned(),
            dialog.focus == ReviewSettingsFocus::Tier,
        ),
        row(
            "Profile",
            dialog.profile_label(),
            dialog.focus == ReviewSettingsFocus::Profile,
        ),
        row(
            "Model",
            ReviewSettingsDialog::value_label(
                dialog.review.model.as_deref(),
                &dialog.model_choices,
                dialog.model_capabilities_discovered,
            ),
            dialog.focus == ReviewSettingsFocus::Model,
        ),
        row(
            "Effort",
            ReviewSettingsDialog::value_label(
                dialog.review.effort.as_deref(),
                &dialog.effort_choices,
                dialog.effort_capabilities_discovered,
            ),
            dialog.focus == ReviewSettingsFocus::Effort,
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Readiness        ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                dialog.readiness.label(),
                Style::default().fg(match dialog.readiness {
                    ReviewReadiness::Verified(_) => Color::Green,
                    ReviewReadiness::Invalid(_) | ReviewReadiness::Failed(_) => Color::Yellow,
                    _ => Color::DarkGray,
                }),
            ),
        ]),
    ];
    if !dialog.target_readiness.is_empty() {
        let targets = &dialog.target_readiness;
        for target in targets {
            lines.push(Line::styled(
                format!(
                    "  {}: {}",
                    target.target,
                    if target.ready {
                        "ready"
                    } else {
                        &target.message
                    }
                ),
                Style::default().fg(if target.ready {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ));
        }
    }
    if let Some(reason) = &dialog.read_only_reason {
        lines.push(Line::styled(
            reason.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(error) = &dialog.save_error {
        lines.push(Line::styled(
            format!("Save failed: {error}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if dialog.review.profile.is_none() && dialog.review.enabled {
        lines.push(Line::styled(
            "Choose a profile before enabling automatic review.",
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.push(Line::raw(""));
    let save_focused = dialog.focus == ReviewSettingsFocus::Save;
    let buttons = if dialog.saving {
        Line::styled(
            "Saving review settings…",
            Style::default().fg(Color::Yellow),
        )
    } else {
        let mut line = focused_buttons(
            REVIEW_SETTINGS_BUTTONS,
            ReviewSettingsDialog::button_index(dialog.focus),
        );
        if !dialog.can_save() && save_focused {
            line = Line::from(vec![Span::styled(
                " Save unavailable ",
                Style::default().fg(Color::DarkGray),
            )]);
        }
        line
    };
    lines.push(buttons);
    lines.push(Line::styled(
        "Tab moves · arrows change values · Profile default clears model/effort · Esc closes",
        Style::default().fg(Color::DarkGray),
    ));
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Review settings "),
        )
        .wrap(Wrap { trim: false });
    let popup = centered_modal(
        frame,
        surfaces,
        86,
        popup_height(&paragraph, 86, 20, area),
        area,
    );
    frame.render_widget(paragraph, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::CommandId;
    use crate::test_support::{config, dashboard_with_session, key, running_session};

    fn open(dashboard: &mut DashboardState) -> DashboardAction {
        dashboard.dispatch_command(CommandId::ReviewSettings)
    }

    fn dialog(dashboard: &DashboardState) -> &ReviewSettingsDialog {
        let Mode::ReviewSettings(dialog) = &dashboard.mode else {
            panic!("expected review settings dialog")
        };
        dialog
    }

    #[test]
    fn review_settings_is_global_even_without_a_selected_session() {
        let mut dashboard = DashboardState::new(
            config(),
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        let action = open(&mut dashboard);
        assert!(matches!(action, DashboardAction::None));
        assert!(matches!(dashboard.mode, Mode::ReviewSettings(_)));
        assert_eq!(dashboard.selected_session_id(), None);

        let mut dashboard = DashboardState::new(
            config(),
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        dashboard.handle_key(key(KeyCode::F(2)));
        let Mode::Palette(palette) = &dashboard.mode else {
            panic!("F2 should open the command palette")
        };
        assert!(
            palette
                .entries
                .iter()
                .any(|entry| entry.id == CommandId::ReviewSettings)
        );
    }

    #[test]
    fn edit_and_save_action_contains_only_global_review_values() {
        let mut dashboard = dashboard_with_session(running_session());
        let _ = open(&mut dashboard);
        // Enabled -> Tier -> Profile, then choose the first configured profile.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        let probe = dashboard.handle_key(key(KeyCode::Right));
        assert!(matches!(probe, DashboardAction::ProbeReviewSettings { .. }));

        while dialog(&dashboard).focus != ReviewSettingsFocus::Tier {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Right));
        while dialog(&dashboard).focus != ReviewSettingsFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        let action = dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, DashboardAction::SaveReviewSettings { .. }));
        assert!(dialog(&dashboard).saving);
    }

    #[test]
    fn stale_probe_does_not_replace_choices_after_model_change() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard = DashboardState::new(
            config,
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        let initial = open(&mut dashboard);
        let DashboardAction::ProbeReviewSettings {
            generation,
            profile_id,
            model,
            effort,
        } = initial
        else {
            panic!("expected initial probe")
        };
        dashboard.apply_review_settings_probe(
            generation,
            &profile_id,
            model.as_deref(),
            effort.as_deref(),
            Ok(ReviewSettingsProbeResult {
                model_choices: vec![SessionConfigChoice {
                    value: "model-a".into(),
                    name: "Model A".into(),
                    description: None,
                }],
                effort_choices: vec![],
                targets: vec![],
            }),
        );
        while dialog(&dashboard).focus != ReviewSettingsFocus::Model {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        let model_action = dashboard.handle_key(key(KeyCode::Right));
        let DashboardAction::ProbeReviewSettings {
            generation: newer,
            profile_id,
            model,
            effort,
        } = model_action
        else {
            panic!("expected model probe")
        };
        assert!(newer > generation);
        assert!(dialog(&dashboard).probing);
        assert_eq!(dialog(&dashboard).model_choices[0].value, "model-a");
        assert!(dialog(&dashboard).effort_choices.is_empty());
        dashboard.apply_review_settings_probe(
            generation,
            &profile_id,
            None,
            None,
            Ok(ReviewSettingsProbeResult {
                model_choices: vec![SessionConfigChoice {
                    value: "stale".into(),
                    name: "Stale".into(),
                    description: None,
                }],
                effort_choices: vec![],
                targets: vec![],
            }),
        );
        assert_eq!(dialog(&dashboard).model_choices[0].value, "model-a");
        assert!(dialog(&dashboard).probing);
        dashboard.apply_review_settings_probe(
            newer,
            &profile_id,
            model.as_deref(),
            effort.as_deref(),
            Ok(ReviewSettingsProbeResult {
                model_choices: vec![],
                effort_choices: vec![],
                targets: vec![],
            }),
        );
        assert!(!dialog(&dashboard).probing);
    }

    #[test]
    fn effort_change_starts_a_fresh_readiness_probe() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard = DashboardState::new(
            config,
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        let DashboardAction::ProbeReviewSettings { generation, .. } = open(&mut dashboard) else {
            panic!("expected initial probe")
        };
        dashboard.apply_review_settings_probe(
            generation,
            "codex-1",
            None,
            None,
            Ok(ReviewSettingsProbeResult {
                model_choices: vec![],
                effort_choices: vec![SessionConfigChoice {
                    value: "low".into(),
                    name: "Low".into(),
                    description: None,
                }],
                targets: vec![ReviewTargetReadiness {
                    target: "worker".into(),
                    ready: true,
                    message: String::new(),
                }],
            }),
        );
        while dialog(&dashboard).focus != ReviewSettingsFocus::Effort {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        let action = dashboard.handle_key(key(KeyCode::Right));
        assert!(matches!(
            action,
            DashboardAction::ProbeReviewSettings { generation: newer, .. } if newer > generation
        ));
        assert!(dialog(&dashboard).probing);
    }

    #[test]
    fn selectors_show_unverified_until_capabilities_are_discovered() {
        assert_eq!(
            ReviewSettingsDialog::value_label(Some("opus"), &[], false),
            "opus (unverified)"
        );
        assert_eq!(
            ReviewSettingsDialog::value_label(Some("opus"), &[], true),
            "opus (unavailable)"
        );
    }

    #[test]
    fn closing_and_reopening_drops_the_previous_dialog_probe() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard = DashboardState::new(
            config,
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        let DashboardAction::ProbeReviewSettings {
            generation,
            profile_id,
            model,
            effort,
        } = open(&mut dashboard)
        else {
            panic!("expected initial probe")
        };
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::CancelReviewSettingsProbe
        ));
        assert!(!dashboard.modal_open());

        let DashboardAction::ProbeReviewSettings {
            generation: reopened,
            ..
        } = open(&mut dashboard)
        else {
            panic!("expected reopened probe")
        };
        assert!(reopened > generation);
        dashboard.apply_review_settings_probe(
            generation,
            &profile_id,
            model.as_deref(),
            effort.as_deref(),
            Ok(ReviewSettingsProbeResult {
                model_choices: vec![SessionConfigChoice {
                    value: "old".into(),
                    name: "Old".into(),
                    description: None,
                }],
                effort_choices: vec![],
                targets: vec![],
            }),
        );
        assert!(dialog(&dashboard).model_choices.is_empty());
        assert!(dialog(&dashboard).probing);
    }

    #[test]
    fn enabled_settings_block_while_checking_but_disabled_settings_can_save() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        config.review.enabled = true;
        let mut dashboard = DashboardState::new(
            config,
            hel::hel_state::HelState::default(),
            Default::default(),
        );
        let _ = open(&mut dashboard);
        while dialog(&dashboard).focus != ReviewSettingsFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        ));
        assert!(!dialog(&dashboard).saving);
        // Disable automatic review and save despite the failed/unverified probe.
        while dialog(&dashboard).focus != ReviewSettingsFocus::Enabled {
            dashboard.handle_key(key(KeyCode::BackTab));
        }
        dashboard.handle_key(key(KeyCode::Char(' ')));
        while dialog(&dashboard).focus != ReviewSettingsFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::SaveReviewSettings { .. }
        ));
    }
}
