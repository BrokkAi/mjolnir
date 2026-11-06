//! The review settings editor. Capability choices come from the dashboard's
//! published catalog; the editor never performs discovery on the event loop.

use std::cell::RefCell;

use crossterm::event::Event;
use mj_chat::components::{ComboBox, ComboBoxState, ControlKind, Dialog, Interaction, PopupSide};
use mj_chat::theme;
use mj_core::acp::SessionConfigChoice;
use mj_core::config::{Config, MAX_REVIEWERS, ReviewConfig, ReviewTrigger, SubagentChoice};
use mj_core::profile_capabilities::ProfileCapabilitiesSnapshot;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::widgets::dismissible_modal_title;
use crate::{DashboardAction, DashboardState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReviewSettingsFocus {
    Trigger,
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
    models: Vec<SessionConfigChoice>,
    models_known: bool,
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
    efforts_known: Vec<bool>,
    /// Empty lists are filled only on the first terminal catalog publication.
    initial_fill_done: bool,
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
            efforts_known: Vec::new(),
            initial_fill_done: !config.review.reviewers.is_empty(),
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
    ) {
        let models = snapshot.options(config, None);
        self.models_known = models.is_some();
        if let Some(options) = models {
            self.models = options.models;
            if !self.initial_fill_done && !self.models.is_empty() {
                self.initial_fill_done = true;
                if self.review.reviewers.is_empty()
                    && let Some(reviewers) =
                        mj_core::review::autoconfigure::autoconfigure(&self.models)
                {
                    self.review.reviewers = reviewers;
                }
            }
        } else {
            self.models.clear();
        }
        self.efforts.clear();
        self.efforts_known.clear();
        for reviewer in &self.review.reviewers {
            match snapshot.options(config, Some(&reviewer.model)) {
                Some(options) => {
                    self.efforts.push(options.efforts);
                    self.efforts_known.push(true);
                }
                None => {
                    self.efforts.push(Vec::new());
                    self.efforts_known.push(false);
                }
            }
        }
        self.prepare();
    }

    fn suggested(&self) -> Option<Vec<SubagentChoice>> {
        self.models_known
            .then(|| mj_core::review::autoconfigure::autoconfigure(&self.models))
            .flatten()
    }

    pub(crate) fn can_save(&self) -> bool {
        if self.saving || self.review.reviewers.len() > MAX_REVIEWERS {
            return false;
        }
        if self.review.trigger != ReviewTrigger::Off && self.review.reviewers.is_empty() {
            return false;
        }
        self.review.reviewers.iter().all(|reviewer| {
            !reviewer.model.trim().is_empty()
                && (!self.models_known
                    || self
                        .models
                        .iter()
                        .any(|choice| choice.value == reviewer.model))
        })
    }

    pub(crate) fn validation_snapshot(&self) -> ReviewSettingsValidation {
        ReviewSettingsValidation {
            reviewers: self.review.reviewers.clone(),
            models: self.models.clone(),
            models_known: self.models_known,
        }
    }

    pub(crate) fn validation_error(
        snapshot: &ReviewSettingsValidation,
        review: &ReviewConfig,
    ) -> Option<String> {
        if review.reviewers.len() > MAX_REVIEWERS
            || (review.trigger != ReviewTrigger::Off && review.reviewers.is_empty())
        {
            return Some("Choose 1–3 reviewers for automatic review.".into());
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
        }
        None
    }

    fn choices(&self, id: ReviewSettingsFocus) -> (Vec<String>, usize) {
        use ReviewSettingsFocus::*;
        match id {
            Trigger => (
                vec!["Off".into(), "End of turn".into(), "Before push".into()],
                match self.review.trigger {
                    ReviewTrigger::Off => 0,
                    ReviewTrigger::Turn => 1,
                    ReviewTrigger::Push => 2,
                },
            ),
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
        let mut selectors = vec![(ReviewSettingsFocus::Trigger, "Trigger".into())];
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
                !self.saving,
            );
            if let Effort(i) = id {
                form.declare_with_enabled(Remove(i), ControlKind::Button, !self.saving);
            }
        }
        form.declare_with_enabled(
            Add,
            ControlKind::Button,
            !self.saving && self.review.reviewers.len() < MAX_REVIEWERS && !self.models.is_empty(),
        );
        form.declare_with_enabled(
            Autoconfigure,
            ControlKind::Button,
            !self.saving && self.suggested().is_some(),
        );
        form.declare_with_enabled(Back, ControlKind::Button, true);
        form.declare_with_enabled(Cancel, ControlKind::Button, true);
        form.declare_with_enabled(Save, ControlKind::Button, self.can_save());
        form.end_frame(Trigger);
    }

    pub(crate) fn handle_event(
        &mut self,
        dashboard: &mut DashboardState,
        event: Event,
    ) -> (DashboardAction, ReviewSettingsOutcome) {
        use ReviewSettingsFocus::*;
        let result = self.form.get_mut().handle(&event);
        dashboard.last_event_consumed.set(result.consumed);
        let interaction = self.combo.route(result.action);
        let outcome = match &interaction {
            Some(Interaction::Cancel | Interaction::Activate(Back)) => ReviewSettingsOutcome::Back,
            Some(Interaction::Activate(Cancel)) => ReviewSettingsOutcome::CancelSetup,
            Some(Interaction::Activate(Save)) if self.can_save() => ReviewSettingsOutcome::Save,
            _ => ReviewSettingsOutcome::Continue,
        };
        match interaction {
            Some(Interaction::ComboBoxCommit(Trigger, index)) => {
                self.review.trigger =
                    [ReviewTrigger::Off, ReviewTrigger::Turn, ReviewTrigger::Push][index]
            }
            Some(Interaction::ComboBoxCommit(Model(i), index)) => {
                if let Some(value) = self.choices(Model(i)).0.get(index).cloned() {
                    self.review.reviewers[i].model = value;
                    self.review.reviewers[i].effort = None;
                    self.update_choices(&dashboard.config, &dashboard.profile_capabilities);
                }
            }
            Some(Interaction::ComboBoxCommit(Effort(i), index)) => {
                if let Some(value) = self.choices(Effort(i)).0.get(index) {
                    self.review.reviewers[i].effort = (index != 0).then(|| value.clone());
                }
            }
            Some(Interaction::Activate(Add)) if self.review.reviewers.len() < MAX_REVIEWERS => {
                if let Some(model) = self.models.first() {
                    self.review.reviewers.push(SubagentChoice {
                        model: model.value.clone(),
                        effort: None,
                    });
                    self.update_choices(&dashboard.config, &dashboard.profile_capabilities);
                }
            }
            Some(Interaction::Activate(Remove(i))) if i < self.review.reviewers.len() => {
                self.review.reviewers.remove(i);
                self.update_choices(&dashboard.config, &dashboard.profile_capabilities);
            }
            Some(Interaction::Activate(Autoconfigure)) => {
                if let Some(reviewers) = self.suggested() {
                    self.review.reviewers = reviewers;
                    self.update_choices(&dashboard.config, &dashboard.profile_capabilities);
                }
            }
            Some(Interaction::Activate(id @ (Trigger | Model(_) | Effort(_)))) => {
                self.combo.open(id, self.choices(id).1)
            }
            Some(Interaction::Activate(Cancel)) => self.review = self.original_review.clone(),
            _ => {}
        }
        self.save_error = None;
        self.prepare();
        (DashboardAction::None, outcome)
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
    let notes = if !dialog.models_known {
        "Loading review models…"
    } else if dialog.suggested().is_none() {
        "Autoconfigure unavailable: no supported models"
    } else if dialog.efforts_known.iter().any(|known| !known) {
        "Loading effort choices…"
    } else {
        "Choose each reviewer’s model and effort."
    };
    // A roomy dialog keeps each reviewer's three controls together. On a
    // narrow terminal, preserve the same focus order in three adjacent rows.
    let inline = body.width >= 64;
    let reviewer_rows = if inline { 1 } else { 3 };
    let actions_row = 1 + reviewer_rows * dialog.review.reviewers.len() as u16;
    let content_height = actions_row + 3;
    let focus_row = match form.focused() {
        Some(Trigger) => 0,
        Some(Model(i)) => 1 + reviewer_rows * i as u16,
        Some(Effort(i)) => 1 + reviewer_rows * i as u16 + u16::from(!inline),
        Some(Remove(i)) => 1 + reviewer_rows * i as u16 + if inline { 0 } else { 2 },
        Some(Add | Autoconfigure) => actions_row,
        _ => 0,
    };
    let viewport = form.viewport(body, content_height, Some(focus_row..focus_row + 1));
    let row = |index| viewport.row(index, 1);
    let mut expanded = None;
    for (id, label) in &selectors {
        let index = match id {
            Trigger => 0,
            Model(i) => 1 + reviewer_rows * *i as u16,
            Effort(i) => 1 + reviewer_rows * *i as u16 + u16::from(!inline),
            _ => unreachable!("selectors contain only combos"),
        };
        let area = row(index);
        let field = if inline {
            match id {
                Trigger => {
                    let label_width = 12.min(area.width);
                    frame.render_widget(
                        Line::raw(label.clone()),
                        Rect::new(area.x, area.y, label_width, 1),
                    );
                    Rect::new(area.x + label_width, area.y, area.width - label_width, 1)
                }
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
    form.end_frame(Trigger);
}

#[cfg(test)]
mod tests;
