//! Notifications: what the host should tell the person about sessions they
//! are not looking at.
//!
//! The dashboard decides; the host (`mj-cli`) emits. `DashboardState` keeps
//! one episode per session that needs a person and reports it once, after the
//! configured delay, unless that session's conversation is the one on screen.
//! The delay is what stops a question the agent answers itself a moment later
//! from ringing. The host turns each [`Notification`] into a terminal bell, a
//! system notification, or nothing, by the `[notify]` configuration, and sets
//! the terminal title from [`DashboardState::terminal_title`].

use mj_core::config::NotifyMode;

use crate::{AttentionLevel, DashboardState};

/// A failure shown in this terminal. A changed error or a recovered session
/// starts a new notice; viewing an error does not change its underlying state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewedFailure {
    state: mj_core::state::SessionState,
    message: Option<String>,
}

impl ViewedFailure {
    pub(crate) fn matches(&self, session: &mj_core::state::SessionRecord) -> bool {
        self.state == session.state && self.message == session.last_error
    }
}

/// One thing worth interrupting the person for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub session_id: String,
    /// The session's display name, for the notification's title.
    pub session_title: String,
    pub level: AttentionLevel,
    /// One line saying what happened: the question, the failure, or that the
    /// agent finished.
    pub body: String,
}

/// One stretch of a session needing a person at one level, and whether it
/// has been reported yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttentionEpisode {
    level: AttentionLevel,
    since_ms: u64,
    reported: bool,
}

impl DashboardState {
    pub(crate) fn attention_notice_level(&self, session_id: &str) -> AttentionLevel {
        let level = self.attention_level(session_id);
        if level == AttentionLevel::Failed
            && self.viewed_failures.get(session_id).is_some_and(|seen| {
                self.state
                    .sessions
                    .get(session_id)
                    .is_some_and(|session| seen.matches(session))
            })
        {
            AttentionLevel::Inactive
        } else {
            level
        }
    }

    /// Remember only failures whose explanation was actually drawn, not rows
    /// merely selected behind a modal or while the terminal is too small.
    pub(crate) fn failure_drawn(&mut self, session_id: &str) {
        if self.modal_open() {
            return;
        }
        if let Some(session) = self.state.sessions.get(session_id) {
            self.drawn_failures.insert(
                session_id.to_owned(),
                ViewedFailure {
                    state: session.state,
                    message: session.last_error.clone(),
                },
            );
        }
    }

    /// The notifications due at `now_ms`, each reported exactly once per
    /// episode. Sessions the person is already looking at are marked as
    /// reported without a notification, so switching away later does not
    /// ring for a question they have seen.
    pub fn notification_events(&mut self, now_ms: u64) -> Vec<Notification> {
        let delay_ms = self.config.notify.delay_seconds.saturating_mul(1000);
        let enabled = self.config.notify.mode != NotifyMode::Off;
        let visible = self.current_session_id().map(str::to_owned);
        let mut episodes = std::mem::take(&mut self.attention_episodes);
        let mut due = Vec::new();
        let ids = self
            .state
            .sessions
            .keys()
            .filter(|id| !self.state.is_subagent_session(id))
            .cloned()
            .collect::<Vec<_>>();
        episodes.retain(|id, _| self.state.sessions.contains_key(id));
        for id in ids {
            let level = self.attention_notice_level(&id);
            if !level.needs_person() {
                episodes.remove(&id);
                continue;
            }
            let episode = episodes.entry(id.clone()).or_insert(AttentionEpisode {
                level,
                since_ms: now_ms,
                reported: false,
            });
            if episode.level != level {
                *episode = AttentionEpisode {
                    level,
                    since_ms: now_ms,
                    reported: false,
                };
            }
            if episode.reported {
                continue;
            }
            if visible.as_deref() == Some(id.as_str()) {
                episode.reported = true;
                continue;
            }
            if now_ms.saturating_sub(episode.since_ms) < delay_ms {
                continue;
            }
            episode.reported = true;
            if !enabled {
                continue;
            }
            let Some(session) = self.state.sessions.get(&id) else {
                continue;
            };
            due.push(Notification {
                session_id: id.clone(),
                session_title: session.display_title().to_owned(),
                level,
                body: self.notification_body(&id, level),
            });
        }
        self.attention_episodes = episodes;
        due
    }

    /// What to say about a session at `level`: the question's own message,
    /// the recorded error, or the last line the agent wrote.
    fn notification_body(&self, session_id: &str, level: AttentionLevel) -> String {
        let detail = self.session_details.get(session_id);
        let text = match level {
            AttentionLevel::Waiting => detail
                .and_then(|detail| detail.pending_elicitations.first())
                .map(|request| request.message.clone())
                .or_else(|| {
                    self.session_review(session_id)
                        .and_then(|review| review.activity_label())
                        .map(str::to_owned)
                })
                .or_else(|| {
                    detail
                        .and_then(|detail| detail.last_agent_message.as_deref())
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "Waiting for your input".to_owned()),
            AttentionLevel::Failed => self
                .state
                .sessions
                .get(session_id)
                .and_then(|session| session.last_error.clone())
                .unwrap_or_else(|| "Session failed".to_owned()),
            AttentionLevel::Unread
                if detail.is_some_and(|detail| detail.unread_interruptions > 0) =>
            {
                "Work was interrupted".to_owned()
            }
            _ => detail
                .and_then(|detail| detail.last_agent_message.as_deref())
                .map(str::to_owned)
                .unwrap_or_else(|| "Finished".to_owned()),
        };
        let line = text.lines().next().unwrap_or_default().trim();
        let mut body = line.chars().take(120).collect::<String>();
        if line.chars().count() > 120 {
            body.push('…');
        }
        body
    }

    /// The terminal title the host should show: `mj` alone when nothing
    /// needs a person, otherwise `mj · 2 waiting · 1 unread`. `None` when
    /// the configuration keeps the title alone.
    pub fn terminal_title(&self) -> Option<String> {
        if !self.config.notify.title {
            return None;
        }
        let (waiting, unread) =
            self.attention_queue()
                .into_iter()
                .fold((0, 0), |(waiting, unread), entry| match entry.level {
                    AttentionLevel::Waiting
                    | AttentionLevel::Unreachable
                    | AttentionLevel::Failed => (waiting + 1, unread),
                    _ => (waiting, unread + 1),
                });
        let mut title = String::from("mj");
        if waiting > 0 {
            title.push_str(&format!(" · {waiting} waiting"));
        }
        if unread > 0 {
            title.push_str(&format!(" · {unread} unread"));
        }
        Some(title)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mj_core::config::NotifyMode;
    use mj_core::state::{STATE_VERSION, State};

    use crate::test_support::{config, running_session};
    use crate::{AttentionLevel, DashboardState};

    fn question(session_id: &str) -> mj_core::elicitation::ElicitationRequest {
        mj_core::elicitation::ElicitationRequest::from_acp_params(
            format!("{session_id}-question"),
            serde_json::json!({
                "mode": "form",
                "sessionId": session_id,
                "message": "Which branch should I use?",
                "requestedSchema": {"type": "object", "properties": {"b": {"type": "string"}}}
            }),
        )
        .expect("valid test question")
    }

    fn dashboard() -> DashboardState {
        let mut sessions = BTreeMap::new();
        for id in ["asks", "done"] {
            let mut session = running_session();
            session.id = id.into();
            sessions.insert(session.id.clone(), session);
        }
        let mut dashboard = DashboardState::new(
            config(),
            State {
                subagents: Default::default(),
                version: STATE_VERSION,
                sessions,
                mount_history: BTreeMap::new(),
                container_sizes: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        dashboard
            .session_details
            .get_mut("asks")
            .unwrap()
            .pending_elicitations = vec![question("asks")];
        let done = dashboard.session_details.get_mut("done").unwrap();
        done.unread_agent_messages = 1;
        done.last_agent_message = Some("All tests pass now.\nSecond line".into());
        dashboard.set_current_session(None);
        dashboard
    }

    #[test]
    fn unstructured_waiting_uses_agent_text_then_fallback() {
        let mut dashboard = dashboard();
        let detail = dashboard.session_details.get_mut("asks").unwrap();
        detail.pending_elicitations.clear();
        detail.last_agent_message = Some("Which branch?".into());
        assert_eq!(
            dashboard.notification_body("asks", AttentionLevel::Waiting),
            "Which branch?"
        );
        dashboard
            .session_details
            .get_mut("asks")
            .unwrap()
            .last_agent_message = None;
        assert_eq!(
            dashboard.notification_body("asks", AttentionLevel::Waiting),
            "Waiting for your input"
        );
    }

    #[test]
    fn interrupted_work_notifies_with_its_own_message() {
        let mut dashboard = dashboard();
        dashboard
            .session_details
            .get_mut("done")
            .unwrap()
            .unread_interruptions = 1;
        assert!(dashboard.notification_events(0).is_empty());
        let due = dashboard.notification_events(2_000);
        assert_eq!(
            due.iter()
                .find(|notice| notice.session_id == "done")
                .unwrap()
                .body,
            "Work was interrupted"
        );
    }

    #[test]
    fn reopening_still_notifies_for_unresolved_activity() {
        for _ in 0..2 {
            let mut dashboard = dashboard();
            assert!(dashboard.notification_events(0).is_empty());
            assert_eq!(dashboard.notification_events(2_000).len(), 2);
        }
    }

    #[test]
    fn a_new_question_notifies_once_after_the_delay() {
        let mut dashboard = dashboard();
        // Nothing before the two-second default delay has elapsed.
        assert!(dashboard.notification_events(0).is_empty());
        assert!(dashboard.notification_events(1_999).is_empty());
        let due = dashboard.notification_events(2_000);
        assert_eq!(due.len(), 2, "{due:?}");
        let asks = due.iter().find(|n| n.session_id == "asks").unwrap();
        assert_eq!(asks.level, AttentionLevel::Waiting);
        assert_eq!(asks.body, "Which branch should I use?");
        let done = due.iter().find(|n| n.session_id == "done").unwrap();
        assert_eq!(done.level, AttentionLevel::Unread);
        assert_eq!(done.body, "All tests pass now.");
        // Reported once; the same episode never repeats.
        assert!(dashboard.notification_events(60_000).is_empty());

        // Answering and asking again starts a new episode.
        dashboard
            .session_details
            .get_mut("asks")
            .unwrap()
            .pending_elicitations
            .clear();
        assert!(dashboard.notification_events(61_000).is_empty());
        dashboard
            .session_details
            .get_mut("asks")
            .unwrap()
            .pending_elicitations = vec![question("asks")];
        assert!(dashboard.notification_events(61_000).is_empty());
        let again = dashboard.notification_events(63_000);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].session_id, "asks");
    }

    #[test]
    fn the_open_conversation_never_notifies_even_after_switching_away() {
        let mut dashboard = dashboard();
        dashboard.set_current_session(Some("asks"));
        // The first call starts both episodes; the delay runs from there.
        assert!(dashboard.notification_events(0).is_empty());
        let due = dashboard.notification_events(5_000);
        assert_eq!(
            due.iter()
                .map(|n| n.session_id.as_str())
                .collect::<Vec<_>>(),
            ["done"]
        );
        dashboard.set_current_session(Some("done"));
        assert!(dashboard.notification_events(10_000).is_empty());
    }

    #[test]
    fn notifications_off_reports_nothing_but_still_tracks_episodes() {
        let mut dashboard = dashboard();
        let mut config = dashboard.config.clone();
        config.notify.mode = NotifyMode::Off;
        dashboard.set_config(config);
        assert!(dashboard.notification_events(5_000).is_empty());
        // Turning notifications on later does not replay what was already due.
        let mut config = dashboard.config.clone();
        config.notify.mode = NotifyMode::Terminal;
        dashboard.set_config(config);
        assert!(dashboard.notification_events(6_000).is_empty());
    }

    #[test]
    fn the_title_counts_waiting_and_unread_and_can_be_turned_off() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.terminal_title().as_deref(),
            Some("mj · 1 waiting · 1 unread")
        );
        dashboard
            .session_details
            .get_mut("asks")
            .unwrap()
            .pending_elicitations
            .clear();
        dashboard
            .session_details
            .get_mut("done")
            .unwrap()
            .unread_agent_messages = 0;
        assert_eq!(dashboard.terminal_title().as_deref(), Some("mj"));
        let mut config = dashboard.config.clone();
        config.notify.title = false;
        dashboard.set_config(config);
        assert_eq!(dashboard.terminal_title(), None);
    }
}
