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
    later: Vec<crate::NotificationCard>,
    pub(crate) errors: std::collections::BTreeMap<String, String>,
    pub(crate) gesture: Option<NotificationGesture>,
    pub(crate) controls: Vec<NotificationGesture>,
    pub(crate) message_read: Option<NotificationMessageRead>,
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
            gesture: None,
            controls: Vec::new(),
            message_read: None,
            scroll: 0,
            scroll_session: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct NotificationMessageRead {
    pub(crate) session_id: String,
    pub(crate) stable_id: String,
    pub(crate) status: crate::ingest::TranscriptHydration,
    pub(crate) item: Option<Box<mj_core::state::TranscriptItem>>,
}

#[derive(Debug, Clone)]
pub(crate) struct NotificationGesture {
    pub(crate) command: CommandId,
    card: crate::NotificationCard,
    holder: String,
    position: crate::NotificationPosition,
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
                        // Preserve stored millisecond precision for ordering;
                        // only zero itself is an unknown activity timestamp.
                        .filter(|millis| *millis > 0)
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
                self.notifications.later.iter().position(|card| {
                    card.session_id == entry.session_id && card.target == entry.target
                })
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

    /// The session whose content the displayed card needs, including a child
    /// form promoted to its parent's row.
    pub fn displayed_notification_session_id(&self) -> Option<String> {
        self.notification_entries()
            .first()
            .map(|entry| self.notification_content_session(&entry.session_id, entry.level))
    }

    /// Keep the existing child-family runtime subscription while its approval
    /// is displayed, even when the family is outside the active workspace.
    pub fn displayed_notification_parent_id(&self) -> Option<String> {
        self.notification_entries()
            .first()
            .filter(|entry| {
                entry.level == AttentionLevel::Waiting
                    && self.waiting_child_of(&entry.session_id).is_some()
            })
            .map(|entry| entry.session_id.clone())
    }

    /// One owner for the existing background summary-hydration selection.
    pub fn wanted_summary_sessions(&self) -> std::collections::BTreeMap<String, u64> {
        let panes: std::collections::BTreeSet<_> = self
            .pane_sessions()
            .into_iter()
            .map(|(_, session)| session)
            .collect();
        let mut wanted: std::collections::BTreeMap<_, _> = self
            .state
            .sessions
            .values()
            .filter(|session| {
                !self.is_native_agent(&session.id)
                    && (panes.contains(&session.id)
                        || (Some(session.workspace_id.as_str()) == self.active_workspace_id()
                            && session.state.is_active())
                        || self.state.subagents.get(&session.id).is_some_and(|child| {
                            Some(child.parent_session_id.as_str()) == self.subagent_parent_id()
                        }))
            })
            .map(|session| (session.id.clone(), session.viewed_through_event_ordinal))
            .collect();
        if let Some(session) = self.displayed_notification_session_id() {
            let viewed = self
                .state
                .sessions
                .get(&session)
                .map_or(0, |session| session.viewed_through_event_ordinal);
            wanted.insert(session, viewed);
        }
        wanted
    }

    /// A displayed located question outside the runtime tail needs one indexed read.
    pub fn wanted_notification_message(&self) -> Option<(String, String)> {
        let entry = self.notification_entries().into_iter().next()?;
        if entry.level != AttentionLevel::Waiting {
            return None;
        }
        let crate::NotificationPosition::Message(stable_id) =
            self.notification_position(&entry.session_id, entry.level)
        else {
            return None;
        };
        let session_id = self.notification_content_session(&entry.session_id, entry.level);
        if self.session_details.get(&session_id).is_some_and(|detail| {
            detail
                .projection
                .transcript
                .iter()
                .any(|item| item.stable_id == stable_id)
        }) {
            return None;
        }
        Some((session_id, stable_id))
    }

    pub fn sync_notification_message_read(
        &mut self,
        wanted: Option<(String, String)>,
        loading: bool,
    ) {
        let Some((session_id, stable_id)) = wanted else {
            self.notifications.message_read = None;
            return;
        };
        if self
            .notifications
            .message_read
            .as_ref()
            .is_none_or(|read| read.session_id != session_id || read.stable_id != stable_id)
        {
            self.notifications.message_read = Some(NotificationMessageRead {
                session_id,
                stable_id,
                status: crate::ingest::TranscriptHydration::Unavailable,
                item: None,
            });
        }
        let read = self.notifications.message_read.as_mut().unwrap();
        if loading {
            read.status = crate::ingest::TranscriptHydration::Loading;
        } else if read.status == crate::ingest::TranscriptHydration::Loading {
            read.status = crate::ingest::TranscriptHydration::Unavailable;
        }
    }

    pub fn notification_message_read_finished(
        &mut self,
        session_id: &str,
        stable_id: &str,
        item: Option<Box<mj_core::state::TranscriptItem>>,
    ) {
        if let Some(read) = self
            .notifications
            .message_read
            .as_mut()
            .filter(|read| read.session_id == session_id && read.stable_id == stable_id)
        {
            read.status = crate::ingest::TranscriptHydration::Ready;
            read.item = item;
        }
    }

    pub(crate) fn notification_gesture(&self, command: CommandId) -> Option<NotificationGesture> {
        self.notification_entries()
            .first()
            .map(|entry| self.gesture_for_notification(entry, command))
    }

    fn gesture_for_notification(
        &self,
        entry: &NotificationEntry,
        command: CommandId,
    ) -> NotificationGesture {
        NotificationGesture {
            command,
            card: crate::NotificationCard {
                session_id: entry.session_id.clone(),
                target: entry.target.clone(),
            },
            holder: self.notification_content_session(&entry.session_id, entry.level),
            position: self.notification_position(&entry.session_id, entry.level),
        }
    }

    pub(crate) fn activate_notification(
        &mut self,
        gesture: NotificationGesture,
    ) -> DashboardAction {
        let NotificationGesture {
            command,
            card,
            holder,
            position,
        } = gesture;
        match command {
            CommandId::NotificationClear => {
                let Some(target) = card.target else {
                    return DashboardAction::None;
                };
                if self.notification_dismissal_pending(&card.session_id) {
                    return DashboardAction::None;
                }
                self.submit_notification_target(&card.session_id, target.clone());
                DashboardAction::DismissNotification {
                    session_id: card.session_id,
                    target,
                }
            }
            CommandId::NotificationLater => {
                let entries = self.notification_entries();
                self.notifications.later.retain(|previous| {
                    previous != &card
                        && entries.iter().any(|entry| {
                            entry.session_id == previous.session_id
                                && entry.target == previous.target
                        })
                });
                self.notifications.later.push(card.clone());
                self.notifications.scroll = 0;
                DashboardAction::NotificationLater { card }
            }
            CommandId::NotificationOpen => {
                self.focus_session_from_cpu_report(&holder);
                self.focus = Focus::Prompt;
                DashboardAction::OpenAtMessage {
                    card,
                    session_id: holder,
                    position,
                }
            }
            _ => DashboardAction::None,
        }
    }

    pub(crate) fn notification_later(&mut self) -> DashboardAction {
        self.notification_gesture(CommandId::NotificationLater)
            .map_or(DashboardAction::None, |gesture| {
                self.activate_notification(gesture)
            })
    }

    pub(crate) fn notification_clear(&mut self) -> DashboardAction {
        self.notification_gesture(CommandId::NotificationClear)
            .map_or(DashboardAction::None, |gesture| {
                self.activate_notification(gesture)
            })
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
        self.notification_gesture(CommandId::NotificationOpen)
            .map_or(DashboardAction::None, |gesture| {
                self.activate_notification(gesture)
            })
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
    let recency_seconds = entry.recency / 1_000;
    let age = if recency_seconds == 0 {
        "age unknown".to_owned()
    } else {
        format!("{}s ago", now.saturating_sub(recency_seconds))
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
    dashboard.notifications.controls = [
        CommandId::NotificationClear,
        CommandId::NotificationLater,
        CommandId::NotificationOpen,
    ]
    .into_iter()
    .map(|command| dashboard.gesture_for_notification(entry, command))
    .collect();
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
        let control = if id == CommandId::NotificationClearImportant {
            SurfaceControl::Command(id)
        } else {
            SurfaceControl::Notification(id)
        };
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

        // An update after the press may replace the card or just its target.
        // Release always carries the pressed identity, never a recomputed top.
        use crossterm::event::{MouseButton, MouseEventKind};
        for label in ["Clear & Next", "Later", "Go To Message"] {
            for replace_target in [false, true] {
                let mut race = self::dashboard();
                race.set_question_message("q1", Some("old-question".into()), true);
                let target = race.notification_dismissal_target("q1");
                let lines = drawn(&mut race, 140, 50);
                let button = point(&lines, label);
                race.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), button));
                if replace_target {
                    race.notification_dismissals
                        .insert("q1".into(), NotificationDismissal::ReadThrough(99));
                    race.set_question_message("q1", Some("new-question".into()), true);
                } else {
                    race.notifications.later.push(crate::NotificationCard {
                        session_id: "q1".into(),
                        target: target.clone(),
                    });
                }
                drawn(&mut race, 140, 50);
                let action =
                    race.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), button));
                match action {
                    DashboardAction::DismissNotification {
                        session_id,
                        target: sent,
                    } => {
                        assert_eq!(session_id, "q1");
                        assert_eq!(Some(sent.clone()), target);
                        race.notification_dismissal_finished(
                            &session_id,
                            sent,
                            Err("notification is no longer current".into()),
                        );
                        assert!(race.notifications.errors["q1"].contains("no longer current"));
                    }
                    DashboardAction::NotificationLater { card } => {
                        assert_eq!(card.session_id, "q1");
                        assert_eq!(card.target, target);
                        assert_eq!(
                            race.notifications
                                .later
                                .last()
                                .map(|card| card.session_id.as_str()),
                            Some("q1")
                        );
                        if replace_target {
                            assert_eq!(
                                race.notification_entries()[0].session_id,
                                "q1",
                                "Later must not defer the replacement question"
                            );
                        }
                    }
                    DashboardAction::OpenAtMessage {
                        card,
                        session_id,
                        position,
                    } => {
                        assert_eq!(card.session_id, "q1");
                        assert_eq!(card.target, target);
                        assert_eq!(session_id, "q1");
                        assert_eq!(
                            position,
                            crate::NotificationPosition::Message("old-question".into())
                        );
                    }
                    other => panic!("captured {label}: {other:?}"),
                }
            }
        }

        // A located question can predate the entire runtime tail. Only an
        // actual background read is loading; missing/unsupported are terminal.
        let mut old = self::dashboard();
        old.set_question_message("q1", Some("outside-tail".into()), true);
        let wanted = old.wanted_notification_message();
        assert_eq!(wanted, Some(("q1".into(), "outside-tail".into())));
        old.sync_notification_message_read(wanted.clone(), true);
        assert_eq!(
            old.notification_card_text("q1", AttentionLevel::Waiting),
            "loading…"
        );
        old.notification_message_read_finished("q1", "outside-tail", None);
        assert_eq!(
            old.notification_card_text("q1", AttentionLevel::Waiting),
            "message not available"
        );
        old.sync_notification_message_read(wanted.clone(), false);
        assert_eq!(
            old.notification_card_text("q1", AttentionLevel::Waiting),
            "message not available"
        );
        old.sync_notification_message_read(wanted, true);
        old.notification_message_read_finished("q1", "outside-tail", Some(Box::new(mj_core::state::TranscriptItem {
            stable_id: "outside-tail".into(), position: 1, latest_content_event_ordinal: Some(1), created_at_ms: 0, last_changed_at_ms: 0,
            body: mj_core::state::TranscriptBody::Agent { chunks: vec![serde_json::json!({"content":{"type":"text", "text":"The buried question"}})], streaming: false },
        })));
        assert_eq!(
            old.notification_message("q1", AttentionLevel::Waiting).0,
            "The buried question"
        );
        assert_eq!(
            old.notification_position("q1", AttentionLevel::Waiting),
            crate::NotificationPosition::Message("outside-tail".into())
        );
        old.notifications.later.push(crate::NotificationCard {
            session_id: "q1".into(),
            target: old.notification_dismissal_target("q1"),
        });
        assert_ne!(
            old.wanted_notification_message(),
            Some(("q1".into(), "outside-tail".into()))
        );
        old.sync_notification_message_read(None, false);
        assert!(old.notifications.message_read.is_none());

        // Millisecond ordering must survive age formatting's coarser clock;
        // an unknown activity must never outrank a real stored timestamp.
        let mut recency = self::dashboard();
        recency
            .session_details
            .get_mut("q1")
            .unwrap()
            .last_activity_at_ms = Some(10);
        recency
            .session_details
            .get_mut("q2")
            .unwrap()
            .last_activity_at_ms = Some(30);
        assert_eq!(recency.notification_entries()[0].session_id, "q2");
        recency
            .session_details
            .get_mut("q1")
            .unwrap()
            .last_activity_at_ms = None;
        assert_eq!(recency.notification_entries()[0].session_id, "q2");
        recency
            .session_details
            .get_mut("q1")
            .unwrap()
            .last_activity_at_ms = Some(0);
        assert_eq!(recency.notification_entries()[0].session_id, "q2");
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
                card: crate::NotificationCard {
                    session_id: "q1".into(),
                    target: None
                },
                session_id: "q1".into(),
                position: crate::NotificationPosition::Message("agent:question".into())
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

        // The visible foreign card participates in the existing summary
        // selection; Later transfers that demand to the next card.
        let mut foreign = self::dashboard();
        for id in foreign.state.sessions.keys().cloned().collect::<Vec<_>>() {
            foreign.state.sessions.get_mut(&id).unwrap().workspace_id = "foreign".into();
        }
        foreign.session_details.remove("q1");
        foreign.session_details.remove("q2");
        assert!(foreign.wanted_summary_sessions().contains_key("q1"));
        assert!(!foreign.wanted_summary_sessions().contains_key("q2"));
        assert_eq!(
            foreign.notification_card_text("q1", AttentionLevel::Waiting),
            "Waiting for your input"
        );
        foreign.sync_transcript_hydration(&["q1".into()].into_iter().collect());
        assert_eq!(
            foreign.notification_card_text("q1", AttentionLevel::Waiting),
            "loading…"
        );
        foreign.apply_prepared_materialized_session_summary(
            crate::PreparedMaterializedSessionSummary::from_materialized(
                mj_core::state::MaterializedSessionSummary {
                    session_id: "q1".into(),
                    applied_event_ordinal: 4,
                    last_activity_at_ms: Some(50),
                    execution: mj_core::state::MaterializedExecutionState::Idle,
                    session_title: None,
                    last_agent_message: Some("Question from the other workspace".into()),
                    last_user_message: None,
                    last_agent_message_follows_last_user: true,
                    agent_message_latest_content_ordinals: vec![4],
                    interruption_event_ordinals: vec![],
                },
                0,
            ),
        );
        assert_eq!(
            foreign.notification_card_text("q1", AttentionLevel::Waiting),
            "Question from the other workspace"
        );
        foreign.handle_key(key(KeyCode::Char('l')));
        assert!(foreign.wanted_summary_sessions().contains_key("q2"));
        assert!(!foreign.wanted_summary_sessions().contains_key("q1"));

        // A promoted parent card retains its daemon target but displays and
        // opens the child's approval through the existing family navigation.
        let (mut family, parent) = dashboard_with_one_subagent();
        serve_attention(
            &mut family,
            &[&parent, "child-session"],
            mj_core::attention::SessionAttention::NeedsInput,
        );
        family.set_form_holders(
            [(parent.clone(), "child-session".into())]
                .into_iter()
                .collect(),
        );
        family
            .session_details
            .get_mut(&parent)
            .unwrap()
            .last_agent_message = Some("Unrelated parent output".into());
        assert_eq!(
            family.displayed_notification_session_id().as_deref(),
            Some("child-session")
        );
        assert_eq!(
            family.displayed_notification_parent_id().as_deref(),
            Some(parent.as_str())
        );
        assert!(
            family
                .wanted_summary_sessions()
                .contains_key("child-session")
        );
        assert_eq!(
            family.notification_card_text(&parent, AttentionLevel::Waiting),
            "Waiting for your input"
        );
        family.sync_transcript_hydration(&["child-session".into()].into_iter().collect());
        assert_eq!(
            family.notification_card_text(&parent, AttentionLevel::Waiting),
            "loading…"
        );
        // Cancellation/failure ends loading even if no message ever arrived.
        family.sync_transcript_hydration(&Default::default());
        assert_eq!(
            family.notification_card_text(&parent, AttentionLevel::Waiting),
            "Waiting for your input"
        );
        family.apply_prepared_materialized_session_summary(
            crate::PreparedMaterializedSessionSummary::from_materialized(
                mj_core::state::MaterializedSessionSummary {
                    session_id: "child-session".into(),
                    applied_event_ordinal: 1,
                    last_activity_at_ms: Some(0),
                    execution: mj_core::state::MaterializedExecutionState::Idle,
                    session_title: None,
                    last_agent_message: None,
                    last_user_message: None,
                    last_agent_message_follows_last_user: false,
                    agent_message_latest_content_ordinals: vec![],
                    interruption_event_ordinals: vec![],
                },
                0,
            ),
        );
        assert_eq!(
            family.session_details["child-session"].last_activity_at_ms,
            None
        );
        assert_eq!(
            family.notification_card_text(&parent, AttentionLevel::Waiting),
            "Waiting for your input"
        );
        family
            .session_details
            .get_mut(&parent)
            .unwrap()
            .last_activity_at_ms = Some(0);
        family
            .state
            .sessions
            .get_mut(&parent)
            .unwrap()
            .updated_at
            .clear();
        let lines = drawn(&mut family, 120, 40);
        assert!(lines.iter().any(|line| line.contains("age unknown")));

        let mut approval = question("child-session");
        approval.tool_permission = Some(mj_core::elicitation::ToolPermissionForm { skip: true });
        family
            .session_details
            .get_mut("child-session")
            .unwrap()
            .pending_elicitations = vec![approval.clone()];
        assert_eq!(
            family
                .notification_message(&parent, AttentionLevel::Waiting)
                .0,
            approval.message
        );
        let entries = family.notification_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, parent);
        assert_eq!(
            entries[0].target,
            family.notification_dismissal_target(&parent)
        );
        family.focus = Focus::Notifications;
        assert_eq!(
            family.handle_key(key(KeyCode::Enter)),
            DashboardAction::OpenAtMessage {
                card: crate::NotificationCard {
                    session_id: parent.clone(),
                    target: family.notification_dismissal_target(&parent)
                },
                session_id: "child-session".into(),
                position: crate::NotificationPosition::End
            }
        );
        assert_eq!(family.subagent_parent_id(), Some(parent.as_str()));
    }
}
