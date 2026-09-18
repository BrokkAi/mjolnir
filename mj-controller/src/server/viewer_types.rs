use super::*;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSnapshot {
    pub revision: u64,
    pub generated_at: String,
    /// Unix time in milliseconds, refreshed when serving the projection.
    /// Clients use this as the clock for live activity cards.
    #[serde(default)]
    pub server_time_ms: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<ViewerWorkspace>,
    pub sessions: Vec<ViewerSession>,
    pub profiles: Vec<ViewerProfile>,
    pub targets: Vec<ViewerTarget>,
    pub bundles: Vec<ViewerBundle>,
    /// The bounded part of `[review]` needed to report whether review is
    /// armed. Reviewer model and effort remain controller-private.
    #[serde(default)]
    pub review_config: ViewerReviewConfig,
    /// The global `[subagents] enabled` setting. The new-session form uses it
    /// as the default for its per-session sub-agent checkbox.
    #[serde(default)]
    pub subagents_enabled: bool,
    /// One entry per host or fleet that can be probed. Empty until the phone
    /// server's capacity poller has published a reading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity: Vec<ViewerTargetCapacity>,
    /// Recent failed launches, independent of provisional session rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_failures: Vec<ViewerLaunchFailure>,
}

/// Carries the launch failure's reason so a client can show why a session
/// never came up. The reason is the provisioning error chain, the same text
/// the session's `last_error` already publishes through `mj events`; it is not
/// the full local diagnostic file, which can hold credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewerLaunchFailure {
    /// Identifies the notice itself, so the browser can dismiss one. It is not
    /// a session id.
    pub id: String,
    pub workspace_id: String,
    /// The session the failed launch was for, when one had been published.
    /// Absent when the launch failed before any session record existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Why the launch failed, when the action recorded a reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ViewerSnapshot {
    /// Build the public projection. In particular, this never copies profile
    /// homes/environment, SSH hosts/keys, container environment, AWS details,
    /// concrete resource locators, native session IDs, or raw error strings.
    pub fn from_config_state(config: &Config, state: &AppState, revision: u64) -> Self {
        let sessions = state
            .sessions
            .values()
            .map(|session| {
                let incompatible = config
                    .targets
                    .keys()
                    .filter(|target_id| {
                        crate::controller::resume_compatibility(session, config, target_id).is_err()
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let lifecycle = ViewerLifecycleCategory::of(session.state);
                // A sub-agent child works in its parent's checkout and owns no
                // worktree, so its project identity has to come from the
                // parent; its own record would name the parent's session id.
                let project = state.project_identity_session(session);
                let source = project.project_source(config);
                let subagent = state.subagents.get(&session.id);
                let subagent_session_ids = state
                    .subagents
                    .values()
                    .filter(|child| child.parent_session_id == session.id)
                    .map(|child| child.child_session_id.clone())
                    .collect();
                ViewerSession {
                    capacity_retry: None,
                    id: session.id.clone(),
                    workspace_id: session.workspace_id.clone(),
                    title: session.display_title().to_owned(),
                    subagent_parent_id: subagent.map(|child| child.parent_session_id.clone()),
                    subagent_task_name: subagent.map(|child| child.task_name.clone()),
                    subagent_session_ids,
                    harness_kind: session.harness_kind.id().into(),
                    profile_id: session.last_profile.clone(),
                    bundle_id: session.bundle_id.clone(),
                    target_id: session.target_template_id.clone(),
                    state: session.state.as_str().into(),
                    created_at: session.created_at.clone(),
                    updated_at: session.updated_at.clone(),
                    has_error: session.last_error.is_some()
                        || session.configuration_issue(config).is_some(),
                    configuration_issue: session.configuration_issue(config),
                    // A session that failed to launch (or a close that left it
                    // dead) carries its reason here so a client need not open
                    // the local diagnostic to learn why. A failed resume rolls
                    // the record back to stopped and leaves its reason in the
                    // same field, so that state reports it too; every
                    // successful transition clears `last_error`, so this never
                    // reports a failure the session has since recovered from.
                    //
                    // A live session's `last_error` is not published here: it
                    // can hold a raw provisioning chain naming profile homes
                    // and SSH hosts. A failed close leaves the session alive
                    // and still owes the person a reason, so the sentence the
                    // controller composed for them is published whatever state
                    // the session is in (#1081).
                    launch_error: matches!(
                        session.state,
                        SessionState::Error | SessionState::Stopped
                    )
                    .then(|| session.last_error.clone())
                    .flatten()
                    .or_else(|| session.public_error().map(str::to_owned)),
                    preview: Vec::new(),
                    queued_prompts: Vec::new(),
                    active_user_shells: Vec::new(),
                    background_tasks: Vec::new(),
                    pending_elicitations: Vec::new(),
                    conversation_available: false,
                    prompt_images_supported: false,
                    incompatible_resume_targets: incompatible.clone(),
                    compatible_resume_targets: config
                        .targets
                        .keys()
                        .filter(|target_id| !incompatible.contains(*target_id))
                        .cloned()
                        .collect(),
                    project_label: source.short,
                    project_key: project_key(&source.key),
                    display_location: project.project_target(config, &session.target_template_id),
                    lifecycle,
                    transitioning: session.state.transition_kind().is_some(),
                    latest_event_ordinal: 0,
                    last_activity_at_ms: None,
                    activity_details: None,
                    activity: String::new(),
                    operation: None,
                    move_recovery: None,
                    // Both are replaced for every session by the phone
                    // projection, from the one shared activity state.
                    chat_phase: ViewerChatPhase::default(),
                    is_idle: false,
                    activity_state: None,
                    config_options: Vec::new(),
                    plan_mode_active: None,
                    turn_review: None,
                    available_commands: Vec::new(),
                    // What the durable record alone can justify. The phone server
                    // widens these once it knows whether the session manager holds
                    // the session and what the agent has advertised.
                    capabilities: ViewerSessionCapabilities {
                        open: false,
                        prompt: false,
                        run_shell: false,
                        cancel_turn: false,
                        cancel_operation: false,
                        stop: lifecycle.is_dashboard_visible(),
                        rename: true,
                        resume: !lifecycle.is_dashboard_visible(),
                        move_session: false,
                        set_config: false,
                        set_plan_mode: false,
                    },
                }
            })
            .collect();
        let profiles = config
            .enabled_profiles()
            .map(|(id, profile)| ViewerProfile {
                id: id.to_owned(),
                harness_kind: profile.kind.id().into(),
                quota: None,
            })
            .collect();
        let targets = config
            .targets
            .iter()
            .map(|(id, target)| ViewerTarget {
                id: id.clone(),
                kind: target.kind_name().into(),
                requires_project_directory: matches!(
                    target,
                    TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
                ),
                recent_project_directories: project_history_host(target)
                    .map(|host| {
                        state
                            .project_directories(host)
                            .iter()
                            .map(|directory| directory.to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        let bundles = config
            .bundles
            .iter()
            .map(|(id, bundle)| ViewerBundle {
                id: id.clone(),
                primary_repository: bundle.primary_repo.clone(),
                repositories: bundle
                    .repositories
                    .iter()
                    .map(|repository| ViewerRepository {
                        id: repository.id.clone(),
                        github: repository.github.clone(),
                        destination: repository.destination.to_string_lossy().into_owned(),
                    })
                    .collect(),
            })
            .collect();
        Self {
            revision,
            generated_at: now_unix().to_string(),
            server_time_ms: mj_core::clock::epoch_millis(),
            workspaces: Vec::new(),
            sessions,
            profiles,
            targets,
            bundles,
            review_config: ViewerReviewConfig {
                enabled: config.review.enabled,
                tier: config.review.tier.label().to_owned(),
                profile: config.review.profile.clone(),
            },
            subagents_enabled: config.subagents.enabled,
            capacity: Vec::new(),
            launch_failures: Vec::new(),
        }
    }
}

/// A stable, opaque grouping key for a project.
///
/// The controller's own project identity is a bundle, filesystem path, or Git
/// remote, and this projection publishes neither. A digest groups exactly as
/// well and says nothing: two sessions in the same project share a key, and a
/// key on its own reveals no source.
pub(super) fn project_key(identity: &str) -> String {
    use sha2::Digest as _;
    let digest = Sha256::digest(identity.as_bytes());
    mj_core::hex::lower_hex(&digest[..8])
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<mj_core::relay::CapacityRetry>,
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    pub title: String,
    /// Parent ownership for a borrowed-target child session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_parent_id: Option<String>,
    /// The stable task label chosen by the parent when it spawned this child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_task_name: Option<String>,
    /// Direct children of this parent. Children are deliberately never nested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_session_ids: Vec<String>,
    pub harness_kind: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_id: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub has_error: bool,
    /// Public identifiers and repair guidance only; never raw runtime errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_issue: Option<String>,
    /// Why a launch failed, for a session that ended in the error state. This
    /// is the same provisioning error text `last_error` already publishes
    /// through `mj events`, surfaced here so `mj sessions`/`mj wait` can show
    /// the reason instead of a bare "failed to launch".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_error: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<ViewerQueuedPrompt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_user_shells: Vec<ViewerUserShell>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<ViewerBackgroundTask>,
    /// Form questions the session is blocked on, published so a phone can
    /// answer them. These are the agent's own questions, already visible in
    /// the transcript, so they travel whole rather than redacted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<ElicitationRequest>,
    pub conversation_available: bool,
    /// Whether this session's agent advertised support for image content in
    /// prompts. The viewer offers the image controls only when it did, and the
    /// server refuses images for a session that did not.
    #[serde(default)]
    pub prompt_images_supported: bool,
    /// Target ids this session cannot resume on. Only the ids travel: the
    /// controller's reasons name project paths and SSH hosts, which this
    /// projection deliberately keeps on the controller.
    ///
    /// Retained beside `compatible_resume_targets` so a viewer cached from
    /// before that field existed keeps working through a deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incompatible_resume_targets: Vec<String>,
    /// Target ids this session can resume on, so the browser never has to
    /// subtract one set from another to find out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatible_resume_targets: Vec<String>,
    /// The canonical short source label for this session: a bundle name, path
    /// leaf, or repository name, never a source path itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_label: String,
    /// A stable key for grouping sessions by project. The controller's own
    /// source identity stays private, so what travels is a digest of it:
    /// enough to group by, and nothing to read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_key: String,
    /// The configured target's human-facing project location. This is the
    /// same target projection the terminal uses while a session is running.
    #[serde(default)]
    pub display_location: String,
    pub lifecycle: ViewerLifecycleCategory,
    /// A lifecycle transition temporarily owns this session's conversation.
    /// This remains separate from the coarse lifecycle category so Move can
    /// hide the old transcript while its durable record is still `Running`.
    #[serde(default)]
    pub transitioning: bool,
    /// How far the controller's projection of this session has advanced. A
    /// phone compares it against its own read frontier to know what is unread,
    /// without fetching a transcript to find out.
    #[serde(default)]
    pub latest_event_ordinal: u64,
    /// Durable relay receipt watermark from the materialized projection.
    /// It remains absent when the background snapshot pipeline has not yet
    /// delivered a projection for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<i64>,
    /// Structured live activity, absent when no operational relay snapshot is
    /// available for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_details: Option<ViewerActivityDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<ViewerOperation>,
    /// Safe recovery choices for a failed or cancelled Move. Diagnostics and
    /// checkpoint paths remain on the controller; this contains only the
    /// settings a person may choose again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_recovery: Option<ViewerMoveRecovery>,
    #[serde(default)]
    pub chat_phase: ViewerChatPhase,
    /// Known live activity is idle: no foreground turn, tool, or background work.
    /// Missing operational state must not be presented as confirmed idle.
    #[serde(default)]
    pub is_idle: bool,
    /// What this session is doing, in the shared vocabulary every part of
    /// Mjolnir now uses. Richer than `chat_phase`, which has only four values
    /// and must keep them: this can also say that the daemon cannot see the
    /// worker and report what was last known about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_state: Option<mj_core::activity::ActivityState>,
    /// What this session is doing, in the words the dashboard row uses:
    /// `Turn 43m36s  Step 12s`, `BG 43m36s`, or `[idle]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// The settings the harness advertised, with the values it accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<ViewerConfigOption>,
    /// Whether plan mode is on, or `None` when this harness has no plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_active: Option<bool>,
    /// The review the daemon is running for this session, if any. A phone
    /// renders the same review the terminal does and resolves it the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_review: Option<ViewerTurnReview>,
    /// The Mjolnir commands this session accepts, published rather than hardcoded
    /// in the browser: a command list kept in two places is a command list that
    /// drifts, which is how `/review` went missing from the phone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_commands: Vec<ViewerMjCommand>,
    pub capabilities: ViewerSessionCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerMoveRecovery {
    pub operation_id: String,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub destination_profile_id: String,
    pub destination_target_template_id: String,
    pub phase: String,
    pub queue: String,
    pub clear_resource_allocation: bool,
    /// The source settings are retained so Resume cannot silently inherit a
    /// partially converted destination record after a failed Move.
    #[serde(default)]
    pub source_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub source_resource_allocation: Option<SessionResourceAllocation>,
    /// The exact destination settings are needed when a queue admission
    /// checkpoint pins retry to the already-provisioned destination.
    #[serde(default)]
    pub destination_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub destination_resource_allocation: Option<SessionResourceAllocation>,
    pub checkpoint_retained: bool,
    pub destination_ready: bool,
    pub queue_admission_started: bool,
    pub queue_admission_finished: bool,
}

impl ViewerMoveRecovery {
    #[must_use]
    pub fn from_operation(operation: &MoveOperation) -> Option<Self> {
        if matches!(operation.phase, MovePhase::Completed) {
            return None;
        }
        Some(Self {
            operation_id: operation.operation_id.clone(),
            source_profile_id: operation.source_profile_id.clone(),
            source_target_template_id: operation.source_target_template_id.clone(),
            destination_profile_id: operation.selection.profile_id.clone().unwrap_or_default(),
            destination_target_template_id: operation
                .selection
                .target_template_id
                .clone()
                .unwrap_or_default(),
            phase: match operation.phase {
                MovePhase::Preparing => "preparing",
                MovePhase::ClosingSource => "closing_source",
                MovePhase::ResumingDestination => "resuming_destination",
                MovePhase::StartingQueue => "starting_queue",
                MovePhase::Completed => "completed",
                MovePhase::Failed => "failed",
                MovePhase::Cancelled => "cancelled",
            }
            .into(),
            queue: match operation.queue {
                ResumeQueueDisposition::Start => "start",
                ResumeQueueDisposition::Discard => "discard",
            }
            .into(),
            clear_resource_allocation: operation.selection.clear_resource_allocation,
            source_additional_mounts: operation.source_additional_mounts.clone(),
            source_resource_allocation: operation.source_resource_allocation.clone(),
            destination_additional_mounts: operation
                .selection
                .additional_mounts
                .clone()
                .unwrap_or_default(),
            destination_resource_allocation: operation.selection.resource_allocation.clone(),
            checkpoint_retained: operation.checkpoint.is_some(),
            destination_ready: operation.destination_target.is_some()
                && operation.destination_native_session_id.is_some(),
            queue_admission_started: operation.queue_admission_started,
            queue_admission_finished: operation.queue_admission_finished,
        })
    }
}

impl ViewerSession {
    /// Apply a resolved controller source while keeping paths and remotes out
    /// of the public projection.
    pub fn set_project_source(&mut self, source: &ProjectSourceIdentity) {
        self.project_label = source.short.clone();
        self.project_key = project_key(&source.key);
    }
}

// One wire representation for the UI and native API activity facts.
pub use crate::database::{
    ApiActivityDetails as ViewerActivityDetails, ApiActivityKind as ViewerActivityKind,
};

/// One Mjolnir command a phone may offer for this session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerMjCommand {
    pub name: String,
    pub description: String,
    /// Whether Mjolnir handles this command locally or forwards it to the
    /// active agent.
    pub source: ViewerCommandSource,
    /// What the argument is called, when the command takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewerCommandSource {
    Mj,
    Agent,
}

/// Public review configuration: exactly what `/review status` needs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewConfig {
    pub enabled: bool,
    pub tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// A turn review as a phone renders it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTurnReview {
    /// `quick` or `extended`.
    pub tier: String,
    /// What the review is doing, in one line.
    pub status: String,
    /// One row per reviewing agent: its label and where it has got to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<ViewerReviewRole>,
    /// Present once the review has reached a verdict the user must answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ViewerReviewVerdict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewRole {
    pub label: String,
    /// `pending`, `running`, `done`, `findings`, or `failed`.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewVerdict {
    /// `clean`, `findings`, or `failed`.
    pub kind: String,
    /// The findings, or the failure's reason.
    pub text: String,
    /// The resolutions this verdict accepts: `forward`, `dismiss`, `cancel`.
    /// A phone shows the rest disabled rather than hiding them, so the buttons
    /// do not move under a thumb.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
}

impl ViewerTurnReview {
    /// The phone's view of one review the daemon is running.
    #[must_use]
    pub fn from_runtime(review: &crate::review_host::RuntimeReviewView) -> Self {
        Self {
            tier: review.tier.label().to_owned(),
            status: review.status.clone(),
            roles: review
                .roles
                .iter()
                .map(|role| ViewerReviewRole {
                    label: role.label.clone(),
                    state: role.state.label().to_owned(),
                })
                .collect(),
            verdict: review.verdict.as_ref().map(|verdict| ViewerReviewVerdict {
                kind: match verdict.kind {
                    crate::review_host::VerdictKind::Clean => "clean",
                    crate::review_host::VerdictKind::Findings => "findings",
                    crate::review_host::VerdictKind::Failed => "failed",
                }
                .to_owned(),
                text: verdict.text.clone(),
                allowed: verdict
                    .allowed
                    .iter()
                    .filter_map(resolution_name)
                    .map(str::to_owned)
                    .collect(),
            }),
        }
    }
}

/// The wire name of one resolution, shared by the projection and the action
/// that performs it, so a button's name is the name the server accepts.
#[must_use]
pub fn resolution_name(resolution: &mj_core::review::driver::Resolution) -> Option<&'static str> {
    match resolution {
        mj_core::review::driver::Resolution::Forwarded => Some("forward"),
        mj_core::review::driver::Resolution::Dismissed => Some("dismiss"),
        mj_core::review::driver::Resolution::Cancelled => Some("cancel"),
        // Not resolutions a surface asks for: the review reaches these itself.
        mj_core::review::driver::Resolution::NothingToReview
        | mj_core::review::driver::Resolution::CoverageStarted => None,
    }
}

/// The resolution a phone's button asked for.
#[must_use]
pub fn resolution_from_name(name: &str) -> Option<mj_core::review::driver::Resolution> {
    match name {
        "forward" => Some(mj_core::review::driver::Resolution::Forwarded),
        "dismiss" => Some(mj_core::review::driver::Resolution::Dismissed),
        "cancel" => Some(mj_core::review::driver::Resolution::Cancelled),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerWorkspace {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQueuedPrompt {
    pub id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerUserShell {
    pub id: String,
    pub command: String,
    pub started_at_ms: Option<i64>,
}

/// One command the active agent left running in the background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerBackgroundTask {
    pub id: String,
    pub command: String,
    pub started_at_ms: i64,
    pub can_stop: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerProfile {
    pub id: String,
    pub harness_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<ViewerQuota>,
}

/// One usage window a harness reports, such as a weekly or five-hour limit.
///
/// `percent_used` is the figure a person acts on, so it travels as a number
/// rather than inside a sentence. The controller computes headroom; this is
/// its complement, because a bar fills as a limit is consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQuotaWindow {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Whether this window is on course to run out before it resets. The
    /// controller already computes this; a phone should not have to.
    pub projects_exhaustion_before_reset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQuota {
    /// One-line rendering, kept so a viewer cached from before the structured
    /// windows existed keeps working. The Quota page renders `windows`.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<ViewerQuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    pub stale: bool,
    /// When the reading was taken. A pulled view delivered by push cannot be
    /// told from a current one without its age, so this is not optional.
    #[serde(default)]
    pub refreshed_at_epoch_seconds: u64,
    /// Error state only. Raw vendor errors may contain paths or account data
    /// and remain on the controller.
    pub has_error: bool,
}

/// What one host or fleet has, and how fresh the reading is.
///
/// Every field that carries a reading is optional, and `sampled_at_epoch_seconds`
/// is present whenever any of them is: a reading without its age cannot be
/// told from a stale one, which is exactly the case where it matters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTargetCapacity {
    pub id: String,
    /// The host or fleet as a person names it. Never a locator, an address or
    /// a full path.
    pub label: String,
    pub target_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_cores: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    /// How many machines a fleet is running. Absent for a plain host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_machines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at_epoch_seconds: Option<u64>,
    pub refreshing: bool,
    pub stale: bool,
    /// Whether the last probe failed. The probe's own message names hosts and
    /// commands, so it stays on the controller.
    pub has_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTarget {
    pub id: String,
    pub kind: String,
    pub requires_project_directory: bool,
    /// Recent raw project directories for this target's physical host. Managed
    /// targets intentionally publish an empty list because they select a
    /// configured bundle rather than a host checkout.
    #[serde(default)]
    pub recent_project_directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerBundle {
    pub id: String,
    pub primary_repository: String,
    pub repositories: Vec<ViewerRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerRepository {
    pub id: String,
    pub github: Option<String>,
    pub destination: String,
}

/// What a phone may do with one session, as the controller sees it.
///
/// The viewer renders a control because a flag here is true, and for no other
/// reason. Deciding legality in the browser means copying controller policy
/// into JavaScript, where it drifts silently: the browser cannot know that a
/// session is unmanaged, that a lifecycle operation holds it, or that the
/// harness never advertised the option a control would change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSessionCapabilities {
    pub open: bool,
    pub prompt: bool,
    pub run_shell: bool,
    /// Cancel the turn the agent is working on now, leaving the session alive.
    pub cancel_turn: bool,
    /// Cancel the provision, resume or stop currently running.
    pub cancel_operation: bool,
    pub stop: bool,
    pub rename: bool,
    pub resume: bool,
    /// Prepare and confirm a daemon-owned move to a compatible profile or
    /// target. The browser must never compose Stop and Resume itself.
    #[serde(default)]
    pub move_session: bool,
    pub set_config: bool,
    pub set_plan_mode: bool,
}

/// The small set of states a phone reasons about, alongside the precise state.
///
/// A phone groups and filters by this; it shows the precise `state` string as
/// the word it prints. Collapsing here rather than in the browser keeps one
/// definition of "live" in the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerLifecycleCategory {
    Live,
    Starting,
    Stopping,
    Stopped,
    Failed,
}

impl ViewerLifecycleCategory {
    pub(super) const fn of(state: SessionState) -> Self {
        match state {
            SessionState::Provisioning => Self::Starting,
            SessionState::Running | SessionState::Disconnected | SessionState::Checkpointing => {
                Self::Live
            }
            SessionState::Closing | SessionState::Destroying => Self::Stopping,
            SessionState::Stopped => Self::Stopped,
            SessionState::Lost | SessionState::Error | SessionState::DestroyedWithDataLoss => {
                Self::Failed
            }
        }
    }

    /// Whether this session belongs on the dashboard. Stopped and failed
    /// sessions belong to the resume flow instead, which is where a person can
    /// do something about them.
    pub const fn is_dashboard_visible(self) -> bool {
        matches!(self, Self::Live | Self::Starting | Self::Stopping)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerOperationKind {
    Create,
    Resume,
    Move,
    Stop,
    Destroy,
    Cleanup,
    Checkpoint,
}

impl ViewerOperationKind {
    pub const fn transition_kind(self) -> Option<SessionTransitionKind> {
        match self {
            Self::Create => Some(SessionTransitionKind::Starting),
            Self::Resume => Some(SessionTransitionKind::Resuming),
            Self::Move => Some(SessionTransitionKind::Moving),
            Self::Stop => Some(SessionTransitionKind::Stopping),
            Self::Destroy | Self::Cleanup => Some(SessionTransitionKind::Destroying),
            // Checkpointing is an ordinary live-session operation. It must
            // not replace a readable conversation with a placeholder.
            Self::Checkpoint => None,
        }
    }
}

/// One stage of a running operation, with the clock it started on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerOperationStage {
    pub label: String,
    pub started_at_epoch_seconds: u64,
}

/// A provision, resume, stop or checkpoint the controller is running now.
///
/// A phone that asked for one of these got `202 Accepted` and an identifier
/// rather than a result, because the work outlives the request. This is how it
/// finds out what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerOperation {
    pub id: String,
    pub session_id: String,
    pub kind: ViewerOperationKind,
    pub started_at_epoch_seconds: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<ViewerOperationStage>,
    /// Controller-authored and already meant for a person to read, unlike the
    /// error text this projection keeps on the controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    pub cancellable: bool,
}

/// What the agent is doing, mirroring `RelayExecutionState`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerChatPhase {
    #[default]
    Idle,
    Running,
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One setting the harness advertised, with the values it will accept.
///
/// The browser completes `/model` and `/effort` from this rather than from a
/// list of its own, so a harness that offers something new needs no viewer
/// change, and a viewer can never offer a value the harness would refuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerConfigOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub choices: Vec<ViewerConfigChoice>,
}
