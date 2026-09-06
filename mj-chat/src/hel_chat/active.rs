//! The live conversation: the background feeds behind an open session, and the
//! transcript and composer the combined surface asks it to draw into the
//! regions it has chosen.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::hel_selection::{FrameSurfaces, SelectionRange, SurfaceFrame, SurfaceId};
use hel::hel_config::HelConfig;
use hel::hel_database::{HistoryScope, PromptHistoryEntry};
use hel::hel_state::{MaterializedSession, SessionRecord, TranscriptItem, config_command_text};
use hel::hel_transcript::ChatEntry;
use hel::hel_worker::WorkerPhase;
use mj_controller::hel_session_manager::{
    ManagedSessionHandle, ManagedSessionView, ReviewerAction, ReviewerOutcome,
    SessionManagerControl, ViewError, new_command_id,
};

use super::autocomplete::render_autocomplete;
use super::elicitation::render_elicitation_in;
use super::history::{highlighted_input_lines, history_scope_name, history_search_footer};
use super::input::{input_cursor_visual_position, input_visual_rows, set_input_cursor};
use super::remote::{
    ChatRemoteOperation, ChatRemoteResult, ChatRemoteSupervisor, apply_chat_remote_result,
    queue_chat_remote_operation, restore_unsent_input, restore_unsent_prompt,
};
use super::rendering::{display_width, truncate_to_width, voice_button_area, voice_button_line};
use super::second_opinion::{
    CapturedProposal, SecondOpinion, SecondOpinionIntent, render_reviewer, render_setup,
    render_split_actions, review_role_session_id, reviewer_session_id,
};
use super::transcript::{ToolDiffstatRequest, materialized_prefix_entries, render_transcript};
use super::{
    ChatAction, ChatElicitationDraft, ChatEventOutcome, ChatRegions, ChatSessionContext, ChatState,
    MOUSE_SCROLL_ROWS, Notices, SessionHeaderIdentity, queued_prompt_preview,
};
use crate::hel_clipboard::ClipboardContent;

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
    Clipboard(std::result::Result<ClipboardContent, String>),
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
        ChatIoUpdate::Clipboard(Ok(content)) => chat.handle_clipboard_content(content),
        ChatIoUpdate::Clipboard(Err(error)) => {
            tracing::warn!(%error, "clipboard read failed and was shown in the UI");
            chat.set_notice(format!("Paste failed: {error}"));
        }
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
            tracing::warn!(error = format!("{error:#}"), "chat session view failed");
            state.set_notice(format!("connection lost: {error:#}"));
            return false;
        }
    };
    // A transient connection error does not make an as-yet-unavailable
    // transcript empty. The actor keeps retrying, so retain the loading row
    // until a real projection arrives; the error still appears in the notice.
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
        state.set_session_activity(crate::usage_format::SessionActivity::of(
            &snapshot.operational,
        ));
    }
    if let Some(error) = view.error {
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
}

impl ActiveChat {
    /// Builds the view from the session's current snapshot and starts its
    /// background feeds. Cheap enough to call from the surface's loop: the
    /// only work done here is converting the tail of the transcript, a bounded
    /// number of items. Every other step, including converting the history in
    /// front of that tail, is a spawned task.
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
                state.set_session_activity(crate::usage_format::SessionActivity::of(
                    &snapshot.operational,
                ));
            }
            let pending = PendingPrefix::of(materialized, state.unconverted_prefix());
            (state, pending)
        };
        state.set_history_context(bundle_id);
        state.set_header_summary(header.target, header.profile);
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
        // Raised after the connection notice, because a notice is a single
        // slot: a cold open would otherwise replace the failure the user has
        // to see with "Connecting to session relay…".
        if let Some(detail) = context
            .as_ref()
            .and_then(|context| context.session.last_checkpoint_error.as_deref())
        {
            state.set_notice(format!("Recovery copy failed: {detail}"));
        }
        let mut diffstats_in_flight = 0;
        dispatch_diffstat_requests(&mut state, &chat_io_tx, &mut diffstats_in_flight);
        // A review that was open when the UI stopped is picked back up. The
        // reviewer's own journal on the target holds its conversation, so the
        // split is restored by replaying it rather than by keeping a second
        // copy of the transcript here.
        let stored =
            hel::hel_database::active_review(session.session_id()).unwrap_or_else(|error| {
                tracing::debug!(error = %format!("{error:#}"), "could not read the open review");
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
            let reviewer_transcript = stored.reviewer_transcript;
            if let Some(view) = state.second_opinion_mut() {
                view.begin_review(stored.workflow, status, stored.context_baseline);
                // The reviewer's own journal is the source while the target
                // lives; this copy is what keeps the conversation readable
                // once it does not.
                view.restore_reviewer(
                    &reviewer_session_id(session.session_id()),
                    reviewer_transcript,
                );
            }
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
    /// place: the fields the chat reads from it are fixed for a session's life,
    /// and a session that disappears from the list is not a reason to lose
    /// them. A chat opened without a context stays without one.
    pub fn refresh_context(&mut self, config: &HelConfig, session: Option<&SessionRecord>) {
        let Some(context) = self.context.as_mut() else {
            return;
        };
        context.config = config.clone();
        if let Some(session) = session.filter(|session| session.id == context.session.id) {
            context.session = session.clone();
        }
        self.state.set_review_config(config.review.clone());
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

    /// Waits for the next background message, applies it, and drains whatever
    /// queued behind it, so one wakeup costs one redraw.
    ///
    /// `None` means no chat is warm, and the feed never wakes the caller. Cancel
    /// safe: every arm is a cancel-safe receive, and a message is applied only
    /// once its arm has won.
    pub async fn pump(chat: Option<&mut Self>) {
        let Some(chat) = chat else {
            return std::future::pending().await;
        };
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
        if let Some(view) = self.state.second_opinion_mut() {
            view.set_status("the reviewer is reading the plan…");
        }
        self.persist_review();
        self.run_workflow_request(request);
    }

    /// Shows whatever review the daemon is running for this session.
    ///
    /// The terminal hosts no part of a review: it renders this view, reads the
    /// reviewing roles' journals to show their transcripts, and sends
    /// resolutions back. A review therefore survives this view closing.
    pub fn apply_review_view(
        &mut self,
        view: Option<mj_controller::hel_review_host::RuntimeReviewView>,
    ) {
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
            .profiles
            .iter()
            .map(|(id, profile)| ReviewerProfileChoice {
                id: id.clone(),
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
            Some(review) => review.report_failure(message),
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
            self.state
                .set_notice(format!("Chat background worker failed: {error}"));
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
            update => update,
        };
        if matches!(&update, ChatIoUpdate::Clipboard(_)) {
            self.paste_in_flight = false;
        }
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
        let paths = crate::dictation::auth_paths(&context.config, &context.session.last_profile);
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
                    crate::dictation::available_auth(paths)
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
                self.state.voice_active = false;
                self.voice_cancel = None;
                self.voice_finishing = false;
                match result {
                    Ok(text) => {
                        if !text.trim().is_empty() {
                            self.state
                                .set_input(append_dictation(&self.state.input, &text));
                        }
                        self.state.notices.clear();
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
        let action = match event {
            Event::Key(key) => self.state.handle_key(key),
            Event::Paste(pasted) => {
                self.state.handle_paste(&pasted);
                ChatAction::None
            }
            Event::Mouse(mouse) => self.state.handle_mouse(mouse),
            // Resize and focus changes only need the redraw.
            _ => ChatAction::None,
        };
        let outcome = self.dispatch(action);
        dispatch_history_search_request(&mut self.state, &self.chat_io_tx);
        outcome
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
                self.state.set_notice("Sending cancellation request…");
                queue_chat_remote_operation(
                    self.remote.operations(),
                    ChatRemoteOperation::Cancel {
                        command_id,
                        cancel_agent: self.state.prompt_in_flight(),
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
                    if let Err(error) = updates.send(ChatIoUpdate::Clipboard(result)) {
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
            .filter(|id| context.config.profiles.contains_key(*id))
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
            if let Some(view) = self.state.second_opinion_mut() {
                view.set_status("resuming the reviewer…");
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
        let controller = reviewer_staging_controller(context);
        let session_id = self.session.session_id().to_owned();
        let session = self.session.clone();
        let updates = self.chat_io_tx.clone();
        // The reviewer's lifetime generation is what decides whether the
        // running reviewer can be kept; `generation` here only says which
        // probe this answer belongs to.
        let lifetime = self.reviewer_generation;
        tokio::spawn(async move {
            let staged = tokio::task::spawn_blocking(move || {
                controller.stage_reviewer_profile(&session_id, &profile_id, lifetime)
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
                if let Some(view) = self.state.second_opinion_mut() {
                    view.report_failure(error);
                }
                return;
            }
        };
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion_mut() else {
            return;
        };
        if !events.is_empty() {
            review.reviewer.apply_events(&session_id, &events);
            // A completed answer is what unlocks transfer. The workflow
            // decides whether this is the turn it was waiting for.
            if let Some(answer) = review.reviewer.latest_answer()
                && let hel::hel_second_opinion::ReviewStage::Reviewing { command_id } =
                    review.workflow.stage().clone()
            {
                review.workflow.reviewer_turn_completed(&command_id, answer);
            }
        }
        let finished = review.workflow.finished();
        if let Some(view) = self.state.second_opinion_mut()
            && view.reviewer().is_some_and(|reviewer| !reviewer.is_empty())
        {
            view.set_status("Enter to act · Tab to choose");
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
        let content_width = usize::from(width.saturating_sub(2)).max(1);
        let input_rows =
            u16::try_from(input_visual_rows(&self.state.input, content_width)).unwrap_or(u16::MAX);
        let queued = u16::try_from(self.state.queued_prompts.len().min(3)).unwrap_or(3);
        input_rows.saturating_add(queued).saturating_add(2).max(4)
    }

    /// Draws the transcript and the composer into `regions`, for a host that
    /// owns the rest of the frame.
    ///
    /// `prompt_focused` says whether the composer owns the keyboard; only then
    /// does it draw a cursor and a double border. `transcript_selected` says
    /// the selection engine still owns a selection on the transcript, so its
    /// row space has to stay frozen for this frame.
    pub fn draw_in(
        &mut self,
        frame: &mut Frame,
        regions: ChatRegions,
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

    /// Whether the last frame's surfaces stand alone, because a modal owned
    /// the frame.
    pub fn frame_surfaces_exclusive(&self) -> bool {
        self.state.frame_surfaces_exclusive()
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
                self.state.scroll_history_up(MOUSE_SCROLL_ROWS)
            }
            SurfaceId::Transcript => self.state.scroll_history_down(MOUSE_SCROLL_ROWS),
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

/// The one-session controller a reviewer is staged through. Staging reads only
/// the profiles the reviewer may run under and the record of the session being
/// reviewed, so the chat builds a controller holding just those.
fn reviewer_staging_controller(
    context: &ChatSessionContext,
) -> mj_controller::hel_controller::Controller {
    mj_controller::hel_controller::Controller {
        config: context.config.clone(),
        state: hel::hel_state::HelState {
            sessions: std::collections::BTreeMap::from([(
                context.session.id.clone(),
                context.session.clone(),
            )]),
            ..hel::hel_state::HelState::default()
        },
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
    let prompt_width = usize::from(inner.width.saturating_sub(2)).max(1);
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
            footer: Some(chunks[2]),
            overlay: inner,
        },
        true,
        transcript_selected,
    );
}

/// Draws the transcript and the composer into `regions`.
///
/// `prompt_focused` says whether the composer owns the keyboard; only then
/// does it draw a cursor and a double border. `transcript_selected` says the
/// selection engine still owns a selection on the transcript, so its row
/// space has to stay frozen for this frame.
pub(super) fn render_in(
    frame: &mut Frame,
    chat: &mut ChatState,
    regions: ChatRegions,
    prompt_focused: bool,
    transcript_selected: bool,
) {
    chat.frame_surfaces.clear();
    chat.frame_surfaces_exclusive = false;
    // Modals and the completion popup are centred in the whole frame, not
    // in the band the transcript happens to have been given.
    let inner = regions.overlay;
    let transcript_area = regions.transcript;
    let prompt_area = regions.prompt;
    let prompt_width = usize::from(prompt_area.width.saturating_sub(2)).max(1);
    // Focus shows as a double border on whichever pane owns the keyboard, so
    // the split stays obvious without the eye following a moving band.
    let prompt_border = if prompt_focused {
        BorderType::Double
    } else {
        BorderType::Plain
    };
    // The button lives on the prompt's bottom border, so it is not part of
    // the selectable prompt interior. Clear the hitbox first because a split
    // view or modal may replace the composer for this frame.
    chat.voice_button_area = None;
    let split = chat.second_opinion_split() || chat.turn_review_split();
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
        let buttons = match chat.turn_review() {
            Some(review) => {
                super::turn_review::render_turn_review_actions(frame, prompt_area, review, &status)
            }
            None => Vec::new(),
        };
        chat.turn_review_action_areas = buttons;
        chat.split_action_areas.clear();
    } else if let Some(SecondOpinion::Review(review)) = chat.second_opinion() {
        // The split has no composer: the revised plan is the planner's to
        // write, so the only input here is which of the three actions to take.
        let buttons = render_split_actions(
            frame,
            prompt_area,
            &review.workflow,
            review.action,
            &review.status,
        );
        chat.split_action_areas = buttons;
        chat.turn_review_action_areas.clear();
    } else {
        chat.split_action_areas.clear();
        chat.turn_review_action_areas.clear();
        let queued = chat.queued_prompts.len();
        let prompt_title = prompt_title(chat, queued);
        let prompt_block = Block::default()
            .borders(Borders::ALL)
            .border_type(prompt_border)
            .title(prompt_title);
        let prompt_inner = prompt_block.inner(prompt_area);
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
                    Style::default().fg(Color::DarkGray),
                ))
            })
            .collect::<Vec<_>>();
        let queue_rows = prompt_lines.len();
        prompt_lines.extend(if let Some(search) = chat.history_search.as_ref() {
            highlighted_input_lines(&chat.input, &search.query)
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
                .wrap(Wrap { trim: false })
                .scroll((input_scroll as u16, 0))
                .block(prompt_block),
            prompt_area,
        );
        if let Some(button_area) = chat.voice_button_area {
            frame.render_widget(
                voice_button_line(chat.voice_available, chat.voice_active),
                button_area,
            );
        }
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
    if let Some(footer_area) = regions.footer {
        render_chat_footer(frame, footer_area, chat, prompt_focused);
    }
    // The popup overlays the prompt and whatever sits above it, so it
    // registers last and wins the cells it covers.
    if let Some(popup) = render_autocomplete(frame, prompt_area, chat) {
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::AutocompletePopup, popup));
    }
    if let Some(body) = super::config_picker::render_config_picker(frame, inner, chat) {
        // The selector owns the frame's interaction, so the chat behind it
        // stops being selectable. An elicitation dialog still draws over it,
        // matching the key routing that lets the dialog win.
        chat.frame_surfaces.clear();
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::ModalBody, body));
    }
    if let Some(SecondOpinion::Setup { captured, setup }) = chat.second_opinion() {
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
        );
        chat.frame_surfaces.clear();
        chat.frame_surfaces
            .push(SurfaceFrame::fixed(SurfaceId::ModalBody, body));
        return;
    }
    if let Some(dialog) = chat.elicitation.as_ref() {
        // The question owns only the session content area while it is up. The
        // navigator and support panes remain in the host's surface registry,
        // so they stay clickable even while this form is waiting for an answer.
        chat.frame_surfaces.clear();
        let question_area = Rect::new(
            transcript_area.x,
            transcript_area.y,
            transcript_area.width,
            prompt_area
                .bottom()
                .saturating_sub(transcript_area.y)
                .max(transcript_area.height),
        );
        render_elicitation_in(
            frame,
            dialog,
            &mut chat.frame_surfaces,
            question_area,
            prompt_focused,
        );
    }
}

/// Draws the one-row footer under the conversation: the reverse-i-search
/// prompt when one is open, else the shared notice, else the hotkey hints
/// for the composer.
pub(super) fn render_chat_footer(
    frame: &mut Frame,
    footer_area: Rect,
    chat: &ChatState,
    prompt_focused: bool,
) {
    // The host only hands the footer to the chat while the composer has
    // focus, so `prompt_focused` is normally true here; the other arm keeps
    // the row honest if it ever is not.
    // The three groups are the dashboard's: what the composer answers, the
    // chords that answer from anywhere, then the function keys. Only the
    // first group changes with what the composer is doing.
    const CHORDS: &[&str] = &["Alt-G panes", "Alt-Q detach"];
    const FUNCTION_KEYS: &str = "F2 palette · F3 workspaces · F4 web · F5 refresh · F1 help";
    let composer_keys = if !prompt_focused {
        "Tab pane · PgUp/PgDn transcript"
    } else if chat.voice_active {
        "Listening… Alt-V stop · PgUp/PgDn transcript"
    } else if !chat.queued_prompts.is_empty() {
        "Up/Ctrl-P edit last queued · Enter send/queue · Ctrl-R history · Shift-Enter newline · Esc cancel"
    } else {
        "Tab pane · Ctrl-V paste · Enter send · Ctrl-R history · Alt-T rendering · Shift-Enter newline"
    };
    let default_footer = fit_footer(composer_keys, CHORDS, FUNCTION_KEYS, footer_area.width);
    let search_footer = chat.history_search.as_ref().map(history_search_footer);
    let notice = chat.notices.current();
    let footer = search_footer
        .as_deref()
        .or(notice.as_deref())
        .unwrap_or(&default_footer);
    // The shared notice bar is yellow wherever it shows; a search prompt or
    // the default hotkey hints stay the quieter dark gray.
    let footer_color = if search_footer.is_none() && notice.is_some() {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(footer_color)),
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

/// The composer's footer row, narrowed to `width` by the dashboard's rule:
/// whole hints go from the composer's own group first, then from the chords,
/// and the function keys are never taken, because they are the way to the
/// palette and the key reference that can name whatever the row dropped.
fn fit_footer(composer_keys: &str, chords: &[&str], functions: &str, width: u16) -> String {
    const HINT: &str = " \u{b7} ";
    const GROUP: &str = " \u{2502} ";
    let mut pane = composer_keys.split(HINT).collect::<Vec<_>>();
    let mut chords = chords.to_vec();
    loop {
        let text = [pane.join(HINT), chords.join(HINT), functions.to_owned()]
            .into_iter()
            .filter(|group| !group.is_empty())
            .collect::<Vec<_>>()
            .join(GROUP);
        if text.chars().count() <= usize::from(width)
            || (pane.pop().is_none() && chords.pop().is_none())
        {
            return text;
        }
    }
}

/// A remembered configuration value, or `None` when it stands for the
/// harness's own default and nothing should be applied.
fn remembered_value(stored: Option<&str>) -> Option<String> {
    stored
        .filter(|value| *value != hel::hel_second_opinion::HARNESS_DEFAULT_VALUE)
        .map(str::to_owned)
}

/// What the agent left running, named for the composer title. One command is
/// worth naming; several are worth counting, with the clock on the oldest.
fn background_work_label(chat: &ChatState) -> Option<String> {
    let commands = &chat.session_activity().background_commands;
    let oldest = commands
        .iter()
        .min_by_key(|command| command.started_at_ms)?;
    let elapsed = crate::usage_format::format_clock(
        hel::clock::epoch_seconds()
            .saturating_sub(u64::try_from(oldest.started_at_ms / 1_000).unwrap_or_default()),
    );
    Some(if commands.len() == 1 {
        format!(
            "Background: {} ({elapsed})",
            super::transcript::compact_terminal_command(&oldest.command)
        )
    } else {
        format!("Background: {} commands, oldest {elapsed}", commands.len())
    })
}

fn prompt_title(chat: &ChatState, queued: usize) -> String {
    let mut parts = [chat.current_model(), chat.current_effort()]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if chat.fast_mode_active() {
        parts.push("Fast".into());
    }
    if chat.plan_mode_active() {
        parts.push("Prompt — PLAN MODE".into());
    } else {
        match chat.phase {
            // An idle session can still have work of its own running: a
            // command the agent backgrounded outlives the turn that started
            // it, and the composer names it rather than reading "Prompt".
            WorkerPhase::Idle => {
                parts.push(background_work_label(chat).unwrap_or_else(|| "Prompt".into()))
            }
            WorkerPhase::Running if chat.pursuing_goal() => parts.push("Pursuing goal".into()),
            WorkerPhase::Running => parts.push("Running".into()),
            WorkerPhase::Closing => parts.push("Closing".into()),
            WorkerPhase::Closed => parts.push("Closed".into()),
        }
    }
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    // Esc cancels a prompt of ours. It cannot stop a turn the harness started
    // on its own, so the hint follows the prompt rather than the phase.
    if chat.prompt_in_flight() {
        parts.push("Esc cancels".into());
    }
    // Auto-review changes what happens when this turn ends, so the composer
    // says it is armed rather than surprising the user with a pane.
    let review = chat.review_config();
    if review.enabled && review.reviewer_profile().is_some() {
        parts.push(format!("review {}", review.tier.label()));
    }
    format!(" {} ", parts.join(" · "))
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
    use hel::hel_elicitation::{ElicitationField, ElicitationFieldKind, ElicitationRequest};
    use hel::hel_transcript::ChatRole;
    use hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST;
    use hel::hel_worker::{SequencedEvent, WorkerEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use std::collections::BTreeMap;

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
                    session_id,
                    execution: hel::hel_worker::RelayExecutionState::Idle,
                    latest_ordinal,
                    latest_digest: latest_digest.clone(),
                    acknowledged_through: latest_ordinal,
                    acknowledged_digest: latest_digest,
                    recovery_floor_ordinal: 0,
                    recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
                    native_session_id: None,
                    agent_capabilities: None,
                    agent_info: None,
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
                },
            }),
            connected: true,
            error: None,
        }
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
        // The question occupies the conversation bands, so the underlying
        // transcript cannot be selected or read through it.
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("UNDERLYING CHAT SENTINEL"))
        );
        assert!(row_of("Tab pane") > popup_top);
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
    fn an_open_elicitation_replaces_only_chat_surfaces_with_its_own() {
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

        // No drag reaches the hidden chat underneath it, while the host can
        // still append navigator and support-pane surfaces around this chat.
        let surfaces = chat.frame_surfaces();
        let message = surfaces
            .surface(SurfaceId::ElicitationMessage)
            .expect("message pane registered");
        assert!(surfaces.surface(SurfaceId::ModalBody).is_some());
        assert!(surfaces.surface(SurfaceId::Transcript).is_none());
        assert!(surfaces.surface(SurfaceId::PromptInput).is_none());
        assert_eq!(
            surfaces
                .surface_at(message.rect.x, message.rect.y)
                .map(|surface| surface.id),
            Some(SurfaceId::ElicitationMessage)
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-dictation",
            72,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-replaced",
            73,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-deferred-review",
            74,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-review-removed",
            76,
        );
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

        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-codex",
            75,
        );
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
        }
    }

    /// The reviewer waterfall offers what the session's own configuration
    /// holds. A chat opened without that context offers nothing, which leaves
    /// `/review` and the second opinion with no harness to run.
    #[tokio::test]
    async fn reviewer_profiles_lists_the_context_profiles_for_the_waterfall() {
        use hel::hel_config::HarnessKind;

        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-profiles",
            80,
        );
        let chat = ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            Some(chat_context(
                "session-profiles",
                &[
                    ("codex-1", HarnessKind::Codex),
                    ("claude-1", HarnessKind::Claude),
                ],
            )),
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

        assert_eq!(
            offered,
            vec![
                ("claude-1".to_owned(), "claude".to_owned()),
                ("codex-1".to_owned(), "codex".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn review_status_configuration_is_applied_on_open_and_refresh() {
        use hel::hel_review::lanes::ReviewTier;

        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-review",
            88,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-checkpoint",
            81,
        );
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

        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-opinion",
            83,
        );
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

        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-alone",
            84,
        );
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

        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-refresh",
            85,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-bare",
            86,
        );
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
    async fn an_active_runtime_record_rearms_a_chat_after_its_handoff_timed_out() {
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-resumed",
            74,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-stop",
            12,
        );
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
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-destroy",
            13,
        );
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
        assert_eq!(buffer[(buffer.area.x, footer_row)].fg, Color::Yellow);

        shared.clear();
        terminal
            .draw(|frame| render_full_frame(frame, &mut chat, false))
            .expect("draw chat");
        let buffer = terminal.backend().buffer();
        let footer_text = (buffer.area.x..buffer.area.right())
            .map(|x| buffer[(x, footer_row)].symbol())
            .collect::<String>();
        assert!(footer_text.contains("Tab pane"), "{footer_text:?}");
        assert_eq!(buffer[(buffer.area.x, footer_row)].fg, Color::DarkGray);
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
            "F2 palette · F3 workspaces · F4 web · F5 refresh · F1 help",
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
            "F2 palette · F3 workspaces · F4 web · F5 refresh · F1 help",
        ] {
            assert!(footer.contains(hint), "{footer:?} omits {hint}");
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

        assert!(rendered.contains("gpt-5.6-sol · high · Running · Esc cancels"));
        // No outer frame wraps the whole session: the transcript's own titled
        // border is the first thing on the frame, not a session title bar.
        assert!(!rendered.contains("HEL /"));
        assert!(rendered.starts_with(" Conversation "), "{rendered:?}");
    }

    #[test]
    fn composer_title_shows_fast_only_while_the_confirmed_mode_is_active() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_config_options(&[fast_mode_option("off")]);
        assert!(!prompt_title(&chat, 0).contains("Fast"));

        chat.set_config_options(&[fast_mode_option("on")]);
        assert_eq!(prompt_title(&chat, 0), " Fast · Prompt ");

        chat.set_config_options(&[]);
        assert!(!prompt_title(&chat, 0).contains("Fast"));
    }

    /// A turn the harness starts on its own also reads as running, and the
    /// relay refuses to cancel it, so the composer offers Esc only while a
    /// prompt of ours is in flight.
    #[test]
    fn composer_title_offers_esc_only_while_a_prompt_of_ours_is_in_flight() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.phase = WorkerPhase::Running;
        assert!(prompt_title(&chat, 0).contains("Running"));
        assert!(!prompt_title(&chat, 0).contains("Esc cancels"));

        chat.set_prompt_in_flight(true);
        assert!(prompt_title(&chat, 0).contains("Esc cancels"));
    }

    /// While the agent is idle, the composer names what it left running
    /// instead of reading "Prompt": one command by name, several by count.
    #[test]
    fn composer_title_names_the_work_the_agent_left_running() {
        let started_at_ms = i64::try_from(hel::clock::epoch_seconds()).unwrap() * 1_000 - 2_616_000;
        let mut chat = ChatState::new(&snapshot(), &[]);
        assert!(prompt_title(&chat, 0).contains("Prompt"));

        chat.set_session_activity(crate::usage_format::SessionActivity {
            execution: None,
            harness_turn_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![hel::hel_worker::BackgroundCommand {
                started_at_ms,
                command: "cargo   test".into(),
            }],
            active_user_shells: Vec::new(),
        });
        assert!(
            prompt_title(&chat, 0).contains("Background: cargo test (43m36s)"),
            "{}",
            prompt_title(&chat, 0)
        );

        chat.set_session_activity(crate::usage_format::SessionActivity {
            execution: None,
            harness_turn_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![
                hel::hel_worker::BackgroundCommand {
                    started_at_ms,
                    command: "cargo test".into(),
                },
                hel::hel_worker::BackgroundCommand {
                    started_at_ms: started_at_ms + 1_000,
                    command: "npm run build".into(),
                },
            ],
            active_user_shells: Vec::new(),
        });
        assert!(
            prompt_title(&chat, 0).contains("Background: 2 commands, oldest 43m36s"),
            "{}",
            prompt_title(&chat, 0)
        );

        // A running turn is named as a turn, whatever it left behind.
        chat.phase = WorkerPhase::Running;
        assert!(prompt_title(&chat, 0).contains("Running"));
    }

    #[test]
    fn composer_title_names_plan_mode_even_during_a_turn() {
        let mut chat = crate::hel_chat::test_support::grok_chat();
        chat.finish_plan_mode_change(true);
        chat.phase = WorkerPhase::Running;

        assert!(prompt_title(&chat, 0).contains("Prompt — PLAN MODE"));
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

        assert!(prompt_title(&chat, 0).contains("Pursuing goal"));
        assert!(!prompt_title(&chat, 0).contains("Running"));

        chat.apply_event(&SequencedEvent {
            seq: 3,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::TurnCompleted,
        });
        assert!(prompt_title(&chat, 0).contains("Prompt"));
        assert!(!prompt_title(&chat, 0).contains("Pursuing goal"));
    }

    #[test]
    fn composer_title_does_not_label_ordinary_or_unadvertised_prompts_as_goals() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.mark_prompt_submitted("/goal ship the release");
        assert!(prompt_title(&chat, 0).contains("Running"));
        assert!(!prompt_title(&chat, 0).contains("Pursuing goal"));

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
        assert!(prompt_title(&chat, 0).contains("Running"));
        assert!(!prompt_title(&chat, 0).contains("Pursuing goal"));
    }

    /// The title names the conversation you are in. The rule around it is
    /// chrome and stays dim; the name draws bright white so it stands out.
    #[test]
    fn the_conversation_title_is_bright_white_and_not_dimmed_with_its_rule() {
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
                Color::White,
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
            Color::DarkGray,
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
        assert!(
            row(16).contains("Prompt"),
            "the prompt's titled border is at the region's top: {:?}",
            row(16)
        );
        // The composer has focus here, so its border is the doubled variant.
        assert!(
            row(20).starts_with('\u{255a}'),
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
            footer: Some(Rect::new(0, 22, 80, 1)),
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
