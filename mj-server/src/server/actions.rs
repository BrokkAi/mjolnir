use super::*;

pub fn is_turn_control_command(command: &mj_core::relay::RelayCommand) -> bool {
    matches!(
        command,
        mj_core::relay::RelayCommand::Steer { .. }
            | mj_core::relay::RelayCommand::ApplyQueuedFor { .. }
            | mj_core::relay::RelayCommand::CancelTurnFor { .. }
            | mj_core::relay::RelayCommand::CancelTurnTarget { .. }
            | mj_core::relay::RelayCommand::ResolveSteering { .. }
    )
}

/// The complete set of operations a phone may ask the controller to perform.
/// Secret/config editing is intentionally not representable here: the
/// signed-in viewer edits configuration through its own Settings routes
/// (`settings.rs`), which never send a literal secret to the browser. The one
/// destructive variant, `ForceClose`, is not representable on the wire: it is
/// `#[serde(skip)]` so only in-process callers such as the HTTP API can build
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
// `SessionReview` includes the deprecated string compatibility field. Keep
// this public action's Rust construction shape inline; boxing would change it.
#[allow(clippy::large_enum_variant)]
pub enum ControllerAction {
    PushSubscribe {
        subscription: mj_store::web_push::Subscription,
    },
    PushRenew {
        old_endpoint: String,
        subscription: mj_store::web_push::Subscription,
    },
    PushUnsubscribe {
        endpoint: String,
    },
    PushTest {
        endpoint: String,
    },
    DismissNotification {
        session_id: String,
        target: mj_core::attention::NotificationDismissal,
    },
    SetSessionNotificationsIgnored {
        session_id: String,
        ignored: bool,
    },
    RecordOperatorActivity,
    SetOperatorAway {
        away: bool,
    },
    TurnControl {
        session_id: String,
        command: mj_core::relay::RelayCommand,
    },
    New {
        #[serde(skip)]
        remember_launch_choices: bool,
        /// Save this session's selected container size as the default for its
        /// physical host. This is independent of the interactive profile,
        /// target, and recent-launch defaults.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        remember_container_size: bool,
        #[serde(default)]
        create_managed_worktree: Option<bool>,
        /// Full commit object ID to start the bundle's primary repository at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
        /// With `at`, the new branch created there; otherwise an existing branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// Diff base; defaults to `at`. Without `at`, also the starting
        /// revision for a raw managed worktree.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        /// With `at`, start with `at`, `base` and their ancestors only.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_later_history: bool,
        /// Omitted uses the selected profile's subagent setting.
        #[serde(default)]
        subagents: Option<mj_core::subagent::SubagentPolicy>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oracle: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oracle_choice: Option<mj_core::config::SubagentChoice>,
        /// Review for this session. Omitted follows `[review]`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_session_review")]
        review: Option<mj_core::config::SessionReview>,
        /// Start the session with no project memory.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_project_memory: bool,
        /// Exclude this session from GitHub watcher delivery targets.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_github_events: bool,
        /// Withhold GitHub credentials; inherited by children.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        no_github_credentials: bool,
        /// Start with notifications ignored.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        notifications_ignored: bool,
        /// Which workspace the session belongs to. Optional on the wire so a
        /// viewer cached from before workspaces reached the phone still parses,
        /// but a controller holding more than one workspace refuses an empty
        /// one rather than guessing.
        #[serde(default)]
        workspace_id: String,
        #[serde(default, deserialize_with = "deserialize_null_as_empty")]
        profile_id: String,
        bundle_id: String,
        target_id: String,
        /// Host directories attached when the session is first provisioned.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        additional_mounts: Vec<AdditionalMount>,
        /// Explicit container or EC2 sizing selected in the create wizard.
        /// Omitted preserves the historical target defaults for older clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_allocation: Option<Box<mj_core::state::SessionResourceAllocation>>,
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
    /// Move a session, and the sub-agents under it, to another workspace.
    ChangeWorkspace {
        session_id: String,
        workspace_id: String,
    },
    /// Save settings for the next creation or preview a preserving replacement.
    SetContainerSettings {
        session_id: String,
        #[serde(default)]
        resource_allocation: Option<SessionResourceAllocation>,
        #[serde(default)]
        mounts: Vec<AdditionalMount>,
    },
    /// Suspend the session if it is live, then resume it with the profile,
    /// target and mounts it last ran with.
    Restart {
        session_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        acknowledge_container_recreation: bool,
    },
    UpgradeContainer {
        request: mj_core::state::ContainerUpgradeRequest,
    },
    /// Stop the turn this session is working on and the turns of every
    /// sub-agent under it, leaving the sessions alive.
    InterruptAll {
        session_id: String,
    },
    RefreshQuota {
        profile_id: String,
    },
    CheckAvailability,
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
        request: Box<MoveSessionRequest>,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<mj_core::attachment::FileAttachmentRef>,
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
        discard_untracked: mj_core::suspend_consent::UntrackedConsent,
        #[serde(default)]
        abort_operations: mj_core::suspend_consent::OperationConsent,
        #[serde(default)]
        without_checkpoint: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        confirm_no_checkpoint: bool,
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
    },
    Cancel {
        session_id: String,
    },
    CancelContainerUpgrade {
        session_id: String,
    },
    /// Review the turn this session just finished.
    StartReview {
        session_id: String,
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
    /// its caller learns what it just created. `notice` is something the
    /// caller should tell the person about what was created, such as review
    /// being turned off for it.
    Accepted {
        session_id: Option<String>,
        notice: Option<String>,
    },
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
        Self::Accepted {
            session_id: None,
            notice: None,
        }
    }

    /// The published session id, when this outcome carries one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Accepted { session_id, .. } => session_id.as_deref(),
            _ => None,
        }
    }

    /// The reply an outcome owes the phone, or `None` when it was accepted.
    pub(super) fn rejection(&self) -> Option<ApiError> {
        match self {
            Self::Accepted { .. } => None,
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
            Self::Refused(refusal) => Some(
                ApiError::new(
                    match refusal.kind() {
                        RefusalKind::Precondition => StatusCode::CONFLICT,
                        RefusalKind::Unusable => StatusCode::UNPROCESSABLE_ENTITY,
                    },
                    refusal.message().to_owned(),
                )
                .with_code(refusal.code()),
            ),
            Self::Failed { reference } => Some(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                lifecycle_failure_message(
                    "the controller could not start this action",
                    None,
                    None,
                    reference,
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

#[derive(Debug, Serialize)]
pub struct CreateBundleResponse {
    pub bundle_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<mj_core::project_catalog::SavedProject>,
}

/// Safe failure classes for bundle creation. Detailed controller errors stay
/// in daemon logs; a browser only needs to know whether to fix its source or
/// report a server-side failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleFailure {
    InvalidSource,
    Refused(Refusal),
    Controller,
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

/// A preflight can fail because the requested bare directory is unusable, an
/// isolated repository lacks a usable network source, or the controller-side
/// check itself could not complete. The HTTP surface keeps those outcomes
/// distinct without carrying filesystem, Git, or SSH details to the phone.
#[derive(Debug)]
pub enum PreflightFailure {
    Terminal(String),
    InvalidMount,
    Validation,
    /// A configured isolated-session repository cannot be used as a network
    /// source. The detail is safe for the phone and tells the person how to
    /// choose the supported raw-local path instead.
    InvalidRepository(String),
    Controller(String),
}

pub use mj_client::daemon::{PreflightNew, PreflightRepository};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStopFailure {
    /// The session manager could not resolve the live session handle.
    SessionUnavailable,
    /// The provider or relay rejected the stop request.
    Provider,
    /// The stop task itself failed before reaching the provider.
    Internal,
}

/// What a public request that asks for per-turn review is told. Stored
/// actions that still say `turn` read as `push` instead.
const TURN_REVIEW_REMOVED: &str = "Per-turn review was removed. Use review.mode \"push\", or omit review to review before push when reviewers are configured.";

fn deserialize_null_as_empty<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

pub(super) fn deserialize_session_review<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<mj_core::config::SessionReview>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    value
        .map(|value| {
            if !matches!(
                value.get("mode").and_then(serde_json::Value::as_str),
                Some("inherit" | "off" | "turn" | "push")
            ) {
                return Err(serde::de::Error::custom(
                    "review.mode must be inherit, off, turn, or push",
                ));
            }
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        })
        .transpose()
}

/// Public requests accept only current reviewer fields. The durable action
/// reader above still loads an accepted legacy request after an upgrade.
pub(super) fn deserialize_api_session_review<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<mj_core::config::SessionReview>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    value
        .map(|value| {
            let mode = value.get("mode").and_then(serde_json::Value::as_str);
            if mode == Some("turn") {
                return Err(serde::de::Error::custom(TURN_REVIEW_REMOVED));
            }
            if !matches!(mode, Some("inherit" | "off" | "push")) {
                return Err(serde::de::Error::custom(
                    "review.mode must be inherit, off, or push",
                ));
            }
            let mode = value
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let object = value
                .as_object()
                .ok_or_else(|| serde::de::Error::custom("review must be an object"))?;
            if object
                .keys()
                .any(|key| key != "mode" && (mode == "off" || key != "reviewers"))
            {
                return Err(serde::de::Error::custom(
                    "review accepts only mode and reviewers",
                ));
            }
            serde_json::from_value(value).map_err(serde::de::Error::custom)
        })
        .transpose()
}

/// Render only trusted boundary guidance; raw diagnostic chains stay in logs.
pub fn lifecycle_failure_message(
    prefix: &str,
    refusal: Option<&mj_core::refusal::Refusal>,
    reason: Option<mj_session_types::LifecycleFailureReason>,
    reference: &str,
) -> String {
    let reason = if let Some(refusal) = refusal {
        format!("{refusal}. Follow this guidance, then retry the action.")
    } else if let Some(reason) = reason {
        reason.to_string()
    } else {
        "The lifecycle operation could not finish. Retry the action; if the runtime is unavailable, Resume the session to repair it first.".into()
    };
    format!("{prefix}: {reason} Reference {reference}.")
}

#[cfg(test)]
mod tests {
    use super::ControllerAction;
    use mj_core::state::SessionResourceAllocation;

    #[test]
    fn new_action_resource_allocation_round_trips_and_remains_optional() {
        let with_allocation = serde_json::json!({
            "action": "new",
            "profile_id": "codex",
            "bundle_id": "project",
            "target_id": "podman",
            "remember_container_size": true,
            "resource_allocation": {
                "kind": "container",
                "cpus": 4,
                "memory_bytes": 8 * 1024 * 1024 * 1024_u64,
            },
        });
        let action: ControllerAction = serde_json::from_value(with_allocation).unwrap();
        assert!(matches!(
            &action,
            ControllerAction::New {
                remember_container_size: true,
                resource_allocation: Some(allocation),
                ..
            } if matches!(allocation.as_ref(), SessionResourceAllocation::Container {
                cpus: 4,
                memory_bytes: 8_589_934_592,
            })
        ));
        let encoded = serde_json::to_value(&action).unwrap();
        assert_eq!(
            serde_json::from_value::<ControllerAction>(encoded).unwrap(),
            action
        );

        let older_client: ControllerAction = serde_json::from_value(serde_json::json!({
            "action": "new",
            "profile_id": "codex",
            "bundle_id": "project",
            "target_id": "podman",
        }))
        .unwrap();
        assert!(matches!(
            &older_client,
            ControllerAction::New {
                remember_container_size: false,
                resource_allocation: None,
                ..
            }
        ));
        assert!(
            serde_json::to_value(older_client)
                .unwrap()
                .get("resource_allocation")
                .is_none()
        );
    }
}
