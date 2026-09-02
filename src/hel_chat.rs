//! Minimal full-screen chat for one persistent Hel worker.
//!
//! The view state lives here; the concerns around it are split into
//! submodules: [`input`] edits the composer, [`history`] recalls earlier
//! prompts, [`autocomplete`] parses and completes slash commands,
//! [`transcript`] projects and draws the conversation, [`remote`] runs the
//! relay operations a key press asks for, and [`active`] wires a live session
//! to all of them.

mod active;
mod autocomplete;
mod config_picker;
mod elicitation;
mod history;
mod input;
mod remote;
mod rendering;
mod second_opinion;
mod transcript;
mod turn_review;

#[cfg(test)]
mod test_support;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AvailableCommand, SessionConfigOption, SessionModeState, SessionUpdate,
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;

use crate::clock::epoch_seconds;
pub use crate::hel_acp::PlanControl;
use crate::hel_acp::SessionConfigChoice;
use crate::hel_acp::surface::{AcpSessionSurface, PlanControlError};
use crate::hel_acp::{RuntimeEvent, plan_review_carries_native_feedback};
use crate::hel_config::{HarnessKind, HelConfig};
use crate::hel_elicitation::ElicitationValue;
use crate::hel_elicitation::{ElicitationRequest, ElicitationResponse};
use crate::hel_selection::{FrameSurfaces, SelectionRange};
use crate::hel_state::{
    MaterializedExecutionState, MaterializedQueuedPrompt, MaterializedSession, QueuedCommandKind,
    SessionRecord, TranscriptBody, TranscriptItem,
};
#[cfg(test)]
use crate::hel_transcript::PlanStatus;
use crate::hel_transcript::{
    ChatEntry, ChatRole, apply_runtime_event_to_entries, apply_session_update_to_entries,
};
use crate::hel_worker::{
    ActiveAgentTerminal, SequencedEvent, WorkerEvent, WorkerPhase, WorkerSnapshot,
};

use autocomplete::{
    Autocomplete, CommandChoice, LocalCommand, builtin_command_choices, parse_local_command,
    prompt_invokes_command,
};
use config_picker::ConfigPicker;
use elicitation::ElicitationDialog;
use history::{HistorySearch, HistorySearchRequest};
pub use rendering::truncate_line_to_width;
use rendering::{TranscriptRenderMode, sanitize_terminal_text};
use second_opinion::{SecondOpinion, SecondOpinionIntent};
use transcript::{
    ToolDiffstatRequest, TranscriptAnchor, TranscriptRenderCache, TranscriptSelectionSpace,
    materialized_chat_entries_reusing,
};
use turn_review::{TurnReview, TurnReviewIntent};

const MOUSE_SCROLL_ROWS: usize = 3;

pub use active::{ActiveChat, ChatDaemonRequest};
pub use second_opinion::SecondOpinionIntent as SecondOpinionRequest;
pub use transcript::{
    TAIL_SEED_ITEMS, TranscriptSnapshot, format_event_time, materialized_chunks_text,
    materialized_content_text, materialized_tool_diffstats, render_agent_message_head,
    render_agent_message_tail,
};
pub use turn_review::TurnReviewIntent as TurnReviewRequest;

/// What `/review status` reports, on every surface.
///
/// It answers the two questions a person actually has: is every turn reviewed,
/// and is one being reviewed right now.
#[must_use]
pub fn review_status_line(review: &crate::hel_config::ReviewConfig, open: bool) -> String {
    let armed = match (review.enabled, review.reviewer_profile()) {
        (true, Some(profile)) => format!(
            "Reviewing every completed turn with [review] profile {profile:?} ({} tier)",
            review.tier.label()
        ),
        (true, None) => {
            "[review] enabled = true but no profile is named, so nothing can review".to_owned()
        }
        (false, Some(profile)) => format!(
            "Automatic review is off; /review reviews one turn with {profile:?} ({} tier)",
            review.tier.label()
        ),
        (false, None) => {
            "Turn review needs a reviewer: set [review] profile in config.toml".to_owned()
        }
    };
    if open {
        format!("{armed}. A review is open now.")
    } else {
        armed
    }
}

/// Where a host surface has told the chat to draw itself.
///
/// `transcript` and `prompt` are the *outer* rectangles including each block's
/// border. `footer` is `Some` only when the host wants the chat to own the
/// footer row, which it does while the composer has focus. `overlay` is the
/// whole frame: modals and the autocomplete popup are centred and clamped
/// inside it rather than inside the bands above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatRegions {
    pub transcript: Rect,
    pub prompt: Rect,
    pub footer: Option<Rect>,
    pub overlay: Rect,
}

/// What one terminal event asked the chat to do.
///
/// `None` means the event only changed local state, which lets the caller keep
/// draining a paste burst before it redraws. Every exit reports the ordinal the
/// user has now seen, which becomes the session's read receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEventOutcome {
    None,
    Handled,
    /// Tab or Shift-Tab from the composer. The host surface owns focus, so
    /// the chat only reports which way to walk.
    CycleFocus {
        reverse: bool,
    },
    QuitDetach {
        last_seen_event_ordinal: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatAction {
    None,
    Prompt(String),
    RunShell(String),
    RemoveQueuedPrompt {
        id: String,
        text: String,
        kind: QueuedCommandKind,
    },
    SetConfig {
        key: String,
        value: String,
    },
    PlanCommand {
        original: String,
        control: PlanControl,
        requested_active: bool,
        prompt: Option<String>,
    },
    Cancel,
    RespondElicitation {
        request: ElicitationRequest,
        response: ElicitationResponse,
    },
    /// The user asked for a second opinion on `request`'s plan. Hel answers
    /// this decision itself, so the harness's review stays pending until the
    /// reviewer is set up.
    StartSecondOpinion {
        request: ElicitationRequest,
        /// The proposal text as the harness sent it.
        proposal: String,
    },
    /// Work the second-opinion view asked the session to perform.
    SecondOpinion(SecondOpinionIntent),
    /// Review the turn that just finished, on the user's explicit request.
    /// Auto-review starts the same path without a key press.
    StartTurnReview,
    /// Work the turn-review view asked the session to perform.
    TurnReview(TurnReviewIntent),
    /// An answer to a form a reviewing harness is waiting on. It is routed to
    /// the role that asked, never to the primary: a turn review can have
    /// several harnesses waiting at once.
    RespondReviewerElicitation {
        role: Option<String>,
        elicitation_id: String,
        response: ElicitationResponse,
    },
    PasteFromClipboard,
    ToggleVoice,
    /// Tab or Shift-Tab with no completion popup open.
    CycleFocus {
        reverse: bool,
    },
    QuitDetach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedPrompt {
    id: String,
    text: String,
    kind: QueuedCommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanReviewFollowup {
    desired_active: bool,
    control: Option<PlanControl>,
    prompt: Option<String>,
}

impl QueuedPrompt {
    /// The label shown above the composer. A queued configuration change is
    /// marked so it is never mistaken for a prompt waiting to be sent.
    fn queue_label(&self) -> &'static str {
        if self.kind.is_prompt() {
            "queued"
        } else {
            "queued config"
        }
    }
}

/// The stable columns the conversation's title shows, snapshotted when the
/// chat opens.
#[derive(Debug, Clone, Default)]
pub struct SessionHeaderIdentity {
    /// Session-list target label, including the project suffix for bare
    /// targets.
    pub target: String,
    /// Profile column from the session list's live-session summary.
    pub profile: String,
    /// Harness the session runs, so the chat can answer harness-specific
    /// questions (like whether Codex exposes plan mode) without a recovery
    /// context, which the daemon now owns.
    pub harness_kind: Option<HarnessKind>,
}

/// The session facts the chat needs to run reviewers and keep per-workspace
/// review settings: the config's harness profiles and targets, and this
/// session's record (workspace, target, identity). Snapshotted when the chat
/// opens and refreshed by the surface when the daemon publishes newer state;
/// the recovery observer that used to travel with these stayed in the daemon.
#[derive(Debug, Clone)]
pub struct ChatSessionContext {
    pub config: HelConfig,
    pub session: SessionRecord,
}

pub struct ChatState {
    session_id: String,
    bundle_id: Option<String>,
    phase: WorkerPhase,
    latest_seq: u64,
    last_compaction_seq: u64,
    entries: Vec<ChatEntry>,
    pending_diffstats: VecDeque<ToolDiffstatRequest>,
    scheduled_diffstats: BTreeSet<(String, u64)>,
    /// Leading transcript items that are not converted to entries yet, because
    /// a large session opens on its tail and converts the rest off the event
    /// loop. Zero whenever the projection is complete.
    unconverted_prefix: usize,
    /// The last unconverted transcript item: the projection item a pending
    /// prefix has to end at to be spliced in front of the tail. `None`
    /// whenever the projection is complete.
    prefix_seam: Option<Arc<TranscriptItem>>,
    /// The session actor has not produced its first relay projection yet.
    /// Empty transcripts render a loading marker until that connection attempt
    /// either yields a snapshot or fails.
    transcript_loading: bool,
    input: String,
    input_cursor: usize,
    /// Stored prompts from other sessions in this project, oldest-first.
    project_history: Vec<String>,
    /// Stored prompts from this session, oldest-first.
    session_history: Vec<String>,
    project_history_error: Option<String>,
    prompt_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    kill_buffer: String,
    /// Set by Ctrl-K so the next Ctrl-K appends instead of replacing.
    chain_kill: bool,
    preferred_column: Option<usize>,
    history_search: Option<HistorySearch>,
    next_history_search_generation: u64,
    pending_history_search: Option<HistorySearchRequest>,
    queued_prompts: VecDeque<QueuedPrompt>,
    /// Queue entries optimistically moved back into the composer. A relay
    /// snapshot can still contain one until its removal command is projected,
    /// so keep its identity hidden across those stale snapshots.
    pending_queue_removals: BTreeSet<String>,
    active_user_shells: Vec<String>,
    active_agent_terminals: Vec<ActiveAgentTerminal>,
    claimed_agent_terminals: BTreeMap<String, i64>,
    elicitation: Option<ElicitationDialog>,
    /// The second-opinion view, when one is open. It owns the frame while it
    /// is up, so the composer and the elicitation dialog stand down.
    second_opinion: Option<SecondOpinion>,
    /// Where the reviewer pane sat on the last frame, so hover can decide
    /// which transcript the wheel drives.
    reviewer_area: Option<Rect>,
    /// Where the split's action buttons sat on the last frame, so a click
    /// picks the same action the keyboard would.
    split_action_areas: Vec<(second_opinion::SplitAction, Rect)>,
    /// Distinguishes the command ids the review's own steps submit.
    second_opinion_sequence: u64,
    /// The turn-review view, when one is open. Like the second opinion, it
    /// owns the frame while it is up, which is what makes review synchronous:
    /// findings can never land in the middle of the next conversation.
    turn_review: Option<Box<TurnReview>>,
    /// Where the turn review's action buttons sat on the last frame.
    turn_review_action_areas: Vec<(turn_review::ReviewAction, Rect)>,
    /// What `[review]` says, mirrored from the config the TUI already drains,
    /// so `/review status` and the composer title can report it.
    review_config: crate::hel_config::ReviewConfig,
    /// Whether the dialog on screen belongs to the reviewer rather than the
    /// primary, so its answer is routed to the harness that asked.
    elicitation_is_reviewers: bool,
    /// Which reviewing role asked the form on screen, when one did.
    elicitation_role: Option<String>,
    goal_prompt_active: bool,
    acp_surface: AcpSessionSurface,
    plan_command_pending: bool,
    command_choices: Vec<CommandChoice>,
    model_values: Vec<SessionConfigChoice>,
    effort_values: Vec<SessionConfigChoice>,
    autocomplete: Option<Autocomplete>,
    /// The `/model` / `/effort` value selector, when one is open. It owns the
    /// keyboard while it is up, though an arriving elicitation still wins.
    config_picker: Option<ConfigPicker>,
    anchor: TranscriptAnchor,
    /// On entry, reveal the response advertised by the session list when later
    /// tool activity would otherwise push it above the first viewport.
    reveal_latest_agent_on_draw: bool,
    last_viewport_height: usize,
    render_mode: TranscriptRenderMode,
    render_cache: TranscriptRenderCache,
    notices: Notices,
    voice_active: bool,
    /// Session-list identity snapshotted when the chat opened.
    header_target: String,
    header_profile: String,
    turn_started_at_epoch_seconds: Option<u64>,
    /// Whether a prompt of ours is in flight. `phase` also goes Running for a
    /// turn the harness started on its own, which the relay refuses to cancel,
    /// so cancellation and the composer's cancel hint key on this instead.
    prompt_in_flight: bool,
    /// What the session is doing beyond `phase`: the turn the harness started
    /// on its own, and the commands the agent left running.
    session_activity: crate::usage_format::SessionActivity,
    last_acp_activity_at_ms: Option<u64>,
    /// Selectable surfaces, rebuilt by every frame in render order so the
    /// selection engine can hit-test the screen the user is looking at.
    pub(super) frame_surfaces: FrameSurfaces,
    /// The last frame's surfaces replace everything behind them, because a
    /// modal owned the frame. A host that composes the chat with its own
    /// panes reads this to decide whether to merge or replace.
    pub(super) frame_surfaces_exclusive: bool,
    /// The row space transcript selections are measured in, re-pinned by every
    /// frame the engine is not holding a transcript selection through.
    transcript_selection: Option<TranscriptSelectionSpace>,
    /// The frozen row space stopped describing the rows on screen. Read and
    /// cleared after each draw; the caller drops the selection.
    transcript_selection_invalid: bool,
    /// Bumped whenever the cached rows are dropped wholesale, so a frozen row
    /// space can tell that the rows it was pinned against are gone.
    render_cache_generation: u64,
}

impl ChatState {
    pub fn new(snapshot: &WorkerSnapshot, events: &[SequencedEvent]) -> Self {
        let mut state = Self {
            session_id: snapshot.session_id.clone(),
            bundle_id: None,
            phase: snapshot.phase,
            latest_seq: 0,
            last_compaction_seq: 0,
            entries: Vec::new(),
            pending_diffstats: VecDeque::new(),
            scheduled_diffstats: BTreeSet::new(),
            unconverted_prefix: 0,
            prefix_seam: None,
            transcript_loading: false,
            input: String::new(),
            input_cursor: 0,
            project_history: Vec::new(),
            session_history: Vec::new(),
            project_history_error: None,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            kill_buffer: String::new(),
            chain_kill: false,
            preferred_column: None,
            history_search: None,
            next_history_search_generation: 0,
            pending_history_search: None,
            queued_prompts: VecDeque::new(),
            pending_queue_removals: BTreeSet::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            claimed_agent_terminals: BTreeMap::new(),
            elicitation: None,
            second_opinion: None,
            reviewer_area: None,
            split_action_areas: Vec::new(),
            second_opinion_sequence: 0,
            turn_review: None,
            turn_review_action_areas: Vec::new(),
            review_config: crate::hel_config::ReviewConfig::default(),
            elicitation_is_reviewers: false,
            elicitation_role: None,
            goal_prompt_active: snapshot
                .active_prompt
                .as_ref()
                .is_some_and(|prompt| prompt_invokes_command(&prompt.text, "goal")),
            acp_surface: AcpSessionSurface::from_configuration(&snapshot.config),
            plan_command_pending: false,
            command_choices: builtin_command_choices(),
            model_values: Vec::new(),
            effort_values: Vec::new(),
            autocomplete: None,
            config_picker: None,
            anchor: TranscriptAnchor::Bottom,
            reveal_latest_agent_on_draw: true,
            last_viewport_height: 0,
            render_mode: TranscriptRenderMode::Rich,
            render_cache: TranscriptRenderCache::default(),
            notices: Notices::default(),
            voice_active: false,
            header_target: String::new(),
            header_profile: String::new(),
            turn_started_at_epoch_seconds: None,
            prompt_in_flight: snapshot.active_prompt.is_some(),
            session_activity: crate::usage_format::SessionActivity::default(),
            last_acp_activity_at_ms: None,
            frame_surfaces: FrameSurfaces::new(),
            frame_surfaces_exclusive: false,
            transcript_selection: None,
            transcript_selection_invalid: false,
            render_cache_generation: 0,
        };
        state.apply_events(events);
        // Bootstrap replays the full canonical log for transcript projection,
        // while the snapshot is authoritative for the queue at that frontier.
        state.queued_prompts = snapshot
            .queued_prompts
            .iter()
            .map(|prompt| QueuedPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
                kind: QueuedCommandKind::Prompt,
            })
            .collect();
        state.latest_seq = state.latest_seq.max(snapshot.latest_seq);
        state
    }

    pub fn from_tail(
        session_id: String,
        phase: WorkerPhase,
        latest_seq: u64,
        entries: Vec<ChatEntry>,
    ) -> Self {
        let snapshot = WorkerSnapshot::summary(session_id, phase, latest_seq);
        let mut state = Self::new(&snapshot, &[]);
        state.entries = entries;
        state
    }

    pub fn from_materialized(
        session: &MaterializedSession,
        config_options: &[SessionConfigOption],
        available_commands: &[AvailableCommand],
    ) -> Self {
        Self::from_materialized_with_prefix(session, config_options, available_commands, 0)
    }

    /// Like `from_materialized`, but a session longer than `TAIL_SEED_ITEMS`
    /// converts only its tail here. The caller converts the recorded prefix off
    /// the event loop and hands it back to `splice_transcript_prefix`, so
    /// opening a long conversation costs the tail rather than the history.
    pub fn from_materialized_tail(
        session: &MaterializedSession,
        config_options: &[SessionConfigOption],
        available_commands: &[AvailableCommand],
    ) -> Self {
        let prefix = session.transcript.len().saturating_sub(TAIL_SEED_ITEMS);
        Self::from_materialized_with_prefix(session, config_options, available_commands, prefix)
    }

    fn from_materialized_with_prefix(
        session: &MaterializedSession,
        config_options: &[SessionConfigOption],
        available_commands: &[AvailableCommand],
        unconverted_prefix: usize,
    ) -> Self {
        let phase = match session.execution {
            MaterializedExecutionState::Idle => WorkerPhase::Idle,
            MaterializedExecutionState::Running { .. } => WorkerPhase::Running,
            MaterializedExecutionState::Closing => WorkerPhase::Closing,
            MaterializedExecutionState::Closed => WorkerPhase::Closed,
        };
        let snapshot = WorkerSnapshot::summary(
            session.session_id.clone(),
            phase,
            session.applied_event_ordinal,
        );
        let mut state = Self::new(&snapshot, &[]);
        state.latest_seq = u64::MAX;
        state.unconverted_prefix = unconverted_prefix;
        state.apply_materialized(session, config_options, available_commands);
        state
    }

    pub fn apply_materialized(
        &mut self,
        session: &MaterializedSession,
        config_options: &[SessionConfigOption],
        available_commands: &[AvailableCommand],
    ) {
        let rebuild_projection = session.applied_event_ordinal != self.latest_seq;
        self.phase = match session.execution {
            MaterializedExecutionState::Idle => WorkerPhase::Idle,
            MaterializedExecutionState::Running { .. } => WorkerPhase::Running,
            MaterializedExecutionState::Closing => WorkerPhase::Closing,
            MaterializedExecutionState::Closed => WorkerPhase::Closed,
        };
        self.latest_seq = session.applied_event_ordinal;
        // The controller's projection is authoritative for the turn clock.
        self.turn_started_at_epoch_seconds = turn_started_at_epoch_seconds(session.execution);
        self.sync_elicitation(&session.pending_elicitations);
        if rebuild_projection {
            // While a prefix conversion is in flight the entries stand for the
            // tail only, so the rebuild has to line up with the same tail.
            // Compaction can shrink the transcript under the recorded prefix;
            // reseat it on the current tail rather than rebuilding the whole
            // history here. The pending prefix then fails its alignment check
            // and is rebuilt off the loop.
            if self.unconverted_prefix > session.transcript.len() {
                self.unconverted_prefix = session.transcript.len().saturating_sub(TAIL_SEED_ITEMS);
                self.entries.clear();
                self.invalidate_render_cache();
            }
            self.entries = materialized_chat_entries_reusing(
                session,
                self.unconverted_prefix,
                std::mem::take(&mut self.entries),
            );
            // Reusing entry rows is safe only after the collapse topology is
            // recomputed. A tool can become completed without changing the
            // transcript length, joining or splitting a collapsed streak.
            self.invalidate_render_cache();
            // Re-read the seam from the projection that produced this tail, so
            // a prefix converted against replaced history is refused.
            self.prefix_seam = self
                .unconverted_prefix
                .checked_sub(1)
                .and_then(|index| session.transcript.get(index))
                .cloned();
            for item in session.transcript.iter().skip(self.unconverted_prefix) {
                let Some(request) = ToolDiffstatRequest::from_item(item) else {
                    continue;
                };
                let key = (request.tool_call_id.clone(), request.revision);
                if self.scheduled_diffstats.insert(key) {
                    self.pending_diffstats.push_back(request);
                }
            }
        }
        // Queue persistence can reach the materialized view independently of
        // transcript projection. Keep the small queue authoritative even when
        // the transcript frontier has not moved and its expensive rebuild is
        // correctly skipped.
        let projected_queue_ids = session
            .queued_prompts
            .iter()
            .map(|prompt| prompt.command_id.as_str())
            .collect::<BTreeSet<_>>();
        self.pending_queue_removals
            .retain(|id| projected_queue_ids.contains(id.as_str()));
        self.queued_prompts = session
            .queued_prompts
            .iter()
            .filter(|prompt| !self.pending_queue_removals.contains(&prompt.command_id))
            .map(|prompt| QueuedPrompt {
                id: prompt.command_id.clone(),
                text: materialized_content_text(&prompt.content),
                kind: prompt.kind.clone(),
            })
            .collect();
        self.set_config_options(config_options);
        self.acp_surface
            .apply_projected_configuration(&session.configuration);
        self.acp_surface
            .set_agent_commands(available_commands.to_vec());
        self.rebuild_command_choices();
    }

    fn take_diffstat_requests(&mut self, maximum: usize) -> Vec<ToolDiffstatRequest> {
        let count = maximum.min(self.pending_diffstats.len());
        self.pending_diffstats.drain(..count).collect()
    }

    fn queue_diffstat_requests(&mut self, requests: Vec<ToolDiffstatRequest>) {
        for request in requests {
            let key = (request.tool_call_id.clone(), request.revision);
            if self.scheduled_diffstats.insert(key) {
                self.pending_diffstats.push_back(request);
            }
        }
    }

    pub(super) fn apply_diffstats(
        &mut self,
        tool_call_id: &str,
        revision: u64,
        result: std::result::Result<Vec<String>, String>,
    ) {
        let key = (tool_call_id.to_owned(), revision);
        let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            entry.tool_call_id.as_deref() == Some(tool_call_id) && entry.revision == revision
        }) else {
            self.scheduled_diffstats.remove(&key);
            return;
        };
        match result {
            Ok(diffstats) => {
                entry.tool_diffstats = diffstats;
                self.invalidate_render_cache();
            }
            Err(error) => {
                self.scheduled_diffstats.remove(&key);
                tracing::warn!(
                    tool_call_id,
                    revision,
                    %error,
                    "could not calculate a tool diff summary"
                );
                self.set_notice(format!("Could not calculate diff summary: {error}"));
            }
        }
    }

    fn sync_elicitation(&mut self, pending: &[ElicitationRequest]) {
        // A reviewer's form is not in the primary's pending list, so the
        // primary's projection must not take it down.
        if self.elicitation_is_reviewers {
            return;
        }
        if self
            .elicitation
            .as_ref()
            .is_some_and(|dialog| pending.iter().any(|request| request.id == dialog.id()))
        {
            return;
        }
        self.elicitation = pending.first().cloned().map(ElicitationDialog::new);
    }

    /// Puts a form the reviewer is waiting on in front of the user.
    ///
    /// The primary's own dialog wins the screen: an answer the planning
    /// harness is blocked on matters more than one its reviewer is.
    /// Puts a reviewing harness's form on screen, remembering which role asked.
    ///
    /// The answer has to go back to that role: in the extended tier several
    /// harnesses run at once, and answering the wrong one leaves the asker
    /// waiting for ever. `None` is the plan reviewer, which is the only
    /// harness the second-opinion split has.
    pub(super) fn show_review_role_elicitation(
        &mut self,
        role: Option<String>,
        request: ElicitationRequest,
    ) -> bool {
        if self.elicitation.is_some() {
            return false;
        }
        self.elicitation_is_reviewers = true;
        self.elicitation_role = role;
        self.elicitation = Some(ElicitationDialog::new(request));
        true
    }

    /// Whether a reviewer's form is currently on screen.
    pub(super) fn reviewer_elicitation_open(&self) -> bool {
        self.elicitation_is_reviewers && self.elicitation.is_some()
    }

    fn restore_elicitation(&mut self, request: ElicitationRequest) {
        if self.elicitation.is_none() {
            self.elicitation = Some(ElicitationDialog::new(request));
        }
    }

    #[cfg(test)]
    pub(crate) fn bounded_entries(
        &self,
        maximum_entries: usize,
        maximum_bytes: usize,
    ) -> Vec<ChatEntry> {
        let start = self.entries.len().saturating_sub(maximum_entries);
        let mut entries = self.entries[start..]
            .iter()
            .cloned()
            .map(ChatEntry::bounded_for_dashboard)
            .collect::<Vec<_>>();
        while entries.len() > 1
            && serde_json::to_vec(&entries).is_ok_and(|body| body.len() > maximum_bytes)
        {
            entries.remove(0);
        }
        entries
    }

    pub fn phase(&self) -> WorkerPhase {
        self.phase
    }

    pub(super) fn set_transcript_loading(&mut self, loading: bool) {
        self.transcript_loading = loading;
    }

    pub fn set_history_context(&mut self, bundle_id: impl Into<String>) {
        self.bundle_id = Some(bundle_id.into());
    }

    pub fn set_session_modes(&mut self, modes: Option<SessionModeState>) {
        self.acp_surface.set_session_modes(modes);
        self.rebuild_command_choices();
    }

    pub fn set_harness_kind(&mut self, harness_kind: HarnessKind) {
        self.acp_surface.set_harness_kind(harness_kind);
        self.rebuild_command_choices();
    }

    fn supports_plan_mode(&self) -> bool {
        self.acp_surface.supports_plan_mode()
    }

    fn supports_fast_mode(&self) -> bool {
        self.acp_surface.supports_fast_mode()
    }

    pub(super) fn fast_mode_active(&self) -> bool {
        self.acp_surface.fast_mode_active()
    }

    fn plan_control(&self, active: bool) -> Result<PlanControl, &'static str> {
        self.acp_surface
            .plan_control(active)
            .map_err(plan_control_error_message)
    }

    fn plan_mode_active(&self) -> bool {
        self.acp_surface.plan_mode_active()
    }

    pub(super) fn begin_plan_mode_change(&mut self, active: bool) {
        self.acp_surface.begin_plan_mode_change(active);
    }

    pub(super) fn finish_plan_mode_change(&mut self, active: bool) {
        self.acp_surface.finish_plan_mode_change(active);
    }

    #[cfg(test)]
    pub(super) fn current_mode(&self) -> Option<&str> {
        self.acp_surface.current_mode()
    }

    pub(super) fn current_model(&self) -> Option<&str> {
        self.acp_surface.current_model()
    }

    pub(super) fn current_effort(&self) -> Option<&str> {
        self.acp_surface.current_effort()
    }

    fn plan_review_followup(
        &self,
        request: &ElicitationRequest,
        response: &ElicitationResponse,
    ) -> Option<PlanReviewFollowup> {
        if !crate::hel_acp::is_plan_review_id(&request.id) {
            return None;
        }
        let ElicitationResponse::Accept { content } = response else {
            return Some(PlanReviewFollowup {
                desired_active: true,
                control: None,
                prompt: None,
            });
        };
        let action = match content.get("action") {
            Some(ElicitationValue::String(action)) => action.as_str(),
            _ => "keep_planning",
        };
        let feedback = match content.get("feedback") {
            Some(ElicitationValue::String(feedback)) if !feedback.trim().is_empty() => {
                Some(feedback.clone())
            }
            _ => None,
        };
        Some(match action {
            "implement" => PlanReviewFollowup {
                desired_active: false,
                control: None,
                prompt: None,
            },
            "exit" => PlanReviewFollowup {
                desired_active: false,
                control: self.plan_control(false).ok(),
                prompt: None,
            },
            "revise" => PlanReviewFollowup {
                desired_active: true,
                control: None,
                // Grok carries feedback in its native response. Standard ACP
                // permission responses cannot, so send it as the next planning turn.
                prompt: (!plan_review_carries_native_feedback(&request.id))
                    .then_some(feedback)
                    .flatten(),
            },
            _ => PlanReviewFollowup {
                desired_active: true,
                control: None,
                prompt: None,
            },
        })
    }

    /// Installs the stable session-list columns used by the conversation title.
    pub fn set_header_summary(&mut self, target: impl Into<String>, profile: impl Into<String>) {
        self.header_target = target.into();
        self.header_profile = profile.into();
    }

    /// Records whether the session has a prompt of ours in flight, which is
    /// what the relay accepts a cancellation for.
    pub(super) fn set_prompt_in_flight(&mut self, in_flight: bool) {
        self.prompt_in_flight = in_flight;
    }

    #[must_use]
    pub(super) fn prompt_in_flight(&self) -> bool {
        self.prompt_in_flight
    }

    /// Records what the session is doing beyond its phase, so the pane title
    /// and the composer can name background work.
    pub(super) fn set_session_activity(&mut self, activity: crate::usage_format::SessionActivity) {
        self.session_activity = activity;
    }

    #[must_use]
    pub(super) fn session_activity(&self) -> &crate::usage_format::SessionActivity {
        &self.session_activity
    }

    fn set_last_acp_activity(&mut self, timestamp_ms: Option<i64>) {
        self.last_acp_activity_at_ms = timestamp_ms.and_then(|value| u64::try_from(value).ok());
    }

    /// The full text of the newest agent message recorded after `seq`.
    ///
    /// A review's context request is answered by the planner's next agent
    /// message, so this is how the answer to a specific request is picked out
    /// of the conversation rather than by taking whatever is last.
    pub(super) fn latest_agent_text_after(&self, seq: u64) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|entry| {
                entry.role == ChatRole::Agent
                    && entry.start_seq > seq
                    && !entry.text.trim().is_empty()
            })
            .map(|entry| entry.text.clone())
    }

    pub fn latest_seq(&self) -> u64 {
        self.latest_seq
    }

    /// Mirrors `[review]` into the view, so `/review status` and the composer
    /// title report what the daemon is actually armed with.
    pub fn set_review_config(&mut self, review: crate::hel_config::ReviewConfig) {
        self.review_config = review;
    }

    #[must_use]
    pub(super) fn review_config(&self) -> &crate::hel_config::ReviewConfig {
        &self.review_config
    }

    fn mark_prompt_submitted(&mut self, prompt: &str) {
        self.phase = WorkerPhase::Running;
        self.prompt_in_flight = true;
        self.goal_prompt_active = prompt_invokes_command(prompt, "goal");
        self.notices.clear();
        // Local echo: start the clock now so the header moves with the send.
        // The next materialized update replaces this with the recorded start.
        self.turn_started_at_epoch_seconds = Some(epoch_seconds());
    }

    /// Starts the header clock for a turn the event log just reported. An
    /// event with no recorded time falls back to now, because the turn is
    /// running either way.
    fn start_turn_clock(&mut self, recorded_at_ms: Option<i64>) {
        self.turn_started_at_epoch_seconds = recorded_at_ms
            .and_then(|recorded_at_ms| u64::try_from(recorded_at_ms).ok())
            .map(|recorded_at_ms| recorded_at_ms / 1_000)
            .or_else(|| Some(epoch_seconds()));
    }

    fn pursuing_goal(&self) -> bool {
        self.goal_prompt_active && self.acp_surface.advertises_command("goal")
    }
    pub fn entries(&self) -> &[ChatEntry] {
        &self.entries
    }

    /// Convert a legacy/import transcript projection into the controller's
    /// canonical logical-session model. Native importers use this at their
    /// boundary; live relay sessions are projected directly from relay events.
    pub fn materialized_session(&self) -> MaterializedSession {
        let mut configuration = BTreeMap::new();
        if let Some(model) = self.acp_surface.current_model() {
            configuration.insert("model".into(), serde_json::Value::String(model.to_owned()));
        }
        if let Some(effort) = self.acp_surface.current_effort() {
            configuration.insert(
                "effort".into(),
                serde_json::Value::String(effort.to_owned()),
            );
        }
        crate::hel_projection::materialized_session_from_entries(
            &self.session_id,
            &self.entries,
            self.latest_seq,
            self.phase,
            configuration,
            self.queued_prompts
                .iter()
                .map(|prompt| MaterializedQueuedPrompt {
                    command_id: prompt.id.clone(),
                    kind: prompt.kind.clone(),
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": prompt.text,
                    })],
                    queued_at_ms: 0,
                })
                .collect(),
            self.elicitation
                .as_ref()
                .map(|dialog| vec![dialog.request().clone()])
                .unwrap_or_default(),
        )
    }

    pub fn queued_prompt_snapshot(&self) -> Vec<crate::hel_worker::QueuedPrompt> {
        self.queued_prompts
            .iter()
            .map(|prompt| crate::hel_worker::QueuedPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
                attachments: Vec::new(),
                created_at_ms: 0,
            })
            .collect()
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notices.set(notice);
    }

    /// The current shared notice, if any.
    pub fn notice(&self) -> Option<String> {
        self.notices.current()
    }

    pub fn apply_events(&mut self, events: &[SequencedEvent]) {
        let original_seq = self.latest_seq;
        for event in events {
            if event.seq <= self.latest_seq {
                continue;
            }
            self.apply_event(event);
            self.latest_seq = event.seq;
        }
        if self.latest_seq != original_seq {
            self.invalidate_render_cache();
        }
    }

    fn reset_interaction(&mut self) {
        self.prompt_history.clear();
        self.history_index = None;
        self.history_draft.clear();
        self.preferred_column = None;
        self.history_search = None;
        self.queued_prompts.clear();
        self.autocomplete = None;
        self.anchor = TranscriptAnchor::Bottom;
        self.reveal_latest_agent_on_draw = true;
        self.last_viewport_height = 0;
        self.render_mode = TranscriptRenderMode::Rich;
        self.notices.clear();
        self.voice_active = false;
    }

    fn set_input(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.len();
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    fn clear_input(&mut self) {
        self.set_input(String::new());
    }

    /// Reinstate the input saved when the user last detached, leaving the
    /// cursor at the end. An empty draft leaves the composer alone.
    fn restore_draft(&mut self, draft: String) {
        if draft.is_empty() {
            return;
        }
        self.set_input(draft);
    }

    fn edit_latest_queued_prompt(&mut self) -> ChatAction {
        let Some(queued) = self.queued_prompts.pop_back() else {
            return ChatAction::None;
        };
        self.pending_queue_removals.insert(queued.id.clone());
        self.set_input(queued.text.clone());
        self.set_notice(if queued.kind.is_prompt() {
            "Editing the most recently queued prompt"
        } else {
            "Editing the most recently queued configuration change"
        });
        ChatAction::RemoveQueuedPrompt {
            id: queued.id,
            text: queued.text,
            kind: queued.kind,
        }
    }

    fn fail_queued_prompt_removal(&mut self, id: String, text: String, kind: QueuedCommandKind) {
        self.pending_queue_removals.remove(&id);
        if !self.queued_prompts.iter().any(|prompt| prompt.id == id) {
            self.queued_prompts
                .push_back(QueuedPrompt { id, text, kind });
        }
    }

    fn submit_input(&mut self) -> ChatAction {
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return ChatAction::None;
        }
        if self.plan_command_pending {
            self.set_notice("A plan-mode transition is still in progress");
            return ChatAction::None;
        }
        if let Some(command) = prompt.strip_prefix('!') {
            if command.trim().is_empty() {
                self.set_notice("usage: !<bash command>");
                return ChatAction::None;
            }
            if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                self.set_notice("The worker is closing; this shell command was not sent");
                return ChatAction::None;
            }
            self.record_prompt_history(&prompt);
            self.clear_input();
            return ChatAction::RunShell(command.to_owned());
        }
        if let Some((command, args)) = parse_local_command(&prompt) {
            return match command {
                LocalCommand::Help => {
                    self.clear_input();
                    self.show_help();
                    ChatAction::None
                }
                // There is no second screen to return to any more, so
                // /detach now means what the word says: leave Hel with the
                // session still running on its target.
                LocalCommand::Detach => {
                    self.clear_input();
                    ChatAction::QuitDetach
                }
                LocalCommand::Model | LocalCommand::Effort => {
                    let key = if command == LocalCommand::Model {
                        "model"
                    } else {
                        "effort"
                    };
                    if args.is_empty() {
                        if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                            self.set_notice(
                                "The worker is closing; this configuration change was not sent",
                            );
                            return ChatAction::None;
                        }
                        if self.open_config_picker(key) {
                            self.clear_input();
                        } else {
                            self.set_notice(format!(
                                "The agent does not advertise {key} values; usage: /{key} <value>"
                            ));
                        }
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice(
                            "The worker is closing; this configuration change was not sent",
                        );
                        return ChatAction::None;
                    }
                    // A busy agent does not refuse the change: it waits in the
                    // command queue and applies when its turn comes.
                    self.clear_input();
                    ChatAction::SetConfig {
                        key: key.to_owned(),
                        value: args.to_owned(),
                    }
                }
                LocalCommand::Fast => {
                    if !args.is_empty() {
                        self.set_notice("usage: /fast");
                        return ChatAction::None;
                    }
                    if !self.supports_fast_mode() {
                        self.set_notice("Fast mode is unavailable for the active Codex model");
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice(
                            "The worker is closing; this configuration change was not sent",
                        );
                        return ChatAction::None;
                    }
                    let value = if self.fast_mode_active() { "off" } else { "on" };
                    self.clear_input();
                    ChatAction::SetConfig {
                        key: "fast-mode".to_owned(),
                        value: value.to_owned(),
                    }
                }
                LocalCommand::Plan => {
                    let (requested, followup) = match args.to_ascii_lowercase().as_str() {
                        "" => (!self.plan_mode_active(), None),
                        "on" => (true, None),
                        "off" => (false, None),
                        _ => (true, Some(args.to_owned())),
                    };
                    if self.phase != WorkerPhase::Idle {
                        self.set_notice("/plan is only available while the agent is idle");
                        return ChatAction::None;
                    }
                    if requested && self.plan_mode_active() {
                        if let Some(followup) = followup {
                            return self.submit_prompt_with_history(followup, prompt);
                        }
                        self.record_prompt_history(&prompt);
                        self.clear_input();
                        self.set_notice("Plan mode is already on");
                        return ChatAction::None;
                    }
                    if !requested && !self.plan_mode_active() && args.eq_ignore_ascii_case("off") {
                        self.record_prompt_history(&prompt);
                        self.clear_input();
                        self.set_notice("Plan mode is already off");
                        return ChatAction::None;
                    }
                    let control = match self.plan_control(requested) {
                        Ok(control) => control,
                        Err(message) => {
                            self.set_notice(message);
                            return ChatAction::None;
                        }
                    };
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    self.begin_plan_mode_change(requested);
                    self.plan_command_pending = true;
                    self.set_notice(if requested {
                        "Plan mode on"
                    } else {
                        "Plan mode off"
                    });
                    ChatAction::PlanCommand {
                        original: prompt,
                        control,
                        requested_active: requested,
                        prompt: followup,
                    }
                }
                LocalCommand::Review => {
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    let review = &self.review_config;
                    return match args.trim().to_ascii_lowercase().as_str() {
                        // Bare `/review` reviews the turn that just finished,
                        // whether or not automatic review is armed.
                        "" => ChatAction::StartTurnReview,
                        // Arming is configuration, not a session gesture: a
                        // slash command that edited config.toml would change a
                        // machine-wide setting from inside one conversation.
                        "on" | "off" | "quick" | "extended" => {
                            self.set_notice(
                                "automatic review is configured in config.toml: [review] enabled, tier",
                            );
                            ChatAction::None
                        }
                        "status" => {
                            self.set_notice(review_status_line(review, self.turn_review.is_some()));
                            ChatAction::None
                        }
                        _ => {
                            self.set_notice("usage: /review [status]");
                            ChatAction::None
                        }
                    };
                }
                LocalCommand::Implement => {
                    if let Err(message) = self.plan_control(false) {
                        self.set_notice(message);
                        return ChatAction::None;
                    }
                    let instruction = if args.is_empty() {
                        "Implement the approved plan.".to_owned()
                    } else {
                        args.to_owned()
                    };
                    if !self.plan_mode_active() {
                        return self.submit_prompt_with_history(instruction, prompt);
                    }
                    if self.phase != WorkerPhase::Idle {
                        self.set_notice("/implement is only available while the agent is idle");
                        return ChatAction::None;
                    }
                    let control = match self.plan_control(false) {
                        Ok(control) => control,
                        Err(message) => {
                            self.set_notice(message);
                            return ChatAction::None;
                        }
                    };
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    self.begin_plan_mode_change(false);
                    self.plan_command_pending = true;
                    ChatAction::PlanCommand {
                        original: prompt,
                        control,
                        requested_active: false,
                        prompt: Some(instruction),
                    }
                }
            };
        }
        self.submit_prompt(prompt)
    }

    fn submit_prompt(&mut self, prompt: String) -> ChatAction {
        self.submit_prompt_with_history(prompt.clone(), prompt)
    }

    fn submit_prompt_with_history(&mut self, prompt: String, history: String) -> ChatAction {
        if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
            self.set_notice("The worker is closing; this prompt was not sent");
            return ChatAction::None;
        }
        // Review is synchronous. While one is unresolved the agent it reviewed
        // stays where the review found it, so findings can never arrive in the
        // middle of the next turn.
        if self.turn_review_active() {
            self.set_notice("A review of the last turn is open; answer it first");
            return ChatAction::None;
        }
        self.record_prompt_history(&history);
        self.clear_input();
        ChatAction::Prompt(prompt)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ChatAction {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return ChatAction::None;
        }
        // Any key breaks a Ctrl-K chain; only the Ctrl-K arm sets it again.
        let chained = std::mem::take(&mut self.chain_kill);
        let (code, modifiers) = normalize_key(key.code, key.modifiers);

        // Leaving the view is never an answer to the agent, so these two come
        // before the elicitation dialog. A pending elicitation is durable
        // projection state: it is rebuilt from `pending_elicitations` the next
        // time the session is opened, so stepping out loses nothing but field
        // text that was typed and not submitted.
        // The pane dial, detach, and the web viewer are global chords now
        // (Alt-G, Alt-Q, F4). The host catches them before the composer sees
        // them, so the composer has no escape hatch of its own left. For one
        // release the two chords that moved off Control say where they went;
        // remove this arm in the release after.
        if modifiers.contains(KeyModifiers::CONTROL)
            && let KeyCode::Char(moved @ ('g' | 'q')) = code
        {
            self.set_notice(if moved == 'g' {
                "Ctrl-G moved to Alt-G"
            } else {
                "Ctrl-Q moved to Alt-Q"
            });
            return ChatAction::None;
        }

        // A reviewing harness that asked a question is blocked until it is
        // answered, and its dialog is drawn over the split, so the dialog
        // below takes keys before either review view does. Without this the
        // review's own actions would swallow the answer and the harness would
        // wait for ever.
        let reviewing = !self.reviewer_elicitation_open();

        // The second-opinion view owns the frame while it is up: the composer
        // and the plan decision behind it are both part of what it is deciding.
        if reviewing && self.second_opinion_active() {
            return self.handle_second_opinion_key(code, modifiers);
        }

        // A turn review owns the frame on the same terms. Its actions are the
        // only input while it is unresolved, which is what holds the primary
        // agent still until the user has answered the findings.
        if reviewing && self.turn_review_active() {
            return self.handle_turn_review_key(code, modifiers);
        }

        if let Some(dialog) = self.elicitation.as_mut() {
            if code == KeyCode::Char('v')
                && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
            {
                return ChatAction::PasteFromClipboard;
            }
            let request = dialog.request().clone();
            if let Some(response) = dialog.handle_key(code, modifiers) {
                self.elicitation = None;
                if std::mem::take(&mut self.elicitation_is_reviewers) {
                    return ChatAction::RespondReviewerElicitation {
                        role: self.elicitation_role.take(),
                        elicitation_id: request.id,
                        response,
                    };
                }
                // A second opinion is Hel's own decision. Sending it to the
                // harness would consume the plan review before the reviewer
                // exists, so it never becomes an elicitation response.
                if let Some(proposal) =
                    crate::hel_acp::plan_review_second_opinion(&request, &response)
                {
                    let proposal = proposal.to_owned();
                    return ChatAction::StartSecondOpinion { request, proposal };
                }
                return ChatAction::RespondElicitation { request, response };
            }
            return ChatAction::None;
        }

        // The value selector owns the keyboard while it is up; it is checked
        // after the elicitation dialog because the dialog draws on top of it.
        if self.config_picker_active() {
            return self.handle_config_picker_key(code, modifiers);
        }

        if code == KeyCode::Char('v')
            && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return ChatAction::PasteFromClipboard;
        }

        if modifiers.contains(KeyModifiers::ALT) && code == KeyCode::Char('v') {
            return ChatAction::ToggleVoice;
        }

        if self.history_search.is_some() {
            self.handle_history_search_key(code, modifiers);
            return ChatAction::None;
        }

        if code == KeyCode::Esc {
            // Only a prompt of ours can be cancelled. A turn the harness
            // started on its own also reads as Running, and the relay refuses
            // to cancel it, so Esc must not claim to.
            return if self.prompt_in_flight || !self.active_user_shells.is_empty() {
                ChatAction::Cancel
            } else {
                ChatAction::None
            };
        }
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                // Reverse-i-search stays on readline's key. The block above
                // hands every key to an open search first, which is what lets
                // Ctrl-R step to the previous match and Alt-R cycle the
                // search's scope once it is open.
                KeyCode::Char('r') => self.begin_history_search(),
                KeyCode::Char('a') => self.move_to_line_start(true),
                KeyCode::Char('e') => self.move_to_line_end(true),
                KeyCode::Char('b') => self.move_input_cursor(-1),
                KeyCode::Char('f') => self.move_input_cursor(1),
                KeyCode::Char('h') => self.backspace(),
                KeyCode::Char('d') => self.delete(),
                KeyCode::Char('u') => self.kill_to_line_start(),
                KeyCode::Char('k') => {
                    self.kill_to_line_end(chained);
                    self.chain_kill = true;
                }
                KeyCode::Char('w') => {
                    let start = self.previous_word_start();
                    self.kill_range(start..self.input_cursor);
                }
                KeyCode::Char('c') => {
                    // Stash the abandoned prompt so history can recall it.
                    if !self.input.is_empty() {
                        let stashed = std::mem::take(&mut self.input);
                        self.record_prompt_history(&stashed);
                        self.clear_input();
                    }
                }
                KeyCode::Char('y') => self.yank(),
                KeyCode::Char('j') | KeyCode::Char('m') => self.insert_character('\n'),
                KeyCode::Char('p') => {
                    if self.input.is_empty() && !self.queued_prompts.is_empty() {
                        return self.edit_latest_queued_prompt();
                    } else if self.input.is_empty() || self.history_index.is_some() {
                        self.move_history(-1);
                    } else {
                        self.move_vertical(-1);
                    }
                }
                KeyCode::Char('n') => {
                    if self.history_index.is_some() {
                        self.move_history(1);
                    } else {
                        self.move_vertical(1);
                    }
                }
                KeyCode::Left => self.move_word(-1),
                KeyCode::Right => self.move_word(1),
                KeyCode::Backspace => {
                    let start = self.previous_word_start();
                    self.kill_range(start..self.input_cursor);
                }
                KeyCode::Delete => {
                    let end = self.next_word_end();
                    self.kill_range(self.input_cursor..end);
                }
                KeyCode::Home => {
                    self.anchor = TranscriptAnchor::Row { entry: 0, row: 0 };
                }
                KeyCode::End => self.anchor = TranscriptAnchor::Bottom,
                _ => {}
            }
            return ChatAction::None;
        }
        if modifiers.contains(KeyModifiers::ALT) {
            match code {
                // The rendering toggle sits here rather than beside Alt-V
                // because the block above hands every key to an open
                // reverse-i-search first.
                KeyCode::Char('t') => self.toggle_render_mode(),
                KeyCode::Char('b') | KeyCode::Left => self.move_word(-1),
                KeyCode::Char('f') | KeyCode::Right => self.move_word(1),
                KeyCode::Char('d') | KeyCode::Delete => {
                    let end = self.next_word_end();
                    self.kill_range(self.input_cursor..end);
                }
                KeyCode::Backspace => {
                    let start = self.previous_word_start();
                    self.kill_range(start..self.input_cursor);
                }
                KeyCode::Enter => self.insert_character('\n'),
                KeyCode::Up if !self.queued_prompts.is_empty() => {
                    return self.edit_latest_queued_prompt();
                }
                _ => {}
            }
            return ChatAction::None;
        }
        match code {
            KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_character('\n');
                ChatAction::None
            }
            KeyCode::Enter => {
                if self.accept_autocomplete() {
                    ChatAction::None
                } else {
                    self.submit_input()
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                ChatAction::None
            }
            KeyCode::Delete => {
                self.delete();
                ChatAction::None
            }
            // Tab completes an open popup first; with none open it hands the
            // keyboard to the next pane of the combined surface.
            KeyCode::Tab => {
                if self.accept_autocomplete() {
                    ChatAction::None
                } else {
                    ChatAction::CycleFocus { reverse: false }
                }
            }
            KeyCode::BackTab => ChatAction::CycleFocus { reverse: true },
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_character(character);
                ChatAction::None
            }
            KeyCode::Up if self.autocomplete.is_some() => {
                self.move_autocomplete(-1);
                ChatAction::None
            }
            KeyCode::Down if self.autocomplete.is_some() => {
                self.move_autocomplete(1);
                ChatAction::None
            }
            KeyCode::Up => {
                if self.input.is_empty() && !self.queued_prompts.is_empty() {
                    return self.edit_latest_queued_prompt();
                } else if self.input.is_empty() || self.history_index.is_some() {
                    self.move_history(-1);
                } else {
                    self.move_vertical(-1);
                }
                ChatAction::None
            }
            KeyCode::Down => {
                if self.history_index.is_some() {
                    self.move_history(1);
                } else {
                    self.move_vertical(1);
                }
                ChatAction::None
            }
            KeyCode::Left
                if modifiers.contains(KeyModifiers::SHIFT) && !self.queued_prompts.is_empty() =>
            {
                self.edit_latest_queued_prompt()
            }
            KeyCode::Left => {
                self.move_input_cursor(-1);
                ChatAction::None
            }
            KeyCode::Right => {
                self.move_input_cursor(1);
                ChatAction::None
            }
            KeyCode::PageUp => {
                self.scroll_history_up(self.last_viewport_height.max(1));
                ChatAction::None
            }
            KeyCode::PageDown => {
                self.scroll_history_down(self.last_viewport_height.max(1));
                ChatAction::None
            }
            KeyCode::Home => {
                self.move_to_line_start(false);
                ChatAction::None
            }
            KeyCode::End => {
                self.move_to_line_end(false);
                ChatAction::None
            }
            _ => ChatAction::None,
        }
    }

    pub(super) fn set_active_user_shells(&mut self, shells: &[crate::hel_worker::ActiveUserShell]) {
        self.active_user_shells = shells
            .iter()
            .map(|shell| shell.command_id.clone())
            .collect();
    }

    pub(super) fn set_active_agent_terminals(
        &mut self,
        terminals: &[ActiveAgentTerminal],
        session: &MaterializedSession,
    ) {
        self.active_agent_terminals = terminals.to_vec();
        self.claimed_agent_terminals.clear();
        let mut unresolved = terminals
            .iter()
            .map(|terminal| (terminal.terminal_id.as_str(), terminal.started_at_ms))
            .collect::<BTreeMap<_, _>>();
        // Claims are normally on the newest item, so walking backward stops
        // immediately. The full-history path is reserved for the uncommon
        // unclaimed fallback this state exists to cover.
        for item in session.transcript.iter().rev() {
            let TranscriptBody::Tool { terminal_refs, .. } = &item.body else {
                continue;
            };
            for terminal_id in terminal_refs {
                let Some(started_at_ms) = unresolved.get(terminal_id.as_str()) else {
                    continue;
                };
                if item.last_changed_at_ms >= *started_at_ms {
                    self.claimed_agent_terminals
                        .insert(terminal_id.clone(), item.last_changed_at_ms);
                    unresolved.remove(terminal_id.as_str());
                }
            }
            if unresolved.is_empty() {
                break;
            }
        }
    }

    pub(super) fn active_user_shell_ids(&self) -> Vec<String> {
        self.active_user_shells.clone()
    }

    fn toggle_render_mode(&mut self) {
        self.render_mode = self.render_mode.toggled();
        self.notices.set(match self.render_mode {
            TranscriptRenderMode::Rich => "Rich transcript rendering enabled",
            TranscriptRenderMode::Raw => "Raw transcript source enabled",
        });
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        &self.frame_surfaces
    }

    /// Whether the last frame's surfaces stand alone, because a modal owned
    /// the frame.
    pub fn frame_surfaces_exclusive(&self) -> bool {
        self.frame_surfaces_exclusive
    }

    /// Scrolls the elicitation message pane, for a drag held at its edge.
    pub(super) fn scroll_elicitation_message(&self, rows: isize) {
        if let Some(dialog) = self.elicitation.as_ref() {
            dialog.scroll_message(rows);
        }
    }

    /// The message text a selection in the elicitation pane covers.
    pub fn elicitation_selection_text(&self, range: &SelectionRange) -> Option<String> {
        let dialog = self.elicitation.as_ref()?;
        let width = dialog.message_area()?.width;
        Some(dialog.selection_text(range, width))
    }

    /// Capture is on for every surface, so the app owns the wheel: terminal
    /// scrollback repaints whole TUI frames and is unusably slow on long
    /// sessions.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> ChatAction {
        // Hover decides which transcript scrolls while the split is up, so a
        // reviewer answer never moves the reader's place in the primary.
        if self.second_opinion_split() || self.turn_review_split() {
            let turn_review = self.turn_review_split();
            let over_reviewer = self
                .reviewer_area
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)));
            let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(1);
            match (mouse.kind, over_reviewer) {
                (MouseEventKind::ScrollUp, true) => {
                    if turn_review {
                        self.scroll_turn_review(-rows);
                    } else {
                        self.scroll_second_opinion(-rows);
                    }
                }
                (MouseEventKind::ScrollDown, true) => {
                    if turn_review {
                        self.scroll_turn_review(rows);
                    } else {
                        self.scroll_second_opinion(rows);
                    }
                }
                (MouseEventKind::ScrollUp, false) => self.scroll_history_up(MOUSE_SCROLL_ROWS),
                (MouseEventKind::ScrollDown, false) => self.scroll_history_down(MOUSE_SCROLL_ROWS),
                (MouseEventKind::Down(MouseButton::Left), _) => {
                    return if turn_review {
                        self.click_turn_review_action(mouse.column, mouse.row)
                    } else {
                        self.click_split_action(mouse.column, mouse.row)
                    };
                }
                _ => {}
            }
            return ChatAction::None;
        }
        if let Some(dialog) = self.elicitation.as_mut() {
            dialog.handle_mouse(mouse);
            return ChatAction::None;
        }
        // The host routes a wheel event here only when the pointer is over
        // the conversation, so it always drives the transcript.
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_history_up(MOUSE_SCROLL_ROWS),
            MouseEventKind::ScrollDown => self.scroll_history_down(MOUSE_SCROLL_ROWS),
            _ => {}
        }
        ChatAction::None
    }

    fn apply_event(&mut self, event: &SequencedEvent) {
        match &event.event {
            WorkerEvent::PromptAccepted { text, .. } => {
                self.mark_prompt_submitted(text);
                self.start_turn_clock(event.recorded_at_ms);
                self.entries.push(
                    ChatEntry::plain(event.seq, ChatRole::User, text)
                        .with_recorded_at(event.recorded_at_ms),
                );
            }
            WorkerEvent::TurnCompleted => {
                self.phase = WorkerPhase::Idle;
                self.prompt_in_flight = false;
                self.goal_prompt_active = false;
                self.turn_started_at_epoch_seconds = None;
            }
            // The durable worker records cancellation acceptance before the
            // ACP prompt future resolves. Keep the chat busy until the later
            // TurnCompleted event so a queued prompt cannot race the runtime.
            WorkerEvent::Cancelled => {
                self.phase = WorkerPhase::Running;
            }
            WorkerEvent::Closing => self.phase = WorkerPhase::Closing,
            WorkerEvent::Closed => {
                self.phase = WorkerPhase::Closed;
                self.prompt_in_flight = false;
            }
            WorkerEvent::Checkpointed { .. } => {}
            WorkerEvent::Adapter { payload, .. } => {
                if is_compaction_artifact(payload) {
                    self.last_compaction_seq = event.seq;
                }
                self.apply_adapter(event.seq, event.recorded_at_ms, payload)
            }
            WorkerEvent::QueuedPromptAdded { prompt } => {
                if !self.pending_queue_removals.contains(&prompt.id) {
                    self.queued_prompts.push_back(QueuedPrompt {
                        id: prompt.id.clone(),
                        text: prompt.text.clone(),
                        kind: QueuedCommandKind::Prompt,
                    });
                }
            }
            WorkerEvent::QueuedPromptRemoved { queue_id } => {
                self.queued_prompts.retain(|prompt| prompt.id != *queue_id);
                self.pending_queue_removals.remove(queue_id);
            }
            WorkerEvent::QueuedPromptPromoted { prompt, .. } => {
                self.queued_prompts.retain(|queued| queued.id != prompt.id);
                self.pending_queue_removals.remove(&prompt.id);
                self.phase = WorkerPhase::Running;
                self.prompt_in_flight = true;
                self.start_turn_clock(event.recorded_at_ms);
                self.entries.push(
                    ChatEntry::plain(event.seq, ChatRole::User, &prompt.text)
                        .with_recorded_at(event.recorded_at_ms),
                );
            }
            WorkerEvent::QueuedPromptsCleared => {
                self.queued_prompts.clear();
                self.pending_queue_removals.clear();
            }
            WorkerEvent::ConfigChanged { .. } => {}
        }
    }

    fn apply_adapter(
        &mut self,
        seq: u64,
        recorded_at_ms: Option<i64>,
        payload: &serde_json::Value,
    ) {
        let runtime = match serde_json::from_value::<RuntimeEvent>(payload.clone()) {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(
                    seq,
                    %error,
                    "ignoring malformed persisted runtime event"
                );
                return;
            }
        };
        let Some(runtime) =
            apply_runtime_event_to_entries(&mut self.entries, seq, recorded_at_ms, runtime)
        else {
            return;
        };
        match runtime {
            RuntimeEvent::SessionUpdate { update } => {
                self.apply_session_update_at(seq, recorded_at_ms, &update)
            }
            RuntimeEvent::SessionConfigured { config_options } => {
                self.set_config_options(&config_options)
            }
            RuntimeEvent::SessionModesConfigured { modes } => self.set_session_modes(modes),
            _ => {}
        }
    }

    /// Project one typed ACP update into stable transcript items. The runtime
    /// keeps JSON at the persistence boundary so old event logs remain wire
    /// compatible; rendering never guesses at arbitrary JSON shapes.
    #[cfg(test)]
    fn apply_session_update(&mut self, seq: u64, update: &serde_json::Value) {
        self.apply_session_update_at(seq, None, update);
    }

    fn apply_session_update_at(
        &mut self,
        seq: u64,
        recorded_at_ms: Option<i64>,
        update: &serde_json::Value,
    ) {
        let parsed = match serde_json::from_value::<SessionUpdate>(update.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::debug!(%error, "ignoring invalid ACP session update");
                return;
            }
        };
        let Some(parsed) =
            apply_session_update_to_entries(&mut self.entries, seq, recorded_at_ms, parsed)
        else {
            return;
        };
        match parsed {
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.acp_surface
                    .set_agent_commands(update.available_commands);
                self.rebuild_command_choices();
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.set_config_options(&update.config_options);
            }
            SessionUpdate::CurrentModeUpdate(update) => self
                .acp_surface
                .apply_current_mode_update(update.current_mode_id.to_string()),
            _ => {}
        }
    }
}

fn plan_control_error_message(error: PlanControlError) -> &'static str {
    match error {
        PlanControlError::DeepseekUnsupported => "Plan mode is unsupported in DSH.",
        PlanControlError::CodexIncompatible => {
            "This Codex ACP version does not expose collaboration_mode with plan/default values."
        }
        PlanControlError::GrokIncompatible => {
            "This Grok Build version does not expose compatible plan/default modes."
        }
        PlanControlError::Incompatible => {
            "This ACP harness does not expose compatible plan/default modes."
        }
    }
}

fn is_compaction_artifact(payload: &serde_json::Value) -> bool {
    let update = payload.get("update").unwrap_or(payload);
    matches!(
        update
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str),
        Some("compaction" | "context_compaction" | "compaction_summary")
    ) || update.get("encrypted_content").is_some()
        || update.get("encryptedContent").is_some()
}

fn normalize_key(code: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(character) = code else {
        return (code, modifiers);
    };
    if modifiers.is_empty() {
        let value = u32::from(character);
        if (1..=26).contains(&value)
            && let Some(control) = char::from_u32(value - 1 + u32::from('a'))
        {
            modifiers.insert(KeyModifiers::CONTROL);
            return (KeyCode::Char(control), modifiers);
        }
    }
    if character.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
            return (KeyCode::Char(character.to_ascii_lowercase()), modifiers);
        }
    }
    (code, modifiers)
}

fn queued_prompt_preview(prompt: &str) -> String {
    const WIDTH: usize = 72;
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= WIDTH {
        return collapsed;
    }
    let mut preview = collapsed.chars().take(WIDTH - 1).collect::<String>();
    preview.push('…');
    preview
}

/// How long a notice is guaranteed on screen before an unrelated key press
/// may dismiss it. Background failures report through this bar and nowhere
/// else, so a keystroke that races one must not wipe it unread.
pub const NOTICE_MINIMUM_DISPLAY: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Debug)]
struct Notice {
    text: String,
    set_at: std::time::Instant,
    protected: bool,
}

#[derive(Debug, Default)]
struct NoticeSlot {
    notice: Option<Notice>,
    /// Bumped on every write, so a dirty-gated renderer can tell that the bar
    /// moved without keeping a copy of its text.
    generation: u64,
}

impl NoticeSlot {
    fn write(&mut self, notice: Option<Notice>) {
        self.notice = notice;
        self.generation = self.generation.wrapping_add(1);
    }
}

/// The one-line notifications bar shared by every view. Cloning shares the
/// same underlying slot; the latest notice wins and a clear in one view
/// clears it for all.
///
/// Each notice carries the time it was set. That is what lets an incidental
/// key press dismiss a notice the user has had a chance to read while leaving
/// a fresh one standing.
#[derive(Debug, Clone, Default)]
pub struct Notices(std::sync::Arc<std::sync::Mutex<NoticeSlot>>);

impl Notices {
    /// Sets the notice, replacing whatever is showing. Sanitizes the text so
    /// escape sequences or stray carriage returns from background work
    /// cannot corrupt the footer row.
    pub fn set(&self, notice: impl Into<String>) {
        let text = sanitize_terminal_text(&notice.into());
        let mut slot = self.lock();
        if slot.notice.as_ref().is_some_and(|current| {
            current.protected && current.set_at.elapsed() < NOTICE_MINIMUM_DISPLAY
        }) {
            return;
        }
        slot.write(Some(Notice {
            text,
            set_at: std::time::Instant::now(),
            protected: false,
        }));
    }

    /// Sets a failure notice that routine background updates cannot replace
    /// before it has been readable for [`NOTICE_MINIMUM_DISPLAY`]. A newer
    /// failure still replaces it immediately.
    pub fn set_failure(&self, notice: impl Into<String>) {
        let text = sanitize_terminal_text(&notice.into());
        self.lock().write(Some(Notice {
            text,
            set_at: std::time::Instant::now(),
            protected: true,
        }));
    }

    /// Replaces the notice only if it still reads `expected`, so a
    /// background task can upgrade its own "in progress" notice to a result
    /// without clobbering whatever replaced it in the meantime. Returns
    /// whether the replacement happened. The replacement is a new report, so
    /// it starts its own display period.
    pub fn replace_if(&self, expected: &str, replacement: impl Into<String>) -> bool {
        let mut slot = self.lock();
        if slot.notice.as_ref().map(|notice| notice.text.as_str()) != Some(expected) {
            return false;
        }
        let text = sanitize_terminal_text(&replacement.into());
        slot.write(Some(Notice {
            text,
            set_at: std::time::Instant::now(),
            protected: false,
        }));
        true
    }

    /// Clears the notice everywhere it is shown, however recent it is. This
    /// is for callers that know the notice no longer applies; a key press
    /// that merely happened to arrive uses [`Notices::dismiss`].
    pub fn clear(&self) {
        let mut slot = self.lock();
        if slot.notice.is_some() {
            slot.write(None);
        }
    }

    /// Clears the notice if it has been showing for at least
    /// [`NOTICE_MINIMUM_DISPLAY`] at `now`. Returns whether the bar is clear
    /// afterwards, so a caller can tell a survivor from a dismissal.
    pub fn dismiss(&self, now: std::time::Instant) -> bool {
        let mut slot = self.lock();
        match slot.notice.as_ref() {
            None => true,
            Some(notice) => {
                if now.saturating_duration_since(notice.set_at) < NOTICE_MINIMUM_DISPLAY {
                    return false;
                }
                slot.write(None);
                true
            }
        }
    }

    /// The current notice, if any.
    pub fn current(&self) -> Option<String> {
        self.lock()
            .notice
            .as_ref()
            .map(|notice| notice.text.clone())
    }

    /// Counts writes to the shared slot. A renderer that records this with
    /// each frame can tell that the bar changed since it last drew, which is
    /// what keeps a notice set by background work from being missed by a
    /// dirty-gated draw.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NoticeSlot> {
        self.0.lock().expect("notices lock poisoned")
    }
}

/// Color of an active session's line. The terminal palette's plain yellow
/// (amber or orange in common palettes) marks a session whose turn is still
/// running; a session with no turn in flight is waiting on the user and
/// switches to the brighter light yellow.
pub fn turn_band_color(turn_in_flight: bool) -> Color {
    if turn_in_flight {
        Color::Yellow
    } else {
        Color::LightYellow
    }
}

/// When the session's current turn started, in epoch seconds. `None` means no
/// turn is in flight.
fn turn_started_at_epoch_seconds(execution: MaterializedExecutionState) -> Option<u64> {
    match execution {
        MaterializedExecutionState::Running { started_at_ms } => {
            u64::try_from(started_at_ms).ok().map(|value| value / 1_000)
        }
        MaterializedExecutionState::Idle
        | MaterializedExecutionState::Closing
        | MaterializedExecutionState::Closed => None,
    }
}

/// Last line of a message that has any text on it, trimmed. `None` means the
/// message is blank.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_chat::test_support::{
        advertise, alt, ctrl, fast_mode_option, grok_chat, key, mode_config_option, queued,
        select_config_option, snapshot,
    };
    use crate::hel_worker::ActivePrompt;

    /// Mirrors what `ActiveChat::open` does for a session with no warm view:
    /// build the state from the snapshot, then seed the saved draft.
    fn freshly_opened_chat(saved_draft: &str) -> ChatState {
        let mut chat =
            ChatState::from_materialized(&MaterializedSession::empty("session-fresh"), &[], &[]);
        chat.set_history_context("bundle-1");
        chat.restore_draft(saved_draft.to_owned());
        chat
    }

    #[test]
    fn alt_v_toggles_voice_without_editing_prompt() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let action = chat.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        assert_eq!(action, ChatAction::ToggleVoice);
        assert!(chat.input.is_empty());
    }

    #[test]
    fn a_saved_draft_reopens_in_the_composer_with_the_cursor_at_its_end() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        chat.restore_draft("half typed thought".into());

        assert_eq!(chat.input, "half typed thought");
        assert_eq!(chat.input_cursor, "half typed thought".len());
    }

    #[test]
    fn an_empty_saved_draft_leaves_the_composer_untouched() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("typed since opening".into());

        chat.restore_draft(String::new());

        assert_eq!(chat.input, "typed since opening");
    }

    #[test]
    fn a_fresh_chat_opens_with_the_session_s_saved_draft_in_the_composer() {
        let chat = freshly_opened_chat("half typed thought");

        assert_eq!(chat.input, "half typed thought");
        assert_eq!(chat.input_cursor, "half typed thought".len());
    }

    #[test]
    fn a_fresh_chat_for_a_session_with_no_saved_draft_opens_empty() {
        assert_eq!(freshly_opened_chat("").input, "");
    }

    /// The pane dial and detach are global chords now, caught by the host
    /// before the composer sees the key. For one release the old Control
    /// chords say where they went rather than doing nothing.
    #[test]
    fn control_g_and_control_q_say_where_they_moved() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let control_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(chat.handle_key(control_g), ChatAction::None);
        assert_eq!(
            chat.notices.current().as_deref(),
            Some("Ctrl-G moved to Alt-G")
        );

        assert_eq!(chat.handle_key(ctrl('q')), ChatAction::None);
        assert_eq!(
            chat.notices.current().as_deref(),
            Some("Ctrl-Q moved to Alt-Q")
        );
    }

    #[test]
    fn enter_submits_to_the_worker_while_idle_or_running() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.handle_key(key(KeyCode::Char('h')));
        chat.handle_key(key(KeyCode::Char('i')));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Prompt("hi".into())
        );

        let mut running = snapshot();
        running.phase = WorkerPhase::Running;
        running.active_prompt = Some(ActivePrompt {
            request_id: "p".into(),
            text: "busy".into(),
            attachments: vec![],
        });
        let mut chat = ChatState::new(&running, &[]);
        chat.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::Prompt("x".into())
        );
        assert!(chat.queued_prompts.is_empty());
        assert!(chat.entries.is_empty());
    }

    #[test]
    fn bang_prefix_submits_a_bash_command_without_starting_a_prompt() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("!printf '%s' hello | tr a-z A-Z".into());

        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::RunShell("printf '%s' hello | tr a-z A-Z".into())
        );
        assert!(chat.input.is_empty());
        assert_eq!(
            chat.prompt_history.last().map(String::as_str),
            Some("!printf '%s' hello | tr a-z A-Z")
        );
    }

    #[test]
    fn empty_bang_command_stays_in_the_composer_and_shows_usage() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("!   ".into());

        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.input, "!   ");
        assert_eq!(chat.notice().as_deref(), Some("usage: !<bash command>"));
    }

    #[test]
    fn enter_does_not_send_a_prompt_while_the_worker_is_closing_or_closed() {
        for phase in [WorkerPhase::Closing, WorkerPhase::Closed] {
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.phase = phase;
            chat.input = "hello".into();
            assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
            assert_eq!(
                chat.notices.current().as_deref(),
                Some("The worker is closing; this prompt was not sent")
            );
            assert_eq!(chat.input, "hello");
        }
    }

    #[test]
    fn bootstrap_uses_snapshot_queue_without_duplicating_replayed_additions() {
        let worker: WorkerSnapshot = serde_json::from_value(serde_json::json!({
            "session_id": "1234567890",
            "phase": "running",
            "latest_seq": 1,
            "last_checkpoint_seq": null,
            "active_prompt": null,
            "config": {},
            "queued_prompts": [{
                "id": "queued-0001",
                "text": "next",
                "attachments": [],
                "created_at_ms": 1
            }],
            "handled_requests": {}
        }))
        .unwrap();
        let events = [SequencedEvent {
            seq: 1,
            recorded_at_ms: Some(1),
            request_id: Some("enqueue-1".into()),
            event: WorkerEvent::QueuedPromptAdded {
                prompt: crate::hel_worker::QueuedPrompt {
                    id: "queued-0001".into(),
                    text: "next".into(),
                    attachments: vec![],
                    created_at_ms: 1,
                },
            },
        }];

        let chat = ChatState::new(&worker, &events);

        assert_eq!(chat.queued_prompts.len(), 1);
        assert_eq!(chat.queued_prompts[0].id, "queued-0001");
    }

    #[test]
    fn submitting_a_prompt_clears_a_stale_queue_notice() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_notice("Queued 1: next");

        chat.mark_prompt_submitted("hello");

        assert_eq!(chat.phase, WorkerPhase::Running);
        assert!(chat.notice().is_none());
    }

    #[test]
    fn notices_set_replace_if_and_clear() {
        let notices = Notices::default();
        assert_eq!(notices.current(), None);

        notices.set("first notice");
        assert_eq!(notices.current().as_deref(), Some("first notice"));

        assert!(!notices.replace_if("wrong expectation", "replaced"));
        assert_eq!(notices.current().as_deref(), Some("first notice"));

        assert!(notices.replace_if("first notice", "second notice"));
        assert_eq!(notices.current().as_deref(), Some("second notice"));

        notices.clear();
        assert_eq!(notices.current(), None);
    }

    #[test]
    fn a_fresh_failure_notice_survives_routine_background_notices() {
        let notices = Notices::default();
        notices.set_failure("Resume failed: archived transcript is invalid");

        notices.set("Profile quotas refreshed");
        assert_eq!(
            notices.current().as_deref(),
            Some("Resume failed: archived transcript is invalid")
        );

        notices.set_failure("Resume failed: target disconnected");
        assert_eq!(
            notices.current().as_deref(),
            Some("Resume failed: target disconnected")
        );

        let after_set = std::time::Instant::now();
        assert!(notices.dismiss(after_set + NOTICE_MINIMUM_DISPLAY));
        notices.set("Profile quotas refreshed");
        assert_eq!(
            notices.current().as_deref(),
            Some("Profile quotas refreshed")
        );
    }

    #[test]
    fn cloned_notices_share_one_slot() {
        let notices = Notices::default();
        let clone = notices.clone();

        notices.set("set through the original");
        assert_eq!(clone.current().as_deref(), Some("set through the original"));

        clone.clear();
        assert_eq!(notices.current(), None);
    }

    /// Dismissal is what an incidental key press asks for, and a notice that
    /// nobody has had time to read must survive it.
    #[test]
    fn a_notice_is_dismissed_only_once_it_has_been_showing_long_enough() {
        let notices = Notices::default();
        assert!(notices.dismiss(std::time::Instant::now()));

        notices.set("Credential sync failed");
        let after_set = std::time::Instant::now();
        assert!(!notices.dismiss(after_set));
        assert_eq!(notices.current().as_deref(), Some("Credential sync failed"));

        assert!(notices.dismiss(after_set + NOTICE_MINIMUM_DISPLAY));
        assert_eq!(notices.current(), None);
    }

    /// Draws are gated on a dirty flag that background work never sets, so a
    /// renderer tells the bar moved by recording this counter with each frame.
    #[test]
    fn every_write_to_the_notice_slot_bumps_its_generation() {
        let notices = Notices::default();
        let drawn = notices.generation();

        notices.set("Import failed");
        assert_ne!(notices.generation(), drawn);
        let drawn = notices.generation();

        assert!(notices.replace_if("Import failed", "Import failed: no space left"));
        assert_ne!(notices.generation(), drawn);
        let drawn = notices.generation();

        notices.clear();
        assert_ne!(notices.generation(), drawn);

        // Clearing an empty bar changes nothing on screen.
        let drawn = notices.generation();
        notices.clear();
        assert_eq!(notices.generation(), drawn);
    }

    fn text_elicitation() -> ElicitationRequest {
        ElicitationRequest {
            id: "ask-1".into(),
            message: "Which branch should I use?".into(),
            title: None,
            description: None,
            fields: vec![crate::hel_elicitation::ElicitationField {
                id: "branch".into(),
                title: "Branch".into(),
                description: None,
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: crate::hel_elicitation::ElicitationFieldKind::Text {
                    default: None,
                    min_length: None,
                    max_length: None,
                    pattern: None,
                    format: None,
                },
            }],
        }
    }

    /// A pending elicitation is durable projection state, rebuilt from the
    /// session the next time it is opened, so leaving the view is a different
    /// act from answering the agent. The moved-key notices are handled on the
    /// same terms: they pass the open form without consuming it.
    #[test]
    fn control_g_and_control_q_pass_a_chat_whose_elicitation_is_still_open() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let request = text_elicitation();
        chat.restore_elicitation(request.clone());

        assert_eq!(chat.handle_key(ctrl('g')), ChatAction::None);
        assert_eq!(chat.handle_key(ctrl('q')), ChatAction::None);
        assert_eq!(
            chat.materialized_session().pending_elicitations,
            vec![request.clone()]
        );

        // Every other key still belongs to the form, and Escape still answers
        // the agent rather than leaving.
        assert_eq!(chat.handle_key(key(KeyCode::Char('q'))), ChatAction::None);
        assert_eq!(
            chat.handle_key(key(KeyCode::Esc)),
            ChatAction::RespondElicitation {
                request,
                response: ElicitationResponse::Cancel,
            }
        );
        assert!(chat.materialized_session().pending_elicitations.is_empty());
    }

    #[test]
    fn escape_only_cancels_an_active_turn() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(chat.handle_key(control_c), ChatAction::None);
        assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);

        // A turn the harness started on its own runs with no prompt of ours in
        // flight. The relay refuses to cancel that, so Esc must not offer to.
        chat.phase = WorkerPhase::Running;
        assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::None);

        chat.set_prompt_in_flight(true);
        assert_eq!(chat.handle_key(key(KeyCode::Esc)), ChatAction::Cancel);
        assert_eq!(chat.handle_key(control_c), ChatAction::None);
    }

    #[test]
    fn cancellation_waits_for_turn_completion_before_queue_can_drain() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Running;
        chat.queued_prompts.push_back(queued("queued-1", "next"));
        chat.apply_event(&SequencedEvent {
            seq: 1,
            recorded_at_ms: None,
            request_id: Some("cancel".into()),
            event: WorkerEvent::Cancelled,
        });
        assert_eq!(chat.phase, WorkerPhase::Running);

        chat.apply_event(&SequencedEvent {
            seq: 2,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::TurnCompleted,
        });
        assert_eq!(chat.phase, WorkerPhase::Idle);
        assert_eq!(chat.queued_prompts.front().unwrap().text, "next");
    }

    #[test]
    fn alt_up_recovers_the_latest_queued_prompt_for_editing() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.queued_prompts.push_back(queued("queued-1", "first"));
        chat.queued_prompts.push_back(queued("queued-2", "second"));

        assert_eq!(
            chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            ChatAction::RemoveQueuedPrompt {
                id: "queued-2".into(),
                text: "second".into(),
                kind: QueuedCommandKind::Prompt,
            }
        );

        assert_eq!(chat.input, "second");
        assert_eq!(chat.queued_prompts.len(), 1);
        assert_eq!(chat.queued_prompts[0].text, "first");
    }

    #[test]
    fn up_and_control_p_peel_queued_prompts_back_into_the_editor() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        for (id, text) in [
            ("queued-1", "first"),
            ("queued-2", "second"),
            ("queued-3", "third"),
        ] {
            chat.queued_prompts.push_back(queued(id, text));
        }

        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "third");
        assert_eq!(chat.queued_prompts.len(), 2);

        chat.clear_input();
        chat.handle_key(ctrl('p'));
        assert_eq!(chat.input, "second");
        assert_eq!(chat.queued_prompts.len(), 1);

        chat.clear_input();
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "first");
        assert!(chat.queued_prompts.is_empty());

        chat.clear_input();
        chat.handle_key(key(KeyCode::Up));
        assert!(chat.input.is_empty());
    }

    #[test]
    fn model_and_effort_slash_commands_change_live_session_config() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.input = "/model gpt-5.6-luna".into();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "gpt-5.6-luna".into(),
            }
        );

        chat.input = "/effort xhigh".into();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "effort".into(),
                value: "xhigh".into(),
            }
        );
    }

    #[test]
    fn fast_toggles_the_advertised_codex_configuration_without_arguments() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[fast_mode_option("off")]);
        chat.input = "/fast".into();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "fast-mode".into(),
                value: "on".into(),
            }
        );

        chat.set_config_options(&[fast_mode_option("on")]);
        chat.input = "/fast".into();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "fast-mode".into(),
                value: "off".into(),
            }
        );

        chat.input = "/fast on".into();
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.notice().as_deref(), Some("usage: /fast"));
    }

    #[test]
    fn fast_stays_local_when_the_active_model_does_not_support_it() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.input = "/fast".into();

        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(chat.input, "/fast");
        assert_eq!(
            chat.notice().as_deref(),
            Some("Fast mode is unavailable for the active Codex model")
        );
    }

    #[test]
    fn config_commands_are_queued_while_the_agent_is_busy() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Running;

        chat.input = "/model".into();
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(
            chat.notices.current().as_deref(),
            Some("The agent does not advertise model values; usage: /model <value>")
        );

        chat.input = "/model sonnet".into();
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            }
        );
        assert!(chat.input.is_empty());

        chat.phase = WorkerPhase::Closing;
        chat.input = "/model sonnet".into();
        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(
            chat.notices.current().as_deref(),
            Some("The worker is closing; this configuration change was not sent")
        );
    }

    #[test]
    fn a_queued_config_change_peels_back_into_the_composer() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut session = MaterializedSession::empty("1234567890");
        // The projection only rebuilds when its frontier moved.
        session.applied_event_ordinal = 5;
        session.queued_prompts.push(MaterializedQueuedPrompt {
            command_id: "queued-config".into(),
            kind: QueuedCommandKind::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            },
            content: vec![serde_json::json!({"type": "text", "text": "/model sonnet"})],
            queued_at_ms: 10,
        });
        chat.apply_materialized(&session, &[], &[]);
        assert_eq!(chat.queued_prompts.len(), 1);
        assert_eq!(chat.queued_prompts[0].queue_label(), "queued config");

        assert_eq!(
            chat.handle_key(ctrl('p')),
            ChatAction::RemoveQueuedPrompt {
                id: "queued-config".into(),
                text: "/model sonnet".into(),
                kind: QueuedCommandKind::SetConfig {
                    key: "model".into(),
                    value: "sonnet".into(),
                },
            }
        );
        assert_eq!(chat.input, "/model sonnet");
        assert!(chat.queued_prompts.is_empty());

        // Resubmitting the peeled-back text parses as the same change.
        chat.phase = WorkerPhase::Running;
        assert_eq!(
            chat.handle_key(key(KeyCode::Enter)),
            ChatAction::SetConfig {
                key: "model".into(),
                value: "sonnet".into(),
            }
        );
    }

    #[test]
    fn stale_projection_does_not_restore_a_queue_entry_being_edited() {
        let mut session = MaterializedSession::empty("session-queue-edit");
        session.applied_event_ordinal = 5;
        session.queued_prompts.push(MaterializedQueuedPrompt {
            command_id: "queued-prompt".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type": "text", "text": "revise me"})],
            queued_at_ms: 10,
        });
        let mut chat = ChatState::from_materialized(&session, &[], &[]);

        assert_eq!(
            chat.handle_key(key(KeyCode::Up)),
            ChatAction::RemoveQueuedPrompt {
                id: "queued-prompt".into(),
                text: "revise me".into(),
                kind: QueuedCommandKind::Prompt,
            }
        );
        remote::apply_chat_remote_result(
            &mut chat,
            remote::ChatRemoteResult::RemoveQueuedPrompt {
                id: "queued-prompt".into(),
                text: "revise me".into(),
                kind: QueuedCommandKind::Prompt,
                result: Ok(()),
            },
        );

        // The relay accepted the removal, but its previously published view
        // can still arrive before the projection containing that command.
        chat.apply_materialized(&session, &[], &[]);
        assert_eq!(chat.input, "revise me");
        assert!(chat.queued_prompts.is_empty());
        assert!(chat.pending_queue_removals.contains("queued-prompt"));

        session.applied_event_ordinal = 6;
        session.queued_prompts.clear();
        chat.apply_materialized(&session, &[], &[]);
        assert!(chat.pending_queue_removals.is_empty());
    }

    #[test]
    fn failed_queue_removal_restores_the_peeled_entry() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.queued_prompts
            .push_back(queued("queued-prompt", "revise me"));
        chat.handle_key(ctrl('p'));

        remote::apply_chat_remote_result(
            &mut chat,
            remote::ChatRemoteResult::RemoveQueuedPrompt {
                id: "queued-prompt".into(),
                text: "revise me".into(),
                kind: QueuedCommandKind::Prompt,
                result: Err("relay rejected removal".into()),
            },
        );

        assert!(chat.pending_queue_removals.is_empty());
        assert_eq!(chat.queued_prompts.len(), 1);
        assert_eq!(chat.queued_prompts[0].id, "queued-prompt");
    }

    #[test]
    fn plan_toggles_the_session_mode_for_a_harness_without_a_plan_command() {
        let mut chat = grok_chat();
        chat.set_input("/plan".into());

        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/plan".into(),
                control: PlanControl::SetSessionMode {
                    mode_id: "plan".into()
                },
                requested_active: true,
                prompt: None,
            }
        );
        assert!(chat.input.is_empty());
        assert!(chat.notices.current().unwrap().contains("Plan mode on"));

        chat.plan_command_pending = false;
        chat.set_input("/plan".into());
        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/plan".into(),
                control: PlanControl::SetSessionMode {
                    mode_id: "default".into()
                },
                requested_active: false,
                prompt: None,
            }
        );
        assert!(chat.notices.current().unwrap().contains("Plan mode off"));
    }

    #[test]
    fn plan_accepts_explicit_on_and_off_arguments() {
        let mut chat = grok_chat();
        chat.set_input("/plan off".into());
        assert_eq!(chat.submit_input(), ChatAction::None);

        chat.set_input("/plan ON".into());
        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/plan ON".into(),
                control: PlanControl::SetSessionMode {
                    mode_id: "plan".into()
                },
                requested_active: true,
                prompt: None,
            }
        );

        chat.plan_command_pending = false;
        chat.set_input("/plan sideways".into());
        assert_eq!(chat.submit_input(), ChatAction::Prompt("sideways".into()));
    }

    #[test]
    fn plan_uses_an_advertised_mode_config_option() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[mode_config_option("default", &["default", "plan"])]);
        chat.set_input("/plan".into());

        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/plan".into(),
                control: PlanControl::SetConfig {
                    key: "mode".into(),
                    value: "plan".into()
                },
                requested_active: true,
                prompt: None,
            }
        );
    }

    #[test]
    fn grok_uses_its_trusted_set_mode_fallback_even_with_an_unrelated_mode_config() {
        let mut chat = grok_chat();
        chat.set_config_options(&[mode_config_option("default", &["default", "act"])]);
        chat.set_input("/plan".into());

        assert!(matches!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                control: PlanControl::SetSessionMode { .. },
                ..
            }
        ));
    }

    #[test]
    fn an_unchanged_mode_catalogue_does_not_undo_an_optimistic_toggle() {
        let options = [mode_config_option("default", &["default", "plan"])];
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&options);
        chat.set_input("/plan".into());
        assert!(matches!(
            chat.submit_input(),
            ChatAction::PlanCommand { .. }
        ));

        chat.set_config_options(&options);

        assert!(chat.plan_mode_active());
    }

    #[test]
    fn an_agent_plan_command_does_not_override_hels_unified_command() {
        let mut chat = grok_chat();
        advertise(&mut chat, 1, &["plan"]);
        chat.set_input("/plan the migration".into());

        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/plan the migration".into(),
                control: PlanControl::SetSessionMode {
                    mode_id: "plan".into()
                },
                requested_active: true,
                prompt: Some("the migration".into()),
            }
        );
    }

    #[test]
    fn plan_is_kept_local_without_a_compatible_mode_surface() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("/plan".into());

        assert_eq!(chat.submit_input(), ChatAction::None);
        assert_eq!(chat.input, "/plan");
        assert!(chat.notices.current().unwrap().contains("does not expose"));
    }

    #[test]
    fn codex_plan_uses_collaboration_mode_not_the_permission_mode() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_harness_kind(HarnessKind::Codex);
        chat.set_config_options(&[
            select_config_option("mode", "read-only", &["read-only", "full-access"]),
            select_config_option("collaboration_mode", "default", &["default", "plan"]),
        ]);
        chat.set_input("/plan inspect the migration".into());

        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/plan inspect the migration".into(),
                control: PlanControl::SetConfig {
                    key: "collaboration_mode".into(),
                    value: "plan".into(),
                },
                requested_active: true,
                prompt: Some("inspect the migration".into()),
            }
        );
        assert_eq!(
            chat.prompt_history.last().map(String::as_str),
            Some("/plan inspect the migration")
        );
    }

    #[test]
    fn claude_and_kimi_prefer_the_exact_mode_config() {
        for harness in [HarnessKind::Claude, HarnessKind::Kimi] {
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.set_harness_kind(harness);
            chat.set_config_options(&[select_config_option(
                "mode",
                "default",
                &["default", "plan"],
            )]);
            chat.set_input("/plan".into());
            assert!(matches!(
                chat.submit_input(),
                ChatAction::PlanCommand {
                    control: PlanControl::SetConfig { ref key, .. },
                    ..
                } if key == "mode"
            ));
        }
    }

    #[test]
    fn grok_uses_set_mode_without_advertising_modes() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_harness_kind(HarnessKind::Grok);
        chat.set_input("/plan".into());
        assert!(matches!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                control: PlanControl::SetSessionMode { ref mode_id },
                ..
            } if mode_id == "plan"
        ));
    }

    #[test]
    fn deepseek_rejects_plan_and_implement_locally() {
        let mut chat = grok_chat();
        chat.set_harness_kind(HarnessKind::Deepseek);
        for command in ["/plan design it", "/implement"] {
            chat.set_input(command.into());
            assert_eq!(chat.submit_input(), ChatAction::None);
            assert_eq!(chat.input, command);
            assert!(
                chat.notices
                    .current()
                    .unwrap()
                    .contains("unsupported in DSH")
            );
        }
    }

    #[test]
    fn implement_exits_plan_mode_before_submitting_the_instruction() {
        let mut chat = grok_chat();
        chat.finish_plan_mode_change(true);
        chat.set_input("/implement start with the parser".into());
        assert_eq!(
            chat.submit_input(),
            ChatAction::PlanCommand {
                original: "/implement start with the parser".into(),
                control: PlanControl::SetSessionMode {
                    mode_id: "default".into()
                },
                requested_active: false,
                prompt: Some("start with the parser".into()),
            }
        );
    }

    #[test]
    fn plan_review_choices_have_distinct_followup_directions() {
        let mut chat = grok_chat();
        chat.finish_plan_mode_change(true);
        let standard = ElicitationRequest {
            id: "plan-review-1".into(),
            message: "review".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        let response = |action: &str, feedback: Option<&str>| {
            let mut content = BTreeMap::new();
            content.insert("action".into(), ElicitationValue::String(action.into()));
            if let Some(feedback) = feedback {
                content.insert("feedback".into(), ElicitationValue::String(feedback.into()));
            }
            ElicitationResponse::Accept { content }
        };

        assert_eq!(
            chat.plan_review_followup(&standard, &response("implement", None)),
            Some(PlanReviewFollowup {
                desired_active: false,
                control: None,
                prompt: None,
            })
        );
        assert_eq!(
            chat.plan_review_followup(&standard, &response("revise", Some("add tests"))),
            Some(PlanReviewFollowup {
                desired_active: true,
                control: None,
                prompt: Some("add tests".into()),
            })
        );
        assert!(matches!(
            chat.plan_review_followup(&standard, &response("exit", None)),
            Some(PlanReviewFollowup {
                desired_active: false,
                control: Some(PlanControl::SetSessionMode { .. }),
                prompt: None,
            })
        ));
    }

    #[test]
    fn plan_waits_for_an_idle_agent() {
        let mut chat = grok_chat();
        chat.phase = WorkerPhase::Running;
        chat.set_input("/plan".into());

        assert_eq!(chat.submit_input(), ChatAction::None);
        assert!(chat.notices.current().unwrap().contains("only available"));
    }

    #[test]
    fn a_current_mode_update_corrects_the_locally_tracked_plan_mode() {
        let mut chat = grok_chat();
        chat.set_input("/plan".into());
        chat.submit_input();
        assert!(chat.plan_mode_active());

        let mut session = MaterializedSession::empty("1234567890");
        session
            .configuration
            .insert("mode".into(), serde_json::Value::String("default".into()));
        chat.apply_materialized(&session, &[], &[]);

        assert!(!chat.plan_mode_active());
    }

    #[test]
    fn config_slash_command_without_value_shows_usage() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.input = "/model".into();

        assert_eq!(chat.handle_key(key(KeyCode::Enter)), ChatAction::None);
        assert_eq!(
            chat.notice().as_deref(),
            Some("The agent does not advertise model values; usage: /model <value>")
        );
    }

    #[test]
    fn editor_preserves_uppercase_text_while_shortcuts_remain_case_insensitive() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        chat.handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT));
        // Some terminals report the uppercase character without a Shift modifier.
        chat.handle_key(key(KeyCode::Char('I')));
        assert_eq!(chat.input, "HI");

        chat.handle_key(ctrl('r'));
        chat.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(chat.history_search.as_ref().unwrap().query, "N");
        chat.handle_key(key(KeyCode::Esc));

        chat.handle_key(KeyEvent::new(
            KeyCode::Char('T'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        assert_eq!(chat.render_mode, TranscriptRenderMode::Raw);
    }

    #[test]
    fn ctrl_v_returns_paste_request_action() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        assert_eq!(chat.handle_key(ctrl('v')), ChatAction::PasteFromClipboard);
        assert!(chat.input.is_empty());
    }

    #[test]
    fn alt_t_toggles_rendering() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        assert_eq!(chat.render_mode, TranscriptRenderMode::Rich);
        chat.handle_key(alt('t'));
        assert_eq!(chat.render_mode, TranscriptRenderMode::Raw);
        chat.handle_key(alt('t'));
        assert_eq!(chat.render_mode, TranscriptRenderMode::Rich);
        // Ctrl-T stayed free for readline when the toggle moved to Alt-T.
        chat.handle_key(ctrl('t'));
        assert_eq!(chat.render_mode, TranscriptRenderMode::Rich);
    }

    #[test]
    fn replay_projects_user_and_agent_text() {
        let runtime = RuntimeEvent::SessionUpdate {
            update: serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "done"}
            }),
        };
        let events = vec![
            SequencedEvent {
                seq: 1,
                recorded_at_ms: None,
                request_id: Some("p".into()),
                event: WorkerEvent::PromptAccepted {
                    request_id: "p".into(),
                    text: "work".into(),
                    attachments: vec![],
                },
            },
            SequencedEvent {
                seq: 2,
                recorded_at_ms: None,
                request_id: None,
                event: WorkerEvent::Adapter {
                    kind: "session_update".into(),
                    payload: serde_json::to_value(runtime).unwrap(),
                },
            },
        ];
        let mut initial = snapshot();
        initial.latest_seq = 2;
        let chat = ChatState::new(&initial, &events);
        assert_eq!(chat.entries.len(), 2);
        assert_eq!(chat.entries[0].role, ChatRole::User);
        assert_eq!(chat.entries[1].text, "done");
    }

    #[test]
    fn hydrated_tail_continues_the_last_streamed_message() {
        let first = RuntimeEvent::SessionUpdate {
            update: serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "answer",
                "content": {"type": "text", "text": "hello"}
            }),
        };
        let event = SequencedEvent {
            seq: 1,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::to_value(first).unwrap(),
            },
        };
        let mut initial = snapshot();
        initial.latest_seq = 1;
        let full = ChatState::new(&initial, &[event]);
        let entries = full.bounded_entries(10, 512 * 1024);
        let mut tail =
            ChatState::from_tail(initial.session_id.clone(), WorkerPhase::Running, 1, entries);
        let second = RuntimeEvent::SessionUpdate {
            update: serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "messageId": "answer",
                "content": {"type": "text", "text": " world"}
            }),
        };
        tail.apply_events(&[SequencedEvent {
            seq: 2,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::to_value(second).unwrap(),
            },
        }]);

        assert_eq!(tail.entries.len(), 1);
        assert_eq!(tail.entries[0].text, "hello world");
        let materialized = tail.materialized_session();
        assert_eq!(materialized.transcript[0].position, 1);
        assert_eq!(
            materialized.transcript[0].latest_content_event_ordinal,
            Some(2)
        );
        assert_eq!(materialized.unread_agent_messages_after(1), 1);
    }

    #[test]
    fn streamed_message_chunks_coalesce_into_one_entry() {
        let mut initial = snapshot();
        initial.latest_seq = 0;
        let mut chat = ChatState::new(&initial, &[]);
        for (seq, text) in [(1, "gpt"), (2, "-5.6"), (3, "-terra")] {
            chat.apply_session_update(
                seq,
                &serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }),
            );
        }
        chat.apply_session_update(
            4,
            &serde_json::json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "hmm"}
            }),
        );
        assert_eq!(chat.entries.len(), 2);
        assert_eq!(chat.entries[0].role, ChatRole::Agent);
        assert_eq!(chat.entries[0].text, "gpt-5.6-terra");
        assert_eq!(chat.entries[1].role, ChatRole::Thought);
    }

    #[test]
    fn tool_calls_render_title_and_updates_stay_quiet() {
        let mut initial = snapshot();
        initial.latest_seq = 0;
        let mut chat = ChatState::new(&initial, &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({"sessionUpdate": "tool_call",
                "toolCallId": "grep-config",
                "title": "grep config", "status": "pending"}),
        );
        chat.apply_session_update(
            2,
            &serde_json::json!({"sessionUpdate": "tool_call_update",
                "toolCallId": "grep-config", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "noise"}}]}),
        );
        assert_eq!(chat.entries.len(), 1);
        assert_eq!(chat.entries[0].role, ChatRole::Tool);
        assert_eq!(chat.entries[0].text, "grep config");
    }

    #[test]
    fn partial_tool_updates_preserve_unchanged_structured_fields() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "inspect",
                "title": "inspect",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "first result"}
                }],
                "locations": [{"path": "src/lib.rs", "line": 7}]
            }),
        );
        chat.apply_session_update(
            2,
            &serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "inspect",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "replacement result"}
                }]
            }),
        );

        assert_eq!(chat.entries[0].tool_content, ["replacement result"]);
        assert_eq!(chat.entries[0].tool_locations, ["src/lib.rs:7"]);
    }

    #[test]
    fn unknown_json_does_not_leak_nested_text_into_the_transcript() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({"items": [{"text": "not an ACP message"}]}),
        );
        assert!(chat.entries.is_empty());
    }

    #[test]
    fn message_ids_keep_adjacent_agent_messages_separate() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        for (seq, id, text) in [(1, "one", "first"), (2, "two", "second")] {
            chat.apply_session_update(
                seq,
                &serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": id,
                    "content": {"type": "text", "text": text}
                }),
            );
        }
        assert_eq!(chat.entries.len(), 2);
        assert_eq!(chat.entries[0].text, "first");
        assert_eq!(chat.entries[1].text, "second");
    }

    #[test]
    fn plan_updates_replace_the_current_turn_plan() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        for (seq, status) in [(1, "pending"), (2, "completed")] {
            chat.apply_session_update(
                seq,
                &serde_json::json!({
                    "sessionUpdate": "plan",
                    "entries": [{
                        "content": "inspect renderer",
                        "priority": "high",
                        "status": status
                    }]
                }),
            );
        }
        assert_eq!(chat.entries.len(), 1);
        assert_eq!(chat.entries[0].role, ChatRole::Plan);
        assert_eq!(chat.entries[0].plan[0].status, PlanStatus::Completed);
    }

    #[test]
    fn same_ordinal_materialized_update_keeps_transcript_cache_but_refreshes_queue() {
        let mut session = MaterializedSession::empty("session-same-ordinal");
        session.applied_event_ordinal = 1;
        session.transcript.push(Arc::new(TranscriptItem {
            stable_id: "user:1".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 10,
            last_changed_at_ms: 10,
            body: TranscriptBody::User {
                content: vec![serde_json::json!("first")],
            },
        }));

        let mut chat = ChatState::from_materialized(&session, &[], &[]);
        assert_eq!(chat.entries[0].text, "first");

        Arc::make_mut(&mut session.transcript[0]).body = TranscriptBody::User {
            content: vec![serde_json::json!("changed without new ordinal")],
        };
        session.queued_prompts.push(MaterializedQueuedPrompt {
            command_id: "queued".into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!("queued prompt")],
            queued_at_ms: 20,
        });
        chat.apply_materialized(&session, &[], &[]);

        assert_eq!(chat.entries[0].text, "first");
        assert_eq!(chat.queued_prompts.len(), 1);
        assert_eq!(chat.queued_prompts[0].text, "queued prompt");
    }

    #[test]
    fn materialized_diff_counts_arrive_after_the_path_and_ignore_stale_revisions() {
        let mut session = MaterializedSession::empty("session-diffstats");
        session.applied_event_ordinal = 1;
        session.transcript.push(Arc::new(TranscriptItem {
            stable_id: "tool:edit".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 10,
            last_changed_at_ms: 10,
            body: TranscriptBody::Tool {
                call: serde_json::json!({
                    "toolCallId": "edit",
                    "title": "Edit src/lib.rs",
                    "status": "completed",
                    "content": [{
                        "type": "diff",
                        "path": "/workspace/src/lib.rs",
                        "oldText": "alpha\n",
                        "newText": "alpha\nbeta\n"
                    }]
                }),
                terminal_outputs: Vec::new(),
                terminal_refs: Vec::new(),
            },
        }));

        let mut chat = ChatState::from_materialized(&session, &[], &[]);
        assert_eq!(chat.entries[0].tool_diffstats, ["/workspace/src/lib.rs"]);
        let request = chat.take_diffstat_requests(1).pop().unwrap();
        let exact = request.clone().compute();
        chat.apply_diffstats("tool:edit", 9, exact.clone());
        assert_eq!(chat.entries[0].tool_diffstats, ["/workspace/src/lib.rs"]);
        chat.apply_diffstats("tool:edit", 10, exact);
        assert_eq!(
            chat.entries[0].tool_diffstats,
            ["/workspace/src/lib.rs  +1 −0"]
        );
    }
}
