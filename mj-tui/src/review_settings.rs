//! The global review settings editor.
//!
//! This dialog owns only the in-memory draft of [`Config::review`]. The
//! controller performs discovery and persistence off the event loop; replies
//! carry a generation so a slow discovery can never replace a newer choice.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::time::Instant;

use crossterm::event::Event;
use mj_chat::components::{
    Checkbox, ComboBox, ComboBoxState, ControlKind, Dialog, FormViewport, Interaction, PopupSide,
};
use mj_chat::theme;
use mj_core::acp::SessionConfigChoice;
use mj_core::config::{Config, ReviewConfig, SpinnerStyle};
use mj_core::review::lanes::ReviewTier;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::widgets::dismissible_modal_title;
use crate::{DashboardAction, DashboardState, Mode};

/// Selectors advertised by one successful reviewer discovery.
///
/// The choices come from the harness adapter. The UI adds the explicit
/// profile-default row while rendering, so it never invents a model or effort
/// accepted by a worker. The boolean distinguishes a successful empty effort
/// list from an effort probe that has not completed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewSettingsChoices {
    pub model_choices: Vec<SessionConfigChoice>,
    pub effort_choices: Vec<SessionConfigChoice>,
    pub effort_capabilities_discovered: bool,
}

/// The final outcome of reviewer selector discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewSettingsDiscoveryResult {
    Available {
        choices: ReviewSettingsChoices,
        cleanup_warning: Option<String>,
    },
    Unavailable,
}

pub(crate) type ReviewSettingsCacheKey = (String, Option<String>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewSettingsFocus {
    Enabled,
    Tier,
    Profile,
    Model,
    Effort,
    Back,
    Cancel,
    Refresh,
    Save,
}

/// What the nested editor wants its owning Setup draft to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewSettingsOutcome {
    Continue,
    Back,
    CancelSetup,
    Save,
}

/// Capability knowledge that must remain attached to a Setup draft after the
/// nested editor is left. Without it, a known unsupported model or effort
/// could be saved after the user pressed Back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewSettingsValidation {
    pub(crate) profile: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) model_choices: Vec<SessionConfigChoice>,
    pub(crate) effort_choices: Vec<SessionConfigChoice>,
    pub(crate) model_choices_discovered: bool,
    pub(crate) effort_capabilities_discovered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewSettingsDiscoveryKind {
    Profile,
    Model,
    Refresh,
}

/// The editable global review form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewSettingsDialog {
    pub(crate) review: ReviewConfig,
    original_review: ReviewConfig,
    pub(crate) profiles: Vec<Option<String>>,
    pub(crate) form: RefCell<Dialog<ReviewSettingsFocus>>,
    combo: ComboBoxState<ReviewSettingsFocus>,
    scroll: Cell<u16>,
    pub(crate) model_choices: Vec<SessionConfigChoice>,
    pub(crate) effort_choices: Vec<SessionConfigChoice>,
    /// Whether the model list came from a successful discovery. This remains
    /// useful when an adapter advertises no configurable models.
    pub(crate) model_choices_discovered: bool,
    pub(crate) effort_capabilities_discovered: bool,
    pub(crate) generation: u64,
    pub(crate) probing: bool,
    choices_loading: bool,
    request_key: Option<ReviewSettingsCacheKey>,
    discovery_started: Instant,
    spinner_style: SpinnerStyle,
    pub(crate) cleanup_warning: Option<String>,
    pub(crate) discovery_error: Option<String>,
    pub(crate) saving: bool,
    pub(crate) save_error: Option<String>,
    pub(crate) read_only_reason: Option<String>,
    /// Profiles whose definitions differ from the saved Setup config. Their
    /// workers must not be queried until Setup persists the account changes.
    pub(crate) blocked_profile_ids: BTreeSet<String>,
}

impl ReviewSettingsDialog {
    pub(crate) fn prepare_dialog_state(&mut self) {
        self.form.get_mut().set_action_role(
            ReviewSettingsFocus::Cancel,
            mj_chat::components::ActionRole::Cancel,
        );
        self.form.get_mut().set_action_role(
            ReviewSettingsFocus::Back,
            mj_chat::components::ActionRole::Back,
        );
        self.form
            .get_mut()
            .track_draft(vec![format!("{:?}", self.review)]);
        self.form
            .get_mut()
            .set_dismiss_actions(&[ReviewSettingsFocus::Back, ReviewSettingsFocus::Cancel]);
        self.form
            .get_mut()
            .set_default_action(ReviewSettingsFocus::Save);
    }

    pub(crate) fn animation_frame(&self) -> Option<&'static str> {
        (self.probing && self.choices_loading).then(|| {
            mj_chat::spinner::compact_frame(
                self.spinner_style,
                self.discovery_started.elapsed().as_millis(),
            )
        })
    }

    pub(crate) fn new(config: &Config) -> Self {
        let mut profiles = vec![None];
        profiles.extend(
            config
                .enabled_profiles()
                .filter(|(_, profile)| profile.kind.supports_injected_mcp())
                .map(|(id, _)| Some(id.to_owned())),
        );
        let dialog = Self {
            review: config.review.clone(),
            original_review: config.review.clone(),
            profiles,
            form: RefCell::new(Dialog::default()),
            combo: ComboBoxState::default(),
            scroll: Cell::new(0),
            model_choices: Vec::new(),
            effort_choices: Vec::new(),
            model_choices_discovered: false,
            effort_capabilities_discovered: false,
            generation: 0,
            probing: false,
            choices_loading: false,
            request_key: None,
            discovery_started: Instant::now(),
            spinner_style: config.spinner,
            cleanup_warning: None,
            discovery_error: None,
            saving: false,
            save_error: None,
            read_only_reason: config.newer_build_notice(),
            blocked_profile_ids: BTreeSet::new(),
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
            self.model_choices_discovered,
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

    pub(crate) fn prepare(&self) {
        use ReviewSettingsFocus::*;
        let mut form = self.form.borrow_mut();
        form.begin_update();
        form.declare_with_enabled(Enabled, ControlKind::Checkbox, !self.saving);
        for (id, _, labels, selected) in self.selectors() {
            form.declare_with_enabled(
                id,
                ControlKind::ComboBox {
                    len: labels.len(),
                    selected: self.combo.selection(id, selected),
                    expanded: self.combo.is_open(id),
                },
                !self.saving,
            );
        }
        form.declare_with_enabled(Cancel, ControlKind::Button, true);
        form.declare_with_enabled(Back, ControlKind::Button, true);
        form.declare_with_enabled(Refresh, ControlKind::Button, !self.saving);
        form.declare_with_enabled(Save, ControlKind::Button, self.can_save());
        form.end_frame(Enabled);
    }

    fn can_save(&self) -> bool {
        if self.saving || self.read_only_reason.is_some() {
            return false;
        }
        // A disabled review remains a useful draft even while discovery is
        // unavailable. Local config validation still runs when the controller
        // saves it.
        if !self.review.enabled {
            return true;
        }
        let Some(profile) = self.review.profile.as_deref() else {
            return false;
        };
        if !self
            .profiles
            .iter()
            .any(|candidate| candidate.as_deref() == Some(profile))
        {
            return false;
        }
        if self.model_choices_discovered
            && self.review.model.as_deref().is_some_and(|model| {
                !self
                    .model_choices
                    .iter()
                    .any(|choice| choice.value == model)
            })
        {
            return false;
        }
        if self.effort_capabilities_discovered
            && self.review.effort.as_deref().is_some_and(|effort| {
                !self
                    .effort_choices
                    .iter()
                    .any(|choice| choice.value == effort)
            })
        {
            return false;
        }
        true
    }

    pub(crate) fn validation_snapshot(&self) -> ReviewSettingsValidation {
        ReviewSettingsValidation {
            profile: self.review.profile.clone(),
            model: self.review.model.clone(),
            effort: self.review.effort.clone(),
            model_choices: self.model_choices.clone(),
            effort_choices: self.effort_choices.clone(),
            model_choices_discovered: self.model_choices_discovered,
            effort_capabilities_discovered: self.effort_capabilities_discovered,
        }
    }

    pub(crate) fn validation_error(
        snapshot: &ReviewSettingsValidation,
        review: &ReviewConfig,
    ) -> Option<String> {
        if !review.enabled
            || snapshot.profile != review.profile
            || snapshot.model != review.model
            || snapshot.effort != review.effort
        {
            return None;
        }
        if snapshot.model_choices_discovered
            && review.model.as_deref().is_some_and(|model| {
                !snapshot
                    .model_choices
                    .iter()
                    .any(|choice| choice.value == model)
            })
        {
            return Some("Selected review model is unavailable in the discovered choices.".into());
        }
        if snapshot.effort_capabilities_discovered
            && review.effort.as_deref().is_some_and(|effort| {
                !snapshot
                    .effort_choices
                    .iter()
                    .any(|choice| choice.value == effort)
            })
        {
            return Some("Selected review effort is unavailable in the discovered choices.".into());
        }
        None
    }

    fn apply_cached_choices(&mut self, choices: &ReviewSettingsChoices) {
        self.model_choices = choices.model_choices.clone();
        self.effort_choices = choices.effort_choices.clone();
        self.model_choices_discovered = true;
        self.effort_capabilities_discovered = choices.effort_capabilities_discovered;
        self.cleanup_warning = None;
        self.discovery_error = None;
        self.probing = false;
        self.choices_loading = false;
    }

    fn clear_profile_choices(&mut self) {
        self.model_choices.clear();
        self.effort_choices.clear();
        self.model_choices_discovered = false;
        self.effort_capabilities_discovered = false;
    }

    fn start_discovery(
        &mut self,
        dashboard: &mut DashboardState,
        kind: ReviewSettingsDiscoveryKind,
    ) -> DashboardAction {
        self.save_error = None;
        match kind {
            ReviewSettingsDiscoveryKind::Profile => self.clear_profile_choices(),
            ReviewSettingsDiscoveryKind::Model => {
                // Model choices describe the profile and remain useful while
                // the model-specific effort discovery is in flight.
                self.effort_choices.clear();
                self.effort_capabilities_discovered = false;
            }
            // Refresh deliberately retains the current choices and knowledge
            // flags while removing the persisted cache entry. This keeps a
            // draft saveable when the fresh discovery cannot reach a worker.
            ReviewSettingsDiscoveryKind::Refresh => {}
        }
        let Some(profile) = self.review.profile.clone() else {
            self.generation = dashboard.next_review_settings_generation();
            self.probing = false;
            self.choices_loading = false;
            self.request_key = None;
            self.cleanup_warning = None;
            self.discovery_error = None;
            return DashboardAction::CancelReviewSettingsDiscovery;
        };

        if self.blocked_profile_ids.contains(&profile) {
            self.generation = dashboard.next_review_settings_generation();
            self.probing = false;
            self.choices_loading = false;
            self.request_key = None;
            self.discovery_error =
                Some("Save account changes before discovering review capabilities.".into());
            return DashboardAction::CancelReviewSettingsDiscovery;
        }

        let key = (profile.clone(), self.review.model.clone());
        if !matches!(kind, ReviewSettingsDiscoveryKind::Refresh)
            && let Some(choices) = dashboard.review_settings_choices.get(&key).cloned()
        {
            // A cache hit supersedes any request still owned by the controller.
            // Bumping the generation makes a late reply harmless; the single
            // cancel action lets the controller stop that request.
            let had_pending = self.probing;
            self.generation = dashboard.next_review_settings_generation();
            self.apply_cached_choices(&choices);
            self.request_key = None;
            return if had_pending {
                DashboardAction::CancelReviewSettingsDiscovery
            } else {
                DashboardAction::None
            };
        }

        // Selecting another key supersedes the previous request. The
        // controller cancels it before starting this generation.
        if !matches!(kind, ReviewSettingsDiscoveryKind::Refresh)
            && self.probing
            && self.request_key.as_ref() == Some(&key)
        {
            return DashboardAction::None;
        }
        self.probing = true;
        self.choices_loading = true;
        self.discovery_started = Instant::now();
        self.cleanup_warning = None;
        self.discovery_error = None;
        self.generation = dashboard.next_review_settings_generation();
        self.request_key = Some(key);
        DashboardAction::DiscoverReviewSettings {
            generation: self.generation,
            profile_id: profile,
            model: self.review.model.clone(),
        }
    }

    /// Invalidates a request before the parent Setup draft is parked behind a
    /// dismissal confirmation. A restored draft must never resume a spinner
    /// whose cancellation result can no longer reach the visible editor.
    pub(crate) fn cancel_discovery(&mut self, dashboard: &mut DashboardState) {
        if self.probing || self.choices_loading || self.request_key.is_some() {
            self.generation = dashboard.next_review_settings_generation();
        }
        self.probing = false;
        self.choices_loading = false;
        self.request_key = None;
    }

    pub(crate) fn start_initial_discovery(
        &mut self,
        dashboard: &mut DashboardState,
    ) -> DashboardAction {
        self.start_discovery(dashboard, ReviewSettingsDiscoveryKind::Profile)
    }

    fn apply_choices(
        &mut self,
        generation: u64,
        profile_id: &str,
        model: Option<&str>,
        choices: ReviewSettingsChoices,
    ) -> bool {
        if !self.probing
            || generation != self.generation
            || self.review.profile.as_deref() != Some(profile_id)
            || self.review.model.as_deref() != model
        {
            return false;
        }
        self.model_choices = choices.model_choices;
        self.effort_choices = choices.effort_choices;
        self.model_choices_discovered = true;
        self.effort_capabilities_discovered = choices.effort_capabilities_discovered;
        self.discovery_error = None;
        self.choices_loading = false;
        true
    }

    fn apply_discovery(
        &mut self,
        generation: u64,
        profile_id: &str,
        model: Option<&str>,
        result: Result<ReviewSettingsDiscoveryResult, String>,
    ) -> Option<ReviewSettingsChoices> {
        if !self.probing
            || generation != self.generation
            || self.review.profile.as_deref() != Some(profile_id)
            || self.review.model.as_deref() != model
        {
            return None;
        }
        self.probing = false;
        self.choices_loading = false;
        match result {
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices,
                cleanup_warning,
            }) => {
                self.model_choices = choices.model_choices.clone();
                self.effort_choices = choices.effort_choices.clone();
                self.model_choices_discovered = true;
                self.effort_capabilities_discovered = choices.effort_capabilities_discovered;
                self.cleanup_warning = cleanup_warning;
                self.discovery_error = None;
                Some(choices)
            }
            Ok(ReviewSettingsDiscoveryResult::Unavailable) => {
                self.cleanup_warning = None;
                self.discovery_error =
                    Some("A connected session is needed to refresh choices.".to_owned());
                None
            }
            Err(error) => {
                self.cleanup_warning = None;
                self.discovery_error = Some(error);
                None
            }
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        dashboard: &mut DashboardState,
        event: Event,
    ) -> (DashboardAction, ReviewSettingsOutcome) {
        use ReviewSettingsFocus::*;
        let result = self.form.get_mut().handle(&event);
        crate::record_form_outcome_cells(
            &dashboard.last_event_outcome,
            &dashboard.render_changed,
            &dashboard.render_change_revision,
            &result,
        );
        let changed = result.action.is_some();
        let interaction = self.combo.route(result.action);
        let dismiss = matches!(&interaction, Some(Interaction::Cancel));
        let back = matches!(&interaction, Some(Interaction::Activate(Back)));
        let cancel_setup = matches!(&interaction, Some(Interaction::Activate(Cancel)));
        let apply = matches!(&interaction, Some(Interaction::Activate(Save))) && self.can_save();
        let action = match interaction {
            Some(Interaction::Cancel | Interaction::Activate(Back | Cancel)) => {
                DashboardAction::CancelReviewSettingsDiscovery
            }
            Some(Interaction::Toggle(Enabled)) => {
                self.review.enabled = !self.review.enabled;
                dashboard.mark_render_changed();
                DashboardAction::None
            }
            Some(Interaction::ComboBoxCommit(Tier, index)) => {
                let tier = if index == 0 {
                    ReviewTier::Quick
                } else {
                    ReviewTier::Extended
                };
                if self.review.tier != tier {
                    self.review.tier = tier;
                    dashboard.mark_render_changed();
                }
                DashboardAction::None
            }
            Some(Interaction::ComboBoxCommit(Profile, index)) => {
                let profile = self.profiles.get(index).cloned().flatten();
                if self.review.profile == profile {
                    DashboardAction::None
                } else {
                    self.review.profile = profile;
                    dashboard.mark_render_changed();
                    self.start_discovery(dashboard, ReviewSettingsDiscoveryKind::Profile)
                }
            }
            Some(Interaction::ComboBoxCommit(Model, index)) => {
                let model = Self::choice_values(self.review.model.as_deref(), &self.model_choices)
                    .get(index)
                    .cloned()
                    .flatten();
                if self.review.model == model {
                    DashboardAction::None
                } else {
                    self.review.model = model;
                    dashboard.mark_render_changed();
                    self.start_discovery(dashboard, ReviewSettingsDiscoveryKind::Model)
                }
            }
            Some(Interaction::ComboBoxCommit(Effort, index)) => {
                let effort =
                    Self::choice_values(self.review.effort.as_deref(), &self.effort_choices)
                        .get(index)
                        .cloned()
                        .flatten();
                if self.review.effort != effort {
                    self.review.effort = effort;
                    dashboard.mark_render_changed();
                }
                DashboardAction::None
            }
            Some(Interaction::Activate(id @ (Tier | Profile | Model | Effort))) => {
                if let Some((_, _, _, selected)) = self
                    .selectors()
                    .into_iter()
                    .find(|(candidate, _, _, _)| *candidate == id)
                {
                    self.combo.open(id, selected);
                    dashboard.mark_render_changed();
                }
                DashboardAction::None
            }
            Some(Interaction::ComboBoxDismiss(Tier | Profile | Model | Effort)) => {
                dashboard.mark_render_changed();
                DashboardAction::None
            }
            Some(Interaction::Activate(Refresh)) => {
                if let Some(profile) = self.review.profile.as_deref() {
                    dashboard.clear_review_settings_choices(profile);
                }
                let action = self.start_discovery(dashboard, ReviewSettingsDiscoveryKind::Refresh);
                dashboard.mark_render_changed();
                action
            }
            Some(Interaction::Activate(Save)) if self.can_save() => {
                let had_pending = self.probing;
                if self.probing {
                    // Saving supersedes the background request. Invalidate its
                    // identity immediately so a queued progress/final reply
                    // from the cancellation cannot repopulate the cache.
                    self.generation = dashboard.next_review_settings_generation();
                    self.probing = false;
                    self.choices_loading = false;
                    self.request_key = None;
                }
                self.save_error = None;
                dashboard.mark_render_changed();
                if had_pending {
                    DashboardAction::CancelReviewSettingsDiscovery
                } else {
                    DashboardAction::None
                }
            }
            _ => DashboardAction::None,
        };
        if changed {
            self.prepare();
        }
        if cancel_setup || dismiss || back {
            self.review = self.original_review.clone();
        }
        let outcome = if cancel_setup {
            ReviewSettingsOutcome::CancelSetup
        } else if back || dismiss {
            ReviewSettingsOutcome::Back
        } else if apply {
            ReviewSettingsOutcome::Save
        } else {
            ReviewSettingsOutcome::Continue
        };
        (action, outcome)
    }
}

impl DashboardState {
    pub(crate) fn review_settings_discovery_active(&self) -> bool {
        fn active(mode: &Mode) -> bool {
            match mode {
                Mode::Setup(setup) => setup
                    .review_editor
                    .as_ref()
                    .is_some_and(|dialog| dialog.probing),
                Mode::Help(overlay) => active(&overlay.return_to),
                _ => false,
            }
        }
        active(&self.mode)
    }

    /// Removes every cached choice for one profile. The open dialog keeps its
    /// current values until the forced refresh settles.
    pub(crate) fn clear_review_settings_choices(&mut self, profile_id: &str) {
        self.review_settings_choices
            .retain(|(profile, _), _| profile != profile_id);
    }

    /// Keep cached choices only for profile definitions that are unchanged in
    /// the new configuration. Review edits alone therefore preserve the cache,
    /// while editing, removing, or replacing a profile invalidates its keys.
    pub(crate) fn invalidate_review_settings_choices_for_config(&mut self, config: &Config) {
        self.review_settings_choices.retain(|(profile, _), _| {
            self.config.profiles.get(profile) == config.profiles.get(profile)
        });
    }

    /// Publish adapter choices while cleanup continues. Successful progress is
    /// cached immediately so reopening the dialog can use it without waiting
    /// for the final cleanup result.
    pub fn apply_review_settings_choices(
        &mut self,
        generation: u64,
        profile_id: &str,
        model: Option<&str>,
        choices: ReviewSettingsChoices,
    ) -> bool {
        let Some(dialog) = review_settings_dialog_mut(&mut self.mode) else {
            return false;
        };
        let old = (
            dialog.model_choices.clone(),
            dialog.effort_choices.clone(),
            dialog.model_choices_discovered,
            dialog.effort_capabilities_discovered,
            dialog.discovery_error.clone(),
            dialog.choices_loading,
        );
        if !dialog.apply_choices(generation, profile_id, model, choices.clone()) {
            return false;
        }
        self.review_settings_choices
            .insert((profile_id.to_owned(), model.map(str::to_owned)), choices);
        dialog.prepare();
        let changed = old.0 != dialog.model_choices
            || old.1 != dialog.effort_choices
            || old.2 != dialog.model_choices_discovered
            || old.3 != dialog.effort_capabilities_discovered
            || old.4 != dialog.discovery_error
            || old.5 != dialog.choices_loading;
        if changed {
            self.mark_render_changed();
        }
        true
    }

    fn next_review_settings_generation(&mut self) -> u64 {
        self.review_settings_generation = self.review_settings_generation.wrapping_add(1);
        self.review_settings_generation
    }

    #[cfg(test)]
    pub(crate) fn begin_review_settings(&mut self) -> DashboardAction {
        self.begin_setup();
        self.begin_setup_review()
    }

    pub fn apply_review_settings_discovery(
        &mut self,
        generation: u64,
        profile_id: &str,
        model: Option<&str>,
        result: Result<ReviewSettingsDiscoveryResult, String>,
    ) -> bool {
        let Some(dialog) = review_settings_dialog_mut(&mut self.mode) else {
            return false;
        };
        if !dialog.probing
            || dialog.generation != generation
            || dialog.review.profile.as_deref() != Some(profile_id)
            || dialog.review.model.as_deref() != model
        {
            return false;
        }
        let key = (profile_id.to_owned(), model.map(str::to_owned));
        let old = (
            dialog.probing,
            dialog.choices_loading,
            dialog.model_choices.clone(),
            dialog.effort_choices.clone(),
            dialog.model_choices_discovered,
            dialog.effort_capabilities_discovered,
            dialog.cleanup_warning.clone(),
            dialog.discovery_error.clone(),
            dialog.save_error.clone(),
        );
        let choices = dialog.apply_discovery(generation, profile_id, model, result);
        dialog.request_key = None;
        if let Some(choices) = choices {
            self.review_settings_choices.insert(key, choices);
        }
        dialog.prepare();
        let changed = old.0 != dialog.probing
            || old.1 != dialog.choices_loading
            || old.2 != dialog.model_choices
            || old.3 != dialog.effort_choices
            || old.4 != dialog.model_choices_discovered
            || old.5 != dialog.effort_capabilities_discovered
            || old.6 != dialog.cleanup_warning
            || old.7 != dialog.discovery_error
            || old.8 != dialog.save_error;
        if changed {
            self.mark_render_changed();
        }
        true
    }
}

fn review_settings_dialog_mut(mode: &mut Mode) -> Option<&mut ReviewSettingsDialog> {
    match mode {
        Mode::Setup(setup) => setup.review_editor.as_deref_mut(),
        Mode::Help(overlay) => review_settings_dialog_mut(&mut overlay.return_to),
        _ => None,
    }
}

pub(crate) fn render_review_settings(
    frame: &mut Frame,
    popup: Rect,
    dialog: &ReviewSettingsDialog,
    setup_saving: bool,
) {
    use ReviewSettingsFocus::*;
    let spinner = dialog.animation_frame().unwrap_or("");
    let status = if dialog.probing && dialog.choices_loading {
        format!("{spinner} Loading choices…")
    } else if dialog.probing || dialog.model_choices_discovered {
        "Choices loaded".to_owned()
    } else if let Some(error) = &dialog.discovery_error {
        format!("Choices unavailable: {error}")
    } else if dialog.review.profile.is_none() {
        "Choose a reviewer profile".to_owned()
    } else {
        "Choices not loaded".to_owned()
    };
    let mut notes = vec![
        Line::styled(
            "Changes stay in the Setup draft until you save Setup.",
            theme::muted(),
        ),
        Line::styled(
            status,
            Style::default().fg(if dialog.choices_loading {
                theme::palette().accent
            } else {
                theme::palette().muted
            }),
        ),
    ];
    if let Some(warning) = &dialog.cleanup_warning {
        notes.push(Line::styled(
            format!("Cleanup warning: {warning}"),
            Style::default().fg(theme::palette().warning),
        ));
    }
    if let Some(error) = &dialog.discovery_error {
        notes.push(Line::styled(
            format!("Discovery: {error}"),
            Style::default().fg(theme::palette().warning),
        ));
    }
    if let Some(reason) = &dialog.read_only_reason {
        notes.push(Line::styled(
            reason.clone(),
            Style::default().fg(theme::palette().warning),
        ));
    }
    if let Some(error) = &dialog.save_error {
        notes.push(Line::styled(
            format!("Save failed: {error}"),
            Style::default().fg(theme::palette().warning),
        ));
    }
    if dialog.review.profile.is_none() && dialog.review.enabled {
        notes.push(Line::raw(
            "Choose a profile before enabling automatic review.",
        ));
    } else if dialog.review.enabled {
        if dialog.model_choices_discovered
            && dialog.review.model.as_deref().is_some_and(|model| {
                !dialog
                    .model_choices
                    .iter()
                    .any(|choice| choice.value == model)
            })
        {
            notes.push(Line::raw(
                "Selected model is unavailable in the discovered choices.",
            ));
        }
        if dialog.effort_capabilities_discovered
            && dialog.review.effort.as_deref().is_some_and(|effort| {
                !dialog
                    .effort_choices
                    .iter()
                    .any(|choice| choice.value == effort)
            })
        {
            notes.push(Line::raw(
                "Selected effort is unavailable in the discovered choices.",
            ));
        }
    }
    let description = Paragraph::new(match dialog.review.tier {
        ReviewTier::Quick => "One general reviewer; a validator checks any findings.",
        ReviewTier::Extended => "A supervisor selects specialist reviewers for deeper coverage.",
    })
    .style(Style::default().fg(theme::palette().muted))
    .wrap(Wrap { trim: true });
    let description_width = popup.width.saturating_sub(12);
    let description_height =
        u16::try_from(description.line_count(description_width.max(1))).unwrap_or(u16::MAX);
    let focus_row = match dialog.focused() {
        Enabled => 0,
        Tier => 1,
        Profile => 2 + description_height,
        Model => 3 + description_height,
        Effort => 4 + description_height,
        _ => 0,
    };
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Setup › Code Review",
        theme::title(true),
        !setup_saving && !dialog.saving && dialog.combo.open_id().is_none(),
    );
    frame.render_widget(theme::modal().title(title), popup);
    let inner = theme::modal().inner(popup);
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let viewport = FormViewport::new(
        body,
        (notes.len() as u16)
            .saturating_add(6)
            .saturating_add(description_height),
        dialog.scroll.get(),
        Some(focus_row),
    );
    dialog.scroll.set(viewport.offset());
    let row = |index: u16| viewport.row(index, 1);
    Checkbox::render(
        frame,
        row(0),
        "Automatic review",
        dialog.review.enabled,
        !dialog.saving,
        &mut form,
        Enabled,
    );
    let mut expanded_combo = None;
    for (index, (id, label, values, selected)) in dialog.selectors().iter().enumerate() {
        let area = row(index as u16 + 1 + if index > 0 { description_height } else { 0 });
        let label_width = 10.min(area.width);
        let loading = dialog.probing && dialog.choices_loading && (*id == Model || *id == Effort);
        frame.render_widget(
            Line::raw(if loading {
                format!("{label} {spinner}")
            } else {
                (*label).to_owned()
            }),
            Rect::new(area.x, area.y, label_width, area.height),
        );
        let field = Rect::new(
            area.x + label_width,
            area.y,
            area.width - label_width,
            area.height,
        );
        let selected = dialog.combo.selection(*id, *selected);
        let value = values.get(selected).cloned().unwrap_or_default();
        let options = values.iter().cloned().map(Line::raw).collect::<Vec<_>>();
        ComboBox::render(
            frame,
            inner,
            field,
            &value,
            &options,
            selected,
            false,
            !dialog.saving,
            " values · ↑/↓ select · Tab/Enter accept ",
            PopupSide::Below,
            &mut form,
            *id,
        );
        if dialog.combo.is_open(*id) {
            expanded_combo = Some((*id, field, value, options, selected));
        }
    }
    let help_area = viewport.row(2, description_height);
    let indent = 10.min(help_area.width);
    frame.render_widget(
        description.scroll((viewport.offset().saturating_sub(2), 0)),
        Rect::new(
            help_area.x + indent,
            help_area.y,
            help_area.width - indent,
            help_area.height,
        ),
    );
    for (index, line) in notes.into_iter().enumerate() {
        frame.render_widget(line, row(index as u16 + 6 + description_height));
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
                "Tab moves · Enter opens choices · Space toggles · Esc goes back",
                Style::default().fg(theme::palette().muted),
            ),
            Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
        );
    }
    Dialog::render_actions(
        frame,
        footer,
        &[
            (Refresh, "Refresh choices", !dialog.saving),
            (Back, "Back", true),
            (Cancel, "Cancel", true),
            (
                Save,
                if dialog.saving {
                    "Saving…"
                } else {
                    "Save Setup"
                },
                dialog.can_save(),
            ),
        ],
        &mut form,
    );
    if let Some((id, field, value, options, selected)) = expanded_combo {
        ComboBox::render(
            frame,
            inner,
            field,
            &value,
            &options,
            selected,
            true,
            !dialog.saving,
            " values · ↑/↓ select · Tab/Enter accept ",
            PopupSide::Below,
            &mut form,
            id,
        );
    }
    form.end_frame(Enabled);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::CommandId;
    use crate::test_support::{buffer_lines, config, dashboard_with_session, key, running_session};
    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    fn open(dashboard: &mut DashboardState) -> DashboardAction {
        dashboard.begin_review_settings()
    }

    fn dialog(dashboard: &DashboardState) -> &ReviewSettingsDialog {
        let Mode::Setup(setup) = &dashboard.mode else {
            panic!("expected review settings dialog")
        };
        setup.review_editor.as_ref().expect("review child")
    }

    fn choice(value: &str) -> SessionConfigChoice {
        SessionConfigChoice {
            value: value.to_owned(),
            name: value.to_owned(),
            description: None,
        }
    }

    fn choose_next(dashboard: &mut DashboardState) -> DashboardAction {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Down)),
            DashboardAction::None
        );
        dashboard.handle_key(key(KeyCode::Enter))
    }

    fn choose_first(dashboard: &mut DashboardState) -> DashboardAction {
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Home)),
            DashboardAction::None
        );
        dashboard.handle_key(key(KeyCode::Enter))
    }

    fn available(
        model_choices: &[&str],
        effort_choices: &[&str],
        effort_known: bool,
    ) -> ReviewSettingsDiscoveryResult {
        ReviewSettingsDiscoveryResult::Available {
            choices: ReviewSettingsChoices {
                model_choices: model_choices.iter().map(|value| choice(value)).collect(),
                effort_choices: effort_choices.iter().map(|value| choice(value)).collect(),
                effort_capabilities_discovered: effort_known,
            },
            cleanup_warning: None,
        }
    }

    #[test]
    fn review_selectors_render_as_comboboxes_and_escape_closes_only_the_popup() {
        let mut dashboard = dashboard_with_session(running_session());
        open(&mut dashboard);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let text = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(text.matches(ComboBox::GLYPH).count() >= 4, "{text}");

        while dialog(&dashboard).focused() != ReviewSettingsFocus::Tier {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert!(dialog(&dashboard).combo.is_open(ReviewSettingsFocus::Tier));
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
        assert!(dialog(&dashboard).combo.open_id().is_none());
    }

    #[test]
    fn progress_choices_are_cached_before_cleanup_and_stale_replies_are_ignored() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.config.review.profile = Some("codex-1".into());
        dashboard.config.review.enabled = true;
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("configured profile must start discovery")
        };
        assert!(dashboard.needs_fast_tick());
        assert!(dashboard.apply_review_settings_choices(
            generation,
            &profile_id,
            model.as_deref(),
            ReviewSettingsChoices::default(),
        ));
        assert!(dialog(&dashboard).model_choices_discovered);
        assert!(dialog(&dashboard).probing);
        assert!(
            dashboard
                .review_settings_choices
                .contains_key(&(profile_id.clone(), model.clone()))
        );
        dashboard.handle_key(key(KeyCode::Esc));
        assert!(!dashboard.needs_fast_tick());
        open(&mut dashboard);
        assert!(!dashboard.apply_review_settings_choices(
            generation,
            &profile_id,
            model.as_deref(),
            ReviewSettingsChoices::default(),
        ));
        assert!(dialog(&dashboard).model_choices_discovered);
    }

    #[test]
    fn review_settings_is_available_through_setup_without_a_selected_session() {
        let mut dashboard = DashboardState::new(
            config(),
            mj_core::state::State::default(),
            Default::default(),
        );
        let action = open(&mut dashboard);
        assert!(matches!(action, DashboardAction::None));
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
        assert_eq!(dashboard.selected_session_id(), None);

        let mut dashboard = DashboardState::new(
            config(),
            mj_core::state::State::default(),
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
                .any(|entry| entry.id == CommandId::OpenConfig)
        );
    }

    #[test]
    fn clearing_the_profile_cancels_discovery_without_closing_the_draft() {
        let mut dashboard = dashboard_with_session(running_session());
        open(&mut dashboard);
        dashboard.handle_key(key(KeyCode::Char(' ')));
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            choose_next(&mut dashboard),
            DashboardAction::DiscoverReviewSettings { .. }
        ));
        assert!(matches!(
            choose_first(&mut dashboard),
            DashboardAction::CancelReviewSettingsDiscovery
        ));
        assert!(dialog(&dashboard).review.profile.is_none());
        assert!(dialog(&dashboard).review.enabled);
        assert!(!dialog(&dashboard).can_save());
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(dashboard.dialog_confirmation_open());
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Esc)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
    }

    #[test]
    fn edit_and_save_action_contains_only_global_review_values() {
        let mut dashboard = dashboard_with_session(running_session());
        let _ = open(&mut dashboard);
        // Enabled -> Tier -> Profile, then choose the first configured profile.
        dashboard.handle_key(key(KeyCode::Tab));
        dashboard.handle_key(key(KeyCode::Tab));
        let probe = choose_next(&mut dashboard);
        assert!(matches!(
            probe,
            DashboardAction::DiscoverReviewSettings { .. }
        ));

        while dialog(&dashboard).focused() != ReviewSettingsFocus::Tier {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        choose_next(&mut dashboard);
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        let action = dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, DashboardAction::SaveSetup { .. }));
        assert!(matches!(dashboard.mode, Mode::Setup(_)));
    }

    #[test]
    fn stale_discovery_does_not_replace_choices_after_model_change() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let initial = open(&mut dashboard);
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = initial
        else {
            panic!("expected initial probe")
        };
        dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices {
                    model_choices: vec![SessionConfigChoice {
                        value: "model-a".into(),
                        name: "Model A".into(),
                        description: None,
                    }],
                    effort_choices: vec![],
                    effort_capabilities_discovered: false,
                },
                cleanup_warning: None,
            }),
        );
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Model {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Down)),
            DashboardAction::None,
            "previewing a model must not start discovery"
        );
        assert_eq!(dialog(&dashboard).generation, generation);
        let model_action = dashboard.handle_key(key(KeyCode::Enter));
        let DashboardAction::DiscoverReviewSettings {
            generation: newer,
            profile_id,
            model,
        } = model_action
        else {
            panic!("expected model probe")
        };
        assert!(newer > generation);
        assert!(dialog(&dashboard).probing);
        assert_eq!(dialog(&dashboard).model_choices[0].value, "model-a");
        assert!(dialog(&dashboard).effort_choices.is_empty());
        dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            None,
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices {
                    model_choices: vec![SessionConfigChoice {
                        value: "stale".into(),
                        name: "Stale".into(),
                        description: None,
                    }],
                    effort_choices: vec![],
                    effort_capabilities_discovered: false,
                },
                cleanup_warning: None,
            }),
        );
        assert_eq!(dialog(&dashboard).model_choices[0].value, "model-a");
        assert!(dialog(&dashboard).probing);
        dashboard.apply_review_settings_discovery(
            newer,
            &profile_id,
            model.as_deref(),
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices::default(),
                cleanup_warning: None,
            }),
        );
        assert!(!dialog(&dashboard).probing);
    }

    #[test]
    fn selecting_the_current_value_does_not_restart_pending_discovery() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let initial = open(&mut dashboard);
        assert!(matches!(
            initial,
            DashboardAction::DiscoverReviewSettings { .. }
        ));
        let generation = dialog(&dashboard).generation;
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Model {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(choose_first(&mut dashboard), DashboardAction::None);
        assert!(dialog(&dashboard).probing);
        assert_eq!(dialog(&dashboard).generation, generation);
    }

    #[test]
    fn effort_change_does_not_restart_discovery() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let DashboardAction::DiscoverReviewSettings { generation, .. } = open(&mut dashboard)
        else {
            panic!("expected initial probe")
        };
        dashboard.apply_review_settings_discovery(
            generation,
            "codex-1",
            None,
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices {
                    model_choices: vec![],
                    effort_choices: vec![SessionConfigChoice {
                        value: "low".into(),
                        name: "Low".into(),
                        description: None,
                    }],
                    effort_capabilities_discovered: true,
                },
                cleanup_warning: None,
            }),
        );
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Effort {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        let action = choose_next(&mut dashboard);
        assert_eq!(action, DashboardAction::None);
        assert!(!dialog(&dashboard).probing);
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
    fn closing_and_reopening_uses_cached_choices() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected initial probe")
        };
        if let Mode::Setup(setup) = &mut dashboard.mode {
            setup
                .review_editor
                .as_mut()
                .expect("review editor")
                .form
                .get_mut()
                .focus(ReviewSettingsFocus::Back);
        } else {
            panic!("setup remains open after discovery");
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::CancelReviewSettingsDiscovery
        );
        dashboard.cancel_modal();
        assert!(!dashboard.modal_open());

        assert!(!dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices {
                    model_choices: vec![SessionConfigChoice {
                        value: "old".into(),
                        name: "Old".into(),
                        description: None,
                    }],
                    effort_choices: vec![],
                    effort_capabilities_discovered: false,
                },
                cleanup_warning: None,
            }),
        ));
        let DashboardAction::DiscoverReviewSettings {
            generation: reopened,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected reopened discovery")
        };
        dashboard.apply_review_settings_discovery(
            reopened,
            &profile_id,
            model.as_deref(),
            Ok(ReviewSettingsDiscoveryResult::Available {
                choices: ReviewSettingsChoices {
                    model_choices: vec![SessionConfigChoice {
                        value: "old".into(),
                        name: "Old".into(),
                        description: None,
                    }],
                    effort_choices: vec![],
                    effort_capabilities_discovered: false,
                },
                cleanup_warning: None,
            }),
        );
        dashboard.cancel_modal();
        let reopened = open(&mut dashboard);
        assert_eq!(reopened, DashboardAction::None);
        assert!(dialog(&dashboard).model_choices_discovered);
        assert_eq!(dialog(&dashboard).model_choices[0].value, "old");
    }

    #[test]
    fn refresh_clears_only_the_profile_cache_and_retains_current_choices() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        dashboard.review_settings_choices.insert(
            ("codex-1".into(), None),
            ReviewSettingsChoices {
                model_choices: vec![choice("tiny")],
                effort_choices: vec![choice("high")],
                effort_capabilities_discovered: true,
            },
        );
        assert_eq!(open(&mut dashboard), DashboardAction::None);
        assert_eq!(dialog(&dashboard).model_choices[0].value, "tiny");
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Refresh {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        let action = dashboard.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            action,
            DashboardAction::DiscoverReviewSettings { model: None, .. }
        ));
        assert!(
            !dashboard
                .review_settings_choices
                .contains_key(&("codex-1".into(), None))
        );
        assert_eq!(dialog(&dashboard).model_choices[0].value, "tiny");
        assert!(dialog(&dashboard).probing);
        assert!(dialog(&dashboard).choices_loading);
    }

    #[test]
    fn profile_definition_changes_invalidate_cache_but_review_edits_do_not() {
        let mut dashboard = DashboardState::new(
            config(),
            mj_core::state::State::default(),
            Default::default(),
        );
        let key = ("codex-1".to_owned(), None);
        dashboard
            .review_settings_choices
            .insert(key.clone(), ReviewSettingsChoices::default());
        let mut review_edit = dashboard.config.clone();
        review_edit.review.enabled = true;
        dashboard.set_config(review_edit);
        assert!(dashboard.review_settings_choices.contains_key(&key));

        let mut profile_edit = dashboard.config.clone();
        profile_edit.profiles.get_mut("codex-1").unwrap().home = "/changed".into();
        dashboard.set_config(profile_edit);
        assert!(!dashboard.review_settings_choices.contains_key(&key));
    }

    #[test]
    fn matching_discovery_replies_apply_through_help() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected discovery")
        };
        dashboard.begin_help();
        assert!(dashboard.apply_review_settings_choices(
            generation,
            &profile_id,
            model.as_deref(),
            ReviewSettingsChoices {
                model_choices: vec![choice("tiny")],
                effort_choices: vec![],
                effort_capabilities_discovered: false,
            },
        ));
        let Mode::Help(overlay) = &dashboard.mode else {
            panic!("expected help")
        };
        let Mode::Setup(setup) = overlay.return_to.as_ref() else {
            panic!("help must cover review settings")
        };
        let dialog = setup.review_editor.as_ref().expect("review child");
        assert!(dialog.model_choices_discovered);
        assert!(dialog.probing);
        assert!(
            dashboard
                .review_settings_choices
                .contains_key(&(profile_id, model))
        );
    }

    #[test]
    fn final_cleanup_warning_keeps_choices_and_zero_effort_is_known() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected discovery")
        };
        let mut final_result = available(&["tiny"], &[], true);
        if let ReviewSettingsDiscoveryResult::Available {
            cleanup_warning, ..
        } = &mut final_result
        {
            *cleanup_warning = Some("worker cleanup timed out".into());
        }
        assert!(dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Ok(final_result),
        ));
        assert!(!dialog(&dashboard).probing);
        assert_eq!(
            dialog(&dashboard).cleanup_warning.as_deref(),
            Some("worker cleanup timed out")
        );
        assert!(dialog(&dashboard).model_choices_discovered);
        assert!(dialog(&dashboard).effort_capabilities_discovered);
        assert!(dialog(&dashboard).can_save());
    }

    #[test]
    fn known_unsupported_values_disable_save_but_unknown_discovery_does_not() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        config.review.enabled = true;
        config.review.model = Some("missing".into());
        config.review.effort = Some("missing".into());
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected discovery")
        };
        assert!(dialog(&dashboard).can_save());
        assert!(dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Ok(available(&["tiny"], &["high"], true)),
        ));
        assert!(!dialog(&dashboard).can_save());
        if let Mode::Setup(setup) = &mut dashboard.mode {
            setup
                .review_editor
                .as_mut()
                .expect("review editor")
                .form
                .get_mut()
                .focus(ReviewSettingsFocus::Back);
        } else {
            panic!("setup remains open after discovery");
        }
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::CancelReviewSettingsDiscovery
        );
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(action, DashboardAction::None);
        let Mode::Setup(setup) = &dashboard.mode else {
            panic!("setup remains open after rejected save")
        };
        assert!(
            setup
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("unavailable"))
        );
    }

    #[test]
    fn save_is_local_while_loading_unavailable_or_failed() {
        let mut config = config();
        config.review.profile = Some("codex-1".into());
        config.review.enabled = true;
        let mut dashboard =
            DashboardState::new(config, mj_core::state::State::default(), Default::default());
        assert!(matches!(
            open(&mut dashboard),
            DashboardAction::DiscoverReviewSettings { .. }
        ));
        while dialog(&dashboard).focused() != ReviewSettingsFocus::Save {
            dashboard.handle_key(key(KeyCode::Tab));
        }
        assert!(matches!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::SaveSetup { .. }
        ));
        dashboard.cancel_modal();

        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected rediscovery")
        };
        assert!(dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Ok(ReviewSettingsDiscoveryResult::Unavailable),
        ));
        assert!(dialog(&dashboard).can_save());
        dashboard.cancel_modal();

        let DashboardAction::DiscoverReviewSettings {
            generation,
            profile_id,
            model,
        } = open(&mut dashboard)
        else {
            panic!("expected rediscovery")
        };
        assert!(dashboard.apply_review_settings_discovery(
            generation,
            &profile_id,
            model.as_deref(),
            Err("offline".to_owned()),
        ));
        assert!(dialog(&dashboard).can_save());
    }
}
