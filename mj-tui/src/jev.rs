//! On-demand inspector state. Its host performs all I/O asynchronously.
use crate::dialogs::DialogControl;
use crate::{DashboardAction, DashboardState, Mode};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use mj_chat::components::Dialog;
use mj_core::jev::DecisionPage;
use std::cell::{Cell, RefCell};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JevDialog {
    pub form: RefCell<Dialog<DialogControl>>,
    session: String,
    generation: u64,
    id: Option<String>,
    result: Option<Result<DecisionPage, String>>,
    selected: usize,
    scroll: usize,
    max_scroll: Cell<usize>,
    technical: bool,
}
fn generation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
impl DashboardState {
    pub fn open_jev_decisions(&mut self, session: String, id: Option<String>) {
        self.mode = Mode::JevDecisions(JevDialog {
            form: RefCell::new(Dialog::default()),
            session,
            generation: generation(),
            id,
            result: None,
            selected: 0,
            scroll: 0,
            max_scroll: Cell::new(0),
            technical: false,
        });
    }
    pub(crate) fn begin_jev_decisions(&mut self, id: Option<String>) {
        let Some(session) = self.command_session().map(|s| s.id.clone()) else {
            return;
        };
        self.open_jev_decisions(session, id);
    }
    pub fn jev_request(&self) -> Option<(u64, String, Option<String>)> {
        match &self.mode {
            Mode::JevDecisions(d) => Some((d.generation, d.session.clone(), d.id.clone())),
            _ => None,
        }
    }
    pub fn apply_jev_result(&mut self, generation: u64, result: Result<DecisionPage, String>) {
        if let Mode::JevDecisions(d) = &mut self.mode
            && d.generation == generation
        {
            d.result = Some(result);
        }
    }
    pub(crate) fn handle_jev_event(&mut self, event: Event, mut d: JevDialog) -> DashboardAction {
        if let Event::Key(key) = event
            && key.kind != KeyEventKind::Release
        {
            self.record_event_handled();
            match key.code {
                KeyCode::Esc if d.id.is_none() => {
                    self.cancel_modal();
                    return DashboardAction::None;
                }
                KeyCode::Esc | KeyCode::Backspace => {
                    d.id = None;
                    d.result = None;
                    d.generation = generation();
                    d.scroll = 0;
                }
                KeyCode::Char('r') => {
                    d.result = None;
                    d.generation = generation();
                }
                KeyCode::Enter if d.id.is_none() => {
                    if let Some(Ok(page)) = &d.result
                        && let Some(record) = page.decisions.get(d.selected)
                    {
                        d.id = Some(record.id.clone());
                        d.result = None;
                        d.generation = generation();
                        d.scroll = 0;
                    }
                }
                KeyCode::Char('t') if d.id.is_some() => {
                    d.technical = !d.technical;
                    d.scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if d.id.is_none() {
                        let count = d
                            .result
                            .as_ref()
                            .and_then(|r| r.as_ref().ok())
                            .map_or(0, |p| p.decisions.len());
                        d.selected = d.selected.saturating_add(1).min(count.saturating_sub(1));
                        d.scroll = d.selected.saturating_sub(4) * 3;
                    } else {
                        d.scroll = d.scroll.saturating_add(1).min(d.max_scroll.get());
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if d.id.is_none() {
                        d.selected = d.selected.saturating_sub(1);
                        d.scroll = d.selected.saturating_sub(4) * 3;
                    } else {
                        d.scroll = d.scroll.saturating_sub(1);
                    }
                }
                KeyCode::PageDown if d.id.is_none() => {
                    let count = d
                        .result
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .map_or(0, |p| p.decisions.len());
                    d.selected = d.selected.saturating_add(10).min(count.saturating_sub(1));
                }
                KeyCode::PageUp if d.id.is_none() => d.selected = d.selected.saturating_sub(10),
                KeyCode::PageDown => d.scroll = d.scroll.saturating_add(10).min(d.max_scroll.get()),
                KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(10),
                KeyCode::Home => {
                    d.scroll = 0;
                    d.selected = 0;
                }
                KeyCode::End if d.id.is_none() => {
                    d.selected = d
                        .result
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .map_or(0, |p| p.decisions.len().saturating_sub(1));
                }
                KeyCode::End => d.scroll = d.max_scroll.get(),
                _ => {}
            }
        } else {
            let result = d.form.get_mut().handle(&event);
            if matches!(
                result.action,
                Some(
                    mj_chat::components::Interaction::Cancel
                        | mj_chat::components::Interaction::Activate(DialogControl::NoticeLogClose)
                )
            ) {
                self.cancel_modal();
                return DashboardAction::None;
            }
        }
        self.mode = Mode::JevDecisions(d);
        DashboardAction::None
    }
}

pub(crate) fn render(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    d: &JevDialog,
    surfaces: &mut mj_chat::selection::FrameSurfaces,
) {
    use mj_chat::theme;
    use ratatui::{
        layout::{Margin, Rect},
        widgets::{Paragraph, Wrap},
    };
    let popup = crate::widgets::centered_modal(
        frame,
        surfaces,
        94,
        area.height.saturating_sub(4).max(8),
        area,
    );
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let mut form = d.form.borrow_mut();
    form.begin_frame();
    let title = crate::widgets::dismissible_modal_title(
        &mut form,
        popup,
        "Jev decisions",
        theme::title(true),
        true,
    );
    frame.render_widget(theme::modal().title(title), popup);
    let mut selected_row = 0;
    let mut text = "Exact submitted conversation text is retained locally in rotating logs (4 × 8 MiB per owner).\n\n".to_owned();
    match &d.result {
        None => text.push_str("Loading… Esc goes back or closes while the read is pending."),
        Some(Err(error)) => text.push_str(error),
        Some(Ok(page)) => {
            for warning in &page.warnings {
                text.push_str(warning);
                text.push('\n');
            }
            if page.decisions.is_empty() {
                text.push_str(if d.id.is_some() {
                    "Details no longer available."
                } else {
                    "No retained Jev checks for this session."
                });
            }
            for (index, record) in page.decisions.iter().enumerate() {
                if index == d.selected {
                    selected_row = Paragraph::new(text.as_str())
                        .wrap(Wrap { trim: false })
                        .line_count(inner.width.max(1));
                }
                text.push_str(&format!(
                    "{} {} · {} · {}\n",
                    if d.id.is_none() && index == d.selected {
                        "›"
                    } else {
                        " "
                    },
                    record.kind,
                    record.status,
                    chrono::DateTime::from_timestamp_millis(record.started_at_ms)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default()
                ));
                if d.id.is_some() {
                    text.push_str(&format!(
                        "\nChecked: {}\n\nAnswer: {}\n\nAction: {}\n\nEvidence: {}\n\n",
                        record.checked, record.answer, record.action, record.scope
                    ));
                    if d.technical {
                        text.push_str(
                            &serde_json::to_string_pretty(&serde_json::json!({"elapsed_ms":record.updated_at_ms.saturating_sub(record.started_at_ms), "details":record.technical})).unwrap_or_default(),
                        );
                    }
                } else {
                    text.push_str(&record.action);
                    text.push_str("\n\n");
                }
            }
        }
    }
    let body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    d.max_scroll.set(
        paragraph
            .line_count(body.width.max(1))
            .saturating_sub(body.height as usize),
    );
    let scroll = if d.id.is_none() {
        selected_row.saturating_sub(body.height as usize / 2)
    } else {
        d.scroll
    };
    frame.render_widget(
        paragraph.scroll((
            scroll.min(d.max_scroll.get()).min(u16::MAX as usize) as u16,
            0,
        )),
        body,
    );
    let footer = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    frame.render_widget(
        Paragraph::new(if d.id.is_some() {
            "↑↓ scroll · T technical details / exact input · R reload · Esc back"
        } else {
            "↑↓ select · Enter details · R reload · Esc close"
        }),
        footer,
    );
    form.end_frame(DialogControl::NoticeLogClose);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{dashboard_with_session, key, running_session};
    #[test]
    fn inspector_closes_while_loading_and_ignores_old_results() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_jev_decisions(None);
        let first = dashboard.jev_request().unwrap();
        dashboard.handle_event_result(Event::Key(key(KeyCode::Esc)));
        assert!(dashboard.jev_request().is_none());
        dashboard.apply_jev_result(first.0, Err("late failure".into()));
        assert!(dashboard.jev_request().is_none());
        dashboard.begin_jev_decisions(None);
        let second = dashboard.jev_request().unwrap();
        assert_ne!(first.0, second.0);
        dashboard.apply_jev_result(first.0, Err("stale failure".into()));
        let Mode::JevDecisions(dialog) = &dashboard.mode else {
            panic!("inspector closed")
        };
        assert!(dialog.result.is_none());
    }

    #[test]
    fn details_are_requested_only_after_selection_and_rotated_records_are_displayed() {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.begin_jev_decisions(None);
        let (generation, _, id) = dashboard.jev_request().unwrap();
        assert!(id.is_none());
        dashboard.apply_jev_result(
            generation,
            Ok(DecisionPage {
                decisions: vec![mj_core::jev::Decision {
                    version: 1,
                    id: "decision-1".into(),
                    session_id: "s".into(),
                    kind: "activity".into(),
                    started_at_ms: 1,
                    updated_at_ms: 2,
                    status: "applied".into(),
                    checked: "Is the work finished?".into(),
                    answer: "Finished".into(),
                    action: "Marked ready".into(),
                    scope: "Current request".into(),
                    owner: "worker".into(),
                    technical: None,
                }],
                warnings: vec![],
            }),
        );
        dashboard.handle_event_result(Event::Key(key(KeyCode::Enter)));
        let (generation, _, id) = dashboard.jev_request().unwrap();
        assert_eq!(id.as_deref(), Some("decision-1"));
        dashboard.apply_jev_result(generation, Ok(DecisionPage::default()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 25)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let lines = crate::test_support::buffer_lines(terminal.backend().buffer());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Details no longer available"))
        );
    }
}
