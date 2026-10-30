use super::*;

impl SetupDialog {
    /// The one action set for the current screen. Buttons appear only on the
    /// pages where they apply; the renderer, the inert mirror behind a choice
    /// popup, and `prepare` all read this list so they cannot drift apart.
    pub(super) fn actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        use SetupControl::*;
        // The search covers the page, so the page's own actions go with it and
        // Back means "leave the search" rather than "leave this page".
        if self.search.is_some() {
            return vec![
                (Back, "Back", true),
                (Save, self.save_label(), !self.saving),
            ];
        }
        // The Detection results view reads its blocks, not the page; Back
        // here means "leave the report".
        if self.report.is_some() {
            return vec![
                (Back, "Back", true),
                (Save, self.save_label(), !self.saving),
            ];
        }
        if self.managed_setup.is_some() {
            return vec![
                (Back, "Back", true),
                (ManagedContinue, "Set up and sign in", !self.saving),
            ];
        }
        if self.editor.is_some() {
            let mut actions = vec![(Back, "Back", true)];
            actions.push((Apply, "Apply", true));
            return actions;
        }
        // The list page stays visible behind a choice popup, but its
        // item and machine actions must not be reachable through it.
        let interactive = self.editor.is_none();
        let collection = self.collection();
        let mut actions = Vec::new();
        if !self.path.is_empty() {
            actions.push((Back, "Back", true));
        }
        if collection {
            // The projects page creates a bundle rather than adding a bare
            // entry, so its button says what it makes.
            let add_label = if self.path == ["bundles"] {
                "Create"
            } else {
                "Add"
            };
            actions.push((Add, add_label, interactive));
        }
        let mut parent = self.path.clone();
        if let Some(key) = parent.pop()
            && self
                .draft
                .pointer(&pointer(&parent))
                .is_some_and(|value| is_collection(&parent, value))
        {
            let local = parent == ["machines"] && key == mj_core::config::LOCAL_MACHINE_ID;
            actions.push((Remove, "Remove", interactive && !local));
        }
        // Each detection writes only the section it belongs to, so each button
        // appears on that section's page alone.
        let detect = match self.path.first().map(String::as_str) {
            Some("profiles") if self.path.len() == 1 => Some((DetectProfiles, "Detect profiles")),
            Some("targets") if self.path.len() == 1 => Some((DetectRuntimes, "Detect runtimes")),
            _ => None,
        };
        if let Some((control, label)) = detect {
            actions.push((
                control,
                label,
                interactive && !self.discovering && !self.saving,
            ));
        }
        // The report of what a runtimes detection turned down stays readable
        // from its page until Settings closes.
        if self.path == ["targets"] && !self.detection_rejected.is_empty() {
            actions.push((
                DetectionResults,
                "Detection results",
                interactive && !self.saving,
            ));
        }
        if self.path == ["profiles"] {
            actions.push((
                ManagedSetup,
                "Set up managed",
                interactive && !self.discovering && !self.saving,
            ));
        }
        // Login operates on a saved profile identified by this detail page.
        if let [section, profile_id] = self.path.as_slice()
            && section == "profiles"
        {
            let saved = self.saved_profiles.contains(profile_id);
            let unchanged = self.saved_identity_matches(profile_id)
                && !self.fields.iter().any(|(path, field)| {
                    path.starts_with(&self.path) && field.input.value() != field.applied
                });
            let enabled = self.draft["profiles"][profile_id]["enabled"]
                .as_bool()
                .unwrap_or(true);
            let needs_login = !matches!(
                self.login_statuses.get(profile_id),
                Some(mj_core::credentials::LoginStatus::NotRequired(_))
            );
            actions.push((
                SignIn,
                "Sign in",
                interactive
                    && saved
                    && unchanged
                    && enabled
                    && needs_login
                    && !self.discovering
                    && !self.saving,
            ));
        }
        if let Some((label, enabled)) = self.install_mbx_page_action() {
            actions.push((InstallMbx, label, interactive && enabled && !self.saving));
        }
        actions.push((Save, self.save_label(), !self.saving));
        actions
    }

    pub(super) fn save_label(&self) -> &'static str {
        if self.saving {
            "Saving…"
        } else {
            "Save and Close"
        }
    }

    /// The actions drawn in the dialog's footer row: the way back out of a
    /// page and the commit that closes the dialog.
    pub(super) fn footer_actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        self.actions()
            .into_iter()
            .filter(|(id, _, _)| matches!(id, SetupControl::Back | SetupControl::Save))
            .collect()
    }

    /// The actions that belong to the page itself, stacked in the column at
    /// the dialog's right edge.
    pub(super) fn page_actions(&self) -> Vec<(SetupControl, &'static str, bool)> {
        self.actions()
            .into_iter()
            .filter(|(id, _, _)| !matches!(id, SetupControl::Back | SetupControl::Save))
            .collect()
    }

    pub(super) fn notice_view(&self) -> (Vec<String>, bool, bool) {
        (
            self.path.clone(),
            self.editor.is_some(),
            self.search.is_some(),
        )
    }

    /// Anchors a new notice to the view showing it, and drops a notice once
    /// the dialog has left that view.
    pub(super) fn expire_notice(&mut self) {
        let Some(text) = self.notice.clone() else {
            self.notice_anchor = None;
            return;
        };
        let view = self.notice_view();
        match &self.notice_anchor {
            Some(anchor) if anchor.text == text => {
                if anchor.view != view {
                    self.notice = None;
                    self.notice_anchor = None;
                }
            }
            _ => self.notice_anchor = Some(NoticeAnchor { text, view }),
        }
    }

    pub(crate) fn prepare(&mut self) {
        if let Some(review) = &self.review_editor {
            review.prepare();
            return;
        }
        self.reconcile_host_owned_fields();
        self.expire_notice();
        use SetupControl::*;
        if let Some(search) = &self.search {
            let len = search.matches.len();
            let selected = search.selected;
            let query = search.input.value().to_owned();
            let actions = self.actions();
            let form = self.form.get_mut();
            form.begin_update();
            form.declare(Search, ControlKind::TextField);
            form.declare_with_enabled(Results, ControlKind::ChoiceList { len, selected }, len > 0);
            // A new query is a new list, so the viewport starts at its top
            // rather than wherever the previous results were scrolled to.
            form.set_list_identity(Results, format!("search/{query}"));
            form.set_list_activation(Results, mj_chat::components::ListActivation::SingleClick);
            form.set_menu(true);
            for (id, _, enabled) in actions {
                form.declare_with_enabled(id, ControlKind::Button, enabled);
            }
            form.end_frame(Search);
            return;
        }
        if self.report.is_some() {
            // Only the footer actions belong to the report; the page's
            // controls stay undeclared so focus cannot land on them.
            let actions = self.actions();
            let form = self.form.get_mut();
            form.begin_update();
            for (id, _, enabled) in actions {
                form.declare_with_enabled(id, ControlKind::Button, enabled);
            }
            form.end_frame(Back);
            return;
        }
        self.reconcile_controls();
        let actions = self.actions();
        let form = self.form.get_mut();
        form.begin_update();
        let initial = if self.editor.is_some() {
            form.declare(Field, ControlKind::TextField);
            Field
        } else {
            for control in &self.descriptors {
                let kind = match &control.kind {
                    SettingKind::Page | SettingKind::ManagedHarness(_) => ControlKind::Button,
                    SettingKind::Checkbox => ControlKind::Checkbox,
                    SettingKind::Choices(values) => ControlKind::ComboBox {
                        len: values.len(),
                        selected: self.combo.selection(
                            control.id,
                            values
                                .iter()
                                .position(|v| {
                                    self.draft.pointer(&pointer(&control.path)) == Some(v)
                                })
                                .unwrap_or(0),
                        ),
                        expanded: self.combo.is_open(control.id),
                    },
                    SettingKind::Field => match &self.fields[&control.path].input {
                        EditorInput::Text(_) => ControlKind::TextField,
                        EditorInput::Path(input) => input.control_kind(),
                    },
                    SettingKind::ReadOnly => continue,
                };
                form.declare_with_enabled(
                    control.id,
                    kind,
                    self.combo.open_id().is_none_or(|id| id == control.id),
                );
            }
            self.descriptors
                .iter()
                .find(|c| c.kind != SettingKind::ReadOnly)
                .map_or(Save, |c| c.id)
        };
        for (id, _, enabled) in actions {
            form.declare_with_enabled(
                id,
                ControlKind::Button,
                enabled && self.combo.open_id().is_none(),
            );
        }
        form.end_frame(initial);
    }
}
/// Setup answers the dashboard's modal questions through whichever of its two
/// forms is on top: the review-settings editor when it is open, the setup form
/// otherwise.
impl ModalSurface for SetupDialog {
    fn confirmation_open(&self) -> bool {
        self.review_editor.as_ref().map_or_else(
            || self.form.borrow().confirmation_open(),
            |review| review.form.borrow().confirmation_open(),
        )
    }

    fn render_confirmation(&self, frame: &mut Frame<'_>, area: Rect, surfaces: &mut FrameSurfaces) {
        if let Some(review) = &self.review_editor {
            review
                .form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces);
        } else {
            self.form
                .borrow_mut()
                .render_confirmation(frame, area, surfaces);
        }
    }

    fn handles_mouse(&self, column: u16, row: u16) -> bool {
        if let Some(dialog) = &self.review_editor {
            let form = dialog.form.borrow();
            return form.captures_pointer() || form.contains(column, row);
        }
        let form = self.form.borrow();
        form.captures_pointer() || form.contains(column, row)
    }

    fn cancel_pointer(&mut self) -> bool {
        if let Some(review) = &mut self.review_editor {
            let form = review.form.get_mut();
            let changed = form.captures_pointer();
            form.cancel_pointer();
            changed
        } else {
            let form = self.form.get_mut();
            let changed = form.captures_pointer();
            form.cancel_pointer();
            changed
        }
    }

    fn reset_geometry(&mut self) {
        if let Some(review) = &mut self.review_editor {
            review.form.get_mut().reset_geometry();
        } else {
            self.form.get_mut().reset_geometry();
        }
    }

    /// Field ownership comes from this page's descriptor, not retained draft buffers.
    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(SetupControl::Field)
            || self.focused_field_path().is_some()
            || (self.search.is_some() && self.form.borrow().is_focused(SetupControl::Search))
    }

    fn prepare_dialog_state(&mut self) {
        self.form.get_mut().set_action_role(
            SetupControl::Cancel,
            mj_chat::components::ActionRole::Cancel,
        );
        self.form
            .get_mut()
            .set_action_role(SetupControl::Back, mj_chat::components::ActionRole::Back);
        if let Some(review) = &mut self.review_editor {
            review.prepare_dialog_state();
        } else if self.managed_setup.is_some() {
            self.form.get_mut().set_dirty(false);
            self.form.get_mut().set_dismiss_actions(&[]);
            self.form
                .get_mut()
                .set_default_action(SetupControl::ManagedContinue);
        } else if self.editor.is_some() {
            self.form.get_mut().set_dirty(false);
            self.form.get_mut().set_dismiss_actions(&[]);
            self.form.get_mut().set_default_action(SetupControl::Apply);
        } else {
            // The outer setup draft uses its normalized saved-config comparison.
            self.form.get_mut().set_dirty(false);
            self.form.get_mut().set_dismiss_actions(&[]);
            self.form.get_mut().set_default_action(SetupControl::Save);
        }
    }

    fn layer_detail(&self) -> String {
        format!(
            "{:?}/{:?}/{}",
            self.path,
            self.editor.as_ref().map(|editor| &editor.path),
            self.review_editor.is_some()
        )
    }
}

impl SetupDialog {
    fn reconcile_controls(&mut self) {
        let previous = std::mem::take(&mut self.descriptors);
        if self.managed_setup.is_some() {
            self.descriptors = MANAGED_SETUP_HARNESSES
                .iter()
                .enumerate()
                .map(|(index, kind)| {
                    let path = vec!["_managed".to_owned(), kind.id().to_owned()];
                    let id = previous
                        .iter()
                        .find(|c| c.path == path)
                        .map(|c| c.id)
                        .unwrap_or_else(|| {
                            let id = SetupControl::Row(self.next_control);
                            self.next_control += 1;
                            id
                        });
                    SettingControl {
                        id,
                        path,
                        kind: SettingKind::ManagedHarness(index),
                    }
                })
                .collect();
            return;
        }
        let open_value = self.combo.open_id().and_then(|id| {
            let control = previous.iter().find(|c| c.id == id)?;
            let SettingKind::Choices(values) = &control.kind else {
                return None;
            };
            let committed = values
                .iter()
                .position(|v| self.draft.pointer(&pointer(&control.path)) == Some(v))
                .unwrap_or(0);
            Some((
                id,
                values.get(self.combo.selection(id, committed))?.clone(),
                values.len(),
            ))
        });
        let paths = if self.current().is_object() || self.current().is_array() {
            self.paths()
        } else {
            vec![self.path.clone()]
        };
        for path in paths {
            let value = self
                .draft
                .pointer(&pointer(&path))
                .expect("visible setting exists");
            let kind = if self.build_cache_field_user_managed(&path) {
                SettingKind::ReadOnly
            } else {
                match mj_client::settings::page::row_kind(&self.draft, &self.path, &path) {
                    RowKind::Page => SettingKind::Page,
                    RowKind::Checkbox => SettingKind::Checkbox,
                    RowKind::Choices(choices) => SettingKind::Choices(choices),
                    RowKind::Field => SettingKind::Field,
                }
            };
            let id = previous
                .iter()
                .find(|c| {
                    c.path == path
                        && std::mem::discriminant(&c.kind) == std::mem::discriminant(&kind)
                })
                .map(|c| c.id)
                .unwrap_or_else(|| {
                    let id = SetupControl::Row(self.next_control);
                    self.next_control += 1;
                    id
                });
            if kind == SettingKind::Field {
                let text = field_text(&path, value);
                let field = self
                    .fields
                    .entry(path.clone())
                    .or_insert_with(|| PendingField {
                        input: field_input(&path, &text),
                        applied: text.clone(),
                        source: value.clone(),
                        error: None,
                    });
                // Background updates refresh untouched fields, never a pending edit.
                if field.input.value() == field.applied && field.source != *value {
                    field.input = field_input(&path, &text);
                    field.applied = text;
                    field.source = value.clone();
                    field.error = None;
                }
            }
            if let Some((open, value, old_len)) = &open_value
                && *open == id
                && let SettingKind::Choices(values) = &kind
            {
                let target = if *old_len == 1 {
                    self.draft.pointer(&pointer(&path)).unwrap()
                } else {
                    value
                };
                self.combo
                    .open(id, values.iter().position(|v| v == target).unwrap_or(0));
            }
            self.descriptors.push(SettingControl { id, path, kind });
        }
        if self
            .combo
            .open_id()
            .is_some_and(|id| !self.descriptors.iter().any(|c| c.id == id))
        {
            self.combo = ComboBoxState::default();
        }
        self.fields
            .retain(|path, _| self.draft.pointer(&pointer(path)).is_some());
    }

    pub(super) fn empty_field_hint(&self, path: &[String]) -> Option<String> {
        let key = path.last()?;
        let parent = &path[..path.len() - 1];
        let defaults = schema::defaults(parent, self.draft.pointer(&pointer(parent))?);
        if parent.last().is_some_and(|key| key == "environment") {
            return None;
        }
        if path == ["interface", "prefix"] {
            return Some(mj_core::config::DEFAULT_PREFIX.to_owned());
        }
        if let Some(hint) = self
            .build_cache_automatic_label(key)
            .or_else(|| self.archive_space_automatic_label(key))
        {
            return Some(hint);
        }
        if schema::whole_number(path).is_some_and(|number| number.defaulted) {
            return Some(
                defaults
                    .get(key)
                    .filter(|v| !v.is_null())
                    .map(|v| field_text(path, v))
                    .unwrap_or_else(|| "Default".to_owned()),
            );
        }
        defaults
            .get(key)
            .filter(|v| v.is_null())
            .map(|_| "Default".to_owned())
    }

    pub(super) fn choice_label(&self, path: &[String], value: &Value) -> String {
        schema::option_label(path, value, &self.draft)
    }
}

use mj_client::settings::schema::field_text;

fn field_input(path: &[String], text: &str) -> EditorInput {
    if schema::path_kind(path).is_some() {
        EditorInput::Path(PathInput::from(text))
    } else {
        EditorInput::Text(TextInput::from(text))
    }
}
