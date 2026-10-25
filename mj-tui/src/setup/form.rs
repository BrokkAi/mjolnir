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
        if let Some(editor) = &self.editor
            && editor.choices.is_empty()
        {
            let mut actions = vec![(Back, "Back", true)];
            if !editor.adding {
                // A name that does not exist yet has no default to restore.
                actions.push((Clear, "Use default", true));
            }
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
            actions.push((Remove, "Remove", interactive && !self.keys().is_empty()));
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
        // Signing in applies to the selected row, so the button belongs to the
        // profiles list rather than to every page. A profile added in this
        // draft is not on disk yet, a disabled profile cannot be selected for
        // work, and an API-key profile has no interactive login.
        if self.path == ["profiles"] {
            let selected = self.keys().get(self.selected).cloned();
            let saved = selected
                .as_deref()
                .is_some_and(|key| self.saved_profiles.contains(key));
            let unchanged = selected
                .as_deref()
                .is_some_and(|key| self.saved_identity_matches(key));
            let enabled = selected.as_deref().is_some_and(|key| {
                self.draft["profiles"]
                    .get(key)
                    .and_then(|profile| profile.get("enabled"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
            });
            let needs_login = selected.as_deref().is_some_and(|key| {
                !matches!(
                    self.login_statuses.get(key),
                    Some(mj_core::credentials::LoginStatus::NotRequired(_))
                )
            });
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
        self.expire_notice();
        use SetupControl::*;
        if let Some(search) = &self.search {
            let len = search.matches.len();
            let selected = search.selected;
            let query = search.input.value().to_owned();
            let actions = self.actions();
            let form = self.form.get_mut();
            form.begin_frame();
            form.declare(Search, ControlKind::TextField);
            form.declare_with_enabled(Results, ControlKind::ChoiceList { len, selected }, len > 0);
            // A new query is a new list, so the viewport starts at its top
            // rather than wherever the previous results were scrolled to.
            form.set_list_identity(Results, format!("search/{query}"));
            form.set_menu(true);
            for (id, _, enabled) in actions {
                form.declare_with_enabled(id, ControlKind::Button, enabled);
            }
            form.end_frame(Search);
            return;
        }
        let len = self.keys().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let actions = self.actions();
        let identity = format!("{:?}/{:?}", self.path, self.keys());
        let form = self.form.get_mut();
        form.begin_frame();
        let initial = if let Some(editor) = &self.editor {
            if editor.choices.is_empty() {
                let kind = match &editor.input {
                    EditorInput::Path(input) => input.control_kind(),
                    EditorInput::Text(_) => ControlKind::TextField,
                };
                form.declare(Field, kind);
                Field
            } else {
                form.declare(
                    Choices,
                    ControlKind::ComboBox {
                        len: editor.choices.len(),
                        selected: editor.combo.selection(Choices, editor.selected),
                        expanded: editor.combo.is_open(Choices),
                    },
                );
                Choices
            }
        } else {
            let kind = ControlKind::ChoiceList {
                len,
                selected: self.selected,
            };
            form.declare(List, kind);
            form.set_list_identity(List, identity);
            List
        };
        for (id, _, enabled) in actions {
            form.declare_with_enabled(id, ControlKind::Button, enabled);
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

    /// Only the setup form's own field counts. The review-settings editor on
    /// top of it has text fields too, but routing has never consulted them and
    /// this milestone does not change that.
    fn text_input_focused(&self) -> bool {
        self.form.borrow().is_focused(SetupControl::Field)
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
        } else if let Some(editor) = &self.editor {
            self.form
                .get_mut()
                .track_draft(vec![editor.input.to_string()]);
            self.form
                .get_mut()
                .set_dismiss_actions(&[SetupControl::Back]);
            // Leaving discards only this field's edit, so the prompt says so.
            let field = if editor.adding {
                "the new entry's name".to_owned()
            } else {
                format!(
                    "“{}”",
                    breadcrumb(&editor.path, &self.draft)
                        .rsplit(" › ")
                        .next()
                        .unwrap_or_default()
                )
            };
            self.form.get_mut().set_confirmation_message(Some(format!(
                "Your edit to {field} will be lost. The rest of the Settings draft is kept."
            )));
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
