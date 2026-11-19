//! The daemon publishes notification cards. This pane owns item selection,
//! deferrals, per-session reply editors and their submission feedback.
use mj_chat::{components::ControlKind, theme};
use mj_core::attention::{EntryMessage, NotificationDismissal, NotificationKind};
use mj_core::notification_message::notification_message_placeholder;
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Borders, Paragraph},
};

use crate::actions::CommandId;
use crate::surface_controls::SurfaceControl;
use crate::{AttentionLevel, DashboardAction, DashboardState, Focus, PaneSize, SupportPane};

#[derive(Debug)]
pub(crate) struct NotificationsPane {
    pub(crate) size: PaneSize,
    pub(crate) maximize_enabled: bool,
    skipped: Vec<SkippedCard>,
    pub(crate) errors: std::collections::BTreeMap<String, String>,
    pub(crate) gesture: Option<NotificationGesture>,
    pub(crate) controls: Vec<NotificationGesture>,
    pub(crate) message_read: Option<NotificationMessageRead>,
    replies: std::collections::BTreeMap<String, QuickReply>,
    form_errors: std::collections::BTreeMap<String, String>,
    pub(crate) form_area: Option<Rect>,
    scroll: u16,
    /// The last displayed card, used to match the selected item across publications.
    scroll_card: Option<CardIdentity>,
    selected_item: usize,
    selection_revision: u64,
    rendered_presentation: Option<(u64, PaneSize)>,
    text_generation: u64,
    text_rows: Option<mj_chat::selection::SelectableText>,
    form_text: Option<(String, u16)>,
}

impl Default for NotificationsPane {
    fn default() -> Self {
        Self {
            size: PaneSize::Standard,
            maximize_enabled: true,
            skipped: Vec::new(),
            errors: Default::default(),
            gesture: None,
            controls: Vec::new(),
            message_read: None,
            replies: Default::default(),
            form_errors: Default::default(),
            form_area: None,
            scroll: 0,
            scroll_card: None,
            selected_item: 0,
            selection_revision: 0,
            rendered_presentation: None,
            text_generation: 0,
            text_rows: None,
            form_text: None,
        }
    }
}

struct QuickReply {
    editor: mj_chat::chat::ChatState,
    pending: Vec<ReplySubmission>,
    error: Option<String>,
    /// Failed submissions and drafts displaced by recovery remain recoverable
    /// independently of the current editor, until that text is sent successfully.
    recoverable: Vec<RecoverableReply>,
}
#[derive(Debug)]
struct RecoverableReply {
    text: String,
    error: Option<String>,
}
impl QuickReply {
    fn restore_candidate(&self) -> Option<&str> {
        self.recoverable
            .iter()
            .find(|reply| reply.text != self.editor.single_line_prompt_text())
            .map(|reply| reply.text.as_str())
    }
    fn keep_unsent(&mut self, text: String, error: Option<String>) {
        if let Some(previous) = self.recoverable.iter_mut().find(|reply| reply.text == text) {
            if error.is_some() {
                previous.error = error;
            }
        } else if !text.trim().is_empty() {
            self.recoverable.push(RecoverableReply { text, error });
        }
    }
}
impl std::fmt::Debug for QuickReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickReply")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}
#[derive(Debug)]
struct ReplySubmission {
    card: CardIdentity,
    command_id: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationReadSource {
    Item(String),
    Through(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NotificationReadRequest {
    pub session_id: String,
    pub source: NotificationReadSource,
    /// Changes on each card or item transition, including a return to the same item.
    pub selection_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CardIdentity {
    action: crate::NotificationCard,
    published: Option<std::sync::Arc<mj_client::runtime_feed::NotificationCard>>,
}

#[derive(Debug, PartialEq, Eq)]
enum SkippedCard {
    Published(String),
    Legacy(crate::NotificationCard),
}

impl CardIdentity {
    fn skip_identity(&self) -> SkippedCard {
        if self.published.is_some() {
            SkippedCard::Published(self.action.session_id.clone())
        } else {
            SkippedCard::Legacy(self.action.clone())
        }
    }
}

impl NotificationsPane {
    pub(crate) fn prune_published_skips(&mut self, contains: impl Fn(&str) -> bool) {
        self.skipped.retain(|card| match card {
            SkippedCard::Published(session_id) => contains(session_id),
            SkippedCard::Legacy(_) => true,
        });
    }
}

#[derive(Debug)]
pub(crate) struct NotificationMessageRead {
    pub(crate) request: NotificationReadRequest,
    pub(crate) status: crate::ingest::TranscriptHydration,
    pub(crate) text: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct NotificationGesture {
    pub(crate) command: CommandId,
    identity: CardIdentity,
    selection_revision: u64,
    session_id: String,
    text: String,
    restore_text: Option<String>,
}

pub(crate) struct NotificationEntry {
    pub(crate) session_id: String,
    pub(crate) level: AttentionLevel,
    title: String,
    project: String,
    recency: u64,
    pub(crate) target: Option<NotificationDismissal>,
    published: Option<std::sync::Arc<mj_client::runtime_feed::NotificationCard>>,
}

impl NotificationEntry {
    fn action_card(&self) -> crate::NotificationCard {
        crate::NotificationCard {
            session_id: self.session_id.clone(),
            target: self.target.clone(),
        }
    }

    fn identity(&self) -> CardIdentity {
        CardIdentity {
            action: self.action_card(),
            published: self.published.clone(),
        }
    }

    fn matches(&self, card: &CardIdentity) -> bool {
        self.session_id == card.action.session_id
            && self.target == card.action.target
            && self.published == card.published
    }

    fn matches_skip(&self, skipped: &SkippedCard) -> bool {
        match skipped {
            SkippedCard::Published(session_id) => {
                self.published.is_some() && self.session_id == *session_id
            }
            SkippedCard::Legacy(card) => {
                self.published.is_none()
                    && self.session_id == card.session_id
                    && self.target == card.target
            }
        }
    }

    fn retained_item_index(&self, previous: &CardIdentity, index: usize) -> Option<usize> {
        if self.matches(previous) {
            return Some(index);
        }
        if self.session_id != previous.action.session_id {
            return None;
        }
        let card = self.published.as_ref()?;
        let previous = previous.published.as_ref()?;
        if let Some(item) = previous.entries.get(index) {
            return card
                .entries
                .iter()
                .position(|entry| entry.completed_ordinal == item.completed_ordinal);
        }
        let previous_form = previous.form.as_ref()?;
        let form = card.form.as_ref()?;
        (card.holder == previous.holder && form.request_id == previous_form.request_id)
            .then_some(card.entries.len())
    }

    fn item_count(&self) -> usize {
        self.published.as_ref().map_or(1, |card| {
            card.entries.len() + usize::from(card.form.is_some())
        })
    }
}

impl DashboardState {
    fn quick_reply_mut(&mut self, session_id: &str) -> &mut QuickReply {
        self.notifications
            .replies
            .entry(session_id.to_owned())
            .or_insert_with(|| QuickReply {
                editor: {
                    let mut editor = mj_chat::chat::ChatState::standby(
                        session_id,
                        &self.config,
                        Default::default(),
                        self.notices.clone(),
                    );
                    editor.set_prompt_history_project(
                        self.state
                            .sessions
                            .get(session_id)
                            .map(|session| session.bundle_id.clone()),
                    );
                    editor.request_session_history();
                    editor
                },
                pending: Vec::new(),
                error: None,
                recoverable: Vec::new(),
            })
    }

    pub(crate) fn handle_notification_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<DashboardAction> {
        if self.focus != Focus::Notifications {
            return None;
        }
        if matches!(
            key.code,
            crossterm::event::KeyCode::Tab | crossterm::event::KeyCode::BackTab
        ) {
            self.cycle_focus(
                key.code == crossterm::event::KeyCode::BackTab
                    || key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::SHIFT),
            );
            return Some(DashboardAction::None);
        }
        let session_id = self.displayed_notification_session_id()?;
        if self.notification_form_session().is_some() {
            self.record_event_handled();
            return Some(
                crate::actions::pane_command_for_key(key, self.focus)
                    .map_or(DashboardAction::None, |command| {
                        self.run_available_command(command)
                    }),
            );
        }
        let result = self
            .quick_reply_mut(&session_id)
            .editor
            .handle_single_line_prompt_key(crate::standby_prompt_key(key));
        match result.action {
            Some(mj_chat::chat::ChatAction::CycleFocus { reverse }) => {
                self.cycle_focus(reverse);
            }
            Some(mj_chat::chat::ChatAction::PasteFromClipboard) => {
                return Some(DashboardAction::PasteFromClipboard);
            }
            _ => {}
        }
        if !result.consumed
            && let Some(command) = crate::actions::pane_command_for_key(key, self.focus)
        {
            return Some(self.run_available_command(command));
        }
        self.record_event_handled();
        Some(DashboardAction::None)
    }

    pub(crate) fn paste_notification_reply(&mut self, text: &str) {
        if self.notification_form_session().is_some() {
            return;
        }
        if let Some(session_id) = self.displayed_notification_session_id() {
            let reply = self.quick_reply_mut(&session_id);
            reply.editor.paste_single_line_prompt(text);
        }
    }

    /// Pump detached editors through the same history owner as ActiveChat.
    pub fn service_notification_composers(
        &mut self,
        control: mj_client::session::SessionControl,
        mut persistence: impl FnMut(&str) -> Option<mj_chat::chat::ChatDaemonSender>,
    ) -> bool {
        let mut changed = false;
        for (session_id, reply) in &mut self.notifications.replies {
            changed |= reply.editor.has_composer_updates();
            reply.editor.drain_composer_updates();
            reply.editor.set_prompt_history_project(
                self.state
                    .sessions
                    .get(session_id)
                    .map(|session| session.bundle_id.clone()),
            );
            let bridge = persistence(session_id);
            let control = control.clone();
            let session_id = session_id.clone();
            reply.editor.dispatch_composer_requests(
                move || {
                    let control = control.clone();
                    let session_id = session_id.clone();
                    async move { control.session(session_id).await }
                },
                bridge.as_ref(),
            );
        }
        changed
    }

    pub fn report_notification_reply_failure(
        &mut self,
        session_id: &str,
        text: &str,
        error: &str,
        recovery: &str,
    ) {
        self.set_failure_notice(format!(
            "Quick reply to {}: {error}. {recovery}\nUnsent text:\n{text}",
            self.session_notice_name(session_id)
        ));
    }

    fn reveal_notification_reply_error(&mut self, session_id: &str) {
        if self.displayed_notification_session_id().as_deref() == Some(session_id) {
            self.notifications.scroll = 0;
        }
    }

    pub fn notification_reply_unconfirmed(
        &mut self,
        session_id: &str,
        command_id: &str,
        error: &str,
    ) -> bool {
        let Some(reply) = self
            .notifications
            .replies
            .get_mut(session_id)
            .filter(|reply| {
                reply
                    .pending
                    .iter()
                    .any(|pending| pending.command_id == command_id)
            })
        else {
            return false;
        };
        reply.error = Some(format!(
            "Could not confirm the reply: {error}; checking its status."
        ));
        let text = reply
            .pending
            .iter()
            .find(|pending| pending.command_id == command_id)
            .unwrap()
            .text
            .clone();
        self.reveal_notification_reply_error(session_id);
        self.report_notification_reply_failure(
            session_id,
            &text,
            error,
            "Admission is unconfirmed; checking its status. The text is kept in this notice.",
        );
        true
    }

    /// A durable prompt outcome belongs to its original session and command,
    /// even when another notification has replaced the displayed card.
    pub fn notification_reply_finished(
        &mut self,
        session_id: &str,
        command_id: &str,
        result: Result<(), String>,
    ) -> bool {
        let Some(reply) = self.notifications.replies.get_mut(session_id) else {
            return false;
        };
        let Some(index) = reply
            .pending
            .iter()
            .position(|pending| pending.command_id == command_id)
        else {
            return false;
        };
        let pending = reply.pending.remove(index);
        let failed = result.is_err();
        match result {
            Ok(()) => {
                if reply.editor.single_line_prompt_text() == pending.text {
                    reply.editor.clear_draft();
                }
                reply.editor.record_prompt_history(&pending.text);
                reply.editor.request_session_history();
                reply
                    .recoverable
                    .retain(|unsent| unsent.text != pending.text);
                reply.error = None;
            }
            Err(error) => {
                reply.keep_unsent(pending.text, Some(error));
                reply.error = None;
            }
        }
        if failed {
            self.reveal_notification_reply_error(session_id);
        }
        true
    }

    pub(crate) fn notification_action(&mut self, command: CommandId) -> DashboardAction {
        self.notification_gesture(command)
            .map_or(DashboardAction::None, |gesture| {
                self.activate_notification(gesture)
            })
    }

    pub(crate) fn notification_entries(&self) -> Vec<NotificationEntry> {
        let mut entries: Vec<_> = match &self.notification_cards {
            Some(cards) => cards
                .keys()
                .filter_map(|id| {
                    let card = cards.get_shared(id)?;
                    let level = match card.badge {
                        NotificationKind::NeedsInput => AttentionLevel::Waiting,
                        NotificationKind::Important => AttentionLevel::Important,
                    };
                    self.notification_entry(id, level, card.dismissal.clone(), Some(card))
                })
                .collect(),
            None => self
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
                    self.notification_entry(id, level, self.notification_dismissal_target(id), None)
                })
                .collect(),
        };
        entries.sort_by(|a, b| {
            let rank = |entry: &NotificationEntry| {
                self.notifications
                    .skipped
                    .iter()
                    .position(|card| entry.matches_skip(card))
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| match (&a.published, &b.published) {
                    (Some(a), Some(b)) => a.since_ms.cmp(&b.since_ms),
                    _ => (a.level == AttentionLevel::Important)
                        .cmp(&(b.level == AttentionLevel::Important))
                        .then_with(|| b.recency.cmp(&a.recency))
                        .then_with(|| a.title.cmp(&b.title)),
                })
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        entries
    }

    fn notification_entry(
        &self,
        id: &str,
        level: AttentionLevel,
        target: Option<NotificationDismissal>,
        published: Option<std::sync::Arc<mj_client::runtime_feed::NotificationCard>>,
    ) -> Option<NotificationEntry> {
        let session = self.state.sessions.get(id)?;
        Some(NotificationEntry {
            session_id: id.to_owned(),
            level,
            title: session.listed_title().to_owned(),
            project: session.project_name(&self.config),
            recency: published.as_ref().map_or_else(
                || {
                    self.session_details
                        .get(id)
                        .and_then(|detail| detail.last_activity_at_ms)
                        .filter(|millis| *millis > 0)
                        .or_else(|| {
                            crate::render::session_updated_at_epoch_seconds(session)
                                .map(|seconds| seconds.saturating_mul(1000))
                        })
                        .unwrap_or_default()
                },
                |card| u64::try_from(card.since_ms).unwrap_or_default(),
            ),
            target,
            published,
        })
    }

    fn notification_destination(&self, entry: &NotificationEntry) -> String {
        match &entry.published {
            Some(card) => card
                .holder
                .clone()
                .unwrap_or_else(|| entry.session_id.clone()),
            None => self.notification_content_session(&entry.session_id, entry.level),
        }
    }

    fn reset_notification_item(&mut self) {
        let pane = &mut self.notifications;
        pane.scroll = 0;
        pane.text_generation = pane.text_generation.wrapping_add(1);
        pane.text_rows = None;
        pane.form_text = None;
        pane.form_area = None;
        pane.message_read = None;
        pane.selection_revision = pane.selection_revision.wrapping_add(1);
    }

    pub(crate) fn sync_notification_selection(&mut self, entry: Option<&NotificationEntry>) {
        if match (entry, &self.notifications.scroll_card) {
            (Some(entry), Some(card)) => !entry.matches(card),
            (None, None) => false,
            _ => true,
        } {
            let retained = entry.and_then(|entry| self.retained_notification_item(entry));
            self.notifications.scroll_card = entry.map(NotificationEntry::identity);
            self.notifications.selected_item = retained
                .unwrap_or_else(|| entry.map_or(0, |entry| entry.item_count().saturating_sub(1)));
            if retained.is_none() {
                self.reset_notification_item();
            }
        }
    }

    pub(crate) fn can_navigate_notification(&self, command: CommandId) -> bool {
        self.notification_entries().first().is_some_and(|entry| {
            entry.published.is_some()
                && self
                    .notification_item_in_direction(entry, command)
                    .is_some()
        })
    }

    fn retained_notification_item(&self, entry: &NotificationEntry) -> Option<usize> {
        let previous = self.notifications.scroll_card.as_ref()?;
        entry.retained_item_index(previous, self.notifications.selected_item)
    }

    fn selected_notification_item(&self, entry: &NotificationEntry) -> usize {
        self.retained_notification_item(entry)
            .unwrap_or_else(|| entry.item_count().saturating_sub(1))
    }

    fn notification_item_in_direction(
        &self,
        entry: &NotificationEntry,
        command: CommandId,
    ) -> Option<usize> {
        let index = self.selected_notification_item(entry);
        match command {
            CommandId::NotificationPreviousMessage => index.checked_sub(1),
            CommandId::NotificationNextMessage => {
                Some(index + 1).filter(|next| *next < entry.item_count())
            }
            _ => None,
        }
    }

    /// The session whose content the displayed card needs, including a child
    /// form promoted to its parent's row.
    pub fn displayed_notification_session_id(&self) -> Option<String> {
        self.notification_entries()
            .first()
            .map(|entry| self.notification_destination(entry))
    }

    /// A card requests a warm chat; only that chat decides which form it shows.
    pub(crate) fn notification_form_content_session(&self) -> Option<String> {
        let entries = self.notification_entries();
        let entry = entries.first()?;
        let form_selected = entry.published.as_ref().is_some_and(|card| {
            card.form
                .as_ref()
                .is_some_and(|form| form.requires_elicitation())
                && self.selected_notification_item(entry) == card.entries.len()
        });
        (entry.level == AttentionLevel::Waiting && form_selected)
            .then(|| self.notification_destination(entry))
    }

    pub fn notification_form_session(&self) -> Option<String> {
        (self.notifications.size != PaneSize::Minimized)
            .then(|| self.notification_form_content_session())
            .flatten()
    }

    pub(crate) fn notification_presentation_changed(&self) -> bool {
        self.notifications.rendered_presentation
            != Some((
                self.notifications.selection_revision,
                self.notifications.size,
            ))
    }

    /// A notification-only attach reports in its card; a requested
    /// conversation's open keeps its shared opening/retry feedback.
    pub fn report_chat_attachment_result(
        &mut self,
        session: &str,
        error: Option<&str>,
        conversation_opening: bool,
    ) {
        self.set_notification_form_error(session, error.map(str::to_owned));
        if conversation_opening {
            if let Some(error) = error {
                self.report_open_failure(session, error);
            } else {
                self.clear_notice();
            }
        }
    }

    pub fn set_notification_form_error(&mut self, session: &str, error: Option<String>) {
        if let Some(error) = error {
            self.notifications
                .form_errors
                .insert(session.to_owned(), error);
        } else {
            self.notifications.form_errors.remove(session);
        }
    }

    /// Input batching refreshes newly selected form content before any next event.
    pub fn notification_input_needs_frame(&self) -> bool {
        self.notification_form_session().is_some() && self.notification_presentation_changed()
    }

    pub fn notification_form_owns_keyboard(&self) -> bool {
        self.focus == Focus::Notifications
            && self.notifications.form_area.is_some()
            && self
                .notification_form_session()
                .is_some_and(|session| self.chat_input_available(&session))
    }

    /// Pointer capture remains with its owning conversation across sessions.
    pub fn conversation_pointer_session(
        &self,
        chats: &std::collections::BTreeMap<String, mj_chat::chat::ActiveChat>,
    ) -> Option<String> {
        self.pane_sessions().into_iter().find_map(|(_, session)| {
            if !self.chat_input_available(&session) {
                return None;
            }
            chats
                .get(&session)?
                .conversation_pointer_captured()
                .then_some(session)
        })
    }

    /// Some(false) is message/chrome in this presentation: another
    /// conversation's geometry must not claim text that is drawn here.
    pub fn notification_form_mouse_ownership(
        &self,
        chats: &std::collections::BTreeMap<String, mj_chat::chat::ActiveChat>,
        mouse: crossterm::event::MouseEvent,
    ) -> Option<bool> {
        if self.modal_open() || self.component_pointer_captured() {
            return None;
        }
        if self.conversation_pointer_session(chats).is_some() {
            return None;
        }
        let area = self.notifications.form_area?;
        let session = self.notification_form_session()?;
        let chat = chats.get(&session)?;
        if !chat.has_elicitation() {
            return None;
        }
        match chat.elicitation_pointer_surface() {
            Some(mj_chat::chat::ElicitationSurface::Notification) => Some(true),
            Some(mj_chat::chat::ElicitationSurface::Conversation) => None,
            None if area.contains(ratatui::layout::Position::new(mouse.column, mouse.row)) => {
                Some(chat.elicitation_component_handles_mouse_in(
                    mj_chat::chat::ElicitationSurface::Notification,
                    mouse,
                ))
            }
            None => None,
        }
    }

    /// Apply notification input to the owning chat's ordinary event dispatcher.
    pub fn handle_notification_form_event(
        &mut self,
        chats: &mut std::collections::BTreeMap<String, mj_chat::chat::ActiveChat>,
        event: &crossterm::event::Event,
    ) -> Option<mj_chat::components::EventResult<mj_chat::chat::ChatEventOutcome>> {
        if self.modal_open()
            || (matches!(event, crossterm::event::Event::Mouse(_))
                && self.component_pointer_captured())
        {
            return None;
        }
        if matches!(event, crossterm::event::Event::Mouse(_))
            && self.conversation_pointer_session(chats).is_some()
        {
            return None;
        }
        let session = self.notification_form_session()?;
        if !self.chat_input_available(&session) {
            if let Some(chat) = chats.get_mut(&session) {
                chat.cancel_component_pointer();
            }
            return None;
        }
        let chat = chats.get_mut(&session)?;
        // The live composer owns input, exactly as for the conversation pane:
        // a parked composer's replacement attach would lose any result started here.
        if !chat.has_live_composer() {
            chat.cancel_component_pointer();
            return None;
        }
        if !chat.has_elicitation() {
            return None;
        }
        let routed = if matches!(event, crossterm::event::Event::Mouse(_)) {
            match chat.elicitation_pointer_surface() {
                Some(mj_chat::chat::ElicitationSurface::Notification) => true,
                Some(mj_chat::chat::ElicitationSurface::Conversation) => false,
                None => self.notification_form_handles_event(event),
            }
        } else {
            self.notification_form_handles_event(event)
        };
        routed.then(|| chat.handle_elicitation_event(event.clone()))
    }

    /// Route only the form body here; the host still owns card buttons and menus.
    fn notification_form_handles_event(&mut self, event: &crossterm::event::Event) -> bool {
        if self.modal_open() || self.notifications.form_area.is_none() {
            return false;
        }
        match event {
            crossterm::event::Event::Mouse(mouse) => {
                let inside = self
                    .notifications
                    .form_area
                    .unwrap()
                    .contains(ratatui::layout::Position::new(mouse.column, mouse.row));
                if inside
                    && mouse.kind
                        == crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        )
                {
                    self.begin_pointer_press();
                    self.focus = Focus::Notifications;
                }
                inside
            }
            crossterm::event::Event::Key(key)
                if crate::actions::pane_command_for_key(*key, Focus::Notifications)
                    .is_some_and(|command| {
                        matches!(
                            command,
                            CommandId::NotificationOpen
                                | CommandId::NotificationClear
                                | CommandId::NotificationSkip
                                | CommandId::NotificationClearImportant
                                | CommandId::NotificationPreviousMessage
                                | CommandId::NotificationNextMessage
                        )
                    }) =>
            {
                false
            }
            crossterm::event::Event::Key(_) | crossterm::event::Event::Paste(_) => {
                self.focus == Focus::Notifications
            }
            _ => false,
        }
    }

    /// Keep the existing child-family runtime subscription while its approval
    /// is displayed, even when the family is outside the active workspace.
    pub fn displayed_notification_parent_id(&self) -> Option<String> {
        self.notification_entries()
            .first()
            .filter(|entry| self.notification_destination(entry) != entry.session_id)
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

    fn loaded_notification_text(&self, session_id: &str, stable_id: &str) -> Option<&str> {
        self.session_details
            .get(session_id)?
            .projection
            .agent_message_text(stable_id)
    }

    fn notification_read_request(
        &self,
        entry: &NotificationEntry,
    ) -> Option<NotificationReadRequest> {
        let (session_id, source) = if let Some(card) = &entry.published {
            let item = card.entries.get(self.selected_notification_item(entry))?;
            let source = match &item.message {
                EntryMessage::Found(id) => NotificationReadSource::Item(id.clone()),
                EntryMessage::Locating | EntryMessage::None => {
                    NotificationReadSource::Through(item.completed_ordinal)
                }
            };
            (entry.session_id.clone(), source)
        } else {
            if entry.level != AttentionLevel::Waiting {
                return None;
            }
            let session_id = self.notification_destination(entry);
            let detail = self.session_details.get(&session_id)?;
            if !detail.pending_elicitations.is_empty() {
                return None;
            }
            let id = detail.question_message.clone()?;
            (session_id, NotificationReadSource::Item(id))
        };
        if let NotificationReadSource::Item(id) = &source
            && self.loaded_notification_text(&session_id, id).is_some()
        {
            return None;
        }
        Some(NotificationReadRequest {
            session_id,
            source,
            selection_revision: self.notifications.selection_revision,
        })
    }

    pub fn wanted_notification_message(&mut self) -> Option<NotificationReadRequest> {
        let entries = self.notification_entries();
        self.sync_notification_selection(entries.first());
        self.notification_read_request(entries.first()?)
    }

    pub fn sync_notification_message_read(
        &mut self,
        wanted: Option<NotificationReadRequest>,
        loading: bool,
    ) {
        let Some(request) = wanted else {
            self.notifications.message_read = None;
            return;
        };
        if self
            .notifications
            .message_read
            .as_ref()
            .is_none_or(|read| read.request != request)
        {
            self.notifications.message_read = Some(NotificationMessageRead {
                request,
                status: crate::ingest::TranscriptHydration::Unavailable,
                text: None,
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
        request: &NotificationReadRequest,
        text: Option<String>,
    ) -> bool {
        if self.wanted_notification_message().as_ref() != Some(request) {
            return false;
        }
        if let Some(read) = self
            .notifications
            .message_read
            .as_mut()
            .filter(|read| &read.request == request)
        {
            read.status = crate::ingest::TranscriptHydration::Ready;
            read.text = text;
            return true;
        }
        false
    }

    pub(crate) fn selected_notification_text(&self, entry: &NotificationEntry) -> String {
        let Some(card) = &entry.published else {
            return self.notification_card_text(&entry.session_id, entry.level);
        };
        let index = self.selected_notification_item(entry);
        let Some(item) = card.entries.get(index) else {
            return card
                .form
                .as_ref()
                .map(|form| form.message.clone())
                .or_else(|| {
                    self.session_review(&entry.session_id)
                        .and_then(|review| review.activity_label())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| notification_message_placeholder(card.badge).to_owned());
        };
        if let EntryMessage::Found(id) = &item.message
            && let Some(text) = self.loaded_notification_text(&entry.session_id, id)
        {
            return text.to_owned();
        }
        let Some(wanted) = self.notification_read_request(entry) else {
            return notification_message_placeholder(item.kind).to_owned();
        };
        let read = self
            .notifications
            .message_read
            .as_ref()
            .filter(|read| read.request == wanted);
        if read.is_some_and(|read| read.status == crate::ingest::TranscriptHydration::Loading) {
            return "loading…".to_owned();
        }
        read.and_then(|read| read.text.clone())
            .unwrap_or_else(|| match wanted.source {
                NotificationReadSource::Item(_) => "message not available".to_owned(),
                NotificationReadSource::Through(_) => {
                    notification_message_placeholder(item.kind).to_owned()
                }
            })
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
        let session_id = self.notification_destination(entry);
        let text = self
            .notifications
            .replies
            .get(&session_id)
            .map(|reply| reply.editor.single_line_prompt_text().to_owned())
            .unwrap_or_default();
        let restore_text = (command == CommandId::NotificationRestore)
            .then(|| {
                self.notifications
                    .replies
                    .get(&session_id)
                    .and_then(|reply| reply.restore_candidate().map(str::to_owned))
            })
            .flatten();
        NotificationGesture {
            command,
            identity: entry.identity(),
            selection_revision: self.notifications.selection_revision,
            session_id,
            text,
            restore_text,
        }
    }

    pub(crate) fn activate_notification(
        &mut self,
        gesture: NotificationGesture,
    ) -> DashboardAction {
        let NotificationGesture {
            command,
            identity,
            selection_revision,
            session_id,
            text,
            restore_text,
        } = gesture;
        let card = &identity.action;
        if let Some(restore) = restore_text {
            let reply = self.quick_reply_mut(&session_id);
            // The gesture carries the captured recovery. Preserve the current
            // draft too, including any edits made after this button was drawn.
            let previous = reply.editor.set_single_line_prompt_text(restore.clone());
            reply.keep_unsent(previous, None);
            // Rotate the restored entry to the back so repeated restoration
            // reaches every retained reply, rather than alternating two drafts.
            if let Some(index) = reply
                .recoverable
                .iter()
                .position(|unsent| unsent.text == restore)
            {
                let restored = reply.recoverable.remove(index);
                reply.recoverable.push(restored);
            }
            return DashboardAction::None;
        }
        match command {
            CommandId::NotificationOpen | CommandId::NotificationDefault
                if command == CommandId::NotificationOpen || text.trim().is_empty() =>
            {
                let action = self.focus_session_from_cpu_report(&session_id);
                self.focus = Focus::Prompt;
                action
            }
            CommandId::NotificationRestore => DashboardAction::None,
            CommandId::NotificationSend | CommandId::NotificationDefault => {
                if text.trim().is_empty()
                    || self
                        .quick_reply_mut(&session_id)
                        .pending
                        .iter()
                        .any(|pending| pending.text == text)
                {
                    return DashboardAction::None;
                }
                let command_id = match mj_client::session::new_command_id("notification-reply") {
                    Ok(id) => id,
                    Err(error) => {
                        self.quick_reply_mut(&session_id).keep_unsent(
                            text.clone(),
                            Some(format!("Could not send reply: {error:#}")),
                        );
                        self.reveal_notification_reply_error(&session_id);
                        return DashboardAction::None;
                    }
                };
                self.quick_reply_mut(&session_id)
                    .pending
                    .push(ReplySubmission {
                        card: identity.clone(),
                        command_id: command_id.clone(),
                        text: text.clone(),
                    });
                self.quick_reply_mut(&session_id).error = None;
                DashboardAction::NotificationReply {
                    card: card.clone(),
                    session_id,
                    prompt: mj_chat::chat::StartupPrompt {
                        command_id,
                        payload: mj_chat::chat::PromptPayload::text(text),
                        move_operation_id: None,
                    },
                }
            }
            CommandId::NotificationClear => {
                let Some(target) = card.target.clone() else {
                    return DashboardAction::None;
                };
                if self.notification_dismissal_pending(&card.session_id) {
                    return DashboardAction::None;
                }
                self.submit_notification_target(&card.session_id, target.clone());
                DashboardAction::DismissNotification {
                    session_id: card.session_id.clone(),
                    target,
                }
            }
            CommandId::NotificationSkip => {
                let entries = self.notification_entries();
                let skipped = identity.skip_identity();
                self.notifications.skipped.retain(|previous| {
                    previous != &skipped && entries.iter().any(|entry| entry.matches_skip(previous))
                });
                if !matches!(&skipped, SkippedCard::Published(_))
                    || entries.iter().any(|entry| entry.matches_skip(&skipped))
                {
                    self.notifications.skipped.push(skipped);
                }
                let card = card.clone();
                self.sync_notification_selection(self.notification_entries().first());
                self.notifications.scroll = 0;
                DashboardAction::NotificationSkip { card }
            }
            CommandId::NotificationPreviousMessage | CommandId::NotificationNextMessage => {
                if let Some(entry) = self.notification_entries().first()
                    && entry.matches(&identity)
                    && selection_revision == self.notifications.selection_revision
                {
                    self.sync_notification_selection(Some(entry));
                    if let Some(index) = self.notification_item_in_direction(entry, command) {
                        self.notifications.selected_item = index;
                        self.reset_notification_item();
                    }
                }
                DashboardAction::None
            }
            _ => DashboardAction::None,
        }
    }

    pub(crate) fn notification_skip(&mut self) -> DashboardAction {
        self.notification_gesture(CommandId::NotificationSkip)
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
                let target = entry.target?;
                if self.notification_dismissal_pending(&entry.session_id) {
                    return None;
                }
                self.submit_notification_target(&entry.session_id, target.clone());
                Some((entry.session_id, target))
            })
            .collect();
        DashboardAction::DismissNotifications { dismissals }
    }

    pub fn notification_has_recoverable_reply(&self) -> bool {
        self.displayed_notification_session_id()
            .and_then(|id| self.notifications.replies.get(&id))
            .is_some_and(|reply| reply.restore_candidate().is_some())
    }

    pub fn has_notification_for_session(&self, session_id: &str) -> bool {
        self.notification_entries()
            .iter()
            .any(|entry| self.notification_destination(entry) == session_id)
    }

    /// Text copying uses the exact row space registered by the last render.
    pub fn notification_selection_is_current(&self, generation: u64) -> bool {
        self.notifications.text_generation == generation
            && self
                .frame_surfaces
                .surface(mj_chat::selection::SurfaceId::NotificationText(generation))
                .is_some()
    }

    pub fn notification_selection_text(
        &self,
        generation: u64,
        range: &mj_chat::selection::SelectionRange,
    ) -> Option<String> {
        self.notification_selection_is_current(generation)
            .then(|| {
                self.notifications
                    .text_rows
                    .as_ref()
                    .map(|rows| rows.selection_text(range))
            })
            .flatten()
    }

    /// Selection edge scrolling uses the pane's existing scroll offset.
    pub fn autoscroll_notification_selection(&mut self, direction: i8) {
        self.scroll_notification(i16::from(direction));
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
    mut chat: Option<&mut mj_chat::chat::ActiveChat>,
) {
    dashboard.notifications.form_area = None;
    dashboard.notifications.rendered_presentation = Some((
        dashboard.notifications.selection_revision,
        dashboard.notifications.size,
    ));
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
    let mut block = theme::panel(focused)
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
    if !minimized
        && chat.as_ref().is_some_and(|chat| chat.has_elicitation())
        && let Some(error) = entries
            .first()
            .and_then(|entry| dashboard.notifications.errors.get(&entry.session_id))
    {
        block = block.title_bottom(Line::styled(
            format!(" Could not clear: {error} "),
            ratatui::style::Style::default().fg(theme::palette().error),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    dashboard
        .pane_size_control_areas
        .extend(crate::render::sessions::pane_size_control_areas(
            area,
            SupportPane::Notifications,
            dashboard.notifications.maximize_enabled,
        ));
    dashboard.notifications.controls.clear();
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
    dashboard.notifications.controls = [
        CommandId::NotificationOpen,
        CommandId::NotificationSend,
        CommandId::NotificationRestore,
        CommandId::NotificationClear,
        CommandId::NotificationSkip,
        CommandId::NotificationPreviousMessage,
        CommandId::NotificationNextMessage,
    ]
    .into_iter()
    .map(|command| dashboard.gesture_for_notification(entry, command))
    .collect();
    let header = Line::raw(format!(
        "{glyph} {} · {} · {age}",
        entry.title, entry.project
    ));
    let header_width =
        render_message_navigation(frame, inner, dashboard, entry, focused, header.width());
    frame.render_widget(
        Paragraph::new(header).style(
            ratatui::style::Style::default()
                .fg(theme::palette().secondary)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Rect::new(inner.x, inner.y, header_width, 1),
    );
    let form_requested = dashboard.notification_form_session().is_some();
    let form_open = chat
        .as_ref()
        .is_some_and(|chat| chat.elicitation_height(inner.width).is_some());
    // The form's panel needs its two borders and one interior row inside the
    // card body (the card's header and bottom margin take two rows).
    let form_fits = inner.height.saturating_sub(2) >= 3;
    let inline_form = form_requested && form_open && form_fits;
    let session_id = dashboard.notification_destination(entry);
    let reply_error = dashboard
        .notifications
        .replies
        .get(&session_id)
        .map(|reply| {
            let mut messages = Vec::new();
            if let Some(error) = &reply.error {
                messages.push(error.clone());
            }
            for unsent in &reply.recoverable {
                let text = unsent.text.replace('\n', "⏎");
                let restore = if unsent.text == reply.editor.single_line_prompt_text() {
                    "Enter sends this text"
                } else {
                    "Alt+Enter restores; Enter sends restored text"
                };
                messages.push(match &unsent.error {
                    Some(error) => format!("Not sent: {text} — {error}; {restore}."),
                    None => format!("Saved draft: {text}; {restore}."),
                });
            }
            messages.join("\n")
        })
        .filter(|message| !message.is_empty());
    let mut text = match dashboard.notifications.errors.get(&entry.session_id) {
        Some(error) => format!("Could not clear: {error}\n{text}"),
        None => text,
    };
    if let Some(error) = reply_error {
        text = format!("{error}\n{text}");
    }
    if form_requested && !inline_form {
        if let Some(error) = dashboard.notifications.form_errors.get(&session_id) {
            text = format!(
                "Could not load form: {error}. Use Go to Session to answer it there.\n{text}"
            );
        } else if chat.is_none() && dashboard.session_failed(&session_id) {
            // No attach will run for a failed session, so no attach error
            // will arrive either; say why instead of loading forever.
            let reason = dashboard.session_failure_text(&session_id);
            text = format!("Could not load form: {reason}\n{text}");
        } else if chat.is_none() && dashboard.pane_session_is_suspended(&session_id) {
            text = format!("Could not load form: the session is suspended.\n{text}");
        } else if form_open {
            // The question comes first: it is what a cramped card must keep.
            text = format!("{text}\nEnlarge this pane to answer here, or use Go to Session.");
        } else if chat.is_none() {
            text = format!("Loading form…\n{text}");
        } else {
            text = format!("Waiting for the session's form update…\n{text}");
        }
    }
    // Header, top text margin, bottom margin, blank separator, reply and buttons.
    let body = Rect::new(
        inner.x + 1,
        inner.y + if form_requested { 1 } else { 2 },
        inner.width.saturating_sub(2),
        inner
            .height
            .saturating_sub(if form_requested { 2 } else { 6 }),
    );
    if inline_form {
        dashboard.notifications.text_rows = None;
        if !dashboard.chat_input_available(&session_id) {
            chat.as_mut().unwrap().cancel_component_pointer();
        }
        let surfaces = chat.as_mut().unwrap().draw_elicitation_in(
            frame,
            body,
            focused && !dashboard.modal_open(),
        );
        let form_text = chat
            .as_ref()
            .unwrap()
            .elicitation_message_text()
            .map(|text| {
                let width = surfaces
                    .surface(mj_chat::selection::SurfaceId::ElicitationMessage)
                    .map_or(0, |surface| surface.rect.width);
                (text, width)
            });
        if dashboard.notifications.form_text != form_text {
            dashboard.notifications.text_generation =
                dashboard.notifications.text_generation.wrapping_add(1);
            dashboard.notifications.form_text = form_text;
        }
        // Claim input exactly when handle_notification_form_event accepts it.
        dashboard.notifications.form_area = (dashboard.chat_input_available(&session_id)
            && chat.as_ref().unwrap().has_live_composer()
            && chat
                .as_ref()
                .unwrap()
                .elicitation_input_available_in(mj_chat::chat::ElicitationSurface::Notification))
        .then_some(body);
        for (source, target) in [
            (
                mj_chat::selection::SurfaceId::ElicitationMessage,
                mj_chat::selection::SurfaceId::NotificationText(
                    dashboard.notifications.text_generation,
                ),
            ),
            (
                mj_chat::selection::SurfaceId::ModalBody,
                mj_chat::selection::SurfaceId::NotificationFormBody(
                    dashboard.notifications.text_generation,
                ),
            ),
        ] {
            if let Some(mut surface) = surfaces.surface(source).copied() {
                surface.id = target;
                dashboard.frame_surfaces.push(surface);
            }
        }
    } else {
        let pane = &mut dashboard.notifications;
        pane.form_text = None;
        if pane
            .text_rows
            .as_ref()
            .is_none_or(|rows| !rows.matches(&text, body.width))
        {
            pane.text_generation = pane.text_generation.wrapping_add(1);
            pane.text_rows = Some(mj_chat::selection::SelectableText::new(text, body.width));
        }
        let rows = pane.text_rows.as_ref().unwrap().rows();
        let max_scroll = rows
            .len()
            .saturating_sub(body.height as usize)
            .min(u16::MAX as usize) as u16;
        pane.scroll = pane.scroll.min(max_scroll);
        if body.height > 0 {
            frame.render_widget(Paragraph::new(rows.to_vec()).scroll((pane.scroll, 0)), body);
            dashboard
                .frame_surfaces
                .push(mj_chat::selection::SurfaceFrame::scrollable(
                    mj_chat::selection::SurfaceId::NotificationText(pane.text_generation),
                    body,
                    pane.scroll as usize,
                    rows.len(),
                ));
        }
    }
    if inner.height < 3 {
        return;
    }
    if !form_requested {
        let current_card = entry.identity();
        let reply = dashboard.quick_reply_mut(&session_id);
        let restoring = reply.restore_candidate().is_some();
        let pending = reply
            .pending
            .iter()
            .find(|pending| pending.text == reply.editor.single_line_prompt_text());
        let send_label = match pending {
            Some(pending) if pending.card != current_card => " Sending earlier reply… ",
            Some(_) => " Sending… ",
            None if !reply.pending.is_empty() => " Send (reply in flight) ",
            None => " Send ",
        };
        let can_send =
            pending.is_none() && !reply.editor.single_line_prompt_text().trim().is_empty();
        let send_width = (Line::raw(send_label).width() as u16).min(inner.width);
        let reply_y = inner.bottom() - 2;
        let send_area = Rect::new(
            inner.right().saturating_sub(send_width),
            reply_y,
            send_width,
            1,
        );
        let restore_label = " Restore unsent ";
        let restore_width = if restoring {
            (Line::raw(restore_label).width() as u16).min(send_area.x.saturating_sub(inner.x))
        } else {
            0
        };
        let restore_area = Rect::new(
            send_area.x.saturating_sub(restore_width),
            reply_y,
            restore_width,
            1,
        );
        if restoring && restore_width > 0 {
            dashboard.surface_form.borrow_mut().register(
                SurfaceControl::Notification(CommandId::NotificationRestore),
                ControlKind::Button,
                restore_area,
                true,
            );
            frame.render_widget(
                Paragraph::new(restore_label).style(theme::actionable().patch(theme::raised())),
                restore_area,
            );
        }
        frame.render_widget(
            Paragraph::new("Quick reply: ").style(theme::muted()),
            Rect::new(inner.x, reply_y, 13.min(inner.width), 1),
        );
        let input = Rect::new(
            inner.x.saturating_add(13).min(restore_area.x),
            reply_y,
            restore_area.x.saturating_sub(inner.x.saturating_add(14)),
            1,
        );
        let input_focused = focused && !dashboard.modal_open();
        dashboard
            .quick_reply_mut(&session_id)
            .editor
            .draw_single_line_prompt(frame, input, input_focused);
        dashboard.surface_form.borrow_mut().register(
            SurfaceControl::Notification(CommandId::NotificationSend),
            ControlKind::Button,
            send_area,
            can_send,
        );
        frame.render_widget(
            Paragraph::new(send_label).style(if can_send {
                theme::actionable().patch(theme::raised())
            } else {
                theme::muted()
            }),
            send_area,
        );
    }
    let important = entries
        .iter()
        .filter(|entry| {
            entry.level == AttentionLevel::Important
                && entry.target.is_some()
                && !dashboard.notification_dismissal_pending(&entry.session_id)
        })
        .count();
    let mut x = inner.x;
    let mut form = dashboard.surface_form.borrow_mut();
    for (id, label, enabled) in [
        (
            CommandId::NotificationOpen,
            "Go to Session".to_owned(),
            true,
        ),
        (
            CommandId::NotificationClear,
            "Clear & Next".to_owned(),
            entry.target.is_some() && !dashboard.notification_dismissal_pending(&entry.session_id),
        ),
        (CommandId::NotificationSkip, "Skip".to_owned(), true),
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

fn render_message_navigation(
    frame: &mut Frame,
    inner: Rect,
    dashboard: &DashboardState,
    entry: &NotificationEntry,
    focused: bool,
    header_width: usize,
) -> u16 {
    if entry.published.is_none() || entry.item_count() == 0 {
        return inner.width;
    }
    let index = dashboard.selected_notification_item(entry);
    let position = format!("{} of {}", index + 1, entry.item_count());
    let mut right = inner.right();
    let mut form = dashboard.surface_form.borrow_mut();
    let full_labels = [" Next Alt+PgDn ", " Previous Alt+PgUp "];
    let full_width =
        full_labels.iter().map(|label| label.len()).sum::<usize>() + position.len() + 1;
    let labels = if focused && usize::from(inner.width) >= header_width + full_width {
        full_labels
    } else {
        [" Next ", " Previous "]
    };
    for (command, label) in [
        CommandId::NotificationNextMessage,
        CommandId::NotificationPreviousMessage,
    ]
    .into_iter()
    .zip(labels)
    {
        let width = label.len() as u16;
        if right.saturating_sub(inner.x) < width + position.len() as u16 + 1 {
            continue;
        }
        right -= width;
        let area = Rect::new(right, inner.y, width, 1);
        let enabled = dashboard
            .notification_item_in_direction(entry, command)
            .is_some();
        form.register(
            SurfaceControl::Notification(command),
            ControlKind::Button,
            area,
            enabled,
        );
        let style = if enabled {
            theme::actionable().patch(theme::raised())
        } else {
            theme::muted()
        };
        frame.render_widget(Paragraph::new(label).style(style), area);
    }
    let width = (position.len() as u16).min(right - inner.x);
    right -= width;
    frame.render_widget(
        Paragraph::new(position).style(theme::muted()),
        Rect::new(right, inner.y, width, 1),
    );
    (right - inner.x).saturating_sub(1)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::*;
    use crossterm::event::KeyCode;
    use mj_core::attention::SessionAttention;

    use mj_client::session::{
        BoxFuture, ManagedSessionView, PendingRelaySubmit, PendingRelaySync, ReviewerAction,
        ReviewerOutcome, SessionControl, SessionControlBackend, SessionHandle,
        SessionHandleBackend,
    };
    use mj_core::{
        elicitation::ElicitationResponse,
        relay::RelayCommand,
        storage::{HistoryCursor, HistoryScope, PromptHistoryEntry, PromptHistoryPage},
    };

    #[derive(Clone)]
    struct HistoryBackend {
        inner: SessionHandle,
        form_view: Option<ManagedSessionView>,
        responses: Option<tokio::sync::mpsc::UnboundedSender<(String, ElicitationResponse)>>,
        response_error: Option<String>,
        listings: tokio::sync::mpsc::UnboundedSender<String>,
        requests: tokio::sync::mpsc::UnboundedSender<(
            String,
            String,
            tokio::sync::oneshot::Sender<PromptHistoryPage>,
        )>,
    }
    impl SessionControlBackend for HistoryBackend {
        fn session(&self, session_id: String) -> BoxFuture<'_, anyhow::Result<SessionHandle>> {
            let mut backend = self.clone();
            backend.inner =
                mj_client::session::replacement_session_test_fixture(&session_id, 1).stopped;
            let handle = SessionHandle::new(backend);
            Box::pin(async move { Ok(handle) })
        }
    }
    impl SessionHandleBackend for HistoryBackend {
        fn search_prompts(
            &self,
            project: String,
            _: HistoryScope,
            query: String,
            _: Option<HistoryCursor>,
            _: usize,
        ) -> BoxFuture<'_, anyhow::Result<PromptHistoryPage>> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.requests.send((project, query, tx)).unwrap();
            Box::pin(async move { Ok(rx.await?) })
        }
        fn session_prompts(&self) -> BoxFuture<'_, anyhow::Result<Vec<PromptHistoryEntry>>> {
            if self.form_view.is_some() {
                return Box::pin(async { Ok(Vec::new()) });
            }
            self.listings.send(self.session_id().to_owned()).unwrap();
            let session_id = self.session_id().to_owned();
            Box::pin(async move {
                Ok(vec![PromptHistoryEntry {
                    id: 1,
                    session_id,
                    text: "stored\nprompt 世界".into(),
                }])
            })
        }
        fn config_result(
            &self,
            _: String,
        ) -> BoxFuture<'_, anyhow::Result<Option<Option<String>>>> {
            Box::pin(async { Ok(None) })
        }
        fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
            Box::new(self.clone())
        }
        fn session_id(&self) -> &str {
            self.inner.session_id()
        }
        fn view(&self) -> ManagedSessionView {
            self.form_view.clone().unwrap_or_else(|| self.inner.view())
        }
        fn is_stopped(&self) -> bool {
            self.form_view.is_none() && self.inner.is_stopped()
        }
        fn has_changed(&self) -> anyhow::Result<bool> {
            self.inner.has_changed()
        }
        fn changed(&mut self) -> BoxFuture<'_, anyhow::Result<ManagedSessionView>> {
            Box::pin(async { std::future::pending().await })
        }
        fn enqueue_submit(
            &self,
            _: String,
            _: RelayCommand,
        ) -> BoxFuture<'_, anyhow::Result<PendingRelaySubmit>> {
            Box::pin(async { anyhow::bail!("unexpected submit") })
        }
        fn enqueue_sync(&self) -> BoxFuture<'_, anyhow::Result<PendingRelaySync>> {
            Box::pin(async { anyhow::bail!("unexpected sync") })
        }
        fn respond_elicitation(
            &self,
            id: String,
            response: ElicitationResponse,
        ) -> BoxFuture<'_, anyhow::Result<()>> {
            let sent = self
                .responses
                .as_ref()
                .expect("form response observer")
                .send((id, response));
            let error = self.response_error.clone();
            Box::pin(async move {
                sent?;
                if let Some(error) = error {
                    anyhow::bail!(error);
                }
                Ok(())
            })
        }
        fn stop_background_task(&self, _: String) -> BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(async { anyhow::bail!("unexpected stop") })
        }
        fn reviewer(
            &self,
            _: Option<String>,
            _: ReviewerAction,
        ) -> BoxFuture<'_, anyhow::Result<ReviewerOutcome>> {
            Box::pin(async { anyhow::bail!("unexpected review") })
        }
    }

    pub(crate) fn form_chat(
        session: &str,
        request: mj_core::elicitation::ElicitationRequest,
    ) -> (
        mj_chat::chat::ActiveChat,
        tokio::sync::mpsc::UnboundedReceiver<(String, ElicitationResponse)>,
    ) {
        form_chat_with_error(session, request, None)
    }

    fn form_chat_with_error(
        session: &str,
        request: mj_core::elicitation::ElicitationRequest,
        response_error: Option<String>,
    ) -> (
        mj_chat::chat::ActiveChat,
        tokio::sync::mpsc::UnboundedReceiver<(String, ElicitationResponse)>,
    ) {
        let fixture = mj_client::session::replacement_session_test_fixture(session, 1);
        let (responses, answers) = tokio::sync::mpsc::unbounded_channel();
        let (listings, _) = tokio::sync::mpsc::unbounded_channel();
        let (requests, _) = tokio::sync::mpsc::unbounded_channel();
        let mut materialized = mj_core::state::MaterializedSession::empty(session);
        materialized.pending_elicitations = vec![request];
        let view = ManagedSessionView {
            connected: true,
            snapshot: Some(mj_core::state::ManagedSessionSnapshot {
                window: Default::default(),
                materialized,
                operational: mj_core::relay::RelaySnapshot::new(session.to_owned())
                    .operational_state()
                    .into(),
                latest_credential_sync_signal: None,
                worker_build: None,
                subagent_requests: Default::default(),
                subagent_completions: Default::default(),
                subagent_queue_error: None,
            }),
            ..Default::default()
        };
        let backend = HistoryBackend {
            inner: fixture.stopped,
            form_view: Some(view),
            responses: Some(responses),
            response_error,
            listings,
            requests,
        };
        let chat = mj_chat::chat::ActiveChat::open(
            SessionHandle::new(backend.clone()),
            "test-bundle",
            None,
            SessionControl::new(backend),
            Default::default(),
            String::new(),
            Default::default(),
        );
        (chat, answers)
    }

    fn alt(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::ALT)
    }

    // Hard-won: 4c23909e0: selection consumed inline checkbox presses, and a size warning retained invisible form input geometry.
    #[tokio::test]
    async fn inline_form_controls_precede_selection_and_hidden_frames_release_input() {
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEventKind};
        for conversation_visible in [false, true] {
            let mut dashboard = dashboard_with_session(running_session());
            serve_attention(&mut dashboard, &["session-1"], SessionAttention::NeedsInput);
            let mut request = question("session-1");
            request.fields[0].kind = mj_core::elicitation::ElicitationFieldKind::Boolean {
                default: Some(false),
            };
            dashboard
                .session_details
                .get_mut("session-1")
                .unwrap()
                .pending_elicitations = vec![request.clone()];
            crate::test_support::serve_form_card(&mut dashboard, "session-1", None, &request);
            dashboard.set_current_session(if conversation_visible {
                Some("session-1")
            } else {
                None
            });
            dashboard.focus = Focus::Notifications;
            dashboard.notifications.size = PaneSize::Maximized;
            let (chat, mut answers) = form_chat("session-1", request);
            let mut chats = std::collections::BTreeMap::from([("session-1".into(), chat)]);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 44)).unwrap();
            terminal
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            let lines = buffer_lines(terminal.backend().buffer());
            let card = dashboard.notifications_area.unwrap();
            let (column, offset) = point(
                &lines[usize::from(card.y)..usize::from(card.bottom())],
                mj_chat::components::Checkbox::marker(false),
            );
            let mut selection = mj_chat::selection::SelectionState::new();
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                let mouse = crossterm::event::MouseEvent {
                    kind,
                    column,
                    row: card.y + offset,
                    modifiers: KeyModifiers::NONE,
                };
                let owns = dashboard
                    .notification_form_mouse_ownership(&chats, mouse)
                    .unwrap();
                if !owns && matches!(kind, MouseEventKind::Down(_)) {
                    selection.on_mouse_down(column, card.y + offset, dashboard.frame_surfaces());
                }
                assert!(owns);
                assert!(selection.active_surface().is_none());
                assert!(
                    dashboard
                        .handle_notification_form_event(&mut chats, &Event::Mouse(mouse))
                        .unwrap()
                        .consumed
                );
            }
            // Hard-won: 9739e9996: lifecycle-owned chats still accepted inline answers before redraw.
            dashboard.begin_session_operation(
                "session-1".into(),
                crate::SessionOperationKind::Moving,
                None,
            );
            for code in [KeyCode::Enter, KeyCode::Esc] {
                assert!(
                    dashboard
                        .handle_notification_form_event(&mut chats, &Event::Key(key(code)))
                        .is_none()
                );
            }
            terminal
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            assert!(!dashboard.notification_form_owns_keyboard());
            dashboard.finish_session_operation("session-1");
            // Hard-won: 9739e9996: border-only forms claimed Esc while rejecting it themselves.
            dashboard.notifications.size = PaneSize::Standard;
            let mut border_only =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 19)).unwrap();
            border_only
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            assert!(dashboard.notifications_area.is_some());
            assert!(!dashboard.notification_form_owns_keyboard());
            assert!(
                !chats["session-1"].elicitation_input_available_in(
                    mj_chat::chat::ElicitationSurface::Notification
                )
            );
            dashboard.notifications.size = PaneSize::Maximized;
            let mut tiny =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 5)).unwrap();
            tiny.draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
            assert!(!dashboard.notification_form_owns_keyboard());
            for code in [KeyCode::Enter, KeyCode::Esc] {
                assert!(
                    dashboard
                        .handle_notification_form_event(&mut chats, &Event::Key(key(code)))
                        .is_none()
                );
            }
            assert!(chats["session-1"].has_elicitation());
            assert!(answers.try_recv().is_err());
            terminal
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            // Hard-won: 1f62bc101: a hidden presentation could regain an old pressed button when it returned.
            let lines = buffer_lines(terminal.backend().buffer());
            let card = dashboard.notifications_area.unwrap();
            let (x, y) = point(
                &lines[usize::from(card.y)..usize::from(card.bottom())],
                "Answer & Next",
            );
            let press = crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: card.y + y,
                modifiers: KeyModifiers::NONE,
            };
            dashboard
                .handle_notification_form_event(&mut chats, &Event::Mouse(press))
                .unwrap();
            tiny.draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
            assert!(chats["session-1"].elicitation_pointer_surface().is_none());
            terminal
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            dashboard
                .handle_notification_form_event(
                    &mut chats,
                    &Event::Mouse(crossterm::event::MouseEvent {
                        kind: MouseEventKind::Up(MouseButton::Left),
                        ..press
                    }),
                )
                .unwrap();
            assert!(chats["session-1"].has_elicitation());
            assert!(answers.try_recv().is_err());
            dashboard
                .handle_notification_form_event(&mut chats, &Event::Key(key(KeyCode::Enter)))
                .unwrap();
            let (_, response) =
                tokio::time::timeout(std::time::Duration::from_secs(5), answers.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                response,
                ElicitationResponse::Accept {
                    content: [(
                        "path".into(),
                        mj_core::elicitation::ElicitationValue::Boolean(true)
                    )]
                    .into_iter()
                    .collect()
                }
            );
            // Replacing the chat leaves an old host rectangle until redraw;
            // only the newly owned dialog knows that it has not been shown.
            let mut next = question("session-1");
            next.id = "not-yet-rendered".into();
            next.fields[0].kind = mj_core::elicitation::ElicitationFieldKind::Boolean {
                default: Some(true),
            };
            let (chat, mut next_answers) = form_chat("session-1", next);
            chats.insert("session-1".into(), chat);
            let result = dashboard
                .handle_notification_form_event(&mut chats, &Event::Key(key(KeyCode::Enter)))
                .unwrap();
            assert!(!result.consumed);
            assert!(chats["session-1"].has_elicitation());
            assert!(next_answers.try_recv().is_err());
            // Hard-won: 45223194f: a visible one-row answer control rejected every event without a message row.
            let (chat, mut compact_answers) = form_chat("session-1", question("session-1"));
            chats.insert("session-1".into(), chat);
            dashboard.notifications.size = PaneSize::Standard;
            let mut compact =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
            compact
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            let area = dashboard.notifications.form_area.unwrap();
            assert_eq!(area.height, 3, "one row inside the form border");
            // Hard-won: 5dc2084: compact controls accepted answers while the request had no visible row.
            let message = dashboard
                .frame_surfaces
                .surface(mj_chat::selection::SurfaceId::NotificationText(
                    dashboard.notifications.text_generation,
                ))
                .unwrap();
            assert!(message.rect.width > 0 && message.rect.height > 0);
            assert!(
                buffer_lines(compact.backend().buffer())
                    .iter()
                    .any(|line| line.contains("path"))
            );

            // Hard-won: c9867384e: inline focus left a prefix armed, so q detached instead of entering the answer.
            dashboard.route_bound_key(&crossterm::event::KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
            ));
            assert!(dashboard.prefix_pending());
            dashboard.resize_mode = true;
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                let mouse = crossterm::event::MouseEvent {
                    kind,
                    column: area.x + area.width / 2 + 4,
                    row: area.y + 1,
                    modifiers: KeyModifiers::NONE,
                };
                assert!(
                    dashboard
                        .handle_notification_form_event(&mut chats, &Event::Mouse(mouse))
                        .unwrap()
                        .consumed
                );
            }
            assert!(!dashboard.prefix_pending());
            assert!(!dashboard.resize_mode);
            assert_eq!(
                dashboard.route_bound_key(&key(KeyCode::Char('q'))),
                crate::KeyRoute::Forward
            );
            for event in [
                Event::Key(key(KeyCode::Char('q'))),
                Event::Paste("compact".into()),
                Event::Key(key(KeyCode::Enter)),
            ] {
                assert!(
                    dashboard
                        .handle_notification_form_event(&mut chats, &event)
                        .unwrap()
                        .consumed
                );
            }
            let (_, response) =
                tokio::time::timeout(std::time::Duration::from_secs(5), compact_answers.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                response,
                ElicitationResponse::Accept {
                    content: [(
                        "path".into(),
                        mj_core::elicitation::ElicitationValue::String("qcompact".into())
                    )]
                    .into_iter()
                    .collect()
                }
            );
        }

        // Hard-won: 45223194f: release over another session's notification bypassed the conversation's capture.
        let mut dashboard = dashboard_with_session(running_session());
        let mut second = running_session();
        second.id = "session-2".into();
        dashboard
            .durable_records
            .insert(second.id.clone(), second.clone());
        dashboard.state.sessions.insert(second.id.clone(), second);
        serve_attention(&mut dashboard, &["session-1"], SessionAttention::NeedsInput);
        let mut request = question("session-1");
        request.fields[0].kind = mj_core::elicitation::ElicitationFieldKind::Boolean {
            default: Some(false),
        };
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .pending_elicitations = vec![request.clone()];
        crate::test_support::serve_form_card(&mut dashboard, "session-1", None, &request);
        dashboard.set_current_session(Some("session-2"));
        dashboard.notifications.size = PaneSize::Maximized;
        let (first, _) = form_chat("session-1", request.clone());
        let (second, _) = form_chat("session-2", request);
        let mut chats = std::collections::BTreeMap::from([
            ("session-1".into(), first),
            ("session-2".into(), second),
        ]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 44)).unwrap();
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let (x, y) = point(&lines, mj_chat::components::Checkbox::marker(false));
        assert!(
            y < dashboard.notifications_area.unwrap().y,
            "the pressed control belongs to the conversation"
        );
        chats
            .get_mut("session-2")
            .unwrap()
            .handle_event_result(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            }));
        assert_eq!(
            dashboard.conversation_pointer_session(&chats).as_deref(),
            Some("session-2")
        );
        let area = dashboard.notifications.form_area.unwrap();
        let release = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 2,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            dashboard.notification_form_mouse_ownership(&chats, release),
            None
        );
        assert!(
            dashboard
                .handle_notification_form_event(&mut chats, &Event::Mouse(release))
                .is_none()
        );
        let owner = dashboard.conversation_pointer_session(&chats).unwrap();
        chats
            .get_mut(&owner)
            .unwrap()
            .handle_event_result(Event::Mouse(release));
        assert!(dashboard.conversation_pointer_session(&chats).is_none());
        // Hard-won: 9739e9996: global commands left a different session's notification button armed.
        let card = dashboard.notifications_area.unwrap();
        let (x, y) = point(
            &lines[usize::from(card.y)..usize::from(card.bottom())],
            "Answer & Next",
        );
        let press = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: card.y + y,
            modifiers: KeyModifiers::NONE,
        };
        dashboard
            .handle_notification_form_event(&mut chats, &Event::Mouse(press))
            .unwrap();
        assert_eq!(
            chats["session-1"].elicitation_pointer_surface(),
            Some(mj_chat::chat::ElicitationSurface::Notification)
        );
        dashboard.cancel_all_component_pointers(&mut chats);
        dashboard.focus = Focus::Sessions;
        assert!(chats["session-1"].elicitation_pointer_surface().is_none());
        dashboard
            .handle_notification_form_event(
                &mut chats,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    ..press
                }),
            )
            .unwrap();
        assert!(chats["session-1"].has_elicitation());
        // Hard-won: dc29e81c6: terminal focus loss left a notification-only button armed.
        dashboard
            .handle_notification_form_event(&mut chats, &Event::Mouse(press))
            .unwrap();
        dashboard.prepare_component_event(&Event::FocusLost, &mut chats);
        assert!(chats["session-1"].elicitation_pointer_surface().is_none());
        dashboard
            .handle_notification_form_event(
                &mut chats,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    ..press
                }),
            )
            .unwrap();
        assert!(chats["session-1"].has_elicitation());
        // Hard-won: c9867384e: a dashboard button's release was stolen by the inline form.
        let (x, y) = point(
            &lines[usize::from(card.y)..usize::from(card.bottom())],
            "Go to Session",
        );
        let dashboard_press = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: card.y + y,
            modifiers: KeyModifiers::NONE,
        };
        dashboard.handle_mouse(dashboard_press);
        assert!(dashboard.component_pointer_captured());
        let release_in_form = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 3,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            dashboard
                .handle_notification_form_event(&mut chats, &Event::Mouse(release_in_form))
                .is_none()
        );
        dashboard.handle_mouse(release_in_form);
        assert!(!dashboard.component_pointer_captured());
        dashboard.handle_mouse(dashboard_press);
        assert_eq!(
            dashboard.handle_mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..dashboard_press
            }),
            DashboardAction::Open {
                session_id: "session-1".into()
            }
        );
        // Go to Session above moved Browse to the notification's session.
        // Put the competing conversation back before exercising its capture.
        dashboard.set_current_session(Some("session-2"));
        // Hard-won: e1b4abc81: lifecycle-owned conversation capture blocked an unrelated notification form.
        let (x, y) = point(&lines, mj_chat::components::Checkbox::marker(false));
        chats
            .get_mut("session-2")
            .unwrap()
            .handle_event_result(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            }));
        assert_eq!(
            chats["session-2"].elicitation_pointer_surface(),
            Some(mj_chat::chat::ElicitationSurface::Conversation)
        );
        dashboard.begin_session_operation(
            "session-2".into(),
            crate::SessionOperationKind::Moving,
            None,
        );
        assert!(dashboard.conversation_pointer_session(&chats).is_none());
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        assert!(chats["session-2"].elicitation_pointer_surface().is_none());
        let lines = buffer_lines(terminal.backend().buffer());
        let card = dashboard.notifications_area.unwrap();
        let (x, y) = point(
            &lines[usize::from(card.y)..usize::from(card.bottom())],
            mj_chat::components::Checkbox::marker(false),
        );
        let press = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: card.y + y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            dashboard.notification_form_mouse_ownership(&chats, press),
            Some(true)
        );
        assert!(
            dashboard
                .handle_notification_form_event(&mut chats, &Event::Mouse(press))
                .unwrap()
                .consumed
        );
        dashboard
            .handle_notification_form_event(
                &mut chats,
                &Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    ..press
                }),
            )
            .unwrap();
        // Hard-won: dc29e81c6: a transcript thumb released over another session's form kept dragging.
        dashboard.finish_session_operation("session-2");
        assert_eq!(dashboard.current_session_id(), Some("session-2"));
        let mut projection = mj_core::state::MaterializedSession::empty("session-2");
        projection.applied_event_ordinal = 40;
        projection.transcript = (0..40).map(|index| mj_core::state::TranscriptItem {
            stable_id: format!("scrollbar-history-{index}"), position: index, latest_content_event_ordinal: Some(index + 1), created_at_ms: 0, last_changed_at_ms: 0,
            body: mj_core::state::TranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content":{"type":"text", "text": format!("Transcript row {index}")}})],
                streaming: false,
            },
        }.into()).collect();
        chats
            .get_mut("session-2")
            .unwrap()
            .apply_materialized(&projection, &[], &[]);
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        let transcript = chats["session-2"]
            .frame_surfaces()
            .surface(mj_chat::selection::SurfaceId::Transcript)
            .unwrap()
            .rect;
        let scrollbar_press = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: transcript.right(),
            row: transcript.y + transcript.height / 2,
            modifiers: KeyModifiers::NONE,
        };
        chats
            .get_mut("session-2")
            .unwrap()
            .handle_event_result(Event::Mouse(scrollbar_press));
        assert!(chats["session-2"].transcript_scrollbar_dragging());
        assert_eq!(
            dashboard.conversation_pointer_session(&chats).as_deref(),
            Some("session-2")
        );
        let form = dashboard.notifications.form_area.unwrap();
        let release = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: form.x + 3,
            row: form.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            dashboard
                .handle_notification_form_event(&mut chats, &Event::Mouse(release))
                .is_none()
        );
        let owner = dashboard.conversation_pointer_session(&chats).unwrap();
        chats
            .get_mut(&owner)
            .unwrap()
            .handle_event_result(Event::Mouse(release));
        assert!(!chats["session-2"].transcript_scrollbar_dragging());
        chats
            .get_mut("session-2")
            .unwrap()
            .handle_event_result(Event::Mouse(scrollbar_press));
        assert!(chats["session-2"].transcript_scrollbar_dragging());
        dashboard.prepare_component_event(&Event::FocusLost, &mut chats);
        assert!(!chats["session-2"].transcript_scrollbar_dragging());
        chats
            .get_mut("session-2")
            .unwrap()
            .handle_event_result(Event::Mouse(scrollbar_press));
        assert!(
            chats["session-2"].transcript_scrollbar_dragging(),
            "focus loss disarms capture without deleting the displayed hitbox"
        );
        // Hard-won: 56747ef: the earlier-message reader left a hidden transcript thumb captured.
        chats
            .get_mut("session-2")
            .unwrap()
            .handle_event_result(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::CONTROL,
            )));
        assert!(!chats["session-2"].transcript_scrollbar_dragging());
        assert!(dashboard.conversation_pointer_session(&chats).is_none());
        assert_eq!(
            dashboard.notification_form_mouse_ownership(&chats, release),
            Some(true)
        );
    }

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

    pub(crate) fn published_dashboard() -> DashboardState {
        use mj_client::runtime_feed::{CardForm, NotificationCard};
        use mj_core::attention::NotificationStackEntry;
        let mut dashboard = dashboard();
        dashboard.set_notification_cards(Some([
            ("q1".into(), NotificationCard {
                since_ms: 20,
                badge: NotificationKind::NeedsInput,
                entries: vec![NotificationStackEntry {
                    completed_at_ms: 20,
                    completed_ordinal: 4,
                    kind: NotificationKind::Important,
                    message: EntryMessage::Found("outside-tail".into()),
                }],
                dismissal: Some(NotificationDismissal::Stack { revision: 50 }),
                holder: Some("q2".into()),
                form: Some(CardForm {
                    source: mj_client::runtime_feed::CardFormSource::Elicitation,
                    raised_at_ms: 60,
                    request_id: "request".into(),
                    message: "Which approach should we take?\nThe question stays here until you answer or clear it.".into(),
                }),
            }),
            ("i1".into(), NotificationCard {
                since_ms: 10,
                badge: NotificationKind::Important,
                entries: vec![NotificationStackEntry {
                    completed_at_ms: 10,
                    completed_ordinal: 7,
                    kind: NotificationKind::Important,
                    message: EntryMessage::None,
                }],
                dismissal: Some(NotificationDismissal::ReadThrough(7)),
                holder: None,
                form: None,
            }),
        ].into_iter().collect()));
        dashboard
    }

    // Hard-won: dc77f11c1: an id-less question skipped its bounded text read.
    // Hard-won: 1f62bc101: changing card items retained a hidden form's armed click.
    #[tokio::test]
    async fn notification_navigation_keeps_reads_bound_to_the_selected_entry() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut dashboard = published_dashboard();
        let entries = dashboard.notification_entries();
        assert_eq!(entries.len(), 2, "q2 has attention but no published card");
        assert_eq!(
            entries[0].session_id, "i1",
            "the older important card stays ahead of the newer question"
        );
        dashboard.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(
            dashboard.displayed_notification_session_id().as_deref(),
            Some("q2")
        );

        // Hard-won: a882023b6: a form beside dismissible messages fell back to a free-text reply.
        assert_eq!(
            dashboard.notification_form_session().as_deref(),
            Some("q2"),
            "a selected form remains answerable beside dismissible messages"
        );
        // Hard-won: 1f62bc101: changing the selected item left the retained chat's notification button armed.
        let mut request = question("q2");
        request.fields[0].kind = mj_core::elicitation::ElicitationFieldKind::Boolean {
            default: Some(true),
        };
        let (chat, mut answers) = form_chat("q2", request);
        let mut chats = std::collections::BTreeMap::from([("q2".into(), chat)]);
        dashboard.set_current_session(Some("q2"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 44)).unwrap();
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let card = dashboard.notifications_area.unwrap();
        let (x, y) = point(
            &lines[usize::from(card.y)..usize::from(card.bottom())],
            "Answer & Next",
        );
        let press = mouse_at(MouseEventKind::Down(MouseButton::Left), (x, card.y + y));
        dashboard
            .handle_notification_form_event(&mut chats, &crossterm::event::Event::Mouse(press))
            .unwrap();
        assert_eq!(
            chats["q2"].elicitation_pointer_surface(),
            Some(mj_chat::chat::ElicitationSurface::Notification)
        );
        dashboard.handle_key(alt(KeyCode::PageUp));
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        assert!(chats["q2"].elicitation_pointer_surface().is_none());
        dashboard.handle_key(alt(KeyCode::PageDown));
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut dashboard,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        dashboard
            .handle_notification_form_event(
                &mut chats,
                &crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    ..press
                }),
            )
            .unwrap();
        assert!(chats["q2"].has_elicitation());
        assert!(answers.try_recv().is_err());
        let newest = drawn(&mut dashboard, 120, 44);
        assert!(newest.iter().any(|line| line.contains("2 of 2")));
        assert!(newest.iter().any(|line| line.contains("Which approach")));
        dashboard.handle_key(alt(KeyCode::PageDown));
        assert!(dashboard.wanted_notification_message().is_none());

        let previous = point(&newest, "Previous");
        dashboard.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), previous));
        dashboard.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), previous));
        // Hard-won: c9867384e: a same-batch paste used the old message frame after revealing a form.
        let revision = dashboard.notifications.selection_revision;
        dashboard.handle_key(alt(KeyCode::PageDown));
        assert_ne!(revision, dashboard.notifications.selection_revision);
        assert!(dashboard.notifications.form_area.is_none());
        assert!(dashboard.notification_input_needs_frame());
        drawn(&mut dashboard, 120, 44);
        assert!(!dashboard.notification_input_needs_frame());
        dashboard.handle_key(alt(KeyCode::PageUp));
        let item = dashboard.wanted_notification_message().unwrap();
        assert_eq!(item.session_id, "q1", "an entry belongs to its own session");
        assert_eq!(
            item.source,
            NotificationReadSource::Item("outside-tail".into())
        );
        dashboard.sync_notification_message_read(Some(item.clone()), true);
        dashboard.handle_key(alt(KeyCode::PageUp));
        assert_eq!(dashboard.wanted_notification_message(), Some(item.clone()));
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("1 of 2"))
        );

        let stored = mj_core::state::TranscriptItem {
            stable_id: "outside-tail".into(),
            position: 2,
            latest_content_event_ordinal: Some(3),
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: mj_core::state::TranscriptBody::Agent {
                chunks: vec![
                    serde_json::json!({"content":{"type":"text", "text":"The earlier completed update"}}),
                ],
                streaming: false,
            },
        };
        assert!(dashboard.notification_message_read_finished(
            &item,
            Some("The earlier completed update".into())
        ));
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("The earlier completed update"))
        );
        let mut cards = dashboard.notification_cards.clone().unwrap();
        let question = cards.get_mut("q1").unwrap();
        let mut arrived = question.entries[0].clone();
        arrived.completed_ordinal = 6;
        arrived.completed_at_ms = 40;
        arrived.message = EntryMessage::None;
        question.entries.push(arrived);
        question.dismissal = Some(NotificationDismissal::Stack { revision: 52 });

        let skipped = cards.get_mut("i1").unwrap();
        let mut arrived = skipped.entries[0].clone();
        arrived.completed_ordinal = 8;
        arrived.completed_at_ms = 45;
        skipped.entries.push(arrived);
        skipped.dismissal = Some(NotificationDismissal::ReadThrough(8));
        dashboard.set_notification_cards(Some(cards));

        let reading = drawn(&mut dashboard, 120, 44);
        assert!(reading.iter().any(|line| line.contains("1 of 3")));
        assert!(
            reading
                .iter()
                .any(|line| line.contains("The earlier completed update"))
        );
        assert_eq!(dashboard.wanted_notification_message(), Some(item.clone()));

        dashboard.handle_key(alt(KeyCode::PageDown));
        assert!(!dashboard.notification_message_read_finished(&item, None));
        dashboard.handle_key(alt(KeyCode::PageDown));
        dashboard.handle_key(alt(KeyCode::PageUp));
        dashboard.handle_key(alt(KeyCode::PageUp));
        let returning = dashboard.wanted_notification_message().unwrap();
        assert_ne!(returning, item);
        dashboard.sync_notification_message_read(Some(returning.clone()), true);
        assert!(!dashboard.notification_message_read_finished(&item, None));

        let mut cards = dashboard.notification_cards.clone().unwrap();
        let replacement = cards.get_mut("q1").unwrap();
        replacement.entries[0].completed_ordinal = 5;
        replacement.entries[0].completed_at_ms = 25;
        replacement.since_ms = 25;
        replacement.form.as_mut().unwrap().request_id = "replacement".into();
        dashboard.set_notification_cards(Some(cards));
        assert!(!dashboard.notification_message_read_finished(&returning, None));
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("3 of 3"))
        );

        let mut projection = materialized_session_for("q1", vec![stored.into()]);
        projection.applied_event_ordinal = 5;
        dashboard.apply_materialized_session(&projection);
        dashboard.handle_key(alt(KeyCode::PageUp));
        dashboard.handle_key(alt(KeyCode::PageUp));
        assert!(dashboard.wanted_notification_message().is_none());
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("The earlier completed update"))
        );

        dashboard.set_question_message("q1", None, true);
        let mut cards = dashboard.notification_cards.clone().unwrap();
        let question = &mut cards.get_mut("q1").unwrap().entries[0];
        question.kind = NotificationKind::NeedsInput;
        question.message = EntryMessage::Locating;
        dashboard.set_notification_cards(Some(cards));
        dashboard.handle_key(alt(KeyCode::PageUp));
        let locating = dashboard.wanted_notification_message().unwrap();
        assert_eq!(locating.session_id, "q1");
        assert_eq!(locating.source, NotificationReadSource::Through(5));

        let mut cards = dashboard.notification_cards.clone().unwrap();
        cards.get_mut("q1").unwrap().entries.clear();
        dashboard.set_notification_cards(Some(cards));
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("1 of 1"))
        );
        assert!(dashboard.wanted_notification_message().is_none());

        assert_eq!(
            dashboard.handle_key(alt(KeyCode::Char('c'))),
            DashboardAction::DismissNotifications {
                dismissals: vec![("i1".into(), NotificationDismissal::ReadThrough(8))],
            }
        );
        assert!(dashboard.notification_dismissal_pending("i1"));
        dashboard.notifications_dismissed(vec![(
            "i1".into(),
            NotificationDismissal::ReadThrough(8),
            Ok(8),
        )]);
        assert!(!dashboard.notification_dismissal_pending("i1"));
        assert_eq!(
            dashboard.state.sessions["i1"].viewed_through_event_ordinal,
            8
        );
        assert_eq!(
            dashboard.handle_key(alt(KeyCode::Char('n'))),
            DashboardAction::DismissNotification {
                session_id: "q1".into(),
                target: NotificationDismissal::Stack { revision: 52 },
            }
        );
        dashboard.handle_key(alt(KeyCode::Char('s')));
        let bounded = dashboard.wanted_notification_message().unwrap();
        assert_eq!(bounded.session_id, "i1");
        assert_eq!(bounded.source, NotificationReadSource::Through(8));

        dashboard.set_notification_cards(Some(Default::default()));
        assert!(dashboard.notification_entries().is_empty());
        assert!(dashboard.notifications.skipped.is_empty());

        // Hard-won: b817d9202: a native child's projected form suppressed its owner's quick reply forever.
        use mj_client::runtime_feed::{CardForm, NotificationCard};
        dashboard.set_notification_cards(Some(
            [(
                "q1".into(),
                NotificationCard {
                    since_ms: 1,
                    badge: NotificationKind::NeedsInput,
                    entries: Vec::new(),
                    dismissal: None,
                    holder: Some("q1".into()),
                    form: Some(CardForm::native(
                        "child",
                        1,
                        "native-request",
                        1,
                        "Approve native action?".into(),
                    )),
                },
            )]
            .into_iter()
            .collect(),
        ));
        assert!(dashboard.notification_form_session().is_none());
        assert!(
            drawn(&mut dashboard, 120, 44)
                .iter()
                .any(|line| line.contains("Quick reply:"))
        );
        dashboard.handle_key(key(KeyCode::Char('y')));
        let DashboardAction::NotificationReply {
            session_id, prompt, ..
        } = dashboard.handle_key(key(KeyCode::Enter))
        else {
            panic!("native forms keep the existing quick reply dispatcher");
        };
        assert_eq!(session_id, "q1");
        assert_eq!(prompt.payload.text, "y");
        // Hard-won: e1b4abc81: completed review findings entered the inline loader without any elicitation to serve.
        dashboard.notification_reply_finished("q1", &prompt.command_id, Ok(()));
        dashboard.set_notification_cards(Some(
            [(
                "q1".into(),
                NotificationCard {
                    since_ms: 2,
                    badge: NotificationKind::NeedsInput,
                    entries: Vec::new(),
                    dismissal: None,
                    holder: Some("q1".into()),
                    form: Some(CardForm::review_findings(
                        "reviewer-a",
                        2,
                        "Review found issues".into(),
                    )),
                },
            )]
            .into_iter()
            .collect(),
        ));
        assert!(dashboard.notification_form_session().is_none());
        let lines = drawn(&mut dashboard, 120, 44);
        assert!(lines.iter().any(|line| line.contains("Quick reply:")));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("Waiting for the session's form update"))
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "q1".into()
            }
        );
        // Hard-won: dc29e81c6: a background form attachment cleared or replaced an unrelated failure notice.
        dashboard.set_failure_notice("Unrelated command failed");
        let notice = dashboard.notice().unwrap().to_owned();
        dashboard.report_chat_attachment_result("q2", Some("Worker unavailable"), false);
        assert_eq!(dashboard.notice().as_deref(), Some(notice.as_str()));
        assert_eq!(
            dashboard
                .notifications
                .form_errors
                .get("q2")
                .map(String::as_str),
            Some("Worker unavailable")
        );
        dashboard.report_chat_attachment_result("q2", None, false);
        assert_eq!(dashboard.notice().as_deref(), Some(notice.as_str()));
        assert!(!dashboard.notifications.form_errors.contains_key("q2"));
    }

    // Hard-won: 6597a0761: a new card from the same session kept the old scroll offset.
    #[test]
    fn a_new_card_from_the_same_session_opens_at_its_beginning() {
        let mut dashboard = dashboard();
        let long = (0..80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for detail in dashboard.session_details.values_mut() {
            detail.last_agent_message = Some(long.as_str().into());
        }
        drawn(&mut dashboard, 140, 50);
        dashboard.scroll_notification(5);
        drawn(&mut dashboard, 140, 50);
        assert_eq!(dashboard.notifications.scroll, 5);
        // The same session publishes a new question while staying the top card.
        dashboard.notification_dismissals.insert(
            "q1".into(),
            NotificationDismissal::Question(mj_core::assessment::InferredQuestion {
                turn_id: "turn-9".into(),
                completed_ordinal: 9,
            }),
        );
        drawn(&mut dashboard, 140, 50);
        assert_eq!(dashboard.notification_entries()[0].session_id, "q1");
        assert_eq!(dashboard.notifications.scroll, 0);
    }

    #[test]
    fn notifications_keys_rotate_submit_and_restore_failed_dismissals() {
        let mut dashboard = dashboard();
        assert_eq!(dashboard.notification_entries()[0].session_id, "q1");
        dashboard.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(dashboard.notification_entries()[0].session_id, "q2");
        dashboard.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(dashboard.notification_entries()[0].session_id, "i1");
        dashboard.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(dashboard.notification_entries()[0].session_id, "q1");
        let action = dashboard.handle_key(alt(KeyCode::Char('n')));
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
            dashboard.handle_key(alt(KeyCode::Char('c')))
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
        for label in ["Go to Session", "Clear & Next", "Skip", "Send"] {
            for replace_target in [false, true] {
                let mut race = self::dashboard();
                race.quick_reply_mut("q1")
                    .editor
                    .set_draft("pressed reply".into());
                race.quick_reply_mut("q2")
                    .editor
                    .set_draft("other reply".into());
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
                    let skipped = race.notification_entries()[0].identity().skip_identity();
                    race.notifications.skipped.push(skipped);
                }
                drawn(&mut race, 140, 50);
                let action =
                    race.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), button));
                match action {
                    DashboardAction::Open { session_id } => {
                        assert_eq!(session_id, "q1");
                    }
                    DashboardAction::NotificationReply {
                        card,
                        session_id,
                        prompt,
                    } => {
                        assert_eq!(card.session_id, "q1");
                        assert_eq!(card.target, target);
                        assert_eq!(session_id, "q1");
                        assert_eq!(prompt.payload.text, "pressed reply");
                    }
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
                    DashboardAction::NotificationSkip { card } => {
                        assert_eq!(card.session_id, "q1");
                        assert_eq!(card.target, target);
                        assert_eq!(
                            race.notifications.skipped.last().map(|card| match card {
                                SkippedCard::Published(id) => id.as_str(),
                                SkippedCard::Legacy(card) => card.session_id.as_str(),
                            }),
                            Some("q1")
                        );
                        if replace_target {
                            assert_eq!(
                                race.notification_entries()[0].session_id,
                                "q1",
                                "Skip must not defer the replacement question"
                            );
                        }
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
        let request = wanted.as_ref().unwrap();
        assert_eq!(request.session_id, "q1");
        assert_eq!(
            request.source,
            NotificationReadSource::Item("outside-tail".into())
        );
        old.sync_notification_message_read(wanted.clone(), true);
        assert_eq!(
            old.notification_card_text("q1", AttentionLevel::Waiting),
            "loading…"
        );
        old.notification_message_read_finished(request, None);
        assert_eq!(
            old.notification_card_text("q1", AttentionLevel::Waiting),
            "message not available"
        );
        old.sync_notification_message_read(wanted.clone(), false);
        assert_eq!(
            old.notification_card_text("q1", AttentionLevel::Waiting),
            "message not available"
        );
        old.sync_notification_message_read(wanted.clone(), true);
        old.notification_message_read_finished(request, Some("The buried question".to_owned()));
        assert_eq!(
            old.notification_message("q1", AttentionLevel::Waiting),
            "The buried question"
        );
        let skipped = old.notification_entries()[0].identity().skip_identity();
        old.notifications.skipped.push(skipped);
        assert_ne!(old.wanted_notification_message(), wanted);
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

    #[tokio::test]
    async fn dragging_an_answer_row_selects_its_drawn_text_and_a_click_still_answers() {
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use mj_chat::selection::{SelectionAction, SelectionState, SurfaceId};
        let mut dashboard = dashboard_with_session(running_session());
        serve_attention(&mut dashboard, &["session-1"], SessionAttention::NeedsInput);
        let mut request = question("session-1");
        request.fields[0].kind = mj_core::elicitation::ElicitationFieldKind::Boolean {
            default: Some(false),
        };
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .pending_elicitations = vec![request.clone()];
        crate::test_support::serve_form_card(&mut dashboard, "session-1", None, &request);
        dashboard.notifications.size = PaneSize::Maximized;
        dashboard.focus = Focus::Notifications;
        let (chat, _) = form_chat("session-1", request);
        let mut chats = std::collections::BTreeMap::from([("session-1".to_owned(), chat)]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 44)).unwrap();
        let draw = |terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
                    dashboard: &mut DashboardState,
                    chats: &mut _| {
            terminal
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        dashboard,
                        chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            buffer_lines(terminal.backend().buffer())
        };
        draw(&mut terminal, &mut dashboard, &mut chats);
        let lines = draw(&mut terminal, &mut dashboard, &mut chats);
        assert!(dashboard.notifications.form_area.is_some(), "{lines:#?}");
        // The conversation pane shows the same question; use the card's copy.
        let form = dashboard.notifications.form_area.unwrap();
        let (x, row) = point(
            &lines[usize::from(form.y)..],
            mj_chat::components::Checkbox::marker(false),
        );
        let y = form.y + row;
        let event = |kind, column, row| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let mut selection = SelectionState::new();

        // A drag from the answer row selects what is drawn there; the form
        // gets the press but never the click.
        let press = event(MouseEventKind::Down(MouseButton::Left), x, y);
        let forwarded = dashboard
            .route_question_press(&mut selection, true, &press)
            .expect("the press reaches the form");
        dashboard
            .handle_notification_form_event(&mut chats, &forwarded)
            .unwrap();
        let surfaces = dashboard.frame_surfaces().clone();
        selection.on_mouse_drag(x + 6, y, &surfaces);
        dashboard.settle_question_press(&selection, &mut chats);
        let SelectionAction::CopyRequested { surface, range } =
            selection.on_mouse_up(x + 6, y, &surfaces)
        else {
            panic!("a drag copies");
        };
        assert!(matches!(surface, SurfaceId::NotificationFormBody(_)));
        let frame = *surfaces.surface(surface).unwrap();
        let drawn = mj_chat::selection::extract_rows(terminal.backend().buffer(), &frame, &range);
        let expected: String = lines[usize::from(y)]
            .chars()
            .skip(usize::from(x))
            .take(7)
            .collect();
        assert_eq!(drawn, expected.trim_end());
        let lines = draw(&mut terminal, &mut dashboard, &mut chats);
        assert!(
            lines
                .iter()
                .any(|line| line.contains(mj_chat::components::Checkbox::marker(false))),
            "the drag did not choose the option: {lines:#?}"
        );
        assert!(chats["session-1"].elicitation_pointer_surface().is_none());

        // A press and release in place is still a click.
        let mut selection = SelectionState::new();
        let release = event(MouseEventKind::Up(MouseButton::Left), x, y);
        for pointer in [press, release] {
            let forwarded = dashboard
                .route_question_press(&mut selection, true, &pointer)
                .expect("a click reaches the form");
            dashboard
                .handle_notification_form_event(&mut chats, &forwarded)
                .unwrap();
        }
        let lines = draw(&mut terminal, &mut dashboard, &mut chats);
        assert!(
            lines
                .iter()
                .any(|line| line.contains(mj_chat::components::Checkbox::marker(true))),
            "the click chose the option: {lines:#?}"
        );
    }

    #[tokio::test]
    async fn dragging_notification_text_uses_transcript_selection_and_card_identity() {
        use mj_chat::selection::{SelectionAction, SelectionState, SurfaceId};
        let mut dashboard = dashboard();
        dashboard.set_pane_size(SupportPane::Notifications, PaneSize::Maximized);
        let source = format!("{}\nSecond hard line 世界", "a word boundary ".repeat(12));
        dashboard
            .session_details
            .get_mut("q1")
            .unwrap()
            .last_agent_message = Some(source.clone().into());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 50)).unwrap();
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        let surfaces = dashboard.frame_surfaces().clone();
        let body = (0..50)
            .flat_map(|y| (0..100).map(move |x| (x, y)))
            .find_map(|(x, y)| {
                surfaces
                    .surface_at(x, y)
                    .filter(|surface| matches!(surface.id, SurfaceId::NotificationText(_)))
                    .copied()
            })
            .unwrap();
        let mut selection = SelectionState::new();
        selection.on_mouse_down(body.rect.x, body.rect.y, &surfaces);
        selection.on_mouse_drag(body.rect.right() + 5, body.rect.bottom() + 2, &surfaces);
        let SelectionAction::CopyRequested {
            surface: SurfaceId::NotificationText(generation),
            range,
        } = selection.on_mouse_up(body.rect.right() + 5, body.rect.bottom() + 2, &surfaces)
        else {
            panic!("drag copies the notification body");
        };
        assert_eq!(
            dashboard
                .notification_selection_text(generation, &range)
                .unwrap(),
            source
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
        );
        terminal
            .draw(|frame| {
                crate::render::render(frame, &mut dashboard);
                mj_chat::selection::highlight(frame.buffer_mut(), &body, &range);
            })
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .cell((body.rect.x, body.rect.y))
                .unwrap()
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(
            !matches!(
                surfaces
                    .surface_at(body.rect.x, body.rect.bottom() + 1)
                    .map(|s| s.id),
                Some(SurfaceId::NotificationText(_))
            ),
            "reply/buttons are not a selection surface"
        );
        // A new dismissal target for the same session invalidates the row space.
        dashboard
            .notification_dismissals
            .insert("q1".into(), NotificationDismissal::ReadThrough(999));
        terminal
            .draw(|frame| crate::render::render(frame, &mut dashboard))
            .unwrap();
        selection.reconcile_surfaces(dashboard.frame_surfaces());
        assert_eq!(selection.active_surface(), None);

        // Hard-won: 45223194f: loading text and successive form pages shared a selection generation.
        let mut dashboard = dashboard_with_session(running_session());
        serve_attention(&mut dashboard, &["session-1"], SessionAttention::NeedsInput);
        let mut request = question("session-1");
        let mut next = request.fields[0].clone();
        next.id = "second".into();
        next.title = "Second question".into();
        request.fields.push(next);
        dashboard
            .session_details
            .get_mut("session-1")
            .unwrap()
            .pending_elicitations = vec![request.clone()];
        crate::test_support::serve_form_card(&mut dashboard, "session-1", None, &request);
        dashboard.notifications.size = PaneSize::Maximized;
        dashboard.focus = Focus::Notifications;
        let mut chats = std::collections::BTreeMap::new();
        let mut previous_generation = None;
        for stage in 0..5 {
            terminal
                .draw(|frame| {
                    crate::combined::render_combined_for_test(
                        frame,
                        &mut dashboard,
                        &mut chats,
                        &Default::default(),
                        false,
                    );
                })
                .unwrap();
            // Hard-won: 9739e9996: copy after redraw applied a stale range to the new form.
            if let Some(generation) = previous_generation {
                assert!(!dashboard.notification_selection_is_current(generation));
            }
            previous_generation = Some(dashboard.notifications.text_generation);
            selection.reconcile_surfaces(dashboard.frame_surfaces());
            assert!(
                selection.active_surface().is_none(),
                "new content invalidates the old row space"
            );
            if stage == 4 {
                break;
            }
            let surface = dashboard
                .frame_surfaces()
                .surface(SurfaceId::NotificationText(
                    dashboard.notifications.text_generation,
                ))
                .unwrap();
            selection.on_mouse_down(surface.rect.x, surface.rect.y, dashboard.frame_surfaces());
            selection.on_mouse_drag(
                surface.rect.x + 5,
                surface.rect.y,
                dashboard.frame_surfaces(),
            );
            if stage == 0 {
                let (chat, _) = form_chat("session-1", request.clone());
                chats.insert("session-1".into(), chat);
            } else if stage >= 2 {
                // Hard-won: 57e492d: height-only compact rewrap kept the old selection generation.
                terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                    120,
                    if stage == 2 { 20 } else { 44 },
                ))
                .unwrap();
                dashboard.notifications.size = if stage == 2 {
                    PaneSize::Standard
                } else {
                    PaneSize::Maximized
                };
            } else {
                dashboard
                    .handle_notification_form_event(
                        &mut chats,
                        &crossterm::event::Event::Paste("first".into()),
                    )
                    .unwrap();
                dashboard
                    .handle_notification_form_event(
                        &mut chats,
                        &crossterm::event::Event::Key(key(KeyCode::Enter)),
                    )
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn notifications_keys_reply_open_skip_and_keep_sizes_exclusive() {
        let mut replies = dashboard();
        // Ctrl-C before the first background pump must already know its
        // destination project, and later project changes cannot retarget it.
        let abandoned_project = replies.state.sessions["q1"].bundle_id.clone();
        for ch in "abandoned before first pump".chars() {
            replies.handle_key(key(KeyCode::Char(ch)));
        }
        replies.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        replies
            .session_details
            .get_mut("q1")
            .unwrap()
            .last_agent_message = Some(
            (0..80)
                .map(|i| format!("question line {i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        );
        for ch in "draft /? d".chars() {
            replies.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(replies.quick_reply_mut("q1").editor.draft(), "draft /? d");
        replies.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(replies.quick_reply_mut("q1").editor.draft(), "draft /? d");

        // Use the real Prompt's kill/yank editing, rather than pane shortcuts.
        for ch in ['a', 'k', 'y'] {
            replies.handle_key(crossterm::event::KeyEvent::new(
                KeyCode::Char(ch),
                crossterm::event::KeyModifiers::CONTROL,
            ));
        }
        assert_eq!(replies.quick_reply_mut("q1").editor.draft(), "draft /? d");
        replies.handle_key(alt(KeyCode::Char('s')));
        for ch in "other draft".chars() {
            replies.handle_key(key(KeyCode::Char(ch)));
        }
        replies.handle_key(alt(KeyCode::Char('s')));
        replies.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(replies.notification_entries()[0].session_id, "q1");
        let DashboardAction::NotificationReply {
            card,
            session_id,
            prompt,
        } = replies.handle_key(key(KeyCode::Enter))
        else {
            panic!("Enter sends a nonempty quick reply");
        };
        assert_eq!(card.session_id, "q1");
        assert_eq!(session_id, "q1");
        assert_eq!(prompt.payload.text, "draft /? d");
        assert!(!replies.quick_reply_mut("q1").pending.is_empty());
        // Admission cannot capture focus or prevent editing another draft.
        replies.handle_key(key(KeyCode::Tab));
        assert_ne!(replies.focus, Focus::Notifications);
        replies.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::BackTab,
            crossterm::event::KeyModifiers::SHIFT,
        ));
        assert_eq!(replies.focus, Focus::Notifications);
        assert_eq!(
            replies.handle_key(key(KeyCode::Enter)),
            DashboardAction::None,
            "same draft is already in flight"
        );
        replies.handle_key(key(KeyCode::Char('x')));
        let DashboardAction::NotificationReply { prompt: newer, .. } =
            replies.handle_key(key(KeyCode::Enter))
        else {
            panic!("a different draft can send");
        };
        assert_eq!(newer.payload.text, "draft /? dx");
        assert!(replies.notification_reply_finished("q1", &newer.command_id, Ok(())));
        replies.paste_notification_reply("draft /? d");
        drawn(&mut replies, 140, 50);
        replies.scroll_notification(5);
        drawn(&mut replies, 140, 50);
        assert_eq!(replies.notifications.scroll, 5);
        assert!(replies.notification_reply_unconfirmed(
            "q1",
            &prompt.command_id,
            "lost acknowledgement"
        ));
        let visible = drawn(&mut replies, 140, 50).join("\n");
        assert!(visible.contains("Could not confirm the reply"), "{visible}");
        assert!(
            !replies.quick_reply_mut("q1").pending.is_empty(),
            "uncertain admission stays in flight"
        );
        assert!(!replies.notification_reply_finished("q2", &prompt.command_id, Ok(())));
        replies.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(replies.notification_entries()[0].session_id, "q2");
        assert!(replies.notification_reply_finished(
            "q1",
            &prompt.command_id,
            Err("refused".into())
        ));
        assert_eq!(replies.quick_reply_mut("q1").editor.draft(), "draft /? d");
        assert!(
            replies
                .quick_reply_mut("q1")
                .recoverable
                .iter()
                .any(
                    |reply| reply.error.as_deref() == Some("refused") && reply.text == "draft /? d"
                )
        );
        assert_eq!(replies.quick_reply_mut("q2").editor.draft(), "other draft");
        // A later successful receipt clears only its original session's line.
        replies.handle_key(alt(KeyCode::Char('s')));
        replies.handle_key(alt(KeyCode::Char('s')));
        let DashboardAction::NotificationReply {
            session_id, prompt, ..
        } = replies.handle_key(key(KeyCode::Enter))
        else {
            panic!("retry sends");
        };
        assert!(replies.notification_reply_finished(&session_id, &prompt.command_id, Ok(())));
        assert_eq!(replies.quick_reply_mut("q1").editor.draft(), "");
        assert_eq!(replies.quick_reply_mut("q2").editor.draft(), "other draft");

        // The detached editor uses the real destination project and shared
        // background search owner. Enter accepts a recalled prompt, then sends
        // only on a separate key press.
        let fixture = mj_client::session::replacement_session_test_fixture("q1", 1);
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (listings, mut listed) = tokio::sync::mpsc::unbounded_channel();
        let (persistence, mut recorded) = tokio::sync::mpsc::unbounded_channel();
        let history = SessionControl::new(HistoryBackend {
            inner: fixture.stopped,
            form_view: None,
            responses: None,
            response_error: None,
            requests,
            listings,
        });
        replies.state.sessions.get_mut("q1").unwrap().bundle_id = "other-project".into();
        replies.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('r'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        for ch in "recalled".chars() {
            replies.handle_key(key(KeyCode::Char(ch)));
        }
        replies.service_notification_composers(history.clone(), |_| Some(persistence.clone()));
        let mj_chat::chat::ChatDaemonRequest::RecordAbandonedPrompt {
            session_id,
            bundle_id,
            text,
        } = recorded.try_recv().unwrap()
        else {
            panic!("initial Ctrl-C reaches the shared dispatcher");
        };
        assert_eq!(session_id, "q1");
        assert_eq!(bundle_id, abandoned_project);
        assert_eq!(text, "abandoned before first pump");
        let (project, query, response) =
            tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(project, "other-project");
        assert_eq!(query, "recalled");
        response
            .send(PromptHistoryPage {
                entries: vec![PromptHistoryEntry {
                    id: 1,
                    session_id: "q1".into(),
                    text: "recalled\nanswer".into(),
                }],
                next: None,
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while replies
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text()
                != "recalled\nanswer"
            {
                replies
                    .service_notification_composers(history.clone(), |_| Some(persistence.clone()));
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let search_line = drawn(&mut replies, 180, 50)
            .into_iter()
            .find(|line| line.contains("Quick reply:"))
            .unwrap();
        assert!(search_line.contains("recalled⏎answer"), "{search_line}");
        assert!(
            search_line.contains("recalled") && search_line.contains("1/1"),
            "{search_line}"
        );
        assert_eq!(
            replies.handle_key(key(KeyCode::Enter)),
            DashboardAction::None
        );
        assert_eq!(
            replies.quick_reply_mut("q1").editor.draft(),
            "recalled\nanswer"
        );
        assert!(replies.quick_reply_mut("q1").pending.is_empty());
        let recalled_line = drawn(&mut replies, 180, 50)
            .into_iter()
            .find(|line| line.contains("Quick reply:"))
            .unwrap();
        assert!(recalled_line.contains("recalled⏎answer"), "{recalled_line}");
        let DashboardAction::NotificationReply {
            prompt: recalled, ..
        } = replies.handle_key(key(KeyCode::Enter))
        else {
            panic!("send recalled text");
        };
        assert_eq!(recalled.payload.text, "recalled\nanswer");
        assert!(replies.notification_reply_finished("q1", &recalled.command_id, Ok(())));
        replies.handle_key(key(KeyCode::Up));
        assert_eq!(
            replies
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text(),
            "recalled\nanswer",
            "successful reply is immediately in local history"
        );
        replies.quick_reply_mut("q1").editor.clear_draft();
        replies.service_notification_composers(history.clone(), |_| Some(persistence.clone()));
        // A stored listing is independently dispatched for the destination.
        let mut observed = Vec::new();
        while let Ok(id) = listed.try_recv() {
            observed.push(id);
        }
        assert!(observed.iter().any(|id| id == "q1"));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !replies.quick_reply_mut("q1").editor.has_composer_updates() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        replies.quick_reply_mut("q2").editor.clear_draft();
        replies.handle_key(alt(KeyCode::Char('s')));
        assert_eq!(
            replies.displayed_notification_session_id().as_deref(),
            Some("q2")
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                replies
                    .service_notification_composers(history.clone(), |_| Some(persistence.clone()));
                replies.handle_key(key(KeyCode::Up));
                if replies
                    .quick_reply_mut("q2")
                    .editor
                    .single_line_prompt_text()
                    == "stored\nprompt 世界"
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        replies.quick_reply_mut("q2").editor.clear_draft();
        replies.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('p'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(
            replies
                .quick_reply_mut("q2")
                .editor
                .single_line_prompt_text(),
            "stored\nprompt 世界"
        );

        replies.quick_reply_mut("q2").editor.clear_draft();
        for ch in "abandoned Quick reply".chars() {
            replies.handle_key(key(KeyCode::Char(ch)));
        }
        replies.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        replies.service_notification_composers(history, |_| Some(persistence.clone()));
        let mj_chat::chat::ChatDaemonRequest::RecordAbandonedPrompt {
            session_id,
            bundle_id,
            text,
        } = recorded.try_recv().unwrap()
        else {
            panic!("Ctrl-C reaches the common daemon dispatcher");
        };
        assert_eq!(session_id, "q2");
        assert_eq!(bundle_id, replies.state.sessions["q2"].bundle_id);
        assert_eq!(text, "abandoned Quick reply");
        replies.handle_key(key(KeyCode::Up));
        assert_eq!(
            replies
                .quick_reply_mut("q2")
                .editor
                .single_line_prompt_text(),
            text
        );

        // Refusal A must survive edits to B and a later unrelated success.
        let mut recovery = self::dashboard();
        for ch in "refused A".chars() {
            recovery.handle_key(key(KeyCode::Char(ch)));
        }
        let DashboardAction::NotificationReply { prompt: first, .. } =
            recovery.handle_key(key(KeyCode::Enter))
        else {
            panic!("send A");
        };
        recovery.quick_reply_mut("q1").editor.clear_draft();
        for ch in "draft B".chars() {
            recovery.handle_key(key(KeyCode::Char(ch)));
        }
        assert!(recovery.notification_reply_finished(
            "q1",
            &first.command_id,
            Err("denied A".into())
        ));
        assert_eq!(
            recovery
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text(),
            "draft B"
        );
        let refusal = drawn(&mut recovery, 180, 50).join("\n");
        assert!(
            refusal.contains("Not sent: refused A") && refusal.contains("denied A"),
            "{refusal}"
        );
        // Send and Restore have distinct hit areas. A refused A cannot
        // retarget a Send gesture over the currently displayed B.
        let lines = drawn(&mut recovery, 180, 50);
        let send = point(&lines, " Send ");
        let restore = point(&lines, "Restore unsent");
        assert_ne!(send, restore);
        use crossterm::event::{MouseButton, MouseEventKind};
        recovery.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), send));
        let DashboardAction::NotificationReply {
            prompt: current, ..
        } = recovery.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), send))
        else {
            panic!("Send submits B, not restoration of A");
        };
        assert_eq!(current.payload.text, "draft B");
        recovery.notification_reply_finished(
            "q1",
            &current.command_id,
            Err("B refused too".into()),
        );
        assert_eq!(
            recovery.handle_key(alt(KeyCode::Enter)),
            DashboardAction::None,
            "restore never sends"
        );
        assert_eq!(
            recovery
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text(),
            "refused A"
        );
        assert!(
            recovery
                .quick_reply_mut("q1")
                .recoverable
                .iter()
                .any(|draft| draft.text == "draft B")
        );
        recovery.handle_key(key(KeyCode::Char('!')));
        let DashboardAction::NotificationReply { prompt: second, .. } =
            recovery.handle_key(key(KeyCode::Enter))
        else {
            panic!("send edited A");
        };
        recovery.notification_reply_finished(
            "q1",
            &second.command_id,
            Err("denied edited A".into()),
        );
        assert_eq!(
            recovery
                .quick_reply_mut("q1")
                .recoverable
                .iter()
                .filter(|draft| draft.error.is_some())
                .count(),
            3
        );
        recovery.handle_key(alt(KeyCode::Enter));
        assert_eq!(
            recovery
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text(),
            "draft B"
        );
        recovery.handle_key(alt(KeyCode::Enter));
        let DashboardAction::NotificationReply { prompt: retry, .. } =
            recovery.handle_key(key(KeyCode::Enter))
        else {
            panic!("retry restored A");
        };
        assert_eq!(retry.payload.text, "refused A");
        recovery.notification_reply_finished("q1", &retry.command_id, Ok(()));
        assert!(
            recovery
                .quick_reply_mut("q1")
                .recoverable
                .iter()
                .any(|draft| draft.error.as_deref() == Some("denied edited A"))
        );
        recovery.handle_key(alt(KeyCode::Enter));
        assert_eq!(
            recovery
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text(),
            "refused A!"
        );
        recovery.handle_key(alt(KeyCode::Enter));
        assert_eq!(
            recovery
                .quick_reply_mut("q1")
                .editor
                .single_line_prompt_text(),
            "draft B",
            "restoration also retains displaced unsent drafts"
        );

        let mut dashboard = dashboard();
        dashboard.notification_dismissals.remove("q1");
        // Hard-won: 56747ef: an older daemon's plain-text question entered the form loader.
        assert!(dashboard.notification_form_session().is_none());
        let lines = drawn(&mut dashboard, 120, 44);
        assert!(lines.iter().any(|line| line.contains("Quick reply:")));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("Waiting for the session's form update"))
        );
        assert_eq!(
            dashboard.handle_key(alt(KeyCode::Char('n'))),
            DashboardAction::None
        );
        assert_eq!(
            dashboard.handle_key(key(KeyCode::Enter)),
            DashboardAction::Open {
                session_id: "q1".into()
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
        foreign.handle_key(alt(KeyCode::Char('s')));
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
        crate::test_support::serve_form_card(
            &mut family,
            &parent,
            Some("child-session"),
            &approval,
        );
        assert_eq!(
            family.notification_message(&parent, AttentionLevel::Waiting),
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
            family.handle_key(alt(KeyCode::Char('g'))),
            DashboardAction::Open {
                session_id: "child-session".into()
            }
        );
        assert_eq!(family.focus, Focus::Prompt);
        family.focus = Focus::Notifications;
        let (chat, mut answers) = form_chat("child-session", approval.clone());
        let mut chats = std::collections::BTreeMap::from([("child-session".to_owned(), chat)]);
        // Reading older conversation messages must not steal the card's form input.
        chats
            .get_mut("child-session")
            .unwrap()
            .handle_event_result(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    KeyCode::PageUp,
                    crossterm::event::KeyModifiers::CONTROL,
                ),
            ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut family,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        assert!(family.notifications.form_area.is_some());
        let message = family
            .frame_surfaces
            .surface(mj_chat::selection::SurfaceId::NotificationText(
                family.notifications.text_generation,
            ))
            .unwrap();
        assert!(
            family
                .notifications
                .form_area
                .unwrap()
                .contains(ratatui::layout::Position::new(
                    message.rect.x,
                    message.rect.y
                ))
        );
        let range = mj_chat::selection::SelectionRange {
            start: mj_chat::selection::ContentPos::new(0, 0),
            end: mj_chat::selection::ContentPos::new(
                message.total_rows.saturating_sub(1),
                message.rect.width.saturating_sub(1),
            ),
        };
        assert!(
            chats
                .get_mut("child-session")
                .unwrap()
                .elicitation_selection_text_in(
                    mj_chat::chat::ElicitationSurface::Notification,
                    &range
                )
                .unwrap()
                .contains("path")
        );

        let area = family.notifications_area.unwrap();
        let mut open = DashboardAction::None;
        for kind in [
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        ] {
            let mouse = crossterm::event::MouseEvent {
                kind,
                column: area.x + 2,
                row: area.bottom() - 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            assert!(
                family
                    .handle_notification_form_event(
                        &mut chats,
                        &crossterm::event::Event::Mouse(mouse)
                    )
                    .is_none()
            );
            open = family.handle_mouse(mouse);
        }
        assert_eq!(
            open,
            DashboardAction::Open {
                session_id: "child-session".into()
            }
        );
        family.focus = Focus::Notifications;

        for code in [KeyCode::Tab, KeyCode::BackTab] {
            assert!(
                family
                    .handle_notification_form_event(
                        &mut chats,
                        &crossterm::event::Event::Key(key(code))
                    )
                    .unwrap()
                    .consumed
            );
            assert_eq!(family.focus, Focus::Notifications);
        }

        assert_eq!(
            family.route_bound_key_event(&crossterm::event::Event::Key(key(KeyCode::Enter))),
            crate::KeyRoute::Forward
        );
        assert!(
            family
                .handle_notification_form_event(
                    &mut chats,
                    &crossterm::event::Event::Key(alt(KeyCode::Char('g')))
                )
                .is_none()
        );

        assert!(
            family
                .handle_notification_form_event(
                    &mut chats,
                    &crossterm::event::Event::Paste("Approved".into())
                )
                .unwrap()
                .consumed
        );
        assert!(
            family
                .handle_notification_form_event(
                    &mut chats,
                    &crossterm::event::Event::Key(key(KeyCode::Enter))
                )
                .unwrap()
                .consumed
        );
        let (id, response) =
            tokio::time::timeout(std::time::Duration::from_secs(5), answers.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(id, approval.id);
        assert_eq!(
            response,
            ElicitationResponse::Accept {
                content: [(
                    "path".into(),
                    mj_core::elicitation::ElicitationValue::String("Approved".into())
                )]
                .into_iter()
                .collect(),
            }
        );
        assert!(!chats["child-session"].has_elicitation());
        let (chat, mut rejected) = form_chat_with_error(
            "child-session",
            approval.clone(),
            Some("not permitted".into()),
        );
        chats.insert("child-session".into(), chat);
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut family,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        family.handle_notification_form_event(
            &mut chats,
            &crossterm::event::Event::Paste("Retry me".into()),
        );
        family.handle_notification_form_event(
            &mut chats,
            &crossterm::event::Event::Key(key(KeyCode::Enter)),
        );
        // Hard-won: f69edee: minimizing a notification-only primary answer retired its result owner.
        family.notifications.size = PaneSize::Minimized;
        assert!(family.notification_form_session().is_none());
        assert!(!chats["child-session"].has_elicitation());
        assert!(chats["child-session"].has_pending_elicitation_input());
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            rejected.recv().await.unwrap();
            while !chats["child-session"].has_elicitation() {
                mj_chat::chat::ActiveChat::pump(chats.get_mut("child-session")).await;
            }
        })
        .await
        .unwrap();
        assert!(!chats["child-session"].has_pending_elicitation_input());
        // Retirement after the callback saves drafts; their existing handoff
        // format must carry the rejection when the form is reopened.
        let drafts = serde_json::to_value(chats["child-session"].elicitation_drafts()).unwrap();
        let (mut restored, _) = form_chat("child-session", approval);
        restored.restore_elicitation_drafts(serde_json::from_value(drafts).unwrap());
        chats.insert("child-session".into(), restored);
        family.notifications.size = PaneSize::Maximized;
        terminal
            .draw(|frame| {
                crate::combined::render_combined_for_test(
                    frame,
                    &mut family,
                    &mut chats,
                    &Default::default(),
                    false,
                );
            })
            .unwrap();
        let lines = buffer_lines(terminal.backend().buffer());
        let area = family.notifications_area.unwrap();
        assert!(
            lines[usize::from(area.y)..usize::from(area.bottom())]
                .iter()
                .any(|line| line.contains("Answer was not sent: not permitted"))
        );
    }
}
