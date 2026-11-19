//! Desktop and bell alerts consume the daemon notify-once event stream.

use mj_core::config::NotifyMode;
use std::time::{Duration, Instant};

use crate::{AttentionLevel, DashboardState};

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

pub(crate) struct PendingAlert {
    event: mj_client::runtime_feed::AlertEvent,
    received: Instant,
}

impl DashboardState {
    pub(crate) fn attention_notice_level(&self, session_id: &str) -> AttentionLevel {
        self.attention_level(session_id)
    }

    pub(crate) fn failure_drawn(&mut self, session_id: &str) {
        if !self.modal_open()
            && matches!(
                self.notification_dismissal_target(session_id),
                Some(mj_core::attention::NotificationDismissal::Failure(_))
            )
        {
            self.rendered_failure_dismissals
                .insert(session_id.to_owned());
        }
    }

    /// Drain only failures actually drawn; the host submits these through the
    /// same supervised action as an explicit notification dismissal.
    pub fn take_drawn_failure_dismissals(&mut self) -> Vec<crate::DashboardAction> {
        std::mem::take(&mut self.rendered_failure_dismissals)
            .into_iter()
            .filter_map(|id| self.begin_dismiss_notification_for(&id))
            .collect()
    }

    /// Advance the durable stream, suppressing history on initial attachment.
    pub fn set_alert_events(
        &mut self,
        high: u64,
        events: Vec<mj_client::runtime_feed::AlertEvent>,
    ) {
        let Some(after) = self.alert_sequence else {
            self.alert_sequence = Some(high);
            return;
        };
        let visible = self.current_session_id().map(str::to_owned);
        for event in events {
            if event.sequence <= after
                || visible.as_deref() == Some(&event.destination)
                || self.config.notify.mode == NotifyMode::Off
            {
                continue;
            }
            self.pending_alerts.push(PendingAlert {
                event,
                received: Instant::now(),
            });
        }
        self.alert_sequence = Some(after.max(high));
    }

    pub fn notification_events(&mut self) -> Vec<Notification> {
        self.notification_events_at(Instant::now())
    }

    fn notification_events_at(&mut self, now: Instant) -> Vec<Notification> {
        let pending = std::mem::take(&mut self.pending_alerts);
        if self.config.notify.mode == NotifyMode::Off {
            return Vec::new();
        }
        let delay = Duration::from_secs(self.config.notify.delay_seconds);
        let mut notices = Vec::new();
        for alert in pending {
            if now.saturating_duration_since(alert.received) < delay {
                self.pending_alerts.push(alert);
                continue;
            }
            let event = alert.event;
            // The daemon made the event from a published item, so with no delay it
            // rings as is. A delayed event rings only if its item survived the
            // delay; the card map may lag the event batch, which a delay absorbs.
            let published = delay.is_zero()
                || self.notification_cards.as_ref().is_some_and(|cards| {
                    cards.iter().any(|(session, card)| {
                        (0..card.entries.len() + usize::from(card.form.is_some())).any(|item| {
                            card.item_destination(session, item) == event.destination
                                && card.item_identity(session, item).as_deref()
                                    == Some(&event.identity)
                        })
                    })
                });
            if !published || self.current_session_id() == Some(event.destination.as_str()) {
                continue;
            }
            notices.push(Notification {
                session_id: event.destination,
                session_title: event.session_title,
                body: event.text,
                level: match event.kind {
                    mj_core::attention::NotificationKind::NeedsInput => AttentionLevel::Waiting,
                    mj_core::attention::NotificationKind::Important => AttentionLevel::Important,
                },
            });
        }
        notices
    }

    pub(crate) fn notification_content_session(
        &self,
        session_id: &str,
        level: AttentionLevel,
    ) -> String {
        if level == AttentionLevel::Waiting {
            self.waiting_child_of(session_id)
                .unwrap_or_else(|| session_id.to_owned())
        } else {
            session_id.to_owned()
        }
    }

    pub(crate) fn notification_card_text(&self, session_id: &str, level: AttentionLevel) -> String {
        let holder = self.notification_content_session(session_id, level);
        let detail = self.session_details.get(&holder);
        let loading = detail.is_some_and(|detail| {
            detail.transcript_hydration == crate::ingest::TranscriptHydration::Loading
        });
        let missing_content = detail.is_none_or(|detail| {
            detail.last_agent_message.is_none()
                && detail.pending_elicitations.is_empty()
                && detail.question_message.is_none()
        });
        if loading && missing_content {
            "loading…".to_owned()
        } else {
            self.notification_message(session_id, level)
        }
    }

    /// What to say about a session at `level`: the question's own message,
    /// the recorded error, or the last line the agent wrote.
    pub(crate) fn notification_message(&self, session_id: &str, level: AttentionLevel) -> String {
        if matches!(level, AttentionLevel::Waiting | AttentionLevel::Important)
            && let Some(card) = self
                .notification_cards
                .as_ref()
                .and_then(|cards| cards.get(session_id))
        {
            return self.notification_item_text(
                session_id,
                card,
                (card.entries.len() + usize::from(card.form.is_some())).saturating_sub(1),
            );
        }
        let holder = self.notification_content_session(session_id, level);
        let session_id = holder.as_str();
        let detail = self.session_details.get(session_id);
        let located_read = self.notifications.message_read.as_ref().filter(|read| {
            read.request.session_id == holder
                && detail
                    .and_then(|detail| detail.question_message.as_ref())
                    .is_some_and(|id| {
                        matches!(&read.request.source,
                        crate::NotificationReadSource::Item(stable_id) if stable_id == id)
                    })
        });
        match level {
            AttentionLevel::Waiting | AttentionLevel::Important => {
                use mj_core::notification_message::{
                    NotificationKind, NotificationMessageFacts, NotificationMessageSource,
                    notification_message_placeholder, notification_message_sources,
                };
                let kind = if level == AttentionLevel::Waiting {
                    NotificationKind::NeedsInput
                } else {
                    NotificationKind::Important
                };
                let review_activity = self
                    .session_review(session_id)
                    .and_then(|review| review.activity_label());
                let facts = NotificationMessageFacts {
                    pending_request: detail
                        .and_then(|detail| detail.pending_elicitations.last())
                        .map(|request| request.message.as_str()),
                    review_activity,
                    question_message: detail.and_then(|detail| detail.question_message.as_deref()),
                    supports_open_request: detail.is_none_or(|detail| detail.supports_open_request),
                };
                notification_message_sources(kind, &facts)
                    .into_iter()
                    .find_map(|source| match source {
                        NotificationMessageSource::PendingRequest(text)
                        | NotificationMessageSource::ReviewActivity(text) => Some(text.to_owned()),
                        NotificationMessageSource::QuestionMessage(target) => {
                            let detail = detail?;
                            mj_core::transcript::materialized_agent_message_text(
                                detail.projection.transcript.iter().map(AsRef::as_ref),
                                target,
                            )
                            .or_else(|| {
                                located_read.map(|read| {
                                    if read.status == crate::ingest::TranscriptHydration::Loading {
                                        return "loading…".to_owned();
                                    }
                                    read.text
                                        .clone()
                                        .unwrap_or_else(|| "message not available".to_owned())
                                })
                            })
                        }
                        NotificationMessageSource::LastAgentMessage => {
                            detail?.last_agent_message.as_deref().map(str::to_owned)
                        }
                    })
                    .unwrap_or_else(|| notification_message_placeholder(kind).to_owned())
            }
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
        }
    }

    /// Reports that a session's worker cannot be reached, in one plain
    /// sentence. The host keeps the relay's own error and any worker
    /// diagnostics for its log; the footer only says what happened.
    pub fn report_session_unreachable(&mut self, session_id: &str) {
        if self.unreachable_notices.contains_key(session_id) {
            return;
        }
        let name = self.session_notice_name(session_id);
        let text = format!("{name} cannot be reached. The log has the worker's details.");
        self.set_notice(text.clone());
        self.unreachable_notices.insert(session_id.to_owned(), text);
    }

    /// Forget an unreachable episode when its worker is no longer expected.
    pub fn clear_session_unreachable(&mut self, session_id: &str) {
        if let Some(text) = self.unreachable_notices.remove(session_id)
            && self.notice().as_deref() == Some(text.as_str())
        {
            self.clear_notice();
        }
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
                .is_some_and(|session| !session.state.has_live_worker())
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

    /// Lets a pane go of a suspended session instead of attaching to a
    /// session with no worker. The host cancels the attach and saves the
    /// layout.
    ///
    /// Browse shows the selection, so it keeps a stopped row that is still
    /// listed because its failure needs a person: emptying it would clear the
    /// selection the row's keys act on. Any other suspended session has left
    /// the list, and Browse just empties. A pinned pane empties and says so.
    pub fn release_suspended_pane(&mut self, pane: crate::tile_layout::PaneId, session_id: &str) {
        if pane == self.browse_pane() {
            if !self.stopped_notification_row(session_id) {
                self.set_pane_session(pane, None);
            }
            return;
        }
        self.set_pane_session(pane, None);
        let name = self.session_notice_name(session_id);
        self.say_suspended_session_was_unpinned(&name);
    }

    pub(crate) fn say_suspended_session_was_unpinned(&mut self, name: &str) {
        let resume = self
            .first_key_label(crate::CommandId::ResumeDialog)
            .map(|key| format!(" {key} finds it to resume."))
            .unwrap_or_default();
        self.set_notice(format!(
            "{name} is suspended, so it was unpinned from its pane.{resume}"
        ));
    }

    /// "Session" and the shared notice name, cut to fit a notice. See
    /// [`mj_core::state::State::session_notice_name`].
    pub(crate) fn session_notice_name(&self, session_id: &str) -> String {
        format!(
            "Session {}",
            crate::fit_session_name(
                &self.state.session_notice_name(session_id),
                crate::NOTICE_NAME_CELLS
            )
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
        let (mut failed, mut unreachable, mut waiting, mut important, mut unread) = (0, 0, 0, 0, 0);
        for entry in self.attention_queue() {
            // Ignored sessions still render their state in the list; the
            // title is an interruption channel, so they are not in it.
            if self
                .state
                .sessions
                .get(&entry.session_id)
                .is_some_and(|session| session.notifications_ignored)
            {
                continue;
            }
            match entry.level {
                AttentionLevel::Failed => failed += 1,
                AttentionLevel::Unreachable => unreachable += 1,
                AttentionLevel::Waiting => waiting += 1,
                AttentionLevel::Important => important += 1,
                AttentionLevel::Unread => unread += 1,
                _ => {}
            }
        }
        let mut title = String::from("mj");
        for (count, word) in [
            (failed, "failed"),
            (unreachable, "unreachable"),
            (waiting, "waiting"),
            (important, "important"),
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

    use mj_core::state::{STATE_VERSION, State};

    use crate::DashboardState;
    use crate::test_support::{config, running_session};

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
        let mut sessions = mj_core::snapshot_map::SnapshotMap::new();
        for id in ["asks", "done"] {
            let mut session = running_session();
            session.id = id.into();
            sessions.insert(session.id.clone(), session);
        }
        let mut dashboard = DashboardState::new(
            config(),
            State {
                last_oracle_choices: Default::default(),
                operator_presence: Default::default(),
                last_subagent_policies: Default::default(),
                subagents: Default::default(),
                version: STATE_VERSION,
                sessions,
                mount_history: Default::default(),
                container_sizes: Default::default(),
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
        dashboard.set_session_attention(mj_core::snapshot_map::SnapshotMap::from([
            (
                "asks".into(),
                mj_core::attention::SessionAttention::NeedsInput,
            ),
            ("done".into(), mj_core::attention::SessionAttention::Unread),
        ]));
        dashboard.set_current_session(None);
        dashboard
    }

    #[test]
    fn the_title_names_an_unreachable_worker_apart_from_questions() {
        let mut dashboard = dashboard();
        assert_eq!(
            dashboard.terminal_title().as_deref(),
            Some("mj · 1 waiting · 1 unread")
        );
        dashboard.state.sessions.get_mut("done").unwrap().state =
            mj_core::state::SessionState::Disconnected;
        crate::test_support::serve_attention(
            &mut dashboard,
            &["done"],
            mj_core::attention::SessionAttention::Unreachable,
        );

        assert_eq!(
            dashboard.terminal_title().as_deref(),
            Some("mj · 1 unreachable · 1 waiting")
        );
        let mut config = dashboard.config.clone();
        config.notify.title = false;
        dashboard.set_config(config);
        assert_eq!(dashboard.terminal_title(), None);
    }

    // Hard-won: 88460860c553: relay stderr leaked into the footer and the unreachable notice stayed after recovery.
    #[test]
    fn an_unreachable_worker_is_reported_plainly_and_withdrawn_on_recovery() {
        let mut dashboard = dashboard();
        let title = dashboard.state.sessions["done"].listed_title().to_owned();
        dashboard.report_session_unreachable("done");
        let notice = dashboard.notice().unwrap();
        assert_eq!(
            notice,
            format!("Session {title} cannot be reached. The log has the worker's details.")
        );
        dashboard.report_session_unreachable("done");
        assert!(!dashboard.notice().unwrap().contains("stderr"));
        dashboard.report_session_reachable("done");
        assert_eq!(
            dashboard.notice().unwrap(),
            format!("Session {title} is reachable again.")
        );
        // A later, unrelated notice is left alone by another recovery.
        dashboard.report_session_unreachable("done");
        dashboard.set_notice("Profile quotas refreshed.");
        dashboard.report_session_reachable("done");
        assert_eq!(dashboard.notice().unwrap(), "Profile quotas refreshed.");
    }

    // Hard-won: #1444: repeated worker polls restored a dismissed unreachable warning.
    #[test]
    fn unreachable_episode_stays_dismissed_until_recovery_and_new_failure() {
        let mut dashboard = dashboard();
        dashboard.report_session_unreachable("done");
        let first_deadline = std::time::Instant::now() + mj_chat::chat::NOTICE_MINIMUM_DISPLAY;
        let first = dashboard.notice().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
        dashboard.report_session_unreachable("done");
        assert!(dashboard.notices.dismiss(first_deadline));
        dashboard.report_session_unreachable("done");
        assert_eq!(dashboard.notice(), None);
        assert_eq!(dashboard.notices.history().len(), 1);

        dashboard.report_session_reachable("done");
        assert_eq!(dashboard.notice(), None);
        dashboard.report_session_unreachable("done");
        assert_eq!(dashboard.notice().as_deref(), Some(first.as_str()));
        dashboard.set_notice("The daemon found a missing target.");
        dashboard.report_session_unreachable("done");
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("The daemon found a missing target.")
        );
        let mut without_session = (*dashboard.state).clone();
        without_session.sessions.remove("done");
        dashboard.set_state(without_session);
        assert!(!dashboard.unreachable_notices.contains_key("done"));
        assert_eq!(
            dashboard.notice().as_deref(),
            Some("The daemon found a missing target.")
        );
    }

    // Hard-won: 751b46ebb: alert consumption ignored the configured delay and item disappearance.
    #[test]
    fn alert_cursor_suppresses_history_duplicates_and_visible_destinations() {
        use mj_client::runtime_feed::AlertEvent;
        use std::time::Duration;
        let mut dashboard = dashboard();
        dashboard.config.notify.delay_seconds = 0;
        dashboard.notification_cards = Some(
            ["asks", "done"]
                .into_iter()
                .map(|id| {
                    (
                        id.into(),
                        mj_client::runtime_feed::NotificationCard {
                            since_ms: 0,
                            badge: mj_core::attention::NotificationKind::NeedsInput,
                            entries: Vec::new(),
                            dismissal: None,
                            holder: None,
                            form: Some(mj_client::runtime_feed::CardForm {
                                raised_at_ms: 1,
                                request_id: "elicitation-1".into(),
                                message: "Question?".into(),
                            }),
                        },
                    )
                })
                .collect(),
        );
        let event = |sequence, destination: &str| AlertEvent {
            sequence,
            identity: serde_json::json!(["form", destination, "elicitation-1", 1]).to_string(),
            destination: destination.into(),
            session_title: "Title".into(),
            text: "Question?".into(),
            kind: mj_core::attention::NotificationKind::NeedsInput,
        };
        dashboard.set_alert_events(7, vec![event(7, "asks")]);
        assert!(dashboard.notification_events().is_empty());
        dashboard.set_alert_events(8, vec![event(8, "asks")]);
        let notices = dashboard.notification_events();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            (notices[0].session_title.as_str(), notices[0].body.as_str()),
            ("Title", "Question?")
        );
        dashboard.set_alert_events(8, vec![event(8, "asks")]);
        assert!(dashboard.notification_events().is_empty());
        dashboard.set_current_session(Some("asks"));
        dashboard.set_alert_events(9, vec![event(9, "asks")]);
        assert!(dashboard.notification_events().is_empty());
        dashboard.set_current_session(None);
        dashboard.set_alert_events(9, vec![event(9, "asks")]);
        assert!(dashboard.notification_events().is_empty());
        dashboard.set_alert_events(11, vec![event(10, "asks"), event(11, "done")]);
        assert_eq!(dashboard.notification_events().len(), 2);
        dashboard.set_alert_events(12, vec![event(12, "asks")]);
        dashboard.set_current_session(Some("asks"));
        assert!(
            dashboard.notification_events().is_empty(),
            "opening the destination before emission suppresses the ring"
        );
        dashboard.set_current_session(None);
        dashboard.set_alert_events(13, vec![event(13, "done")]);
        dashboard.config.notify.mode = mj_core::config::NotifyMode::Off;
        assert!(
            dashboard.notification_events().is_empty(),
            "turning notifications off before emission suppresses the ring"
        );
        dashboard.config.notify.mode = mj_core::config::NotifyMode::Terminal;
        dashboard.config.notify.delay_seconds = 5;
        dashboard.set_alert_events(14, vec![event(14, "asks")]);
        let received = dashboard.pending_alerts[0].received;
        assert!(
            dashboard
                .notification_events_at(received + Duration::from_secs(4))
                .is_empty()
        );
        assert_eq!(
            dashboard
                .notification_events_at(received + Duration::from_secs(5))
                .len(),
            1
        );
        assert!(
            dashboard
                .notification_events_at(received + Duration::from_secs(6))
                .is_empty()
        );

        dashboard.set_alert_events(15, vec![event(15, "asks")]);
        let received = dashboard.pending_alerts[0].received;
        // A new form reuses the request ID, but is a different published item.
        dashboard
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("asks")
            .unwrap()
            .form
            .as_mut()
            .unwrap()
            .raised_at_ms = 2;
        assert!(
            dashboard
                .notification_events_at(received + Duration::from_secs(5))
                .is_empty()
        );

        dashboard.set_alert_events(16, vec![event(16, "done")]);
        let received = dashboard.pending_alerts[0].received;
        dashboard
            .notification_cards
            .as_mut()
            .unwrap()
            .remove("done");
        assert!(
            dashboard
                .notification_events_at(received + Duration::from_secs(5))
                .is_empty()
        );
    }
}
