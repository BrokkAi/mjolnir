//! The global review settings editor.
//!
//! This dialog owns only the in-memory draft of [`HelConfig::review`]. The
//! controller performs discovery and persistence off the event loop; replies
//! carry a generation so a slow probe can never replace a newer choice.

use std::cell::{Cell, RefCell};

use crossterm::event::Event;
use hel::hel_acp::SessionConfigChoice;
use hel::hel_config::{HelConfig, ReviewConfig};
use hel::hel_review::lanes::ReviewTier;
use mj_chat::components::{
    ButtonRow, Checkbox, ControlKind, Form, FormViewport, Interaction, TabStrip,
};
use mj_chat::hel_selection::FrameSurfaces;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};

use crate::widgets::centered_modal;
use crate::{DashboardAction, DashboardState, Mode};

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
    pub(crate) form: RefCell<Form<ReviewSettingsFocus>>,
    scroll: Cell<u16>,
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
        let dialog = Self {
            review: config.review.clone(),
            profiles,
            form: RefCell::new(Form::default()),
            scroll: Cell::new(0),
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
        };
        dialog.prepare();
        dialog
    }

    fn profile_index(&self) -> usize {
        self.profiles
            .iter()
            .position(|profile| profile.as_deref() == self.review.profile.as_deref())
            .unwrap_or(0)
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

    fn focused(&self) -> ReviewSettingsFocus {
        self.form
            .borrow()
            .focused()
            .unwrap_or(ReviewSettingsFocus::Enabled)
    }

    fn selectors(&self) -> Vec<(ReviewSettingsFocus, &'static str, Vec<String>, usize)> {
        use ReviewSettingsFocus::*;
        let choices = |value: Option<&str>, choices: &[SessionConfigChoice], discovered| {
            let values = Self::choice_values(value, choices);
            let selected = values
                .iter()
                .position(|entry| entry.as_deref() == value)
                .unwrap_or(0);
            let labels = values
                .iter()
                .map(|entry| Self::value_label(entry.as_deref(), choices, discovered))
                .collect();
            (labels, selected)
        };
        let (models, model) = choices(
            self.review.model.as_deref(),
            &self.model_choices,
            self.model_capabilities_discovered,
        );
        let (efforts, effort) = choices(
            self.review.effort.as_deref(),
            &self.effort_choices,
            self.effort_capabilities_discovered,
        );
        vec![
            (
                Tier,
                "Tier",
                vec!["Quick".into(), "Extended".into()],
                usize::from(self.review.tier == ReviewTier::Extended),
            ),
            (
                Profile,
                "Profile",
                self.profiles
                    .iter()
                    .map(|value| {
                        value
                            .clone()
                            .unwrap_or_else(|| "No reviewer profile".into())
                    })
                    .collect(),
                self.profile_index(),
            ),
            (Model, "Model", models, model),
            (Effort, "Effort", efforts, effort),
        ]
    }

    fn prepare(&self) {
        use ReviewSettingsFocus::*;
        let mut form = self.form.borrow_mut();
        form.begin_update();
        form.declare_with_enabled(Enabled, ControlKind::Checkbox, !self.saving);
        for (id, _, labels, selected) in self.selectors() {
            form.declare_with_enabled(
                id,
                ControlKind::Tabs {
                    len: labels.len(),
                    selected,
                },
                !self.saving,
            );
        }
        form.declare_with_enabled(Cancel, ControlKind::Button, true);
        form.declare_with_enabled(Save, ControlKind::Button, self.can_save());
        form.end_frame(Enabled);
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

    fn handle_event(&mut self, dashboard: &mut DashboardState, event: Event) -> DashboardAction {
        use ReviewSettingsFocus::*;
        let interaction = self.form.get_mut().handle(&event).action;
        let changed = interaction.is_some();
        let action = match interaction {
            Some(Interaction::Cancel | Interaction::Activate(Cancel)) => {
                DashboardAction::CancelReviewSettingsProbe
            }
            Some(Interaction::Toggle(Enabled)) => {
                self.review.enabled = !self.review.enabled;
                DashboardAction::None
            }
            Some(Interaction::Select(Tier, index)) => {
                self.review.tier = if index == 0 {
                    ReviewTier::Quick
                } else {
                    ReviewTier::Extended
                };
                DashboardAction::None
            }
            Some(Interaction::Select(Profile, index)) => {
                self.review.profile = self.profiles.get(index).cloned().flatten();
                self.probe_action(dashboard, ReviewProbeRefresh::Profile)
            }
            Some(Interaction::Select(Model, index)) => {
                self.review.model =
                    Self::choice_values(self.review.model.as_deref(), &self.model_choices)
                        .get(index)
                        .cloned()
                        .flatten();
                self.probe_action(dashboard, ReviewProbeRefresh::Model)
            }
            Some(Interaction::Select(Effort, index)) => {
                self.review.effort =
                    Self::choice_values(self.review.effort.as_deref(), &self.effort_choices)
                        .get(index)
                        .cloned()
                        .flatten();
                self.probe_action(dashboard, ReviewProbeRefresh::Effort)
            }
            Some(Interaction::Activate(Profile | Model | Effort)) => {
                let refresh = match self.focused() {
                    Model => ReviewProbeRefresh::Model,
                    Effort => ReviewProbeRefresh::Effort,
                    _ => ReviewProbeRefresh::Profile,
                };
                self.probe_action(dashboard, refresh)
            }
            Some(Interaction::Activate(Save)) if self.can_save() => {
                self.saving = true;
                self.save_error = None;
                DashboardAction::SaveReviewSettings {
                    review: self.review.clone(),
                }
            }
            _ => DashboardAction::None,
        };
        if changed {
            self.prepare();
        }
        action
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
        dialog.prepare();
        self.mode = Mode::ReviewSettings(dialog);
        action
    }

    pub(crate) fn handle_review_settings_event(
        &mut self,
        event: Event,
        mut dialog: ReviewSettingsDialog,
    ) -> DashboardAction {
        let action = dialog.handle_event(self, event);
        if matches!(action, DashboardAction::CancelReviewSettingsProbe) {
            self.cancel_modal();
        } else {
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
        let changed = dialog.apply_probe(generation, profile_id, model, effort, result);
        if changed {
            dialog.prepare();
        }
        changed
    }

    pub fn review_settings_save_failed(&mut self, error: String) {
        let Mode::ReviewSettings(dialog) = &mut self.mode else {
            return;
        };
        dialog.apply_save_result(Err(error));
        dialog.prepare();
    }
}

pub(crate) fn render_review_settings(
    frame: &mut Frame,
    area: Rect,
    dialog: &ReviewSettingsDialog,
    surfaces: &mut FrameSurfaces,
) {
    use ReviewSettingsFocus::*;
    let mut notes = vec![
        Line::raw("Global settings; changes apply to subsequent reviews."),
        Line::raw(format!("Readiness: {}", dialog.readiness.label())),
    ];
    for target in &dialog.target_readiness {
        notes.push(Line::raw(format!(
            "{}: {}",
            target.target,
            if target.ready {
                "ready"
            } else {
                &target.message
            }
        )));
    }
    if let Some(reason) = &dialog.read_only_reason {
        notes.push(Line::styled(
            reason.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(error) = &dialog.save_error {
        notes.push(Line::styled(
            format!("Save failed: {error}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    if dialog.review.profile.is_none() && dialog.review.enabled {
        notes.push(Line::raw(
            "Choose a profile before enabling automatic review.",
        ));
    }
    let popup = centered_modal(
        frame,
        surfaces,
        86,
        (notes.len() as u16).saturating_add(10).max(20),
        area,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Review settings ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let focus_row = match dialog.focused() {
        Enabled => 0,
        Tier => 1,
        Profile => 2,
        Model => 3,
        Effort => 4,
        _ => 0,
    };
    let viewport = FormViewport::new(
        body,
        (notes.len() as u16).saturating_add(6),
        dialog.scroll.get(),
        Some(focus_row),
    );
    dialog.scroll.set(viewport.offset());
    let row = |index: u16| viewport.row(index, 1);
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    Checkbox::render(
        frame,
        row(0),
        "Automatic review",
        dialog.review.enabled,
        !dialog.saving,
        &mut form,
        Enabled,
    );
    for (index, (id, label, values, selected)) in dialog.selectors().iter().enumerate() {
        let area = row(index as u16 + 1);
        let label_width = 10.min(area.width);
        frame.render_widget(
            Line::raw(*label),
            Rect::new(area.x, area.y, label_width, area.height),
        );
        let field = Rect::new(
            area.x + label_width,
            area.y,
            area.width - label_width,
            area.height,
        );
        TabStrip::render_enabled(
            frame,
            field,
            &values.iter().map(String::as_str).collect::<Vec<_>>(),
            *selected,
            !dialog.saving,
            &mut form,
            *id,
        );
    }
    for (index, line) in notes.into_iter().enumerate() {
        frame.render_widget(line, row(index as u16 + 6));
    }
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    if inner.height > 1 {
        frame.render_widget(
            Line::styled(
                "Tab moves · arrows select · Space toggles · Esc closes",
                Style::default().fg(Color::DarkGray),
            ),
            Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
        );
    }
    ButtonRow::render(
        frame,
        footer,
        &[
            (Cancel, "Cancel", true),
            (
                Save,
                if dialog.saving { "Saving…" } else { "Save" },
                dialog.can_save(),
            ),
        ],
        &mut form,
    );
    form.end_frame(Enabled);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::CommandId;
    use crate::test_support::{config, dashboard_with_session, key, running_session};
    use crossterm::event::KeyCode;

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

        while dialog(&dashboard).focused() != ReviewSettingsFocus::Tier {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        dashboard.handle_key(key(KeyCode::Right));
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
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
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Model {
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
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Effort {
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
        for _ in 0..8 {
            dashboard.handle_key(key(KeyCode::Tab));
            assert_ne!(dialog(&dashboard).focused(), ReviewSettingsFocus::Save);
        }
        assert!(!dialog(&dashboard).saving);
        // Disable automatic review and save despite the failed/unverified probe.
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Enabled {
            dashboard.handle_key(key(KeyCode::BackTab));
        }
        dashboard.handle_key(key(KeyCode::Char(' ')));
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::SaveReviewSettings { .. }
        ));
    }
}
