//! The review settings editor. Capability choices come from the dashboard's
//! published catalog; the editor never performs discovery on the event loop.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crossterm::event::Event;
use mj_chat::components::{ComboBox, ComboBoxState, ControlKind, Dialog, Interaction, PopupSide};
use mj_chat::theme;
use mj_core::acp::SessionConfigChoice;
use mj_core::config::{Config, MAX_REVIEWERS, ReviewConfig, SubagentChoice};
use mj_core::profile_capabilities::ProfileCapabilitiesSnapshot;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::widgets::dismissible_modal_title;
use crate::{DashboardAction, DashboardState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReviewSettingsFocus {
    Model(usize),
    Effort(usize),
    Remove(usize),
    Add,
    Autoconfigure,
    Back,
    Cancel,
    Save,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewSettingsOutcome {
    Continue,
    Back,
    CancelSetup,
    Save,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewSettingsValidation {
    reviewers: Vec<SubagentChoice>,
    legacy: bool,
    models: Vec<SessionConfigChoice>,
    models_known: bool,
    efforts: Vec<Vec<SessionConfigChoice>>,
    efforts_known: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewSettingsDialog {
    pub(crate) review: ReviewConfig,
    original_review: ReviewConfig,
    pub(crate) form: RefCell<Dialog<ReviewSettingsFocus>>,
    combo: ComboBoxState<ReviewSettingsFocus>,
    models: Vec<SessionConfigChoice>,
    efforts: Vec<Vec<SessionConfigChoice>>,
    models_known: bool,
    models_partial: bool,
    efforts_known: Vec<bool>,
    efforts_partial: Vec<bool>,
    effort_failures: Vec<String>,
    suggested: Option<Vec<SubagentChoice>>,
    suggested_adjustments: Vec<mj_core::review::autoconfigure::EffortAdjustment>,
    pending_autoconfigure: Option<(Vec<SubagentChoice>, Instant)>,
    effort_notice: Option<String>,
    pub(crate) saving: bool,
    pub(crate) save_error: Option<String>,
}

impl ReviewSettingsDialog {
    pub(crate) fn new(config: &Config) -> Self {
        let dialog = Self {
            review: config.review.clone(),
            original_review: config.review.clone(),
            form: RefCell::new(Dialog::default()),
            combo: ComboBoxState::default(),
            models: Vec::new(),
            efforts: Vec::new(),
            models_known: false,
            models_partial: false,
            efforts_known: Vec::new(),
            efforts_partial: Vec::new(),
            effort_failures: Vec::new(),
            suggested: None,
            suggested_adjustments: Vec::new(),
            pending_autoconfigure: None,
            effort_notice: None,
            saving: false,
            save_error: None,
        };
        dialog.prepare();
        dialog
    }

    pub(crate) fn prepare_dialog_state(&mut self) {
        let form = self.form.get_mut();
        form.set_action_role(
            ReviewSettingsFocus::Cancel,
            mj_chat::components::ActionRole::Cancel,
        );
        form.set_action_role(
            ReviewSettingsFocus::Back,
            mj_chat::components::ActionRole::Back,
        );
        form.set_dirty(false);
        form.set_dismiss_actions(&[ReviewSettingsFocus::Cancel]);
        form.set_default_action(ReviewSettingsFocus::Save);
    }

    pub(crate) fn update_choices(
        &mut self,
        config: &Config,
        snapshot: &ProfileCapabilitiesSnapshot,
        usage_priced: &BTreeSet<String>,
    ) {
        let models = snapshot.options(config, None);
        let offers = snapshot.model_offers(config, |profile| usage_priced.contains(profile));
        self.models_known = models.is_some();
        if let Some(options) = models {
            self.models_partial = !options.unavailable.is_empty();
            self.models = options.models;
            let configured = mj_core::review::autoconfigure::autoconfigure(&offers, |model| {
                known_efforts(snapshot, config, model)
            });
            self.suggested_adjustments = configured
                .as_ref()
                .map(|configured| configured.adjusted.clone())
                .unwrap_or_default();
            self.suggested = configured.map(|configured| configured.reviewers);
        } else {
            self.models.clear();
            self.models_partial = false;
            self.suggested = None;
            self.suggested_adjustments.clear();
        }
        if let Some((draft, started)) = &self.pending_autoconfigure {
            let pending_choices = self.suggested.as_ref().unwrap_or(draft);
            let ready = pending_choices.iter().all(|choice| {
                snapshot
                    .options(config, Some(&choice.model))
                    .is_some_and(|options| {
                        snapshot.effort_blockers(config, &choice.model).is_empty()
                            || choice.effort.as_deref().is_some_and(|effort| {
                                options.efforts.iter().any(|item| item.value == effort)
                            })
                    })
            });
            if ready || started.elapsed() >= Duration::from_secs(20) {
                let configured = mj_core::review::autoconfigure::autoconfigure(&offers, |model| {
                    known_efforts(snapshot, config, model)
                });
                let mut resolved = configured
                    .as_ref()
                    .map(|result| result.reviewers.clone())
                    .unwrap_or_else(|| draft.clone());
                let adjustments = configured.map(|result| result.adjusted).unwrap_or_default();
                let mut notices = Vec::new();
                if !adjustments.is_empty() {
                    notices.push(format!(
                        "Adjusted reviewer efforts: {}",
                        adjustments
                            .iter()
                            .map(|item| format!(
                                "{}: {} → {}",
                                item.model,
                                item.requested,
                                item.used.as_deref().unwrap_or("default")
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let mut failures = Vec::new();
                for choice in &mut resolved {
                    let offered = snapshot.options(config, Some(&choice.model));
                    let blockers = snapshot.effort_blockers(config, &choice.model);
                    let unknown = offered.as_ref().is_none_or(|options| {
                        !blockers.is_empty()
                            && choice.effort.as_deref().is_none_or(|effort| {
                                !options.efforts.iter().any(|item| item.value == effort)
                            })
                    });
                    if unknown {
                        choice.effort = None;
                        for reason in blockers {
                            let reason = if reason.contains("discovery pending") {
                                format!(
                                    "{} (timed out)",
                                    reason.replace(": effort discovery pending", "")
                                )
                            } else {
                                reason
                            };
                            if !failures.contains(&reason) {
                                failures.push(reason);
                            }
                        }
                    }
                }
                if !failures.is_empty() {
                    notices.push(format!(
                        "Reviewer effort discovery failed: {}; using defaults.",
                        failures.join("; ")
                    ));
                }
                self.effort_notice = (!notices.is_empty()).then(|| notices.join(" "));
                self.review.reviewers = resolved;
                self.review.clear_legacy();
                self.pending_autoconfigure = None;
            }
        }
        self.efforts.clear();
        self.efforts_known.clear();
        self.efforts_partial.clear();
        self.effort_failures.clear();
        for reviewer in &self.review.reviewers {
            match snapshot.options(config, Some(&reviewer.model)) {
                Some(options) => {
                    self.effort_failures.push(options.unavailable.join("; "));
                    self.efforts_partial.push(!options.unavailable.is_empty());
                    self.efforts.push(options.efforts);
                    self.efforts_known.push(true);
                }
                None => {
                    self.efforts.push(Vec::new());
                    self.effort_failures.push(String::new());
                    self.efforts_partial.push(false);
                    self.efforts_known.push(false);
                }
            }
        }
        self.prepare();
    }

    pub(crate) fn checking_efforts(&self) -> bool {
        self.pending_autoconfigure.is_some()
    }

    pub(crate) fn pending_autoconfigure_choices(&self) -> Option<&[SubagentChoice]> {
        self.pending_autoconfigure
            .as_ref()
            .map(|(draft, _)| self.suggested.as_deref().unwrap_or(draft))
    }

    fn suggested(&self) -> Option<Vec<SubagentChoice>> {
        self.suggested.clone()
    }

    fn effort_unavailable(&self, i: usize) -> bool {
        let Some(effort) = self.review.reviewers[i].effort.as_deref() else {
            return false;
        };
        self.efforts_known.get(i) == Some(&true)
            && self.efforts_partial.get(i) != Some(&true)
            && !self.efforts[i].iter().any(|choice| choice.value == effort)
    }

    pub(crate) fn can_save(&self) -> bool {
        if self.saving
            || self.pending_autoconfigure.is_some()
            || self.review.reviewers.len() > MAX_REVIEWERS
        {
            return false;
        }
        self.review
            .reviewers
            .iter()
            .enumerate()
            .all(|(i, reviewer)| {
                !reviewer.model.trim().is_empty()
                    && (!self.models_known
                        || self.models_partial
                        || self
                            .models
                            .iter()
                            .any(|choice| choice.value == reviewer.model))
                    && !self.effort_unavailable(i)
            })
    }

    pub(crate) fn validation_snapshot(&self) -> ReviewSettingsValidation {
        ReviewSettingsValidation {
            reviewers: self.review.reviewers.clone(),
            legacy: self.review.is_legacy(),
            models: self.models.clone(),
            models_known: self.models_known && !self.models_partial,
            efforts: self.efforts.clone(),
            efforts_known: self
                .efforts_known
                .iter()
                .zip(&self.efforts_partial)
                .map(|(known, partial)| *known && !*partial)
                .collect(),
        }
    }

    pub(crate) fn validation_error(
        snapshot: &ReviewSettingsValidation,
        review: &ReviewConfig,
    ) -> Option<String> {
        if review.is_legacy() && snapshot.legacy {
            return None;
        }
        if review.reviewers.len() > MAX_REVIEWERS {
            return Some("Choose at most 3 reviewers.".into());
        }
        if snapshot.reviewers != review.reviewers {
            return None;
        }
        for (i, reviewer) in review.reviewers.iter().enumerate() {
            if reviewer.model.trim().is_empty()
                || (snapshot.models_known
                    && !snapshot
                        .models
                        .iter()
                        .any(|choice| choice.value == reviewer.model))
            {
                return Some(format!("Reviewer {} model is unavailable.", i + 1));
            }
            if let Some(effort) = reviewer.effort.as_deref()
                && snapshot.efforts_known.get(i) == Some(&true)
                && !snapshot.efforts[i]
                    .iter()
                    .any(|choice| choice.value == effort)
            {
                return Some(format!("Reviewer {} effort is unavailable.", i + 1));
            }
        }
        None
    }

    fn choices(&self, id: ReviewSettingsFocus) -> (Vec<String>, usize) {
        use ReviewSettingsFocus::*;
        match id {
            Model(i) => {
                let current = &self.review.reviewers[i].model;
                let mut values: Vec<String> = self
                    .models
                    .iter()
                    .map(|choice| choice.value.clone())
                    .collect();
                if !values.contains(current) {
                    values.insert(0, current.clone());
                }
                let selected = values
                    .iter()
                    .position(|value| value == current)
                    .unwrap_or(0);
                (values, selected)
            }
            Effort(i) => {
                let current = self.review.reviewers[i].effort.as_deref();
                let mut values = vec!["Default".into()];
                values.extend(
                    self.efforts
                        .get(i)
                        .into_iter()
                        .flatten()
                        .map(|choice| choice.value.clone()),
                );
                if let Some(current) = current
                    && !values.iter().any(|value| value == current)
                {
                    values.insert(1, current.into());
                }
                let selected = current
                    .and_then(|current| values.iter().position(|value| value == current))
                    .unwrap_or(0);
                (values, selected)
            }
            _ => (Vec::new(), 0),
        }
    }

    fn selectors(&self) -> Vec<(ReviewSettingsFocus, String)> {
        let mut selectors = Vec::new();
        for i in 0..self.review.reviewers.len() {
            selectors.push((
                ReviewSettingsFocus::Model(i),
                format!("Reviewer {} model", i + 1),
            ));
            selectors.push((
                ReviewSettingsFocus::Effort(i),
                format!("Reviewer {} effort", i + 1),
            ));
        }
        selectors
    }

    pub(crate) fn prepare(&self) {
        use ReviewSettingsFocus::*;
        let mut form = self.form.borrow_mut();
        form.begin_update();
        for (id, _) in self.selectors() {
            let (values, selected) = self.choices(id);
            form.declare_with_enabled(
                id,
                ControlKind::ComboBox {
                    len: values.len(),
                    selected: self.combo.selection(id, selected),
                    expanded: self.combo.is_open(id),
                },
                !self.saving && self.pending_autoconfigure.is_none(),
            );
            if let Effort(i) = id {
                form.declare_with_enabled(
                    Remove(i),
                    ControlKind::Button,
                    !self.saving && self.pending_autoconfigure.is_none(),
                );
            }
        }
        form.declare_with_enabled(
            Add,
            ControlKind::Button,
            !self.saving
                && self.pending_autoconfigure.is_none()
                && self.review.reviewers.len() < MAX_REVIEWERS
                && !self.models.is_empty(),
        );
        form.declare_with_enabled(
            Autoconfigure,
            ControlKind::Button,
            !self.saving && self.pending_autoconfigure.is_none() && self.suggested().is_some(),
        );
        form.declare_with_enabled(Back, ControlKind::Button, true);
        form.declare_with_enabled(Cancel, ControlKind::Button, true);
        form.declare_with_enabled(Save, ControlKind::Button, self.can_save());
        form.end_frame(self.initial_focus());
    }

    /// Where focus starts: the first reviewer, or the button that adds one.
    fn initial_focus(&self) -> ReviewSettingsFocus {
        if self.review.reviewers.is_empty() {
            ReviewSettingsFocus::Add
        } else {
            ReviewSettingsFocus::Model(0)
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        dashboard: &mut DashboardState,
        config: &Config,
        event: Event,
    ) -> (DashboardAction, ReviewSettingsOutcome) {
        use ReviewSettingsFocus::*;
        let result = self.form.get_mut().handle(&event);
        dashboard.last_event_consumed.set(result.consumed);
        let interaction = self.combo.route(result.action);
        let mut action = DashboardAction::None;
        let outcome = match &interaction {
            Some(Interaction::Cancel | Interaction::Activate(Back)) => ReviewSettingsOutcome::Back,
            Some(Interaction::Activate(Cancel)) => ReviewSettingsOutcome::CancelSetup,
            Some(Interaction::Activate(Save)) if self.can_save() => ReviewSettingsOutcome::Save,
            _ => ReviewSettingsOutcome::Continue,
        };
        match interaction {
            Some(Interaction::ComboBoxCommit(Model(i), index)) => {
                if let Some(value) = self.choices(Model(i)).0.get(index).cloned() {
                    self.review.reviewers[i].model = value;
                    self.review.reviewers[i].effort = None;
                    self.review.clear_legacy();
                    self.update_choices(
                        config,
                        &dashboard.profile_capabilities,
                        &dashboard.usage_priced_profiles(config),
                    );
                }
            }
            Some(Interaction::ComboBoxCommit(Effort(i), index)) => {
                if let Some(value) = self.choices(Effort(i)).0.get(index) {
                    self.review.reviewers[i].effort = (index != 0).then(|| value.clone());
                    self.review.clear_legacy();
                }
            }
            Some(Interaction::Activate(Add)) if self.review.reviewers.len() < MAX_REVIEWERS => {
                if let Some(model) = self.models.first() {
                    self.review.reviewers.push(SubagentChoice {
                        model: model.value.clone(),
                        effort: None,
                    });
                    self.review.clear_legacy();
                    self.update_choices(
                        config,
                        &dashboard.profile_capabilities,
                        &dashboard.usage_priced_profiles(config),
                    );
                }
            }
            Some(Interaction::Activate(Remove(i))) if i < self.review.reviewers.len() => {
                self.review.reviewers.remove(i);
                self.review.clear_legacy();
                self.update_choices(
                    config,
                    &dashboard.profile_capabilities,
                    &dashboard.usage_priced_profiles(config),
                );
            }
            Some(Interaction::Activate(Autoconfigure)) => {
                // The cached suggestion may predate the latest quota reports,
                // which decide which profiles are billed per use.
                self.update_choices(
                    config,
                    &dashboard.profile_capabilities,
                    &dashboard.usage_priced_profiles(config),
                );
                if let Some(reviewers) = self.suggested() {
                    self.effort_notice = (!self.suggested_adjustments.is_empty()).then(|| {
                        format!(
                            "Adjusted reviewer efforts: {}",
                            self.suggested_adjustments
                                .iter()
                                .map(|item| format!(
                                    "{}: {} → {}",
                                    item.model,
                                    item.requested,
                                    item.used.as_deref().unwrap_or("default")
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    });
                    let mut config = config.clone();
                    config.review.reviewers = reviewers.clone();
                    config.review.clear_legacy();
                    let unknown = reviewers.iter().any(|choice| {
                        dashboard
                            .profile_capabilities
                            .options(&config, Some(&choice.model))
                            .is_none_or(|options| {
                                !dashboard
                                    .profile_capabilities
                                    .effort_blockers(&config, &choice.model)
                                    .is_empty()
                                    && choice.effort.as_deref().is_some_and(|effort| {
                                        !options.efforts.iter().any(|item| item.value == effort)
                                    })
                            })
                    });
                    if unknown {
                        let key = config.capability_demand_key();
                        dashboard.profile_hydration_sent = Some(key.clone());
                        dashboard
                            .profile_hydration_requests
                            .insert(key.clone(), Default::default());
                        action = DashboardAction::WarmProfileCapabilities {
                            key,
                            config: serde_json::to_string(&config).expect("config serializes"),
                        };
                        self.pending_autoconfigure = Some((reviewers, Instant::now()));
                    } else {
                        self.review.reviewers = reviewers;
                        self.review.clear_legacy();
                    }
                    self.update_choices(
                        &config,
                        &dashboard.profile_capabilities,
                        &dashboard.usage_priced_profiles(&config),
                    );
                }
            }
            Some(Interaction::Activate(id @ (Model(_) | Effort(_)))) => {
                self.combo.open(id, self.choices(id).1)
            }
            Some(Interaction::Activate(Cancel)) => self.review = self.original_review.clone(),
            _ => {}
        }
        self.save_error = None;
        self.prepare();
        (action, outcome)
    }
}

pub(crate) fn render_review_settings(
    frame: &mut Frame,
    popup: Rect,
    dialog: &ReviewSettingsDialog,
    setup_saving: bool,
) {
    use ReviewSettingsFocus::*;
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings › Code Review",
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
    let selectors = dialog.selectors();
    let notes = if dialog.pending_autoconfigure.is_some() {
        "Checking reviewer efforts…"
    } else if let Some(notice) = &dialog.effort_notice {
        notice.as_str()
    } else if !dialog.models_known {
        "Loading review models…"
    } else if dialog.suggested().is_none() {
        "Autoconfigure unavailable: no supported models"
    } else if dialog.efforts_known.iter().any(|known| !known) {
        "Loading effort choices…"
    } else {
        "Review runs before push when reviewers are set."
    };
    // A roomy dialog keeps each reviewer's three controls together. On a
    // narrow terminal, preserve the same focus order in three adjacent rows.
    let inline = body.width >= 64;
    let reviewer_rows = if inline { 2 } else { 4 };
    let actions_row = reviewer_rows * dialog.review.reviewers.len() as u16;
    let content_height = actions_row + 3;
    let focus_row = match form.focused() {
        Some(Model(i)) => reviewer_rows * i as u16,
        Some(Effort(i)) => reviewer_rows * i as u16 + u16::from(!inline),
        Some(Remove(i)) => reviewer_rows * i as u16 + if inline { 0 } else { 2 },
        Some(Add | Autoconfigure) => actions_row,
        _ => 0,
    };
    let viewport = form.viewport(body, content_height, Some(focus_row..focus_row + 1));
    let row = |index| viewport.row(index, 1);
    let mut expanded = None;
    for (id, label) in &selectors {
        let index = match id {
            Model(i) => reviewer_rows * *i as u16,
            Effort(i) => reviewer_rows * *i as u16 + u16::from(!inline),
            _ => unreachable!("selectors contain only combos"),
        };
        let area = row(index);
        let field = if inline {
            match id {
                Model(_) | Effort(_) => {
                    let label_width = 12;
                    let remaining = area.width.saturating_sub(label_width);
                    let remove_width = 10;
                    let effort_width = 18;
                    let model_width = remaining.saturating_sub(remove_width + effort_width + 2);
                    if matches!(id, Model(_)) {
                        let reviewer = label.trim_end_matches(" model");
                        frame.render_widget(
                            Line::raw(reviewer.to_owned()),
                            Rect::new(area.x, area.y, label_width, 1),
                        );
                        Rect::new(area.x + label_width, area.y, model_width, 1)
                    } else {
                        Rect::new(
                            area.x + label_width + model_width + 1,
                            area.y,
                            effort_width,
                            1,
                        )
                    }
                }
                _ => unreachable!(),
            }
        } else {
            let label_width = 20.min(area.width);
            frame.render_widget(
                Line::raw(label.clone()),
                Rect::new(area.x, area.y, label_width, 1),
            );
            Rect::new(area.x + label_width, area.y, area.width - label_width, 1)
        };
        let (values, selected) = dialog.choices(*id);
        let selected = dialog.combo.selection(*id, selected);
        let value = values.get(selected).cloned().unwrap_or_default();
        let options = values.into_iter().map(Line::raw).collect::<Vec<_>>();
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
            expanded = Some((*id, field, value, options, selected));
        }
        if let Effort(i) = id {
            let remove_area = if inline {
                Rect::new(
                    area.right().saturating_sub(10),
                    area.y,
                    10.min(area.width),
                    1,
                )
            } else {
                row(index + 1)
            };
            let label = if inline {
                "Remove".to_owned()
            } else {
                format!("Remove reviewer {}", i + 1)
            };
            Dialog::render_actions(
                frame,
                remove_area,
                &[(Remove(*i), label.as_str(), !dialog.saving)],
                &mut form,
            );
            if dialog.effort_unavailable(*i) || !dialog.effort_failures[*i].is_empty() {
                let reason = if dialog.effort_failures[*i].is_empty() {
                    "Effort unavailable for this model"
                } else {
                    dialog.effort_failures[*i].as_str()
                };
                frame.render_widget(
                    Line::styled(reason, Style::default().fg(theme::palette().warning)),
                    row(reviewer_rows * *i as u16 + reviewer_rows - 1),
                );
            }
        }
    }
    let area = row(actions_row);
    Dialog::render_actions(
        frame,
        area,
        &[
            (
                Add,
                "Add reviewer",
                !dialog.saving
                    && dialog.review.reviewers.len() < MAX_REVIEWERS
                    && !dialog.models.is_empty(),
            ),
            (
                Autoconfigure,
                "Autoconfigure",
                !dialog.saving && dialog.suggested().is_some(),
            ),
        ],
        &mut form,
    );
    frame.render_widget(
        Line::styled(notes, Style::default().fg(theme::palette().muted)),
        row(actions_row + 1),
    );
    if let Some(error) = &dialog.save_error {
        frame.render_widget(
            Line::styled(error.clone(), Style::default().fg(theme::palette().warning)),
            row(actions_row + 2),
        );
    }
    let footer = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        u16::from(inner.height > 0),
    );
    Dialog::render_actions(
        frame,
        footer,
        &[
            (Back, "Back", true),
            (Cancel, "Cancel", true),
            (
                Save,
                if dialog.saving {
                    "Saving…"
                } else {
                    "Save Settings"
                },
                dialog.can_save(),
            ),
        ],
        &mut form,
    );
    if let Some((id, field, value, options, selected)) = expanded {
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
    form.end_frame(dialog.initial_focus());
}

/// A model's offered efforts, or `None` while a profile that may offer the
/// model has not reported them. Profiles that do not offer the model are
/// irrelevant to this.
fn known_efforts(
    snapshot: &ProfileCapabilitiesSnapshot,
    config: &Config,
    model: &str,
) -> Option<Vec<SessionConfigChoice>> {
    let options = snapshot.options(config, Some(model))?;
    snapshot
        .effort_blockers(config, model)
        .is_empty()
        .then_some(options.efforts)
}

#[cfg(test)]
mod tests;
