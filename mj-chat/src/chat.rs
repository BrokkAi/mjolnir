//! Minimal full-screen chat for one persistent Hel worker.
//!
//! The view state lives here; the concerns around it are split into
//! submodules: [`input`] edits the composer, [`history`] recalls earlier
//! prompts, [`autocomplete`] parses and completes slash commands,
//! [`transcript`] projects and draws the conversation, [`remote`] runs the
//! relay operations a key press asks for, and [`active`] wires a live session
//! to all of them.

mod active;
mod attachments;
mod autocomplete;
mod config_picker;
mod elicitation;
mod feedback;
mod history;
mod input;
mod remote;
mod rendering;
mod second_opinion;
mod submissions;
mod transcript;
mod turn_review;
mod viewport;

#[cfg(test)]
mod test_support;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AvailableCommand, ContentBlock, SessionConfigOption, SessionModeState, SessionUpdate,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::text::Line;

use crate::clipboard::{ClipboardContent, ClipboardImage};
use crate::components::{ControlKind, Form, Interaction};
use crate::selection::{FrameSurfaces, SelectionRange};
use crate::text_input;
pub use mj_core::acp::PlanControl;
use mj_core::acp::SessionConfigChoice;
use mj_core::acp::surface::{AcpSessionSurface, PlanControlError};
use mj_core::acp::{RuntimeEvent, plan_review_carries_native_feedback};
use mj_core::attachment::MAX_IMAGES;
use mj_core::clock::epoch_seconds;
use mj_core::config::{Config, HarnessKind};
use mj_core::elicitation::ElicitationValue;
use mj_core::elicitation::{ElicitationRequest, ElicitationResponse};
use mj_core::state::{
    MaterializedExecutionState, MaterializedQueuedPrompt, MaterializedSession, QueuedCommandKind,
    SessionRecord, TranscriptBody, TranscriptItem, TurnOutcomeKind,
};

use mj_core::relay::{
    ActiveAgentTerminal, SequencedEvent, WorkerEvent, WorkerPhase, WorkerSnapshot,
};
#[cfg(test)]
use mj_core::transcript::PlanStatus;
use mj_transcript::transcript::{
    ChatEntry, ChatRole, apply_runtime_event_to_entries, apply_session_update_to_entries,
};

use autocomplete::{
    Autocomplete, CommandChoice, LocalCommand, builtin_command_choices, parse_local_command,
    prompt_invokes_command,
};
use config_picker::ConfigPicker;
use elicitation::ElicitationDialog;
pub use elicitation::ElicitationDraft;
use history::{HistorySearch, HistorySearchRequest};
#[cfg(test)]
use rendering::voice_button_area;
use rendering::{TranscriptRenderMode, sanitize_terminal_text};
pub use rendering::{truncate_line_to_width, wrap_styled_line};
use second_opinion::{SecondOpinion, SecondOpinionIntent};
use transcript::{
    ToolDiffstatRequest, TranscriptAnchor, TranscriptRenderCache, TranscriptScrollbarState,
    TranscriptSelectionSpace, TranscriptToolClickTarget, materialized_chat_entries_reusing,
};
use turn_review::{TurnReview, TurnReviewIntent};

const MOUSE_SCROLL_ROWS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceControl {
    Microphone,
}

/// Controls owned by the background-task dialog. The index is only a
/// frame-local form identity; the task's opaque id is what crosses the remote
/// boundary and is retained in pending state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackgroundTaskControl {
    Stop(usize),
}

fn voice_form() -> Form<VoiceControl> {
    let mut form = Form::new();
    form.declare(VoiceControl::Microphone, ControlKind::Button);
    form.end_frame(VoiceControl::Microphone);
    form
}

pub use active::{ActiveChat, ChatDaemonRequest, PreparedChat};
pub use second_opinion::SecondOpinionIntent as SecondOpinionRequest;
pub use transcript::{
    TAIL_SEED_ITEMS, TranscriptSnapshot, format_event_time, render_agent_message_head,
    render_agent_message_tail,
};
pub use turn_review::TurnReviewIntent as TurnReviewRequest;

pub use attachments::{CHAT_DRAFT_PREFIX, PromptImage, PromptPayload};

/// What `/review status` reports, on every surface.
///
/// It answers the two questions a person actually has: is every turn reviewed,
/// and is one being reviewed right now.
#[must_use]
pub fn review_status_line(review: &mj_core::config::ReviewConfig, open: bool) -> String {
    let armed = match (review.enabled, review.reviewer_profile()) {
        (true, Some(profile)) => format!(
            "Reviewing every completed turn with [review] profile {profile:?} ({} tier)",
            review.tier.label()
        ),
        (true, None) => {
            format!(
                "Reviewing every completed turn with Auto ({} tier)",
                review.tier.label()
            )
        }
        (false, Some(profile)) => format!(
            "Automatic review is off; /review reviews one turn with {profile:?} ({} tier)",
            review.tier.label()
        ),
        (false, None) => {
            format!(
                "Automatic review is off; /review uses Auto ({} tier)",
                review.tier.label()
            )
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
/// border. While busy, the transcript's last row holds activity above the
/// prompt. `footer` is `Some` only when the host wants the chat to own the
/// footer row, which it does while the composer has focus. `overlay` is the
/// rectangle this conversation owns: dialogs and the autocomplete popup are
/// centred and clamped inside it rather than inside the bands above.
/// `title_controls` is how many columns the host draws its own chips into at
/// the right of the transcript's title row, which the title must stop short
/// of. `pane_focused` says the host has given this pane the keyboard and wants
/// the transcript's border drawn in the focused style; a host with a single
/// pane leaves it clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatRegions<'a> {
    pub transcript: Rect,
    pub prompt: Rect,
    pub footer: Option<ChatFooter<'a>>,
    pub overlay: Rect,
    pub title_controls: u16,
    pub pane_focused: bool,
}

/// The footer area and global hints supplied by the host's command registry.
///
/// `banner` replaces the groups entirely while the host is in the middle of
/// something the row has to report instead, such as a pending prefix chord.
/// It is borrowed so the whole structure stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatFooter<'a> {
    pub area: Rect,
    pub chords: &'a [&'a str],
    pub functions: &'a [&'a str],
    pub banner: Option<&'a Line<'static>>,
}

/// The local form state saved while the dashboard attaches another session.
/// The reviewer metadata is part of the identity because reviewer answers are
/// delivered to a different harness than primary-agent answers.
#[derive(Debug, Clone)]
pub struct ChatElicitationDraft {
    form: ElicitationDraft,
    reviewer: bool,
    reviewer_role: Option<String>,
}

impl ChatElicitationDraft {
    #[must_use]
    pub fn matches(&self, request: &ElicitationRequest) -> bool {
        self.form.matches(request)
    }

    pub(super) fn request(&self) -> &ElicitationRequest {
        // This accessor stays crate-private; hosts only need `matches` while
        // ChatState uses it to reconcile delayed reviewer streams.
        self.form.request()
    }

    #[must_use]
    pub fn reviewer(&self) -> bool {
        self.reviewer
    }

    #[must_use]
    pub fn reviewer_role(&self) -> Option<&str> {
        self.reviewer_role.as_deref()
    }
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
    OpenSubagents,
    OpenJevDecisions,
    QuitDetach {
        last_seen_event_ordinal: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatAction {
    TurnControl(mj_core::relay::RelayCommand),
    None,
    OpenSubagents,
    OpenJevDecisions,
    Prompt(String),
    RunShell(String),
    RemoveQueuedPrompt {
        id: String,
        text: String,
        kind: QueuedCommandKind,
    },
    /// Stop one provider-owned background task without ending the session or
    /// cancelling its foreground turn.
    StopBackgroundTask {
        id: String,
    },
    GoalControl {
        action: mj_core::goal::GoalControlAction,
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
    Attach {
        path: PathBuf,
        command: String,
    },
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
    images: Vec<PromptImage>,
    /// The durable queue contained an image we cannot safely materialize for
    /// terminal editing. Keep the entry visible and refuse an edit instead of
    /// silently dropping that content.
    attachments_unsupported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnControlIntent {
    Cancel,
    Steer,
}

impl TurnControlIntent {
    fn escape_hint(self) -> &'static str {
        match self {
            Self::Cancel => "Esc cancels",
            Self::Steer => "Esc steers next",
        }
    }

    fn failure_notice(self, error: &str) -> String {
        let action = match self {
            Self::Cancel => "Cancellation",
            Self::Steer => "Steering request",
        };
        format!("{action} failed: {error}")
    }
}

/// A submit the relay refused. The relay never saw it, so it is never
/// journaled; the chat keeps the record beside the projected entries and
/// draws it at the end of the transcript, preserving the failure and payload
/// until the user explicitly retries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct UnsentPrompt {
    kind: UnsentKind,
    #[serde(flatten)]
    payload: PromptPayload,
    error: String,
    recorded_at_ms: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedChatDraft {
    #[serde(flatten)]
    composer: PromptPayload,
    #[serde(default)]
    unsent: Vec<UnsentPrompt>,
    #[serde(default)]
    pending: Vec<submissions::PendingSubmission>,
}

/// Which submit an [`UnsentPrompt`] stands for. A prompt and a shell command
/// can carry the same text and are cleared independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum UnsentKind {
    Prompt,
    Shell,
    /// A prompt the relay did send and the harness ended without answering
    /// (#970). It is kept here for the same reason a refused one is: so the
    /// text can be put back in the composer without being retyped.
    Unanswered,
}

impl UnsentKind {
    /// The one wording both the notice and the transcript row use.
    fn headline(self) -> &'static str {
        match self {
            Self::Prompt => "Prompt was not sent",
            Self::Shell => "Shell command was not sent",
            Self::Unanswered => "Prompt was not answered",
        }
    }
}

impl UnsentPrompt {
    /// The transcript row for this record.
    fn entry(&self, seq: u64) -> ChatEntry {
        let attachment = if self.payload.images.is_empty() {
            ""
        } else {
            "\nEmpty the composer, then Ctrl-Alt-R to restore the latest unsent prompt"
        };
        let mut entry = ChatEntry::plain(
            seq,
            ChatRole::System,
            format!(
                "{}: {}\n{}{}",
                self.kind.headline(),
                self.error,
                queued_prompt_preview(&self.payload.text),
                attachment,
            ),
        );
        entry.recorded_at_ms = Some(self.recorded_at_ms);
        entry
    }
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
        if self.kind.is_prompt() && self.attachments_unsupported {
            "queued attachments"
        } else if self.kind.is_prompt() && !self.images.is_empty() {
            "queued image"
        } else if self.kind.is_prompt() {
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
    /// Display title from the session list, including any user override.
    pub title: String,
    /// Harness the session runs, so the chat can answer harness-specific
    /// questions (like whether Codex exposes plan mode) without a recovery
    /// context, which the daemon now owns.
    pub harness_kind: Option<HarnessKind>,
    /// Direct children owned by this parent session.
    pub subagent_count: usize,
}

/// The session facts the chat needs to run reviewers and keep per-workspace
/// review settings: the config's harness profiles and targets, and this
/// session's record (workspace, target, identity). Snapshotted when the chat
/// opens and refreshed by the surface when the daemon publishes newer state;
/// the recovery observer that used to travel with these stayed in the daemon.
#[derive(Debug, Clone)]
pub struct ChatSessionContext {
    pub config: Config,
    pub session: SessionRecord,
    pub reviewer_stager: mj_client::session::ReviewerStager,
}

/// A session-local transcript position, retained when its view is replaced.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptPosition(TranscriptAnchor);

pub struct ChatState {
    pub(crate) clear_context_supported: bool,
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
    /// A composer parked in front of a session that is not attached yet: a
    /// Starting/Resuming transition or an in-flight attach. It is the real
    /// composer — same key handling, same rendering — but nothing can be sent
    /// while no session is live, and no command completion is offered because
    /// no session can answer commands.
    standby: bool,
    input: String,
    input_cursor: usize,
    /// Image bytes belong to tracked marker ranges in the composer text.
    input_images: Vec<PromptImage>,
    next_image_number: u64,
    /// Moved out of `input_images` while the remote submit is in flight, so a
    /// failed submission can restore the images alongside their text.
    submitting_images: Vec<PromptImage>,
    /// Sequence numbers map background jobs to their tracked marker. If the
    /// user removes a pending marker, the late result has nowhere to land.
    pending_attachment_markers: BTreeMap<u64, u64>,
    /// Changes whenever the composer text or tracked images change. Clipboard
    /// reads capture this value so a slow native read cannot paste into a new
    /// draft after the user has continued editing.
    input_generation: u64,
    /// Stored prompts from other sessions in this project, oldest-first.
    project_history: Vec<String>,
    /// Stored prompts from this session, oldest-first.
    session_history: Vec<String>,
    project_history_error: Option<String>,
    prompt_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    history_draft_images: Vec<PromptImage>,
    kill_buffer: String,
    kill_images: Vec<PromptImage>,
    /// Set by Ctrl-K so the next Ctrl-K appends instead of replacing.
    chain_kill: bool,
    preferred_column: Option<usize>,
    history_search: Option<HistorySearch>,
    next_history_search_generation: u64,
    pending_history_search: Option<HistorySearchRequest>,
    queued_prompts: VecDeque<QueuedPrompt>,
    /// Submits the relay refused, oldest first. Client-local: `apply_materialized`
    /// rebuilds `entries` from the projection, which never saw these.
    unsent_prompts: Vec<UnsentPrompt>,
    pending_submissions: Vec<submissions::PendingSubmission>,
    submission_renders: Vec<(String, std::time::Instant)>,
    /// The command id of the last turn recorded as unanswered, so the same
    /// projection arriving again does not record it twice (#970).
    unanswered_turn: Option<String>,
    /// Queue entries optimistically moved back into the composer. A relay
    /// snapshot can still contain one until its removal command is projected,
    /// so keep its identity hidden across those stale snapshots.
    pending_queue_removals: BTreeSet<String>,
    /// Images peeled back from queued prompts while their removal command is
    /// still in flight. Keeping them by queue id makes a failed removal exact.
    pending_queue_images: BTreeMap<String, Vec<PromptImage>>,
    active_user_shells: Vec<String>,
    active_agent_terminals: Vec<ActiveAgentTerminal>,
    claimed_agent_terminals: BTreeMap<String, i64>,
    elicitation: Option<ElicitationDialog>,
    /// The latest primary pending forms, retained while a reviewer form owns
    /// the display so the primary can surface immediately when that review
    /// request is answered or withdrawn.
    pending_elicitations: Vec<ElicitationRequest>,
    /// The second-opinion view, when one is open. It owns this pane while it
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
    /// owns this pane while it is up, which is what makes review synchronous:
    /// findings can never land in the middle of the next conversation.
    turn_review: Option<Box<TurnReview>>,
    /// Where the turn review's action buttons sat on the last frame.
    turn_review_action_areas: Vec<(turn_review::ReviewAction, Rect)>,
    /// What `[review]` says, mirrored from the config the TUI already drains,
    /// so `/review status` and the composer title can report it.
    review_config: mj_core::config::ReviewConfig,
    /// Whether the dialog on screen belongs to the reviewer rather than the
    /// primary, so its answer is routed to the harness that asked.
    elicitation_is_reviewers: bool,
    /// Which reviewing role asked the form on screen, when one did.
    elicitation_role: Option<String>,
    goal_state: mj_core::goal::GoalState,
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
    transcript_scrollbar: TranscriptScrollbarState,
    /// Completed tool calls the user has opened in the Rich transcript.
    /// Presentation state is local to this chat and keyed by durable entry.
    expanded_tool_calls: BTreeSet<u64>,
    /// Screen-coordinate targets rebuilt with every transcript frame.
    transcript_tool_click_targets: Vec<TranscriptToolClickTarget>,
    jev_click_targets: Vec<Rect>,
    notices: Notices,
    feedback: Notices,
    connection_feedback: Option<String>,
    operation_feedback: BTreeMap<String, String>,
    conversation_notices: Vec<feedback::ConversationNotice>,
    /// Whether Codex OAuth credentials and the voice helper are available. The runtime
    /// owns discovering this asynchronously; the chat starts disabled until
    /// the host reports a successful probe.
    voice_available: bool,
    voice_active: bool,
    /// The last frame's microphone button, which sits on the prompt's
    /// upper-left border rather than inside its selectable text surface.
    voice_button_area: Option<Rect>,
    voice_form: Form<VoiceControl>,
    /// The last frame's model and effort chips on the prompt's top border.
    /// Each entry pairs the config key its click opens with its hitbox.
    config_chip_areas: Vec<(&'static str, Rect)>,
    /// The embedded background-task control and its dialog.
    task_control_focused: bool,
    task_dialog_open: bool,
    task_dialog_scroll: usize,
    task_dialog_max_scroll: usize,
    task_dialog_form: Form<BackgroundTaskControl>,
    task_control_area: Option<Rect>,
    subagent_count: usize,
    subagent_working_count: usize,
    subagent_control_focused: bool,
    subagent_control_area: Option<Rect>,
    task_dialog_area: Option<Rect>,
    /// The opaque ids represented by the last frame's visible Stop controls.
    /// Keeping this separate from row indices prevents a snapshot reorder
    /// between mouse-down and mouse-up from stopping a different task.
    task_dialog_control_ids: Vec<(BackgroundTaskControl, String)>,
    /// Task ids for which a stop request has been submitted. A successful
    /// provider acknowledgement deliberately leaves the id here until the
    /// next operational snapshot removes the task.
    pending_background_stops: BTreeSet<String>,
    prompt_content_width: usize,
    /// Session-list identity snapshotted when the chat opened.
    header_target: String,
    header_profile: String,
    header_title: String,
    spinner_style: mj_core::config::SpinnerStyle,
    turn_started_at_epoch_seconds: Option<u64>,
    detailed_activity_clocks: bool,
    activity_reachable: bool,
    /// Whether a prompt of ours is in flight. `phase` also goes Running for a
    /// turn the harness started on its own, which the relay refuses to cancel,
    /// so cancellation and the composer's cancel hint key on this instead.
    prompt_in_flight: bool,
    steering_supported: Option<bool>,
    targeted_turn_control_supported: bool,
    active_prompt_id: Option<String>,
    steering: Option<mj_core::relay::SteeringOperation>,
    cancelling_prompt_id: Option<String>,
    turn_control_submitting: bool,
    turn_control_error: Option<String>,
    turn_control_target: Option<String>,
    turn_control_awaiting_state: Option<String>,
    turn_control_dialog_open: bool,
    turn_control_dialog: crate::components::Dialog<turn_control::Control>,
    /// What the session is doing beyond `phase`: the turn the harness started
    /// on its own, and the commands the agent left running.
    session_activity: mj_client::usage_format::SessionActivity,
    /// When the step the agent is on began, so the pane title can age it.
    current_step_started_at_ms: Option<u64>,
    /// Selectable surfaces, rebuilt by every frame in render order so the
    /// selection engine can hit-test the screen the user is looking at.
    pub(super) frame_surfaces: FrameSurfaces,
    /// Visible host shortcuts, indexed through chords followed by function keys.
    footer_command_areas: RefCell<Vec<(usize, Rect)>>,
    /// The row space transcript selections are measured in, re-pinned by every
    /// frame the engine is not holding a transcript selection through.
    transcript_selection: Option<TranscriptSelectionSpace>,
    /// The frozen row space stopped describing the rows on screen. Read and
    /// cleared after each draw; the caller drops the selection.
    transcript_selection_invalid: bool,
    /// Bumped whenever the cached rows are dropped wholesale, so a frozen row
    /// space can tell that the rows it was pinned against are gone.
    render_cache_generation: u64,
    last_clock_text: Option<String>,
    last_animation_frame: Option<Line<'static>>,
}

mod events;
mod input_state;
mod keys;
mod pointer;
mod status;
mod turn_control;

impl ChatState {
    pub fn new(snapshot: &WorkerSnapshot, events: &[SequencedEvent]) -> Self {
        let mut state = Self {
            clear_context_supported: false,
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
            standby: false,
            input: String::new(),
            input_cursor: 0,
            input_images: Vec::new(),
            next_image_number: 1,
            submitting_images: Vec::new(),
            pending_attachment_markers: BTreeMap::new(),
            input_generation: 0,
            project_history: Vec::new(),
            session_history: Vec::new(),
            project_history_error: None,
            prompt_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_draft_images: Vec::new(),
            kill_buffer: String::new(),
            kill_images: Vec::new(),
            chain_kill: false,
            preferred_column: None,
            history_search: None,
            next_history_search_generation: 0,
            pending_history_search: None,
            queued_prompts: VecDeque::new(),
            unsent_prompts: Vec::new(),
            pending_submissions: Vec::new(),
            submission_renders: Vec::new(),
            unanswered_turn: None,
            pending_queue_removals: BTreeSet::new(),
            pending_queue_images: BTreeMap::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            claimed_agent_terminals: BTreeMap::new(),
            elicitation: None,
            pending_elicitations: Vec::new(),
            second_opinion: None,
            reviewer_area: None,
            split_action_areas: Vec::new(),
            second_opinion_sequence: 0,
            turn_review: None,
            turn_review_action_areas: Vec::new(),
            review_config: mj_core::config::ReviewConfig::default(),
            elicitation_is_reviewers: false,
            elicitation_role: None,
            goal_state: mj_core::goal::GoalState::from_configuration(&snapshot.config)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "invalid projected goal state");
                    Default::default()
                }),
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
            transcript_scrollbar: TranscriptScrollbarState::default(),
            expanded_tool_calls: BTreeSet::new(),
            transcript_tool_click_targets: Vec::new(),
            jev_click_targets: Vec::new(),
            notices: Notices::default(),
            feedback: Notices::default(),
            connection_feedback: None,
            operation_feedback: BTreeMap::new(),
            conversation_notices: Vec::new(),
            voice_available: false,
            voice_active: false,
            voice_button_area: None,
            voice_form: voice_form(),
            config_chip_areas: Vec::new(),
            task_control_focused: false,
            task_dialog_open: false,
            task_dialog_scroll: 0,
            task_dialog_max_scroll: 0,
            task_dialog_form: Form::new(),
            task_control_area: None,
            subagent_count: 0,
            subagent_working_count: 0,
            subagent_control_focused: false,
            subagent_control_area: None,
            task_dialog_area: None,
            task_dialog_control_ids: Vec::new(),
            pending_background_stops: BTreeSet::new(),
            prompt_content_width: 1,
            header_target: String::new(),
            header_profile: String::new(),
            header_title: String::new(),
            spinner_style: mj_core::config::SpinnerStyle::default(),
            turn_started_at_epoch_seconds: None,
            detailed_activity_clocks: false,
            activity_reachable: true,
            prompt_in_flight: snapshot.active_prompt.is_some(),
            session_activity: mj_client::usage_format::SessionActivity {
                pursuing_goal: Default::default(),
                prompt_in_flight: snapshot.active_prompt.is_some(),
                ..mj_client::usage_format::SessionActivity::default()
            },
            steering_supported: None,
            targeted_turn_control_supported: false,
            active_prompt_id: None,
            steering: None,
            cancelling_prompt_id: None,
            turn_control_submitting: false,
            turn_control_error: None,
            turn_control_target: None,
            turn_control_awaiting_state: None,
            turn_control_dialog_open: false,
            turn_control_dialog: turn_control::dialog(),
            current_step_started_at_ms: None,
            frame_surfaces: FrameSurfaces::new(),
            footer_command_areas: RefCell::new(Vec::new()),
            transcript_selection: None,
            transcript_selection_invalid: false,
            render_cache_generation: 0,
            last_clock_text: None,
            last_animation_frame: None,
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
                images: Vec::new(),
                attachments_unsupported: false,
            })
            .collect();
        state.latest_seq = state.latest_seq.max(snapshot.latest_seq);
        state
    }

    /// The real composer for a session that is not attached yet: parked
    /// behind a Starting/Resuming transition or an in-flight attach. It edits
    /// exactly like the attached composer — the whole readline chord set —
    /// and `Enter` on a plain prompt clears the input, shows the text as a
    /// queued preview, and returns `ChatAction::Prompt` so the host can have
    /// the daemon deliver it once the session is live. A command keeps the
    /// draft and explains, because no session can answer it yet, and command
    /// completion stays off for the same reason. The draft survives until the
    /// host carries it into the attached chat.
    pub fn standby(
        session_id: &str,
        config: &Config,
        header: SessionHeaderIdentity,
        notices: Notices,
    ) -> Self {
        let snapshot = WorkerSnapshot::summary(session_id.to_owned(), WorkerPhase::Idle, 0);
        let mut state = Self::new(&snapshot, &[]);
        state.notices = notices;
        state.standby = true;
        state.set_subagent_count(header.subagent_count);
        state.set_header_summary(header.target, header.profile, header.title);
        if let Some(harness_kind) = header.harness_kind {
            state.set_harness_kind(harness_kind);
        }
        state.set_review_config(config.review.clone());
        state.set_spinner_style(config.spinner);
        state.set_detailed_activity_clocks(config.advanced.detailed_activity_clocks);
        state
    }

    /// Replaces the draft with `draft`, cursor at the end. An empty draft
    /// leaves the composer alone.
    pub fn set_draft(&mut self, draft: String) {
        self.restore_draft(draft);
    }

    /// The composer's current draft, including any embedded image markers.
    pub fn draft(&self) -> String {
        self.encoded_draft()
    }

    /// Rows the composer wants at `width`: the wrapped input, up to three
    /// queued-prompt previews with a separating row, and the block's borders.
    pub fn desired_prompt_height(&self, width: u16) -> u16 {
        let content_width = active::prompt_content_width(width);
        let input_rows =
            u16::try_from(input::input_visual_rows(&self.input, content_width)).unwrap_or(u16::MAX);
        let queued = u16::try_from(self.queued_prompts.len().min(3)).unwrap_or(3);
        input_rows
            .saturating_add(queued)
            .saturating_add(u16::from(queued > 0))
            .saturating_add(2)
            .max(4)
    }

    /// Draws only the composer band into `area`, for hosts that show the real
    /// prompt while the session is not attached. Clears and re-registers this
    /// band's chat surfaces; the host merges them into its own frame
    /// surfaces. `note` adds a left-aligned line to the bottom border.
    pub fn draw_prompt_band(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        prompt_focused: bool,
        note: Option<Line<'static>>,
    ) {
        self.frame_surfaces.clear();
        self.footer_command_areas.borrow_mut().clear();
        self.config_chip_areas.clear();
        active::render_composer_band(frame, area, self, prompt_focused, note);
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
        let started = std::time::Instant::now();
        self.reconcile_submissions(session);
        self.reconcile_notices(session);
        let rebuild_projection = session.applied_event_ordinal != self.latest_seq;
        let phase = match session.execution {
            MaterializedExecutionState::Idle => WorkerPhase::Idle,
            MaterializedExecutionState::Running { .. } => WorkerPhase::Running,
            MaterializedExecutionState::Closing => WorkerPhase::Closing,
            MaterializedExecutionState::Closed => WorkerPhase::Closed,
        };
        if self.phase != phase {
            self.phase = phase;
        }
        let turn_started_at = turn_started_at_epoch_seconds(session.execution);
        if self.turn_started_at_epoch_seconds != turn_started_at {
            self.turn_started_at_epoch_seconds = turn_started_at;
        }
        self.latest_seq = session.applied_event_ordinal;
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
        self.pending_queue_images
            .retain(|id, _| projected_queue_ids.contains(id.as_str()));
        let queued_prompts = session
            .queued_prompts
            .iter()
            .filter(|prompt| !self.pending_queue_removals.contains(&prompt.command_id))
            .map(|prompt| {
                let (payload, attachments_unsupported) =
                    materialized_content_prompt(&prompt.content);
                QueuedPrompt {
                    id: prompt.command_id.clone(),
                    text: payload.text,
                    kind: prompt.kind.clone(),
                    images: payload.images,
                    attachments_unsupported,
                }
            })
            .collect();
        if self.queued_prompts != queued_prompts {
            self.queued_prompts = queued_prompts;
        }
        match mj_core::goal::GoalState::from_configuration(&session.configuration) {
            Ok(goal) => {
                if self.goal_state != goal {
                    self.goal_state = goal;
                }
            }
            Err(error) => {
                self.goal_state = Default::default();
                self.set_notice(format!("Could not read goal state: {error:#}"));
            }
        }
        tracing::debug!(target: "mj_chat::latency", ordinal = session.applied_event_ordinal, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0, "terminal projection applied");
        self.keep_unanswered_prompt(session);
        self.set_config_options(config_options);
        self.acp_surface
            .apply_projected_configuration(&session.configuration);
        self.acp_surface
            .set_agent_commands(available_commands.to_vec());
        self.rebuild_command_choices();
    }

    /// Keep the text of a prompt the harness ended without answering, so it
    /// can be put back in the composer with Ctrl-Alt-R instead of retyped.
    ///
    /// The prompt is read from the turn's own first transcript item rather
    /// than from anything this client remembers, so a prompt submitted from
    /// the web viewer or promoted from the queue is recoverable here too. The
    /// record is deliberately not a resend: Mjolnir cannot tell a prompt the
    /// harness dropped from one it acted on silently, so resending is the
    /// person's decision (#970).
    fn keep_unanswered_prompt(&mut self, session: &MaterializedSession) {
        let Some(outcome) = &session.last_turn_outcome else {
            return;
        };
        let TurnOutcomeKind::Completed { stop_reason } = &outcome.outcome else {
            return;
        };
        if stop_reason != mj_core::acp::PROMPT_UNANSWERED_STOP_REASON
            || self.unanswered_turn.as_deref() == Some(outcome.command_id.as_str())
        {
            return;
        }
        self.unanswered_turn = Some(outcome.command_id.clone());
        let Some(position) = outcome.turn_start_position else {
            return;
        };
        let Some(TranscriptBody::User { content }) = session
            .transcript
            .iter()
            .find(|item| item.position == position)
            .map(|item| &item.body)
        else {
            return;
        };
        let (payload, _) = materialized_content_prompt(content);
        if payload.text.trim().is_empty() {
            return;
        }
        self.record_unsent_prompt(
            UnsentKind::Unanswered,
            payload.text,
            payload.images,
            "the harness ended the turn without answering; check the workspace before resending"
                .to_owned(),
        );
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
        if self.pending_elicitations != pending {
            self.pending_elicitations = pending.to_vec();
        }
        // A reviewer's form is not in the primary's pending list, so the
        // primary's projection must not take it down.
        if self.elicitation_is_reviewers {
            return;
        }
        if let Some(dialog) = self.elicitation.as_ref() {
            if pending.iter().any(|request| request == dialog.request()) {
                return;
            }
            // An answer or cancellation removed the request. Drop the local
            // form immediately so no later relay snapshot can resurrect it.
            self.elicitation = None;
        }
        let next = pending.first().cloned().map(ElicitationDialog::new);
        if next.is_some() != self.elicitation.is_some() {
            self.elicitation = next;
        }
    }

    /// Reconcile the reviewer form against the sidecar's latest pending
    /// requests. The primary projection intentionally cannot do this because
    /// reviewer requests are kept in a different stream.
    pub(super) fn reconcile_reviewer_elicitation(
        &mut self,
        pending: &[(Option<String>, ElicitationRequest)],
    ) {
        if !self.reviewer_elicitation_open() {
            return;
        }
        let matches = self.elicitation.as_ref().is_some_and(|dialog| {
            pending.iter().any(|(role, request)| {
                role.as_deref() == self.elicitation_role.as_deref() && request == dialog.request()
            })
        });
        if !matches {
            self.elicitation = None;
            self.elicitation_is_reviewers = false;
            self.elicitation_role = None;
            self.elicitation = self
                .pending_elicitations
                .first()
                .cloned()
                .map(ElicitationDialog::new);
        }
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

    pub(super) fn elicitation_source(&self) -> (bool, Option<&str>) {
        (
            self.elicitation_is_reviewers,
            self.elicitation_role.as_deref(),
        )
    }

    pub(super) fn pending_reviewer_matches(&self, draft: &ChatElicitationDraft) -> bool {
        if !draft.reviewer {
            return false;
        }
        match (self.second_opinion(), self.turn_review()) {
            (Some(view), _) => view.reviewer().is_some_and(|reviewer| {
                reviewer
                    .pending_elicitations()
                    .iter()
                    .any(|request| request == draft.request())
            }),
            (None, Some(review)) => {
                review
                    .pending_elicitations()
                    .into_iter()
                    .any(|(role, request)| {
                        draft.reviewer_role() == Some(role.as_str()) && request == *draft.request()
                    })
            }
            (None, None) => false,
        }
    }

    /// Take a process-local snapshot before this chat is replaced by another
    /// session. The snapshot carries reviewer routing metadata as well as the
    /// form values, because those answers do not all go to the primary agent.
    pub(super) fn elicitation_draft(&self) -> Option<ChatElicitationDraft> {
        let dialog = self.elicitation.as_ref()?;
        Some(ChatElicitationDraft {
            form: dialog.draft(),
            reviewer: self.elicitation_is_reviewers,
            reviewer_role: self.elicitation_role.clone(),
        })
    }

    /// Restore a matching snapshot onto the currently pending request. A
    /// request must already be present in the newly attached projection; this
    /// prevents a late attach or an externally answered form from reviving a
    /// stale draft.
    pub(super) fn restore_elicitation_draft(&mut self, draft: ChatElicitationDraft) -> bool {
        let Some(request) = self
            .elicitation
            .as_ref()
            .map(|dialog| dialog.request().clone())
        else {
            return false;
        };
        if self.elicitation_source() != (draft.reviewer, draft.reviewer_role.as_deref()) {
            return false;
        }
        let Some(dialog) = ElicitationDialog::from_draft(request, draft.form) else {
            return false;
        };
        self.elicitation = Some(dialog);
        self.elicitation_is_reviewers = draft.reviewer;
        self.elicitation_role = draft.reviewer_role;
        true
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
}

fn plan_control_error_message(error: PlanControlError) -> &'static str {
    match error {
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

fn materialized_content_prompt(content: &[serde_json::Value]) -> (PromptPayload, bool) {
    let mut payload = PromptPayload::text("");
    let mut unsupported = false;
    for value in content {
        match serde_json::from_value::<ContentBlock>(value.clone()) {
            Ok(ContentBlock::Image(content)) => {
                let image = match mj_core::attachment::image_reference(&content) {
                    Ok(Some(reference)) => ClipboardImage::from_reference(reference),
                    Ok(None) => ClipboardImage::from_base64(content.data, content.mime_type),
                    Err(error) => Err(error),
                };
                match image {
                    Ok(image) => {
                        let number = payload.images.len() as u64 + 1;
                        let end = payload.text.len();
                        attachments::insert_image(
                            &mut payload.text,
                            &mut payload.images,
                            end,
                            number,
                            image,
                        );
                    }
                    Err(_) => unsupported = true,
                }
            }
            Ok(ContentBlock::Text(content)) => payload.text.push_str(&content.text),
            _ if value.is_string() => payload.text.push_str(value.as_str().unwrap()),
            _ => unsupported = true,
        }
    }
    (payload, unsupported)
}

fn prompt_content_blocks(text: &str, images: &[PromptImage]) -> Vec<serde_json::Value> {
    PromptPayload {
        text: text.to_owned(),
        images: images.to_vec(),
    }
    .content_blocks()
    .into_iter()
    .map(|block| serde_json::to_value(block).expect("ACP content serializes"))
    .collect()
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

/// How many past notices the log keeps.
pub const NOTICE_HISTORY: usize = 30;

/// One notice as the log remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeRecord {
    pub text: String,
    pub at: std::time::Instant,
    /// Whether it was a failure, which the log draws in the warning color.
    pub failure: bool,
}

#[derive(Debug, Default)]
struct NoticeSlot {
    notice: Option<Notice>,
    persistent_failure: Option<String>,
    /// Bumped when the displayed text changes, so a dirty-gated renderer can
    /// tell that the bar moved without keeping a copy of its text.
    generation: u64,
    /// The last [`NOTICE_HISTORY`] notices, oldest first, so a burst of
    /// background failures that overwrote each other can still be read.
    history: std::collections::VecDeque<NoticeRecord>,
    /// Failures that arrived while an earlier failure was still protected on
    /// the bar; the bar names their count until it is cleared or dismissed.
    stacked_failures: usize,
}

impl NoticeSlot {
    fn write(&mut self, notice: Option<Notice>) {
        if let Some(notice) = &notice
            && self
                .history
                .back()
                .is_none_or(|last| last.text != notice.text)
        {
            self.history.push_back(NoticeRecord {
                text: notice.text.clone(),
                at: notice.set_at,
                failure: notice.protected,
            });
            while self.history.len() > NOTICE_HISTORY {
                self.history.pop_front();
            }
        }
        if notice.is_none() {
            self.stacked_failures = 0;
        }
        let displayed_text_changed = self.notice.as_ref().map(|current| current.text.as_str())
            != notice.as_ref().map(|next| next.text.as_str());
        self.notice = notice;
        if displayed_text_changed {
            self.generation = self.generation.wrapping_add(1);
        }
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
        let mut slot = self.lock();
        // A failure landing on a failure the person has not had time to
        // read: keep the newest visible and say how many there were.
        let stacking = slot.notice.as_ref().is_some_and(|current| {
            current.protected
                && current.set_at.elapsed() < NOTICE_MINIMUM_DISPLAY
                && current.text != text
        });
        slot.stacked_failures = if stacking {
            slot.stacked_failures.max(1) + 1
        } else {
            1
        };
        let stacked = slot.stacked_failures;
        slot.write(Some(Notice {
            text: text.clone(),
            set_at: std::time::Instant::now(),
            protected: true,
        }));
        if stacked > 1 {
            // The log keeps the plain text; only the bar carries the count.
            if let Some(current) = slot.notice.as_mut() {
                current.text = format!("{stacked} failures · latest: {text}");
                slot.generation = slot.generation.wrapping_add(1);
            }
        }
    }

    /// The notices seen so far, newest first, for the log overlay.
    pub fn history(&self) -> Vec<NoticeRecord> {
        self.lock().history.iter().rev().cloned().collect()
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
        if slot.persistent_failure.is_some() {
            return false;
        }
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
        let slot = self.lock();
        slot.persistent_failure
            .clone()
            .or_else(|| slot.notice.as_ref().map(|notice| notice.text.clone()))
    }

    /// A continuing feed failure stays visible until its owner reports recovery.
    /// Transient notices remain available after recovery and in the notice log.
    pub fn set_persistent_failure(&self, error: Option<String>) {
        let error = error.map(|text| sanitize_terminal_text(&text));
        let mut slot = self.lock();
        if slot.persistent_failure == error {
            return;
        }
        if let Some(text) = &error {
            slot.history.push_back(NoticeRecord {
                text: text.clone(),
                at: std::time::Instant::now(),
                failure: true,
            });
            while slot.history.len() > NOTICE_HISTORY {
                slot.history.pop_front();
            }
        }
        slot.persistent_failure = error;
        slot.generation = slot.generation.wrapping_add(1);
    }

    /// Counts displayed-text changes to the shared slot. A renderer that
    /// records this with each frame can tell that the bar changed since it
    /// last drew, which is what keeps a notice set by background work from
    /// being missed by a dirty-gated draw.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, NoticeSlot> {
        self.0.lock().expect("notices lock poisoned")
    }
}

/// Active work and ready sessions share the terminal's semantic palette.
pub fn turn_band_color(turn_in_flight: bool) -> Color {
    if turn_in_flight {
        crate::theme::palette().accent
    } else {
        crate::theme::palette().success
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

#[cfg(test)]
mod tests;

impl ChatState {
    pub fn transcript_position(&self) -> TranscriptPosition {
        TranscriptPosition(self.anchor)
    }
    pub fn restore_transcript_position(&mut self, position: TranscriptPosition) {
        self.anchor = position.0;
    }
}
