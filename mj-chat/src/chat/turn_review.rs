//! The parent conversation's review status, verdict and resolution controls.
//!
//! Reviewers opens full conversations in the dashboard's child workspace.
//! The parent keeps its status and resolution actions, and shows findings at
//! full width. A review ends by forwarding findings, dismissing them, or cancelling.
//!
//! The review itself runs in the controller daemon
//! (`mj_core::review::host`), not here. This module is a projection of it:
//! the terminal renders the [`RuntimeReviewView`] the daemon publishes and
//! sends back resolutions, exactly as the phone does. That is why closing the
//! terminal no longer ends a review, and why a session nobody is watching is
//! reviewed too.
//!
//! The view still owns the screen while it is up, which is the point of a
//! synchronous review: findings can never land out of the blue in the middle
//! of the next conversation.

use crate::theme;
use crossterm::event::{Event, MouseEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::{ButtonRow, ControlKind, Form, Interaction, TabStrip};
use mj_client::review::{RuntimeReviewView, VerdictKind};
use mj_core::review::driver::{Resolution, RoleState, TurnReviewPhase};

use super::reviewer_pane::ReviewerPane;
use super::viewport::RowViewport;
use std::collections::BTreeMap;

/// Which of the review's actions the keyboard is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewAction {
    Forward,
    Dismiss,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewTab {
    Overview,
    Transcript,
    Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewControl {
    Tabs,
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
    /// Each child keeps its own transcript rows and scroll position.
    panes: BTreeMap<String, ReviewerPane>,
    empty_pane: ReviewerPane,
    selected_child: Option<String>,
    /// Which review tab is selected. The compact Overview is the first tab
    /// while the review runs; transcripts and a verdict remain tab stops.
    tab: ReviewTab,
    /// None means no action is selected. In particular, a running review
    /// cannot accidentally turn Enter into cancellation.
    pub(super) action: Option<ReviewAction>,
    /// The verdict's wrapped rows currently above the viewport. Verdicts are
    /// rendered separately from a reviewer's journal because the daemon can
    /// publish one before that journal has produced a readable event.
    verdict_viewport: RowViewport,
    verdict_total_rows: usize,
    verdict_viewport_height: usize,
    /// The progress tab is short in a normal pane but remains scrollable when
    /// the view is only a few rows high.
    overview_viewport: RowViewport,
    overview_total_rows: usize,
    overview_viewport_height: usize,
    /// A refusal from the daemon, shown in place rather than in a dialog that
    /// would take the review off screen with it.
    pub(super) failure: Option<String>,
    pub(super) form: Form<ReviewControl>,
    component_ready: bool,
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
    pub(super) fn begin_actions_frame(&mut self) {
        self.form.begin_frame();
        // Keep the neutral keyboard stop when the running review has no
        // visible tabs. Otherwise form reconciliation would select Cancel.
        self.form.declare(
            ReviewControl::Tabs,
            ControlKind::Tabs {
                len: 0,
                selected: 0,
            },
        );
        self.component_ready = true;
    }

    fn preferred_action(view: &RuntimeReviewView) -> Option<ReviewAction> {
        // A running review must not make an accidental Enter a cancellation;
        // explicit arrow navigation or the Cancel button can still pick it.
        view.verdict.as_ref().and_then(|verdict| {
            ReviewAction::ORDER
                .into_iter()
                .find(|action| verdict.allowed.contains(&action.resolution()))
        })
    }

    fn action_allowed_in_view(view: &RuntimeReviewView, action: Option<ReviewAction>) -> bool {
        match action {
            None => view.verdict.is_none(),
            Some(ReviewAction::Cancel) => {
                !matches!(&view.phase, TurnReviewPhase::Forwarding { error: None, .. })
            }
            Some(action) => view
                .verdict
                .as_ref()
                .is_some_and(|verdict| verdict.allowed.contains(&action.resolution())),
        }
    }

    #[must_use]
    pub(super) fn new(view: RuntimeReviewView) -> Self {
        let selected_child = view.reviewer_children().next().map(str::to_owned);
        Self {
            tab: if view
                .verdict
                .as_ref()
                .is_some_and(|verdict| !verdict.text.trim().is_empty())
            {
                ReviewTab::Verdict
            } else {
                ReviewTab::Overview
            },
            action: Self::preferred_action(&view),
            view,
            panes: BTreeMap::new(),
            empty_pane: ReviewerPane::default(),
            selected_child,
            verdict_viewport: RowViewport::default(),
            verdict_total_rows: 0,
            verdict_viewport_height: 0,
            overview_viewport: RowViewport::default(),
            overview_total_rows: 0,
            overview_viewport_height: 0,
            failure: None,
            form: review_form(),
            component_ready: false,
        }
    }

    /// Takes the daemon's newer view of the same review.
    pub(super) fn update(&mut self, view: RuntimeReviewView) -> bool {
        self.panes.retain(|child, _| view.owns_reviewer(child));
        if self
            .selected_child
            .as_deref()
            .is_some_and(|child| !view.owns_reviewer(child))
        {
            self.selected_child = None;
        }
        let changed = self.view != view;
        let verdict_changed = self.view.verdict.as_ref().map(|verdict| verdict.kind)
            != view.verdict.as_ref().map(|verdict| verdict.kind);
        if self.tab == ReviewTab::Transcript && view.reviewer_children().next().is_none() {
            self.tab = if view.verdict.is_some() {
                ReviewTab::Verdict
            } else {
                ReviewTab::Overview
            };
        }
        if verdict_changed {
            self.tab = if view
                .verdict
                .as_ref()
                .is_some_and(|verdict| !verdict.text.trim().is_empty())
            {
                ReviewTab::Verdict
            } else {
                ReviewTab::Overview
            };
        }
        if view.verdict.is_none() && self.tab == ReviewTab::Verdict {
            self.tab = ReviewTab::Overview;
        }
        if verdict_changed || !Self::action_allowed_in_view(&view, self.action) {
            self.action = Self::preferred_action(&view);
        }
        self.view = view;
        if self.selected_child.is_none() {
            self.selected_child = self.view.reviewer_children().next().map(str::to_owned);
        }
        changed
    }

    /// The selected reviewer's pane remains available before its transcript arrives.
    pub(super) fn pane(&mut self) -> &mut ReviewerPane {
        match self.selected_child.as_ref() {
            Some(child) => self.panes.entry(child.clone()).or_default(),
            None => &mut self.empty_pane,
        }
    }

    pub(super) fn pane_for(&mut self, child: &str) -> Option<&mut ReviewerPane> {
        self.view
            .owns_reviewer(child)
            .then(|| self.panes.entry(child.to_owned()).or_default())
    }

    fn transcript_children(&self) -> Vec<String> {
        self.view.reviewer_children().map(str::to_owned).collect()
    }

    fn selected_pane(&self) -> Option<&ReviewerPane> {
        self.selected_child
            .as_ref()
            .and_then(|id| self.panes.get(id))
    }

    #[must_use]
    pub(super) fn pane_is_empty(&self) -> bool {
        match self.tab {
            ReviewTab::Overview | ReviewTab::Verdict => true,
            ReviewTab::Transcript => self.selected_pane().is_none_or(ReviewerPane::is_empty),
        }
    }

    #[must_use]
    pub(super) fn verdict_selected(&self) -> bool {
        self.tab == ReviewTab::Verdict
    }

    #[must_use]
    pub(super) fn overview_selected(&self) -> bool {
        self.tab == ReviewTab::Overview
    }

    /// Moves between the overview, the reviewer transcript, and the verdict.
    pub(super) fn cycle_selection(&mut self) -> bool {
        let before_tab = self.tab;
        let before_child = self.selected_child.clone();
        let children = self.transcript_children();
        let verdict = self.verdict_text().is_some();
        self.tab = match self.tab {
            ReviewTab::Overview if !children.is_empty() => {
                self.selected_child = children.first().cloned();
                ReviewTab::Transcript
            }
            ReviewTab::Overview if verdict => ReviewTab::Verdict,
            ReviewTab::Overview => ReviewTab::Overview,
            ReviewTab::Transcript => {
                let next = children
                    .iter()
                    .position(|child| Some(child) == self.selected_child.as_ref())
                    .and_then(|index| children.get(index + 1));
                if let Some(next) = next {
                    self.selected_child = Some(next.clone());
                    ReviewTab::Transcript
                } else if verdict {
                    ReviewTab::Verdict
                } else {
                    ReviewTab::Overview
                }
            }
            ReviewTab::Verdict if !children.is_empty() => {
                self.selected_child = children.first().cloned();
                ReviewTab::Transcript
            }
            ReviewTab::Verdict => ReviewTab::Overview,
        };
        before_tab != self.tab || before_child != self.selected_child
    }

    /// Forms the reviewer's pending requests with the role that asked, because
    /// that is where the answer has to go.
    #[must_use]
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

    /// What the pane's status line says right now.
    #[must_use]
    pub(super) fn status(&self) -> String {
        if let Some(failure) = &self.failure {
            return failure.clone();
        }
        if matches!(&self.view.phase, TurnReviewPhase::Forwarding { .. }) {
            return self.view.status.clone();
        }
        match self.verdict_kind() {
            Some(VerdictKind::Findings | VerdictKind::Failed) => {
                "Enter to act · Tab changes view".to_owned()
            }
            _ => self.view.status.clone(),
        }
    }

    #[must_use]
    pub(super) fn verdict_text(&self) -> Option<&str> {
        self.view
            .verdict
            .as_ref()
            .and_then(|verdict| (!verdict.text.trim().is_empty()).then_some(verdict.text.as_str()))
    }

    #[must_use]
    fn verdict_kind(&self) -> Option<VerdictKind> {
        self.view.verdict.as_ref().map(|verdict| verdict.kind)
    }

    /// Whether an action is available, which the daemon decides and publishes.
    #[must_use]
    fn allows(&self, action: ReviewAction) -> bool {
        Self::action_allowed_in_view(&self.view, Some(action))
    }

    fn set_verdict_viewport(&mut self, total_rows: usize, height: usize) {
        self.verdict_total_rows = total_rows;
        self.verdict_viewport_height = height;
        self.verdict_viewport.clamp(total_rows, height);
    }

    fn scroll_verdict(&mut self, delta: isize) -> bool {
        self.verdict_viewport.scroll_by(
            delta,
            self.verdict_total_rows,
            self.verdict_viewport_height,
        )
    }

    fn set_overview_viewport(&mut self, total_rows: usize, height: usize) {
        self.overview_total_rows = total_rows;
        self.overview_viewport_height = height;
        self.overview_viewport.clamp(total_rows, height);
    }

    fn scroll_overview(&mut self, delta: isize) -> bool {
        self.overview_viewport.scroll_by(
            delta,
            self.overview_total_rows,
            self.overview_viewport_height,
        )
    }

    fn scroll_pane(&mut self, delta: isize, height: usize) -> bool {
        if self.tab == ReviewTab::Overview {
            self.scroll_overview(delta)
        } else if self.tab == ReviewTab::Verdict {
            self.scroll_verdict(delta)
        } else if self.tab == ReviewTab::Transcript {
            self.pane().scroll_by(delta, height)
        } else {
            false
        }
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

fn review_form() -> Form<ReviewControl> {
    let mut form = Form::new();
    form.declare(
        ReviewControl::Tabs,
        ControlKind::Tabs {
            len: 0,
            selected: 0,
        },
    );
    form.declare(ReviewControl::Forward, ControlKind::Button);
    form.declare(ReviewControl::Dismiss, ControlKind::Button);
    form.declare(ReviewControl::Cancel, ControlKind::Button);
    form.end_frame(ReviewControl::Tabs);
    form
}

fn review_tab_index(review: &TurnReview) -> usize {
    let children = review.transcript_children();
    let has_reviewer = !children.is_empty();
    let has_overview = !has_reviewer || review.verdict_text().is_none();
    match review.tab {
        ReviewTab::Overview => 0,
        ReviewTab::Transcript => {
            usize::from(has_overview)
                + children
                    .iter()
                    .position(|child| Some(child) == review.selected_child.as_ref())
                    .unwrap_or(0)
        }
        ReviewTab::Verdict => usize::from(has_overview) + children.len(),
    }
}

fn review_tab_labels(review: &TurnReview) -> Vec<String> {
    let mut labels = Vec::new();
    let has_reviewer = review.view.reviewer_children().next().is_some();
    let has_overview = !has_reviewer || review.verdict_text().is_none();
    if has_overview {
        labels.push("Overview".to_owned());
    }
    for role in review
        .view
        .roles
        .iter()
        .filter(|role| role.child_session_id.is_some())
    {
        labels.push(format!("{} {}", role.label, role.state.label()));
    }
    if review.verdict_text().is_some() {
        labels.push("Verdict".to_owned());
    }
    labels
}

fn select_review_tab(review: &mut TurnReview, selected: usize) {
    let children = review.transcript_children();
    let has_reviewer = !children.is_empty();
    let has_overview = !has_reviewer || review.verdict_text().is_none();
    if has_overview && selected == 0 {
        review.tab = ReviewTab::Overview;
        return;
    }
    let reviewer_tab = usize::from(has_overview);
    if let Some(child) = selected
        .checked_sub(reviewer_tab)
        .and_then(|index| children.get(index))
    {
        review.selected_child = Some(child.clone());
        review.tab = ReviewTab::Transcript;
        return;
    }
    if review.verdict_text().is_some() && selected == reviewer_tab + children.len() {
        review.tab = ReviewTab::Verdict;
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
    /// The text a selection in the reviewer's transcript covers.
    pub fn reviewer_selection_text(
        &self,
        range: &crate::selection::SelectionRange,
    ) -> Option<String> {
        self.turn_review.as_ref().and_then(|review| {
            (review.tab == ReviewTab::Transcript)
                .then(|| {
                    review
                        .selected_pane()
                        .and_then(|pane| pane.selection_text(range))
                })
                .flatten()
        })
    }

    /// Whether the turn-review view owns the screen. While it does, the
    /// composer is not accepting prompts for the primary agent.
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

    pub(super) fn handle_turn_review_mouse(
        &mut self,
        mouse: MouseEvent,
    ) -> (bool, super::ChatAction) {
        let event = Event::Mouse(mouse);
        let result = match self.turn_review.as_mut() {
            Some(review) => review.form.handle(&event),
            None => return (false, super::ChatAction::None),
        };
        match result.action {
            Some(Interaction::Activate(control)) => {
                if let Some(review) = self.turn_review.as_mut() {
                    review.action = Some(match control {
                        ReviewControl::Forward => ReviewAction::Forward,
                        ReviewControl::Dismiss => ReviewAction::Dismiss,
                        ReviewControl::Cancel => ReviewAction::Cancel,
                        ReviewControl::Tabs => return (true, super::ChatAction::None),
                    });
                }
                return (true, self.activate_turn_review_action());
            }
            Some(Interaction::Select(ReviewControl::Tabs, selected)) => {
                if let Some(review) = self.turn_review.as_mut() {
                    select_review_tab(review, selected);
                }
            }
            _ => {}
        }
        (result.consumed, super::ChatAction::None)
    }

    /// Shows the daemon's review, or takes the pane down when it has resolved.
    pub(super) fn set_turn_review(&mut self, view: Option<RuntimeReviewView>) {
        match (view, self.turn_review.as_mut()) {
            (Some(view), Some(open)) => {
                open.update(view);
            }
            (Some(view), None) => {
                self.turn_review = Some(Box::new(TurnReview::new(view)));
            }
            (None, _) => self.close_turn_review(),
        }
    }

    pub(super) fn close_turn_review(&mut self) {
        if self.turn_review.take().is_some() {
            self.turn_review_action_areas.clear();
        }
    }

    pub(super) fn handle_turn_review_event(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> super::ChatAction {
        use crossterm::event::{KeyCode, KeyModifiers};

        let (code, modifiers) = super::normalize_key(key.code, key.modifiers);
        let event = Event::Key(crossterm::event::KeyEvent::new_with_kind_and_state(
            code, modifiers, key.kind, key.state,
        ));
        let (component_ready, result) = {
            let Some(review) = self.turn_review.as_mut() else {
                return super::ChatAction::None;
            };
            (review.component_ready, review.form.handle(&event))
        };
        if let Some(interaction) = result.action {
            match interaction {
                Interaction::Select(ReviewControl::Tabs, selected) => {
                    if let Some(review) = self.turn_review.as_mut() {
                        select_review_tab(review, selected);
                    }
                    return super::ChatAction::None;
                }
                Interaction::Activate(ReviewControl::Tabs) => {
                    return self.activate_turn_review_action();
                }
                Interaction::Activate(ReviewControl::Forward) => {
                    if let Some(review) = self.turn_review.as_mut() {
                        review.action = Some(ReviewAction::Forward);
                    }
                    return self.activate_turn_review_action();
                }
                Interaction::Activate(ReviewControl::Dismiss) => {
                    if let Some(review) = self.turn_review.as_mut() {
                        review.action = Some(ReviewAction::Dismiss);
                    }
                    return self.activate_turn_review_action();
                }
                Interaction::Activate(ReviewControl::Cancel) => {
                    if let Some(review) = self.turn_review.as_mut() {
                        review.action = Some(ReviewAction::Cancel);
                    }
                    return self.activate_turn_review_action();
                }
                Interaction::Cancel => {
                    return self.resolve_turn_review(Resolution::Cancelled);
                }
                _ => {}
            }
        }
        if result.consumed
            && (component_ready || !matches!(key.code, KeyCode::Tab | KeyCode::BackTab))
        {
            return super::ChatAction::None;
        }
        match code {
            // Tab moves between review views; the arrows move between actions,
            // keeping Forward available as a one-key action.
            KeyCode::Tab => {
                if let Some(review) = self.turn_review.as_mut() {
                    review.cycle_selection();
                }
                super::ChatAction::None
            }
            KeyCode::Right => {
                if let Some(review) = self.turn_review.as_mut() {
                    review.action = Some(match review.action {
                        Some(action) => action.next(1),
                        None => ReviewAction::Forward,
                    });
                }
                super::ChatAction::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                if let Some(review) = self.turn_review.as_mut() {
                    review.action = Some(match review.action {
                        Some(action) => action.next(-1),
                        None => ReviewAction::Cancel,
                    });
                }
                super::ChatAction::None
            }
            KeyCode::PageUp => {
                let page = self.last_viewport_height.max(1);
                if let Some(review) = self.turn_review.as_mut() {
                    review.scroll_pane(-(page as isize), page);
                }
                super::ChatAction::None
            }
            KeyCode::PageDown => {
                let page = self.last_viewport_height.max(1);
                if let Some(review) = self.turn_review.as_mut() {
                    review.scroll_pane(page as isize, page);
                }
                super::ChatAction::None
            }
            KeyCode::Enter => self.activate_turn_review_action(),
            // Escape cancels every cancellable stage; an in-flight handoff
            // remains visible until its relay acknowledgement arrives.
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
        let Some(action) = review.action else {
            return super::ChatAction::None;
        };
        // Actions wait for the daemon's typed phase. In particular, an
        // in-flight handoff cannot be cancelled or submitted a second time.
        if !review.allows(action) {
            return super::ChatAction::None;
        }
        self.resolve_turn_review(action.resolution())
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
        review.action = Some(action);
        self.activate_turn_review_action()
    }

    /// Scrolls the reviewer pane of an open turn review.
    pub(super) fn scroll_turn_review(&mut self, rows: isize) -> bool {
        let height = self.last_viewport_height.max(1);
        let Some(review) = self.turn_review.as_mut() else {
            return false;
        };
        review.scroll_pane(rows, height)
    }
}

/// Draws the full-width review verdict. A verdict gets its own
/// wrapped panel so findings and failure reasons remain readable even when
/// the reviewer's journal is empty. The transcript remains reachable through
/// Tab.
pub(super) fn render_turn_review_pane(
    frame: &mut Frame,
    area: Rect,
    review: &mut TurnReview,
) -> (Rect, usize, usize) {
    review.form.begin_frame();
    review.component_ready = true;
    let tab_labels = review_tab_labels(review);
    let tab_area = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        1.min(area.height.saturating_sub(2)),
    );
    let title = verdict_title(Some(review)).to_owned();
    let strip = role_strip(review);
    let status = review.status();
    let result = if let Some(verdict_text) = review.verdict_text().map(str::to_owned) {
        if review.verdict_selected() {
            render_verdict_panel(frame, area, review, &verdict_text, &title, strip)
        } else {
            // Keep the role transcript as the full pane. Tab reaches the
            // verdict as its own scrollable panel, so a long synthesis never
            // competes with or hides the journal the reader is following.
            let transcript_status = "this reviewer has not written a transcript";
            let transcript_empty = review.pane_is_empty();
            super::reviewer_pane::render_reviewer_titled(
                frame,
                area,
                review.pane(),
                if transcript_empty {
                    transcript_status
                } else {
                    &status
                },
                &title,
                strip,
            )
        }
    } else {
        if review.overview_selected() {
            render_review_overview(frame, area, review, &title, strip)
        } else {
            super::reviewer_pane::render_reviewer_titled(
                frame,
                area,
                review.pane(),
                &status,
                &title,
                strip,
            )
        }
    };
    let tab_refs = tab_labels.iter().map(String::as_str).collect::<Vec<_>>();
    TabStrip::render(
        frame,
        tab_area,
        &tab_refs,
        review_tab_index(review),
        &mut review.form,
        ReviewControl::Tabs,
    );
    result
}

/// Draws the compact progress tab shown until a verdict is available. The
/// daemon's status is the stage text; the rows are deliberately qualitative,
/// since the review has no meaningful percentage to report.
fn render_review_overview(
    frame: &mut Frame,
    area: Rect,
    review: &mut TurnReview,
    title: &str,
    strip: Option<Line<'static>>,
) -> (Rect, usize, usize) {
    let block = theme::panel(false)
        .title(title.to_owned())
        .border_style(Style::default().fg(theme::palette().warning));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return (inner, 0, 0);
    }

    let mut lines = Vec::new();
    if let Some(strip) = strip {
        lines.push(strip);
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled("Stage: ", Style::default().fg(theme::palette().muted)),
        Span::raw(review.view.status.clone()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Reviewer",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if review.view.roles.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting for the reviewer to start",
            Style::default().fg(theme::palette().muted),
        )));
    } else {
        for role in &review.view.roles {
            let color = role_state_color(role.state);
            lines.push(Line::from(vec![
                Span::raw(format!("  {}", role.label)),
                Span::raw("  "),
                Span::styled(role.state.label(), Style::default().fg(color)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Prompt paused during review. Esc cancels; Tab changes view.",
        Style::default().fg(theme::palette().muted),
    )));

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    review.set_overview_viewport(total, usize::from(inner.height));
    let top = u16::try_from(review.overview_viewport.top_row).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)), inner);
    // Expose the measured viewport for mouse scrolling in short panes.
    (inner, review.overview_viewport.top_row, total)
}

fn role_state_color(state: RoleState) -> Color {
    match state {
        RoleState::Pending => theme::palette().muted,
        RoleState::Running => theme::palette().warning,
        RoleState::Clean => theme::palette().success,
        RoleState::Findings => theme::palette().secondary,
        RoleState::Failed => theme::palette().error,
    }
}

fn render_verdict_panel(
    frame: &mut Frame,
    area: Rect,
    review: &mut TurnReview,
    text: &str,
    title: &str,
    strip: Option<Line<'static>>,
) -> (Rect, usize, usize) {
    let block = theme::panel(false)
        .title(title.to_owned())
        .border_style(Style::default().fg(theme::palette().secondary));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(strip) = strip
        && inner.height > 1
    {
        let strip_area = Rect::new(inner.x, inner.y, inner.width, 1);
        frame.render_widget(Paragraph::new(strip), strip_area);
        inner = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    }
    if inner.width == 0 || inner.height == 0 {
        review.set_verdict_viewport(0, usize::from(inner.height));
        return (inner, 0, 0);
    }
    let paragraph = Paragraph::new(text.to_owned()).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    review.set_verdict_viewport(total, usize::from(inner.height));
    let top = u16::try_from(review.verdict_viewport.top_row).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)), inner);
    (inner, review.verdict_viewport.top_row, total)
}

/// Draws the review's action bar and reports where each button landed, so a
/// click picks the same action the keyboard would.
pub(super) fn render_turn_review_actions(
    frame: &mut ratatui::Frame,
    area: Rect,
    review: &mut TurnReview,
    status: &str,
) -> Vec<(ReviewAction, Rect)> {
    let mut buttons = Vec::new();
    let mut column = area.x;
    let mut specs = Vec::new();
    for candidate in ReviewAction::ORDER {
        let available = review.allows(candidate);
        let failed_delivery = matches!(
            review.view.phase,
            TurnReviewPhase::Forwarding { error: Some(_), .. }
        );
        let label = match (failed_delivery, candidate) {
            (true, ReviewAction::Forward) => "Retry delivery",
            (true, ReviewAction::Cancel) => "Stop retrying",
            _ => candidate.label(),
        };
        let width = u16::try_from(label.chars().count() + 4).unwrap_or(u16::MAX);
        if column < area.right() {
            buttons.push((
                candidate,
                Rect::new(column, area.y, width.min(area.right() - column), 1),
            ));
        }
        specs.push((
            match candidate {
                ReviewAction::Forward => ReviewControl::Forward,
                ReviewAction::Dismiss => ReviewControl::Dismiss,
                ReviewAction::Cancel => ReviewControl::Cancel,
            },
            label,
            available,
        ));
        column = column.saturating_add(width).saturating_add(1);
    }
    review.form.focus(match review.action {
        Some(ReviewAction::Forward) => ReviewControl::Forward,
        Some(ReviewAction::Dismiss) => ReviewControl::Dismiss,
        Some(ReviewAction::Cancel) => ReviewControl::Cancel,
        None => ReviewControl::Tabs,
    });
    ButtonRow::render(frame, area, &specs, &mut review.form);
    let status_column = area.x.saturating_add(
        u16::try_from(
            specs
                .iter()
                .map(|(_, label, _)| label.len() + 6)
                .sum::<usize>(),
        )
        .unwrap_or(u16::MAX),
    );
    if status_column < area.right() {
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme::palette().muted)),
            Rect::new(
                status_column,
                area.y,
                area.right() - status_column,
                area.height,
            ),
        );
    }
    review.form.end_frame(ReviewControl::Tabs);
    buttons
}

/// The one-line strip above the reviewer transcript.
#[must_use]
pub(super) fn role_strip(review: &TurnReview) -> Option<Line<'static>> {
    let verdict = review.verdict_text().is_some();
    let overview = !verdict;
    let mut spans = Vec::new();
    if overview {
        spans.push(Span::styled(
            "Overview",
            if review.overview_selected() {
                Style::default()
                    .fg(theme::palette().accent)
                    .add_modifier(Modifier::REVERSED)
            } else {
                theme::actionable_chip()
            },
        ));
    }
    for role in &review.view.roles {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let color = role_state_color(role.state);
        let mut style = Style::default().fg(color);
        if review.tab == ReviewTab::Transcript
            && role.child_session_id.as_ref() == review.selected_child.as_ref()
        {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(
            format!("{} {}", role.label, role.state.label()),
            style,
        ));
    }
    if verdict {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let color = match review.verdict_kind() {
            Some(VerdictKind::Failed) => theme::palette().error,
            Some(VerdictKind::Findings) => theme::palette().secondary,
            _ => theme::palette().success,
        };
        let mut style = Style::default().fg(color);
        if review.verdict_selected() {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled("Verdict", style));
    }
    Some(Line::from(spans))
}

/// What the reviewer pane's title says while a verdict is up.
#[must_use]
pub(super) fn verdict_title(review: Option<&TurnReview>) -> &'static str {
    if let Some(review) = review {
        if matches!(
            &review.view.phase,
            TurnReviewPhase::Forwarding { error: None, .. }
        ) {
            return " Turn review · sending findings ";
        }
        if matches!(
            &review.view.phase,
            TurnReviewPhase::Forwarding { error: Some(_), .. }
        ) {
            return " Turn review · forward failed ";
        }
    }
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
    use crate::chat::ChatAction;
    use crate::chat::test_support::{key, snapshot};
    use crossterm::event::KeyCode;
    use mj_client::review::VerdictView;
    use mj_core::review::driver::{RoleStatus, TurnReviewPhase};
    use mj_core::review::verdict::ReviewPassEvidence;

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
    fn reviewer_tabs_keep_each_childs_pane_and_drop_late_children() {
        let mut view = running_view();
        let mut second = view.roles[0].clone();
        second.role = "reviewer-2".into();
        second.label = "Reviewer 2".into();
        second.child_session_id = Some("child-2".into());
        view.roles.push(second);
        let mut review = TurnReview::new(view.clone());
        review.pane_for("reviewer").unwrap();
        review.pane_for("child-2").unwrap();
        assert_eq!(review.panes.len(), 2);
        assert_eq!(review_tab_labels(&review).len(), 3);
        select_review_tab(&mut review, 2);
        assert_eq!(review.selected_child.as_deref(), Some("child-2"));
        assert_eq!(review_tab_index(&review), 2);

        view.roles.remove(0);
        review.update(view);
        assert_eq!(review.selected_child.as_deref(), Some("child-2"));
        assert!(review.pane_for("reviewer").is_none());
        assert!(!review.panes.contains_key("reviewer"));
        assert!(review.panes.contains_key("child-2"));
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
        assert!(
            chat.turn_review()
                .is_some_and(TurnReview::overview_selected)
        );
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    }

    /// R4-13: the header of a failed review read "General done Verdictt".
    /// The role strip and the tab strip draw the same row, and the tab strip
    /// left the strip's wider spacing behind its own labels.
    // Hard-won: ee21d9ca: old row text remained after the review header drew a shorter tab label.
    #[test]
    fn the_review_header_names_each_tab_once() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let mut view = findings_view();
        view.roles[0].state = RoleState::Clean;
        if let Some(verdict) = view.verdict.as_mut() {
            verdict.kind = VerdictKind::Failed;
            verdict.text = "the review could not analyze the change".to_owned();
        }
        let mut review = TurnReview::new(view);
        let mut terminal = Terminal::new(TestBackend::new(46, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, Rect::new(0, 0, 46, 10), &mut review);
            })
            .expect("draw the failed review");
        let buffer = terminal.backend().buffer();
        let header = (0..46).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
        assert_eq!(
            header.trim(),
            "│Reviewer done Verdict                       │"
        );
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

    fn review_pane_golden_buffer(
        review: &mut TurnReview,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, frame.area(), review);
            })
            .expect("draw review pane");
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
    fn golden_turn_review_pane() {
        use mj_core::state::{TranscriptBody, TranscriptItem};

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

        let mut review = TurnReview::new(running_view());
        append_review_golden_state(
            &mut output,
            "review running overview",
            64,
            14,
            &review_pane_golden_buffer(&mut review, 64, 14),
            &[],
        );
        let mut chat = fresh_chat();
        chat.set_turn_review(Some(running_view()));
        chat.set_turn_review(Some(findings_view()));
        append_review_golden_state(
            &mut output,
            "verdict selects first enabled action and panel",
            64,
            14,
            &review_golden_buffer(&mut chat, 64, 14),
            &[
                format!(
                    "selected action: {:?}",
                    chat.turn_review().map(|review| review.action)
                ),
                format!(
                    "verdict selected: {}",
                    chat.turn_review().is_some_and(TurnReview::verdict_selected)
                ),
            ],
        );

        let mut review = TurnReview::new(running_view());
        let buffer = review_pane_golden_buffer(&mut review, 64, 14);
        let rendered = crate::golden::buffer_lines(&buffer).join("\n");
        append_review_golden_state(
            &mut output,
            "running progress and held prompt",
            64,
            14,
            &buffer,
            &[format!("percentage displayed: {}", rendered.contains('%'))],
        );

        let mut review = TurnReview::new(running_view());
        let _ = review_pane_golden_buffer(&mut review, 32, 6);
        review.scroll_overview(isize::MAX);
        let buffer = review_pane_golden_buffer(&mut review, 32, 6);
        append_review_golden_state(
            &mut output,
            "short overview scrolled to held prompt",
            32,
            6,
            &buffer,
            &[],
        );

        let mut review = {
            let mut clean = running_view();
            clean.roles[0].state = RoleState::Clean;
            TurnReview::new(clean)
        };
        append_review_golden_state(
            &mut output,
            "clean reviewer role",
            64,
            14,
            &review_pane_golden_buffer(&mut review, 64, 14),
            &[],
        );

        let mut review = TurnReview::new(findings_view());

        assert!(review.verdict_selected());
        review.view.child_session_id = Some("reviewer-child".into());
        let mut child = mj_core::state::MaterializedSession::empty("reviewer-child");
        child.transcript = vec![std::sync::Arc::new(TranscriptItem {
            stable_id: "agent:1".to_owned(),
            position: 1,
            latest_content_event_ordinal: Some(1),
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: TranscriptBody::Agent {
                chunks: vec![serde_json::json!({
                    "content": {"type": "text", "text": "reviewer transcript"}
                })],
                streaming: false,
            },
        })];
        review.pane().replace_snapshot(child);
        append_review_golden_state(
            &mut output,
            "verdict tab",
            64,
            14,
            &review_pane_golden_buffer(&mut review, 64, 14),
            &[],
        );
        review.cycle_selection();

        assert!(!review.verdict_selected());

        review.cycle_selection();
        append_review_golden_state(
            &mut output,
            "return to verdict tab",
            64,
            14,
            &review_pane_golden_buffer(&mut review, 64, 14),
            &[format!("verdict selected: {}", review.verdict_selected())],
        );

        let mut review = TurnReview::new(findings_view());
        let mut long = findings_view();
        long.verdict.as_mut().unwrap().text = (0..24)
            .map(|index| format!("finding line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        review.update(long);
        append_review_golden_state(
            &mut output,
            "long verdict top",
            48,
            10,
            &review_pane_golden_buffer(&mut review, 48, 10),
            &[],
        );
        review.scroll_verdict(100);
        append_review_golden_state(
            &mut output,
            "long verdict scrolled",
            48,
            10,
            &review_pane_golden_buffer(&mut review, 48, 10),
            &[],
        );

        mj_core::golden::assert_platform_golden(
            env!("CARGO_MANIFEST_DIR"),
            "turn-review-pane",
            &output,
        );
    }

    #[test]
    fn the_daemon_taking_the_review_away_closes_the_pane() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert!(chat.turn_review_active());
        chat.set_turn_review(None);
        assert!(!chat.turn_review_active());
        assert!(!chat.turn_review_active());
    }

    /// A form asked by the reviewer is answered back to that reviewer.

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
    fn tab_cycles_overview_transcript_and_verdict_for_one_reviewer() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert!(
            chat.turn_review()
                .is_some_and(TurnReview::overview_selected)
        );
        assert_eq!(
            chat.turn_review().map(review_tab_labels),
            Some(vec!["Overview".to_owned(), "Reviewer running".to_owned()])
        );
        chat.handle_key(key(KeyCode::Tab));
        assert_eq!(chat.turn_review().map(review_tab_index), Some(1));

        chat.set_turn_review(Some(findings_view()));
        assert!(chat.turn_review().is_some_and(TurnReview::verdict_selected));
        assert_eq!(
            chat.turn_review().map(review_tab_labels),
            Some(vec!["Reviewer running".to_owned(), "Verdict".to_owned()])
        );
        chat.handle_key(key(KeyCode::Tab));
        assert_eq!(chat.turn_review().map(review_tab_index), Some(0));
        assert!(!chat.turn_review().unwrap().verdict_selected());
        chat.handle_key(key(KeyCode::Tab));
        assert!(chat.turn_review().is_some_and(TurnReview::verdict_selected));
    }
}
