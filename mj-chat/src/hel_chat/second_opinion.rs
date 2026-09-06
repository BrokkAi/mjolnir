//! The chat's second-opinion view: choosing a reviewer, then the split.
//!
//! Two shapes share this module because they are two states of one thing. The
//! waterfall picks a reviewer; once one is running the view becomes a split
//! with the primary conversation on the left and the reviewer's on the right.
//!
//! The reviewer's pane owns its own wrapped rows, its own scroll and its own
//! selection surface. Sharing the primary's would tie the two panes together:
//! a reviewer answer arriving would move the reader's place in the primary
//! transcript, and a drag started in one pane would run into the other.

use std::sync::Arc;

use crossterm::event::{Event, KeyEvent, MouseEvent};
use rat_event::ConsumedEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::components::{ButtonRow, ChoiceList, ControlKind, Form, Interaction};
use crate::hel_selection::{SelectionRange, SurfaceFrame, SurfaceId};
use hel::hel_elicitation::ElicitationRequest;
use hel::hel_projection::{apply_committed_projection_event, project_relay_event};
use hel::hel_second_opinion::{
    ReviewStage, ReviewWorkflow, ReviewerSetup, SetupRequest, SetupStage, WorkflowRequest,
};
use hel::hel_state::MaterializedSession;
use hel::hel_transcript::ChatEntry;
use hel::hel_worker::RelayEvent;

use super::rendering::TranscriptRenderMode;
use super::transcript::{materialized_chat_entries_reusing, render_entry_rows};

/// The plan a review is about, captured when the user asked for one.
///
/// The harness's own decision is resolved to gather context, so this is the
/// only copy of the proposal that survives the review. Cancelling therefore
/// owes the user a Hel-owned decision built from it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CapturedProposal {
    /// The pending plan decision, so it can be resolved through the dialect
    /// bridge once the reviewer is ready.
    pub(super) request: ElicitationRequest,
    pub(super) proposal: String,
}

impl CapturedProposal {
    pub(super) fn id(&self) -> &str {
        &self.request.id
    }
}

/// Which of the split's actions the keyboard is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SplitAction {
    Transfer,
    Implement,
    Cancel,
}

impl SplitAction {
    const ORDER: [Self; 3] = [Self::Transfer, Self::Implement, Self::Cancel];

    fn label(self) -> &'static str {
        match self {
            Self::Transfer => "Transfer feedback",
            Self::Implement => "Implement original",
            Self::Cancel => "Cancel",
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
pub(super) enum SetupControl {
    Options,
    Confirm,
    Back,
    Retry,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SplitControl {
    Transfer,
    Implement,
    Cancel,
}

enum SetupInteraction {
    Select(usize),
    Activate(SetupControl),
}

impl From<Interaction<SetupControl>> for SetupInteraction {
    fn from(interaction: Interaction<SetupControl>) -> Self {
        match interaction {
            Interaction::Select(SetupControl::Options, selected) => Self::Select(selected),
            Interaction::Activate(control) => Self::Activate(control),
            Interaction::Cancel => Self::Activate(SetupControl::Cancel),
            _ => Self::Activate(SetupControl::Options),
        }
    }
}

enum SplitInteraction {
    Activate(SplitControl),
}

impl From<Interaction<SplitControl>> for SplitInteraction {
    fn from(interaction: Interaction<SplitControl>) -> Self {
        match interaction {
            Interaction::Activate(control) => Self::Activate(control),
            Interaction::Cancel => Self::Activate(SplitControl::Cancel),
            _ => Self::Activate(SplitControl::Cancel),
        }
    }
}

/// Where the second-opinion view has got to.
#[derive(Debug)]
pub(super) enum SecondOpinion {
    /// Choosing which harness reviews the plan.
    Setup {
        captured: CapturedProposal,
        setup: Box<ReviewerSetup>,
        form: Box<Form<SetupControl>>,
    },
    /// The reviewer is running; the split is up.
    Review(Box<ActiveReview>),
}

/// A review in progress, boxed so the waterfall state stays small.
#[derive(Debug)]
pub(super) struct ActiveReview {
    pub(super) captured: CapturedProposal,
    pub(super) workflow: ReviewWorkflow,
    pub(super) reviewer: ReviewerPane,
    pub(super) action: SplitAction,
    /// What the review is doing, shown beside the actions.
    pub(super) status: String,
    /// The primary's transcript frontier when the context request went out.
    /// Only an agent message after it can be the answer to it.
    pub(super) context_baseline: u64,
    pub(super) form: Form<SplitControl>,
}

/// What the second-opinion view asked the session to do.
#[derive(Debug, Clone, PartialEq)]
pub enum SecondOpinionIntent {
    /// Reviewer setup steps, in order.
    Setup(Vec<SetupRequest>),
    /// The user chose a reviewer. Stage it, start it, and begin the review.
    Confirmed {
        profile_id: String,
        model: Option<String>,
        effort: Option<String>,
    },
    /// Review steps, in order.
    Workflow(Vec<WorkflowRequest>),
    /// The view closed without anything further to do.
    Closed,
}

impl SecondOpinion {
    pub(super) fn captured(&self) -> &CapturedProposal {
        match self {
            Self::Setup { captured, .. } => captured,
            Self::Review(review) => &review.captured,
        }
    }

    /// The reviewer pane, once the split is up.
    pub(super) fn reviewer(&self) -> Option<&ReviewerPane> {
        match self {
            Self::Review(review) => Some(&review.reviewer),
            Self::Setup { .. } => None,
        }
    }

    pub(super) fn reviewer_mut(&mut self) -> Option<&mut ReviewerPane> {
        match self {
            Self::Review(review) => Some(&mut review.reviewer),
            Self::Setup { .. } => None,
        }
    }

    pub(super) fn set_status(&mut self, text: impl Into<String>) {
        if let Self::Review(review) = self {
            review.status = text.into();
        }
    }

    /// Rebuilds the reviewer's pane from a stored transcript.
    pub(super) fn restore_prepared_reviewer(&mut self, reviewer: ReviewerPane) {
        if let Self::Review(review) = self {
            review.reviewer = reviewer;
        }
    }

    /// Replaces the waterfall with the split once a reviewer is running.
    pub(super) fn begin_review(
        &mut self,
        workflow: ReviewWorkflow,
        status: impl Into<String>,
        context_baseline: u64,
    ) {
        let Self::Setup { captured, .. } = self else {
            return;
        };
        *self = Self::Review(Box::new(ActiveReview {
            captured: captured.clone(),
            workflow,
            reviewer: ReviewerPane::default(),
            action: SplitAction::Transfer,
            status: status.into(),
            context_baseline,
            form: split_form(),
        }));
    }

    /// Reports a failure in place, leaving the view up so the user can retry
    /// or cancel rather than losing the captured plan to a dismissed dialog.
    pub(super) fn report_failure(&mut self, message: impl Into<String>) {
        match self {
            Self::Setup { setup, .. } => setup.probe_failed_current(message),
            Self::Review(review) => review.status = message.into(),
        }
    }
}

/// The reviewer's conversation, as its own scrollable pane.
#[derive(Debug, Default)]
pub(super) struct ReviewerPane {
    /// Projected reviewer session, folded from its relay events.
    session: Option<MaterializedSession>,
    entries: Vec<ChatEntry>,
    /// Wrapped rows for `width`. This is the pane's own row cache: a
    /// selection in it is resolved here and never against the primary's.
    rows: Vec<Line<'static>>,
    width: u16,
    /// First content row drawn.
    top_row: usize,
    /// Whether new rows keep the pane pinned to the end.
    follow: bool,
    /// Frontier this pane has folded, so a replay resumes from it.
    pub(super) cursor_ordinal: u64,
    pub(super) cursor_digest: String,
}

impl ReviewerPane {
    /// Folds one page of reviewer relay events into the pane.
    ///
    /// Reusing the primary projection is the point of giving the reviewer a
    /// real relay: its transcript is built by the same code, so it renders and
    /// collapses identically.
    pub(super) fn apply_events(&mut self, session_id: &str, events: &[RelayEvent]) {
        let session = self
            .session
            .get_or_insert_with(|| MaterializedSession::empty(session_id));
        for event in events {
            let Ok(projected) = project_relay_event(session, event) else {
                continue;
            };
            if apply_committed_projection_event(session, event, projected.mutation).is_err() {
                continue;
            }
            self.cursor_ordinal = event.ordinal;
            self.cursor_digest.clone_from(&event.digest);
        }
        // Rebuilt whole rather than incrementally: a reviewer's conversation
        // is one short turn, so the simpler path costs nothing here.
        self.entries = materialized_chat_entries_reusing(session, 0, Vec::new());
        // Rows are rebuilt on the next draw, at whatever width that draw has.
        self.width = 0;
        self.follow = true;
    }

    /// The reviewer's latest complete agent answer, which is what a transfer
    /// sends. Thoughts and tool logs are deliberately not part of it.
    pub(super) fn latest_answer(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        session
            .transcript
            .iter()
            .rev()
            .find(|item| item.is_nonempty_agent_message())
            .map(|item| {
                let hel::hel_state::TranscriptBody::Agent { chunks, .. } = &item.body else {
                    return String::new();
                };
                super::transcript::materialized_chunks_text(chunks)
            })
            .filter(|text| !text.trim().is_empty())
    }

    /// What the pane has read of the reviewer's conversation, for the copy
    /// the controller keeps against the target going away.
    pub(super) fn transcript(&self) -> Vec<Arc<hel::hel_state::TranscriptItem>> {
        self.session
            .as_ref()
            .map(|session| session.transcript.clone())
            .unwrap_or_default()
    }

    /// Rebuilds a pane from a stored transcript, for a review restored after
    /// the reviewer's own journal became unreachable.
    pub(super) fn restore(
        &mut self,
        session_id: &str,
        transcript: Vec<Arc<hel::hel_state::TranscriptItem>>,
    ) {
        if transcript.is_empty() {
            return;
        }
        let mut session = MaterializedSession::empty(session_id);
        session.applied_event_ordinal = transcript
            .iter()
            .map(|item| item.position)
            .max()
            .unwrap_or(0);
        session.transcript = transcript;
        self.entries = materialized_chat_entries_reusing(&session, 0, Vec::new());
        self.session = Some(session);
        self.width = 0;
        self.follow = true;
    }

    /// Forms the reviewer's harness is waiting on.
    pub(super) fn pending_elicitations(&self) -> &[ElicitationRequest] {
        self.session
            .as_ref()
            .map_or(&[], |session| session.pending_elicitations.as_slice())
    }

    /// Whether the reviewer has produced anything yet.
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn ensure_rows(&mut self, width: u16) {
        if self.width == width && !self.rows.is_empty() {
            return;
        }
        self.width = width;
        self.rows = self
            .entries
            .iter()
            .flat_map(|entry| {
                render_entry_rows(entry, usize::from(width), TranscriptRenderMode::Rich)
            })
            .collect();
    }

    /// Scrolls by `delta` rows, leaving follow mode on only at the end.
    pub(super) fn scroll_by(&mut self, delta: isize, height: usize) {
        let maximum = self.rows.len().saturating_sub(height);
        let top = if delta.is_negative() {
            self.top_row.saturating_sub(delta.unsigned_abs())
        } else {
            self.top_row.saturating_add(delta as usize)
        };
        self.top_row = top.min(maximum);
        self.follow = self.top_row >= maximum;
    }

    /// The text a selection in this pane covers, resolved against this pane's
    /// rows so it can never pick up the primary transcript's.
    pub(super) fn selection_text(&self, range: &SelectionRange) -> Option<String> {
        if self.width == 0 {
            return None;
        }
        let end = range.end.row.min(self.rows.len().saturating_sub(1));
        let text = (range.start.row..=end)
            .filter_map(|row| {
                let line = self.rows.get(row)?;
                Some(match range.columns_on(row, self.width) {
                    Some((first, last)) if first > 0 || last + 1 < self.width => {
                        sliced_row(line, self.width, first, last)
                    }
                    _ => row_text(line),
                })
            })
            .collect::<Vec<_>>();
        (!text.is_empty()).then(|| text.join("\n"))
    }
}

fn setup_form(setup: &ReviewerSetup) -> Form<SetupControl> {
    let mut form = Form::new();
    prepare_setup_form(setup, &mut form);
    form
}

fn prepare_setup_form(setup: &ReviewerSetup, form: &mut Form<SetupControl>) {
    let options_were_available = form.is_enabled(SetupControl::Options);
    form.begin_update();
    if setup.failure().is_some() {
        form.declare(SetupControl::Retry, ControlKind::Button);
    } else if !setup.busy() {
        form.declare(SetupControl::Options, setup_control_kind(setup));
        form.declare_with_enabled(
            SetupControl::Confirm,
            ControlKind::Button,
            setup.can_confirm(),
        );
        form.declare_with_enabled(
            SetupControl::Back,
            ControlKind::Button,
            setup.stage() != SetupStage::Profile,
        );
    }
    form.declare(SetupControl::Cancel, ControlKind::Button);
    if !options_were_available && form.is_enabled(SetupControl::Options) && !form.captures_pointer()
    {
        form.focus(SetupControl::Options);
    }
    form.end_frame(SetupControl::Options);
}

fn setup_control_kind(setup: &ReviewerSetup) -> ControlKind {
    let (len, selected) = match setup.stage() {
        SetupStage::Profile => (setup.profiles().len(), setup.profile_index()),
        SetupStage::Model => (setup.models().len(), setup.model_index()),
        SetupStage::Effort => (setup.efforts().len(), setup.effort_index()),
    };
    ControlKind::ChoiceList { len, selected }
}

fn split_form() -> Form<SplitControl> {
    let mut form = Form::new();
    form.declare(SplitControl::Transfer, ControlKind::Button);
    form.declare(SplitControl::Implement, ControlKind::Button);
    form.declare(SplitControl::Cancel, ControlKind::Button);
    form.end_frame(SplitControl::Transfer);
    form
}

fn setup_current_index(setup: &ReviewerSetup) -> usize {
    match setup.stage() {
        SetupStage::Profile => setup.profile_index(),
        SetupStage::Model => setup.model_index(),
        SetupStage::Effort => setup.effort_index(),
    }
}

fn row_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn sliced_row(line: &Line<'static>, width: u16, first: u16, last: u16) -> String {
    let area = Rect::new(0, 0, width, 1);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(line.clone()).render(area, &mut buffer);
    crate::hel_selection::extract_rows(
        &buffer,
        &SurfaceFrame::fixed(SurfaceId::ReviewerTranscript, area),
        &SelectionRange {
            start: crate::hel_selection::ContentPos::new(0, first),
            end: crate::hel_selection::ContentPos::new(0, last),
        },
    )
}

impl super::ChatState {
    /// Whether the second-opinion view owns the screen.
    pub(super) fn second_opinion_active(&self) -> bool {
        self.second_opinion.is_some()
    }

    /// Whether the split is up, which is when the transcript shares the frame.
    pub(super) fn second_opinion_split(&self) -> bool {
        matches!(self.second_opinion, Some(SecondOpinion::Review(_)))
    }

    /// Opens the waterfall for `captured`.
    pub(super) fn open_second_opinion(&mut self, captured: CapturedProposal, setup: ReviewerSetup) {
        // The waterfall owns the frame; a value selector left open underneath
        // would fight it for keys when the review closes.
        self.config_picker = None;
        self.second_opinion = Some(SecondOpinion::Setup {
            captured,
            form: Box::new(setup_form(&setup)),
            setup: Box::new(setup),
        });
    }

    pub(super) fn second_opinion(&self) -> Option<&SecondOpinion> {
        self.second_opinion.as_ref()
    }

    pub(super) fn second_opinion_mut(&mut self) -> Option<&mut SecondOpinion> {
        self.second_opinion.as_mut()
    }

    pub(super) fn second_opinion_handles_mouse(&self, column: u16, row: u16) -> bool {
        match self.second_opinion.as_ref() {
            Some(SecondOpinion::Setup { form, .. }) => {
                form.captures_pointer() || form.contains(column, row)
            }
            Some(SecondOpinion::Review(review)) => {
                review.form.captures_pointer() || review.form.contains(column, row)
            }
            None => false,
        }
    }

    pub(super) fn cancel_second_opinion_pointer(&mut self) {
        match self.second_opinion.as_mut() {
            Some(SecondOpinion::Setup { form, .. }) => form.cancel_pointer(),
            Some(SecondOpinion::Review(review)) => review.form.cancel_pointer(),
            None => {}
        }
    }

    pub(super) fn reset_second_opinion_geometry(&mut self) {
        match self.second_opinion.as_mut() {
            Some(SecondOpinion::Setup { form, .. }) => form.reset_geometry(),
            Some(SecondOpinion::Review(review)) => review.form.reset_geometry(),
            None => {}
        }
    }

    /// Routes a mouse gesture through the active component form. The boolean
    /// reports consumption even when the release only focused or armed a
    /// control, keeping background selection from seeing the same gesture.
    pub(super) fn handle_second_opinion_mouse(
        &mut self,
        mouse: MouseEvent,
    ) -> (bool, super::ChatAction) {
        let event = Event::Mouse(mouse);
        if matches!(self.second_opinion, Some(SecondOpinion::Setup { .. })) {
            let result = match self.second_opinion.as_mut() {
                Some(SecondOpinion::Setup { form, .. }) => form.handle(&event),
                _ => unreachable!(),
            };
            let consumed = result.outcome.is_consumed();
            if let Some(interaction) = result.action.map(SetupInteraction::from) {
                return match interaction {
                    SetupInteraction::Select(selected) => {
                        if let Some(SecondOpinion::Setup { setup, form, .. }) =
                            self.second_opinion.as_mut()
                        {
                            let current = setup_current_index(setup);
                            let delta = if selected >= current { 1 } else { -1 };
                            for _ in 0..selected.abs_diff(current) {
                                setup.move_selection(delta);
                            }
                            form.set_selected(SetupControl::Options, selected);
                        }
                        (true, super::ChatAction::None)
                    }
                    SetupInteraction::Activate(control) => {
                        let outcome = match self.second_opinion.as_mut() {
                            Some(SecondOpinion::Setup { setup, .. }) => match control {
                                SetupControl::Confirm | SetupControl::Options => setup.confirm(),
                                SetupControl::Back => setup.back(),
                                SetupControl::Retry => setup.retry(),
                                SetupControl::Cancel => setup.cancel(),
                            },
                            _ => hel::hel_second_opinion::SetupOutcome::None,
                        };
                        (true, self.apply_setup_outcome(outcome))
                    }
                };
            }
            return (consumed, super::ChatAction::None);
        }
        if matches!(self.second_opinion, Some(SecondOpinion::Review(_))) {
            let result = match self.second_opinion.as_mut() {
                Some(SecondOpinion::Review(review)) => review.form.handle(&event),
                _ => unreachable!(),
            };
            let consumed = result.outcome.is_consumed();
            if let Some(SplitInteraction::Activate(control)) =
                result.action.map(SplitInteraction::from)
            {
                if let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() {
                    review.action = match control {
                        SplitControl::Transfer => SplitAction::Transfer,
                        SplitControl::Implement => SplitAction::Implement,
                        SplitControl::Cancel => SplitAction::Cancel,
                    };
                }
                return (true, self.activate_split_action());
            }
            return (consumed, super::ChatAction::None);
        }
        (false, super::ChatAction::None)
    }

    fn handle_setup_component_event(&mut self, key: KeyEvent) -> (bool, super::ChatAction) {
        let (code, modifiers) = super::normalize_key(key.code, key.modifiers);
        let event = Event::Key(KeyEvent::new_with_kind_and_state(
            code, modifiers, key.kind, key.state,
        ));
        if let Some(SecondOpinion::Setup { setup, form, .. }) = self.second_opinion.as_mut() {
            prepare_setup_form(setup, form);
        }
        let (consumed, interaction) = {
            let Some(SecondOpinion::Setup { form, .. }) = self.second_opinion.as_mut() else {
                return (false, super::ChatAction::None);
            };
            let result = form.handle(&event);
            (result.outcome.is_consumed(), result.action)
        };
        let Some(interaction) = interaction else {
            return (consumed, super::ChatAction::None);
        };
        let outcome = match interaction {
            Interaction::Select(SetupControl::Options, selected) => {
                if let Some(SecondOpinion::Setup { setup, form, .. }) = self.second_opinion.as_mut()
                {
                    let current = setup_current_index(setup);
                    let delta = if selected >= current { 1 } else { -1 };
                    let distance = selected.abs_diff(current);
                    for _ in 0..distance {
                        setup.move_selection(delta);
                    }
                    form.set_selected(SetupControl::Options, selected);
                }
                super::ChatAction::None
            }
            Interaction::Activate(SetupControl::Options | SetupControl::Confirm) => {
                let outcome = match self.second_opinion.as_mut() {
                    Some(SecondOpinion::Setup { setup, .. }) => setup.confirm(),
                    _ => hel::hel_second_opinion::SetupOutcome::None,
                };
                self.apply_setup_outcome(outcome)
            }
            Interaction::Activate(SetupControl::Back) => {
                let outcome = match self.second_opinion.as_mut() {
                    Some(SecondOpinion::Setup { setup, .. }) => setup.back(),
                    _ => hel::hel_second_opinion::SetupOutcome::None,
                };
                self.apply_setup_outcome(outcome)
            }
            Interaction::Activate(SetupControl::Retry) => {
                let outcome = match self.second_opinion.as_mut() {
                    Some(SecondOpinion::Setup { setup, .. }) => setup.retry(),
                    _ => hel::hel_second_opinion::SetupOutcome::None,
                };
                self.apply_setup_outcome(outcome)
            }
            Interaction::Activate(SetupControl::Cancel) | Interaction::Cancel => {
                let outcome = match self.second_opinion.as_mut() {
                    Some(SecondOpinion::Setup { setup, .. }) => setup.cancel(),
                    _ => hel::hel_second_opinion::SetupOutcome::None,
                };
                self.apply_setup_outcome(outcome)
            }
            _ => super::ChatAction::None,
        };
        (true, outcome)
    }

    fn handle_split_component_event(&mut self, key: KeyEvent) -> (bool, super::ChatAction) {
        let (code, modifiers) = super::normalize_key(key.code, key.modifiers);
        let event = Event::Key(KeyEvent::new_with_kind_and_state(
            code, modifiers, key.kind, key.state,
        ));
        let (consumed, interaction, focused) = {
            let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() else {
                return (false, super::ChatAction::None);
            };
            let result = review.form.handle(&event);
            (
                result.outcome.is_consumed(),
                result.action,
                review.form.focused(),
            )
        };
        if let Some(action) = interaction {
            match action {
                Interaction::Activate(SplitControl::Transfer) => {
                    if let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() {
                        review.action = SplitAction::Transfer;
                    }
                    return (true, self.activate_split_action());
                }
                Interaction::Activate(SplitControl::Implement) => {
                    if let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() {
                        review.action = SplitAction::Implement;
                    }
                    return (true, self.activate_split_action());
                }
                Interaction::Activate(SplitControl::Cancel) => {
                    if let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() {
                        review.action = SplitAction::Cancel;
                    }
                    return (true, self.activate_split_action());
                }
                Interaction::Cancel => {
                    if let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() {
                        review.action = SplitAction::Cancel;
                    }
                    return (true, self.activate_split_action());
                }
                _ => {}
            }
        }
        if consumed {
            if let Some(control) = focused
                && let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut()
            {
                review.action = match control {
                    SplitControl::Transfer => SplitAction::Transfer,
                    SplitControl::Implement => SplitAction::Implement,
                    SplitControl::Cancel => SplitAction::Cancel,
                };
            }
            return (true, super::ChatAction::None);
        }
        (false, super::ChatAction::None)
    }

    pub(super) fn handle_second_opinion_event(&mut self, key: KeyEvent) -> super::ChatAction {
        let (code, modifiers) = super::normalize_key(key.code, key.modifiers);
        use crossterm::event::{KeyCode, KeyModifiers};

        if matches!(self.second_opinion, Some(SecondOpinion::Setup { .. })) {
            let (handled, action) = self.handle_setup_component_event(key);
            if handled {
                return action;
            }
        }
        if matches!(self.second_opinion, Some(SecondOpinion::Review(_))) {
            let (handled, action) = self.handle_split_component_event(key);
            if handled {
                return action;
            }
        }
        let Some(view) = self.second_opinion.as_mut() else {
            return super::ChatAction::None;
        };
        match view {
            SecondOpinion::Setup { setup, .. } => {
                let outcome = match code {
                    KeyCode::Char('r') if setup.failure().is_some() => setup.retry(),
                    KeyCode::Left | KeyCode::Backspace => setup.back(),
                    KeyCode::Esc => setup.cancel(),
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        setup.cancel()
                    }
                    _ => return super::ChatAction::None,
                };
                self.apply_setup_outcome(outcome)
            }
            SecondOpinion::Review(review) => match code {
                KeyCode::Tab | KeyCode::Right => {
                    review.action = review.action.next(1);
                    super::ChatAction::None
                }
                KeyCode::BackTab | KeyCode::Left => {
                    review.action = review.action.next(-1);
                    super::ChatAction::None
                }
                KeyCode::PageUp => {
                    let page = self.last_viewport_height.max(1);
                    review.reviewer.scroll_by(-(page as isize), page);
                    super::ChatAction::None
                }
                KeyCode::PageDown => {
                    let page = self.last_viewport_height.max(1);
                    review.reviewer.scroll_by(page as isize, page);
                    super::ChatAction::None
                }
                KeyCode::Enter => self.activate_split_action(),
                KeyCode::Esc => self.cancel_review(),
                _ => super::ChatAction::None,
            },
        }
    }

    fn apply_setup_outcome(
        &mut self,
        outcome: hel::hel_second_opinion::SetupOutcome,
    ) -> super::ChatAction {
        use hel::hel_second_opinion::SetupOutcome;

        match outcome {
            SetupOutcome::None => super::ChatAction::None,
            SetupOutcome::Requests(requests) => {
                super::ChatAction::SecondOpinion(SecondOpinionIntent::Setup(requests))
            }
            SetupOutcome::Confirmed { selection } => {
                super::ChatAction::SecondOpinion(SecondOpinionIntent::Confirmed {
                    profile_id: selection.profile_id,
                    model: selection.model,
                    effort: selection.effort,
                })
            }
            SetupOutcome::Cancelled { requests } => {
                if let Some(SecondOpinion::Setup { captured, .. }) = self.second_opinion.take() {
                    self.restore_elicitation(captured.request);
                }
                if requests.is_empty() {
                    super::ChatAction::SecondOpinion(SecondOpinionIntent::Closed)
                } else {
                    super::ChatAction::SecondOpinion(SecondOpinionIntent::Setup(requests))
                }
            }
        }
    }

    fn activate_split_action(&mut self) -> super::ChatAction {
        let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() else {
            return super::ChatAction::None;
        };
        let action = &review.action;
        let workflow = &review.workflow;
        // The id is minted before the workflow is borrowed, because both come
        // from the same view state.
        let purpose = match action {
            SplitAction::Transfer => "transfer",
            SplitAction::Implement => "implement",
            SplitAction::Cancel => "cancel",
        };
        let chosen = *action;
        let can_transfer = workflow.can_transfer();
        if chosen == SplitAction::Transfer && !can_transfer {
            // Transfer stays unavailable until the reviewer's current turn has
            // a complete answer; pressing it early does nothing.
            return super::ChatAction::None;
        }
        let command_id = self.next_second_opinion_command_id(purpose);
        let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() else {
            return super::ChatAction::None;
        };
        let requests = match chosen {
            SplitAction::Transfer => review.workflow.transfer(command_id),
            SplitAction::Implement => review.workflow.implement_original(command_id),
            SplitAction::Cancel => review.workflow.cancel(),
        };
        if requests.is_empty() {
            return super::ChatAction::None;
        }
        self.second_opinion = None;
        super::ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests))
    }

    fn cancel_review(&mut self) -> super::ChatAction {
        let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() else {
            return super::ChatAction::None;
        };
        let requests = review.workflow.cancel();
        self.second_opinion = None;
        super::ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests))
    }

    /// A command id for a review step, namespaced so it cannot collide with a
    /// prompt the composer submitted.
    pub(super) fn next_second_opinion_command_id(&mut self, purpose: &str) -> String {
        self.second_opinion_sequence += 1;
        format!("second-opinion-{purpose}-{}", self.second_opinion_sequence)
    }

    /// Activates the split action under the pointer, if any.
    pub(super) fn click_split_action(&mut self, column: u16, row: u16) -> super::ChatAction {
        let Some(action) = self
            .split_action_areas
            .iter()
            .find(|(_, area)| area.contains(ratatui::layout::Position::new(column, row)))
            .map(|(action, _)| *action)
        else {
            return super::ChatAction::None;
        };
        let Some(SecondOpinion::Review(review)) = self.second_opinion.as_mut() else {
            return super::ChatAction::None;
        };
        review.action = action;
        self.activate_split_action()
    }

    /// Scrolls whichever pane the pointer is over.
    pub(super) fn scroll_second_opinion(&mut self, rows: isize) -> bool {
        let height = self.last_viewport_height.max(1);
        let Some(reviewer) = self
            .second_opinion
            .as_mut()
            .and_then(SecondOpinion::reviewer_mut)
        else {
            return false;
        };
        reviewer.scroll_by(rows, height);
        true
    }

    /// The text a reviewer-pane selection covers.
    pub fn reviewer_selection_text(&self, range: &SelectionRange) -> Option<String> {
        self.second_opinion
            .as_ref()
            .and_then(SecondOpinion::reviewer)
            .and_then(|reviewer| reviewer.selection_text(range))
    }
}

/// Draws the waterfall over the chat and reports the rows it owns.
pub(super) fn render_setup(
    frame: &mut ratatui::Frame,
    area: Rect,
    headline: &str,
    setup: &ReviewerSetup,
    form: &mut Form<SetupControl>,
) -> Rect {
    prepare_setup_form(setup, form);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Choose a reviewer ")
        .border_style(Style::default().fg(Color::LightMagenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(headline).style(Style::default().fg(Color::DarkGray)),
        chunks[0],
    );
    let (heading, rows, selected) = match setup.stage() {
        SetupStage::Profile => (
            "Profile",
            setup
                .profiles()
                .iter()
                .map(|profile| format!("{} ({})", profile.id, profile.harness))
                .collect::<Vec<_>>(),
            setup.profile_index(),
        ),
        SetupStage::Model => (
            "Model",
            setup
                .models()
                .iter()
                .map(|choice| choice.name.clone())
                .collect(),
            setup.model_index(),
        ),
        SetupStage::Effort => (
            "Effort",
            setup
                .efforts()
                .iter()
                .map(|choice| choice.name.clone())
                .collect(),
            setup.effort_index(),
        ),
    };
    frame.render_widget(
        Paragraph::new(heading).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[1],
    );
    form.begin_frame();
    if let Some(failure) = setup.failure() {
        frame.render_widget(
            Paragraph::new(failure).style(Style::default().fg(Color::Red)),
            chunks[2],
        );
        ButtonRow::render(
            frame,
            chunks[3],
            &[
                (SetupControl::Retry, "Retry", true),
                (SetupControl::Cancel, "Cancel", true),
            ],
            form,
        );
        frame.render_widget(
            Paragraph::new("Enter retry · Esc cancel").style(Style::default().fg(Color::DarkGray)),
            chunks[4],
        );
        form.end_frame(SetupControl::Retry);
        return inner;
    } else if setup.busy() {
        frame.render_widget(
            Paragraph::new("Starting the reviewer…").style(Style::default().fg(Color::Yellow)),
            chunks[2],
        );
        ButtonRow::render(
            frame,
            chunks[3],
            &[(SetupControl::Cancel, "Cancel", true)],
            form,
        );
        frame.render_widget(
            Paragraph::new("Waiting for reviewer discovery · Esc cancel")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[4],
        );
        form.end_frame(SetupControl::Cancel);
        return inner;
    } else {
        let row_lines = rows
            .iter()
            .map(|row| Line::from(row.clone()))
            .collect::<Vec<_>>();
        ChoiceList::render(
            frame,
            chunks[2],
            &row_lines,
            selected,
            form,
            SetupControl::Options,
        );
        ButtonRow::render(
            frame,
            chunks[3],
            &[
                (SetupControl::Confirm, "Confirm", setup.can_confirm()),
                (
                    SetupControl::Back,
                    "Back",
                    setup.stage() != SetupStage::Profile,
                ),
                (SetupControl::Cancel, "Cancel", true),
            ],
            form,
        );
        frame.render_widget(
            Paragraph::new("↑/↓ choose · Tab controls · Enter confirm · Esc cancel")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[4],
        );
    }
    form.end_frame(SetupControl::Options);
    inner
}

/// Draws the reviewer pane and reports its content area, its first drawn row
/// and its total rows, so the caller can register the selection surface.
pub(super) fn render_reviewer(
    frame: &mut ratatui::Frame,
    area: Rect,
    reviewer: &mut ReviewerPane,
    status: &str,
) -> (Rect, usize, usize) {
    render_reviewer_titled(frame, area, reviewer, status, " Second opinion ", None)
}

/// The same pane under another title, with an optional one-row strip above the
/// transcript. Turn review uses the strip to show which reviewing agents are
/// running and where each has got to.
pub(super) fn render_reviewer_titled(
    frame: &mut ratatui::Frame,
    area: Rect,
    reviewer: &mut ReviewerPane,
    status: &str,
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
    reviewer.ensure_rows(inner.width);
    let height = usize::from(inner.height);
    if reviewer.follow {
        reviewer.top_row = reviewer.rows.len().saturating_sub(height);
    }
    let top = reviewer.top_row;
    let visible = reviewer
        .rows
        .iter()
        .skip(top)
        .take(height)
        .cloned()
        .collect::<Vec<_>>();
    let rows = if visible.is_empty() {
        vec![Line::from(Span::styled(
            status.to_owned(),
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        visible
    };
    let total = reviewer.rows.len().max(rows.len());
    frame.render_widget(Paragraph::new(rows), inner);
    (inner, top, total)
}

/// Draws the split's action bar and reports where each button landed, so a
/// click can pick the same action the keyboard would.
pub(super) fn render_split_actions(
    frame: &mut ratatui::Frame,
    area: Rect,
    workflow: &ReviewWorkflow,
    action: SplitAction,
    status: &str,
    form: &mut Form<SplitControl>,
) -> Vec<(SplitAction, Rect)> {
    form.begin_frame();
    let mut buttons = Vec::new();
    let mut column = area.x;
    let mut button_specs = Vec::new();
    for candidate in SplitAction::ORDER {
        let available = match candidate {
            SplitAction::Transfer => workflow.can_transfer(),
            _ => true,
        };
        let label = candidate.label();
        let width = u16::try_from(label.chars().count() + 4).unwrap_or(u16::MAX);
        if column < area.right() {
            buttons.push((
                candidate,
                Rect::new(column, area.y, width.min(area.right() - column), 1),
            ));
        }
        button_specs.push((
            match candidate {
                SplitAction::Transfer => SplitControl::Transfer,
                SplitAction::Implement => SplitControl::Implement,
                SplitAction::Cancel => SplitControl::Cancel,
            },
            candidate.label(),
            available,
        ));
        column = column.saturating_add(width).saturating_add(1);
    }
    let waiting = match workflow.stage() {
        ReviewStage::GatheringContext { .. } => "asking the planner for context…",
        ReviewStage::Reviewing { .. } => "the reviewer is reading the plan…",
        ReviewStage::Answered { .. } => status,
        _ => status,
    };
    form.focus(match action {
        SplitAction::Transfer => SplitControl::Transfer,
        SplitAction::Implement => SplitControl::Implement,
        SplitAction::Cancel => SplitControl::Cancel,
    });
    ButtonRow::render(frame, area, &button_specs, form);
    let status_column = area.x.saturating_add(
        u16::try_from(
            button_specs
                .iter()
                .map(|(_, label, _)| label.chars().count() + 6)
                .sum::<usize>(),
        )
        .unwrap_or(u16::MAX),
    );
    if status_column < area.right() {
        frame.render_widget(
            Paragraph::new(waiting).style(Style::default().fg(Color::DarkGray)),
            Rect::new(
                status_column,
                area.y,
                area.right() - status_column,
                area.height,
            ),
        );
    }
    form.end_frame(SplitControl::Transfer);
    buttons
}

/// Kept beside the pane so the projection helper and the pane agree on which
/// session id a reviewer's events are folded under.
pub(super) fn reviewer_session_id(primary_session_id: &str) -> String {
    format!("{primary_session_id}-reviewer")
}

/// The same, for one turn-review role.
///
/// One definition, in the host that also folds these journals, so the pane and
/// the review can never disagree about which session id a role's events belong
/// under.
pub(super) fn review_role_session_id(primary_session_id: &str, role: &str) -> String {
    mj_controller::hel_review_host::role_session_id(primary_session_id, role)
}

/// Builds a pane straight from entries, for tests that need a populated
/// reviewer without a live relay behind it.
#[cfg(test)]
pub(super) fn pane_from_entries(entries: Vec<ChatEntry>) -> ReviewerPane {
    ReviewerPane {
        entries,
        follow: true,
        ..ReviewerPane::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_chat::test_support::{key, snapshot};
    use crate::hel_chat::{ChatAction, ChatState};
    use crossterm::event::{KeyCode, KeyModifiers};
    use hel::hel_second_opinion::{HARNESS_DEFAULT_VALUE, ReviewerDefaults, ReviewerProfileChoice};
    use hel::hel_transcript::ChatRole;

    fn plan_review() -> ElicitationRequest {
        hel::hel_acp::normalized_plan_review(
            "plan-review-1".into(),
            &serde_json::json!({ "plan": "1. Read\n2. Change" }),
        )
    }

    fn captured() -> CapturedProposal {
        let request = plan_review();
        let proposal = hel::hel_acp::plan_review_proposal(&request)
            .expect("a normalized plan review carries its proposal")
            .to_owned();
        CapturedProposal { request, proposal }
    }

    fn profiles() -> Vec<ReviewerProfileChoice> {
        vec![
            ReviewerProfileChoice {
                id: "codex".into(),
                harness: "codex".into(),
            },
            ReviewerProfileChoice {
                id: "claude".into(),
                harness: "claude".into(),
            },
        ]
    }

    fn chat_in_setup() -> ChatState {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.open_second_opinion(
            captured(),
            ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
        );
        chat
    }

    fn press(chat: &mut ChatState, code: KeyCode) -> ChatAction {
        chat.handle_key(key(code))
    }

    /// Answering one decision, from the dialog the user actually sees.
    /// `steps` moves the highlight before the answer is accepted.
    fn answer_plan_review(steps: usize) -> ChatAction {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.restore_elicitation(plan_review());
        press(&mut chat, KeyCode::Home);
        for _ in 0..steps {
            press(&mut chat, KeyCode::Down);
        }
        // Enter walks to the accept button and then presses it.
        for _ in 0..8 {
            let action = press(&mut chat, KeyCode::Enter);
            if !matches!(action, ChatAction::None) {
                return action;
            }
        }
        panic!("the decision dialog never produced an answer");
    }

    #[test]
    fn cancelling_reviewer_setup_restores_the_unanswered_plan() {
        let mut chat = chat_in_setup();
        press(&mut chat, KeyCode::Enter);
        press(&mut chat, KeyCode::Esc);
        assert!(!chat.second_opinion_active());
        assert_eq!(
            chat.elicitation
                .as_ref()
                .expect("restored decision")
                .request(),
            &captured().request
        );
    }

    #[test]
    fn choosing_the_second_opinion_never_answers_the_harness() {
        let hel::hel_elicitation::ElicitationFieldKind::SingleSelect { options, .. } =
            &plan_review().fields[0].kind
        else {
            panic!("the decision is a single select");
        };
        // Every reachable decision is answered; exactly one of them is Hel's
        // own, and it must never become an elicitation response.
        let mut local = Vec::new();
        for steps in 0..options.len() {
            match answer_plan_review(steps) {
                ChatAction::StartSecondOpinion { request, proposal } => {
                    assert_eq!(request.id, "plan-review-1");
                    local.push(proposal);
                }
                ChatAction::RespondElicitation { response, .. } => {
                    let hel::hel_elicitation::ElicitationResponse::Accept { content } = response
                    else {
                        panic!("accepting the dialog produces an accept");
                    };
                    assert_ne!(
                        content.get("action"),
                        Some(&hel::hel_elicitation::ElicitationValue::String(
                            hel::hel_acp::PLAN_REVIEW_SECOND_OPINION.to_owned()
                        )),
                        "a second opinion must never reach the harness"
                    );
                }
                other => panic!("unexpected answer {other:?}"),
            }
        }
        assert_eq!(local, vec!["1. Read\n2. Change".to_owned()]);
    }

    #[test]
    fn every_dialect_offers_the_second_opinion() {
        // Both plan-decision paths build their dialog through the same
        // normalizer, so the option is offered whatever the harness sent.
        for value in [
            serde_json::json!({ "plan": "standard permission plan" }),
            serde_json::json!({ "plan_content": "native feedback plan" }),
            serde_json::json!({ "planContent": "switch mode plan" }),
        ] {
            let request = hel::hel_acp::normalized_plan_review("plan-review-1".into(), &value);
            let hel::hel_elicitation::ElicitationFieldKind::SingleSelect { options, .. } =
                &request.fields[0].kind
            else {
                panic!("the decision is a single select");
            };
            assert!(
                options
                    .iter()
                    .any(|option| option.value == hel::hel_acp::PLAN_REVIEW_SECOND_OPINION),
                "a plan decision must always offer a second opinion"
            );
        }
    }

    #[test]
    fn the_waterfall_asks_the_session_to_probe_the_chosen_profile() {
        let mut chat = chat_in_setup();
        assert!(chat.second_opinion_active());

        chat.handle_key(key(KeyCode::Down));
        let action = press(&mut chat, KeyCode::Enter);
        let ChatAction::SecondOpinion(SecondOpinionIntent::Setup(requests)) = action else {
            panic!("confirming a profile probes it: {action:?}");
        };
        assert_eq!(
            requests,
            vec![SetupRequest::Probe {
                generation: 1,
                profile_id: "claude".into(),
            }]
        );
    }

    #[test]
    fn cancelling_the_waterfall_leaves_the_captured_plan_alone() {
        let mut chat = chat_in_setup();
        let action = press(&mut chat, KeyCode::Esc);

        assert!(!chat.second_opinion_active());
        // Nothing was sent to the harness, so its own decision is still
        // pending and will be rebuilt from the projection.
        assert!(matches!(
            action,
            ChatAction::SecondOpinion(SecondOpinionIntent::Closed)
        ));
    }

    /// The split's actions are the whole keyboard: there is no composer,
    /// because the revised plan is the planner's to write.
    #[test]
    fn the_split_cycles_its_actions_and_gates_transfer() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let captured = captured();
        let (workflow, _) =
            ReviewWorkflow::start(captured.id(), captured.proposal.clone(), "context-1");
        chat.open_second_opinion(
            captured,
            ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
        );
        chat.second_opinion_mut()
            .expect("the view is open")
            .begin_review(workflow, "waiting", 0);
        assert!(chat.second_opinion_split());

        // Transfer is first and refuses until the reviewer has answered.
        assert_eq!(press(&mut chat, KeyCode::Enter), ChatAction::None);
        assert!(chat.second_opinion_split(), "a refused transfer stays put");

        // Implementing the original needs no reviewer answer.
        press(&mut chat, KeyCode::Tab);
        let action = press(&mut chat, KeyCode::Enter);
        let ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests)) = action else {
            panic!("implementing the original is a workflow step: {action:?}");
        };
        let [
            WorkflowRequest::PromptPrimary { prompt, .. },
            WorkflowRequest::PauseReviewer,
        ] = requests.as_slice()
        else {
            panic!("implementing prompts the primary and pauses the reviewer");
        };
        assert!(prompt.contains("1. Read\n2. Change"));
        assert!(!chat.second_opinion_active(), "the split closes behind it");
    }

    #[test]
    fn cancelling_the_split_asks_for_the_captured_decision_back() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let (workflow, _) = ReviewWorkflow::start("plan-review-1", "the plan", "context-1");
        chat.open_second_opinion(
            captured(),
            ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
        );
        chat.second_opinion_mut()
            .expect("the view is open")
            .begin_review(workflow, "waiting", 0);

        let action = press(&mut chat, KeyCode::Esc);
        let ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests)) = action else {
            panic!("cancelling is a workflow step: {action:?}");
        };
        assert!(requests.contains(&WorkflowRequest::PauseReviewer));
        assert!(requests.iter().any(|request| matches!(
            request,
            WorkflowRequest::RestoreDecision { proposal, .. } if proposal == "the plan"
        )));
        // Nothing was transferred.
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, WorkflowRequest::PromptPrimary { .. }))
        );
    }

    #[test]
    fn the_reviewer_pane_scrolls_and_copies_from_its_own_rows() {
        let entries = (0..40)
            .map(|index| {
                ChatEntry::plain(index + 1, ChatRole::Agent, format!("reviewer line {index}"))
            })
            .collect::<Vec<_>>();
        let mut pane = pane_from_entries(entries);
        pane.ensure_rows(40);
        let total = pane.rows.len();
        assert!(total > 10, "the fixture must not fit on one screen");

        // Scrolling stops at the last full screen rather than running past it.
        pane.scroll_by(1_000, 10);
        assert_eq!(pane.top_row, total - 10);
        pane.scroll_by(-1_000, 10);
        assert_eq!(pane.top_row, 0);

        let text = pane
            .selection_text(&SelectionRange {
                start: crate::hel_selection::ContentPos::new(0, 0),
                end: crate::hel_selection::ContentPos::new(1, 39),
            })
            .expect("a selection over this pane's rows resolves here");
        assert!(
            text.contains("reviewer line 0"),
            "the pane resolves its own rows: {text:?}"
        );
    }

    /// A live split, drawn once so its panes and buttons have real rects.
    fn drawn_split() -> (ChatState, ratatui::layout::Rect) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut chat = ChatState::new(&snapshot(), &[]);
        // Enough primary history to have somewhere to scroll to, so a wheel
        // that reaches the primary is visible in its anchor.
        chat.entries.extend((1..=60).map(|index| {
            ChatEntry::plain(index, ChatRole::Agent, format!("primary line {index}"))
        }));
        let captured = captured();
        let (mut workflow, _) =
            ReviewWorkflow::start(captured.id(), captured.proposal.clone(), "context-1");
        workflow.primary_context_completed("context-1", "context", "review-1");
        workflow.reviewer_turn_completed("review-1", "the plan misses error handling");
        chat.open_second_opinion(
            captured,
            ReviewerSetup::new("workspace-1", profiles(), ReviewerDefaults::default()),
        );
        chat.second_opinion_mut()
            .expect("the view is open")
            .begin_review(workflow, "ready", 0);

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::hel_chat::active::render_full_frame(frame, &mut chat, false))
            .unwrap();
        let reviewer = chat.reviewer_area.expect("the split draws a reviewer pane");
        (chat, reviewer)
    }

    #[test]
    fn the_wheel_scrolls_whichever_pane_it_is_over() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut chat, reviewer) = drawn_split();
        let primary_before = chat.anchor;

        let wheel = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Over the reviewer: only the reviewer moves.
        chat.handle_mouse(wheel(
            MouseEventKind::ScrollUp,
            reviewer.x + 1,
            reviewer.y + 1,
        ));
        assert_eq!(chat.anchor, primary_before);

        // Outside it: the primary transcript takes the wheel instead.
        chat.handle_mouse(wheel(MouseEventKind::ScrollUp, 1, reviewer.y + 1));
        assert_ne!(
            chat.anchor, primary_before,
            "a wheel outside the reviewer pane scrolls the primary"
        );

        let _ = MouseButton::Left;
    }

    #[test]
    fn clicking_a_split_button_takes_the_same_action_as_the_keyboard() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut chat, _) = drawn_split();
        let (action, area) = chat
            .split_action_areas
            .iter()
            .find(|(action, _)| *action == SplitAction::Implement)
            .copied()
            .expect("the split draws its action buttons");
        assert_eq!(action, SplitAction::Implement);

        let press = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(press, ChatAction::None);
        let outcome = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        let ChatAction::SecondOpinion(SecondOpinionIntent::Workflow(requests)) = outcome else {
            panic!("clicking a button acts on it: {outcome:?}");
        };
        assert!(requests.iter().any(|request| matches!(
            request,
            WorkflowRequest::PromptPrimary { prompt, .. } if prompt.contains("1. Read")
        )));
    }

    #[test]
    fn clicking_beside_the_buttons_does_nothing() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut chat, reviewer) = drawn_split();
        let outcome = chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: reviewer.x + 1,
            row: reviewer.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(outcome, ChatAction::None);
        assert!(chat.second_opinion_split(), "the split stays up");
    }

    /// A reviewer's form must be answered, or the review stalls waiting on a
    /// harness nobody is talking to. It is shown in the ordinary dialog and
    /// its answer is routed back to the reviewer, never to the planner.
    #[test]
    fn a_reviewer_form_is_answered_back_to_the_reviewer() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let form = hel::hel_elicitation::ElicitationRequest {
            id: "reviewer-form-1".into(),
            message: "Allow reading /etc?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        assert!(chat.show_review_role_elicitation(None, form));
        assert!(chat.reviewer_elicitation_open());

        // The primary's own projection must not take a reviewer's form down.
        chat.sync_elicitation(&[]);
        assert!(chat.reviewer_elicitation_open());

        let action = press(&mut chat, KeyCode::Esc);
        let ChatAction::RespondReviewerElicitation { elicitation_id, .. } = action else {
            panic!("a reviewer's answer goes to the reviewer: {action:?}");
        };
        assert_eq!(elicitation_id, "reviewer-form-1");
        assert!(!chat.reviewer_elicitation_open());
    }

    #[test]
    fn the_primary_form_keeps_the_screen_over_a_reviewer_one() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.restore_elicitation(plan_review());
        let reviewer_form = hel::hel_elicitation::ElicitationRequest {
            id: "reviewer-form-1".into(),
            message: "Allow reading /etc?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        // An answer the planning harness is blocked on matters more than one
        // its reviewer is.
        assert!(!chat.show_review_role_elicitation(None, reviewer_form));
        assert!(!chat.reviewer_elicitation_open());
    }

    /// The prompts a review generates are Hel's, not the user's. Rendering
    /// them as user messages would put words in their mouth and would make a
    /// later resume replay them as if they had been typed.
    #[test]
    fn generated_review_prompts_never_read_as_the_user() {
        use hel::hel_second_opinion::{
            PRIMARY_CONTEXT_REQUEST, implement_original_note, is_control_origin_prompt,
            review_request, transfer_note,
        };

        for generated in [
            PRIMARY_CONTEXT_REQUEST.to_owned(),
            review_request("context", "the plan"),
            transfer_note("the review"),
            implement_original_note("the plan"),
        ] {
            assert!(
                is_control_origin_prompt(&generated),
                "a generated prompt must be recognizable as Hel's: {generated:?}"
            );
        }
        // Something a person typed is not, even when it mentions one.
        assert!(!is_control_origin_prompt(
            "please add a [HARNESS NOTE: ...] to the docs"
        ));
        assert!(!is_control_origin_prompt("fix the parser"));

        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.entries.push(ChatEntry::plain(
            1,
            ChatRole::System,
            PRIMARY_CONTEXT_REQUEST,
        ));
        assert_eq!(chat.entries[0].role, ChatRole::System);
    }

    /// A review whose target is gone still has to be readable: the reviewer's
    /// own journal died with it, so the pane is rebuilt from the copy the
    /// controller kept.
    #[test]
    fn a_reviewer_pane_rebuilds_from_a_stored_transcript() {
        let item = std::sync::Arc::new(hel::hel_state::TranscriptItem {
            stable_id: "agent:1".into(),
            position: 1,
            latest_content_event_ordinal: Some(1),
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: hel::hel_state::TranscriptBody::Agent {
                chunks: vec![serde_json::json!({
                    "content": {"type": "text", "text": "the plan misses error handling"}
                })],
                streaming: false,
            },
        });

        let mut pane = ReviewerPane::default();
        assert!(pane.is_empty());
        pane.restore("session-1-reviewer", vec![item]);

        assert!(!pane.is_empty());
        assert_eq!(
            pane.latest_answer().as_deref(),
            Some("the plan misses error handling"),
            "a restored review can still be read, and transferred from"
        );
        // Restoring nothing leaves the pane alone rather than clearing it.
        pane.restore("session-1-reviewer", Vec::new());
        assert!(!pane.is_empty());
    }

    #[test]
    fn an_empty_reviewer_has_no_answer_to_transfer() {
        let pane = pane_from_entries(Vec::new());
        assert!(pane.is_empty());
        assert_eq!(pane.latest_answer(), None);
    }

    #[test]
    fn a_harness_default_selection_is_stored_under_its_sentinel() {
        let selection = hel::hel_second_opinion::ReviewerSelection {
            profile_id: "codex".into(),
            model: None,
            effort: Some("high".into()),
        };
        assert_eq!(
            selection.stored_values(),
            ("codex", HARNESS_DEFAULT_VALUE, "high")
        );

        let mut defaults = ReviewerDefaults::default();
        let (profile, model, effort) = selection.stored_values();
        defaults.restore("workspace-1", profile, model, effort);
        assert_eq!(defaults.profile("workspace-1"), Some("codex"));
        assert_eq!(
            defaults.model("workspace-1", "codex"),
            Some(HARNESS_DEFAULT_VALUE)
        );
        assert_eq!(
            defaults.effort("workspace-1", "codex", HARNESS_DEFAULT_VALUE),
            Some("high")
        );
    }

    #[test]
    fn a_review_ignores_a_context_answer_that_predates_its_request() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.entries.push(ChatEntry::plain(
            3,
            ChatRole::Agent,
            "an answer from before the review",
        ));
        // The baseline is the frontier when the request went out, so only a
        // later message can be the answer to it.
        assert_eq!(chat.latest_agent_text_after(3), None);
        assert_eq!(
            chat.latest_agent_text_after(2).as_deref(),
            Some("an answer from before the review")
        );

        let _ = KeyModifiers::NONE;
    }
}
