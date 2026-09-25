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
                session_title: session.listed_title().to_owned(),
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
                    self.subagent_question(session_id).map(|(child, question)| {
                        format!(
                            "Sub-agent \"{}\": {}",
                            child.listed_title(),
                            question.message
                        )
                    })
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

    /// Reports that a session's worker cannot be reached, in one plain
    /// sentence. The host keeps the relay's own error and any worker
    /// diagnostics for its log; the footer only says what happened.
    /// `checking` is true while worker diagnostics are still being collected.
    pub fn report_session_unreachable(&mut self, session_id: &str, checking: bool) {
        let name = self.session_notice_name(session_id);
        let text = if checking {
            format!("{name} cannot be reached; checking its worker.")
        } else {
            format!("{name} cannot be reached. The log has the worker's details.")
        };
        self.set_notice(text.clone());
        self.unreachable_notices.insert(session_id.to_owned(), text);
    }

    /// Withdraws the unreachable notice for a session whose worker answered
    /// again, if that notice is still the one showing.
    pub fn report_session_reachable(&mut self, session_id: &str) {
        let Some(text) = self.unreachable_notices.remove(session_id) else {
            return;
        };
        let name = self.session_notice_name(session_id);
        self.replace_notice_if(&text, format!("{name} is reachable again."));
    }

    /// Reports that a session could not be opened, naming it, so a failure
    /// from a restored pane says which of its sessions it was about.
    pub fn report_open_failure(&mut self, session_id: &str, error: &str) {
        let name = self.session_notice_name(session_id);
        let detach = self
            .first_key_label(crate::CommandId::QuitDetach)
            .map(|key| format!(" {key} quits."))
            .unwrap_or_default();
        self.set_notice(format!(
            "Could not open {name}: {error}. Press Enter in Sessions to retry, or select another session.{detach}"
        ));
    }

    /// Whether `session_id` is not running and no transition is bringing it
    /// back, so it has no worker to attach to.
    pub(crate) fn session_has_no_worker(&self, session_id: &str) -> bool {
        self.transition_kind(session_id).is_none()
            && self
                .state
                .sessions
                .get(session_id)
                .is_some_and(|session| !session.state.is_active())
    }

    /// Whether a pane holding `session_id` must let it go rather than attach
    /// to it: the session is suspended (or otherwise not running) and no
    /// transition is bringing it back.
    ///
    /// A harness-native child and a stopped Mjolnir sub-agent have no worker
    /// either, but their panes draw them from the store, so they stay
    /// (R10-2: a finished native child's row is `Stopped`, and Browse let it
    /// go as soon as the selection put it there).
    pub fn pane_session_is_suspended(&self, session_id: &str) -> bool {
        self.session_has_no_worker(session_id)
            && !self.is_native_agent(session_id)
            && !self.state.subagents.contains_key(session_id)
    }

    /// Whether a pane showing `session_id` may start attaching to it: no
    /// lifecycle owns it, it has not failed, and it still has a worker. A
    /// suspended session has none, so an attach would only wait out its
    /// timeout and report "Session opening did not respond" (R4-11).
    pub fn pane_session_can_attach(&self, session_id: &str) -> bool {
        self.transition_kind(session_id).is_none()
            && self.transition_failure_kind(session_id).is_none()
            && !self.session_failed(session_id)
            && !self.pane_session_is_suspended(session_id)
    }

    /// Empties the pane that held a suspended session, instead of attaching
    /// to a session with no worker, and says so. The host saves the layout.
    pub fn release_suspended_pane(&mut self, pane: crate::tile_layout::PaneId, session_id: &str) {
        self.set_pane_session(pane, None);
        let name = self.session_notice_name(session_id);
        let resume = self
            .first_key_label(crate::CommandId::ResumeDialog)
            .map(|key| format!(" {key} finds it to resume."))
            .unwrap_or_default();
        self.set_notice(format!(
            "{name} is suspended, so it was unpinned from its pane.{resume}"
        ));
    }

    pub(crate) fn session_notice_name(&self, session_id: &str) -> String {
        self.state.sessions.get(session_id).map_or_else(
            || format!("Session {}", &session_id[..session_id.len().min(8)]),
            |session| format!("Session {}", session.listed_title()),
        )
    }

    /// The terminal title the host should show: `mj` alone when nothing
    /// needs a person, otherwise counts such as `mj · 1 unreachable · 2 waiting
    /// · 1 unread`. `None` when
    /// the configuration keeps the title alone.
    pub fn terminal_title(&self) -> Option<String> {
        if !self.config.notify.title {
            return None;
        }
        // Questions, unreachable workers and failures each get their own
        // word, most urgent first: "waiting" means a question for a person.
        let (mut failed, mut unreachable, mut waiting, mut unread) = (0, 0, 0, 0);
        for entry in self.attention_queue() {
            match entry.level {
                AttentionLevel::Failed => failed += 1,
                AttentionLevel::Unreachable => unreachable += 1,
                AttentionLevel::Waiting => waiting += 1,
                _ => unread += 1,
            }
        }
        let mut title = String::from("mj");
        for (count, word) in [
            (failed, "failed"),
            (unreachable, "unreachable"),
            (waiting, "waiting"),
            (unread, "unread"),
        ] {
            if count > 0 {
                title.push_str(&format!(" · {count} {word}"));
            }
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

    #[test]
    fn the_title_names_an_unreachable_worker_apart_from_questions() {
        let mut dashboard = dashboard();
        dashboard.state.sessions.get_mut("done").unwrap().state =
            mj_core::state::SessionState::Disconnected;
        assert_eq!(
            dashboard.terminal_title().as_deref(),
            Some("mj · 1 unreachable · 1 waiting")
        );
    }

    #[test]
    fn an_unreachable_worker_is_reported_plainly_and_withdrawn_on_recovery() {
        let mut dashboard = dashboard();
        let title = dashboard.state.sessions["done"].display_title().to_owned();
        dashboard.report_session_unreachable("done", true);
        let notice = dashboard.notice().unwrap();
        assert_eq!(
            notice,
            format!("Session {title} cannot be reached; checking its worker.")
        );
        dashboard.report_session_unreachable("done", false);
        assert!(!dashboard.notice().unwrap().contains("stderr"));
        dashboard.report_session_reachable("done");
        assert_eq!(
            dashboard.notice().unwrap(),
            format!("Session {title} is reachable again.")
        );
        // A later, unrelated notice is left alone by another recovery.
        dashboard.report_session_unreachable("done", false);
        dashboard.set_notice("Profile quotas refreshed.");
        dashboard.report_session_reachable("done");
        assert_eq!(dashboard.notice().unwrap(), "Profile quotas refreshed.");
    }
}
