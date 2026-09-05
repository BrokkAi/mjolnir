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

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use hel::hel_review::driver::{Resolution, RoleState, TurnReviewPhase};
use mj_controller::hel_review_host::{RuntimeReviewView, VerdictKind};

use super::second_opinion::ReviewerPane;

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
    /// Which review tab is selected. The compact Overview is the first tab
    /// while the review runs; transcripts and a verdict remain tab stops.
    tab: ReviewTab,
    /// None means no action is selected. In particular, a running review
    /// cannot accidentally turn Enter into cancellation.
    pub(super) action: Option<ReviewAction>,
    /// The verdict's wrapped rows currently above the viewport. Verdicts are
    /// rendered separately from a reviewer's journal because the daemon can
    /// publish one before that journal has produced a readable event.
    verdict_top_row: usize,
    verdict_total_rows: usize,
    verdict_viewport_height: usize,
    /// The progress tab is short in a normal pane but remains scrollable when
    /// a split is only a few rows high.
    overview_top_row: usize,
    overview_total_rows: usize,
    overview_viewport_height: usize,
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
            Some(ReviewAction::Cancel) => true,
            Some(action) => view
                .verdict
                .as_ref()
                .is_some_and(|verdict| verdict.allowed.contains(&action.resolution())),
        }
    }

    #[must_use]
    pub(super) fn new(view: RuntimeReviewView) -> Self {
        Self {
            selected: view
                .roles
                .first()
                .map(|role| role.role.clone())
                .unwrap_or_else(|| hel::hel_review::driver::REVIEWER_ROLE.to_owned()),
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
            verdict_top_row: 0,
            verdict_total_rows: 0,
            verdict_viewport_height: 0,
            overview_top_row: 0,
            overview_total_rows: 0,
            overview_viewport_height: 0,
            failure: None,
        }
    }

    /// Takes the daemon's newer view of the same review.
    pub(super) fn update(&mut self, view: RuntimeReviewView) {
        let verdict_changed = self.view.verdict.as_ref().map(|verdict| verdict.kind)
            != view.verdict.as_ref().map(|verdict| verdict.kind);
        let roles = view.roles.clone();
        if self.tab == ReviewTab::Transcript
            && !roles.iter().any(|role| role.role == self.selected)
            && let Some(first) = roles.first()
        {
            // The reader was watching a role this review no longer lists.
            self.selected = first.role.clone();
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

    #[must_use]
    pub(super) fn selected_pane_is_empty(&self) -> bool {
        match self.tab {
            ReviewTab::Overview | ReviewTab::Verdict => true,
            ReviewTab::Transcript => self
                .panes
                .get(&self.selected)
                .is_none_or(ReviewerPane::is_empty),
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

    #[must_use]
    pub(super) fn role_is_active(&self, role: &str) -> bool {
        matches!(&self.view.phase, TurnReviewPhase::Running { roles } if roles.iter().any(|status| status.role == role))
    }

    /// Moves the transcript to the next role that has one, so a reader can
    /// follow a lane without losing the supervisor.
    pub(super) fn cycle_selection(&mut self) {
        let roles = self.view.roles.clone();
        let verdict = self.verdict_text().is_some();
        if roles.is_empty() && !verdict {
            return;
        }
        if self.tab == ReviewTab::Verdict {
            if let Some(role) = roles.first() {
                self.tab = ReviewTab::Transcript;
                self.selected = role.role.clone();
            }
            return;
        }
        if self.tab == ReviewTab::Overview {
            if let Some(role) = roles.first() {
                self.tab = ReviewTab::Transcript;
                self.selected = role.role.clone();
            } else if verdict {
                self.tab = ReviewTab::Verdict;
            }
            return;
        }
        let next = roles
            .iter()
            .position(|role| role.role == self.selected)
            .map(|position| position + 1)
            .unwrap_or(0);
        if next < roles.len() {
            self.selected = roles[next].role.clone();
        } else if verdict {
            self.tab = ReviewTab::Verdict;
        } else {
            self.tab = ReviewTab::Overview;
        }
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
        let active_roles = match &self.view.phase {
            TurnReviewPhase::Running { .. } => self
                .view
                .roles
                .iter()
                .map(|role| role.role.as_str())
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
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
        let maximum = total_rows.saturating_sub(height);
        self.verdict_top_row = self.verdict_top_row.min(maximum);
    }

    fn scroll_verdict(&mut self, delta: isize) {
        let maximum = self
            .verdict_total_rows
            .saturating_sub(self.verdict_viewport_height);
        self.verdict_top_row = if delta.is_negative() {
            self.verdict_top_row.saturating_sub(delta.unsigned_abs())
        } else {
            self.verdict_top_row.saturating_add(delta as usize)
        }
        .min(maximum);
    }

    fn set_overview_viewport(&mut self, total_rows: usize, height: usize) {
        self.overview_total_rows = total_rows;
        self.overview_viewport_height = height;
        let maximum = total_rows.saturating_sub(height);
        self.overview_top_row = self.overview_top_row.min(maximum);
    }

    fn scroll_overview(&mut self, delta: isize) {
        let maximum = self
            .overview_total_rows
            .saturating_sub(self.overview_viewport_height);
        self.overview_top_row = if delta.is_negative() {
            self.overview_top_row.saturating_sub(delta.unsigned_abs())
        } else {
            self.overview_top_row.saturating_add(delta as usize)
        }
        .min(maximum);
    }

    fn scroll_pane(&mut self, delta: isize, height: usize) {
        if self.tab == ReviewTab::Overview {
            self.scroll_overview(delta);
        } else if self.tab == ReviewTab::Verdict {
            self.scroll_verdict(delta);
        } else if self.tab == ReviewTab::Transcript {
            self.selected_pane().scroll_by(delta, height);
        }
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
                review.action = Some(match review.action {
                    Some(action) => action.next(1),
                    None => ReviewAction::Forward,
                });
                super::ChatAction::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                review.action = Some(match review.action {
                    Some(action) => action.next(-1),
                    None => ReviewAction::Cancel,
                });
                super::ChatAction::None
            }
            KeyCode::PageUp => {
                let page = self.last_viewport_height.max(1);
                review.scroll_pane(-(page as isize), page);
                super::ChatAction::None
            }
            KeyCode::PageDown => {
                let page = self.last_viewport_height.max(1);
                review.scroll_pane(page as isize, page);
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
        let Some(action) = review.action else {
            return super::ChatAction::None;
        };
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
        review.action = Some(action);
        self.activate_turn_review_action()
    }

    /// Scrolls the reviewer pane of an open turn review.
    pub(super) fn scroll_turn_review(&mut self, rows: isize) -> bool {
        let height = self.last_viewport_height.max(1);
        let Some(review) = self.turn_review.as_mut() else {
            return false;
        };
        review.scroll_pane(rows, height);
        true
    }
}

/// Draws the turn-review split's reviewer side. A verdict gets its own
/// wrapped panel so findings and failure reasons remain readable even when a
/// role's journal is empty. Role journals remain reachable through Tab.
pub(super) fn render_turn_review_pane(
    frame: &mut Frame,
    area: Rect,
    review: &mut TurnReview,
) -> (Rect, usize, usize) {
    let title = verdict_title(Some(review)).to_owned();
    let strip = role_strip(review);
    let status = review.status();
    let Some(verdict_text) = review.verdict_text().map(str::to_owned) else {
        if review.overview_selected() {
            return render_review_overview(frame, area, review, &title, strip);
        }
        return super::second_opinion::render_reviewer_titled(
            frame,
            area,
            review.selected_pane(),
            &status,
            &title,
            strip,
        );
    };

    if review.verdict_selected() {
        return render_verdict_panel(frame, area, review, &verdict_text, &title, strip);
    }

    // Keep the role transcript as the full pane. Tab reaches the verdict as
    // its own scrollable panel, so a long synthesis never competes with or
    // hides the journal the reader is following.
    let transcript_status = "this reviewer has not written a transcript";
    let transcript_empty = review.selected_pane_is_empty();
    super::second_opinion::render_reviewer_titled(
        frame,
        area,
        review.selected_pane(),
        if transcript_empty {
            transcript_status
        } else {
            &status
        },
        &title,
        strip,
    )
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(Style::default().fg(Color::Yellow));
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
        Span::styled("Stage: ", Style::default().fg(Color::DarkGray)),
        Span::raw(review.view.status.clone()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Reviewers",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if review.view.roles.is_empty() {
        lines.push(Line::from(Span::styled(
            "  waiting for reviewer roles to start",
            Style::default().fg(Color::DarkGray),
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
        "Prompt paused during review. Esc cancels; Tab opens transcripts.",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    review.set_overview_viewport(total, usize::from(inner.height));
    let top = u16::try_from(review.overview_top_row).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)), inner);
    // Expose the measured viewport for mouse scrolling in short panes.
    (inner, review.overview_top_row, total)
}

fn role_state_color(state: RoleState) -> Color {
    match state {
        RoleState::Pending => Color::DarkGray,
        RoleState::Running => Color::Yellow,
        RoleState::Clean => Color::Green,
        RoleState::Findings => Color::LightMagenta,
        RoleState::Failed => Color::Red,
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(Style::default().fg(Color::LightMagenta));
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
    let top = u16::try_from(review.verdict_top_row).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((top, 0)), inner);
    (inner, review.verdict_top_row, total)
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
        // A running review intentionally has no selected action, so its
        // action bar shows all choices without implying that Enter will
        // cancel the review.
        if Some(candidate) == review.action {
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
    let roles = &review.view.roles;
    let verdict = review.verdict_text().is_some();
    let overview = !verdict;
    let mut spans = Vec::new();
    if overview {
        spans.push(Span::styled(
            "Overview",
            if review.overview_selected() {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Cyan)
            },
        ));
    }
    for (index, role) in roles.iter().enumerate() {
        if index > 0 || overview {
            spans.push(Span::raw("  "));
        }
        let color = role_state_color(role.state);
        let mut style = Style::default().fg(color);
        if review.tab == ReviewTab::Transcript && role.role == review.selected_role() {
            // The strip is also the tab bar: the highlighted row is the
            // transcript below it.
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
            Some(VerdictKind::Failed) => Color::Red,
            Some(VerdictKind::Findings) => Color::LightMagenta,
            _ => Color::Green,
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
    fn enter_is_harmless_until_a_verdict_arrives() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        // Repeated Enter presses do not turn the absence of a selected action
        // into a cancellation request.
        assert_eq!(chat.turn_review().map(|review| review.action), Some(None));
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
    }

    #[test]
    fn explicit_navigation_can_select_cancel_while_running() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        chat.handle_key(key(KeyCode::Left));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Cancelled))
        );
    }

    #[test]
    fn the_action_bar_starts_on_an_enabled_action_at_each_verdict_stage() {
        let mut chat = chat();
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
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::TurnReview(TurnReviewIntent::Resolve(Resolution::Dismissed))
        );
    }

    #[test]
    fn updating_to_a_verdict_selects_its_first_enabled_action_and_panel() {
        let mut chat = chat();
        chat.set_turn_review(Some(running_view()));
        assert_eq!(chat.turn_review().map(|review| review.action), Some(None));
        chat.set_turn_review(Some(findings_view()));
        let review = chat.turn_review().expect("review remains open");
        assert_eq!(review.action, Some(ReviewAction::Forward));
        assert!(
            review.verdict_selected(),
            "the new verdict is immediately shown"
        );
        assert!(role_strip(review).is_some_and(|line| {
            line.spans
                .iter()
                .any(|span| span.content.as_ref() == "Verdict")
        }));
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

    #[test]
    fn running_review_defaults_to_overview_with_progress_and_prompt_hold() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let mut review = TurnReview::new(running_view());
        let mut terminal = Terminal::new(TestBackend::new(64, 14)).expect("terminal");
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, Rect::new(0, 0, 64, 14), &mut review);
            })
            .expect("draw overview");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(review.overview_selected());
        assert!(screen.contains("Overview"));
        assert!(screen.contains("Stage:"));
        assert!(screen.contains("the reviewer is reading the change"));
        assert!(screen.contains("General"));
        assert!(screen.contains("running"));
        assert!(screen.contains("Prompt paused during review"));
        assert!(
            !screen.contains("%"),
            "progress has no fabricated percentage"
        );
    }

    #[test]
    fn short_overview_can_scroll_to_the_held_prompt() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let mut review = TurnReview::new(running_view());
        let mut terminal = Terminal::new(TestBackend::new(32, 6)).expect("terminal");
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, Rect::new(0, 0, 32, 6), &mut review);
            })
            .expect("draw short overview");
        review.scroll_overview(isize::MAX);
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, Rect::new(0, 0, 32, 6), &mut review);
            })
            .expect("draw scrolled overview");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Prompt paused"));
    }

    #[test]
    fn clean_role_is_presented_as_done() {
        let mut view = running_view();
        view.roles[0].state = RoleState::Clean;
        let review = TurnReview::new(view);
        let strip = role_strip(&review).expect("role strip");
        assert!(strip.spans.iter().any(|span| span.content.contains("done")));
        assert!(
            !strip
                .spans
                .iter()
                .any(|span| span.content.contains("clean"))
        );
    }

    #[test]
    fn a_verdict_tab_keeps_role_transcripts_reachable() {
        let mut review = TurnReview::new(findings_view());
        assert!(review.verdict_selected());
        review.pane("reviewer").restore(
            "reviewer-session",
            vec![std::sync::Arc::new(hel::hel_state::TranscriptItem {
                stable_id: "agent:1".to_owned(),
                position: 1,
                latest_content_event_ordinal: Some(1),
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body: hel::hel_state::TranscriptBody::Agent {
                    chunks: vec![serde_json::json!({
                        "content": {"type": "text", "text": "reviewer transcript"}
                    })],
                    streaming: false,
                },
            })],
        );
        review.cycle_selection();
        assert!(!review.verdict_selected());
        assert_eq!(review.selected_role(), "reviewer");
        review.cycle_selection();
        assert!(review.verdict_selected());
    }

    #[test]
    fn a_long_verdict_wraps_and_scrolls_when_no_journal_exists() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let mut view = findings_view();
        view.verdict.as_mut().expect("findings").text = (0..24)
            .map(|index| format!("finding line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut review = TurnReview::new(view);
        let mut terminal = Terminal::new(TestBackend::new(48, 10)).expect("terminal");
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, Rect::new(0, 0, 48, 10), &mut review);
            })
            .expect("draw verdict");
        let first = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(first.contains("finding line 0"));

        review.scroll_verdict(100);
        terminal
            .draw(|frame| {
                render_turn_review_pane(frame, Rect::new(0, 0, 48, 10), &mut review);
            })
            .expect("draw scrolled verdict");
        let last = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(last.contains("finding line 23"));
        assert!(!last.contains("finding line 0"));
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
        assert!(
            chat.turn_review()
                .is_some_and(TurnReview::overview_selected)
        );
        chat.handle_key(key(KeyCode::Tab));
        assert_eq!(
            chat.turn_review().map(TurnReview::selected_role),
            Some("reviewer")
        );
        chat.handle_key(key(KeyCode::Tab));
        assert_eq!(
            chat.turn_review().map(TurnReview::selected_role),
            Some("tests")
        );
        chat.handle_key(key(KeyCode::Tab));
        assert!(
            chat.turn_review()
                .is_some_and(TurnReview::overview_selected)
        );
        // A role that goes away takes the selection with it.
        chat.handle_key(key(KeyCode::Tab));
        chat.set_turn_review(Some(running_view()));
        assert_eq!(
            chat.turn_review().map(TurnReview::selected_role),
            Some("reviewer")
        );
    }
}
