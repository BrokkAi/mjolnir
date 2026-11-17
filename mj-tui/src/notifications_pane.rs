//! The in-memory notification stack. Dismissals remain owned by the existing
//! session dismissal state; this pane owns only deferral order and scrolling.
use mj_chat::{components::ControlKind, theme};
use mj_core::attention::NotificationDismissal;
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Borders, Paragraph, Wrap},
};

use crate::actions::CommandId;
use crate::surface_controls::SurfaceControl;
use crate::{AttentionLevel, DashboardAction, DashboardState, Focus, PaneSize, SupportPane};

#[derive(Debug)]
pub(crate) struct NotificationsPane {
    pub(crate) size: PaneSize,
    pub(crate) maximize_enabled: bool,
    later: Vec<String>,
    pub(crate) errors: std::collections::BTreeMap<String, String>,
    scroll: u16,
    scroll_session: Option<String>,
}

impl Default for NotificationsPane {
    fn default() -> Self {
        Self {
            size: PaneSize::Standard,
            maximize_enabled: true,
            later: Vec::new(),
            errors: Default::default(),
            scroll: 0,
            scroll_session: None,
        }
    }
}

pub(crate) struct NotificationEntry {
    pub(crate) session_id: String,
    pub(crate) level: AttentionLevel,
    title: String,
    project: String,
    recency: u64,
    pub(crate) target: Option<NotificationDismissal>,
}

impl DashboardState {
    pub(crate) fn notification_entries(&self) -> Vec<NotificationEntry> {
        let mut entries: Vec<_> = self
            .served_attention
            .keys()
            .filter_map(|id| {
                if self.state.subagents.contains_key(id) {
                    return None;
                }
                let level = self.attention_level(id);
                if !matches!(level, AttentionLevel::Waiting | AttentionLevel::Important) {
                    return None;
                }
                let session = self.state.sessions.get(id)?;
                Some(NotificationEntry {
                    session_id: id.clone(),
                    level,
                    title: session.listed_title().to_owned(),
                    project: session.project_name(&self.config),
                    recency: self
                        .session_details
                        .get(id)
                        .and_then(|detail| detail.last_activity_at_ms)
                        .or_else(|| {
                            crate::render::session_updated_at_epoch_seconds(session)
                                .map(|seconds| seconds.saturating_mul(1000))
                        })
                        .unwrap_or_default(),
                    target: self.notification_dismissal_target(id),
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            let rank = |entry: &NotificationEntry| {
                self.notifications
                    .later
                    .iter()
                    .position(|id| id == &entry.session_id)
            };
            rank(a)
                .map(|n| n + 1)
                .unwrap_or(0)
                .cmp(&rank(b).map(|n| n + 1).unwrap_or(0))
                .then_with(|| {
                    (a.level == AttentionLevel::Important)
                        .cmp(&(b.level == AttentionLevel::Important))
                })
                .then_with(|| b.recency.cmp(&a.recency))
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        entries
    }

    pub(crate) fn notification_later(&mut self) -> DashboardAction {
        let entries = self.notification_entries();
        if let Some(entry) = entries.first() {
            self.notifications.later.retain(|id| {
                id != &entry.session_id && entries.iter().any(|e| &e.session_id == id)
            });
            self.notifications.later.push(entry.session_id.clone());
            self.notifications.scroll = 0;
        }
        DashboardAction::None
    }

    pub(crate) fn notification_clear(&mut self) -> DashboardAction {
        self.notification_entries()
            .first()
            .and_then(|entry| self.begin_dismiss_notification_for(&entry.session_id))
            .unwrap_or(DashboardAction::None)
    }

    pub(crate) fn notification_clear_important(&mut self) -> DashboardAction {
        let dismissals = self
            .notification_entries()
            .into_iter()
            .filter(|entry| entry.level == AttentionLevel::Important)
            .filter_map(|entry| {
                self.submit_notification_dismissal(&entry.session_id)
                    .map(|target| (entry.session_id, target))
            })
            .collect();
        DashboardAction::DismissNotifications { dismissals }
    }

    pub(crate) fn notification_open(&mut self) -> DashboardAction {
        let Some(entry) = self.notification_entries().into_iter().next() else {
            return DashboardAction::None;
        };
        let (_, message_id) = self.notification_message(&entry.session_id, entry.level);
        // The pane spans workspaces; use the normal workspace navigation owner
        // before the host starts the asynchronous attach.
        self.focus_session_from_cpu_report(&entry.session_id);
        self.focus = Focus::Prompt;
        DashboardAction::OpenAtMessage {
            session_id: entry.session_id,
            message_id,
        }
    }

    pub(crate) fn scroll_notification(&mut self, delta: i16) {
        self.notifications.scroll = self.notifications.scroll.saturating_add_signed(delta);
    }
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    dashboard: &mut DashboardState,
    entries: &[NotificationEntry],
    text: String,
) {
    let focused = dashboard.focus == Focus::Notifications;
    let minimized = dashboard.notifications.size == PaneSize::Minimized;
    let clearing = dashboard
        .session_details
        .values()
        .filter(|detail| {
            matches!(
                detail.notification_dismissal,
                Some(crate::ingest::NotificationDismissalState::Submitting(_))
            )
        })
        .count();
    let title = format!(
        " Notifications ({}){} ",
        entries.len(),
        if clearing > 0 { " clearing…" } else { "" }
    );
    let block = theme::panel(focused)
        .title(title)
        .title(crate::render::sessions::pane_size_controls(
            dashboard.notifications.size,
            dashboard.notifications.maximize_enabled,
        ))
        .borders(if minimized {
            Borders::TOP
        } else {
            Borders::ALL
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    dashboard
        .pane_size_control_areas
        .extend(crate::render::sessions::pane_size_control_areas(
            area,
            SupportPane::Notifications,
            dashboard.notifications.maximize_enabled,
        ));
    if minimized || inner.height == 0 {
        return;
    }
    let Some(entry) = entries.first() else {
        frame.render_widget(
            Paragraph::new("Nothing needs you").style(theme::muted()),
            inner,
        );
        return;
    };
    if dashboard.notifications.scroll_session.as_deref() != Some(&entry.session_id) {
        dashboard.notifications.scroll_session = Some(entry.session_id.clone());
        dashboard.notifications.scroll = 0;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = if entry.recency == 0 {
        "age unknown".to_owned()
    } else {
        format!("{}s ago", now.saturating_sub(entry.recency / 1000))
    };
    let glyph = if entry.level == AttentionLevel::Waiting {
        '?'
    } else {
        '!'
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{glyph} {} · {} · {age}",
            entry.title, entry.project
        ))
        .style(theme::actionable()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let reason = entry.target.is_none();
    let body_height = inner.height.saturating_sub(2 + u16::from(reason));
    let text = match dashboard.notifications.errors.get(&entry.session_id) {
        Some(error) => format!("Could not clear: {error}\n{text}"),
        None => text,
    };
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width)
        .saturating_sub(body_height as usize)
        .min(u16::MAX as usize) as u16;
    dashboard.notifications.scroll = dashboard.notifications.scroll.min(max_scroll);
    frame.render_widget(
        paragraph.scroll((dashboard.notifications.scroll, 0)),
        Rect::new(inner.x, inner.y + 1, inner.width, body_height),
    );
    if reason && inner.height > 2 {
        frame.render_widget(
            Paragraph::new("Clear unavailable: answer this request in the session.")
                .style(theme::muted()),
            Rect::new(inner.x, inner.bottom() - 2, inner.width, 1),
        );
    }
    if inner.height < 2 {
        return;
    }
    let important = entries
        .iter()
        .filter(|entry| entry.level == AttentionLevel::Important && entry.target.is_some())
        .count();
    let mut x = inner.x;
    let mut form = dashboard.surface_form.borrow_mut();
    for (id, label, enabled) in [
        (
            CommandId::NotificationClear,
            "Clear & Next".to_owned(),
            entry.target.is_some(),
        ),
        (CommandId::NotificationLater, "Later".to_owned(), true),
        (
            CommandId::NotificationOpen,
            "Go To Message".to_owned(),
            true,
        ),
        (
            CommandId::NotificationClearImportant,
            format!("Clear all ! ({important})"),
            important > 0,
        ),
    ] {
        let width = Line::raw(&label).width() as u16 + 2;
        if x + width > inner.right() {
            break;
        }
        let rect = Rect::new(x, inner.bottom() - 1, width, 1);
        let control = SurfaceControl::Command(id);
        form.register(control, ControlKind::Button, rect, enabled);
        let style = if enabled {
            theme::actionable().patch(theme::raised())
        } else {
            theme::muted()
        };
        frame.render_widget(Paragraph::new(format!(" {label} ")).style(style), rect);
        x += width + 1;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::*;
    use crossterm::event::KeyCode;
    use mj_core::attention::SessionAttention;

    pub(crate) fn dashboard() -> DashboardState {
        let mut dashboard = dashboard_with_session(running_session());
        dashboard.state.sessions.clear();
        for (id, title, attention, recency, target) in [
            (
                "q1",
                "Newest question",
                SessionAttention::NeedsInput,
                30,
                NotificationDismissal::Question(mj_core::assessment::InferredQuestion {
                    turn_id: "turn-1".into(),
                    completed_ordinal: 4,
                }),
            ),
            (
                "q2",
                "Older question",
                SessionAttention::NeedsInput,
                20,
                NotificationDismissal::Question(mj_core::assessment::InferredQuestion {
                    turn_id: "turn-2".into(),
                    completed_ordinal: 3,
                }),
            ),
            (
                "i1",
                "Important update",
                SessionAttention::Important,
                40,
                NotificationDismissal::ReadThrough(5),
            ),
        ] {
            let mut session = running_session();
            session.id = id.into();
            session.title = title.into();
            session.acp_session_title = None;
            session.updated_at.clear();
            session.viewed_through_event_ordinal = 0;
            dashboard.state.sessions.insert(id.into(), session);
            serve_attention(&mut dashboard, &[id], attention);
            let detail = dashboard.session_details.entry(id.into()).or_default();
            detail.last_activity_at_ms = Some(recency);
            detail.last_agent_message = Some("Which approach should we take?\nThe question stays here until you answer or clear it.".into());
            dashboard.notification_dismissals.insert(id.into(), target);
        }
        dashboard.focus = Focus::Notifications;
        dashboard
    }

    #[test]
    fn notifications_keys_rotate_submit_and_restore_failed_dismissals() {
        let mut dashboard = dashboard();
        assert_eq!(dashboard.notification_entries()[0].session_id, "q1");
        dashboard.handle_key(key(KeyCode::Char('l')));
        assert_eq!(dashboard.notification_entries()[0].session_id, "q2");
        dashboard.handle_key(key(KeyCode::Char('l')));
        assert_eq!(dashboard.notification_entries()[0].session_id, "i1");
        dashboard.handle_key(key(KeyCode::Char('l')));
        assert_eq!(dashboard.notification_entries()[0].session_id, "q1");
        let action = dashboard.handle_key(key(KeyCode::Char('d')));
        let DashboardAction::DismissNotification { session_id, target } = action else {
            panic!("dismissal action");
        };
        assert_eq!(session_id, "q1");
        assert_eq!(dashboard.attention_level("q1"), AttentionLevel::Idle);
        assert_eq!(dashboard.notification_entries()[0].session_id, "q2");
        dashboard.notification_dismissal_finished(
            &session_id,
            target,
            Err("Worker disconnected".into()),
        );
        assert!(
            dashboard
                .notification_entries()
                .iter()
                .any(|entry| entry.session_id == "q1")
        );
        assert_eq!(dashboard.notifications.errors["q1"], "Worker disconnected");
        assert_eq!(dashboard.attention_level("q1"), AttentionLevel::Waiting);
        let DashboardAction::DismissNotifications { dismissals } =
            dashboard.handle_key(key(KeyCode::Char('c')))
        else {
            panic!("batch dismissal");
        };
        assert_eq!(
            dismissals,
            vec![("i1".into(), NotificationDismissal::ReadThrough(5))]
        );
        assert_eq!(dashboard.notification_entries().len(), 2);
        assert!(
            dashboard
                .notification_entries()
                .iter()
                .all(|entry| entry.level == AttentionLevel::Waiting)
        );
        assert_eq!(dashboard.attention_level("i1"), AttentionLevel::Idle);
        dashboard.notification_dismissal_finished(
            "i1",
            NotificationDismissal::ReadThrough(5),
            Ok(5),
        );
        assert_eq!(dashboard.attention_level("i1"), AttentionLevel::Idle);
        // A new served dismissal target invalidates the same cached attention
        // projection without needing another session-record change.
        dashboard
            .notification_dismissals
            .insert("i1".into(), NotificationDismissal::ReadThrough(6));
        assert_eq!(dashboard.attention_level("i1"), AttentionLevel::Important);
        dashboard
            .notification_dismissals
            .insert("i1".into(), NotificationDismissal::ReadThrough(5));
        assert_eq!(dashboard.attention_level("i1"), AttentionLevel::Idle);
        assert!(
            dashboard
                .notification_entries()
                .iter()
                .all(|entry| entry.level == AttentionLevel::Waiting)
        );
    }

    #[test]
    fn notifications_keys_navigate_without_dismissing_and_keep_sizes_exclusive() {
        let mut dashboard = dashboard();
        dashboard.notification_dismissals.remove("q1");
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Char('d'))),
            DashboardAction::None
        );
        dashboard
            .session_details
            .get_mut("q1")
            .unwrap()
            .question_message = Some("agent:question".into());
        let action = dashboard.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            DashboardAction::OpenAtMessage {
                session_id: "q1".into(),
                message_id: Some("agent:question".into())
            }
        );
        assert_eq!(dashboard.focus, Focus::Prompt);
        assert_eq!(dashboard.notification_entries().len(), 3);
        dashboard.focus = Focus::Quota;
        dashboard.handle_key(key(KeyCode::Tab));
        assert_eq!(dashboard.focus, Focus::Notifications);
        dashboard.set_pane_size(SupportPane::Targets, PaneSize::Maximized);
        dashboard.set_pane_size(SupportPane::Notifications, PaneSize::Maximized);
        assert!(dashboard.pane_sizes().validate().is_ok());
        assert_eq!(
            dashboard.pane_size(SupportPane::Targets),
            PaneSize::Standard
        );
        dashboard.set_pane_size(SupportPane::Quota, PaneSize::Maximized);
        assert_eq!(
            dashboard.pane_size(SupportPane::Notifications),
            PaneSize::Standard
        );
        dashboard.set_pane_size(SupportPane::Notifications, PaneSize::Maximized);
        dashboard
            .restore_pane_sizes(crate::PaneSizes {
                targets: PaneSize::Maximized,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            dashboard.pane_size(SupportPane::Notifications),
            PaneSize::Standard
        );
    }
}
