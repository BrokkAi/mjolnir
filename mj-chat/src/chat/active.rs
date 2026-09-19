//! The live conversation: the background feeds behind an open session, and the
//! transcript and composer the combined surface asks it to draw into the
//! regions it has chosen.

mod render;
pub(crate) use render::*;

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::theme;
use anyhow::Result;
use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph, Wrap};

use crate::components::{Button, ControlKind};
use crate::components::{EventResult, render_scrollbar, scrollbar_geometry};
use crate::selection::{SurfaceFrame, SurfaceId};
use mj_core::config::Config;
use mj_core::state::{MaterializedSession, SessionRecord, TranscriptItem, config_command_text};
use mj_core::storage::{HistoryScope, PromptHistoryEntry};

use mj_client::session::{
    ManagedSessionView, ReviewerAction, ReviewerOutcome, SessionControl as SessionManagerControl,
    SessionHandle as ManagedSessionHandle, ViewError, new_command_id,
};
use mj_core::relay::WorkerPhase;
use mj_core::transcript::ChatEntry;

use super::attachments;
use super::autocomplete::render_autocomplete;
use super::elicitation::render_elicitation_in;
use super::history::{highlighted_input_lines, history_scope_name, history_search_footer};
use super::input::{input_cursor_visual_position, set_input_cursor};
use super::remote::{
    ChatRemoteOperation, ChatRemoteResult, ChatRemoteSupervisor, apply_chat_remote_result,
    queue_chat_remote_operation, restore_unsent_input, restore_unsent_prompt,
};
use super::rendering::{
    display_width, truncate_line_to_width, truncate_to_width, voice_button_area,
    voice_button_glyph, voice_button_line, wrap_styled_line,
};
use super::second_opinion::{
    CapturedProposal, ReviewerPane, SecondOpinion, SecondOpinionIntent, render_reviewer,
    render_setup, render_split_actions, review_role_session_id, reviewer_session_id,
};
use super::transcript::{ToolDiffstatRequest, materialized_prefix_entries, render_transcript};
use super::{
    BackgroundTaskControl, ChatAction, ChatElicitationDraft, ChatEventOutcome, ChatFooter,
    ChatRegions, ChatSessionContext, ChatState, MOUSE_SCROLL_ROWS, Notices, SessionHeaderIdentity,
    queued_prompt_preview,
};
use crate::clipboard::{ClipboardContent, ClipboardImage};

/// Durable chat-side state that a host process must ask the daemon to store.
#[derive(Debug, Clone)]
/// What the chat asks the controller daemon to do.
///
/// The chat runs in the terminal process, which owns no session state and no
/// database: everything here crosses to the daemon, which does. Review actions
/// travel this way too -- the review runs there.
pub enum ChatDaemonRequest {
    SaveReview {
        session_id: String,
        review: mj_core::storage::StoredReview,
    },
    ClearReview {
        session_id: String,
    },
    RememberReviewerSelection {
        workspace_id: String,
        selection: mj_core::second_opinion::ReviewerSelection,
    },
    /// Review the turn this session just finished.
    StartTurnReview {
        session_id: String,
    },
    /// Forward the findings, dismiss them, or cancel the review.
    ResolveTurnReview {
        session_id: String,
        resolution: mj_core::review::driver::Resolution,
    },
}
use agent_client_protocol::schema::v1::SessionConfigOption;
use mj_core::second_opinion::{
    ReviewWorkflow, ReviewerDefaults, ReviewerProfileChoice, ReviewerSelection, ReviewerSetup,
    SetupRequest, WorkflowRequest,
};

const MAX_DIFFSTAT_TASKS: usize = 2;
/// How long an idle reviewing role waits before reading its journal again. An
/// attach answers immediately even when nothing has been journaled, so without
/// this a review with several roles would spin on empty pages.
const REVIEW_POLL_IDLE_INTERVAL: Duration = Duration::from_millis(200);
const SESSION_ACTOR_RECONNECT_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum ChatIoUpdate {
    ProjectHistoryPrefetched(std::result::Result<Vec<PromptHistoryEntry>, String>),
    HistorySearchResults {
        generation: u64,
        result: std::result::Result<Vec<PromptHistoryEntry>, String>,
    },
    Clipboard {
        generation: u64,
        result: std::result::Result<ClipboardContent, String>,
    },
    AttachmentFinished(AttachmentResult),
    /// The history a large session did not convert when it opened, built off
    /// the event loop. `attempt` counts the tries so far, so a transcript that
    /// keeps changing under the conversion cannot retry for ever.
    TranscriptPrefix {
        attempt: u32,
        result: std::result::Result<(Vec<ChatEntry>, Vec<ToolDiffstatRequest>), String>,
    },
    ToolDiffstats {
        tool_call_id: String,
        revision: u64,
        result: std::result::Result<Vec<String>, String>,
    },
    SessionReconnected(std::result::Result<ManagedSessionHandle, String>),
    /// A reviewer setup step finished. `generation` is the probe it belongs
    /// to, so a result the user has already moved past is discarded.
    ReviewerProbe {
        generation: u64,
        result: std::result::Result<Vec<SessionConfigOption>, String>,
    },
    /// A reviewer model change finished.
    ReviewerConfigured {
        generation: u64,
        result: std::result::Result<Vec<SessionConfigOption>, String>,
    },
    /// The chosen reviewer is running and the review can begin.
    ReviewerStarted(std::result::Result<(), String>),
    /// A page of the reviewer's own relay events.
    ReviewerEvents {
        result: std::result::Result<Vec<mj_core::relay::RelayEvent>, String>,
    },
    /// A page of one reviewing role's own relay events.
    TurnReviewEvents {
        role: String,
        result: std::result::Result<Vec<mj_core::relay::RelayEvent>, String>,
    },
}

const MAX_ATTACHMENT_TASKS: usize = 2;

#[derive(Debug)]
struct AttachmentResult {
    sequence: u64,
    command: Option<String>,
    result: std::result::Result<ClipboardImage, String>,
}

#[derive(Debug)]
enum AttachmentSource {
    Clipboard(ClipboardImage),
    Path(PathBuf),
}

/// How many times a refused prefix is rebuilt before the view settles for its
/// tail. Compaction rewriting the history under a pending conversion is rare,
/// and one rebuild against the current snapshot normally lands.
const MAX_PREFIX_CONVERSION_ATTEMPTS: u32 = 3;

fn dispatch_history_search_request(
    session: ManagedSessionHandle,
    chat: &mut ChatState,
    updates: &tokio::sync::mpsc::UnboundedSender<ChatIoUpdate>,
) {
    let Some(request) = chat.take_history_search_request() else {
        return;
    };
    let generation = request.generation;
    let updates = updates.clone();
    tokio::spawn(async move {
        let result = ChatState::resolve_history_search_request(request, &session).await;
        if let Err(error) = updates.send(ChatIoUpdate::HistorySearchResults { generation, result })
        {
            tracing::debug!(%error, "history search result dropped because the chat closed");
        }
    });
}

fn dispatch_diffstat_requests(
    chat: &mut ChatState,
    updates: &tokio::sync::mpsc::UnboundedSender<ChatIoUpdate>,
    in_flight: &mut usize,
) {
    let available = MAX_DIFFSTAT_TASKS.saturating_sub(*in_flight);
    for request in chat.take_diffstat_requests(available) {
        *in_flight += 1;
        let tool_call_id = request.tool_call_id.clone();
        let revision = request.revision;
        let updates = updates.clone();
        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(move || request.compute()).await {
                Ok(result) => result,
                Err(error) => Err(format!("diff summary task failed: {error}")),
            };
            if let Err(error) = updates.send(ChatIoUpdate::ToolDiffstats {
                tool_call_id,
                revision,
                result,
            }) {
                tracing::debug!(%error, "tool diff summary dropped because the chat closed");
            }
        });
    }
}

/// The history a tail-first open still owes the view: the transcript items in
/// front of the loaded tail, and the frontier they are converted against.
/// Cloning the item vector copies handles, not conversations.
struct PendingPrefix {
    items: Vec<Arc<TranscriptItem>>,
    frontier: u64,
}

impl PendingPrefix {
    fn of(session: &MaterializedSession, length: usize) -> Option<Self> {
        let length = length.min(session.transcript.len());
        (length > 0).then(|| Self {
            items: session.transcript[..length].to_vec(),
            frontier: session.applied_event_ordinal,
        })
    }
}

/// Converts the unloaded history on a blocking thread and reports it back over
/// the chat's I/O feed, including the failure of the conversion itself.
fn spawn_transcript_prefix(
    pending: PendingPrefix,
    attempt: u32,
    updates: tokio::sync::mpsc::UnboundedSender<ChatIoUpdate>,
) {
    tokio::spawn(async move {
        let result = match tokio::task::spawn_blocking(move || {
            let entries = materialized_prefix_entries(&pending.items, pending.frontier);
            let diffstats = pending
                .items
                .iter()
                .filter_map(ToolDiffstatRequest::from_item)
                .collect();
            (entries, diffstats)
        })
        .await
        {
            Ok(entries) => Ok(entries),
            Err(error) => Err(format!("history conversion task failed: {error}")),
        };
        if let Err(error) = updates.send(ChatIoUpdate::TranscriptPrefix { attempt, result }) {
            tracing::debug!(%error, "transcript conversion result dropped because the chat closed");
        }
    });
}

/// Whether applying an update left history that still has to be converted.
#[derive(Debug, PartialEq, Eq)]
enum PrefixRebuild {
    NotNeeded,
    /// The converted history no longer lines up with the tail, so it has to be
    /// rebuilt from the session's current snapshot. `attempt` numbers the try.
    Needed {
        attempt: u32,
    },
}

fn apply_chat_io_update(chat: &mut ChatState, update: ChatIoUpdate) -> PrefixRebuild {
    match update {
        ChatIoUpdate::TranscriptPrefix { attempt, result } => match result {
            Ok((entries, diffstats)) => {
                if chat.splice_transcript_prefix(entries) {
                    chat.queue_diffstat_requests(diffstats);
                    return PrefixRebuild::NotNeeded;
                }
                if attempt >= MAX_PREFIX_CONVERSION_ATTEMPTS {
                    chat.set_notice(
                        "Earlier messages could not be loaded; showing the recent history only.",
                    );
                    return PrefixRebuild::NotNeeded;
                }
                return PrefixRebuild::Needed {
                    attempt: attempt.saturating_add(1),
                };
            }
            Err(error) => {
                tracing::warn!(%error, "earlier chat history could not be converted");
                chat.set_notice(format!("Earlier messages failed to load: {error}"));
            }
        },
        ChatIoUpdate::ProjectHistoryPrefetched(Ok(entries)) => chat.set_project_history(entries),
        ChatIoUpdate::ProjectHistoryPrefetched(Err(error)) => {
            tracing::warn!(%error, "project chat history prefetch failed");
            chat.set_project_history_unavailable(error);
        }
        ChatIoUpdate::HistorySearchResults { generation, result } => {
            chat.apply_history_search_results(generation, result);
        }
        ChatIoUpdate::Clipboard { result, .. } => match result {
            Ok(content) => chat.handle_clipboard_content(content),
            Err(error) => {
                tracing::warn!(%error, "clipboard read failed and was shown in the UI");
                chat.set_notice(format!("Paste failed: {error}"));
            }
        },
        ChatIoUpdate::ToolDiffstats {
            tool_call_id,
            revision,
            result,
        } => chat.apply_diffstats(&tool_call_id, revision, result),
        // Reviewer updates are handled where the session handle is, because
        // acting on one starts more reviewer work.
        ChatIoUpdate::ReviewerProbe { .. }
        | ChatIoUpdate::ReviewerConfigured { .. }
        | ChatIoUpdate::ReviewerStarted(_)
        | ChatIoUpdate::ReviewerEvents { .. }
        | ChatIoUpdate::TurnReviewEvents { .. } => {}
        ChatIoUpdate::AttachmentFinished(_) => {
            unreachable!("attachment results are applied by ActiveChat")
        }
        ChatIoUpdate::SessionReconnected(_) => {
            unreachable!("session reconnects are applied by ActiveChat")
        }
    }
    PrefixRebuild::NotNeeded
}

/// Applies one session view to the chat. `false` means this particular actor's
/// feed has closed, so it must not be awaited while the chat reacquires the
/// manager's replacement actor.
///
/// This runs whether or not the chat is on screen: a warm chat behind the
/// session list stays as current as one the user is watching.
fn apply_session_view(state: &mut ChatState, view: Result<ManagedSessionView>) -> bool {
    let view = match view {
        Ok(view) => view,
        Err(error) => {
            // Keep the transcript readable rather than tearing the surface
            // down around a stopped manager.
            if state.activity_reachable {
                state.activity_reachable = false;
            }
            tracing::warn!(error = format!("{error:#}"), "chat session view failed");
            state.set_connection_notice(format!("connection lost: {error:#}"));
            return false;
        }
    };
    // A transient connection error does not make an as-yet-unavailable
    // transcript empty. The actor keeps retrying, so retain the loading row
    // until a real projection arrives; the error still appears in the notice.
    let activity_reachable = view.connected && view.snapshot.is_some() && view.error.is_none();
    if state.activity_reachable != activity_reachable {
        state.activity_reachable = activity_reachable;
    }
    if view.connected && view.error.is_none() {
        state.connection_feedback = None;
    }
    if view.snapshot.is_some() {
        state.set_transcript_loading(false);
    }
    if let Some(snapshot) = view.snapshot {
        state.apply_materialized(
            &snapshot.materialized,
            &snapshot.operational.config_options,
            &snapshot.operational.available_commands,
        );
        state.set_session_modes(snapshot.operational.modes.clone());
        state.set_active_user_shells(&snapshot.operational.active_user_shells);
        state.set_active_agent_terminals(
            &snapshot.operational.active_agent_terminals,
            &snapshot.materialized,
        );
        state.set_current_step_start(snapshot.operational.current_step_started_at_ms);
        state.set_prompt_in_flight(snapshot.operational.active_prompt.is_some());
        if state.steering_supported != snapshot.operational.steering_supported {
            state.steering_supported = snapshot.operational.steering_supported;
        }
        state.set_session_activity(mj_client::usage_format::SessionActivity::of(
            &snapshot.operational,
        ));
    }
    if let Some(error) = view.error {
        if state.activity_reachable {
            state.activity_reachable = false;
        }
        match error {
            ViewError::Unreachable(detail) => {
                tracing::warn!(%detail, "chat session became unreachable");
                state.set_connection_notice(format!("connection lost: {detail}"))
            }
            ViewError::TargetMissing(detail) => {
                tracing::warn!(%detail, "chat session target is missing");
                state.set_connection_notice(format!("managed target lost: {detail}"))
            }
            ViewError::ProjectionIntegrity(detail) => {
                tracing::error!(%detail, "chat transcript projection failed");
                state.set_connection_notice(format!("transcript projection failed: {detail}"))
            }
        }
    }
    true
}

/// The user has left the chat, whether for the session list, another
/// conversation, or the shell. Clears the interaction state that should not
/// follow them back in and reports how far they have now read, which becomes
/// the session's read receipt.
fn detach_chat(state: &mut ChatState) -> u64 {
    let last_seen_event_ordinal = state.latest_seq();
    state.reset_interaction();
    last_seen_event_ordinal
}

/// A chat view and every background feed behind it.
///
/// The combined surface owns one of these for the conversation on screen. It
/// keeps following the worker while another pane has the keyboard, so nothing
/// is lost while the user looks elsewhere. Dropping it detaches the proxy and
/// leaves the target worker alive.
pub struct ActiveChat {
    reviewer_defaults: ReviewerDefaults,
    state: ChatState,
    session: ManagedSessionHandle,
    session_manager: SessionManagerControl,
    context: Option<ChatSessionContext>,
    remote: ChatRemoteSupervisor,
    /// Held so the receiver never reports the feed closed, and so spawned
    /// clipboard and history tasks have somewhere to report.
    chat_io_tx: tokio::sync::mpsc::UnboundedSender<ChatIoUpdate>,
    chat_io_rx: tokio::sync::mpsc::UnboundedReceiver<ChatIoUpdate>,
    diffstats_in_flight: usize,
    /// Held for the same reason, and cloned into each dictation thread.
    voice_updates_tx: tokio::sync::mpsc::UnboundedSender<VoiceUpdate>,
    voice_updates_rx: tokio::sync::mpsc::UnboundedReceiver<VoiceUpdate>,
    voice_cancel: Option<std::sync::mpsc::Sender<crate::speech::VoiceCommand>>,
    voice_auth: Option<std::path::PathBuf>,
    voice_probe_at: Option<std::time::Instant>,
    voice_probe_pending: bool,
    voice_probe_paths: Vec<std::path::PathBuf>,
    voice_finishing: bool,
    /// A closed feed reports `None` for ever, which would leave its arm
    /// permanently ready. Each flag retires its own arm instead.
    remote_open: bool,
    session_open: bool,
    session_reconnect_in_flight: bool,
    session_feed_expected: bool,
    /// Whether this session is being retired on purpose: a stop or destroy is
    /// in flight, or its record is no longer pollable. A closed feed is then
    /// expected, so it starts no handoff attempt and reports no failure.
    session_retiring: bool,
    /// Preserve the stronger reconnect result when the initial sync, which
    /// independently reacquires the same actor, finishes just afterwards.
    reconnect_notice_pending_sync: bool,
    /// The reviewer lifetime this session is on. It is bumped only when the
    /// reviewer's native conversation is lost, never by an ordinary probe, so
    /// a repeat review reloads the same conversation.
    reviewer_generation: u64,
    /// The remembered selection a resumed review is starting under, if this
    /// workspace has already chosen a reviewer. It short-circuits the
    /// waterfall: the choice is only asked again when this fails.
    resuming_reviewer: Option<ReviewerSelection>,
    /// Reviewing roles whose journals this chat is already reading. The
    /// daemon runs the review; the terminal only displays it, so this is
    /// display bookkeeping and nothing more.
    reviewed_roles: BTreeSet<String>,
    persistence: Option<tokio::sync::mpsc::UnboundedSender<ChatDaemonRequest>>,
    /// A cached question may arrive in the newly attached review stream a few
    /// ticks after the chat itself opens. Keep it here until that exact source
    /// form surfaces, instead of dropping it or applying it to the primary.
    deferred_elicitation_draft: Option<ChatElicitationDraft>,
    /// At most one native clipboard operation may run for this chat. A second
    /// paste is reported instead of starting an unbounded burst of readers.
    paste_in_flight: bool,
    /// Image codecs and session-store writes run in supervised blocking tasks.
    /// Each result carries its marker sequence, so completion order cannot
    /// reorder images already laid out in the composer.
    attachment_queue: VecDeque<(u64, AttachmentSource, Option<String>)>,
    attachment_tasks_in_flight: usize,
    next_attachment_sequence: u64,
}

/// Sendable chat initialization data. Build this off the UI thread, then
/// call `open` on the UI thread to create thread-local control identities.
pub struct PreparedChat {
    session: ManagedSessionHandle,
    bundle_id: String,
    context: Option<ChatSessionContext>,
    control: SessionManagerControl,
    header: SessionHeaderIdentity,
    draft: String,
    notices: Notices,
    persistence: Option<tokio::sync::mpsc::UnboundedSender<ChatDaemonRequest>>,
    stored_review: std::result::Result<Option<mj_core::storage::StoredReview>, String>,
    reviewer_defaults: ReviewerDefaults,
    reviewer: ReviewerPane,
}

impl PreparedChat {
    pub fn with_review_state(
        mut self,
        state: std::result::Result<mj_client::session::ReviewState, String>,
    ) -> Self {
        match state {
            Ok(mut state) => {
                if let Some(stored) = &mut state.review {
                    self.reviewer.restore(
                        &reviewer_session_id(self.session.session_id()),
                        std::mem::take(&mut stored.reviewer_transcript),
                    );
                }
                self.stored_review = Ok(state.review);
                self.reviewer_defaults = state.defaults;
            }
            Err(error) => self.stored_review = Err(error),
        }
        self
    }

    /// Refresh the local composer after asynchronous preparation. A surface
    /// may have captured newer input while this attachment was in flight.
    pub fn with_draft(mut self, draft: String) -> Self {
        self.draft = draft;
        self
    }

    /// Constructs the view without filesystem or database access.
    pub fn open(self) -> ActiveChat {
        ActiveChat::from_prepared(self)
    }

    /// A same-session handoff keeps the latest local composer, not the saved
    /// draft captured before asynchronous preparation. Other sessions retain
    /// their own saved drafts.
    pub fn open_replacing(mut self, previous: Option<&ActiveChat>) -> ActiveChat {
        if let Some(previous) = previous
            && previous.session_id() == self.session.session_id()
        {
            self.draft = previous.draft();
        }
        self.open()
    }
}

mod dispatch;
mod io;
mod reviewer;
mod surfaces;

impl ActiveChat {
    /// Builds the view from the session's current snapshot and starts its
    /// background feeds. This convenience constructor uses empty review state; applications load
    /// review state asynchronously and pass it through `PreparedChat::with_review_state`.
    /// Interactive hosts prepare it with
    /// `prepare_with_persistence`, then construct the view with `PreparedChat::open`.
    ///
    /// `draft` is the unsent input saved when this session was last detached.
    /// Only a fresh view takes it: a warm chat the surface kept alive already
    /// holds newer input than the database copy.
    ///
    /// `notices` is the process-wide notifications bar; it is installed on the
    /// new state before any notice is raised below, so recovery and connection
    /// notices land in the same shared slot the surface reads.
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        session: ManagedSessionHandle,
        bundle_id: &str,
        context: Option<ChatSessionContext>,
        control: SessionManagerControl,
        header: SessionHeaderIdentity,
        draft: String,
        notices: Notices,
    ) -> Self {
        Self::open_with_persistence(
            session, bundle_id, context, control, header, draft, notices, None,
        )
    }

    /// Open a chat whose mutations are forwarded to its host's daemon client.
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_persistence(
        session: ManagedSessionHandle,
        bundle_id: &str,
        context: Option<ChatSessionContext>,
        control: SessionManagerControl,
        header: SessionHeaderIdentity,
        draft: String,
        notices: Notices,
        persistence: Option<tokio::sync::mpsc::UnboundedSender<ChatDaemonRequest>>,
    ) -> Self {
        Self::prepare_with_persistence(
            session,
            bundle_id,
            context,
            control,
            header,
            draft,
            notices,
            persistence,
        )
        .open()
    }

    /// Reads stored review state off the UI thread. The returned data can cross
    /// a task channel; live focus flags are created only by `PreparedChat::open`.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_with_persistence(
        session: ManagedSessionHandle,
        bundle_id: &str,
        context: Option<ChatSessionContext>,
        control: SessionManagerControl,
        header: SessionHeaderIdentity,
        draft: String,
        notices: Notices,
        persistence: Option<tokio::sync::mpsc::UnboundedSender<ChatDaemonRequest>>,
    ) -> PreparedChat {
        let stored_review = Ok(None);
        let reviewer = ReviewerPane::default();
        let reviewer_defaults = ReviewerDefaults::default();
        PreparedChat {
            session,
            bundle_id: bundle_id.to_owned(),
            context,
            control,
            header,
            draft,
            notices,
            persistence,
            stored_review,
            reviewer_defaults,
            reviewer,
        }
    }

    fn from_prepared(prepared: PreparedChat) -> Self {
        let PreparedChat {
            session,
            bundle_id,
            context,
            control,
            header,
            draft,
            notices,
            persistence,
            stored_review,
            reviewer_defaults,
            reviewer,
        } = prepared;
        let view = session.view();
        let needs_initial_sync = view.snapshot.is_none();
        // The history a tail-first open leaves behind is converted off the
        // loop; those entries arrive over the I/O feed and are spliced in front
        // of the tail.
        let (mut state, pending_prefix) = {
            let empty = MaterializedSession::empty(session.session_id());
            let snapshot = view.snapshot;
            let materialized = snapshot
                .as_ref()
                .map_or(&empty, |snapshot| &snapshot.materialized);
            let mut state = ChatState::from_materialized_tail(
                materialized,
                snapshot
                    .as_ref()
                    .map_or(&[][..], |snapshot| &snapshot.operational.config_options),
                snapshot
                    .as_ref()
                    .map_or(&[][..], |snapshot| &snapshot.operational.available_commands),
            );
            if let Some(harness_kind) = header
                .harness_kind
                .or_else(|| context.as_ref().map(|context| context.session.harness_kind))
            {
                state.set_harness_kind(harness_kind);
            }
            if let Some(context) = context.as_ref() {
                state.set_review_config(context.config.review.clone());
                state.set_spinner_style(context.config.spinner);
                state
                    .set_detailed_activity_clocks(context.config.advanced.detailed_activity_clocks);
            }
            state.set_session_modes(
                snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.operational.modes.clone()),
            );
            if let Some(snapshot) = snapshot.as_ref() {
                state.set_active_user_shells(&snapshot.operational.active_user_shells);
                state.set_active_agent_terminals(
                    &snapshot.operational.active_agent_terminals,
                    &snapshot.materialized,
                );
                state.set_current_step_start(snapshot.operational.current_step_started_at_ms);
                state.set_prompt_in_flight(snapshot.operational.active_prompt.is_some());
                state.steering_supported = snapshot.operational.steering_supported;
                state.set_session_activity(mj_client::usage_format::SessionActivity::of(
                    &snapshot.operational,
                ));
            }
            let pending = PendingPrefix::of(materialized, state.unconverted_prefix());
            (state, pending)
        };
        state.set_history_context(&bundle_id);
        state.set_subagent_count(header.subagent_count);
        state.set_header_summary(header.target, header.profile, header.title);
        state.restore_draft(draft);
        state.notices = notices;
        let (chat_io_tx, chat_io_rx) = tokio::sync::mpsc::unbounded_channel::<ChatIoUpdate>();
        {
            let updates = chat_io_tx.clone();
            let history_session = session.clone();
            let bundle_id = bundle_id.to_owned();
            tokio::spawn(async move {
                let result = history_session
                    .search_prompts(bundle_id, HistoryScope::Project, String::new())
                    .await
                    .map_err(|error| format!("{error:#}"));
                if let Err(error) = updates.send(ChatIoUpdate::ProjectHistoryPrefetched(result)) {
                    tracing::debug!(%error, "project history result dropped because the chat closed");
                }
            });
        }
        if let Some(pending) = pending_prefix {
            spawn_transcript_prefix(pending, 1, chat_io_tx.clone());
        }
        let (voice_updates_tx, voice_updates_rx) =
            tokio::sync::mpsc::unbounded_channel::<VoiceUpdate>();
        let remote = ChatRemoteSupervisor::spawn(session.clone(), control.clone());
        if needs_initial_sync {
            state.set_transcript_loading(true);
            state.set_connection_notice("Connecting to session relay…");
            queue_chat_remote_operation(remote.operations(), ChatRemoteOperation::Sync, &mut state);
        }
        let mut diffstats_in_flight = 0;
        dispatch_diffstat_requests(&mut state, &chat_io_tx, &mut diffstats_in_flight);
        // A review that was open when the UI stopped is picked back up. The
        // reviewer's own journal on the target holds its conversation, so the
        // split is restored by replaying it rather than by keeping a second
        // copy of the transcript here.
        let stored = stored_review.unwrap_or_else(|error| {
            tracing::warn!(%error, "could not restore the open review");
            state.set_notice(format!("Could not restore the open review: {error}"));
            None
        });
        let reviewer_generation = stored.as_ref().map_or(0, |review| review.generation);
        if let Some(stored) = stored.filter(|stored| !stored.workflow.finished()) {
            let captured = CapturedProposal {
                request: mj_core::acp::normalized_plan_review(
                    stored.workflow.proposal_id().to_owned(),
                    &serde_json::json!({ "plan": stored.workflow.proposal() }),
                ),
                proposal: stored.workflow.proposal().to_owned(),
            };
            state.open_second_opinion(
                captured,
                ReviewerSetup::new(
                    String::new(),
                    Vec::new(),
                    mj_core::second_opinion::ReviewerDefaults::default(),
                ),
            );
            let status = if stored.native_lost {
                "the reviewer's conversation did not survive; a new review starts fresh"
            } else {
                "reloading the review…"
            };
            if let Some(view) = state.second_opinion_mut() {
                view.begin_review(stored.workflow, status, stored.context_baseline);
                // The reviewer's own journal is the source while the target
                // lives; this copy is what keeps the conversation readable
                // once it does not.
                view.restore_prepared_reviewer(reviewer);
            }
        }
        // Raised after connection and review notices, because a notice is a single
        // slot: a cold open would otherwise replace the failure the user has
        // to see with "Connecting to session relay…".
        if let Some(detail) = context
            .as_ref()
            .and_then(|context| context.session.last_checkpoint_error.as_deref())
        {
            state.set_notice(format!("Recovery copy failed: {detail}"));
        }
        let mut chat = Self {
            reviewer_defaults,
            state,
            session,
            session_manager: control,
            context,
            remote,
            chat_io_tx,
            chat_io_rx,
            diffstats_in_flight,
            voice_updates_tx,
            voice_updates_rx,
            voice_cancel: None,
            voice_auth: None,
            voice_probe_at: None,
            voice_probe_pending: false,
            voice_probe_paths: Vec::new(),
            voice_finishing: false,
            remote_open: true,
            session_open: true,
            session_reconnect_in_flight: false,
            session_feed_expected: false,
            session_retiring: false,
            reconnect_notice_pending_sync: false,
            reviewer_generation,
            resuming_reviewer: None,
            reviewed_roles: BTreeSet::new(),
            persistence,
            deferred_elicitation_draft: None,
            paste_in_flight: false,
            attachment_queue: VecDeque::new(),
            attachment_tasks_in_flight: 0,
            next_attachment_sequence: 0,
        };
        chat.refresh_voice_availability();
        if chat.state.second_opinion_split() {
            chat.poll_reviewer_events();
        }
        chat
    }

    pub fn session_id(&self) -> &str {
        self.session.session_id()
    }

    /// Snapshot every pending question so the host can retain local answers
    /// while this single warm chat is replaced by another session.
    pub fn elicitation_draft(&self) -> Option<ChatElicitationDraft> {
        self.elicitation_drafts().into_iter().next()
    }

    /// Snapshot every local question that may need to survive this chat's
    /// replacement. A reviewer form can be deferred while the sidecar stream
    /// catches up, so it lives alongside the currently visible form.
    pub fn elicitation_drafts(&self) -> Vec<ChatElicitationDraft> {
        let mut drafts = self
            .state
            .elicitation_draft()
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(draft) = self.deferred_elicitation_draft.clone() {
            drafts.push(draft);
        }
        drafts
    }

    /// Restore all matching drafts collected before this session was
    /// detached. The state machine decides whether each request is currently
    /// visible or must wait for the reviewer stream.
    pub fn restore_elicitation_drafts(&mut self, drafts: Vec<ChatElicitationDraft>) {
        for draft in drafts {
            let _ = self.restore_elicitation_draft(draft);
        }
    }

    /// Restore a draft only when the newly attached projection is already
    /// showing the exact same pending request.
    pub fn restore_elicitation_draft(&mut self, draft: ChatElicitationDraft) -> bool {
        if self.state.restore_elicitation_draft(draft.clone()) {
            true
        } else if self.state.pending_reviewer_matches(&draft) {
            // The review projection is present, but its native form has not
            // yet been surfaced by `surface_reviewer_elicitations`.
            self.deferred_elicitation_draft = Some(draft);
            true
        } else {
            false
        }
    }

    fn apply_deferred_elicitation_draft(&mut self) {
        let Some(draft) = self.deferred_elicitation_draft.take() else {
            return;
        };
        if !self.state.restore_elicitation_draft(draft.clone()) {
            // Keep it only while the review projection still names this exact
            // request. An answer, removal, or changed request invalidates it
            // immediately, including when the primary currently has no form.
            if self.state.pending_reviewer_matches(&draft) {
                self.deferred_elicitation_draft = Some(draft);
            }
        }
    }

    /// Whether a second opinion is open on this session.
    ///
    /// Stopping the session tears its target down, which takes the reviewer's
    /// conversation with it, so the stop confirmation asks about this.
    pub fn has_open_review(&self) -> bool {
        self.state.second_opinion_active()
    }

    /// Whether this view is still attached to a live session actor.
    ///
    /// Pause, destroy, and a replaced target retire the actor and close this
    /// feed. A visible chat reacquires replacements in place; the flag remains
    /// useful while that asynchronous handoff is in flight.
    pub fn session_feed_open(&self) -> bool {
        self.session_open
    }

    /// Keeps a visible chat attached when another control surface makes its
    /// session runnable again. A stopped actor gets one bounded handoff attempt
    /// on its own; a durable active record means replacement should keep being
    /// retried until the actor appears or another record retires the session.
    pub fn set_session_feed_expected(&mut self, expected: bool) {
        self.session_feed_expected = expected;
        if expected {
            // A session that is runnable again is no longer being retired.
            self.session_retiring = false;
            if !self.session_open {
                self.begin_session_reconnect();
            }
        }
    }

    /// Tells this chat that its session is being retired on purpose, so the
    /// feed closing is the expected outcome rather than a lost actor: the chat
    /// stays on screen as a transcript, attempts no handoff, and reports no
    /// reconnect failure. An expected feed clears it: see
    /// [`Self::set_session_feed_expected`].
    pub fn set_session_retiring(&mut self, retiring: bool) {
        self.session_retiring = retiring;
    }

    /// Whether this chat is treating a closed feed as a deliberate retirement.
    pub fn session_retiring(&self) -> bool {
        self.session_retiring
    }

    /// Takes the surface's newer view of the config and this session's record,
    /// so a chat that stays open across a config reload offers the profiles
    /// that are configured now rather than the ones that were configured when
    /// it opened.
    ///
    /// A record the daemon no longer publishes leaves the open-time copy in
    /// place: disappearing from the list is not a reason to lose the last
    /// known context. A chat opened without a context stays without one.
    ///
    /// `project` is the session whose project identity names the location, so
    /// a sub-agent child can be named after the parent whose checkout it works
    /// in. `None` uses the session's own record.
    pub fn refresh_context(
        &mut self,
        config: &Config,
        session: Option<&SessionRecord>,
        project: Option<&SessionRecord>,
    ) {
        let Some(context) = self.context.as_mut() else {
            return;
        };
        context.config = config.clone();
        if let Some(session) = session.filter(|session| session.id == context.session.id) {
            context.session = session.clone();
        }
        // The context and the session-list columns are two snapshots of the
        // same durable record. A warm chat keeps its ChatState, so refreshing
        // only the former leaves the pane title naming the pre-move target and
        // profile. Re-derive the canonical display identity from the refreshed
        // record while keeping all transcript and composer state in place.
        let target = project
            .unwrap_or(&context.session)
            .project_target(config, &context.session.target_template_id);
        let profile = context.session.last_profile.clone();
        let title = context.session.display_title().to_owned();
        let harness_kind = context.session.harness_kind;
        self.state.set_header_summary(target, profile, title);
        self.state.set_harness_kind(harness_kind);
        self.state.set_review_config(config.review.clone());
        self.state.set_spinner_style(config.spinner);
        self.state
            .set_detailed_activity_clocks(config.advanced.detailed_activity_clocks);
        self.refresh_voice_availability();
    }

    /// Let a focused surface supply a readable title for unnamed conversations.
    pub fn set_display_title(&mut self, title: String) {
        self.state.set_header_summary(
            self.state.header_target.clone(),
            self.state.header_profile.clone(),
            title,
        );
    }

    pub fn latest_event_ordinal(&self) -> u64 {
        self.state.latest_seq()
    }
}

/// A live chat is its [`ChatState`] plus the session plumbing around it, so
/// every read and edit of the conversation reaches the state directly instead
/// of through a wrapper per method. Methods that consult the session handle or
/// the feed flags stay inherent on `ActiveChat` and take precedence.
impl std::ops::Deref for ActiveChat {
    type Target = ChatState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
impl std::ops::DerefMut for ActiveChat {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl Drop for ActiveChat {
    fn drop(&mut self) {
        self.cancel_dictation();
        // A review outlives this view: the daemon owns it, so closing the
        // terminal leaves it running and resolvable from the phone.
    }
}

enum VoiceUpdate {
    Availability(
        Vec<std::path::PathBuf>,
        anyhow::Result<Option<std::path::PathBuf>>,
    ),
    Status(String),
    Finished(anyhow::Result<String>),
}

fn spawn_dictation(
    auth_path: std::path::PathBuf,
    updates: tokio::sync::mpsc::UnboundedSender<VoiceUpdate>,
    cancel: std::sync::mpsc::Receiver<crate::speech::VoiceCommand>,
) {
    tokio::spawn(async move {
        let worker_updates = updates.clone();
        let result = tokio::task::spawn_blocking(move || {
            let updates = worker_updates;
            let status_updates = updates.clone();
            crate::speech::run_dictation(
                &auth_path,
                |_| {},
                |_| {},
                move |status| {
                    if let Err(error) = status_updates.send(VoiceUpdate::Status(status)) {
                        tracing::debug!(%error, "voice status dropped because the chat closed");
                    }
                },
                cancel,
            )
        })
        .await
        .unwrap_or_else(|error| Err(anyhow::anyhow!("dictation task failed: {error}")));
        if let Err(error) = updates.send(VoiceUpdate::Finished(result)) {
            tracing::debug!(%error, "voice result dropped because the chat closed");
        }
    });
}

fn append_dictation(prefix: &str, transcript: &str) -> String {
    match (prefix.trim_end(), transcript.trim()) {
        ("", transcript) => transcript.to_owned(),
        (prefix, "") => prefix.to_owned(),
        (prefix, transcript) => format!("{prefix} {transcript}"),
    }
}

#[cfg(test)]
mod tests;
