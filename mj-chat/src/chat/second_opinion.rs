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

use crate::theme;
use crossterm::event::{Event, KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::components::{ButtonRow, ChoiceList, ControlKind, Form, Interaction};
use crate::selection::{SelectionRange, SurfaceFrame, SurfaceId};
use mj_core::elicitation::ElicitationRequest;
use mj_core::relay::RelayEvent;
use mj_core::second_opinion::{
    ReviewStage, ReviewWorkflow, ReviewerSetup, SetupRequest, SetupStage, WorkflowRequest,
};
use mj_core::state::MaterializedSession;
use mj_core::transcript::ChatEntry;
use mj_transcript::projection::{apply_committed_projection_event, project_relay_event};

use super::rendering::TranscriptRenderMode;
use super::transcript::{materialized_chat_entries_reusing, render_entry_rows};
use super::viewport::RowViewport;

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

    pub(super) fn set_status(&mut self, text: impl Into<String>) -> bool {
        if let Self::Review(review) = self {
            let text = text.into();
            if review.status != text {
                review.status = text;
                return true;
            }
        }
        false
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
    ) -> bool {
        let Self::Setup { captured, .. } = self else {
            return false;
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
        true
    }

    /// Reports a failure in place, leaving the view up so the user can retry
    /// or cancel rather than losing the captured plan to a dismissed dialog.
    pub(super) fn report_failure(&mut self, message: impl Into<String>) -> bool {
        match self {
            Self::Setup { setup, .. } => {
                setup.probe_failed_current(message);
                true
            }
            Self::Review(review) => {
                let message = message.into();
                if review.status != message {
                    review.status = message;
                    true
                } else {
                    false
                }
            }
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
    theme: theme::UiTheme,
    width: u16,
    /// Where this pane is scrolled to.
    viewport: RowViewport,
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
    pub(super) fn apply_events(&mut self, session_id: &str, events: &[RelayEvent]) -> bool {
        let session = self
            .session
            .get_or_insert_with(|| MaterializedSession::empty(session_id));
        let mut changed = false;
        for event in events {
            if event.ordinal < self.cursor_ordinal
                || (event.ordinal == self.cursor_ordinal && event.digest == self.cursor_digest)
            {
                continue;
            }
            let Ok(projected) = project_relay_event(session, event) else {
                continue;
            };
            if apply_committed_projection_event(session, event, projected.mutation).is_err() {
                continue;
            }
            self.cursor_ordinal = event.ordinal;
            self.cursor_digest.clone_from(&event.digest);
            changed = true;
        }
        if !changed {
            return false;
        }
        // Rebuilt whole rather than incrementally: a reviewer's conversation
        // is one short turn, so the simpler path costs nothing here.
        self.entries = materialized_chat_entries_reusing(session, 0, Vec::new());
        // Rows are rebuilt on the next draw, at whatever width that draw has.
        self.width = 0;
        self.viewport.follow = true;
        true
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
                let mj_core::state::TranscriptBody::Agent { chunks, .. } = &item.body else {
                    return String::new();
                };
                super::transcript::materialized_chunks_text(chunks)
            })
            .filter(|text| !text.trim().is_empty())
    }

    /// What the pane has read of the reviewer's conversation, for the copy
    /// the controller keeps against the target going away.
    pub(super) fn transcript(&self) -> Vec<Arc<mj_core::state::TranscriptItem>> {
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
        transcript: Vec<Arc<mj_core::state::TranscriptItem>>,
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
        self.viewport.follow = true;
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
        if self.width == width && self.theme == theme::current() && !self.rows.is_empty() {
            return;
        }
        self.width = width;
        self.theme = theme::current();
        self.rows = self
            .entries
            .iter()
            .flat_map(|entry| {
                render_entry_rows(entry, usize::from(width), TranscriptRenderMode::Rich)
            })
            .collect();
    }

    /// Scrolls by `delta` rows, leaving follow mode on only at the end.
    pub(super) fn scroll_by(&mut self, delta: isize, height: usize) -> bool {
        self.viewport.scroll_by(delta, self.rows.len(), height)
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
    update_setup_form(setup, form, options_were_available);
    form.end_frame(SetupControl::Options);
}

fn update_setup_form(
    setup: &ReviewerSetup,
    form: &mut Form<SetupControl>,
    options_were_available: bool,
) {
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
    crate::selection::extract_rows(
        &buffer,
        &SurfaceFrame::fixed(SurfaceId::ReviewerTranscript, area),
        &SelectionRange {
            start: crate::selection::ContentPos::new(0, first),
            end: crate::selection::ContentPos::new(0, last),
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
            let consumed = result.consumed;
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
                        (true, self.apply_setup_control(control))
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
            let consumed = result.consumed;
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
            (result.consumed, result.action)
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
                self.apply_setup_control(SetupControl::Confirm)
            }
            Interaction::Activate(SetupControl::Back) => {
                self.apply_setup_control(SetupControl::Back)
            }
            Interaction::Activate(SetupControl::Retry) => {
                self.apply_setup_control(SetupControl::Retry)
            }
            Interaction::Activate(SetupControl::Cancel) | Interaction::Cancel => {
                self.apply_setup_control(SetupControl::Cancel)
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
            (result.consumed, result.action, review.form.focused())
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
        if matches!(self.second_opinion, Some(SecondOpinion::Setup { .. })) {
            let failed = self.second_opinion.as_ref().is_some_and(|view| {
                matches!(view, SecondOpinion::Setup { setup, .. } if setup.failure().is_some())
            });
            return match code {
                KeyCode::Char('r') if failed => self.apply_setup_control(SetupControl::Retry),
                KeyCode::Left | KeyCode::Backspace => self.apply_setup_control(SetupControl::Back),
                KeyCode::Esc => self.apply_setup_control(SetupControl::Cancel),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.apply_setup_control(SetupControl::Cancel)
                }
                _ => super::ChatAction::None,
            };
        }
        let Some(view) = self.second_opinion.as_mut() else {
            return super::ChatAction::None;
        };
        match view {
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
            SecondOpinion::Setup { .. } => super::ChatAction::None,
        }
    }

    fn apply_setup_control(&mut self, control: SetupControl) -> super::ChatAction {
        self.apply_setup_operation(|setup| match control {
            SetupControl::Confirm | SetupControl::Options => setup.confirm(),
            SetupControl::Back => setup.back(),
            SetupControl::Retry => setup.retry(),
            SetupControl::Cancel => setup.cancel(),
        })
    }

    fn apply_setup_operation<F>(&mut self, operation: F) -> super::ChatAction
    where
        F: FnOnce(&mut ReviewerSetup) -> mj_core::second_opinion::SetupOutcome,
    {
        let outcome = match self.second_opinion.as_mut() {
            Some(SecondOpinion::Setup { setup, form, .. }) => {
                let previous_stage = setup.stage();
                let outcome = operation(setup);
                if setup.stage() != previous_stage && !form.captures_pointer() {
                    form.focus(SetupControl::Options);
                }
                outcome
            }
            _ => mj_core::second_opinion::SetupOutcome::None,
        };
        self.apply_setup_outcome(outcome)
    }

    fn apply_setup_outcome(
        &mut self,
        outcome: mj_core::second_opinion::SetupOutcome,
    ) -> super::ChatAction {
        use mj_core::second_opinion::SetupOutcome;

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
        reviewer.scroll_by(rows, height)
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
    let options_were_available = form.is_enabled(SetupControl::Options);
    form.begin_frame();
    update_setup_form(setup, form, options_were_available);
    let title = crate::modal::dismissible_modal_title(
        form,
        area,
        "Choose a reviewer",
        theme::title(true),
        true,
    );
    let block = theme::modal().title(title);
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
        Paragraph::new(headline).style(Style::default().fg(theme::palette().muted)),
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
    if let Some(failure) = setup.failure() {
        frame.render_widget(
            Paragraph::new(failure).style(Style::default().fg(theme::palette().error)),
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
            Paragraph::new("Enter retry · Esc cancel")
                .style(Style::default().fg(theme::palette().muted)),
            chunks[4],
        );
        form.end_frame(SetupControl::Retry);
        return inner;
    } else if setup.busy() {
        frame.render_widget(
            Paragraph::new("Starting the reviewer…")
                .style(Style::default().fg(theme::palette().warning)),
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
                .style(Style::default().fg(theme::palette().muted)),
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
                .style(Style::default().fg(theme::palette().muted)),
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
    reviewer.ensure_rows(inner.width);
    let height = usize::from(inner.height);
    if reviewer.viewport.follow {
        reviewer.viewport.top_row = reviewer.rows.len().saturating_sub(height);
    }
    let top = reviewer.viewport.top_row;
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
            Style::default().fg(theme::palette().muted),
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
            Paragraph::new(waiting).style(Style::default().fg(theme::palette().muted)),
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
    mj_client::review::role_session_id(primary_session_id, role)
}

/// Builds a pane straight from entries, for tests that need a populated
/// reviewer without a live relay behind it.
#[cfg(test)]
pub(super) fn pane_from_entries(entries: Vec<ChatEntry>) -> ReviewerPane {
    ReviewerPane {
        entries,
        viewport: RowViewport {
            top_row: 0,
            follow: true,
        },
        ..ReviewerPane::default()
    }
}

#[cfg(test)]
mod tests;
