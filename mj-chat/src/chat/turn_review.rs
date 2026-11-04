//! Review status and resolution controls in the parent's prompt area.
//!
//! Reviewer conversations use the dashboard's shared virtual workspace. The
//! daemon owns the review; this module only projects its status and actions.
use crate::components::{ButtonRow, ControlKind, Form, Interaction};
use crate::theme;
use crossterm::event::{Event, MouseEvent};
use mj_client::review::RuntimeReviewView;
use mj_core::review::driver::{Resolution, TurnReviewPhase};
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Wrap};

/// Which of the review's actions the keyboard is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewAction {
    ReadDetails,
    Forward,
    Dismiss,
    Cancel,
}

impl ReviewAction {
    const ORDER: [Self; 4] = [
        Self::ReadDetails,
        Self::Forward,
        Self::Dismiss,
        Self::Cancel,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::ReadDetails => "Read details",
            Self::Forward => "Forward findings",
            Self::Dismiss => "Dismiss",
            Self::Cancel => "Cancel",
        }
    }

    /// The resolution this action asks the daemon for.
    const fn resolution(self) -> Option<Resolution> {
        match self {
            Self::ReadDetails => None,
            Self::Forward => Some(Resolution::Forwarded),
            Self::Dismiss => Some(Resolution::Dismissed),
            Self::Cancel => Some(Resolution::Cancelled),
        }
    }

    fn next(self, delta: isize) -> Self {
        let position = Self::ORDER
            .iter()
            .position(|action| *action == self)
            .unwrap_or(0);
        let length = Self::ORDER.len();
        let moved = if delta.is_negative() {
            position.checked_sub(1).unwrap_or(length - 1)
        } else {
            (position + 1) % length
        };
        Self::ORDER[moved]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewControl {
    Status,
    ReadDetails,
    Forward,
    Dismiss,
    Cancel,
}

#[derive(Debug)]
pub(super) struct TurnReview {
    pub(super) view: RuntimeReviewView,
    pub(super) action: Option<ReviewAction>,
    pub(super) failure: Option<String>,
    pub(super) form: Form<ReviewControl>,
}

fn report_sections<'a>(
    view: &'a RuntimeReviewView,
    failure: Option<&'a str>,
) -> impl Iterator<Item = (Option<&'static str>, &'a str)> {
    let delivery_error = match &view.phase {
        TurnReviewPhase::Forwarding { error, .. } => error.as_deref(),
        _ => None,
    };
    [
        (
            None,
            view.verdict.as_ref().map(|verdict| verdict.text.as_str()),
        ),
        (Some("Delivery failed"), delivery_error),
        (Some("Action refused"), failure),
    ]
    .into_iter()
    .filter_map(|(title, text)| {
        text.filter(|text| !text.trim().is_empty())
            .map(|text| (title, text))
    })
}

impl TurnReview {
    pub(super) fn new(view: RuntimeReviewView) -> Self {
        Self {
            action: Self::preferred_action(&view),
            view,
            failure: None,
            form: Form::new(),
        }
    }

    pub(super) fn update(&mut self, view: RuntimeReviewView) -> bool {
        let changed = self.view != view;
        let verdict_changed = self.view.verdict.as_ref().map(|verdict| verdict.kind)
            != view.verdict.as_ref().map(|verdict| verdict.kind);
        if verdict_changed
            || !Self::action_allowed_in_view(&view, self.failure.as_deref(), self.action)
        {
            self.action = Self::preferred_action(&view);
        }
        self.view = view;
        changed
    }

    pub(super) fn status(&self) -> String {
        self.failure.as_ref().unwrap_or(&self.view.status).clone()
    }

    fn preferred_action(view: &RuntimeReviewView) -> Option<ReviewAction> {
        // A running review must not make an accidental Enter a cancellation;
        // explicit arrow navigation or the Cancel button can still pick it.
        view.verdict.as_ref().and_then(|verdict| {
            ReviewAction::ORDER.into_iter().find(|action| {
                action
                    .resolution()
                    .is_some_and(|resolution| verdict.allowed.contains(&resolution))
            })
        })
    }

    fn action_allowed_in_view(
        view: &RuntimeReviewView,
        failure: Option<&str>,
        action: Option<ReviewAction>,
    ) -> bool {
        match action {
            None => view.verdict.is_none(),
            Some(ReviewAction::ReadDetails) => report_sections(view, failure).next().is_some(),
            Some(ReviewAction::Cancel) => {
                !matches!(&view.phase, TurnReviewPhase::Forwarding { error: None, .. })
            }
            Some(action) => view.verdict.as_ref().is_some_and(|verdict| {
                action
                    .resolution()
                    .is_some_and(|resolution| verdict.allowed.contains(&resolution))
            }),
        }
    }

    pub(super) fn pending_elicitations(
        &self,
    ) -> Vec<(String, mj_core::elicitation::ElicitationRequest)> {
        let TurnReviewPhase::Running { .. } = &self.view.phase else {
            return Vec::new();
        };
        self.view
            .questions
            .iter()
            .filter(|question| self.view.owns_reviewer(&question.role))
            .map(|question| (question.role.clone(), question.request.clone()))
            .collect()
    }

    pub(super) fn verdict_text(&self) -> Option<&str> {
        self.view
            .verdict
            .as_ref()
            .and_then(|verdict| (!verdict.text.trim().is_empty()).then_some(verdict.text.as_str()))
    }

    pub(super) fn has_report(&self) -> bool {
        report_sections(&self.view, self.failure.as_deref())
            .next()
            .is_some()
    }

    fn report_text(&self) -> Option<String> {
        let parts = report_sections(&self.view, self.failure.as_deref())
            .map(|(title, text)| {
                title.map_or_else(|| text.to_owned(), |title| format!("{title}:\n{text}"))
            })
            .collect::<Vec<_>>();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn allows(&self, action: ReviewAction) -> bool {
        Self::action_allowed_in_view(&self.view, self.failure.as_deref(), Some(action))
    }

    pub(super) fn report_failure(&mut self, message: impl Into<String>) -> bool {
        let message = message.into();
        if self.failure.as_deref() != Some(message.as_str()) {
            self.failure = Some(message);
            true
        } else {
            false
        }
    }
}

/// A request to the daemon, which owns the review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnReviewIntent {
    Start,
    Resolve(Resolution),
}

impl super::ChatState {
    pub(super) fn turn_review_active(&self) -> bool {
        self.turn_review.is_some()
    }

    pub(super) fn turn_review(&self) -> Option<&TurnReview> {
        self.turn_review.as_deref()
    }

    pub(super) fn turn_review_mut(&mut self) -> Option<&mut TurnReview> {
        self.turn_review.as_deref_mut()
    }

    pub(super) fn turn_review_handles_mouse(&self, column: u16, row: u16) -> bool {
        self.turn_review.as_ref().is_some_and(|review| {
            review.form.captures_pointer() || review.form.contains(column, row)
        })
    }

    pub(super) fn cancel_turn_review_pointer(&mut self) {
        if let Some(review) = self.turn_review.as_mut() {
            review.form.cancel_pointer();
        }
    }

    pub(super) fn reset_turn_review_geometry(&mut self) {
        if let Some(review) = self.turn_review.as_mut() {
            review.form.reset_geometry();
        }
    }

    pub(super) fn set_turn_review(&mut self, view: Option<RuntimeReviewView>) {
        match (view, self.turn_review.as_mut()) {
            (Some(view), Some(open)) => {
                open.update(view);
            }
            (Some(view), None) => {
                self.turn_review = Some(Box::new(TurnReview::new(view)));
            }
            (None, _) => self.turn_review = None,
        }
    }

    fn activate_turn_review_action(&mut self) -> super::ChatAction {
        let Some(review) = self.turn_review.as_ref() else {
            return super::ChatAction::None;
        };
        let Some(action) = review.action else {
            return super::ChatAction::None;
        };
        // Actions wait for the daemon's typed phase. In particular, an
        // in-flight handoff cannot be cancelled or submitted a second time.
        if !review.allows(action) {
            return super::ChatAction::None;
        }
        if let Some(resolution) = action.resolution() {
            self.resolve_turn_review(resolution)
        } else {
            let text = review.report_text().unwrap_or_default();
            self.open_text_reader("Review details", &text);
            super::ChatAction::None
        }
    }

    fn resolve_turn_review(&mut self, resolution: Resolution) -> super::ChatAction {
        if self.turn_review.is_none() {
            return super::ChatAction::None;
        }
        if matches!(
            self.turn_review.as_ref().map(|review| &review.view.phase),
            Some(&TurnReviewPhase::Forwarding { error: None, .. })
        ) {
            return super::ChatAction::None;
        }
        // The prompt stays held until the daemon says the review is gone: the
        // terminal is a projection, and pretending otherwise would flash the
        // composer back for a moment when the request fails.
        super::ChatAction::TurnReview(TurnReviewIntent::Resolve(resolution))
    }

    pub(super) fn handle_turn_review_mouse(
        &mut self,
        mouse: MouseEvent,
    ) -> (bool, super::ChatAction) {
        let result = match self.turn_review.as_mut() {
            Some(review) => review.form.handle(&Event::Mouse(mouse)),
            None => return (false, super::ChatAction::None),
        };
        if let Some(Interaction::Activate(control)) = result.action {
            if let Some(review) = self.turn_review.as_mut() {
                review.action = Some(match control {
                    ReviewControl::ReadDetails => ReviewAction::ReadDetails,
                    ReviewControl::Forward => ReviewAction::Forward,
                    ReviewControl::Dismiss => ReviewAction::Dismiss,
                    ReviewControl::Cancel => ReviewAction::Cancel,
                    ReviewControl::Status => return (true, super::ChatAction::None),
                });
            }
            return (true, self.activate_turn_review_action());
        }
        (result.consumed, super::ChatAction::None)
    }

    pub(super) fn handle_turn_review_event(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> super::ChatAction {
        use super::ChatAction;
        use crossterm::event::{KeyCode, KeyModifiers};
        let (code, modifiers) = super::normalize_key(key.code, key.modifiers);
        match code {
            KeyCode::PageUp => {
                self.scroll_history_up(self.last_viewport_height.max(1));
            }
            KeyCode::PageDown => {
                self.scroll_history_down(self.last_viewport_height.max(1));
            }
            KeyCode::Right | KeyCode::Left => {
                if let Some(review) = self.turn_review.as_mut() {
                    review.action = Some(match review.action {
                        Some(action) => action.next(if code == KeyCode::Left { -1 } else { 1 }),
                        None if code == KeyCode::Left => ReviewAction::Cancel,
                        None => ReviewAction::Forward,
                    });
                }
            }
            KeyCode::Char('v') if modifiers.is_empty() => {
                if let Some(text) = self.turn_review().and_then(TurnReview::report_text) {
                    self.open_text_reader("Review details", &text);
                }
            }
            KeyCode::Enter => return self.activate_turn_review_action(),
            KeyCode::Tab | KeyCode::BackTab => {
                return ChatAction::CycleFocus {
                    reverse: code == KeyCode::BackTab,
                };
            }
            KeyCode::Esc => return self.resolve_turn_review(Resolution::Cancelled),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                return self.resolve_turn_review(Resolution::Cancelled);
            }
            _ => {}
        }
        ChatAction::None
    }
}

pub(super) fn render_turn_review_actions(
    frame: &mut ratatui::Frame,
    area: Rect,
    review: &mut TurnReview,
) {
    review.form.begin_frame();
    // Status is the neutral stop: rendering cannot select Cancel on its own.
    review
        .form
        .declare(ReviewControl::Status, ControlKind::Button);
    let failed_delivery = matches!(
        review.view.phase,
        TurnReviewPhase::Forwarding { error: Some(_), .. }
    );
    let specs = ReviewAction::ORDER
        .into_iter()
        .filter(|candidate| *candidate != ReviewAction::ReadDetails || review.has_report())
        .map(|candidate| {
            let label = match (failed_delivery, candidate) {
                (true, ReviewAction::Forward) => "Retry delivery",
                (true, ReviewAction::Cancel) => "Stop retrying",
                _ => candidate.label(),
            };
            let control = match candidate {
                ReviewAction::ReadDetails => ReviewControl::ReadDetails,
                ReviewAction::Forward => ReviewControl::Forward,
                ReviewAction::Dismiss => ReviewControl::Dismiss,
                ReviewAction::Cancel => ReviewControl::Cancel,
            };
            (control, label, review.allows(candidate))
        })
        .collect::<Vec<_>>();
    review.form.focus(match review.action {
        Some(ReviewAction::ReadDetails) => ReviewControl::ReadDetails,
        Some(ReviewAction::Forward) => ReviewControl::Forward,
        Some(ReviewAction::Dismiss) => ReviewControl::Dismiss,
        Some(ReviewAction::Cancel) => ReviewControl::Cancel,
        None => ReviewControl::Status,
    });
    ButtonRow::render(
        frame,
        Rect::new(area.x, area.y, area.width, area.height.min(1)),
        &specs,
        &mut review.form,
    );
    if let Some(text) = review.verdict_text() {
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(theme::muted()),
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(1),
                area.width.saturating_sub(2),
                area.height.saturating_sub(1),
            ),
        );
    }
    review.form.end_frame(ReviewControl::Status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatAction;
    use crate::chat::test_support::{key, snapshot};
    use crossterm::event::KeyCode;
    use mj_client::review::{VerdictKind, VerdictView};
    use mj_core::review::driver::{RoleState, RoleStatus};
    use mj_core::review::verdict::ReviewPassEvidence;

    fn chat() -> super::super::ChatState {
        super::super::ChatState::new(&snapshot(), &[])
    }

    fn running_view() -> RuntimeReviewView {
        RuntimeReviewView {
            child_session_id: None,
            session_id: "1234567890".to_owned(),
            questions: Vec::new(),
            phase: TurnReviewPhase::Running {
                roles: vec![RoleStatus {
                    role: "reviewer".to_owned(),
                    label: "Reviewer".to_owned(),
                    state: RoleState::Running,
                    child_session_id: Some("reviewer".to_owned()),
                }],
            },
            roles: vec![RoleStatus {
                role: "reviewer".to_owned(),
                label: "Reviewer".to_owned(),
                state: RoleState::Running,
                child_session_id: Some("reviewer".to_owned()),
            }],
            status: "the reviewer is reading the change…".to_owned(),
            verdict: None,
        }
    }

    fn findings_view() -> RuntimeReviewView {
        RuntimeReviewView {
            status: "Review findings".into(),
            verdict: Some(VerdictView {
                kind: VerdictKind::Findings,
                text: "[P1] src/lib.rs:1 -- unbounded retry".to_owned(),
                allowed: vec![
                    Resolution::Forwarded,
                    Resolution::Dismissed,
                    Resolution::Cancelled,
                ],
            }),
            ..running_view()
        }
    }

    fn forwarding_view(error: Option<&str>) -> RuntimeReviewView {
        let synthesis = "[P1] src/lib.rs:1 -- unbounded retry".to_owned();
        RuntimeReviewView {
            phase: TurnReviewPhase::Forwarding {
                synthesis: synthesis.clone(),
                evidence: ReviewPassEvidence::default(),
                command_id: "turn-review-forward-1".to_owned(),
                error: error.map(str::to_owned),
            },
            status: error.map_or_else(
                || "sending findings to the primary agent…".to_owned(),
                |error| {
                    format!(
                        "could not send findings to the primary agent: {error} · Retry or cancel"
                    )
                },
            ),
            verdict: Some(VerdictView {
                kind: VerdictKind::Findings,
                text: synthesis,
                allowed: if error.is_some() {
                    vec![Resolution::Forwarded, Resolution::Cancelled]
                } else {
                    Vec::new()
                },
            }),
            ..running_view()
        }
    }

    fn review_golden_buffer(
        chat: &mut super::super::ChatState,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| crate::chat::active::render_full_frame(frame, chat, false))
            .expect("draw turn review");
        terminal.backend().buffer().clone()
    }

    fn append_review_golden_state(
        output: &mut String,
        label: &str,
        width: u16,
        height: u16,
        buffer: &ratatui::buffer::Buffer,
        details: &[String],
    ) {
        use std::fmt::Write as _;

        if !output.is_empty() {
            output.push('\n');
        }
        writeln!(output, "=== {label} ({width}x{height}) ===").expect("write state header");
        output.push_str(&crate::golden::buffer_lines(buffer).join("\n"));
        output.push('\n');
        for detail in details {
            writeln!(output, "{detail}").expect("write state detail");
        }
    }

    #[test]
    fn changing_the_reviewer_child_discards_the_previous_childs_pending_form() {
        let mut view = running_view();
        view.roles[0].child_session_id = Some("old-reviewer".into());
        view.questions.push(mj_client::review::ReviewerQuestion {
            role: "old-reviewer".into(),
            request: mj_core::elicitation::ElicitationRequest {
                id: "permission-1".into(),
                message: "Allow this tool?".into(),
                title: None,
                description: None,
                fields: Vec::new(),
                tool_permission: None,
            },
        });
        let mut review = TurnReview::new(view.clone());
        assert_eq!(review.pending_elicitations()[0].0, "old-reviewer");
        view.roles[0].child_session_id = Some("new-reviewer".into());
        view.questions.clear();
        review.update(view);
        assert!(
            review.pending_elicitations().is_empty(),
            "a reused elicitation id must not be answered on the replacement child"
        );
    }

    #[test]
    fn questions_from_parallel_reviewers_keep_their_child_owners() {
        let mut view = running_view();
        let mut second = view.roles[0].clone();
        second.role = "reviewer-2".into();
        second.child_session_id = Some("child-2".into());
        view.roles.push(second);
        let request = mj_core::elicitation::ElicitationRequest {
            id: "permission-1".into(),
            message: "Allow this tool?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
            tool_permission: None,
        };
        view.questions = ["reviewer", "child-2", "stale-child"]
            .into_iter()
            .map(|child| mj_client::review::ReviewerQuestion {
                role: child.into(),
                request: request.clone(),
            })
            .collect();
        let review = TurnReview::new(view);
        assert_eq!(
            review
                .pending_elicitations()
                .into_iter()
                .map(|(child, _)| child)
                .collect::<Vec<_>>(),
            vec!["reviewer", "child-2"]
        );
    }

    #[test]
    fn submission_refused_while_review_unresolved() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        // Typed input never reaches the composer: the review owns the keyboard
        // while it is up.
        assert_eq!(chat.handle_key(key(KeyCode::Char('h'))), ChatAction::None);
        assert!(chat.input.is_empty(), "the composer takes no input");

        chat.input = "next".to_owned();
        chat.input_cursor = 4;
        assert_eq!(chat.submit_input(), ChatAction::None);
        assert!(
            chat.notice()
                .is_some_and(|notice| notice.contains("review of the last turn is open")),
            "the refusal says why: {:?}",
            chat.notice()
        );
    }

    #[test]
    fn escape_asks_the_daemon_to_cancel_and_leaves_the_review_open() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert_eq!(
            chat.handle_key(key(KeyCode::Esc)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Cancelled))
        );
        assert!(
            chat.turn_review_active(),
            "the prompt returns when the daemon says the review is gone, not before"
        );
    }

    #[test]
    fn enter_is_harmless_until_a_verdict_arrives() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        let _ = review_golden_buffer(&mut chat, 80, 16);
        // Repeated Enter presses do not turn the absence of a selected action
        // into a cancellation request.
        assert_eq!(chat.turn_review().map(|review| review.action), Some(None));
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    }

    #[test]
    fn an_inflight_forward_has_no_retry_or_cancel_action() {
        let mut chat = chat();
        chat.set_turn_review(Some(forwarding_view(None)));
        let review = chat.turn_review().expect("handoff remains visible");
        assert_eq!(review.action, None);
        assert!(review.status().contains("sending findings"));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::None,
            "repeated Enter cannot submit the same handoff twice"
        );
        assert_eq!(
            chat.handle_key(key(KeyCode::Esc)),
            ChatAction::None,
            "an ambiguous handoff cannot be cancelled before acknowledgement"
        );
    }

    #[test]
    fn a_rejected_forward_keeps_findings_and_selects_retry() {
        let mut chat = chat();
        chat.set_turn_review(Some(forwarding_view(Some("primary rejected it"))));
        let review = chat.turn_review().expect("failed handoff remains visible");
        assert_eq!(review.action, Some(ReviewAction::Forward));
        assert!(
            review
                .verdict_text()
                .is_some_and(|text| text.contains("unbounded retry"))
        );
        assert!(review.status().contains("primary rejected it"));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Forwarded))
        );
    }

    #[test]
    fn a_new_running_snapshot_resets_the_previous_verdict_selection() {
        let mut chat = chat();
        chat.set_turn_review(Some(findings_view()));
        // A snapshot feed may coalesce the closed view and the next start.
        chat.set_turn_review(Some(running_view()));
        assert_eq!(chat.turn_review().and_then(|review| review.action), None);
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    }

    #[test]
    fn the_daemon_taking_the_review_away_clears_the_prompt_controls() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert!(chat.turn_review_active());
        chat.set_turn_review(None);
        assert!(!chat.turn_review_active());
    }

    #[test]
    fn reviewer_form_is_answered_back_to_the_reviewer() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        let form = mj_core::elicitation::ElicitationRequest {
            id: "reviewer-form-1".into(),
            message: "Allow reading /etc?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
            tool_permission: None,
        };
        assert!(chat.show_review_role_elicitation(Some("reviewer".to_owned()), form));
        assert!(chat.reviewer_elicitation_open());

        // The dialog takes the key, not the review's action bar: Escape here
        // answers the harness rather than cancelling the review out from under
        // it.
        let action = chat.handle_key(key(KeyCode::Esc));
        let ChatAction::RespondReviewerElicitation {
            role,
            elicitation_id,
            ..
        } = action
        else {
            panic!("a reviewing harness's answer goes back to it: {action:?}");
        };
        assert_eq!(role.as_deref(), Some("reviewer"));
        assert_eq!(elicitation_id, "reviewer-form-1");
        assert!(
            chat.turn_review_active(),
            "answering a form does not end the review"
        );
    }

    #[test]
    fn golden_review_prompt_controls() {
        fn fresh_chat() -> super::super::ChatState {
            let mut chat = chat();
            chat.set_reviewer_count(1);
            chat
        }

        let mut output = String::new();

        let mut running = fresh_chat();
        running.set_turn_review(Some(running_view()));
        let buffer = review_golden_buffer(&mut running, 80, 16);
        let primary_width = running
            .frame_surfaces
            .surface(crate::selection::SurfaceId::Transcript)
            .map(|surface| surface.rect.width);
        let reviewer = running
            .reviewer_control_area
            .expect("reviewer button on prompt border");
        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: reviewer.x,
            row: reviewer.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let captures_pointer = running.component_handles_mouse(mouse);
        let clicked = running.handle_mouse(mouse);
        running.handle_key(key(KeyCode::Down));
        let keyboard = running.handle_key(key(KeyCode::Enter));
        append_review_golden_state(
            &mut output,
            "running review opens virtual workspace",
            80,
            16,
            &buffer,
            &[
                format!("primary width: {primary_width:?}"),
                format!("captures pointer: {captures_pointer}"),
                format!("click: {clicked:?}"),
                format!("keyboard: {keyboard:?}"),
            ],
        );

        running.handle_key(key(KeyCode::Up));
        running.entries = (0..30)
            .map(|index| {
                mj_core::transcript::ChatEntry::plain(
                    index,
                    mj_core::transcript::ChatRole::Agent,
                    format!("parent transcript message {index}"),
                )
            })
            .collect();
        let before_page_up = review_golden_buffer(&mut running, 80, 16);
        running.handle_key(key(KeyCode::PageUp));
        let paged_up = review_golden_buffer(&mut running, 80, 16);
        assert_ne!(before_page_up, paged_up, "PageUp moves the visible parent");
        // Hard-won: 298330b19: page keys scrolled a hidden review pane while the parent was visible.
        append_review_golden_state(
            &mut output,
            "running review PageUp scrolls the parent",
            80,
            16,
            &paged_up,
            &[],
        );
        running.handle_key(key(KeyCode::PageDown));
        let paged_down = review_golden_buffer(&mut running, 80, 16);
        assert_eq!(before_page_up, paged_down, "PageDown returns to the bottom");
        append_review_golden_state(
            &mut output,
            "running review PageDown returns to the parent tail",
            80,
            16,
            &paged_down,
            &[],
        );

        running.set_turn_review(Some(findings_view()));
        let verdict_tail = review_golden_buffer(&mut running, 80, 16);
        running.handle_key(key(KeyCode::PageUp));
        let verdict_paged = review_golden_buffer(&mut running, 80, 16);
        assert_ne!(
            verdict_tail, verdict_paged,
            "findings keep the parent scrollable"
        );
        append_review_golden_state(
            &mut output,
            "findings keep the parent transcript visible",
            80,
            16,
            &verdict_paged,
            &[],
        );
        running.handle_key(key(KeyCode::PageDown));
        assert_eq!(verdict_tail, review_golden_buffer(&mut running, 80, 16));
        assert_eq!(
            running.handle_key(key(KeyCode::Tab)),
            ChatAction::CycleFocus { reverse: false }
        );
        assert_eq!(
            running.handle_key(key(KeyCode::BackTab)),
            ChatAction::CycleFocus { reverse: true }
        );

        let mut chat = fresh_chat();
        chat.set_turn_review(Some(findings_view()));
        let selected = chat.turn_review().map(|review| review.action);
        let action = chat.handle_key(key(KeyCode::Enter));
        append_review_golden_state(
            &mut output,
            "findings default action",
            64,
            14,
            &review_golden_buffer(&mut chat, 64, 14),
            &[
                format!("selected action: {selected:?}"),
                format!("action: {action:?}"),
            ],
        );

        let mut chat = fresh_chat();
        chat.set_turn_review(Some(RuntimeReviewView {
            status: "Review failed".into(),
            verdict: Some(VerdictView {
                kind: VerdictKind::Failed,
                text: "reviewer exited with 1".to_owned(),
                allowed: vec![Resolution::Dismissed, Resolution::Cancelled],
            }),
            ..running_view()
        }));
        let action = chat.handle_key(key(KeyCode::Enter));
        append_review_golden_state(
            &mut output,
            "failed verdict allowed actions",
            64,
            14,
            &review_golden_buffer(&mut chat, 64, 14),
            &[
                format!(
                    "selected action: {:?}",
                    chat.turn_review().map(|review| review.action)
                ),
                format!(
                    "allowed actions: {:?}",
                    chat.turn_review().map(|review| {
                        [
                            ReviewAction::Forward,
                            ReviewAction::Dismiss,
                            ReviewAction::Cancel,
                        ]
                        .map(|action| (action, review.allows(action)))
                    })
                ),
                format!("action: {action:?}"),
            ],
        );

        let mut long = fresh_chat();
        let mut view = findings_view();
        view.verdict.as_mut().unwrap().text = (0..40)
            .map(|index| format!("complete verdict line {index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        long.set_turn_review(Some(view));
        long.input = "unsent draft".into();
        let _ = review_golden_buffer(&mut long, 80, 16);
        long.handle_key(key(KeyCode::Char('v')));
        // Hard-won: 86b6980ed: multi-line verdicts were clipped in the prompt with no full-text reader.
        append_review_golden_state(
            &mut output,
            "full verdict uses the existing text reader",
            80,
            16,
            &review_golden_buffer(&mut long, 80, 16),
            &[],
        );
        for _ in 0..4 {
            long.handle_key(key(KeyCode::PageDown));
        }
        let bottom = review_golden_buffer(&mut long, 80, 16);
        assert!(
            crate::golden::buffer_lines(&bottom)
                .join("\n")
                .contains("complete verdict line 39")
        );
        append_review_golden_state(
            &mut output,
            "full verdict reader reaches the final line",
            80,
            16,
            &bottom,
            &[],
        );
        assert_eq!(long.handle_key(key(KeyCode::Esc)), ChatAction::None);
        assert!(long.earlier.is_none());
        assert!(long.turn_review_active());
        assert_eq!(long.input, "unsent draft");
        assert_eq!(
            long.handle_key(key(KeyCode::Enter)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Forwarded))
        );

        let error = format!("{}\ncomplete error reason at the end", "The remote worker refused this request because its control connection is unavailable. ".repeat(18));
        for (label, view, refusal) in [
            (
                "delivery error is fully readable",
                forwarding_view(Some(&error)),
                None,
            ),
            (
                "action refusal is readable without a verdict",
                running_view(),
                Some(error.clone()),
            ),
        ] {
            let mut chat = fresh_chat();
            chat.set_turn_review(Some(view));
            if let Some(refusal) = refusal {
                chat.turn_review_mut().unwrap().report_failure(refusal);
            }
            let _ = review_golden_buffer(&mut chat, 64, 14);
            assert!(
                chat.turn_review()
                    .unwrap()
                    .allows(ReviewAction::ReadDetails)
            );
            chat.handle_key(key(KeyCode::Char('v')));
            assert!(chat.earlier.is_some());
            for _ in 0..4 {
                chat.handle_key(key(KeyCode::PageDown));
            }
            let buffer = review_golden_buffer(&mut chat, 64, 14);
            assert!(
                crate::golden::buffer_lines(&buffer)
                    .join("\n")
                    .contains("complete error reason at the end")
            );
            // Hard-won: eeb875f7b: delivery and action errors were only visible in a truncated prompt title.
            append_review_golden_state(&mut output, label, 64, 14, &buffer, &[]);
            assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);
            assert!(chat.turn_review_active());
        }

        mj_core::golden::assert_platform_golden(
            env!("CARGO_MANIFEST_DIR"),
            "review-prompt-controls",
            &output,
        );
    }
}
