//! The live conversation: the background feeds behind an open session, and the
//! transcript and composer the combined surface asks it to draw into the
//! regions it has chosen.

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
use crate::components::{EventResult, Outcome, render_scrollbar, scrollbar_geometry};
use crate::hel_selection::{FrameSurfaces, SelectionRange, SurfaceFrame, SurfaceId};
use hel::hel_config::HelConfig;
use hel::hel_database::{HistoryScope, PromptHistoryEntry};
use hel::hel_state::{MaterializedSession, SessionRecord, TranscriptItem, config_command_text};
use hel::hel_transcript::ChatEntry;
use hel::hel_worker::WorkerPhase;
use mj_client::session::{
    ManagedSessionView, ReviewerAction, ReviewerOutcome, SessionControl as SessionManagerControl,
    SessionHandle as ManagedSessionHandle, ViewError, new_command_id,
};

use super::attachments;
use super::autocomplete::render_autocomplete;
use super::elicitation::render_elicitation_in;
use super::history::{highlighted_input_lines, history_scope_name, history_search_footer};
use super::input::{input_cursor_visual_position, input_visual_rows, set_input_cursor};
use super::remote::{
    ChatRemoteOperation, ChatRemoteResult, ChatRemoteSupervisor, apply_chat_remote_result,
    queue_chat_remote_operation, restore_unsent_input, restore_unsent_prompt,
};
use super::rendering::{
    VOICE_BUTTON_GLYPH, display_width, truncate_line_to_width, truncate_to_width,
    voice_button_area, voice_button_line, wrap_styled_line,
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
use crate::hel_clipboard::{ClipboardContent, ClipboardImage};

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
        review: hel::hel_database::StoredReview,
    },
    ClearReview {
        session_id: String,
    },
    RememberReviewerSelection {
        workspace_id: String,
        selection: hel::hel_second_opinion::ReviewerSelection,
    },
    /// Review the turn this session just finished.
    StartTurnReview {
        session_id: String,
    },
    /// Forward the findings, dismiss them, or cancel the review.
    ResolveTurnReview {
        session_id: String,
        resolution: hel::hel_review::driver::Resolution,
    },
}
use agent_client_protocol::schema::v1::SessionConfigOption;
use hel::hel_second_opinion::{
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
        result: std::result::Result<Vec<hel::hel_worker::RelayEvent>, String>,
    },
    /// A page of one reviewing role's own relay events.
    TurnReviewEvents {
        role: String,
        result: std::result::Result<Vec<hel::hel_worker::RelayEvent>, String>,
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
    chat: &mut ChatState,
    updates: &tokio::sync::mpsc::UnboundedSender<ChatIoUpdate>,
) {
    let Some(request) = chat.take_history_search_request() else {
        return;
    };
    let generation = request.generation;
    let updates = updates.clone();
    tokio::spawn(async move {
        let result = match tokio::task::spawn_blocking(move || {
            ChatState::resolve_history_search_request(request)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(format!("history search task failed: {error}")),
        };
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
                state.mark_visible_changed();
            }
            tracing::warn!(error = format!("{error:#}"), "chat session view failed");
            state.set_notice(format!("connection lost: {error:#}"));
            return false;
        }
    };
    // A transient connection error does not make an as-yet-unavailable
    // transcript empty. The actor keeps retrying, so retain the loading row
    // until a real projection arrives; the error still appears in the notice.
    let activity_reachable = view.connected && view.snapshot.is_some() && view.error.is_none();
    if state.activity_reachable != activity_reachable {
        state.activity_reachable = activity_reachable;
        state.mark_visible_changed();
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
            state.mark_visible_changed();
        }
        state.set_session_activity(crate::usage_format::SessionActivity::of(
            &snapshot.operational,
        ));
    }
    if let Some(error) = view.error {
        if state.activity_reachable {
            state.activity_reachable = false;
            state.mark_visible_changed();
        }
        match error {
            ViewError::Unreachable(detail) => {
                tracing::warn!(%detail, "chat session became unreachable");
                state.set_notice(format!("connection lost: {detail}"))
            }
            ViewError::TargetMissing(detail) => {
                tracing::warn!(%detail, "chat session target is missing");
                state.set_notice(format!("managed target lost: {detail}"))
            }
            ViewError::ProjectionIntegrity(detail) => {
                tracing::error!(%detail, "chat transcript projection failed");
                state.set_notice(format!("transcript projection failed: {detail}"))
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
    stored_review: std::result::Result<Option<hel::hel_database::StoredReview>, String>,
    reviewer: ReviewerPane,
}

impl PreparedChat {
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

impl ActiveChat {
    /// Builds the view from the session's current snapshot and starts its
    /// background feeds. This convenience constructor reads stored review
    /// state. Interactive hosts must prepare it off-thread with
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
        let mut stored_review = hel::hel_database::active_review(session.session_id())
            .map_err(|error| format!("{error:#}"));
        let mut reviewer = ReviewerPane::default();
        if let Ok(Some(stored)) = &mut stored_review {
            reviewer.restore(
                &reviewer_session_id(session.session_id()),
                std::mem::take(&mut stored.reviewer_transcript),
            );
        }
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
                state.set_session_activity(crate::usage_format::SessionActivity::of(
                    &snapshot.operational,
                ));
            }
            let pending = PendingPrefix::of(materialized, state.unconverted_prefix());
            (state, pending)
        };
        state.set_history_context(&bundle_id);
        state.set_header_summary(header.target, header.profile, header.title);
        state.restore_draft(draft);
        state.notices = notices;
        let (chat_io_tx, chat_io_rx) = tokio::sync::mpsc::unbounded_channel::<ChatIoUpdate>();
        {
            let updates = chat_io_tx.clone();
            let session_id = session.session_id().to_owned();
            let bundle_id = bundle_id.to_owned();
            tokio::spawn(async move {
                let result = match tokio::task::spawn_blocking(move || {
                    hel::hel_database::search_prompts(
                        &session_id,
                        &bundle_id,
                        HistoryScope::Project,
                        "",
                    )
                    .map_err(|error| format!("{error:#}"))
                })
                .await
                {
                    Ok(result) => result,
                    Err(error) => Err(format!("history prefetch task failed: {error}")),
                };
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
            state.set_notice("Connecting to session relay…");
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
                request: hel::hel_acp::normalized_plan_review(
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
                    hel::hel_second_opinion::ReviewerDefaults::default(),
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
    pub fn refresh_context(&mut self, config: &HelConfig, session: Option<&SessionRecord>) {
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
        let target = context
            .session
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

    /// The composer's current draft. Image-bearing drafts use a versioned
    /// envelope so detach and session switching preserve the embedded bytes.
    pub fn draft(&self) -> String {
        self.state.encoded_draft()
    }

    pub fn latest_event_ordinal(&self) -> u64 {
        self.state.latest_seq()
    }

    /// Whether visible background activity needs animation frames.
    pub fn needs_animation(&self) -> bool {
        self.state.needs_animation()
    }

    /// Whether the transcript or task clocks differ from the last drawn frame.
    pub fn clock_changed(&self) -> bool {
        self.state.clock_changed()
    }

    /// An animation tick changes the activity spinner while work is visible.
    pub fn animation_changed(&self) -> bool {
        self.state.animation_changed()
    }

    /// Consumes a visible mutation performed by an external host callback,
    /// such as review projection or refreshed session context.
    pub fn take_render_changed(&mut self) -> bool {
        self.state.take_render_changed()
    }

    /// Records the time-dependent cells represented by the frame just drawn.
    /// The next clock or animation tick can then request a redraw only after
    /// its displayed value actually moves.
    pub fn acknowledge_render(&mut self) {
        self.state.acknowledge_render();
    }

    /// Waits for the next background message, applies it, and drains whatever
    /// queued behind it, reporting whether their visible state changed.
    ///
    /// `None` means no chat is warm, and the feed never wakes the caller. Cancel
    /// safe: every arm is a cancel-safe receive, and a message is applied only
    /// once its arm has won.
    pub async fn pump(chat: Option<&mut Self>) -> Outcome {
        let Some(chat) = chat else {
            return std::future::pending().await;
        };
        let before = chat.state.visible_revision();
        enum Wakeup {
            Remote(Option<ChatRemoteResult>),
            Io(ChatIoUpdate),
            Voice(VoiceUpdate),
            // Boxed: a view carries the whole session snapshot, and the enum
            // is built on every wakeup.
            View(Box<Result<ManagedSessionView>>),
        }
        // The senders for the I/O and voice feeds live in this struct, so those
        // receivers cannot report a closed channel and need no retirement flag.
        let wakeup = tokio::select! {
            result = chat.remote.recv(), if chat.remote_open => Wakeup::Remote(result),
            Some(update) = chat.chat_io_rx.recv() => Wakeup::Io(update),
            Some(update) = chat.voice_updates_rx.recv() => Wakeup::Voice(update),
            view = chat.session.changed(), if chat.session_open => Wakeup::View(Box::new(view)),
        };
        match wakeup {
            Wakeup::Remote(Some(result)) => chat.apply_remote_result(result),
            Wakeup::Remote(None) => chat.remote_open = false,
            Wakeup::Io(update) => chat.apply_io_update(update),
            Wakeup::Voice(update) => chat.apply_voice_update(update),
            Wakeup::View(view) => chat.apply_session_view(*view),
        }
        chat.drain().await;
        chat.report_worker_death().await;
        let changed = chat.state.visible_revision() != before;
        if changed {
            Outcome::Changed
        } else {
            Outcome::Unchanged
        }
    }

    async fn drain(&mut self) {
        self.refresh_voice_availability();
        while let Ok(result) = self.remote.try_recv() {
            self.apply_remote_result(result);
        }
        while let Ok(update) = self.chat_io_rx.try_recv() {
            self.apply_io_update(update);
        }
        while let Ok(update) = self.voice_updates_rx.try_recv() {
            self.apply_voice_update(update);
        }
        while self.session_open && self.session.has_changed().unwrap_or(false) {
            let view = self.session.changed().await;
            self.apply_session_view(view);
        }
        self.advance_review();
    }

    /// Moves a review on when the planner has answered the context request.
    ///
    /// The answer is the planner's next agent message after the request went
    /// out, so a message already in the transcript can never be mistaken for
    /// it, and a reconnect that replays the same completion starts no second
    /// reviewer turn.
    fn advance_review(&mut self) {
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion() else {
            return;
        };
        let context_baseline = review.context_baseline;
        let hel::hel_second_opinion::ReviewStage::GatheringContext { command_id } =
            review.workflow.stage()
        else {
            return;
        };
        if self.state.phase != WorkerPhase::Idle {
            return;
        }
        let command_id = command_id.clone();
        let Some(summary) = self.state.latest_agent_text_after(context_baseline) else {
            return;
        };
        let reviewer_command_id = self.state.next_second_opinion_command_id("review");
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion_mut() else {
            return;
        };
        let Some(request) =
            review
                .workflow
                .primary_context_completed(&command_id, summary, reviewer_command_id)
        else {
            return;
        };
        if let Some(view) = self.state.second_opinion_mut()
            && view.set_status("the reviewer is reading the plan…")
        {
            self.state.mark_visible_changed();
        }
        self.persist_review();
        self.run_workflow_request(request);
    }

    /// Shows whatever review the daemon is running for this session.
    ///
    /// The terminal hosts no part of a review: it renders this view, reads the
    /// reviewing roles' journals to show their transcripts, and sends
    /// resolutions back. A review therefore survives this view closing.
    pub fn apply_review_view(&mut self, view: Option<mj_client::review::RuntimeReviewView>) {
        let roles = view
            .as_ref()
            .map(|view| {
                view.roles
                    .iter()
                    .map(|role| role.role.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.state.set_turn_review(view);
        if self.state.turn_review().is_none() {
            self.reviewed_roles.clear();
            // The daemon's review projection is also the authority for
            // reviewer elicitations. Reconcile here so a form disappears as
            // soon as an external answer closes the review, even when no
            // reviewer journal event arrives afterward.
            self.surface_reviewer_elicitations();
            return;
        }
        // One reader per role, started the first time the daemon names it.
        for role in roles {
            if self.reviewed_roles.insert(role.clone()) {
                self.poll_turn_review_role(&role);
            }
        }
        self.surface_reviewer_elicitations();
    }

    /// Mirrors `[review]` into the view, from the config the dashboard drains.
    pub fn set_review_config(&mut self, review: hel::hel_config::ReviewConfig) {
        self.state.set_review_config(review);
    }

    /// Asks the daemon to review the turn that just finished.
    fn request_turn_review(&mut self) {
        let session_id = self.session.session_id().to_owned();
        self.state.set_notice("Starting a review…");
        self.send_daemon_request(ChatDaemonRequest::StartTurnReview { session_id });
    }

    /// Sends one resolution to the daemon, which owns the review.
    fn run_turn_review(&mut self, intent: crate::hel_chat::TurnReviewRequest) {
        use crate::hel_chat::TurnReviewRequest;

        let session_id = self.session.session_id().to_owned();
        match intent {
            TurnReviewRequest::Start => self.request_turn_review(),
            TurnReviewRequest::Resolve(resolution) => {
                self.send_daemon_request(ChatDaemonRequest::ResolveTurnReview {
                    session_id,
                    resolution,
                });
            }
        }
    }

    /// The configured profiles a reviewer can run under. The waterfall offers
    /// the same list plan review offers, so a workspace's remembered reviewer
    /// serves both.
    fn reviewer_profiles(&self) -> Vec<ReviewerProfileChoice> {
        let Some(context) = self.context.as_ref() else {
            return Vec::new();
        };
        context
            .config
            .enabled_profiles()
            .map(|(id, profile)| ReviewerProfileChoice {
                id: id.to_owned(),
                harness: profile.kind.id().to_owned(),
            })
            .collect()
    }

    /// Hands one request to the daemon bridge, or reports that this chat has
    /// none: a chat without a bridge cannot reach the daemon at all, and
    /// silently dropping a review action would leave the pane sitting there.
    fn send_daemon_request(&mut self, request: ChatDaemonRequest) {
        let Some(persistence) = &self.persistence else {
            self.state
                .set_notice("This chat cannot reach the Mjolnir daemon");
            return;
        };
        if let Err(error) = persistence.send(request) {
            tracing::warn!(%error, "a review request could not be queued for the daemon");
            self.state.set_notice("The Mjolnir daemon is not reachable");
        }
    }

    /// Reports a daemon refusal in the review pane, or as a notice when no
    /// review is open -- a refused `/review` has nowhere else to appear.
    pub fn report_review_refusal(&mut self, message: String) {
        match self.state.turn_review_mut() {
            Some(review) => {
                if review.report_failure(message) {
                    self.state.mark_visible_changed();
                }
            }
            None => self.state.set_notice(message),
        }
    }

    /// Reports a background worker that stopped on its own. Cheap enough to
    /// check on every wakeup: it only joins a handle that already finished.
    async fn report_worker_death(&mut self) {
        let Some(result) = self.remote.take_finished().await else {
            return;
        };
        if let Err(error) = result {
            if self.state.fail_all_background_stops() {
                self.state
                    .set_notice(format!("Background task could not be stopped: {error}"));
            } else {
                self.state
                    .set_notice(format!("Chat background worker failed: {error}"));
            }
        } else {
            self.state
                .set_notice("Chat background worker stopped unexpectedly");
        }
    }

    fn apply_io_update(&mut self, update: ChatIoUpdate) {
        let update = match update {
            ChatIoUpdate::SessionReconnected(result) => {
                self.finish_session_reconnect(result);
                return;
            }
            ChatIoUpdate::ReviewerProbe { generation, result } => {
                self.apply_reviewer_options(generation, result, false);
                return;
            }
            ChatIoUpdate::ReviewerConfigured { generation, result } => {
                self.apply_reviewer_options(generation, result, true);
                return;
            }
            ChatIoUpdate::ReviewerStarted(result) => {
                if let Err(error) = result {
                    if let Some(view) = self.state.second_opinion_mut() {
                        view.report_failure(error);
                    } else {
                        self.report_review_refusal(error);
                    }
                }
                return;
            }
            ChatIoUpdate::ReviewerEvents { result } => {
                self.apply_reviewer_events(result);
                return;
            }
            ChatIoUpdate::TurnReviewEvents { role, result } => {
                self.apply_turn_review_role_events(role, result);
                return;
            }
            ChatIoUpdate::Clipboard { generation, result } => {
                self.paste_in_flight = false;
                if generation != self.state.input_generation() {
                    self.state
                        .set_notice("Clipboard result discarded because the draft changed");
                    return;
                }
                match result {
                    Ok(ClipboardContent::Image(image)) => {
                        self.queue_attachment(AttachmentSource::Clipboard(image), None);
                    }
                    Ok(content) => self.state.handle_clipboard_content(content),
                    Err(error) => {
                        tracing::warn!(%error, "clipboard read failed and was shown in the UI");
                        self.state.set_notice(format!("Paste failed: {error}"));
                    }
                }
                return;
            }
            ChatIoUpdate::AttachmentFinished(result) => {
                self.apply_attachment_result(result);
                self.pump_attachment_queue();
                return;
            }
            update => update,
        };
        if matches!(&update, ChatIoUpdate::ToolDiffstats { .. }) {
            self.diffstats_in_flight = self.diffstats_in_flight.saturating_sub(1);
        }
        if let PrefixRebuild::Needed { attempt } = apply_chat_io_update(&mut self.state, update) {
            self.rebuild_transcript_prefix(attempt);
        }
        dispatch_history_search_request(&mut self.state, &self.chat_io_tx);
        dispatch_diffstat_requests(
            &mut self.state,
            &self.chat_io_tx,
            &mut self.diffstats_in_flight,
        );
    }

    fn queue_attachment(&mut self, source: AttachmentSource, command: Option<String>) {
        let sequence = self.next_attachment_sequence;
        if !self.state.reserve_attachment(sequence) {
            return;
        }
        self.next_attachment_sequence = self.next_attachment_sequence.wrapping_add(1);
        self.attachment_queue.push_back((sequence, source, command));
        self.pump_attachment_queue();
    }

    fn pump_attachment_queue(&mut self) {
        while self.attachment_tasks_in_flight < MAX_ATTACHMENT_TASKS {
            let Some((sequence, source, command)) = self.attachment_queue.pop_front() else {
                break;
            };
            self.attachment_tasks_in_flight += 1;
            let session_id = self.session.session_id().to_owned();
            let updates = self.chat_io_tx.clone();
            tokio::spawn(async move {
                let result = match tokio::task::spawn_blocking(move || match source {
                    AttachmentSource::Clipboard(image) => {
                        attachments::install_clipboard_image(&session_id, image)
                    }
                    AttachmentSource::Path(path) => attachments::install_path(&session_id, &path),
                })
                .await
                {
                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                    Err(error) => Err(format!("attachment task failed: {error}")),
                };
                if let Err(error) =
                    updates.send(ChatIoUpdate::AttachmentFinished(AttachmentResult {
                        sequence,
                        command,
                        result,
                    }))
                {
                    tracing::debug!(%error, "attachment result dropped because the chat closed");
                }
            });
        }
    }

    fn apply_attachment_result(&mut self, result: AttachmentResult) {
        self.attachment_tasks_in_flight = self.attachment_tasks_in_flight.saturating_sub(1);
        self.state
            .finish_attachment(result.sequence, result.result, result.command);
    }

    /// Restarts the history conversion against the session's current snapshot,
    /// after the transcript changed under the last one. Only the spawn happens
    /// here; the conversion itself stays off the event loop.
    fn rebuild_transcript_prefix(&mut self, attempt: u32) {
        let view = self.session.view();
        let Some(snapshot) = view.snapshot else {
            return;
        };
        let Some(pending) =
            PendingPrefix::of(&snapshot.materialized, self.state.unconverted_prefix())
        else {
            return;
        };
        spawn_transcript_prefix(pending, attempt, self.chat_io_tx.clone());
    }

    fn refresh_voice_availability(&mut self) {
        let Some(context) = &self.context else {
            return;
        };
        let paths = mj_client::auth::auth_paths(&context.config, &context.session.last_profile);
        if paths != self.voice_probe_paths {
            self.voice_probe_paths.clone_from(&paths);
            self.voice_auth = None;
            self.state.set_voice_available(false);
            self.voice_probe_at = None;
        }
        if self.voice_probe_pending
            || self
                .voice_probe_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return;
        }
        self.voice_probe_pending = true;
        self.voice_probe_at = Some(std::time::Instant::now());
        let updates = self.voice_updates_tx.clone();
        tokio::spawn(async move {
            let probed_paths = paths.clone();
            let result = tokio::task::spawn_blocking(move || {
                if crate::speech::voice_input_supported() {
                    mj_client::auth::available_auth(paths)
                } else {
                    None
                }
            })
            .await;
            let result = result
                .map_err(|error| anyhow::anyhow!("dictation availability task failed: {error}"));
            if let Err(error) = updates.send(VoiceUpdate::Availability(probed_paths, result)) {
                tracing::debug!(%error, "dictation availability dropped because the chat closed");
            }
        });
    }

    fn apply_voice_update(&mut self, update: VoiceUpdate) {
        match update {
            VoiceUpdate::Availability(paths, result) => {
                self.voice_probe_pending = false;
                if paths != self.voice_probe_paths {
                    return;
                }
                match result {
                    Ok(path) => {
                        self.state.set_voice_available(path.is_some());
                        self.voice_auth = path;
                    }
                    Err(error) => self.state.set_notice(error.to_string()),
                }
            }
            VoiceUpdate::Status(status) => self.state.set_notice(status),
            VoiceUpdate::Finished(result) => {
                if self.state.voice_active {
                    self.state.voice_active = false;
                    self.state.mark_visible_changed();
                }
                self.voice_cancel = None;
                self.voice_finishing = false;
                match result {
                    Ok(text) => {
                        if !text.trim().is_empty() {
                            self.state
                                .set_input(append_dictation(&self.state.input, &text));
                        }
                        self.state.clear_notice();
                    }
                    Err(error) => self
                        .state
                        .set_notice(crate::speech::dictation_error_message(&error)),
                }
            }
        }
    }

    fn apply_session_view(&mut self, view: Result<ManagedSessionView>) {
        if self.session_retiring
            && let Err(error) = &view
        {
            // The feed closing is the expected end of a deliberate stop or
            // destroy, so it is neither a lost connection nor a reason to
            // chase a replacement actor. The transcript stays readable.
            tracing::debug!(
                error = format!("{error:#}"),
                session_id = %self.session.session_id(),
                "session feed closed because the session is being retired"
            );
            self.session_open = false;
            return;
        }
        self.session_open = apply_session_view(&mut self.state, view);
        self.apply_deferred_elicitation_draft();
        self.surface_reviewer_elicitations();
        if !self.session_open && !self.session_retiring {
            self.begin_session_reconnect();
        }
        dispatch_diffstat_requests(
            &mut self.state,
            &self.chat_io_tx,
            &mut self.diffstats_in_flight,
        );
    }

    fn apply_remote_result(&mut self, result: ChatRemoteResult) {
        let sync_succeeded = matches!(&result, ChatRemoteResult::Sync(Ok(())));
        let sync_finished = matches!(&result, ChatRemoteResult::Sync(_));
        apply_chat_remote_result(&mut self.state, result);
        if sync_finished {
            if sync_succeeded && self.reconnect_notice_pending_sync {
                self.state.set_notice("Reconnected to session relay");
            }
            self.reconnect_notice_pending_sync = false;
        }
    }

    fn begin_session_reconnect(&mut self) {
        if self.session_reconnect_in_flight {
            return;
        }
        self.session_reconnect_in_flight = true;
        let session_id = self.session.session_id().to_owned();
        let session_manager = self.session_manager.clone();
        let updates = self.chat_io_tx.clone();
        tokio::spawn(async move {
            let result = session_manager
                .wait_for_session(&session_id, SESSION_ACTOR_RECONNECT_WAIT)
                .await
                .map_err(|error| format!("{error:#}"));
            if let Err(error) = updates.send(ChatIoUpdate::SessionReconnected(result)) {
                tracing::debug!(
                    %error,
                    %session_id,
                    "session reconnect result dropped because the chat closed"
                );
            }
        });
    }

    fn finish_session_reconnect(
        &mut self,
        result: std::result::Result<ManagedSessionHandle, String>,
    ) {
        self.session_reconnect_in_flight = false;
        match result {
            Ok(session) => {
                let view = session.view();
                self.session = session;
                self.session_open = apply_session_view(&mut self.state, Ok(view));
                self.apply_deferred_elicitation_draft();
                if self.session_open {
                    self.reconnect_notice_pending_sync = true;
                    self.state.set_notice("Reconnected to session relay");
                } else {
                    self.begin_session_reconnect();
                }
            }
            Err(error) => {
                if self.session_retiring {
                    // The session was stopped or destroyed on purpose, so
                    // losing its actor is the expected outcome, not a failure.
                    tracing::debug!(
                        %error,
                        session_id = %self.session.session_id(),
                        "session relay handoff ended because the session is being retired"
                    );
                    return;
                }
                self.state
                    .set_notice(format!("Could not reconnect to session relay: {error}"));
                if self.session_feed_expected {
                    self.begin_session_reconnect();
                }
            }
        }
    }

    /// Applies one terminal event and reports what it asked for.
    pub fn handle_event(&mut self, event: Event) -> ChatEventOutcome {
        self.handle_event_result(event)
            .action
            .unwrap_or(ChatEventOutcome::None)
    }

    /// Applies one terminal event while preserving both dispatch and repaint
    /// information.  The action is intentionally separate from `Outcome`:
    /// editing the composer may need a redraw without asking the host to do
    /// anything, while a remote command can be consumed with no visual delta.
    pub fn handle_event_result(&mut self, event: Event) -> EventResult<ChatEventOutcome> {
        let before = self.state.visible_revision();
        let action = match &event {
            Event::Key(key) => self.state.handle_key(*key),
            Event::Paste(pasted) => self.state.handle_terminal_paste(pasted),
            Event::Mouse(mouse) => self.state.handle_mouse(*mouse),
            // Resize and focus changes are handled by the host's geometry.
            _ => ChatAction::None,
        };
        let consumed = self.state.event_consumed(&event, &action);
        let dispatched = self.dispatch(action);
        dispatch_history_search_request(&mut self.state, &self.chat_io_tx);
        let changed = self.state.visible_revision() != before;
        let action = (!matches!(dispatched, ChatEventOutcome::None)).then_some(dispatched);
        EventResult {
            outcome: if changed {
                Outcome::Changed
            } else if consumed || action.is_some() {
                Outcome::Unchanged
            } else {
                Outcome::Continue
            },
            action,
        }
    }

    /// A command ID for one remote operation. `None` means the system random
    /// source failed, which leaves the command unsent rather than closing the
    /// view the user is reading.
    fn command_id(&mut self, prefix: &str) -> Option<String> {
        match new_command_id(prefix) {
            Ok(command_id) => Some(command_id),
            Err(error) => {
                self.state
                    .set_notice(format!("Could not identify the command: {error:#}"));
                None
            }
        }
    }

    fn dispatch(&mut self, action: ChatAction) -> ChatEventOutcome {
        match action {
            ChatAction::None => return ChatEventOutcome::None,
            ChatAction::Prompt(text) => {
                let images = self.state.take_submitting_images();
                let Some(command_id) = self.command_id("prompt") else {
                    restore_unsent_prompt(&mut self.state, text, images);
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Prompt queued for delivery…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Prompt {
                        command_id,
                        text,
                        images,
                    },
                    &mut self.state,
                );
            }
            ChatAction::Attach { path, command } => {
                self.queue_attachment(AttachmentSource::Path(path), Some(command));
            }
            ChatAction::RunShell(command) => {
                let Some(command_id) = self.command_id("shell") else {
                    restore_unsent_input(&mut self.state, &format!("!{command}"));
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Shell command queued…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::RunShell {
                        command_id,
                        command,
                    },
                    &mut self.state,
                );
            }
            ChatAction::RemoveQueuedPrompt { id, text, kind } => {
                let Some(command_id) = self.command_id("remove-prompt") else {
                    self.state.fail_queued_prompt_removal(id, text, kind);
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Removing queued prompt…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::RemoveQueuedPrompt {
                        command_id,
                        id,
                        text,
                        kind,
                    },
                    &mut self.state,
                );
            }
            ChatAction::StopBackgroundTask { id } => {
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::StopBackgroundTask { id },
                    &mut self.state,
                );
            }
            ChatAction::SetConfig { key, value } => {
                let Some(command_id) = self.command_id("set-config") else {
                    restore_unsent_input(&mut self.state, &config_command_text(&key, &value));
                    return ChatEventOutcome::Handled;
                };
                self.state.set_notice("Sending configuration update…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::SetConfig {
                        command_id,
                        key,
                        value,
                    },
                    &mut self.state,
                );
            }
            ChatAction::PlanCommand {
                original,
                control,
                requested_active,
                prompt,
            } => {
                let Some(command_id) = self.command_id("plan-mode") else {
                    self.state.plan_command_pending = false;
                    self.state.finish_plan_mode_change(!requested_active);
                    restore_unsent_input(&mut self.state, &original);
                    return ChatEventOutcome::Handled;
                };
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::PlanCommand {
                        command_id,
                        original,
                        control,
                        requested_active,
                        prompt,
                    },
                    &mut self.state,
                );
            }
            ChatAction::Cancel => {
                let Some(command_id) = self.command_id("cancel") else {
                    return ChatEventOutcome::Handled;
                };
                let intent = self.state.turn_control_intent();
                self.state.set_notice(intent.sending_notice());
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Cancel {
                        command_id,
                        intent,
                        cancel_agent: self.state.prompt_in_flight()
                            || self.state.session_activity.capacity_retry.is_some(),
                        shell_command_ids: self.state.active_user_shell_ids(),
                    },
                    &mut self.state,
                );
            }
            ChatAction::StartSecondOpinion { request, proposal } => {
                self.open_second_opinion(request, proposal);
            }
            ChatAction::SecondOpinion(intent) => {
                self.run_second_opinion(intent);
            }
            ChatAction::StartTurnReview => self.request_turn_review(),
            ChatAction::TurnReview(intent) => self.run_turn_review(intent),
            ChatAction::RespondReviewerElicitation {
                role,
                elicitation_id,
                response,
            } => self.answer_reviewer(role, elicitation_id, response),
            ChatAction::RespondElicitation { request, response } => {
                let plan_followup = self.state.plan_review_followup(&request, &response);
                self.state.set_notice("Sending answer…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::RespondElicitation {
                        request,
                        response,
                        plan_followup,
                    },
                    &mut self.state,
                );
            }
            ChatAction::PasteFromClipboard => {
                if self.paste_in_flight {
                    self.state.set_notice("Clipboard read already in progress…");
                    return ChatEventOutcome::Handled;
                }
                self.paste_in_flight = true;
                self.state.set_notice("Reading clipboard…");
                let updates = self.chat_io_tx.clone();
                let text_only = self.state.clipboard_is_text_only();
                let generation = self.state.input_generation();
                tokio::spawn(async move {
                    let result = match tokio::task::spawn_blocking(move || {
                        if text_only {
                            crate::hel_clipboard::read_text()
                                .map(ClipboardContent::Text)
                                .map_err(|error| format!("{error:#}"))
                        } else {
                            crate::hel_clipboard::read().map_err(|error| format!("{error:#}"))
                        }
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(format!("clipboard task failed: {error}")),
                    };
                    if let Err(error) = updates.send(ChatIoUpdate::Clipboard { generation, result })
                    {
                        tracing::debug!(%error, "clipboard result dropped because the chat closed");
                    }
                });
            }
            ChatAction::ToggleVoice => {
                if let Some(cancel) = self.voice_cancel.as_ref() {
                    let (command, notice) = if self.voice_finishing {
                        (crate::speech::VoiceCommand::Cancel, "Cancelling dictation…")
                    } else {
                        self.voice_finishing = true;
                        (
                            crate::speech::VoiceCommand::Finish,
                            "Finishing dictation… click the microphone again to cancel",
                        )
                    };
                    if let Err(error) = cancel.send(command) {
                        self.state
                            .set_notice(format!("Dictation worker stopped: {error}"));
                    } else {
                        self.state.set_notice(notice);
                    }
                } else if let Some(auth_path) = self.voice_auth.clone() {
                    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
                    self.voice_cancel = Some(cancel_tx);
                    self.voice_finishing = false;
                    self.state.voice_active = true;
                    self.state.mark_visible_changed();
                    self.state.set_notice(
                        "Starting microphone… click again or press Alt-V to transcribe",
                    );
                    spawn_dictation(auth_path, self.voice_updates_tx.clone(), cancel_rx);
                }
            }
            // Moving the keyboard to another pane is not leaving the
            // conversation: it stays on screen, so nothing is detached and
            // dictation keeps running.
            ChatAction::CycleFocus { reverse } => {
                return ChatEventOutcome::CycleFocus { reverse };
            }
            ChatAction::QuitDetach => return self.detach(),
        }
        ChatEventOutcome::Handled
    }

    /// Leaves the conversation: stops any dictation and reports how far the
    /// transcript has been read, which the host turns into the session's read
    /// receipt and its saved draft.
    ///
    /// `Alt-Q` is a global chord, so the host catches it before the composer
    /// sees the key and calls this directly; `/detach` reaches it through
    /// [`ChatAction::QuitDetach`]. Both paths must do the same bookkeeping.
    pub fn detach(&mut self) -> ChatEventOutcome {
        self.cancel_dictation();
        ChatEventOutcome::QuitDetach {
            last_seen_event_ordinal: detach_chat(&mut self.state),
        }
    }

    /// Opens the reviewer waterfall for a captured plan.
    ///
    /// The harness's decision stays pending: it is answered only once a
    /// reviewer is running, because gathering context needs an idle planning
    /// session and cancelling before then must leave the decision intact.
    fn open_second_opinion(
        &mut self,
        request: hel::hel_elicitation::ElicitationRequest,
        proposal: String,
    ) {
        let Some(context) = self.context.as_ref() else {
            self.state
                .set_notice("A second opinion needs this session's configuration");
            self.state.restore_elicitation(request);
            return;
        };
        let profiles = self.reviewer_profiles();
        if profiles.is_empty() {
            self.state
                .set_notice("Configure a second profile to review plans with");
            self.state.restore_elicitation(request);
            return;
        }
        let defaults = hel::hel_database::reviewer_defaults().unwrap_or_else(|error| {
            tracing::debug!(%error, "could not read remembered reviewer choices");
            ReviewerDefaults::default()
        });
        let workspace_id = context.session.workspace_id.clone();
        // A workspace that has already chosen a reviewer does not choose
        // again: the same reviewer resumes with its own conversation. The
        // waterfall reopens only when starting it that way fails.
        let remembered = defaults
            .profile(&workspace_id)
            .filter(|id| context.config.enabled_profile(id).is_some())
            .map(|profile_id| ReviewerSelection {
                profile_id: profile_id.to_owned(),
                model: remembered_value(defaults.model(&workspace_id, profile_id)),
                effort: remembered_value(
                    defaults.effort(
                        &workspace_id,
                        profile_id,
                        defaults
                            .model(&workspace_id, profile_id)
                            .unwrap_or(hel::hel_second_opinion::HARNESS_DEFAULT_VALUE),
                    ),
                ),
            });
        let setup = ReviewerSetup::new(workspace_id, profiles, defaults);
        self.state
            .open_second_opinion(CapturedProposal { request, proposal }, setup);
        if let Some(selection) = remembered {
            if let Some(view) = self.state.second_opinion_mut()
                && view.set_status("resuming the reviewer…")
            {
                self.state.mark_visible_changed();
            }
            self.probe_reviewer(
                0,
                selection.profile_id.clone(),
                selection.model.clone(),
                selection.effort.clone(),
                false,
            );
            self.resuming_reviewer = Some(selection);
        }
    }

    /// Performs the steps the second-opinion view asked for.
    fn run_second_opinion(&mut self, intent: SecondOpinionIntent) {
        match intent {
            SecondOpinionIntent::Setup(requests) => {
                for request in requests {
                    self.run_setup_request(request);
                }
            }
            SecondOpinionIntent::Confirmed {
                profile_id,
                model,
                effort,
            } => self.confirm_reviewer(profile_id, model, effort),
            SecondOpinionIntent::Workflow(requests) => {
                // Every workflow batch that reaches here ends the review, so
                // the record goes before the steps run: a crash between them
                // must not restore a split whose feedback already went out.
                self.forget_review();
                for request in requests {
                    self.run_workflow_request(request);
                }
            }
            SecondOpinionIntent::Closed => {}
        }
    }

    fn run_setup_request(&mut self, request: SetupRequest) {
        match request {
            SetupRequest::Probe {
                generation,
                profile_id,
            } => self.probe_reviewer(generation, profile_id, None, None, false),
            SetupRequest::ApplyModel { generation, model } => {
                let Some(profile_id) = self.setup_profile_id() else {
                    return;
                };
                self.probe_reviewer(generation, profile_id, Some(model), None, true);
            }
            SetupRequest::CancelProbe { .. } => self.pause_reviewer(),
        }
    }

    /// Persists the open review so a UI restart can pick it back up.
    fn persist_review(&self) {
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion() else {
            return;
        };
        let stored = hel::hel_database::StoredReview {
            workflow: review.workflow.clone(),
            generation: self.reviewer_generation,
            context_baseline: review.context_baseline,
            native_lost: false,
            reviewer_transcript: review.reviewer.transcript(),
        };
        if let Some(persistence) = &self.persistence {
            if let Err(error) = persistence.send(ChatDaemonRequest::SaveReview {
                session_id: self.session.session_id().to_owned(),
                review: stored,
            }) {
                tracing::warn!(%error, "could not queue the open review for persistence");
            }
        } else if let Err(error) =
            hel::hel_database::save_active_review(self.session.session_id(), &stored)
        {
            tracing::debug!(error = %format!("{error:#}"), "could not record the open review");
        }
    }

    /// Forgets a review that has finished, so nothing is restored for it.
    fn forget_review(&self) {
        if let Some(persistence) = &self.persistence {
            if let Err(error) = persistence.send(ChatDaemonRequest::ClearReview {
                session_id: self.session.session_id().to_owned(),
            }) {
                tracing::warn!(%error, "could not queue the finished review for persistence");
            }
        } else if let Err(error) = hel::hel_database::clear_active_review(self.session.session_id())
        {
            tracing::debug!(error = %format!("{error:#}"), "could not clear the finished review");
        }
    }

    fn setup_profile_id(&self) -> Option<String> {
        let SecondOpinion::Setup { setup, .. } = self.state.second_opinion()? else {
            return None;
        };
        setup
            .profiles()
            .get(setup.profile_index())
            .map(|profile| profile.id.clone())
    }

    /// Stages a profile and starts (or reconfigures) the reviewer under it,
    /// reporting the options it advertises back to the waterfall.
    fn probe_reviewer(
        &mut self,
        generation: u64,
        profile_id: String,
        model: Option<String>,
        effort: Option<String>,
        configuring: bool,
    ) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let reviewer_stager = context.reviewer_stager.clone();
        let config = context.config.clone();
        let session_record = context.session.clone();
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        // The reviewer's lifetime generation is what decides whether the
        // running reviewer can be kept; `generation` here only says which
        // probe this answer belongs to.
        let lifetime = self.reviewer_generation;
        tokio::spawn(async move {
            let staged = tokio::task::spawn_blocking(move || {
                reviewer_stager.stage(config, session_record, profile_id, lifetime)
            })
            .await;
            let result = async {
                let mut config = match staged {
                    Ok(Ok(config)) => config,
                    Ok(Err(error)) => return Err(format!("{error:#}")),
                    Err(error) => return Err(format!("staging the reviewer stopped: {error}")),
                };
                config.model = model;
                config.effort = effort;
                match session
                    .reviewer(ReviewerAction::Start {
                        config: Box::new(config),
                    })
                    .await
                {
                    Ok(ReviewerOutcome::Started(started)) => Ok(started.config_options),
                    Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                    Err(error) => Err(format!("{error:#}")),
                }
            }
            .await;
            let update = if configuring {
                ChatIoUpdate::ReviewerConfigured { generation, result }
            } else {
                ChatIoUpdate::ReviewerProbe { generation, result }
            };
            if let Err(error) = updates.send(update) {
                tracing::debug!(%error, "reviewer result dropped because the chat closed");
            }
        });
    }

    /// Confirms the chosen reviewer: remember it, answer the harness's own
    /// plan decision, and ask the planner for the context the reviewer needs.
    fn confirm_reviewer(
        &mut self,
        profile_id: String,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(view) = self.state.second_opinion() else {
            return;
        };
        let captured = view.captured().clone();
        if let Some(context) = self.context.as_ref() {
            let selection = ReviewerSelection {
                profile_id,
                model,
                effort,
            };
            if let Some(persistence) = &self.persistence {
                if let Err(error) = persistence.send(ChatDaemonRequest::RememberReviewerSelection {
                    workspace_id: context.session.workspace_id.clone(),
                    selection,
                }) {
                    tracing::warn!(%error, "could not queue the reviewer choice for persistence");
                }
            } else if let Err(error) = hel::hel_database::remember_reviewer_selection(
                &context.session.workspace_id,
                &selection,
            ) {
                tracing::debug!(%error, "could not remember the reviewer choice");
            }
        }
        let command_id = self.state.next_second_opinion_command_id("context");
        let (workflow, request) =
            ReviewWorkflow::start(captured.id(), captured.proposal.clone(), command_id.clone());
        let baseline = self.state.latest_seq();
        if let Some(view) = self.state.second_opinion_mut() {
            view.begin_review(workflow, "asking the planner for context…", baseline);
        }
        // The harness's decision is answered only now. Declining keeps plan
        // mode active, which is what lets the planner answer a context
        // question instead of starting to implement.
        queue_chat_remote_operation(
            self.remote.operations(),
            ChatRemoteOperation::RespondElicitation {
                request: captured.request.clone(),
                response: hel::hel_acp::plan_review_keep_planning(),
                plan_followup: None,
            },
            &mut self.state,
        );
        self.persist_review();
        self.run_workflow_request(request);
        self.poll_reviewer_events();
    }

    fn run_workflow_request(&mut self, request: WorkflowRequest) {
        match request {
            WorkflowRequest::PromptPrimary { command_id, prompt } => {
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Prompt {
                        command_id,
                        text: prompt,
                        images: Vec::new(),
                    },
                    &mut self.state,
                );
            }
            WorkflowRequest::PromptReviewer { command_id, prompt } => {
                let session = self.session.clone();
                let updates = self.chat_io_tx.clone();
                tokio::spawn(async move {
                    let result = session
                        .reviewer(ReviewerAction::Submit {
                            command_id,
                            command: hel::hel_worker::RelayCommand::Prompt {
                                prompt: vec![
                                    agent_client_protocol::schema::v1::ContentBlock::Text(
                                        agent_client_protocol::schema::v1::TextContent::new(prompt),
                                    ),
                                ],
                            },
                        })
                        .await
                        .map(|_| ())
                        .map_err(|error| format!("{error:#}"));
                    if let Err(error) = updates.send(ChatIoUpdate::ReviewerStarted(result)) {
                        tracing::debug!(%error, "reviewer prompt result dropped");
                    }
                });
            }
            WorkflowRequest::PauseReviewer => self.pause_reviewer(),
            WorkflowRequest::RestoreDecision { proposal, .. } => {
                // Gathering context consumed the harness's own approval, so
                // only Hel can put this decision back in front of the user.
                let restored = hel::hel_acp::normalized_plan_review(
                    self.state.next_second_opinion_command_id("plan-review"),
                    &serde_json::json!({ "plan": proposal }),
                );
                self.state.restore_elicitation(restored);
            }
        }
    }

    fn pause_reviewer(&self) {
        let session = self.session.clone();
        tokio::spawn(async move {
            if let Err(error) = session.reviewer(ReviewerAction::Pause).await {
                tracing::debug!(error = %format!("{error:#}"), "pausing the reviewer failed");
            }
        });
    }

    /// Answers a form the reviewer's harness is waiting on.
    fn answer_reviewer(
        &mut self,
        role: Option<String>,
        elicitation_id: String,
        response: hel::hel_elicitation::ElicitationResponse,
    ) {
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        let answered_role = role.clone();
        tokio::spawn(async move {
            let result = session
                .reviewer_as(
                    role,
                    ReviewerAction::RespondElicitation {
                        elicitation_id,
                        response,
                    },
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            if let Err(error) = updates.send(ChatIoUpdate::ReviewerStarted(result)) {
                tracing::debug!(%error, "reviewer form answer result dropped");
            }
        });
        // The answer unblocks that harness's turn, so keep reading its journal.
        match answered_role {
            Some(role) => self.poll_turn_review_role(&role),
            None => self.poll_reviewer_events(),
        }
    }

    /// Puts a form the reviewer is waiting on in front of the user, or answers
    /// it for them when it is not theirs to answer.
    fn surface_reviewer_elicitations(&mut self) {
        let pending: Vec<(Option<String>, hel::hel_elicitation::ElicitationRequest)> =
            match (self.state.second_opinion(), self.state.turn_review()) {
                (Some(view), _) => view
                    .reviewer()
                    .map(|reviewer| {
                        reviewer
                            .pending_elicitations()
                            .iter()
                            .map(|request| (None, request.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
                (None, Some(review)) => review
                    .pending_elicitations()
                    .into_iter()
                    .map(|(role, request)| (Some(role), request))
                    .collect(),
                (None, None) => Vec::new(),
            };
        self.state.reconcile_reviewer_elicitation(&pending);
        for (role, request) in pending {
            // A reviewer's plan decision is the reviewer proposing work, not
            // the plan under review. It is never shown as the primary's
            // decision; the reviewer was asked to critique, not to implement,
            // so it is declined and its critique stands as the answer.
            if hel::hel_acp::is_plan_review_id(&request.id) {
                self.answer_reviewer(role, request.id, hel::hel_acp::plan_review_keep_planning());
                continue;
            }
            if self.state.reviewer_elicitation_open() {
                return;
            }
            if !self.state.show_review_role_elicitation(role, request) {
                return;
            }
        }
        self.apply_deferred_elicitation_draft();
    }

    /// Reads the reviewer's journal from where the pane left off.
    ///
    /// One sidecar serves both review views, so whichever is open supplies the
    /// cursor; they are mutually exclusive by construction.
    fn poll_reviewer_events(&self) {
        let Some(reviewer) = self
            .state
            .second_opinion()
            .and_then(SecondOpinion::reviewer)
        else {
            return;
        };
        let after_ordinal = reviewer.cursor_ordinal;
        let after_digest = if reviewer.cursor_digest.is_empty() {
            hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            reviewer.cursor_digest.clone()
        };
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        tokio::spawn(async move {
            let result = match session
                .reviewer(ReviewerAction::Attach {
                    after_ordinal,
                    after_digest,
                })
                .await
            {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                Err(error) => Err(format!("{error:#}")),
            };
            if let Err(error) = updates.send(ChatIoUpdate::ReviewerEvents { result }) {
                tracing::debug!(%error, "reviewer events dropped because the chat closed");
            }
        });
    }

    /// Reads one reviewing role's journal from where its pane left off.
    ///
    /// Each role has its own relay, so each is polled on its own cursor; a
    /// lane's transcript never arrives in the supervisor's pane.
    fn poll_turn_review_role(&self, role: &str) {
        self.poll_turn_review_role_after(role, Duration::ZERO);
    }

    /// The same, after `delay`.
    ///
    /// An attach answers at once even when the journal has not moved, so a
    /// loop that re-attaches on every empty page is a spin. A review runs
    /// several roles at once, so each idle role waits a beat before asking
    /// again; a page that did carry events is followed up immediately, which
    /// is what keeps a streaming answer smooth.
    fn poll_turn_review_role_after(&self, role: &str, delay: Duration) {
        let Some(review) = self.state.turn_review() else {
            return;
        };
        let (after_ordinal, cursor_digest) = review.cursor(role);
        let after_digest = if cursor_digest.is_empty() {
            hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            cursor_digest
        };
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        let role = role.to_owned();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = match session
                .reviewer_as(
                    Some(role.clone()),
                    ReviewerAction::Attach {
                        after_ordinal,
                        after_digest,
                    },
                )
                .await
            {
                Ok(ReviewerOutcome::Attached(attachment)) => Ok(attachment.events),
                Ok(other) => Err(format!("unexpected reviewer response {other:?}")),
                Err(error) => Err(format!("{error:#}")),
            };
            if let Err(error) = updates.send(ChatIoUpdate::TurnReviewEvents { role, result }) {
                tracing::debug!(%error, "review events dropped because the chat closed");
            }
        });
    }

    /// Reports what the reviewer advertises back to the waterfall.
    ///
    /// A result from a probe the user has moved past is dropped by the state
    /// machine, which also names the reviewer to stop, so a slow harness can
    /// never overwrite a newer selection.
    fn apply_reviewer_options(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<SessionConfigOption>, String>,
        configuring: bool,
    ) {
        // A resumed review never shows the waterfall: the choice was already
        // made, so a successful start goes straight to the review and only a
        // failure falls back to asking again.
        if let Some(selection) = self.resuming_reviewer.clone() {
            match result {
                Ok(_) => {
                    self.resuming_reviewer = None;
                    self.confirm_reviewer(selection.profile_id, selection.model, selection.effort);
                    return;
                }
                Err(error) => {
                    self.resuming_reviewer = None;
                    if let Some(view) = self.state.second_opinion_mut() {
                        view.report_failure(format!(
                            "the remembered reviewer could not start: {error}"
                        ));
                    }
                    return;
                }
            }
        }
        let Some(SecondOpinion::Setup { setup, .. }) = self.state.second_opinion_mut() else {
            return;
        };
        let stale = match result {
            Ok(options) if configuring => setup.model_applied(generation, &options),
            Ok(options) => setup.probe_succeeded(generation, &options),
            Err(error) => {
                setup.probe_failed(generation, error);
                None
            }
        };
        if let Some(request) = stale {
            self.run_setup_request(request);
        }
    }

    /// Folds one reviewing role's events into its pane, and keeps reading.
    ///
    /// Display only: the daemon reads the same journals to drive the review,
    /// and nothing here advances it. Two readers on one journal are safe --
    /// an attach is a read at a cursor, and only the host acknowledges.
    fn apply_turn_review_role_events(
        &mut self,
        role: String,
        result: std::result::Result<Vec<hel::hel_worker::RelayEvent>, String>,
    ) {
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                // A journal this terminal cannot read is a display problem,
                // not a review problem: the daemon is still running it.
                tracing::debug!(%role, %error, "could not read a reviewing role's journal");
                self.report_review_refusal(error);
                return;
            }
        };
        let idle = events.is_empty();
        let session_id = review_role_session_id(self.session.session_id(), &role);
        if let Some(review) = self.state.turn_review_mut()
            && !events.is_empty()
        {
            review.pane(&role).apply_events(&session_id, &events);
        }
        self.surface_reviewer_elicitations();
        if self.state.turn_review().is_none() {
            self.reviewed_roles.remove(&role);
            return;
        }
        if !self
            .state
            .turn_review()
            .is_some_and(|review| review.role_is_active(&role))
        {
            // A verdict retains role rows for navigation, but their harnesses
            // have already been paused and need no further journal attaches.
            self.reviewed_roles.remove(&role);
            return;
        }
        self.poll_turn_review_role_after(
            &role,
            if idle {
                REVIEW_POLL_IDLE_INTERVAL
            } else {
                Duration::ZERO
            },
        );
    }

    /// Folds a page of reviewer events into the pane and keeps reading.
    fn apply_reviewer_events(
        &mut self,
        result: std::result::Result<Vec<hel::hel_worker::RelayEvent>, String>,
    ) {
        let session_id = reviewer_session_id(self.session.session_id());
        let events = match result {
            Ok(events) => events,
            Err(error) => {
                let changed = self
                    .state
                    .second_opinion_mut()
                    .is_some_and(|view| view.report_failure(error));
                if changed {
                    self.state.mark_visible_changed();
                }
                return;
            }
        };
        let reviewer_changed = if !events.is_empty() {
            self.state
                .second_opinion_mut()
                .and_then(|view| view.reviewer_mut())
                .is_some_and(|reviewer| reviewer.apply_events(&session_id, &events))
        } else {
            false
        };
        if reviewer_changed {
            if let Some(SecondOpinion::Review(review)) = self.state.second_opinion_mut()
                && let Some(answer) = review.reviewer.latest_answer()
                && let hel::hel_second_opinion::ReviewStage::Reviewing { command_id } =
                    review.workflow.stage().clone()
            {
                review.workflow.reviewer_turn_completed(&command_id, answer);
            }
            self.state.mark_visible_changed();
        }
        let finished = self.state.second_opinion().is_some_and(
            |view| matches!(view, SecondOpinion::Review(review) if review.workflow.finished()),
        );
        let status_changed = if let Some(view) = self.state.second_opinion_mut()
            && view.reviewer().is_some_and(|reviewer| !reviewer.is_empty())
        {
            view.set_status("Enter to act · Tab to choose")
        } else {
            false
        };
        if status_changed {
            self.state.mark_visible_changed();
        }
        self.persist_review();
        self.surface_reviewer_elicitations();
        if !finished {
            self.poll_reviewer_events();
        }
    }

    /// Stops any dictation thread. The thread reports `Finished`, which clears
    /// the view's voice state, so this only asks it to stop.
    fn cancel_dictation(&mut self) {
        if let Some(cancel) = self.voice_cancel.take()
            && let Err(error) = cancel.send(crate::speech::VoiceCommand::Cancel)
        {
            tracing::debug!(%error, "dictation worker already stopped");
        }
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        self.state.frame_surfaces()
    }

    /// Keep a scrollbar gesture routed here even outside the chat pane.
    pub fn transcript_scrollbar_dragging(&self) -> bool {
        self.state.transcript_scrollbar_dragging()
    }

    /// Rows the composer wants at `width`: the wrapped input, up to three
    /// queued-prompt previews, and the block's own border rows.
    pub fn desired_prompt_height(&self, width: u16) -> u16 {
        let content_width = prompt_content_width(width);
        let input_rows =
            u16::try_from(input_visual_rows(&self.state.input, content_width)).unwrap_or(u16::MAX);
        let queued = u16::try_from(self.state.queued_prompts.len().min(3)).unwrap_or(3);
        input_rows.saturating_add(queued).saturating_add(2).max(4)
    }

    /// Draws the transcript and the composer into `regions`, for a host that
    /// owns the rest of the frame.
    ///
    /// `prompt_focused` says whether the composer owns the keyboard; only then
    /// does it draw a cursor and an accent border. `transcript_selected` says
    /// the selection engine still owns a selection on the transcript, so its
    /// row space has to stay frozen for this frame.
    pub fn draw_in(
        &mut self,
        frame: &mut Frame,
        regions: ChatRegions<'_>,
        prompt_focused: bool,
        transcript_selected: bool,
    ) {
        render_in(
            frame,
            &mut self.state,
            regions,
            prompt_focused,
            transcript_selected,
        );
    }

    /// Visible host footer commands, indexed through the supplied chords then functions.
    pub fn footer_command_areas(&self) -> Vec<(usize, Rect)> {
        self.state.footer_command_areas.borrow().clone()
    }

    /// Whether the last frame's surfaces stand alone, because a modal owned
    /// the frame.
    pub fn frame_surfaces_exclusive(&self) -> bool {
        self.state.frame_surfaces_exclusive()
    }

    /// Clears the screen geometry retained by chat components before a host
    /// redraw. Focus and an in-flight pointer gesture remain owned by chat.
    pub fn reset_component_geometry(&mut self) {
        self.state.reset_component_geometry();
    }

    /// Whether a chat component owns this pointer event before host selection.
    pub fn component_handles_mouse(&self, mouse: crossterm::event::MouseEvent) -> bool {
        self.state.component_handles_mouse(mouse)
    }

    /// Whether a chat modal currently owns the frame.
    pub fn component_modal_open(&self) -> bool {
        self.state.component_modal_open()
    }

    /// Releases any pointer gesture held by a chat component.
    pub fn cancel_component_pointer(&mut self) {
        self.state.cancel_component_pointer();
    }

    /// The transcript text a finished selection covers.
    pub fn transcript_selection_text(&mut self, range: &SelectionRange) -> Option<String> {
        self.state.transcript_selection_text(range)
    }

    /// The message text a selection in the elicitation pane covers.
    pub fn elicitation_selection_text(&self, range: &SelectionRange) -> Option<String> {
        self.state.elicitation_selection_text(range)
    }

    /// The text a selection in the reviewer pane covers. It is resolved
    /// against that pane's own rows, so a drag there can never pick up the
    /// primary transcript's text.
    pub fn reviewer_selection_text(&self, range: &SelectionRange) -> Option<String> {
        self.state.reviewer_selection_text(range)
    }

    /// Whether the transcript's selection row space stopped describing the
    /// rows on screen since the last call.
    pub fn transcript_selection_invalidated(&mut self) -> bool {
        self.state.transcript_selection_invalidated()
    }

    /// Scrolls the surface a drag is holding against one of its edges.
    /// `direction` is negative for up and positive for down.
    pub fn autoscroll_selection(&mut self, surface: SurfaceId, direction: i8) {
        let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(1);
        match surface {
            SurfaceId::Transcript if direction < 0 => {
                self.state.scroll_history_up(MOUSE_SCROLL_ROWS);
            }
            SurfaceId::Transcript => {
                self.state.scroll_history_down(MOUSE_SCROLL_ROWS);
            }
            SurfaceId::ElicitationMessage => self
                .state
                .scroll_elicitation_message(if direction < 0 { -rows } else { rows }),
            SurfaceId::ReviewerTranscript => {
                self.state
                    .scroll_second_opinion(if direction < 0 { -rows } else { rows });
            }
            _ => {}
        }
    }
}

impl Drop for ActiveChat {
    fn drop(&mut self) {
        self.cancel_dictation();
        // A review outlives this view: the daemon owns it, so closing the
        // terminal leaves it running and resolvable from the phone.
    }
}

/// Draws a chat across a whole frame, the way the combined surface lays it out
/// when nothing else is competing for the rows.
///
/// Only tests use this: the real surface owns the layout and calls
/// [`render_in`] with the bands it chose.
#[cfg(test)]
pub(super) fn render_full_frame(
    frame: &mut Frame,
    chat: &mut ChatState,
    transcript_selected: bool,
) {
    let inner = frame.area();
    let prompt_width = prompt_content_width(inner.width);
    let visible_queued = chat.queued_prompts.len().min(3) as u16;
    let input_rows = input_visual_rows(&chat.input, prompt_width) as u16;
    let prompt_height = input_rows
        .saturating_add(visible_queued)
        .saturating_add(2)
        .max(4)
        .min(inner.height.saturating_sub(6).max(3));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(prompt_height),
            Constraint::Length(1),
        ])
        .split(inner);
    render_in(
        frame,
        chat,
        ChatRegions {
            transcript: chunks[0],
            prompt: chunks[1],
            footer: Some(test_footer(chunks[2])),
            overlay: inner,
        },
        true,
        transcript_selected,
    );
}

/// Draws the transcript and the composer into `regions`.
///
/// `prompt_focused` says whether the composer owns the keyboard; only then
/// does it draw a cursor and an accent border. `transcript_selected` says the
/// selection engine still owns a selection on the transcript, so its row
/// space has to stay frozen for this frame.
pub(super) fn render_in(
    frame: &mut Frame,
    chat: &mut ChatState,
    regions: ChatRegions<'_>,
    prompt_focused: bool,
    transcript_selected: bool,
) {
    chat.footer_command_areas.borrow_mut().clear();
    chat.voice_form.begin_frame();
    if chat.component_modal_open() || chat.second_opinion_split() || chat.turn_review_split() {
        chat.voice_form.cancel_pointer();
    }
    chat.frame_surfaces.clear();
    chat.frame_surfaces_exclusive = false;
    // Modals and the completion popup are centred in the whole frame, not
    // in the band the transcript happens to have been given.
    let inner = regions.overlay;
    let mut transcript_area = regions.transcript;
    let prompt_area = regions.prompt;
    let prompt_width = prompt_content_width(prompt_area.width);

    // An open question replaces the composer, but only with the natural
    // height of its current page (up to half of the combined conversation
    // bands). The transcript is rendered into the rows left above it so its
    // viewport, scrollbar, and selection row space describe what is visible.
    // This branch deliberately runs before the ordinary prompt/split drawing:
    // no hidden prompt or autocomplete surface may survive underneath the
    // question, and both visible scrollable surfaces remain registered.
    if let Some(question_height) = chat.elicitation.as_ref().map(|dialog| {
        let combined = Rect::new(
            transcript_area.x,
            transcript_area.y,
            transcript_area.width,
            prompt_area.bottom().saturating_sub(transcript_area.y),
        );
        dialog
            .natural_height(combined.width)
            .min(combined.height / 2)
    }) {
        let combined = Rect::new(
            transcript_area.x,
            transcript_area.y,
            transcript_area.width,
            prompt_area.bottom().saturating_sub(transcript_area.y),
        );
        let transcript_height = combined.height.saturating_sub(question_height);
        let question_area = Rect::new(
            combined.x,
            combined.bottom().saturating_sub(question_height),
            combined.width,
            question_height,
        );
        let upper_transcript = Rect::new(combined.x, combined.y, combined.width, transcript_height);

        chat.voice_button_area = None;
        chat.task_control_area = None;
        chat.task_dialog_area = None;
        chat.reviewer_area = None;
        chat.split_action_areas.clear();
        chat.turn_review_action_areas.clear();
        chat.voice_form.cancel_pointer();
        chat.voice_form.end_frame(super::VoiceControl::Microphone);

        render_transcript(frame, upper_transcript, chat, transcript_selected);
        if question_height > 0
            && let Some(dialog) = chat.elicitation.as_ref()
        {
            render_elicitation_in(
                frame,
                dialog,
                &mut chat.frame_surfaces,
                question_area,
                prompt_focused,
            );
        }
        if let Some(footer) = regions.footer {
            render_chat_footer(frame, footer, chat, prompt_focused);
        }
        return;
    }

    let split = chat.second_opinion_split() || chat.turn_review_split();
    // Review panes replace the composer, so retain their existing activity
    // row. Normal conversations put the spinner in the composer border.
    let activity_area =
        (split && chat.needs_animation() && transcript_area.height > 3).then(|| {
            transcript_area.height -= 1;
            Rect::new(
                transcript_area.x,
                transcript_area.bottom(),
                transcript_area.width,
                1,
            )
        });
    // The button lives on the prompt border, so it is not part of
    // the selectable prompt interior. Clear the hitbox first because a split
    // view or modal may replace the composer for this frame.
    chat.voice_button_area = None;
    chat.task_control_area = None;
    chat.task_dialog_area = None;
    let (primary_area, reviewer_area) = if split {
        let halves = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(transcript_area);
        (halves[0], Some(halves[1]))
    } else {
        (transcript_area, None)
    };
    render_transcript(frame, primary_area, chat, transcript_selected);
    chat.reviewer_area = None;
    if let Some(area) = reviewer_area {
        if chat.turn_review_split() {
            if let Some(review) = chat.turn_review_mut() {
                let (inner, top, total) =
                    super::turn_review::render_turn_review_pane(frame, area, review);
                // Route the wheel to whichever review tab is selected.
                chat.reviewer_area = Some(area);
                chat.frame_surfaces.push(SurfaceFrame::scrollable(
                    SurfaceId::ReviewerTranscript,
                    inner,
                    top,
                    total,
                ));
            }
        } else {
            let status = match chat.second_opinion() {
                Some(SecondOpinion::Review(review)) => review.status.clone(),
                _ => String::new(),
            };
            if let Some(SecondOpinion::Review(review)) = chat.second_opinion_mut() {
                let (inner, top, total) =
                    render_reviewer(frame, area, &mut review.reviewer, &status);
                chat.reviewer_area = Some(inner);
                chat.frame_surfaces.push(SurfaceFrame::scrollable(
                    SurfaceId::ReviewerTranscript,
                    inner,
                    top,
                    total,
                ));
            }
        }
    }
    if chat.turn_review_split() {
        // The split has no composer: a review is synchronous, so the only
        // input while it is up is which of its actions to take.
        let status = chat
            .turn_review()
            .map(super::turn_review::TurnReview::status)
            .unwrap_or_default();
        let buttons = match chat.turn_review_mut() {
            Some(review) => {
                super::turn_review::render_turn_review_actions(frame, prompt_area, review, &status)
            }
            None => Vec::new(),
        };
        chat.turn_review_action_areas = buttons;
        chat.split_action_areas.clear();
    } else if let Some(SecondOpinion::Review(review)) = chat.second_opinion_mut() {
        // The split has no composer: the revised plan is the planner's to
        // write, so the only input here is which of the three actions to take.
        let buttons = render_split_actions(
            frame,
            prompt_area,
            &review.workflow,
            review.action,
            &review.status,
            &mut review.form,
        );
        chat.split_action_areas = buttons;
        chat.turn_review_action_areas.clear();
    } else {
        chat.split_action_areas.clear();
        chat.turn_review_action_areas.clear();
        let (prompt_title, activity_title) = prompt_title_line(chat, prompt_area.width);
        let mut prompt_block = theme::panel(prompt_focused)
            .padding(Padding::new(2, 1, 0, 0))
            .title(prompt_title);
        if let Some(activity_title) = activity_title {
            prompt_block = prompt_block.title(activity_title.right_aligned());
        }
        chat.task_control_area = None;
        let bottom_width = prompt_area.width.saturating_sub(2);
        let queue_control = prompt_bottom_queue_control(chat);
        let task_label = (chat.background_task_count() > 0)
            .then(|| format!(" View tasks ({}) ", chat.background_task_count()));
        let command_hints = (prompt_focused && prompt_area.width >= 56).then(|| {
            Line::from(vec![
                Span::styled(" Enter ", theme::selection(false)),
                Span::styled(" send  ", theme::muted()),
                Span::styled(" / ", theme::selection(false)),
                Span::styled(" commands ", theme::muted()),
            ])
            .right_aligned()
        });
        let queue_width = queue_control.as_ref().map_or(0, Line::width);
        let task_width = task_label.as_ref().map_or(0, |label| display_width(label));
        let command_width = command_hints.as_ref().map_or(0, Line::width);
        let task_separator_width = usize::from(queue_control.is_some() && task_label.is_some()) * 2;
        // Fit queue/control text first, then a complete task button, then hints.
        let left_with_task = queue_width + task_separator_width + task_width;
        let show_task = task_label.is_some() && left_with_task <= usize::from(bottom_width);
        let left_width = if show_task {
            left_with_task
        } else {
            queue_width
        };
        let show_command_hints = command_hints.is_some()
            && left_width + usize::from(left_width > 0) + command_width
                <= usize::from(bottom_width);
        let mut bottom_spans = Vec::new();
        let mut bottom_left_width = 0usize;
        if let Some(queue_control) = queue_control {
            bottom_left_width = queue_width;
            bottom_spans.extend(queue_control.spans);
        }
        if show_task {
            let task_start = bottom_left_width + task_separator_width;
            if bottom_left_width > 0 {
                bottom_spans.push(Span::raw(" ·"));
            }
            let task_label = task_label.expect("show_task implies a task label");
            let task_width = u16::try_from(task_width).expect("task label fits in u16");
            chat.task_control_area = Some(Rect::new(
                prompt_area
                    .x
                    .saturating_add(1)
                    .saturating_add(u16::try_from(task_start).unwrap_or(u16::MAX)),
                prompt_area.bottom().saturating_sub(1),
                task_width,
                1,
            ));
            bottom_spans.push(Span::styled(
                task_label,
                if chat.task_control_focused() {
                    theme::selection(false)
                } else {
                    theme::muted()
                },
            ));
        }
        if !bottom_spans.is_empty() {
            prompt_block = prompt_block.title_bottom(Line::from(bottom_spans).left_aligned());
        }
        if show_command_hints {
            prompt_block = prompt_block.title_bottom(command_hints.expect("presence checked"));
        }
        let prompt_inner = prompt_block.inner(prompt_area);
        chat.prompt_content_width = prompt_width;
        chat.voice_button_area = voice_button_area(prompt_area);
        let mut prompt_lines = chat
            .queued_prompts
            .iter()
            .rev()
            .take(3)
            .rev()
            .enumerate()
            .map(|(index, queued)| {
                Line::from(Span::styled(
                    truncate_to_width(
                        &format!(
                            "{} {}: {}",
                            queued.queue_label(),
                            index + 1,
                            queued_prompt_preview(&queued.text)
                        ),
                        usize::from(prompt_inner.width),
                    ),
                    Style::default().fg(theme::palette().muted),
                ))
            })
            .collect::<Vec<_>>();
        let queue_rows = prompt_lines.len();
        prompt_lines.extend(if let Some(search) = chat.history_search.as_ref() {
            highlighted_input_lines(&chat.input, &search.query)
        } else if chat.input.is_empty() {
            vec![Line::from(Span::styled(
                if chat.phase == WorkerPhase::Running {
                    "Add a follow-up while the agent works…"
                } else if chat.entries.is_empty()
                    && chat.unconverted_prefix == 0
                    && !chat.transcript_loading
                {
                    "What would you like to build?"
                } else {
                    ""
                },
                theme::muted(),
            ))]
        } else {
            chat.input
                .split('\n')
                .map(|line| Line::raw(line.to_owned()))
                .collect()
        });
        let cursor_row = input_cursor_visual_position(&chat.input, chat.input_cursor, prompt_width)
            .1
            + queue_rows;
        let content_height = usize::from(prompt_inner.height).max(1);
        let input_scroll = cursor_row.saturating_add(1).saturating_sub(content_height);
        frame.render_widget(
            Paragraph::new(prompt_lines)
                .style(Style::default().fg(theme::palette().text))
                .wrap(Wrap { trim: false })
                .scroll((input_scroll as u16, 0))
                .block(prompt_block),
            prompt_area,
        );
        if let Some(marker_row) = queue_rows.checked_sub(input_scroll)
            && marker_row < usize::from(prompt_inner.height)
        {
            frame.render_widget(
                Line::styled(">", theme::title(prompt_focused)),
                Rect::new(
                    prompt_inner.x.saturating_sub(2),
                    prompt_inner.y.saturating_add(marker_row as u16),
                    1,
                    1,
                ),
            );
        }
        if let Some(button_area) = chat.voice_button_area {
            chat.voice_form.register(
                super::VoiceControl::Microphone,
                ControlKind::Button,
                button_area,
                chat.voice_available || chat.voice_active,
            );
            frame.render_widget(
                voice_button_line(chat.voice_available, chat.voice_active),
                button_area,
            );
        }
        chat.voice_form.end_frame(super::VoiceControl::Microphone);
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::PromptInput, prompt_inner));
        // The cursor belongs to whatever has focus, so the composer only shows one
        // while the keyboard is driving it.
        if chat.history_search.is_none() && prompt_focused && chat.elicitation.is_none() {
            set_input_cursor(
                frame,
                prompt_inner,
                &chat.input,
                chat.input_cursor,
                queue_rows,
                input_scroll,
            );
        }
    }
    if let Some(area) = activity_area {
        let mut activity = if area.width >= 48 {
            chat.activity_spinner()
        } else {
            Line::from(crate::spinner::compact_span(
                chat.spinner_style,
                crate::spinner::elapsed_ms(),
            ))
        };
        activity.spans.insert(0, Span::raw("  "));
        let status = chat
            .turn_review()
            .and_then(|review| review.view.activity_label())
            .map_or_else(|| prompt_title(chat), |label| format!(" {label} "));
        activity.spans.push(Span::styled(status, theme::muted()));
        frame.render_widget(
            Paragraph::new(truncate_line_to_width(activity, usize::from(area.width)))
                .style(theme::base()),
            area,
        );
    }
    if let Some(footer) = regions.footer {
        render_chat_footer(frame, footer, chat, prompt_focused);
    }
    // The popup overlays the prompt and whatever sits above it, so it
    // registers last and wins the cells it covers.
    if let Some(popup) = render_autocomplete(frame, prompt_area, chat) {
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::AutocompletePopup, popup));
    }
    // A new elicitation can arrive while the task list is open.
    // Keep the task dialog state so it can reappear afterwards, but let the
    // question render and receive input on top of it.
    if chat.task_dialog_open() && chat.elicitation.is_none() {
        render_background_task_dialog(frame, inner, chat);
        return;
    }
    if let Some(body) = super::config_picker::render_config_picker(frame, inner, chat) {
        // The selector owns the frame's interaction, so the chat behind it
        // stops being selectable. An elicitation dialog still draws over it,
        // matching the key routing that lets the dialog win.
        chat.frame_surfaces.clear();
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::ModalBody, body));
    }
    if let Some(SecondOpinion::Setup {
        captured,
        setup,
        form,
    }) = chat.second_opinion_mut()
    {
        // The waterfall owns the frame's interaction, so the chat behind it
        // stops being selectable while a reviewer is being chosen.
        let area = crate::hel_modal::centered_modal_rect_fixed(frame, 60, 16, inner);
        let body = render_setup(
            frame,
            area,
            &format!(
                "Reviewing a {}-line plan",
                captured.proposal.lines().count()
            ),
            setup,
            form,
        );
        chat.frame_surfaces.clear();
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::ModalBody, body));
    }
}

/// Draws the one-row footer under the conversation: the reverse-i-search
/// prompt when one is open, else the shared notice, else the hotkey hints
/// for the composer.
pub(super) fn render_chat_footer(
    frame: &mut Frame,
    footer: ChatFooter<'_>,
    chat: &ChatState,
    prompt_focused: bool,
) {
    // The host only hands the footer to the chat while the composer has
    // focus, so `prompt_focused` is normally true here; the other arm keeps
    // the row honest if it ever is not.
    // The three groups are the dashboard's: what the composer answers, the
    // chords that answer from anywhere, then the function keys. Only the
    // first group changes with what the composer is doing.
    let footer_area = footer.area;
    let queued_keys = format!(
        "Up/Ctrl-P edit last queued · Enter send/queue · Ctrl-R history · Shift-Enter newline · {}",
        chat.turn_control_intent().escape_hint(),
    );
    let composer_keys = if !prompt_focused {
        "Tab pane · PgUp/PgDn transcript"
    } else if chat.voice_active {
        "Listening… Alt-V stop · PgUp/PgDn transcript"
    } else if !chat.queued_prompts.is_empty() {
        &queued_keys
    } else {
        "Tab pane · Ctrl-V paste · Enter send · Ctrl-R history · Alt-T rendering · Shift-Enter newline"
    };
    let groups = theme::fit_footer_items(
        [
            composer_keys
                .split(theme::FOOTER_SEPARATOR)
                .map(|text| (None, text))
                .collect(),
            footer
                .chords
                .iter()
                .enumerate()
                .map(|(index, text)| (Some(index), *text))
                .collect(),
            footer
                .functions
                .iter()
                .enumerate()
                .map(|(index, text)| (Some(footer.chords.len() + index), *text))
                .collect(),
        ],
        footer_area.width,
        |(_, text)| *text,
    );
    let default_footer = theme::footer_items_text(&groups, |(_, text)| *text);
    let search_footer = chat.history_search.as_ref().map(history_search_footer);
    let notice = chat.notices.current();
    let footer = search_footer
        .as_deref()
        .or(notice.as_deref())
        .unwrap_or(&default_footer);
    let mut command_areas = chat.footer_command_areas.borrow_mut();
    command_areas.clear();
    if search_footer.is_none() && notice.is_none() {
        let mut x = footer_area.x;
        for group in groups.iter().filter(|group| !group.is_empty()) {
            if x > footer_area.x {
                x += display_width(theme::FOOTER_GROUP_SEPARATOR) as u16;
            }
            for (index, (command, text)) in group.iter().enumerate() {
                if index > 0 {
                    x += display_width(theme::FOOTER_SEPARATOR) as u16;
                }
                let width = display_width(text) as u16;
                if let Some(command) = command {
                    command_areas.push((
                        *command,
                        Rect::new(x, footer_area.y, width, footer_area.height),
                    ));
                }
                x += width;
            }
        }
    }
    // Notices keep a warm accent; navigation hints remain quiet.
    let footer_color = if search_footer.is_none() && notice.is_some() {
        theme::palette().warning
    } else {
        theme::palette().muted
    };
    let line = if search_footer.is_none() && notice.is_none() {
        theme::hints(footer)
    } else {
        Line::raw(footer)
    };
    frame.render_widget(
        Paragraph::new(line).style(theme::base().fg(footer_color)),
        footer_area,
    );
    if let Some(search) = chat.history_search.as_ref()
        && chat.elicitation.is_none()
        && footer_area.width > 0
    {
        let prefix = format!("reverse-i-search [{}]: ", history_scope_name(search.scope));
        let column = display_width(&prefix) + display_width(&search.query);
        frame.set_cursor_position((
            footer_area.x + column.min(usize::from(footer_area.width.saturating_sub(1))) as u16,
            footer_area.y,
        ));
    }
}

#[cfg(test)]
fn test_footer(area: Rect) -> ChatFooter<'static> {
    ChatFooter {
        area,
        chords: &["Alt-G panes", "Alt-Q detach"],
        functions: &["F2 palette", "F4 web", "F5 refresh", "F7 setup", "F1 help"],
    }
}

/// A remembered configuration value, or `None` when it stands for the
/// harness's own default and nothing should be applied.
fn remembered_value(stored: Option<&str>) -> Option<String> {
    stored
        .filter(|value| *value != hel::hel_second_opinion::HARNESS_DEFAULT_VALUE)
        .map(str::to_owned)
}

fn prompt_title(chat: &ChatState) -> String {
    let title = prompt_title_parts(chat).join(" · ");
    if title.is_empty() {
        String::new()
    } else {
        format!(" {title} ")
    }
}

fn prompt_title_parts(chat: &ChatState) -> Vec<String> {
    let mut parts = [chat.current_model(), chat.current_effort()]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if chat.fast_mode_active() {
        parts.push("Fast".into());
    }
    if chat.plan_mode_active() {
        parts.push("PLAN MODE".into());
    } else {
        match chat.phase {
            WorkerPhase::Idle => {}
            WorkerPhase::Running if chat.pursuing_goal() => parts.push("Pursuing goal".into()),
            // The spinner already indicates an ordinary running turn.
            WorkerPhase::Running => {}
            WorkerPhase::Closing => parts.push("Closing".into()),
            WorkerPhase::Closed => parts.push("Closed".into()),
        }
    }
    // Auto-review changes what happens when this turn ends, so the composer
    // says it is armed rather than surprising the user with a pane.
    let review = chat.review_config();
    if review.enabled && review.reviewer_profile().is_some() {
        parts.push(format!("review {}", review.tier.label()));
    }
    parts
}

/// The microphone chip owns the prompt border's upper-left corner, followed by
/// model, effort, and state. Activity owns the upper-right corner. A full
/// configured spinner is used when both titles fit; the one-column frame keeps
/// narrow prompts readable.
fn prompt_title_line(
    chat: &ChatState,
    prompt_width: u16,
) -> (Line<'static>, Option<Line<'static>>) {
    let parts = prompt_title_parts(chat);
    let prefix_count =
        usize::from(chat.current_model().is_some()) + usize::from(chat.current_effort().is_some());
    let prefix = parts[..prefix_count.min(parts.len())].join(" · ");
    let suffix = parts[prefix_count.min(parts.len())..].join(" · ");
    let mut spans = vec![Span::raw(format!(" {VOICE_BUTTON_GLYPH} "))];
    if !prefix.is_empty() {
        spans.push(Span::raw(format!(" {prefix} ")));
    }
    if !suffix.is_empty() {
        spans.push(Span::raw(format!(" {suffix} ")));
    }
    let left_width = spans.iter().map(Span::width).sum::<usize>();
    let activity_title = chat.needs_animation().then(|| {
        let full = chat.activity_spinner();
        let max_title_width = usize::from(prompt_width.saturating_sub(2));
        let spinner =
            if left_width.saturating_add(full.width()).saturating_add(3) <= max_title_width {
                full
            } else {
                Line::from(crate::spinner::compact_span(
                    chat.spinner_style,
                    crate::spinner::elapsed_ms(),
                ))
            };
        let mut title = vec![Span::raw(" ")];
        title.extend(spinner.spans);
        title.push(Span::raw(" "));
        Line::from(title)
    });
    (Line::from(spans), activity_title)
}

/// Queue state and the hint for controlling the current turn live together
/// on the prompt's bottom border. This stays independent of background tasks,
/// so a queued prompt remains visible even when there is no task button.
fn prompt_bottom_queue_control(chat: &ChatState) -> Option<Line<'static>> {
    let mut labels = Vec::new();
    if !chat.queued_prompts.is_empty() {
        labels.push(format!("{} queued", chat.queued_prompts.len()));
    }
    if chat.prompt_in_flight() || chat.session_activity.capacity_retry.is_some() {
        labels.push(chat.turn_control_intent().escape_hint().to_owned());
    }
    (!labels.is_empty()).then(|| {
        Line::from(Span::styled(
            format!(" {}", labels.join(" · ")),
            theme::muted(),
        ))
    })
}

fn render_background_task_dialog(frame: &mut Frame, area: Rect, chat: &mut ChatState) {
    let commands = chat.session_activity().background_commands.clone();
    let height = u16::try_from(commands.len().saturating_add(4))
        .unwrap_or(u16::MAX)
        .max(1)
        .min(area.height.max(1));
    let width = area
        .width
        .saturating_sub(4)
        .clamp(32, 96)
        .min(area.width.max(1));
    let popup = crate::hel_modal::centered_modal_rect_fixed(frame, width, height, area);
    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    let now = hel::clock::epoch_seconds();
    let content_width = usize::from(inner.width.max(1));
    let mut stop_rows = Vec::new();
    let mut lines = Vec::new();
    if commands.is_empty() {
        lines.push(Line::from(Span::styled(
            "No background tasks remain.",
            theme::muted(),
        )));
    } else {
        for (index, command) in commands.iter().enumerate() {
            let started = u64::try_from(command.started_at_ms.max(0) / 1_000).unwrap_or_default();
            let elapsed = crate::usage_format::format_clock(now.saturating_sub(started));
            let prefix = format!("{elapsed:>8}  ");
            let prefix_width = display_width(&prefix);
            let label = if chat.background_stop_pending(&command.id) {
                "Stopping…"
            } else {
                "[Stop]"
            };
            let button_width = display_width(&format!("  {label}  "));
            // Keep the acknowledgement state visible even if the provider
            // revokes the affordance before its task-disappeared snapshot.
            let draw_button = (command.can_stop || chat.background_stop_pending(&command.id))
                && content_width >= button_width.saturating_add(prefix_width).saturating_add(2);
            let text_width = if draw_button {
                content_width
                    .saturating_sub(button_width.saturating_add(1))
                    .max(1)
            } else {
                content_width
            };
            let start = lines.len();
            lines.extend(wrap_styled_line(
                Line::from(format!(
                    "{prefix}{}",
                    super::rendering::sanitize_terminal_text(&command.command)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                )),
                text_width,
                prefix_width,
            ));
            if draw_button {
                stop_rows.push((
                    BackgroundTaskControl::Stop(index),
                    start,
                    label,
                    !chat.background_stop_pending(&command.id),
                    button_width,
                    command.id.clone(),
                ));
            }
        }
    }
    let visible = usize::from(inner.height).max(1);
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible);
    chat.task_dialog_max_scroll = max_scroll;
    chat.task_dialog_scroll = chat.task_dialog_scroll.min(max_scroll);
    if chat.task_dialog_scroll > 0 {
        lines = lines.into_iter().skip(chat.task_dialog_scroll).collect();
    }
    lines.truncate(visible);
    chat.task_dialog_form.begin_frame();
    let title = crate::hel_modal::dismissible_modal_title(
        &mut chat.task_dialog_form,
        popup,
        "Background tasks",
        theme::title(true),
        true,
    );
    let block = theme::panel(true)
        .title(title)
        .title_bottom(Line::from(Span::styled(" Esc close ", theme::muted())).right_aligned());
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::base().fg(theme::palette().text))
            .wrap(Wrap { trim: false })
            .block(block),
        popup,
    );
    chat.task_dialog_control_ids.clear();
    for (control, row, label, enabled, button_width, id) in stop_rows {
        if row < chat.task_dialog_scroll || row >= chat.task_dialog_scroll.saturating_add(visible) {
            continue;
        }
        chat.task_dialog_control_ids.push((control, id));
        let y = inner
            .y
            .saturating_add((row - chat.task_dialog_scroll) as u16);
        let button_area = Rect::new(
            inner
                .right()
                .saturating_sub(u16::try_from(button_width).unwrap_or(u16::MAX)),
            y,
            u16::try_from(button_width)
                .unwrap_or(u16::MAX)
                .min(inner.width),
            1,
        );
        Button::render(
            frame,
            button_area,
            label,
            enabled,
            &mut chat.task_dialog_form,
            control,
        );
    }
    if let Some(geometry) = scrollbar_geometry(
        Rect::new(inner.right(), inner.y, 1, inner.height),
        total_lines,
        chat.task_dialog_scroll,
        visible,
    ) {
        render_scrollbar(frame, geometry);
    }
    chat.task_dialog_area = Some(inner);
    chat.frame_surfaces_exclusive = true;
    chat.frame_surfaces.clear();
    chat.frame_surfaces
        .push(SurfaceFrame::fixed(SurfaceId::ModalBody, inner));
    chat.task_dialog_form
        .end_frame(BackgroundTaskControl::Stop(0));
}

/// The composer keeps a `>` gutter on the left and one cell of space on the
/// right.
fn prompt_content_width(width: u16) -> usize {
    usize::from(width.saturating_sub(5)).max(1)
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
mod tests {
    use super::*;

    use crate::hel_chat::test_support::{
        agent_message_item, agent_transcript_item, drawn_transcript, fast_mode_option, queued,
        snapshot,
    };
    use agent_client_protocol::schema::v1::{SessionConfigOption, SessionConfigOptionCategory};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use hel::hel_elicitation::{ElicitationField, ElicitationFieldKind, ElicitationRequest};
    use hel::hel_transcript::ChatRole;
    use hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST;
    use hel::hel_worker::{SequencedEvent, WorkerEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn handle_event_result_reports_only_real_chat_repaints() {
        let fixture =
            mj_client::session::replacement_session_test_fixture("session-event-result", 72);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        chat.acknowledge_render();

        let moved = chat.handle_event_result(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        assert_ne!(moved.outcome, Outcome::Changed);
        assert!(!chat.take_render_changed());

        let unchanged = chat.handle_event_result(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(unchanged.outcome, Outcome::Unchanged);

        let ignored = chat.handle_event_result(Event::Key(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::NONE,
        )));
        assert_eq!(ignored.outcome, Outcome::Continue);

        let changed = chat.handle_event_result(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert_eq!(changed.outcome, Outcome::Changed);
        assert_eq!(chat.draft(), "x");

        let cursor_changed =
            chat.handle_event_result(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        assert_eq!(cursor_changed.outcome, Outcome::Changed);

        let clamped =
            chat.handle_event_result(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        assert_eq!(clamped.outcome, Outcome::Unchanged);
    }

    #[test]
    fn a_disconnected_view_without_a_snapshot_stops_stale_animation() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.turn_started_at_epoch_seconds = Some(1);
        assert!(chat.needs_animation());
        apply_session_view(
            &mut chat,
            Ok(ManagedSessionView {
                snapshot: None,
                connected: false,
                error: None,
            }),
        );
        assert!(!chat.needs_animation());
    }

    /// Captures the real conversation renderer for visual review. The caller
    /// chooses an artifact path; ordinary test runs never write screenshots.
    #[test]
    #[ignore = "writes a terminal-cell capture to MJ_CHAT_CAPTURE_PATH"]
    fn capture_chat_preview() {
        let path = std::env::var_os("MJ_CHAT_CAPTURE_PATH")
            .expect("set MJ_CHAT_CAPTURE_PATH to the preview JSON path");
        let dimension = |name, fallback| {
            std::env::var_os(name)
                .map(|value| {
                    value
                        .to_str()
                        .expect("capture dimensions must be Unicode")
                        .parse::<u16>()
                        .expect("capture dimensions must be unsigned integers")
                })
                .unwrap_or(fallback)
        };
        let columns = dimension("MJ_CHAT_CAPTURE_COLUMNS", 110);
        let rows = dimension("MJ_CHAT_CAPTURE_ROWS", 40);
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_header_summary("local / mjolnir", "Claude · Sonnet", "");
        chat.mark_prompt_submitted("Make the terminal feel beautifully crafted.");
        chat.turn_started_at_epoch_seconds = Some(hel::clock::epoch_seconds().saturating_sub(42));
        chat.set_current_step_start(Some(hel::clock::epoch_millis().saturating_sub(7_000)));
        chat.set_session_activity(crate::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            execution: Some(hel::hel_worker::RelayExecutionState::Running),
            ..Default::default()
        });
        let tool = |seq, title: &str, summary: &str, status| {
            let mut entry = ChatEntry::tool(seq, title, None, status);
            entry.tool_summary = Some(summary.to_owned());
            entry
        };
        chat.entries = vec![
            ChatEntry::plain(
                1,
                ChatRole::User,
                "Make the terminal feel beautifully crafted. Keep it fast, readable, and calm.",
            ),
            ChatEntry::plain(
                2,
                ChatRole::Agent,
                "I’m bringing the interface together around a midnight palette, clear hierarchy, and the original animated activity indicators.\n\n### A little more room to think\n\n- Focus follows a soft teal border\n- **Your conversation stays readable** while tools work\n- Code and keyboard shortcuts have their own quiet surfaces",
            ),
            tool(
                3,
                "cd dir && python x.py | cat | wc ; print ok",
                "cd && python | cat | wc ; print",
                hel::hel_transcript::ToolStatus::Completed,
            ),
            tool(
                4,
                "cargo test -p brokk-mj-chat",
                "cargo test",
                hel::hel_transcript::ToolStatus::Completed,
            ),
            ChatEntry::plain(
                5,
                ChatRole::Agent,
                "The shared theme is in place. Here’s the panel style used throughout the app:\n\n```rust\nlet panel = theme::panel(focused)\n    .title(\" Conversation \");\n```\n\nI’m checking the narrow layouts and selection behavior now.",
            ),
            tool(
                6,
                "cargo clippy --all-targets -- -D warnings",
                "cargo clippy",
                hel::hel_transcript::ToolStatus::Running,
            ),
        ];
        // Keep the capture representative of the in-place tool expansion:
        // the first completed call opens to its provider title and splits the
        // surrounding completed streak.
        chat.expanded_tool_calls.insert(3);
        let mut terminal = Terminal::new(TestBackend::new(columns, rows)).expect("terminal");
        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("render preview");
        let buffer = terminal.backend().buffer();
        let color = |color, fallback| match color {
            ratatui::style::Color::Rgb(r, g, b) => [r, g, b],
            _ => fallback,
        };
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| {
                        let cell = &buffer[(x, y)];
                        serde_json::json!({
                            "text": cell.symbol(),
                            "fg": color(cell.fg, [223, 235, 244]),
                            "bg": color(cell.bg, [11, 18, 32]),
                            "bold": cell.modifier.contains(ratatui::style::Modifier::BOLD),
                            "italic": cell.modifier.contains(ratatui::style::Modifier::ITALIC),
                            "underline": cell.modifier.contains(ratatui::style::Modifier::UNDERLINED),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let capture = serde_json::json!({ "width": buffer.area.width, "height": buffer.area.height, "rows": rows });
        std::fs::write(path, serde_json::to_vec(&capture).expect("encode preview"))
            .expect("write preview");
    }

    #[tokio::test]
    async fn replacement_chat_preserves_the_latest_same_session_draft_even_when_cleared() {
        fn prepare(id: &str, draft: &str) -> PreparedChat {
            let fixture = mj_client::session::replacement_session_test_fixture(id, 89);
            ActiveChat::prepare_with_persistence(
                fixture.stopped,
                "bundle-1",
                None,
                fixture.control,
                SessionHeaderIdentity::default(),
                draft.into(),
                Notices::default(),
                None,
            )
        }
        let mut previous = prepare("moving", "saved before preparation").open();
        let pending = prepare("moving", "stale saved draft");
        previous.state.set_input("edited during preparation".into());
        let mut replacement = pending.open_replacing(Some(&previous));
        assert_eq!(replacement.draft(), "edited during preparation");

        replacement.state.clear_input();
        let replacement =
            prepare("moving", "stale draft must not return").open_replacing(Some(&replacement));
        assert!(replacement.draft().is_empty());
        let different = prepare("other", "other session draft").open_replacing(Some(&previous));
        assert_eq!(different.draft(), "other session draft");
    }

    fn managed_view(session: MaterializedSession) -> ManagedSessionView {
        let session_id = session.session_id.clone();
        let latest_ordinal = session.applied_event_ordinal;
        let latest_digest = session.applied_event_digest.clone();
        ManagedSessionView {
            snapshot: Some(hel::hel_state::ManagedSessionSnapshot {
                window: hel::hel_state::ProjectionWindow::of(&session),
                materialized: session,
                latest_credential_sync_signal: None,
                worker_build: None,
                operational: hel::hel_worker::RelayOperationalState {
                    goal: Default::default(),
                    capacity_retry: None,
                    activity_turn_started_at_ms: None,
                    store_id: None,
                    idle_since_ms: None,
                    session_id,
                    execution: hel::hel_worker::RelayExecutionState::Idle,
                    latest_ordinal,
                    latest_digest: latest_digest.clone(),
                    acknowledged_through: latest_ordinal,
                    acknowledged_digest: latest_digest,
                    recovery_floor_ordinal: 0,
                    recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
                    native_session_id: None,
                    checkpoint_only: false,
                    acp_ready: None,
                    agent_capabilities: None,
                    agent_info: None,
                    steering_supported: None,
                    config_options: Vec::new(),
                    modes: None,
                    available_commands: Vec::new(),
                    config: BTreeMap::new(),
                    active_prompt: None,
                    queued_prompts: Vec::new(),
                    active_user_shells: Vec::new(),
                    active_agent_terminals: Vec::new(),
                    checkpoint_barrier: None,
                    checkpoint_ready: None,
                    last_acp_activity_at_ms: None,
                    current_step_started_at_ms: None,
                    foreground_tool_started_at_ms: None,
                    harness_turn: None,
                    last_harness_turn_started_ordinal: None,
                    background_commands: Vec::new(),
                    background_work_known: None,
                },
            }),
            connected: true,
            error: None,
        }
    }

    #[test]
    fn build_prompt_only_appears_without_session_history() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Idle;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        let mut rendered = |chat: &mut ChatState| {
            terminal
                .draw(|frame| render_full_frame(frame, chat, false))
                .expect("draw chat");
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        assert!(rendered(&mut chat).contains("What would you like to build?"));
        chat.transcript_loading = true;
        assert!(!rendered(&mut chat).contains("What would you like to build?"));
        chat.transcript_loading = false;
        chat.unconverted_prefix = 1;
        assert!(!rendered(&mut chat).contains("What would you like to build?"));
        chat.unconverted_prefix = 0;
        chat.entries
            .push(ChatEntry::plain(1, ChatRole::User, "Hello"));
        assert!(!rendered(&mut chat).contains("What would you like to build?"));
        chat.phase = WorkerPhase::Running;
        assert!(rendered(&mut chat).contains("Add a follow-up while the agent works…"));
    }

    #[test]
    fn dictation_appends_to_existing_prompt_cleanly() {
        assert_eq!(append_dictation("please", "fix this"), "please fix this");
        assert_eq!(append_dictation("", "fix this"), "fix this");
        assert_eq!(append_dictation("please ", ""), "please");
    }

    #[test]
    fn detaching_leaves_the_unsent_input_where_the_dashboard_saves_it_from() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("half typed thought".into());

        // Detaching keeps the composer intact: the warm chat goes on holding
        // it, and the surface reads it here to write it to the session row.
        detach_chat(&mut chat);
        assert_eq!(chat.input, "half typed thought");

        detach_chat(&mut chat);
        assert_eq!(chat.input, "half typed thought");
    }

    #[test]
    fn detaching_an_empty_composer_leaves_an_empty_draft_to_save() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        detach_chat(&mut chat);

        assert_eq!(chat.input, "");
    }

    #[test]
    fn an_elicitation_is_bounded_to_the_chat_content_area() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.entries.push(ChatEntry::plain(
            1,
            ChatRole::Agent,
            "UNDERLYING CHAT SENTINEL",
        ));
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            ElicitationRequest {
                id: "question-1".into(),
                message: "Visible dialog message".into(),
                title: Some("Overlaid dialog".into()),
                description: None,
                fields: Vec::new(),
            },
        ));
        chat.task_dialog_open = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");

        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("draw elicitation");
        let buffer = terminal.backend().buffer();
        let lines = (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let row_of = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle} in {lines:#?}"))
        };

        let popup_top = row_of("Overlaid dialog");
        row_of("Visible dialog message");
        assert!(!lines.iter().any(|line| line.contains("Background tasks")));
        // The reduced transcript remains visible above the bottom-anchored
        // question, rather than being hidden behind it.
        let sentinel_top = row_of("UNDERLYING CHAT SENTINEL");
        assert!(sentinel_top < popup_top);
        // The footer remains outside the question even when width fitting
        // drops composer hints to make room for the host's function keys.
        assert_eq!(row_of("F1 help"), lines.len() - 1);
        assert!(row_of("F1 help") > popup_top);
    }

    #[test]
    fn drawing_the_chat_registers_the_transcript_and_prompt_interiors() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        drawn_transcript(&mut chat, 80, 24);

        let surfaces = chat.frame_surfaces();
        let transcript = surfaces
            .surface(SurfaceId::Transcript)
            .expect("transcript registered");
        let prompt = surfaces
            .surface(SurfaceId::PromptInput)
            .expect("prompt registered");
        // The registered rect is the text inside each border, which is what
        // the wheel already hit-tests against.
        assert_eq!(
            surfaces
                .surface_at(prompt.rect.x, prompt.rect.y)
                .map(|surface| surface.id),
            Some(SurfaceId::PromptInput)
        );
        assert!(
            transcript.rect.bottom() <= prompt.rect.y,
            "the transcript sits above the composer: {transcript:?} {prompt:?}"
        );
    }

    #[test]
    fn the_autocomplete_popup_takes_the_cells_it_covers() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("/".into());

        drawn_transcript(&mut chat, 80, 24);

        let surfaces = chat.frame_surfaces();
        let popup = surfaces
            .surface(SurfaceId::AutocompletePopup)
            .expect("popup registered");
        assert_eq!(
            surfaces
                .surface_at(popup.rect.x, popup.rect.bottom() - 1)
                .map(|surface| surface.id),
            Some(SurfaceId::AutocompletePopup)
        );
    }

    #[test]
    fn an_open_elicitation_keeps_the_upper_transcript_and_hides_the_prompt() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            ElicitationRequest {
                id: "question-1".into(),
                message: "Visible dialog message".into(),
                title: Some("Overlaid dialog".into()),
                description: None,
                fields: Vec::new(),
            },
        ));

        drawn_transcript(&mut chat, 80, 24);

        // The question owns the lower half, while the reduced transcript
        // remains independently selectable and scrollable above it.
        let surfaces = chat.frame_surfaces();
        let transcript = surfaces
            .surface(SurfaceId::Transcript)
            .expect("reduced transcript registered");
        let message = surfaces
            .surface(SurfaceId::ElicitationMessage)
            .expect("message pane registered");
        assert!(surfaces.surface(SurfaceId::ModalBody).is_some());
        assert!(surfaces.surface(SurfaceId::PromptInput).is_none());
        assert!(transcript.rect.bottom() <= message.rect.y);
        assert_eq!(
            surfaces
                .surface_at(message.rect.x, message.rect.y)
                .map(|surface| surface.id),
            Some(SurfaceId::ElicitationMessage)
        );
    }

    #[test]
    fn short_question_stays_natural_and_leaves_more_than_half_to_transcript() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            ElicitationRequest {
                id: "short-question".into(),
                message: "One short question".into(),
                title: Some("Custom title".into()),
                description: None,
                fields: Vec::new(),
            },
        ));
        let natural = chat.elicitation.as_ref().unwrap().natural_height(80);

        drawn_transcript(&mut chat, 80, 24);

        let transcript = chat
            .frame_surfaces()
            .surface(SurfaceId::Transcript)
            .expect("transcript registered");
        let message = chat
            .frame_surfaces()
            .surface(SurfaceId::ElicitationMessage)
            .expect("question message registered");
        let question_height = 23 - transcript.rect.bottom() - 1;
        assert_eq!(question_height, natural);
        assert!(question_height < 23 / 2);
        assert!(transcript.rect.height > message.rect.height);
    }

    #[test]
    fn tall_question_is_capped_at_half_and_keeps_both_surfaces_non_overlapping() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            ElicitationRequest {
                id: "tall-question".into(),
                message: "Question message".into(),
                title: Some("Tall question".into()),
                description: Some("This field description is intentionally long enough to wrap into many rows when it is shown in the focused form.".into()),
                fields: vec![ElicitationField {
                    id: "answer".into(),
                    title: "Answer".into(),
                    description: Some("The focused answer description also consumes wrapped rows. ".repeat(20)),
                    required: false,
                    secret: false,
                    custom_answer_for: None,
                    custom_answer_option: None,
                    kind: ElicitationFieldKind::Text {
                        default: None,
                        min_length: None,
                        max_length: None,
                        pattern: None,
                        format: None,
                    },
                }],
            },
        ));
        let natural = chat.elicitation.as_ref().unwrap().natural_height(80);
        assert!(natural > 23 / 2);

        drawn_transcript(&mut chat, 80, 24);

        let surfaces = chat.frame_surfaces();
        let transcript = surfaces
            .surface(SurfaceId::Transcript)
            .expect("transcript registered");
        let message = surfaces
            .surface(SurfaceId::ElicitationMessage)
            .expect("question message registered");
        let question_height = 23 - transcript.rect.bottom() - 1;
        assert_eq!(question_height, 23 / 2);
        assert!(transcript.rect.bottom() <= message.rect.y);
        assert!(surfaces.surface(SurfaceId::PromptInput).is_none());
    }

    #[test]
    fn transcript_wheel_scrolls_the_reduced_viewport_independently() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.entries = (0..40)
            .map(|index| {
                ChatEntry::plain(index, ChatRole::Agent, format!("transcript row {index}"))
            })
            .collect();
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            ElicitationRequest {
                id: "scroll-question".into(),
                message: "Answer this".into(),
                title: None,
                description: None,
                fields: Vec::new(),
            },
        ));
        chat.task_dialog_open = true;
        drawn_transcript(&mut chat, 80, 24);
        let transcript = chat
            .frame_surfaces()
            .surface(SurfaceId::Transcript)
            .expect("transcript registered")
            .rect;
        let before = chat.anchor;

        assert_eq!(
            chat.handle_mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: transcript.x + 1,
                row: transcript.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            ChatAction::None
        );
        assert_ne!(chat.anchor, before);
        assert!(chat.elicitation.is_some());

        let scrollbar_x = chat
            .frame_surfaces()
            .surface(SurfaceId::Transcript)
            .expect("transcript registered")
            .rect
            .right();
        assert_eq!(
            chat.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: scrollbar_x,
                row: transcript.y + transcript.height / 2,
                modifiers: KeyModifiers::NONE,
            }),
            ChatAction::None
        );
        assert!(chat.transcript_scrollbar_dragging());
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: scrollbar_x,
            row: transcript.y + transcript.height / 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!chat.transcript_scrollbar_dragging());
    }

    #[test]
    fn question_pointer_capture_wins_when_dragged_into_transcript() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.entries = (0..40)
            .map(|index| ChatEntry::plain(index, ChatRole::Agent, "transcript"))
            .collect();
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            ElicitationRequest {
                id: "captured-question".into(),
                message: "Choose whether to continue".into(),
                title: None,
                description: None,
                fields: vec![ElicitationField {
                    id: "continue".into(),
                    title: "Continue".into(),
                    description: None,
                    required: false,
                    secret: false,
                    custom_answer_for: None,
                    custom_answer_option: None,
                    kind: ElicitationFieldKind::Boolean {
                        default: Some(false),
                    },
                }],
            },
        ));
        drawn_transcript(&mut chat, 80, 24);
        let form = chat
            .frame_surfaces()
            .surface(SurfaceId::ModalBody)
            .expect("question form registered")
            .rect;
        let press = MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: form.x + 1,
            row: form.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(chat.component_handles_mouse(press));
        chat.handle_mouse(press);
        let transcript = chat
            .frame_surfaces()
            .surface(SurfaceId::Transcript)
            .expect("transcript registered")
            .rect;
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: transcript.x + 1,
            row: transcript.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        assert!(chat.component_handles_mouse(drag));
        let before = chat.anchor;
        chat.handle_mouse(drag);
        assert_eq!(chat.anchor, before);
        chat.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(crossterm::event::MouseButton::Left),
            ..drag
        });
    }

    #[test]
    fn an_off_screen_chat_follows_the_session_view() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_transcript_loading(true);
        let mut session = MaterializedSession::empty("session-warm");
        session.applied_event_ordinal = 5;
        session.applied_event_digest = "a".repeat(64);
        session.transcript = vec![agent_transcript_item("first", 5)];

        let mut first_view = managed_view(session.clone());
        first_view
            .snapshot
            .as_mut()
            .unwrap()
            .operational
            .current_step_started_at_ms = Some(12_345);
        assert!(apply_session_view(&mut chat, Ok(first_view)));
        assert_eq!(chat.latest_seq(), 5);
        assert_eq!(chat.entries.len(), 1);
        assert_eq!(chat.current_step_started_at_ms, Some(12_345));
        assert!(!chat.transcript_loading);

        session.applied_event_ordinal = 8;
        session.transcript.push(agent_transcript_item("second", 8));
        assert!(apply_session_view(&mut chat, Ok(managed_view(session))));
        assert_eq!(chat.latest_seq(), 8);
        assert_eq!(chat.entries.len(), 2);
        assert_eq!(chat.current_step_started_at_ms, None);
    }

    #[test]
    fn a_transient_error_before_the_first_snapshot_keeps_the_loading_row() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_transcript_loading(true);
        let view = ManagedSessionView {
            snapshot: None,
            connected: false,
            error: Some(ViewError::Unreachable("database is busy".into())),
        };

        assert!(apply_session_view(&mut chat, Ok(view)));
        assert!(chat.transcript_loading);
        assert_eq!(
            chat.notice().as_deref(),
            Some("connection lost: database is busy")
        );
        assert_eq!(
            super::super::test_support::transcript_text(&mut chat, 80),
            ["Loading…"]
        );
    }

    #[test]
    fn a_stopped_session_manager_retires_its_feed_and_says_so() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_transcript_loading(true);

        let open = apply_session_view(&mut chat, Err(anyhow::anyhow!("session manager stopped")));

        assert!(!open);
        assert!(chat.transcript_loading);
        assert_eq!(
            chat.notice().as_deref(),
            Some("connection lost: session manager stopped")
        );
    }

    #[tokio::test]
    async fn dictation_completion_preserves_edits_and_recovers_after_errors() {
        let fixture = mj_client::session::replacement_session_test_fixture("session-dictation", 72);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            "original".into(),
            Notices::default(),
        );
        chat.state.voice_active = true;
        chat.state.set_input("edited while recording".into());
        chat.apply_voice_update(VoiceUpdate::Finished(Ok("spoken words".into())));
        assert_eq!(chat.draft(), "edited while recording spoken words");
        assert!(!chat.state.voice_active);
        chat.state.voice_active = true;
        chat.apply_voice_update(VoiceUpdate::Finished(Err(anyhow::anyhow!(
            "capture failed"
        ))));
        assert_eq!(chat.draft(), "edited while recording spoken words");
        assert!(!chat.state.voice_active);
        assert!(chat.state.notice().unwrap().contains("capture failed"));
        chat.apply_voice_update(VoiceUpdate::Finished(Ok(String::new())));
        assert_eq!(chat.draft(), "edited while recording spoken words");
    }

    #[tokio::test]
    async fn an_open_chat_hands_off_to_a_replacement_actor_without_losing_its_draft() {
        let fixture = mj_client::session::replacement_session_test_fixture("session-replaced", 73);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            "half-written prompt".into(),
            Notices::default(),
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                ActiveChat::pump(Some(&mut chat)).await;
                if chat.session_feed_open() && !chat.session.is_stopped() {
                    break;
                }
            }
        })
        .await
        .expect("the replacement actor became the live chat feed");

        assert_eq!(chat.draft(), "half-written prompt");
        assert_eq!(
            chat.state.notice().as_deref(),
            Some("Reconnected to session relay")
        );
    }

    #[tokio::test]
    async fn detaching_a_chat_keeps_a_reviewer_draft_waiting_for_its_late_stream() {
        let fixture =
            mj_client::session::replacement_session_test_fixture("session-deferred-review", 74);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        let request = ElicitationRequest {
            id: "reviewer-deferred-1".into(),
            message: "Allow the reviewer to continue?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        let mut reviewer = ChatState::new(&snapshot(), &[]);
        assert!(reviewer.show_review_role_elicitation(Some("reviewer-a".into()), request.clone(),));
        let reviewer_draft = reviewer
            .elicitation_draft()
            .expect("reviewer form can be snapshotted");

        // The primary form is visible while the sidecar has not surfaced its
        // form yet. A session switch must retain both local snapshots so the
        // reviewer answer is restored when its stream catches up.
        chat.state.restore_elicitation(request);
        chat.deferred_elicitation_draft = Some(reviewer_draft);
        let drafts = chat.elicitation_drafts();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().any(|draft| !draft.reviewer()));
        assert!(
            drafts
                .iter()
                .any(|draft| draft.reviewer() && draft.reviewer_role() == Some("reviewer-a"))
        );
    }

    #[tokio::test]
    async fn an_external_review_removal_closes_its_visible_question() {
        let fixture =
            mj_client::session::replacement_session_test_fixture("session-review-removed", 76);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        let request = ElicitationRequest {
            id: "reviewer-removed-1".into(),
            message: "Allow the reviewer to continue?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };
        assert!(
            chat.state
                .show_review_role_elicitation(Some("reviewer-a".into()), request)
        );
        assert!(chat.state.reviewer_elicitation_open());

        // A missing runtime review is authoritative even when the journal
        // poll that used to carry the form has no more events.
        chat.apply_review_view(None);
        assert!(!chat.state.reviewer_elicitation_open());
        assert!(chat.elicitation_draft().is_none());
    }

    /// A Codex session exposes plan mode through its `collaboration_mode`
    /// config, which the chat reads only once it knows the harness is Codex.
    /// That fact reaches the chat through the header now that the daemon owns
    /// the recovery context the open path used to carry, so a Codex session
    /// must still list `/plan`.
    #[tokio::test]
    async fn a_codex_session_lists_plan_from_the_header_harness() {
        use crate::hel_chat::test_support::select_config_option;
        use hel::hel_config::HarnessKind;

        let fixture = mj_client::session::replacement_session_test_fixture("session-codex", 75);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity {
                harness_kind: Some(HarnessKind::Codex),
                ..Default::default()
            },
            String::new(),
            Notices::default(),
        );

        chat.state.set_config_options(&[select_config_option(
            "collaboration_mode",
            "default",
            &["default", "plan"],
        )]);

        assert!(
            chat.state.lists_command("plan"),
            "a Codex session lists /plan once the header names the harness"
        );
    }

    /// A workspace id no configuration uses, so the per-workspace rows the
    /// constructor reads are simply absent and it falls back to its defaults —
    /// the same tolerance the other open tests rely on.
    const CONTEXT_TEST_WORKSPACE: &str = "workspace-for-chat-session-context-tests";

    fn context_session_record(id: &str, workspace_id: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            title: "work".into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
            last_profile: "codex-1".into(),
            bundle_id: "bundle-1".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            container_cpus: None,
            container_memory: None,
            state: hel::hel_state::SessionState::Running,
            archived: false,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-09T12:00:00Z".into(),
            updated_at: "2026-08-09T12:01:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    fn config_with_profiles(profiles: &[(&str, hel::hel_config::HarnessKind)]) -> HelConfig {
        HelConfig {
            profiles: profiles
                .iter()
                .map(|(id, kind)| {
                    (
                        (*id).to_owned(),
                        hel::hel_config::HarnessProfile {
                            enabled: true,
                            kind: *kind,
                            home: std::path::PathBuf::from("/profiles").join(id),
                            environment: BTreeMap::new(),
                            context_window_bytes: None,
                        },
                    )
                })
                .collect(),
            ..HelConfig::default()
        }
    }

    fn chat_context(
        session_id: &str,
        profiles: &[(&str, hel::hel_config::HarnessKind)],
    ) -> ChatSessionContext {
        ChatSessionContext {
            config: config_with_profiles(profiles),
            session: context_session_record(session_id, CONTEXT_TEST_WORKSPACE),
            reviewer_stager: mj_client::session::ReviewerStager::unavailable(
                "reviewer staging is unavailable in this chat test",
            ),
        }
    }

    /// The reviewer waterfall offers what the session's own configuration
    /// holds. A chat opened without that context offers nothing, which leaves
    /// `/review` and the second opinion with no harness to run.
    #[tokio::test]
    async fn reviewer_profiles_lists_only_enabled_context_profiles_for_the_waterfall() {
        use hel::hel_config::HarnessKind;

        let fixture = mj_client::session::replacement_session_test_fixture("session-profiles", 80);
        let mut context = chat_context(
            "session-profiles",
            &[
                ("codex-1", HarnessKind::Codex),
                ("claude-1", HarnessKind::Claude),
            ],
        );
        context.config.profiles.get_mut("claude-1").unwrap().enabled = false;
        let chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(context),
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );

        let offered = chat
            .reviewer_profiles()
            .into_iter()
            .map(|choice| (choice.id, choice.harness))
            .collect::<Vec<_>>();

        assert_eq!(offered, vec![("codex-1".to_owned(), "codex".to_owned())]);
    }

    #[tokio::test]
    async fn review_status_configuration_is_applied_on_open_and_refresh() {
        use hel::hel_review::lanes::ReviewTier;

        let fixture = mj_client::session::replacement_session_test_fixture("session-review", 88);
        let mut context = chat_context("session-review", &[]);
        context.config.review = hel::hel_config::ReviewConfig {
            enabled: true,
            tier: ReviewTier::Extended,
            profile: Some("reviewer-a".into()),
            model: None,
            effort: None,
        };
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(context),
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );

        assert_eq!(
            chat.state.review_config(),
            &hel::hel_config::ReviewConfig {
                enabled: true,
                tier: ReviewTier::Extended,
                profile: Some("reviewer-a".into()),
                model: None,
                effort: None,
            }
        );

        let mut reloaded = HelConfig::default();
        reloaded.review.profile = Some("reviewer-b".into());
        chat.refresh_context(&reloaded, None);

        assert_eq!(chat.state.review_config(), &reloaded.review);
    }

    /// A failed recovery copy is the one thing the user has to see on opening
    /// the session, so it is raised after the connection notice a cold open
    /// also sets: a notice is a single slot, and the last write wins.
    #[tokio::test]
    async fn a_recorded_checkpoint_error_reaches_the_notice_when_the_chat_opens() {
        let fixture =
            mj_client::session::replacement_session_test_fixture("session-checkpoint", 81);
        let mut context = chat_context("session-checkpoint", &[]);
        context.session.last_checkpoint_error = Some("the target ran out of disk".into());

        let chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(context),
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );

        assert_eq!(
            chat.state.notice().as_deref(),
            Some("Recovery copy failed: the target ran out of disk")
        );
    }

    /// A captured plan opens the waterfall over the profiles the context
    /// holds, and says so plainly when the context holds none.
    #[tokio::test]
    async fn a_second_opinion_opens_the_reviewer_waterfall_from_the_context() {
        use hel::hel_config::HarnessKind;

        let request = ElicitationRequest {
            id: "plan-1".into(),
            message: "may I run this plan?".into(),
            title: None,
            description: None,
            fields: Vec::new(),
        };

        let fixture = mj_client::session::replacement_session_test_fixture("session-opinion", 83);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(chat_context(
                "session-opinion",
                &[("claude-1", HarnessKind::Claude)],
            )),
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );

        chat.open_second_opinion(request.clone(), "the plan".into());

        let Some(SecondOpinion::Setup { setup, .. }) = chat.state.second_opinion() else {
            panic!("a captured plan opens the reviewer waterfall");
        };
        assert_eq!(
            setup
                .profiles()
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-1"]
        );

        let fixture = mj_client::session::replacement_session_test_fixture("session-alone", 84);
        let mut alone = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(chat_context("session-alone", &[])),
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );

        alone.open_second_opinion(request, "the plan".into());

        assert!(alone.state.second_opinion().is_none());
        assert_eq!(
            alone.state.notice().as_deref(),
            Some("Configure a second profile to review plans with")
        );
    }

    /// The chat snapshots the configuration when it opens, so a reload has to
    /// be handed to it; otherwise a long-lived conversation goes on offering
    /// the profiles that existed when it was opened.
    #[tokio::test]
    async fn a_refreshed_config_changes_the_offered_reviewer_profiles() {
        use hel::hel_config::HarnessKind;

        let fixture = mj_client::session::replacement_session_test_fixture("session-refresh", 85);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(chat_context(
                "session-refresh",
                &[("codex-1", HarnessKind::Codex)],
            )),
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        assert_eq!(chat.reviewer_profiles().len(), 1);

        let reloaded = config_with_profiles(&[
            ("codex-1", HarnessKind::Codex),
            ("claude-1", HarnessKind::Claude),
        ]);
        let moved = context_session_record("session-refresh", "workspace-moved");
        chat.refresh_context(&reloaded, Some(&moved));

        assert_eq!(
            chat.reviewer_profiles()
                .into_iter()
                .map(|choice| choice.id)
                .collect::<Vec<_>>(),
            vec!["claude-1".to_owned(), "codex-1".to_owned()]
        );
        assert_eq!(
            chat.context
                .as_ref()
                .map(|context| context.session.workspace_id.as_str()),
            Some("workspace-moved")
        );

        // Another session's record is not this session's, so it is ignored.
        let other = context_session_record("session-other", "workspace-other");
        chat.refresh_context(&reloaded, Some(&other));
        assert_eq!(
            chat.context
                .as_ref()
                .map(|context| context.session.workspace_id.as_str()),
            Some("workspace-moved")
        );

        // A chat opened without a context has nothing to refresh.
        let fixture = mj_client::session::replacement_session_test_fixture("session-bare", 86);
        let mut bare = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        bare.refresh_context(&reloaded, None);
        assert!(bare.reviewer_profiles().is_empty());
    }

    #[tokio::test]
    async fn a_same_session_context_refresh_updates_the_visible_header_without_losing_chat_state() {
        use hel::hel_config::HarnessKind;

        let fixture =
            mj_client::session::replacement_session_test_fixture("session-header-refresh", 89);
        let mut initial =
            chat_context("session-header-refresh", &[("codex-1", HarnessKind::Codex)]);
        initial.session.target_template_id = "localhost".into();
        initial.session.last_profile = "codex-1".into();
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(initial),
            fixture.control,
            SessionHeaderIdentity {
                target: "localhost".into(),
                profile: "codex-1".into(),
                title: "Original session title".into(),
                harness_kind: Some(HarnessKind::Codex),
            },
            "keep this draft".into(),
            Notices::default(),
        );
        chat.state.entries.push(ChatEntry::plain(
            1,
            ChatRole::User,
            "history that must remain",
        ));

        let reloaded = config_with_profiles(&[
            ("codex-1", HarnessKind::Codex),
            ("claude-2", HarnessKind::Claude),
        ]);
        let mut moved = context_session_record("session-header-refresh", "workspace-moved");
        moved.target_template_id = "podman".into();
        moved.last_profile = "claude-2".into();
        moved.harness_kind = HarnessKind::Claude;
        moved.acp_session_title = Some("Harness session title".into());
        moved.session_title_override = Some("Renamed session".into());
        chat.refresh_context(&reloaded, Some(&moved));

        assert_eq!(chat.draft(), "keep this draft");
        assert!(
            chat.state
                .entries
                .iter()
                .any(|entry| entry.text == "history that must remain")
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| render_full_frame(frame, &mut chat.state, false))
            .expect("draw refreshed chat");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("podman  Idle  claude-2  Renamed session"),
            "the refreshed target/profile must be visible in the conversation header: {rendered:?}"
        );
        assert!(!rendered.contains("localhost  Idle  codex-1"));
        assert!(!rendered.contains("Original session title"));
        assert!(!rendered.contains("Harness session title"));
    }

    #[tokio::test]
    async fn an_active_runtime_record_rearms_a_chat_after_its_handoff_timed_out() {
        let fixture = mj_client::session::replacement_session_test_fixture("session-resumed", 74);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            "still drafting".into(),
            Notices::default(),
        );
        chat.session_open = false;
        chat.finish_session_reconnect(Err("session session-resumed is not managed".into()));

        chat.set_session_feed_expected(true);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                ActiveChat::pump(Some(&mut chat)).await;
                if chat.session_feed_open() && !chat.session.is_stopped() {
                    break;
                }
            }
        })
        .await
        .expect("the active runtime record restarted the session handoff");

        assert_eq!(chat.draft(), "still drafting");
        assert_eq!(
            chat.state.notice().as_deref(),
            Some("Reconnected to session relay")
        );
    }

    #[tokio::test]
    async fn a_retiring_session_does_not_reconnect_when_its_feed_closes() {
        let fixture = mj_client::session::replacement_session_test_fixture("session-stop", 12);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        chat.set_session_retiring(true);

        chat.apply_session_view(Err(anyhow::anyhow!(
            "session manager stopped: channel closed"
        )));

        assert!(!chat.session_feed_open());
        assert!(
            !chat.session_reconnect_in_flight,
            "a deliberate stop starts no handoff"
        );
        assert!(
            !chat.state.notice().is_some_and(|notice| {
                notice.contains("Could not reconnect") || notice.contains("connection lost")
            }),
            "a deliberate stop reports neither a lost connection nor a failed handoff, but the notice was {:?}",
            chat.state.notice()
        );

        // A session that becomes runnable again is expected once more, and the
        // handoff comes back with it.
        chat.set_session_feed_expected(true);
        assert!(!chat.session_retiring());
        assert!(chat.session_reconnect_in_flight);
    }

    #[tokio::test]
    async fn a_retiring_sessions_reconnect_failure_is_not_reported() {
        let fixture = mj_client::session::replacement_session_test_fixture("session-destroy", 13);
        let mut chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        );
        chat.session_open = false;
        chat.set_session_retiring(true);

        chat.finish_session_reconnect(Err("session session-destroy is not managed".into()));

        assert!(
            !chat
                .state
                .notice()
                .is_some_and(|notice| notice.contains("Could not reconnect")),
            "a deliberate stop reports no reconnect failure, but the notice was {:?}",
            chat.state.notice()
        );
        assert!(!chat.session_reconnect_in_flight);
    }

    #[test]
    fn retiring_the_session_feed_keeps_a_closing_phase_in_place() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Closed;

        assert!(!apply_session_view(
            &mut chat,
            Err(anyhow::anyhow!("session manager stopped"))
        ));
        assert_eq!(chat.phase(), WorkerPhase::Closed);
    }

    #[test]
    fn leaving_the_chat_reports_the_ordinal_the_user_has_read() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut session = MaterializedSession::empty("session-read");
        session.applied_event_ordinal = 12;
        session.transcript = vec![agent_transcript_item("first", 12)];
        apply_session_view(&mut chat, Ok(managed_view(session)));
        chat.queued_prompts.push_back(queued("queued-1", "queued"));

        assert_eq!(detach_chat(&mut chat), 12);
        // The transcript stays warm for the next visit; the interaction state
        // that belonged to the visit does not.
        assert_eq!(chat.entries.len(), 1);
        assert!(chat.queued_prompts.is_empty());
        assert_eq!(detach_chat(&mut chat), 12);
    }

    #[test]
    fn a_notice_set_through_a_shared_handle_shows_in_the_chat_footer_in_yellow() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let shared = Notices::default();
        chat.notices = shared.clone();
        // Wide enough that the default hint line (over 100 columns) is not
        // truncated, so the footer text comparisons below are meaningful.
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");

        // Set from "outside", the way the surface's clone of the same handle
        // would.
        shared.set("Background import finished");
        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("draw chat");
        let buffer = terminal.backend().buffer();
        let footer_row = buffer.area.bottom() - 1;
        let footer_text = (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, footer_row)].symbol())
            .collect::<String>();
        assert!(footer_text.contains("Background import finished"));
        assert_eq!(
            buffer[(buffer.area.x, footer_row)].fg,
            theme::palette().warning
        );

        shared.clear();
        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("draw chat");
        let buffer = terminal.backend().buffer();
        let footer_text = (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, footer_row)].symbol())
            .collect::<String>();
        assert!(footer_text.contains("Tab pane"), "{footer_text:?}");
        assert_eq!(
            buffer[(buffer.area.x, footer_row)].fg,
            theme::palette().text
        );
    }

    /// The composer's own row is where a user typing in it learns the keys,
    /// so it carries the same three groups the dashboard's row does: the
    /// composer's own keys, the chords, then the function keys.
    #[test]
    fn chat_footer_advertises_alt_keys_and_f1() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut terminal = Terminal::new(TestBackend::new(200, 24)).expect("terminal");
        let footer_of = |terminal: &Terminal<TestBackend>| {
            let buffer = terminal.backend().buffer();
            let row = buffer.area.bottom() - 1;
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, row)].symbol())
                .collect::<String>()
        };

        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, true))
            .expect("draw chat");
        let footer = footer_of(&terminal);
        for hint in [
            "Ctrl-R history",
            "Alt-T rendering",
            "│ Alt-G panes · Alt-Q detach │",
            "F2 palette · F4 web · F5 refresh · F7 setup · F1 help",
        ] {
            assert!(footer.contains(hint), "{footer:?} omits {hint}");
        }
        assert!(!footer.contains("Ctrl-G"), "{footer:?}");

        assert!(!footer.contains("Ctrl-T"), "{footer:?}");

        // The queued-prompt variant is a different string and must say the
        // same things about the keys it still names.
        chat.queued_prompts.push_back(queued("queued-1", "next"));
        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, true))
            .expect("draw chat with a queued prompt");
        let footer = footer_of(&terminal);
        for hint in [
            "Ctrl-R history",
            "│ Alt-G panes · Alt-Q detach │",
            "F2 palette · F4 web · F5 refresh · F7 setup · F1 help",
        ] {
            assert!(footer.contains(hint), "{footer:?} omits {hint}");
        }
    }

    #[test]
    fn narrow_chat_footer_keeps_complete_palette_and_help_hints_on_screen() {
        let chat = ChatState::new(&snapshot(), &[]);
        for width in [7, 20, 32, 40, 80] {
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("terminal");
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_chat_footer(frame, test_footer(area), &chat, true);
                })
                .expect("draw narrow footer");
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.trim_end().ends_with("F1 help"), "{width}: {text:?}");
            if width >= 20 {
                assert!(text.contains("F2 palette"), "{width}: {text:?}");
            }
            if width == 32 {
                assert_eq!(text.trim_end(), "F2 palette · F4 web · F1 help");
            }
        }
    }

    #[test]
    fn composer_title_shows_live_model_and_effort_without_outer_session_frame() {
        use agent_client_protocol::schema::v1::{
            SessionConfigSelectOption, SessionConfigSelectOptions,
        };

        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "gpt-5.6-sol",
                SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                    "gpt-5.6-sol",
                    "Sol",
                )]),
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "effort",
                "Effort",
                "high",
                SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                    "high", "High",
                )]),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Running;
        chat.set_prompt_in_flight(true);
        chat.set_config_options(&options);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");

        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("draw chat");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert_eq!(rendered.matches("gpt-5.6-sol · high").count(), 1);
        assert!(rendered.contains("Esc cancels"));
        assert!(!rendered.contains("Running"));
        // No outer frame wraps the whole session: the transcript's own titled
        // border is the first thing on the frame, not a session title bar.
        assert!(!rendered.contains("HEL /"));
        assert!(rendered.starts_with("╭ Conversation "), "{rendered:?}");
    }

    #[test]
    fn composer_title_shows_fast_only_while_the_confirmed_mode_is_active() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[fast_mode_option("off")]);
        assert!(!prompt_title(&chat).contains("Fast"));

        chat.set_config_options(&[fast_mode_option("on")]);
        assert_eq!(prompt_title(&chat), " Fast ");

        chat.set_config_options(&[]);
        assert!(!prompt_title(&chat).contains("Fast"));
    }

    /// The relay cannot cancel a turn the harness started on its own, so the
    /// composer offers Esc only while a prompt of ours is in flight.
    #[test]
    fn composer_bottom_offers_esc_only_while_a_prompt_of_ours_is_in_flight() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Running;
        assert!(prompt_bottom_queue_control(&chat).is_none());

        chat.set_prompt_in_flight(true);
        assert_eq!(
            prompt_bottom_queue_control(&chat).unwrap().to_string(),
            " Esc cancels"
        );
        assert!(!prompt_title(&chat).contains("Esc cancels"));
    }

    #[tokio::test]
    async fn escape_names_steering_through_submission_and_acceptance() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use hel::hel_state::{MaterializedQueuedPrompt, QueuedCommandKind};
        use hel::hel_worker::{ActiveRelayPrompt, RelayCommand};

        for (supported, queue_kind, hint, sending, requested) in [
            (
                Some(true),
                Some(QueuedCommandKind::Prompt),
                "Esc steers next",
                "Sending steering request…",
                "Steering requested",
            ),
            (
                Some(false),
                Some(QueuedCommandKind::Prompt),
                "Esc cancels",
                "Sending cancellation request…",
                "Cancellation requested",
            ),
            (
                None,
                Some(QueuedCommandKind::Prompt),
                "Esc applies next",
                "Requesting queued prompt…",
                "Queued prompt requested",
            ),
            (
                Some(true),
                Some(QueuedCommandKind::SetConfig {
                    key: "model".into(),
                    value: "next-model".into(),
                }),
                "Esc cancels",
                "Sending cancellation request…",
                "Cancellation requested",
            ),
            (
                Some(true),
                None,
                "Esc cancels",
                "Sending cancellation request…",
                "Cancellation requested",
            ),
        ] {
            let mut fixture =
                mj_client::session::replacement_session_test_fixture("steering-session", 12);
            let mut chat = ActiveChat::open(
                fixture.stopped,
                "bundle-1",
                None,
                fixture.control,
                SessionHeaderIdentity::default(),
                String::new(),
                Notices::default(),
            );
            let mut materialized = MaterializedSession::empty("steering-session");
            if let Some(kind) = queue_kind {
                materialized.queued_prompts.push(MaterializedQueuedPrompt {
                    accepted_ordinal: None,
                    command_id: "queued-correction".into(),
                    kind,
                    content: vec![serde_json::json!({"type": "text", "text": "change direction"})],
                    queued_at_ms: 0,
                });
            }
            let mut view = managed_view(materialized);
            let operational = &mut view.snapshot.as_mut().unwrap().operational;
            operational.steering_supported = supported;
            operational.active_prompt = Some(ActiveRelayPrompt {
                command_id: "running-prompt".into(),
                created_at_ms: 0,
                started_at_ms: 0,
            });
            apply_session_view(&mut chat.state, Ok(view));
            let screen = drawn_transcript(&mut chat.state, 100, 24).join("\n");
            assert!(screen.contains(hint), "{screen}");

            chat.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            assert_eq!(chat.state.notice().as_deref(), Some(sending));

            let result = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let result = chat
                        .remote
                        .recv()
                        .await
                        .expect("remote worker remains open");
                    if matches!(result, ChatRemoteResult::Cancel { .. }) {
                        break result;
                    }
                }
            })
            .await
            .expect("turn control request completes");
            assert!(matches!(
                fixture.submitted.recv().await,
                Some(RelayCommand::Cancel)
            ));

            // A newer view may already have consumed the queue. The reply
            // must still describe the request that was actually submitted.
            chat.state.queued_prompts.clear();
            apply_chat_remote_result(&mut chat.state, result);
            assert_eq!(chat.state.notice().as_deref(), Some(requested));
        }
    }

    /// Background commands are exposed through the embedded task control;
    /// the composer title stays focused on meaningful state.
    #[test]
    fn composer_title_names_the_work_the_agent_left_running() {
        let now_seconds = 10_000;
        let started_at_ms = now_seconds as i64 * 1_000 - 2_616_000;
        let mut chat = ChatState::new(&snapshot(), &[]);
        assert!(prompt_title(&chat).is_empty());

        chat.set_session_activity(crate::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            prompt_in_flight: false,
            idle_since_ms: None,
            execution: None,
            harness_turn_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![hel::hel_worker::BackgroundCommand {
                id: "test:active".into(),
                started_at_ms,
                command: "cargo   test".into(),
                can_stop: false,
            }],
            active_user_shells: Vec::new(),
        });
        assert!(prompt_title(&chat).is_empty());
        assert!(!prompt_title(&chat).contains("Background"));

        chat.set_session_activity(crate::usage_format::SessionActivity {
            pursuing_goal: Default::default(),
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            prompt_in_flight: false,
            idle_since_ms: None,
            execution: None,
            harness_turn_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![
                hel::hel_worker::BackgroundCommand {
                    id: "test:active-1".into(),
                    started_at_ms,
                    command: "cargo test".into(),
                    can_stop: false,
                },
                hel::hel_worker::BackgroundCommand {
                    id: "test:active-2".into(),
                    started_at_ms: started_at_ms + 1_000,
                    command: "npm run build".into(),
                    can_stop: false,
                },
            ],
            active_user_shells: Vec::new(),
        });
        let screen = drawn_transcript(&mut chat, 100, 24).join("\n");
        assert!(screen.contains("View tasks (2)"), "{screen}");

        // The spinner represents a running turn, whatever it left behind.
        chat.phase = WorkerPhase::Running;
        assert!(!prompt_title(&chat).contains("Running"));
    }

    #[test]
    fn composer_title_names_plan_mode_even_during_a_turn() {
        let mut chat = crate::hel_chat::test_support::grok_chat();
        chat.finish_plan_mode_change(true);
        chat.phase = WorkerPhase::Running;

        assert!(prompt_title(&chat).contains("PLAN MODE"));
        assert!(!prompt_title(&chat).contains("Prompt"));
    }

    #[test]
    fn composer_title_identifies_an_active_advertised_goal() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.apply_session_update(
            1,
            &serde_json::json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    {"name": "goal", "description": "set a persistent goal"}
                ]
            }),
        );
        chat.apply_event(&SequencedEvent {
            seq: 2,
            recorded_at_ms: None,
            request_id: Some("goal".into()),
            event: WorkerEvent::PromptAccepted {
                request_id: "goal".into(),
                text: "/goal ship the release".into(),
                attachments: Vec::new(),
            },
        });

        assert!(prompt_title(&chat).contains("Pursuing goal"));
        assert!(!prompt_title(&chat).contains("Running"));

        chat.apply_event(&SequencedEvent {
            seq: 3,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::TurnCompleted,
        });
        assert!(prompt_title(&chat).is_empty());
        assert!(!prompt_title(&chat).contains("Pursuing goal"));
    }

    #[test]
    fn composer_title_does_not_label_ordinary_or_unadvertised_prompts_as_goals() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.mark_prompt_submitted("/goal ship the release");
        assert!(!prompt_title(&chat).contains("Running"));
        assert!(!prompt_title(&chat).contains("Pursuing goal"));

        chat.apply_session_update(
            1,
            &serde_json::json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    {"name": "goal", "description": "set a persistent goal"}
                ]
            }),
        );
        chat.mark_prompt_submitted("please ship the release");
        assert!(!prompt_title(&chat).contains("Running"));
        assert!(!prompt_title(&chat).contains("Pursuing goal"));
    }

    /// The title names the conversation you are in. The rule around it is
    /// chrome and stays dim; the name draws bright white so it stands out.
    #[test]
    fn the_conversation_title_remains_readable_above_its_quiet_rule() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");

        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("draw chat");

        let buffer = terminal.backend().buffer();
        let cells: Vec<_> = (0..80)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect();
        let title_start = cells
            .windows(1)
            .position(|cell| cell[0] == "C")
            .expect("the title is on the top row");
        for offset in 0.."Conversation".chars().count() {
            let column = u16::try_from(title_start + offset).unwrap();
            assert_eq!(
                buffer[(column, 0)].fg,
                theme::palette().text,
                "the title draws bright white: {}",
                cells.concat()
            );
        }
        let rule = cells
            .iter()
            .rposition(|cell| cell == "\u{2500}")
            .expect("the rule follows the title");
        assert_eq!(
            buffer[(u16::try_from(rule).unwrap(), 0)].fg,
            theme::palette().border,
            "the rule stays chrome"
        );
    }

    /// A host that owns the rest of the frame gives the chat two rectangles;
    /// nothing it draws may leak outside them.
    #[test]
    fn draw_in_places_the_transcript_and_prompt_in_the_given_regions() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let regions = ChatRegions {
            transcript: Rect::new(0, 4, 80, 12),
            prompt: Rect::new(0, 16, 80, 5),
            footer: None,
            overlay: Rect::new(0, 0, 80, 24),
        };

        terminal
            .draw(|frame| render_in(frame, &mut chat, regions, true, false))
            .expect("draw chat");

        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        for y in 0..4 {
            assert_eq!(
                row(y).trim(),
                "",
                "row {y} sits above the transcript region and must stay untouched"
            );
        }
        assert!(
            row(4).contains("Conversation"),
            "the transcript's titled border is the region's first row: {:?}",
            row(4)
        );
        assert!(row(16).contains(VOICE_BUTTON_GLYPH), "{:?}", row(16));
        assert!(!row(16).contains("Prompt"), "{:?}", row(16));
        assert_eq!(
            row(17).chars().take(4).collect::<String>(),
            "│> W",
            "{:?}",
            row(17)
        );
        // Focus changes the border's color without changing its geometry.
        assert!(
            row(20).starts_with('╰'),
            "the prompt's bottom border closes the region: {:?}",
            row(20)
        );
        for y in 21..24 {
            assert_eq!(
                row(y).trim(),
                "",
                "row {y} sits below the prompt region and must stay untouched"
            );
        }
    }

    #[test]
    fn composer_border_holds_activity_without_moving_the_transcript_or_input() {
        for width in [16, 32, 48, 80] {
            let mut chat = ChatState::new(&snapshot(), &[]);
            chat.set_spinner_style(crate::spinner::SpinnerStyle::Pulse);
            chat.set_input("my follow-up".into());
            let idle = drawn_transcript(&mut chat, width, 24);
            let input = chat
                .frame_surfaces()
                .surface(SurfaceId::PromptInput)
                .unwrap()
                .rect;

            let transcript = chat
                .frame_surfaces()
                .surface(SurfaceId::Transcript)
                .unwrap()
                .rect;
            chat.mark_prompt_submitted("continue");
            let running = drawn_transcript(&mut chat, width, 24);
            let prompt_top = usize::from(input.y.saturating_sub(1));
            let prompt_bottom = prompt_top + 1 + usize::from(input.height);
            let prompt_title = &running[prompt_top];
            assert!(prompt_title.contains(VOICE_BUTTON_GLYPH), "{prompt_title}");
            assert!(!prompt_title.contains("Prompt"), "{prompt_title}");
            assert!(!prompt_title.contains("Running"), "{prompt_title}");
            let spinner_width = prompt_title
                .chars()
                .filter(|ch| matches!(ch, '·' | '∙' | '•' | '●'))
                .count();
            assert_eq!(
                spinner_width,
                if width == 16 {
                    1
                } else {
                    crate::spinner::SPINNER_WIDTH
                },
                "{prompt_title}"
            );
            let last_spinner = prompt_title
                .chars()
                .enumerate()
                .filter_map(|(index, ch)| matches!(ch, '·' | '∙' | '•' | '●').then_some(index))
                .last()
                .expect("spinner frame on the prompt title");
            assert_eq!(
                last_spinner,
                prompt_title.chars().count() - 3,
                "spinner occupies the upper-right title: {prompt_title}"
            );
            assert!(
                running[prompt_bottom].contains("Esc cancels"),
                "{running:?}"
            );
            assert_eq!(running[0], idle[0], "the transcript keeps its full height");
            assert_eq!(running[input.y as usize], idle[input.y as usize]);
            assert_eq!(
                chat.frame_surfaces()
                    .surface(SurfaceId::Transcript)
                    .unwrap()
                    .rect,
                transcript,
                "activity must not consume a transcript row"
            );

            chat.phase = WorkerPhase::Idle;
            chat.prompt_in_flight = false;
            chat.turn_started_at_epoch_seconds = None;
            assert_eq!(drawn_transcript(&mut chat, width, 24), idle);
        }
    }

    /// The cursor belongs to whatever owns the keyboard, and the host decides
    /// that, so the composer only shows one when it is told it has focus.
    #[test]
    fn draw_in_draws_a_cursor_only_when_the_prompt_has_focus() {
        use ratatui::backend::Backend as _;

        let mut chat = ChatState::new(&snapshot(), &[]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let regions = ChatRegions {
            transcript: Rect::new(0, 0, 80, 16),
            prompt: Rect::new(0, 16, 80, 6),
            footer: Some(test_footer(Rect::new(0, 22, 80, 1))),
            overlay: Rect::new(0, 0, 80, 24),
        };

        terminal
            .draw(|frame| render_in(frame, &mut chat, regions, false, false))
            .expect("draw chat");
        terminal.backend_mut().assert_cursor_position((0, 0));

        terminal
            .draw(|frame| render_in(frame, &mut chat, regions, true, false))
            .expect("draw chat");
        let cursor = terminal
            .backend_mut()
            .get_cursor_position()
            .expect("cursor position");
        assert!(
            cursor.y > 16 && cursor.y < 21,
            "the cursor sits inside the prompt region: {cursor:?}"
        );

        let boolean_question = ElicitationRequest {
            id: "boolean-question".into(),
            message: "Should I continue?".into(),
            title: None,
            description: None,
            fields: vec![ElicitationField {
                id: "continue".into(),
                title: "Continue".into(),
                description: None,
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::Boolean { default: None },
            }],
        };
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            boolean_question,
        ));
        let mut question_terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        question_terminal
            .draw(|frame| render_in(frame, &mut chat, regions, true, false))
            .expect("draw question");
        question_terminal
            .backend_mut()
            .assert_cursor_position((0, 0));

        let text_question = ElicitationRequest {
            id: "text-question".into(),
            message: "What should I call it?".into(),
            title: None,
            description: None,
            fields: vec![ElicitationField {
                id: "name".into(),
                title: "Name".into(),
                description: None,
                required: false,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::Text {
                    default: None,
                    min_length: None,
                    max_length: None,
                    pattern: None,
                    format: None,
                },
            }],
        };
        chat.elicitation = Some(super::super::elicitation::ElicitationDialog::new(
            text_question,
        ));
        let mut text_terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        text_terminal
            .draw(|frame| render_in(frame, &mut chat, regions, true, false))
            .expect("draw text question");
        let question_cursor = text_terminal
            .backend_mut()
            .get_cursor_position()
            .expect("question cursor position");
        assert!(
            question_cursor.y < regions.prompt.bottom(),
            "the text question owns its cursor: {question_cursor:?}"
        );
        assert_ne!(question_cursor, Position::new(0, 0));
    }

    /// A conversation long enough that opening it converts the tail only.
    fn long_session() -> MaterializedSession {
        let mut session = MaterializedSession::empty("session-long");
        session.transcript = (1..=300)
            .map(|position| {
                agent_message_item(
                    &format!("agent:{position}"),
                    position,
                    &format!("message {position}"),
                )
            })
            .collect();
        session.applied_event_ordinal = 301;
        session
    }

    #[test]
    fn the_converted_history_completes_a_chat_opened_on_its_tail() {
        let session = long_session();
        let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
        let pending = chat.unconverted_prefix();
        assert!(pending > 0);
        let prefix = materialized_prefix_entries(
            &session.transcript[..pending],
            session.applied_event_ordinal,
        );

        let rebuild = apply_chat_io_update(
            &mut chat,
            ChatIoUpdate::TranscriptPrefix {
                attempt: 1,
                result: Ok((prefix, Vec::new())),
            },
        );

        assert_eq!(rebuild, PrefixRebuild::NotNeeded);
        assert_eq!(chat.unconverted_prefix(), 0);
        assert_eq!(chat.entries.len(), session.transcript.len());
        assert_eq!(chat.entries[0].text, "message 1");
        assert_eq!(chat.notice(), None);
    }

    #[test]
    fn history_that_no_longer_fits_the_tail_is_rebuilt_and_then_gives_up_with_a_notice() {
        let session = long_session();
        let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);
        let pending = chat.unconverted_prefix();
        // History from a transcript compaction rewrote: it overlaps the tail.
        let stale = materialized_prefix_entries(
            &session.transcript[session.transcript.len() - pending..],
            session.applied_event_ordinal,
        );

        let rebuild = apply_chat_io_update(
            &mut chat,
            ChatIoUpdate::TranscriptPrefix {
                attempt: 1,
                result: Ok((stale.clone(), Vec::new())),
            },
        );

        assert_eq!(rebuild, PrefixRebuild::Needed { attempt: 2 });
        assert_eq!(chat.unconverted_prefix(), pending);
        assert_eq!(chat.notice(), None);

        let exhausted = apply_chat_io_update(
            &mut chat,
            ChatIoUpdate::TranscriptPrefix {
                attempt: MAX_PREFIX_CONVERSION_ATTEMPTS,
                result: Ok((stale, Vec::new())),
            },
        );

        assert_eq!(exhausted, PrefixRebuild::NotNeeded);
        assert_eq!(chat.unconverted_prefix(), pending);
        assert!(
            chat.notice()
                .is_some_and(|notice| notice.contains("Earlier messages")),
            "giving up on the history has to be reported"
        );
    }

    #[test]
    fn a_failed_history_conversion_is_reported_instead_of_dropped() {
        let session = long_session();
        let mut chat = ChatState::from_materialized_tail(&session, &[], &[]);

        let rebuild = apply_chat_io_update(
            &mut chat,
            ChatIoUpdate::TranscriptPrefix {
                attempt: 1,
                result: Err("worker panicked".into()),
            },
        );

        assert_eq!(rebuild, PrefixRebuild::NotNeeded);
        assert_eq!(
            chat.notice().as_deref(),
            Some("Earlier messages failed to load: worker panicked")
        );
    }
}
