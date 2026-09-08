//! In-memory setup form. Discovery and persistence run in supervised workers.
mod schema;

use crate::{DashboardAction, DashboardState, Mode, widgets::centered_modal};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use hel::hel_config::HelConfig;
use mj_chat::components::{ButtonRow, ChoiceList, ControlKind, Form, Interaction, TextField};
use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;
use mj_chat::theme;
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::cell::RefCell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupControl {
    List,
    Field,
    Choices,
    Back,
    Add,
    Remove,
    Clear,
    Apply,
    Detect,
    Save,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Editor {
    path: Vec<String>,
    input: TextInput,
    choices: Vec<Value>,
    selected: usize,
    adding: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupDialog {
    generation: u64,
    original: String,
    draft: Value,
    path: Vec<String>,
    selected: usize,
    editor: Option<Editor>,
    pub(crate) form: RefCell<Form<SetupControl>>,
    pub(crate) saving: bool,
    discovering: bool,
    notice: Option<String>,
    read_only: Option<String>,
}

fn pointer(path: &[String]) -> String {
    path.iter()
        .map(|key| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
        .collect()
}

impl SetupDialog {
    fn new(config: &HelConfig) -> Self {
        let mut draft = serde_json::to_value(config).expect("configuration serializes");
        let original = draft.to_string();
        schema::expand(&mut draft, &mut Vec::new());
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut dialog = Self {
            generation: NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            original,
            draft,
            path: Vec::new(),
            selected: 0,
            editor: None,
            form: RefCell::new(Form::default()),
            saving: false,
            discovering: false,
            notice: None,
            read_only: config.newer_build_notice(),
        };
        dialog.prepare();
        dialog
    }

    fn current(&self) -> &Value {
        self.draft
            .pointer(&pointer(&self.path))
            .expect("setup path exists")
    }

    fn keys(&self) -> Vec<String> {
        match self.current() {
            Value::Object(entries) => entries
                .keys()
                .filter(|key| {
                    key.as_str() != "version"
                        && !(self.path.len() == 1
                            && self.path[0] == "startup"
                            && key.as_str() == "enabled")
                })
                .cloned()
                .collect(),
            Value::Array(entries) => (0..entries.len()).map(|i| i.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    fn selected_path(&self) -> Option<Vec<String>> {
        let mut path = self.path.clone();
        path.push(self.keys().get(self.selected)?.clone());
        Some(path)
    }

    fn collection(&self) -> bool {
        self.current().is_array()
            || (self.path.len() == 1
                && matches!(self.path[0].as_str(), "profiles" | "targets" | "bundles"))
            || self.path.last().is_some_and(|key| key == "environment")
    }

    fn prepare(&mut self) {
        use SetupControl::*;
        let len = self.keys().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        let collection = self.collection();
        let form = self.form.get_mut();
        form.begin_frame();
        let initial = if let Some(editor) = &self.editor {
            if editor.choices.is_empty() {
                form.declare(Field, ControlKind::TextField);
                Field
            } else {
                form.declare(
                    Choices,
                    ControlKind::ChoiceList {
                        len: editor.choices.len(),
                        selected: editor.selected,
                    },
                );
                Choices
            }
        } else {
            form.declare(
                List,
                ControlKind::ChoiceList {
                    len,
                    selected: self.selected,
                },
            );
            List
        };
        form.declare(Back, ControlKind::Button);
        if self.editor.is_some() {
            form.declare(Clear, ControlKind::Button);
            form.declare(Apply, ControlKind::Button);
        } else {
            form.declare_with_enabled(Add, ControlKind::Button, collection);
            form.declare_with_enabled(Remove, ControlKind::Button, collection && len > 0);
            form.declare_with_enabled(
                Detect,
                ControlKind::Button,
                !self.discovering && !self.saving,
            );
            form.declare_with_enabled(
                Save,
                ControlKind::Button,
                !self.saving && self.read_only.is_none(),
            );
            form.declare(Cancel, ControlKind::Button);
        }
        form.end_frame(initial);
    }

    fn open_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        let value = self.draft.pointer(&pointer(&path)).unwrap();
        if value.is_object() || value.is_array() {
            self.path = path;
            self.selected = 0;
        } else if let Some(value) = value.as_bool() {
            *self.draft.pointer_mut(&pointer(&path)).unwrap() = Value::Bool(!value);
        } else {
            let choices = schema::choices(&path, &self.draft);
            let selected = choices
                .iter()
                .position(|choice| choice == value)
                .unwrap_or(0);
            self.editor = Some(Editor {
                path,
                input: TextInput::from(if value.is_null() {
                    String::new()
                } else {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                }),
                choices,
                selected,
                adding: false,
            });
        }
        self.form = RefCell::new(Form::default());
    }

    fn back(&mut self) -> bool {
        if self.editor.take().is_none() && self.path.pop().is_none() {
            return true;
        }
        self.selected = 0;
        self.form = RefCell::new(Form::default());
        false
    }

    fn add(&mut self) {
        if !self.collection() {
            return;
        }
        if self.current().is_array() {
            let value = if self.path.last().is_some_and(|key| key == "repositories") {
                schema::repository_default()
            } else {
                Value::String(String::new())
            };
            let array = self
                .draft
                .pointer_mut(&pointer(&self.path))
                .unwrap()
                .as_array_mut()
                .unwrap();
            array.push(value);
            self.selected = array.len() - 1;
            self.open_selected();
        } else {
            self.editor = Some(Editor {
                path: self.path.clone(),
                input: TextInput::new(),
                choices: Vec::new(),
                selected: 0,
                adding: true,
            });
            self.form = RefCell::new(Form::default());
        }
    }

    fn apply_editor(&mut self, clear: bool) -> Result<(), String> {
        let Some(editor) = self.editor.clone() else {
            return Ok(());
        };
        if editor.adding {
            let name = editor.input.trim();
            if name.is_empty() {
                return Err("Choose a name for the new entry.".into());
            }
            let object = self
                .draft
                .pointer_mut(&pointer(&editor.path))
                .unwrap()
                .as_object_mut()
                .unwrap();
            if object.contains_key(name) {
                return Err(format!("An entry named {name:?} already exists."));
            }
            let mut new_path = editor.path.clone();
            new_path.push(name.to_owned());
            let value = if editor.path.last().is_some_and(|key| key == "environment") {
                Value::String(String::new())
            } else {
                schema::defaults(&new_path, &json!({}))
            };
            object.insert(name.to_owned(), value);
            self.editor = None;
            self.selected = self.keys().iter().position(|key| key == name).unwrap();
            self.open_selected();
            return Ok(());
        }
        let old = self.draft.pointer(&pointer(&editor.path)).unwrap();
        let value = if clear {
            Value::Null
        } else if !editor.choices.is_empty() {
            editor.choices[editor.selected].clone()
        } else if editor
            .path
            .last()
            .is_some_and(|key| key == "context_window_bytes")
        {
            if editor.input.trim().is_empty() {
                Value::Null
            } else {
                Value::from(
                    editor
                        .input
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| "Enter a whole number of bytes.".to_owned())?,
                )
            }
        } else if editor.input.trim().is_empty() && old.is_null() {
            Value::Null
        } else {
            Value::String(editor.input.to_string())
        };
        // An empty optional field means the account/runtime default.
        let mut parent_path = editor.path.clone();
        let key = parent_path.pop().unwrap();
        let defaults = schema::defaults(
            &parent_path,
            self.draft.pointer(&pointer(&parent_path)).unwrap(),
        );
        let value = if value == "" && defaults.get(&key).is_some_and(Value::is_null) {
            Value::Null
        } else {
            value
        };
        if clear && !defaults.get(&key).is_some_and(Value::is_null) {
            return Err("This setting is required. Choose a value instead of clearing it.".into());
        }
        if key == "kind" {
            let old_parent = self.draft.pointer(&pointer(&parent_path)).unwrap().clone();
            let mut replacement = schema::defaults(&parent_path, &json!({"kind":value}));
            for (key, target) in replacement.as_object_mut().unwrap() {
                if key != "kind"
                    && let Some(old) = old_parent.get(key)
                {
                    *target = old.clone();
                }
            }
            replacement["kind"] = value;
            *self.draft.pointer_mut(&pointer(&parent_path)).unwrap() = replacement;
        } else {
            *self.draft.pointer_mut(&pointer(&editor.path)).unwrap() = value;
        }
        self.editor = None;
        self.form = RefCell::new(Form::default());
        Ok(())
    }

    fn save(&mut self) -> DashboardAction {
        if self.saving || self.read_only.is_some() {
            return DashboardAction::None;
        }
        let result = serde_json::from_value::<HelConfig>(self.draft.clone())
            .map_err(|error| error.to_string())
            .and_then(|config| {
                config
                    .validate()
                    .map(|()| config)
                    .map_err(|error| format!("{error:#}"))
            });
        match result {
            Ok(config) => {
                self.saving = true;
                self.notice = Some("Saving setup…".into());
                DashboardAction::SaveSetup {
                    generation: self.generation,
                    original: self.original.clone(),
                    updated: serde_json::to_string(&config).expect("config serializes"),
                }
            }
            Err(error) => {
                self.notice = Some(error);
                DashboardAction::None
            }
        }
    }
}

impl DashboardState {
    pub fn begin_setup(&mut self) {
        self.mode = Mode::Setup(SetupDialog::new(&self.config));
    }

    pub(crate) fn handle_setup_event(
        &mut self,
        event: Event,
        mut dialog: SetupDialog,
    ) -> DashboardAction {
        use SetupControl::*;
        if dialog.saving {
            self.mode = Mode::Setup(dialog);
            return DashboardAction::None;
        }
        let shortcut = match &event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(Interaction::Activate(Save))
                }
                KeyCode::Backspace if dialog.editor.is_none() => Some(Interaction::Activate(Back)),
                KeyCode::Char('a') if dialog.editor.is_none() => Some(Interaction::Activate(Add)),
                KeyCode::Delete if dialog.editor.is_none() => Some(Interaction::Activate(Remove)),
                _ => None,
            },
            _ => None,
        };
        let interaction = shortcut.or_else(|| dialog.form.get_mut().handle(&event).action);
        let mut action = DashboardAction::None;
        match interaction {
            Some(Interaction::Cancel | Interaction::Activate(Back)) => {
                if dialog.back() {
                    self.cancel_modal();
                    return DashboardAction::None;
                }
            }
            Some(Interaction::Activate(Cancel)) => {
                self.cancel_modal();
                return DashboardAction::None;
            }
            Some(Interaction::Select(List, index)) => dialog.selected = index,
            Some(Interaction::Activate(List)) => dialog.open_selected(),
            Some(Interaction::Edit(Field, edit)) => {
                if let Some(editor) = &mut dialog.editor {
                    TextField::apply(&mut editor.input, edit);
                }
            }
            Some(Interaction::Select(Choices, index)) => {
                if let Some(editor) = &mut dialog.editor {
                    editor.selected = index;
                }
            }
            Some(Interaction::Activate(Field | Choices | Apply)) => {
                dialog.notice = dialog.apply_editor(false).err();
            }
            Some(Interaction::Activate(Clear)) => dialog.notice = dialog.apply_editor(true).err(),
            Some(Interaction::Activate(Add)) => dialog.add(),
            Some(Interaction::Activate(Remove)) if dialog.collection() => {
                if let Some(key) = dialog.keys().get(dialog.selected).cloned() {
                    match dialog.draft.pointer_mut(&pointer(&dialog.path)).unwrap() {
                        Value::Object(object) => {
                            object.remove(&key);
                        }
                        Value::Array(array) => {
                            array.remove(dialog.selected);
                        }
                        _ => {}
                    }
                }
            }
            Some(Interaction::Activate(Save)) if dialog.editor.is_none() => action = dialog.save(),
            Some(Interaction::Activate(Detect)) if !dialog.discovering => {
                dialog.discovering = true;
                dialog.notice = Some("Detecting agent accounts and usable local runtimes…".into());
                action = DashboardAction::DiscoverSetup {
                    generation: dialog.generation,
                };
            }
            _ => {}
        }
        dialog.prepare();
        self.mode = Mode::Setup(dialog);
        action
    }

    pub fn setup_saved(&mut self, generation: u64, result: Result<HelConfig, String>) {
        let current =
            setup_dialog_mut(&mut self.mode).is_some_and(|dialog| dialog.generation == generation);
        if !current {
            match result {
                Ok(config) => self.set_config(config),
                Err(error) => self.set_failure_notice(format!("Could not save Setup: {error}")),
            }
            return;
        }
        match result {
            Ok(config) => {
                self.set_config(config);
                self.cancel_modal();
                self.set_notice("Setup saved. New sessions use these defaults. Web listener changes apply on its next start.");
            }
            Err(error) => {
                if let Some(dialog) = setup_dialog_mut(&mut self.mode) {
                    dialog.saving = false;
                    dialog.notice = Some(format!("Could not save: {error}"));
                    dialog.prepare();
                } else {
                    self.set_failure_notice(error);
                }
            }
        }
    }

    pub fn setup_discovered(&mut self, generation: u64, result: Result<HelConfig, String>) {
        let Some(dialog) = setup_dialog_mut(&mut self.mode) else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.discovering = false;
        match result {
            Ok(config) => {
                let discovered = serde_json::to_value(config).expect("config serializes");
                for section in ["profiles", "targets", "bundles"] {
                    if let Some(entries) = discovered[section].as_object() {
                        for (key, value) in entries {
                            dialog.draft[section]
                                .as_object_mut()
                                .unwrap()
                                .entry(key.clone())
                                .or_insert_with(|| value.clone());
                        }
                    }
                }
                schema::expand(&mut dialog.draft, &mut Vec::new());
                dialog.notice =
                    Some("Detected entries added to the draft. Review them, then Save.".into());
            }
            Err(error) => dialog.notice = Some(format!("Detection failed: {error}")),
        }
        dialog.prepare();
    }
}

fn setup_dialog_mut(mode: &mut Mode) -> Option<&mut SetupDialog> {
    match mode {
        Mode::Setup(dialog) => Some(dialog),
        Mode::Help(overlay) => setup_dialog_mut(&mut overlay.return_to),
        _ => None,
    }
}

pub(crate) fn render_setup(
    frame: &mut Frame,
    area: Rect,
    dialog: &SetupDialog,
    surfaces: &mut FrameSurfaces,
) {
    use SetupControl::*;
    let popup = centered_modal(
        frame,
        surfaces,
        100,
        area.height.saturating_sub(2).min(32),
        area,
    );
    frame.render_widget(theme::modal().title(" Setup "), popup);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.height < 7 {
        dialog.form.borrow_mut().reset_geometry();
        return;
    }
    let path = dialog
        .editor
        .as_ref()
        .map(|editor| &editor.path)
        .unwrap_or(&dialog.path);
    let breadcrumb = std::iter::once("Setup".to_owned())
        .chain(path.iter().map(|key| schema::label(key)))
        .collect::<Vec<_>>()
        .join(" › ");
    frame.render_widget(
        Paragraph::new(breadcrumb).style(theme::title(true)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(schema::help(path))
            .wrap(Wrap { trim: false })
            .style(theme::muted()),
        Rect::new(inner.x, inner.y + 1, inner.width, 2),
    );
    let body = Rect::new(
        inner.x,
        inner.y + 4,
        inner.width,
        inner.height.saturating_sub(9).max(1),
    );
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let initial;
    if let Some(editor) = &dialog.editor {
        if editor.choices.is_empty() {
            let label = if editor.adding {
                "Name for the new entry".to_owned()
            } else {
                schema::label(editor.path.last().unwrap())
            };
            frame.render_widget(
                Paragraph::new(label),
                Rect::new(body.x, body.y, body.width, 1),
            );
            TextField::render(
                frame,
                Rect::new(body.x, body.y + 1, body.width, 1),
                &editor.input,
                &mut form,
                Field,
            );
            initial = Field;
        } else {
            let rows = editor
                .choices
                .iter()
                .map(|value| Line::raw(schema::choice_label(value)))
                .collect::<Vec<_>>();
            ChoiceList::render(frame, body, &rows, editor.selected, &mut form, Choices);
            initial = Choices;
        }
        ButtonRow::render(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            &[
                (Back, "Back", true),
                (Clear, "Use default", true),
                (Apply, "Apply", true),
            ],
            &mut form,
        );
    } else {
        let rows = dialog
            .keys()
            .iter()
            .map(|key| {
                let value = if let Some(object) = dialog.current().as_object() {
                    &object[key]
                } else {
                    &dialog.current()[key.parse::<usize>().unwrap()]
                };
                let summary = match value {
                    Value::Object(v) => format!("{} settings  ›", v.len()),
                    Value::Array(v) => format!("{} entries  ›", v.len()),
                    Value::Bool(v) => {
                        if *v {
                            "On".into()
                        } else {
                            "Off".into()
                        }
                    }
                    Value::Null => "Automatic / default".into(),
                    _ => schema::choice_label(value),
                };
                let name = if dialog.current().is_array() {
                    value
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("Item {}", key.parse::<usize>().unwrap() + 1))
                } else {
                    schema::label(key)
                };
                Line::raw(format!("{name:<32}  {summary}"))
            })
            .collect::<Vec<_>>();
        ChoiceList::render(frame, body, &rows, dialog.selected, &mut form, List);
        initial = List;
        ButtonRow::render(
            frame,
            Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
            &[
                (Back, "Back", !dialog.path.is_empty()),
                (Add, "Add", dialog.collection()),
                (Remove, "Remove", dialog.collection() && !rows.is_empty()),
                (
                    Detect,
                    "Detect machine",
                    !dialog.discovering && !dialog.saving,
                ),
            ],
            &mut form,
        );
        ButtonRow::render(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            &[
                (Cancel, "Cancel", !dialog.saving),
                (
                    Save,
                    if dialog.saving {
                        "Saving…"
                    } else {
                        "Save (Ctrl-S)"
                    },
                    !dialog.saving && dialog.read_only.is_none(),
                ),
            ],
            &mut form,
        );
    }
    if let Some(notice) = dialog.read_only.as_ref().or(dialog.notice.as_ref()) {
        frame.render_widget(
            Paragraph::new(notice.as_str()).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.bottom() - 5, inner.width, 3),
        );
    }
    form.end_frame(initial);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{buffer_lines, config, dashboard_with_session, key, stopped_session};
    use ratatui::{Terminal, backend::TestBackend};

    fn choose(dashboard: &mut DashboardState, name: &str) {
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("setup");
        };
        dialog.selected = dialog.keys().iter().position(|key| key == name).unwrap();
        dialog.form.get_mut().focus(SetupControl::List);
        dialog.prepare();
        dashboard.handle_key(key(KeyCode::Enter));
    }

    #[test]
    fn results_from_a_closed_setup_do_not_change_the_new_draft() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        let old = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        dashboard.cancel_modal();
        dashboard.begin_setup();
        let new = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        let original = setup_dialog_mut(&mut dashboard.mode).unwrap().draft.clone();
        let mut detected = dashboard.config.clone();
        detected.targets.insert(
            "stale-discovery".into(),
            hel::hel_config::TargetTemplate::LocalBare,
        );
        dashboard.setup_discovered(old, Ok(detected));
        dashboard.setup_saved(old, Ok(dashboard.config.clone()));
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert_eq!(dialog.generation, new);
        assert_eq!(dialog.draft, original);
    }

    #[test]
    fn setup_is_available_with_existing_config_and_edits_quick_creation_defaults() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.handle_key(key(KeyCode::F(4)));
        choose(&mut dashboard, "startup");
        choose(&mut dashboard, "prompt");
        choose(&mut dashboard, "profile");
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("setup");
        };
        let editor = dialog.editor.as_mut().unwrap();
        editor.selected = editor
            .choices
            .iter()
            .position(|value| value == "claude-1")
            .unwrap();
        dialog.prepare();
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        let DashboardAction::SaveSetup { updated, .. } = action else {
            panic!("{action:?}");
        };
        let saved: HelConfig = serde_json::from_str(&updated).unwrap();
        assert!(!saved.startup.prompt);
        assert_eq!(saved.startup.profile.as_deref(), Some("claude-1"));
        let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        dashboard.setup_saved(generation, Ok(saved));
        assert!(!dashboard.modal_open());
        let action = dashboard
            .quick_session_action(std::path::PathBuf::from("/project"))
            .unwrap();
        assert!(
            matches!(action, DashboardAction::CreateStartupSession { profile_id, .. } if profile_id == "claude-1")
        );
        assert!(!dashboard.prompt_has_focus());
    }

    #[test]
    fn deprecated_startup_enabled_is_preserved_but_hidden_from_setup() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.config.startup.enabled = false;
        dashboard.begin_setup();
        let dialog = setup_dialog_mut(&mut dashboard.mode).unwrap();
        assert!(!dialog.keys().iter().any(|key| key == "enabled"));
        assert_eq!(dialog.draft["startup"]["enabled"], false);
    }

    #[test]
    fn setup_adds_a_remote_runtime_and_reports_invalid_fields_without_losing_the_draft() {
        let mut dashboard = dashboard_with_session(stopped_session());
        dashboard.begin_setup();
        choose(&mut dashboard, "targets");
        dashboard.handle_key(key(KeyCode::Char('a')));
        dashboard.handle_paste("builder");
        dashboard.handle_key(key(KeyCode::Enter));
        choose(&mut dashboard, "kind");
        let Mode::Setup(dialog) = &mut dashboard.mode else {
            panic!("setup");
        };
        let editor = dialog.editor.as_mut().unwrap();
        editor.selected = editor
            .choices
            .iter()
            .position(|value| value == "ssh-docker")
            .unwrap();
        dialog.prepare();
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(action, DashboardAction::None);
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("setup");
        };
        assert!(dialog.notice.as_ref().unwrap().contains("SSH host"));
        choose(&mut dashboard, "host");
        dashboard.handle_paste("builder.example.test");
        dashboard.handle_key(key(KeyCode::Enter));
        let action = dashboard.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        let DashboardAction::SaveSetup { updated, .. } = action else {
            panic!("{action:?}");
        };
        let saved: HelConfig = serde_json::from_str(&updated).unwrap();
        assert!(
            matches!(&saved.targets["builder"], hel::hel_config::TargetTemplate::SshDocker { ssh, .. } if ssh.host == "builder.example.test")
        );
        let generation = setup_dialog_mut(&mut dashboard.mode).unwrap().generation;
        dashboard.setup_saved(generation, Err("disk full".into()));
        let Mode::Setup(dialog) = &dashboard.mode else {
            panic!("setup");
        };
        assert!(!dialog.saving);
        assert_eq!(
            dialog.draft["targets"]["builder"]["host"],
            "builder.example.test"
        );
        assert!(dialog.notice.as_ref().unwrap().contains("disk full"));
    }

    #[test]
    fn cancelling_setup_preserves_configuration_and_render_keeps_controls_visible() {
        let mut dashboard = dashboard_with_session(stopped_session());
        let original = dashboard.config.clone();
        for (width, height) in [(80, 18), (100, 30), (140, 42)] {
            dashboard.begin_setup();
            choose(&mut dashboard, "startup");
            choose(&mut dashboard, "prompt");
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::render::render(frame, &mut dashboard))
                .unwrap();
            let text = buffer_lines(terminal.backend().buffer()).join("\n");
            for label in ["Ask for a task", "Save (Ctrl-S)", "Cancel"] {
                assert!(text.contains(label), "{text}");
            }
            dashboard.handle_key(key(KeyCode::Esc));
            dashboard.handle_key(key(KeyCode::Esc));
            assert!(!dashboard.modal_open());
            assert_eq!(dashboard.config, original);
        }
    }

    #[test]
    fn expanded_form_preserves_existing_optional_settings() {
        let mut original = config();
        original.startup.prompt = false;
        original.phone.tls_cert = Some("/keys/cert.pem".into());
        original.phone.tls_key = Some("/keys/key.pem".into());
        original
            .profiles
            .get_mut("codex-1")
            .unwrap()
            .context_window_bytes = Some(250000);
        let dialog = SetupDialog::new(&original);
        let decoded: HelConfig = serde_json::from_value(dialog.draft).unwrap();
        assert_eq!(decoded, original);
    }
}
