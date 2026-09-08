//! A fresh task prompt, isolated from the currently selected conversation.
use std::cell::RefCell;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use mj_chat::components::{ButtonRow, ControlKind, FieldEdit, Form, Interaction, TextField};
use mj_chat::hel_selection::FrameSurfaces;
use mj_chat::hel_text_input::TextInput;
use mj_chat::theme;
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::widgets::{Paragraph, Wrap};

use crate::dialogs::DialogControl;
use crate::{DashboardAction, DashboardState, Mode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuickNewDialog {
    generation: u64,
    prompt: TextInput,
    pub(crate) preparing: bool,
    pub(crate) form: RefCell<Form<DialogControl>>,
    notice: Option<String>,
}

impl DashboardState {
    pub(crate) fn begin_quick_new(&mut self) -> DashboardAction {
        self.focus_sessions();
        if !self.config.startup.prompt {
            return DashboardAction::QuickNewSession {
                generation: None,
                initial_prompt: None,
            };
        }
        let mut form = Form::new();
        form.register(
            DialogControl::Field,
            ControlKind::TextField,
            Rect::default(),
            true,
        );
        form.focus(DialogControl::Field);
        static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.mode = Mode::QuickNew(QuickNewDialog {
            generation: NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            prompt: TextInput::multiline(),
            preparing: false,
            form: RefCell::new(form),
            notice: None,
        });
        DashboardAction::None
    }

    pub fn finish_quick_new(&mut self, generation: Option<u64>) {
        fn finish(mode: &mut Mode, generation: Option<u64>) {
            match mode {
                Mode::QuickNew(dialog)
                    if Some(dialog.generation) == generation && dialog.preparing =>
                {
                    *mode = Mode::Dashboard
                }
                Mode::Help(help) => finish(&mut help.return_to, generation),
                _ => {}
            }
        }
        finish(&mut self.mode, generation);
    }

    pub fn quick_new_failed(&mut self, generation: Option<u64>, error: String) {
        fn fail(mode: &mut Mode, generation: Option<u64>, error: &str) {
            match mode {
                Mode::QuickNew(dialog) if Some(dialog.generation) == generation => {
                    dialog.preparing = false;
                    dialog.notice = Some(error.to_owned());
                }
                Mode::Help(help) => fail(&mut help.return_to, generation, error),
                _ => {}
            }
        }
        fail(&mut self.mode, generation, &error);
        self.set_failure_notice(error);
    }

    pub(crate) fn handle_quick_new_event(
        &mut self,
        event: Event,
        mut dialog: QuickNewDialog,
    ) -> DashboardAction {
        if dialog.preparing {
            if matches!(event, Event::Key(key) if key.code == KeyCode::Esc) {
                self.cancel_modal();
            } else {
                self.mode = Mode::QuickNew(dialog);
            }
            return DashboardAction::None;
        }
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Enter && key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT))
            && dialog.form.borrow().is_focused(DialogControl::Field)
        {
            dialog.prompt.push('\n');
            self.mode = Mode::QuickNew(dialog);
            return DashboardAction::None;
        }
        match dialog.form.get_mut().handle(&event).action {
            Some(Interaction::Cancel | Interaction::Activate(DialogControl::Cancel)) => {
                self.cancel_modal()
            }
            Some(Interaction::Edit(DialogControl::Field, edit)) => {
                match edit {
                    FieldEdit::Key(key) => {
                        dialog.prompt.handle_key(key);
                    }
                    FieldEdit::Paste(text) => {
                        dialog.prompt.insert_str(&text.replace("\r\n", "\n"));
                    }
                    FieldEdit::Cursor(offset) => dialog.prompt.set_cursor(offset),
                }
                self.mode = Mode::QuickNew(dialog);
            }
            Some(Interaction::Activate(DialogControl::Field | DialogControl::Primary)) => {
                let initial_prompt =
                    (!dialog.prompt.trim().is_empty()).then(|| dialog.prompt.value().to_owned());
                dialog.preparing = true;
                dialog.notice = None;
                let generation = Some(dialog.generation);
                self.mode = Mode::QuickNew(dialog);
                return DashboardAction::QuickNewSession {
                    generation,
                    initial_prompt,
                };
            }
            _ => self.mode = Mode::QuickNew(dialog),
        }
        DashboardAction::None
    }
}

pub(crate) fn render_quick_new(
    frame: &mut Frame,
    area: Rect,
    dialog: &QuickNewDialog,
    surfaces: &mut FrameSurfaces,
) {
    let popup = crate::widgets::centered_modal(frame, surfaces, 78, 16, area);
    frame.render_widget(theme::modal().title(" New session "), popup);
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    if inner.height < 3 {
        form.end_frame(DialogControl::Field);
        return;
    }
    let message = dialog.notice.as_deref().unwrap_or(if dialog.preparing {
        "Creating your session… Esc hides this window."
    } else {
        "What would you like to do? Enter starts; Shift-Enter adds a line."
    });
    frame.render_widget(
        Paragraph::new(message).wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );
    let field = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(4),
    );
    TextField::render_multiline(
        frame,
        field,
        &dialog.prompt,
        !dialog.preparing,
        !dialog.preparing,
        &mut form,
        DialogControl::Field,
    );
    if !dialog.preparing {
        ButtonRow::render(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            &[
                (DialogControl::Cancel, "Cancel", true),
                (DialogControl::Primary, "Start session", true),
            ],
            &mut form,
        );
    }
    form.end_frame(DialogControl::Field);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn rendered_quick_new_click_inserts_at_the_clicked_character() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_quick_new();
        dashboard.handle_paste("abcdefghi");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (x, y) = (0..30)
            .find_map(|y| {
                (0..92).find_map(|x| {
                    let text = (x..x + 9)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>();
                    (text == "abcdefghi").then_some((x, y))
                })
            })
            .expect("prompt is rendered");
        dashboard.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x + 4,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
        dashboard.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE));
        assert!(
            matches!(&dashboard.mode, Mode::QuickNew(dialog) if dialog.prompt.value() == "abcdXefghi")
        );
    }

    #[test]
    fn a_background_launch_cannot_close_a_newer_task_prompt() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_quick_new();
        let old = match dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            DashboardAction::QuickNewSession { generation, .. } => generation,
            _ => panic!("launch"),
        };
        dashboard.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        dashboard.begin_quick_new();
        dashboard.handle_paste("another task");
        dashboard.finish_quick_new(old);
        dashboard.quick_new_failed(old, "older launch failed".into());
        assert!(
            matches!(&dashboard.mode, Mode::QuickNew(dialog) if !dialog.preparing && dialog.prompt.value() == "another task" && dialog.notice.is_none())
        );
    }

    #[test]
    fn quick_new_keeps_the_task_separate_until_the_new_session_launches() {
        let mut dashboard = dashboard_with_session(running_session());
        let selected = dashboard.selected_session_id.clone();
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            DashboardAction::None
        );
        let generation = match &dashboard.mode {
            Mode::QuickNew(dialog) => Some(dialog.generation),
            _ => panic!("fresh prompt"),
        };
        let prompt = format!(
            "Fix this:\n{}\nKeep indentation\n\tand Unicode λ",
            "x".repeat(70_000)
        );
        dashboard.handle_paste(&prompt);
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::QuickNewSession {
                generation,
                initial_prompt: Some(prompt.clone())
            }
        );
        assert_eq!(dashboard.selected_session_id, selected);
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DashboardAction::None
        );
        dashboard.quick_new_failed(generation, "Runtime unavailable".into());
        assert!(
            matches!(&dashboard.mode, Mode::QuickNew(dialog) if dialog.prompt.value() == prompt && !dialog.preparing)
        );
    }

    #[test]
    fn saved_setting_skips_the_task_prompt_and_the_wizard_has_its_own_key() {
        let mut config = config();
        config.startup.prompt = false;
        let mut dashboard = DashboardState::new(config, Default::default(), Default::default());
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            DashboardAction::QuickNewSession {
                generation: None,
                initial_prompt: None
            }
        );
        assert_eq!(
            dashboard.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)),
            DashboardAction::None
        );
        assert!(matches!(dashboard.mode, Mode::New(_)));
    }
}
