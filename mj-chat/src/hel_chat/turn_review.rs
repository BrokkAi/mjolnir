//! The chat's turn-review view: the split pane a completed turn is reviewed in.
//!
//! This is the sibling of `second_opinion.rs`. Both put a reviewer's
//! conversation beside the primary's and replace the composer with a row of
//! actions; they differ in what starts them and what the actions mean. Plan
//! review starts at a plan-approval decision mid-turn and ends by transferring
//! a critique. Turn review starts when a turn *finishes* and ends by forwarding
//! validated findings, dismissing them, or cancelling.
//!
//! The review itself runs in the controller daemon
//! (`hel::hel_review::host`), not here. This module is a projection of it:
//! the terminal renders the [`RuntimeReviewView`] the daemon publishes and
//! sends back resolutions, exactly as the phone does. That is why closing the
//! terminal no longer ends a review, and why a session nobody is watching is
//! reviewed too.
//!
//! The view still owns the screen while it is up, which is the point of a
//! synchronous review: findings can never land out of the blue in the middle
//! of the next conversation.

use std::collections::{BTreeMap, BTreeSet};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use hel::hel_review::driver::{Resolution, RoleState};
use mj_controller::hel_review_host::{RuntimeReviewView, VerdictKind};

use super::second_opinion::ReviewerPane;

/// Which of the review's actions the keyboard is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewAction {
    Forward,
    Dismiss,
    Cancel,
}

impl ReviewAction {
    const ORDER: [Self; 3] = [Self::Forward, Self::Dismiss, Self::Cancel];

    fn label(self) -> &'static str {
        match self {
            Self::Forward => "Forward findings",
            Self::Dismiss => "Dismiss",
            Self::Cancel => "Cancel",
        }
    }

    /// The resolution this action asks the daemon for.
    const fn resolution(self) -> Resolution {
        match self {
            Self::Forward => Resolution::Forwarded,
            Self::Dismiss => Resolution::Dismissed,
            Self::Cancel => Resolution::Cancelled,
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

/// One turn review on screen: what the daemon published, plus where the
/// reader is looking.
pub(super) struct TurnReview {
    /// The daemon's latest word on this review. Replaced wholesale on every
    /// snapshot, so nothing here can drift from what is actually running.
    pub(super) view: RuntimeReviewView,
    /// One pane per reviewing role: the extended tier runs a supervisor, an
    /// intent analyst and several lanes at once, and each has its own journal.
    panes: BTreeMap<String, ReviewerPane>,
    /// Which role's transcript the pane is showing. Tab cycles it.
    selected: String,
    pub(super) action: ReviewAction,
    /// A refusal from the daemon, shown in place rather than in a dialog that
    /// would take the review off screen with it.
    pub(super) failure: Option<String>,
}

impl std::fmt::Debug for TurnReview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnReview")
            .field("phase", &self.view.phase)
            .field("action", &self.action)
            .finish()
    }
}

impl TurnReview {
    #[must_use]
    pub(super) fn new(view: RuntimeReviewView) -> Self {
        Self {
            selected: view
                .roles
                .first()
                .map(|role| role.role.clone())
                .unwrap_or_else(|| hel::hel_review::driver::REVIEWER_ROLE.to_owned()),
            view,
            panes: BTreeMap::new(),
            action: ReviewAction::Forward,
            failure: None,
        }
    }

    /// Takes the daemon's newer view of the same review.
    pub(super) fn update(&mut self, view: RuntimeReviewView) {
        if !view.roles.iter().any(|role| role.role == self.selected)
            && let Some(first) = view.roles.first()
        {
            // The reader was watching a role this review no longer lists.
            self.selected = first.role.clone();
        }
        self.view = view;
    }

    /// One role's pane, created on first use.
    pub(super) fn pane(&mut self, role: &str) -> &mut ReviewerPane {
        self.panes.entry(role.to_owned()).or_default()
    }

    /// The pane on screen. A review that has not produced a transcript yet
    /// still needs somewhere to render its status line.
    pub(super) fn selected_pane(&mut self) -> &mut ReviewerPane {
        let selected = self.selected.clone();
        self.panes.entry(selected).or_default()
    }

    #[must_use]
    pub(super) fn selected_role(&self) -> &str {
        &self.selected
    }

    /// Moves the transcript to the next role that has one, so a reader can
    /// follow a lane without losing the supervisor.
    pub(super) fn cycle_selection(&mut self) {
        if self.view.roles.is_empty() {
            return;
        }
        let next = self
            .view
            .roles
            .iter()
            .position(|role| role.role == self.selected)
            .map(|position| (position + 1) % self.view.roles.len())
            .unwrap_or(0);
        self.selected = self.view.roles[next].role.clone();
    }

    /// Where a role's journal has been read to.
    #[must_use]
    pub(super) fn cursor(&self, role: &str) -> (u64, String) {
        self.panes
            .get(role)
            .map(|pane| (pane.cursor_ordinal, pane.cursor_digest.clone()))
            .unwrap_or((0, String::new()))
    }

    /// Forms any reviewing role's harness is waiting on, each paired with the
    /// role that asked, because that is where the answer has to go.
    #[must_use]
    pub(super) fn pending_elicitations(
        &self,
    ) -> Vec<(String, hel::hel_elicitation::ElicitationRequest)> {
        let active_roles = self
            .view
            .roles
            .iter()
            .map(|role| role.role.as_str())
            .collect::<BTreeSet<_>>();
        self.panes
            .iter()
            .filter(|(role, _)| active_roles.contains(role.as_str()))
            .flat_map(|(role, pane)| {
                pane.pending_elicitations()
                    .iter()
                    .map(|request| (role.clone(), request.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// What the pane's status line says right now.
    #[must_use]
    pub(super) fn status(&self) -> String {
        if let Some(failure) = &self.failure {
            return failure.clone();
        }
        match self.verdict_kind() {
            Some(VerdictKind::Findings | VerdictKind::Failed) => {
                "Enter to act · Tab switches agent".to_owned()
            }
            _ => self.view.status.clone(),
        }
    }

    #[must_use]
    fn verdict_kind(&self) -> Option<VerdictKind> {
        self.view.verdict.as_ref().map(|verdict| verdict.kind)
    }

    /// Whether an action is available, which the daemon decides and publishes.
    #[must_use]
    fn allows(&self, action: ReviewAction) -> bool {
        self.view
            .verdict
            .as_ref()
            .is_some_and(|verdict| verdict.allowed.contains(&action.resolution()))
    }

    pub(super) fn report_failure(&mut self, message: impl Into<String>) {
        self.failure = Some(message.into());
    }
}

/// What the turn-review view asked the session to do. Every variant is a
/// request to the daemon: the terminal hosts no part of a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnReviewIntent {
    /// Review the turn that just finished.
    Start,
    /// Forward the findings, dismiss them, or cancel the review.
    Resolve(Resolution),
}

impl super::ChatState {
    /// Whether the turn-review view owns the screen. While it does, the
    /// composer is not accepting prompts for the primary agent.
    pub(super) fn turn_review_active(&self) -> bool {
        self.turn_review.is_some()
    }

    /// Whether the split is up, which is when the transcript shares the frame.
    pub(super) fn turn_review_split(&self) -> bool {
        self.turn_review.is_some()
    }

    pub(super) fn turn_review(&self) -> Option<&TurnReview> {
        self.turn_review.as_deref()
    }

    pub(super) fn turn_review_mut(&mut self) -> Option<&mut TurnReview> {
        self.turn_review.as_deref_mut()
    }

    /// Shows the daemon's review, or takes the pane down when it has resolved.
    pub(super) fn set_turn_review(&mut self, view: Option<RuntimeReviewView>) {
        match (view, self.turn_review.as_mut()) {
            (Some(view), Some(open)) => open.update(view),
            (Some(view), None) => self.turn_review = Some(Box::new(TurnReview::new(view))),
            (None, _) => self.close_turn_review(),
        }
    }

    pub(super) fn close_turn_review(&mut self) {
        self.turn_review = None;
        self.turn_review_action_areas.clear();
    }

    /// Drives the turn-review view from one key press.
    pub(super) fn handle_turn_review_key(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> super::ChatAction {
        use crossterm::event::{KeyCode, KeyModifiers};

        let Some(review) = self.turn_review.as_mut() else {
            return super::ChatAction::None;
        };
        match code {
            // Tab moves between the reviewing agents; the arrows move between
            // the actions, so a fan-out stays readable without giving up the
            // one-key Forward.
            KeyCode::Tab => {
                review.cycle_selection();
                super::ChatAction::None
            }
            KeyCode::Right => {
                review.action = review.action.next(1);
                super::ChatAction::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                review.action = review.action.next(-1);
                super::ChatAction::None
            }
            KeyCode::PageUp => {
                let page = self.last_viewport_height.max(1);
                review.selected_pane().scroll_by(-(page as isize), page);
                super::ChatAction::None
            }
            KeyCode::PageDown => {
                let page = self.last_viewport_height.max(1);
                review.selected_pane().scroll_by(page as isize, page);
                super::ChatAction::None
            }
            KeyCode::Enter => self.activate_turn_review_action(),
            // Escape cancels at every stage before the review resolves, which
            // is what keeps the composer one keypress away.
            KeyCode::Esc => self.resolve_turn_review(Resolution::Cancelled),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.resolve_turn_review(Resolution::Cancelled)
            }
            _ => super::ChatAction::None,
        }
    }

    fn activate_turn_review_action(&mut self) -> super::ChatAction {
        let Some(review) = self.turn_review.as_ref() else {
            return super::ChatAction::None;
        };
        let action = review.action;
        // Cancel is always available; the rest wait for the verdict the daemon
        // publishes, so pressing one early does nothing.
        if action != ReviewAction::Cancel && !review.allows(action) {
            return super::ChatAction::None;
        }
        self.resolve_turn_review(action.resolution())
    }

    fn resolve_turn_review(&mut self, resolution: Resolution) -> super::ChatAction {
        if self.turn_review.is_none() {
            return super::ChatAction::None;
        }
        // The pane stays up until the daemon says the review is gone: the
        // terminal is a projection, and pretending otherwise would flash the
        // composer back for a moment when the request fails.
        super::ChatAction::TurnReview(TurnReviewIntent::Resolve(resolution))
    }

    /// Activates the review action under the pointer, if any.
    pub(super) fn click_turn_review_action(&mut self, column: u16, row: u16) -> super::ChatAction {
        let Some(action) = self
            .turn_review_action_areas
            .iter()
            .find(|(_, area)| area.contains(ratatui::layout::Position::new(column, row)))
            .map(|(action, _)| *action)
        else {
            return super::ChatAction::None;
        };
        let Some(review) = self.turn_review.as_mut() else {
            return super::ChatAction::None;
        };
        review.action = action;
        self.activate_turn_review_action()
    }

    /// Scrolls the reviewer pane of an open turn review.
    pub(super) fn scroll_turn_review(&mut self, rows: isize) -> bool {
        let height = self.last_viewport_height.max(1);
        let Some(review) = self.turn_review.as_mut() else {
            return false;
        };
        review.selected_pane().scroll_by(rows, height);
        true
    }
}

/// Draws the review's action bar and reports where each button landed, so a
/// click picks the same action the keyboard would.
pub(super) fn render_turn_review_actions(
    frame: &mut ratatui::Frame,
    area: Rect,
    review: &TurnReview,
    status: &str,
) -> Vec<(ReviewAction, Rect)> {
    let mut spans = Vec::new();
    let mut buttons = Vec::new();
    let mut column = area.x;
    for candidate in ReviewAction::ORDER {
        let available = candidate == ReviewAction::Cancel || review.allows(candidate);
        let mut style = Style::default();
        if !available {
            style = style.fg(Color::DarkGray);
        }
        if candidate == review.action {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let label = format!(" {} ", candidate.label());
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        if column < area.right() {
            buttons.push((
                candidate,
                Rect::new(column, area.y, width.min(area.right() - column), 1),
            ));
        }
        column = column.saturating_add(width).saturating_add(2);
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(
        status.to_owned(),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    buttons
}

/// The one-line strip above the reviewer transcript: which reviewing agents
/// this review is running and where each has got to.
#[must_use]
pub(super) fn role_strip(review: &TurnReview) -> Option<Line<'static>> {
    if review.view.roles.is_empty() {
        return None;
    }
    let mut spans = Vec::new();
    for (index, role) in review.view.roles.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let color = match role.state {
            RoleState::Pending => Color::DarkGray,
            RoleState::Running => Color::Yellow,
            RoleState::Clean => Color::Green,
            RoleState::Findings => Color::LightMagenta,
            RoleState::Failed => Color::Red,
        };
        let mut style = Style::default().fg(color);
        if role.role == review.selected_role() {
            // The strip is also the tab bar: the highlighted row is the
            // transcript below it.
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(
            format!("{} {}", role.label, role.state.label()),
            style,
        ));
    }
    Some(Line::from(spans))
}

/// What the reviewer pane's title says while a verdict is up.
#[must_use]
pub(super) fn verdict_title(review: Option<&TurnReview>) -> &'static str {
    match review.and_then(TurnReview::verdict_kind) {
        Some(VerdictKind::Clean) => " Turn review · clean ",
        Some(VerdictKind::Findings) => " Turn review · findings ",
        Some(VerdictKind::Failed) => " Turn review · failed ",
        None => " Turn review ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_chat::ChatAction;
    use crate::hel_chat::test_support::{key, snapshot};
    use crossterm::event::KeyCode;
    use hel::hel_review::driver::{RoleStatus, TurnReviewPhase};
    use hel::hel_review::lanes::ReviewTier;
    use mj_controller::hel_review_host::VerdictView;

    fn chat() -> super::super::ChatState {
        super::super::ChatState::new(&snapshot(), &[])
    }

    fn running_view() -> RuntimeReviewView {
        RuntimeReviewView {
            session_id: "1234567890".to_owned(),
            tier: ReviewTier::Quick,
            phase: TurnReviewPhase::Running {
                roles: vec![RoleStatus {
                    role: "reviewer".to_owned(),
                    label: "General".to_owned(),
                    state: RoleState::Running,
                }],
            },
            roles: vec![RoleStatus {
                role: "reviewer".to_owned(),
                label: "General".to_owned(),
                state: RoleState::Running,
            }],
            status: "the reviewer is reading the change…".to_owned(),
            verdict: None,
        }
    }

    fn findings_view() -> RuntimeReviewView {
        RuntimeReviewView {
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
    fn escape_asks_the_daemon_to_cancel_and_leaves_the_pane_up() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert_eq!(
            chat.handle_key(key(KeyCode::Esc)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Cancelled))
        );
        assert!(
            chat.turn_review_active(),
            "the pane closes when the daemon says the review is gone, not before"
        );
    }

    #[test]
    fn the_action_bar_offers_nothing_until_the_daemon_publishes_a_verdict() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        // Forward is highlighted first, and does nothing while the review runs.
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        chat.set_turn_review(Some(findings_view()));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Forwarded))
        );
    }

    #[test]
    fn a_failed_verdict_offers_dismiss_and_cancel_but_not_forward() {
        let mut chat = chat();
        chat.set_turn_review(Some(RuntimeReviewView {
            verdict: Some(VerdictView {
                kind: VerdictKind::Failed,
                text: "bifrost exited with 1".to_owned(),
                allowed: vec![Resolution::Dismissed, Resolution::Cancelled],
            }),
            ..running_view()
        }));
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        // Right moves to Dismiss, which a failed review does allow.
        assert_eq!(chat.handle_key(key(KeyCode::Right)), ChatAction::None);
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Dismissed))
        );
    }

    #[test]
    fn the_daemon_taking_the_review_away_closes_the_pane() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert!(chat.turn_review_active());
        chat.set_turn_review(None);
        assert!(!chat.turn_review_active());
        assert!(!chat.turn_review_split());
    }

    /// A form is answered back to the harness that asked it. In the extended
    /// tier several are running at once, so answering the default role would
    /// leave a lane waiting for ever while the answer went somewhere else.
    #[test]
    fn a_lanes_form_is_answered_back_to_that_lane() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        let form = hel::hel_elicitation::ElicitationRequest {
            id: "lane-form-1".into(),
            message: "Allow reading /etc?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        assert!(chat.show_review_role_elicitation(Some("tests".to_owned()), form));
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
        assert_eq!(role.as_deref(), Some("tests"));
        assert_eq!(elicitation_id, "lane-form-1");
        assert!(
            chat.turn_review_active(),
            "answering a form does not end the review"
        );
    }

    #[test]
    fn tab_follows_the_roles_the_daemon_published() {
        let mut chat = chat();
        let mut view = running_view();
        view.roles.push(RoleStatus {
            role: "tests".to_owned(),
            label: "Tests".to_owned(),
            state: RoleState::Running,
        });
        chat.set_turn_review(Some(view));
        assert_eq!(
            chat.turn_review().map(TurnReview::selected_role),
            Some("reviewer")
        );
        chat.handle_key(key(KeyCode::Tab));
        assert_eq!(
            chat.turn_review().map(TurnReview::selected_role),
            Some("tests")
        );
        // A role that goes away takes the selection with it.
        chat.set_turn_review(Some(running_view()));
        assert_eq!(
            chat.turn_review().map(TurnReview::selected_role),
            Some("reviewer")
        );
    }
}
