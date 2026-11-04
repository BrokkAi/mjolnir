//! The Detection results view: what a runtimes detection turned down, as one
//! block per runtime — name, reason, and an indented fix — in a scrollable
//! panel, so a status line never truncates a remediation again.

use super::*;
use crossterm::event::MouseEventKind;
use ratatui::text::Span;
use std::cell::Cell;

/// The open Detection results view: how far the blocks are scrolled, and the
/// last rendered maximum so End can reach the bottom.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct ReportState {
    scroll: u16,
    max_scroll: Cell<u16>,
}

impl ReportState {
    fn scroll_to(&mut self, target: u16) {
        self.scroll = target.min(self.max_scroll.get());
    }
}

/// Each refusal as its own block. The reason stands alone and the fix sits on
/// an indented `Fix:` line of its own, never folded back into the reason.
pub(super) fn report_lines(rejected: &[RejectedRuntime]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, runtime) in rejected.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(runtime.label.clone(), Modifier::BOLD));
        lines.push(Line::raw(runtime.detail.trim().to_owned()));
        if let Some(remediation) = &runtime.remediation {
            lines.push(Line::from(vec![
                Span::styled("  Fix: ", theme::muted()),
                Span::raw(remediation.trim().to_owned()),
            ]));
        }
    }
    lines
}

/// Applies a scroll key or wheel step to the open report, and reports whether
/// the event was the report's: scrolling, or leaving it.
pub(super) fn handle_report_event(dialog: &mut SetupDialog, event: &Event) -> bool {
    let Some(report) = dialog.report.as_mut() else {
        return false;
    };
    let step = match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Down => 1,
            KeyCode::Up => -1,
            KeyCode::PageDown => 10,
            KeyCode::PageUp => -10,
            KeyCode::Home => {
                report.scroll_to(0);
                return true;
            }
            KeyCode::End => {
                let max = report.max_scroll.get();
                report.scroll_to(max);
                return true;
            }
            KeyCode::Esc | KeyCode::Backspace => {
                dialog.report = None;
                return true;
            }
            _ => return false,
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollDown => 3,
            MouseEventKind::ScrollUp => -3,
            _ => return false,
        },
        _ => return false,
    };
    let scroll = i32::from(report.scroll) + step;
    report.scroll_to(scroll.clamp(0, i32::from(u16::MAX)) as u16);
    true
}

pub(super) fn render_report(frame: &mut Frame, popup: Rect, inner: Rect, dialog: &SetupDialog) {
    let Some(report) = dialog.report.as_ref() else {
        return;
    };
    let mut form = dialog.form.borrow_mut();
    form.begin_frame();
    let title = dismissible_modal_title(
        &mut form,
        popup,
        "Settings",
        theme::title(true),
        !dialog.saving,
    );
    frame.render_widget(
        theme::modal()
            .title(title)
            .title(Line::styled(" Detection results ", theme::muted()).right_aligned())
            .title_bottom(Line::raw(" ↑/↓ scroll · PgUp/PgDn · Home/End · Esc back ")),
        popup,
    );
    frame.render_widget(
        Paragraph::new("Why the last Detect runtimes left each one out. Esc returns to Runtimes.")
            .wrap(Wrap { trim: false })
            .style(theme::muted()),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );
    let footer_row = mj_chat::components::DialogShell::layout(inner, 0).actions;
    let top = inner.y + 3;
    let body = Rect::new(
        inner.x,
        top,
        inner.width,
        footer_row.y.saturating_sub(top).max(1),
    );
    let blocks =
        Paragraph::new(report_lines(&dialog.detection_rejected)).wrap(Wrap { trim: false });
    let total = u16::try_from(blocks.line_count(body.width.max(1))).unwrap_or(u16::MAX);
    let max_scroll = total.saturating_sub(body.height);
    report.max_scroll.set(max_scroll);
    frame.render_widget(blocks.scroll((report.scroll.min(max_scroll), 0)), body);
    Dialog::render_actions(frame, footer_row, &dialog.footer_actions(), &mut form);
    form.end_frame(SetupControl::Back);
}
