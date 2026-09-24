use super::*;

/// The complete set of operations a phone may ask the controller to perform.
/// Secret/config editing is intentionally not representable here, and the one
/// destructive variant, `ForceClose`, is not representable on the wire: it is
/// `#[serde(skip)]` so only in-process callers such as the HTTP API can build
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControllerAction {
    TurnControl {
        session_id: String,
        command: mj_core::relay::RelayCommand,
    },
    New {
        #[serde(default)]
        create_managed_worktree: Option<bool>,
        /// Git revision the session starts at, as the caller typed it.
        #[serde(default)]
        launch_base: Option<String>,
        #[serde(default)]
        launch_branch: Option<String>,
        /// None follows the global `[subagents] enabled` setting.
        #[serde(default)]
        mjolnir_subagents: Option<bool>,
        /// Which workspace the session belongs to. Optional on the wire so a
        /// viewer cached from before workspaces reached the phone still parses,
        /// but a controller holding more than one workspace refuses an empty
        /// one rather than guessing.
        #[serde(default)]
        workspace_id: String,
        profile_id: String,
        bundle_id: String,
        target_id: String,
        /// Absent means "derive it", which is what the terminal does.
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        project_directory: Option<PathBuf>,
        /// The repositories the person was shown as having uncommitted changes
        /// and chose to launch over anyway.
        ///
        /// This names them rather than being a bare yes, so an acknowledgement
        /// cannot be replayed against a set the person never saw: if a
        /// different repository has gone dirty since the preflight, the launch
        /// stops and asks again.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dirty_ack: Vec<String>,
    },
    /// Give a session a new title. The terminal calls this a rename.
    Rename {
        session_id: String,
        title: String,
    },
    /// Stop the turn the agent is working on, leaving the session alive. This
    /// is not `Cancel`, which stops a provision, resume or stop.
    InterruptTurn {
        session_id: String,
    },
    /// Change one setting the harness advertised, such as `model` or `effort`.
    SetConfig {
        session_id: String,
        key: String,
        value: String,
    },
    /// Turn plan mode on or off. The harness decides how, which is why this
    /// carries an intent rather than a mode id.
    SetPlanMode {
        session_id: String,
        active: bool,
    },
    RefreshQuota {
        profile_id: String,
    },
    RefreshCapacity {
        target_id: String,
    },
    Resume {
        session_id: String,
        workspace_id: String,
        profile_id: String,
        target_id: String,
        queue: ResumeQueueDisposition,
        /// A failed Move supplies the settings recorded before source
        /// teardown. Ordinary Resume requests leave these absent and retain
        /// the historical inheritance behavior.
        #[serde(default)]
        additional_mounts: Option<Vec<AdditionalMount>>,
        #[serde(default)]
        resource_allocation: Option<SessionResourceAllocation>,
    },
    /// Confirm a previously prepared move. Preparation is a separate
    /// authenticated request so changing the destination cannot be smuggled
    /// into a confirmation from an older browser form.
    Move {
        request: MoveSessionRequest,
    },
    Open {
        session_id: String,
    },
    Prompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        session_id: String,
        text: String,
        /// Images to send with the prompt. The controller turns each one into
        /// the ACP image content block its prompt path already speaks.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ViewerPromptImage>,
    },
    RunShell {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<String>,
        session_id: String,
        command: String,
    },
    CancelShell {
        session_id: String,
        shell_command_id: String,
    },
    Suspend {
        session_id: String,
        #[serde(default)]
        acknowledge_unpublished_work: bool,
    },
    /// Destroy a session without checkpointing it: the live target is torn
    /// down, the recovery archive is removed, and sub-agent children are
    /// destroyed first. This is irreversible.
    ///
    /// Skipped by serde on purpose. The browser viewer posts this enum to
    /// `/actions`, so a wire request must never be able to name this variant;
    /// it is reachable only from the HTTP API, which builds it in process.
    #[serde(skip)]
    Destroy {
        session_id: String,
        /// Whether the managed worktree's branch goes with the session.
        /// Destruction keeps it unless the request asks for the deletion.
        delete_branch: bool,
    },
    Cancel {
        session_id: String,
    },
    /// Review the turn this session just finished.
    StartReview {
        session_id: String,
    },
    /// Forward the findings, dismiss them, or cancel the open review.
    ResolveReview {
        session_id: String,
        /// `forward`, `dismiss`, or `cancel`.
        resolution: String,
    },
    RemoveQueuedPrompt {
        session_id: String,
        queue_id: String,
    },
    /// Answer one of the session's pending form questions.
    RespondElicitation {
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    },
}

/// One image a phone attached to a prompt. Legacy callers may send inline
/// base64 data; the server normalizes it into an attachment before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerPromptImage {
    /// Legacy inline image bytes. New browser uploads and normalized inline
    /// prompts carry an attachment reference and leave this empty.
    #[serde(default)]
    pub data_base64: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// Session-scoped, immutable image bytes. The worker resolves this just
    /// before dispatch, keeping browser actions and durable commands small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentRef>,
}

/// The controller's answer to one phone action.
///
/// The answer means "accepted", not "finished": provisioning, resume and close
/// run for minutes, and a phone on a mobile network drops a request held open
/// that long. How the action then goes travels in snapshots — session state,
/// queued prompts, transcripts, and `has_error`.
///
/// Only the outcome crosses this boundary. The controller's own failure text
/// names profile homes, project paths and SSH hosts, so it stays on the
/// controller. A caller therefore gets one of two things: a [`Refusal`], whose
/// sentence was written for it at the place the failure was produced, or a
/// generic internal failure carrying a reference that also appears in the
/// daemon log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Admitted and now running; watch the snapshot for what happens next.
    ///
    /// A `new` action carries the published session id, which is the only way
    /// its caller learns what it just created.
    Accepted { session_id: Option<String> },
    /// The controller already runs as many phone actions as it allows.
    Busy,
    /// This session already has an operation running.
    SessionBusy,
    /// A cancel found no operation to cancel.
    NotCancellable,
    /// The action was refused for a reason the caller can act on, and the
    /// refusal says what it is.
    Refused(Refusal),
    /// The controller could not start the action, for a reason that stays
    /// server-side. `reference` is logged with the failure, so the person who
    /// owns the daemon can find the entry that explains it.
    Failed { reference: String },
}

impl ActionOutcome {
    /// Admitted, with no session id to report.
    pub const fn accepted() -> Self {
        Self::Accepted { session_id: None }
    }

    /// The published session id, when this outcome carries one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Accepted { session_id } => session_id.as_deref(),
            _ => None,
        }
    }

    /// The reply an outcome owes the phone, or `None` when it was accepted.
    pub(super) fn rejection(&self) -> Option<ApiError> {
        match self {
            Self::Accepted { .. } => None,
            Self::Busy => Some(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "the controller is at its concurrent action limit (a session that is still starting holds an action until it is ready); retry shortly",
            )),
            Self::SessionBusy => Some(ApiError::new(
                StatusCode::CONFLICT,
                "another operation is already running for this session",
            )),
            Self::NotCancellable => Some(ApiError::new(
                StatusCode::CONFLICT,
                "the session has no cancellable operation",
            )),
            // A refusal is a precondition the caller can fix, so it answers
            // 4xx with the sentence written for it: 409 for a state that has
            // to change first, 422 for a request naming something unusable.
            Self::Refused(refusal) => Some(ApiError::new(
                match refusal.kind() {
                    RefusalKind::Precondition => StatusCode::CONFLICT,
                    RefusalKind::Unusable => StatusCode::UNPROCESSABLE_ENTITY,
                },
                refusal.message().to_owned(),
            )),
            Self::Failed { reference } => Some(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "the controller could not start this action; \
                     the daemon log records the reason under reference {reference}"
                ),
            )),
        }
    }
}

#[derive(Debug)]
pub struct ControllerRequest {
    pub action: ControllerAction,
    pub reply: tokio::sync::oneshot::Sender<ActionOutcome>,
}

/// A phone request to create or reuse a quick project bundle. This has its
/// own channel because bundle creation returns a durable id and must publish a
/// config snapshot before the HTTP request can succeed; [`ControllerAction`]
/// intentionally carries only action admission outcomes.
#[derive(Debug)]
pub struct BundleRequest {
    pub source: String,
    pub reply: tokio::sync::oneshot::Sender<Result<String, BundleFailure>>,
}

/// Safe failure classes for bundle creation. Detailed controller errors stay
/// in daemon logs; a browser only needs to know whether to fix its source or
/// report a server-side failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleFailure {
    InvalidSource,
    Controller,
}

/// A phone acknowledging how far it has read a conversation.
///
/// This deliberately is not a `ControllerAction`: the viewer posts it after
/// every conversation fetch, and a fetch follows every revision. Routing it
/// through the action pipeline made each receipt reload the controller, bump
/// the revision and broadcast a snapshot, which triggered the next fetch, so
/// viewer and controller never went quiet; it also consumed the session's
/// single action slot, intermittently rejecting real actions. A receipt
/// therefore travels on its own channel and only persists one cursor field.
/// A phone asking whether a session it is about to create would launch
/// cleanly, and which network sources it will use first.
///
/// This is not a `ControllerAction`: it starts nothing, it takes no session
/// slot, and it must answer before the person has decided anything. It also
/// needs the controller, because resolving a local repository's configured
/// remotes is a fact about the disk rather than about the projection.
///
/// Resume preflights share this channel, and so the concurrency cap on it,
/// because they do the same kind of work on the same disk.
#[derive(Debug)]
pub enum PreflightRequest {
    New(NewPreflightRequest),
    Resume(ResumePreflightRequest),
    CompletePath(PathCompletionRequest),
}

/// A browser asking what a half-typed path could be. It shares the preflight
/// channel because it does the same kind of work: one short-lived, cancellable
/// look at a local or remote filesystem, under the same concurrency cap.
#[derive(Debug)]
pub struct PathCompletionRequest {
    pub host: CompletionHost,
    pub prefix: String,
    pub kind: CompletionKind,
    pub reply: tokio::sync::oneshot::Sender<Result<PathCompletion, String>>,
}

#[derive(Debug)]
pub struct NewPreflightRequest {
    pub bundle_id: String,
    pub target_id: String,
    pub project_directory: Option<PathBuf>,
    pub remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    pub reply: tokio::sync::oneshot::Sender<Result<PreflightNew, PreflightFailure>>,
}

/// A resume preflight for one stopped session and one destination target. It
/// travels on the same channel and under the same concurrency cap as the
/// new-session preflight because it does the same kind of work: reading a
/// working tree and asking a remote about itself.
#[derive(Debug)]
pub struct ResumePreflightRequest {
    pub session_id: String,
    pub target_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<PreflightResume, PreflightFailure>>,
}

/// What a resume preflight found.
///
/// `Ready` covers every resume that changes nothing about where repository
/// content comes from. `ConvertingRawCheckout` means this resume moves a
/// local checkout into an isolated workspace, and carries the preview the
/// person has to confirm. `Unavailable` reports why the conversion cannot be
/// planned, in the plan's own words, because that message says what to do
/// about it (add a remote, commit a submodule) and the browser has no other
/// way to learn it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PreflightResume {
    Ready,
    ConvertingRawCheckout {
        preview: Box<mj_core::state::RawConversionPreview>,
    },
    Unavailable {
        detail: String,
    },
}

/// A move preparation is intentionally separate from action admission. It
/// performs read-only compatibility checks and returns the exact fingerprint
/// the later confirmation must echo; it never interrupts the source session.
#[derive(Debug)]
pub struct MovePreparationRequest {
    pub selection: MoveSelection,
    pub reply: tokio::sync::oneshot::Sender<Result<MovePreparation, String>>,
}

/// A preflight can fail because the requested bare directory is unusable, an
/// isolated repository lacks a usable network source, or the controller-side
/// check itself could not complete. The HTTP surface keeps those outcomes
/// distinct without carrying filesystem, Git, or SSH details to the phone.
#[derive(Debug)]
pub enum PreflightFailure {
    Validation,
    /// A configured isolated-session repository cannot be used as a network
    /// source. The detail is safe for the phone and tells the person how to
    /// choose the supported raw-local path instead.
    InvalidRepository(String),
    Controller(String),
}

/// One configured repository's network clone and publication destinations.
/// URLs have already been passed through the shared display sanitizer before
/// they reach a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightRepository {
    pub id: String,
    pub fetch_url: String,
    pub default_branch: String,
    pub push_urls: Vec<String>,
}

/// What a preflight found. Isolated sessions expose their complete network
/// source plan so the person can review it before creation. Raw-local targets
/// leave the plan empty because they use the selected checkout directly;
/// isolated targets set `local_changes_excluded` to make the copy boundary
/// explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightNew {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub managed_worktree: mj_core::state::ManagedWorktreeOptions,
    #[serde(default)]
    pub remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    #[serde(default)]
    pub dirty_repositories: Vec<String>,
    #[serde(default)]
    pub remote_repositories: Vec<PreflightRepository>,
    pub local_changes_excluded: bool,
}

/// What a phone asks about, or stores against, its own identity.
///
/// These travel on their own channel rather than as actions, for the reason a
/// read receipt does: they are frequent, they start nothing, and routing them
/// through the action pipeline would consume the session's single action slot
/// and reload the controller on every keystroke.
#[derive(Debug)]
pub enum ClientStateRequest {
    Read {
        client_id: String,
        session_id: String,
        reply: tokio::sync::oneshot::Sender<Result<ViewerClientState, String>>,
    },
    SaveDraft {
        client_id: String,
        session_id: String,
        draft: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    MarkWorkspaceRead {
        client_id: String,
        workspace_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    History {
        session_id: String,
        query: String,
        scope: String,
        reply: tokio::sync::oneshot::Sender<Result<ViewerPromptHistory, String>>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerClientState {
    pub draft: String,
    pub through_event_ordinal: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerPromptHistory {
    pub entries: Vec<String>,
    /// Whether the search stopped before it ran out of history, so a phone can
    /// say the answer is partial rather than presenting it as complete.
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ReadReceiptRequest {
    pub client_id: String,
    pub session_id: String,
    pub through: u64,
    pub reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// A phone request to stop one currently projected background task.
///
/// This is intentionally not a [`ControllerAction`]. The request is already
/// validated against the current operational snapshot by the HTTP handler,
/// then the controller resolves the live session handle and waits for the
/// provider acknowledgement in a supervised task.
#[derive(Debug)]
pub struct BackgroundTaskStopRequest {
    pub session_id: String,
    pub background_task_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<(), BackgroundTaskStopFailure>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStopFailure {
    /// The session manager could not resolve the live session handle.
    SessionUnavailable,
    /// The provider or relay rejected the stop request.
    Provider,
    /// The stop task itself failed before reaching the provider.
    Internal,
}
