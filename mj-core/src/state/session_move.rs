//! Durable intent for a verified stop followed by destination restoration.

use super::*;

/// Refusal for a resource allocation aimed at a bare target. Callers match on
/// it to add their own remedy (a flag, a checkbox).
pub const BARE_TARGET_FIXED_RESOURCES: &str = "bare targets have fixed host resources";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeQueueDisposition {
    Start,
    Discard,
}

/// Route taken to move an archive (and optional `.mj/keep/` stream) between targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferRoute {
    SameHost,
    DirectPush,
    DirectPull,
    DaemonStream,
}

impl TransferRoute {
    pub const fn is_direct(self) -> bool {
        matches!(self, Self::DirectPush | Self::DirectPull)
    }

    pub const fn is_pulled(self) -> bool {
        matches!(self, Self::DirectPull)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::SameHost => "same-host pipe",
            Self::DirectPush => "direct push",
            Self::DirectPull => "direct pull",
            Self::DaemonStream => "daemon stream",
        }
    }
}

/// Read-only route preview for `--dry-run`. Never installs keys or AWS SG rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePreview {
    SameHost,
    DirectOrDaemonStream,
}

impl RoutePreview {
    pub const fn label(self) -> &'static str {
        match self {
            Self::SameHost => "same-host pipe",
            Self::DirectOrDaemonStream => "direct host-to-host (or daemon stream fallback)",
        }
    }
}

/// Durable ownership record for an ephemeral Move SSH key / known_hosts stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedMoveKey {
    pub host: String,
    pub comment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_path: Option<PathBuf>,
    /// Remote `authorized_keys` file where the ephemeral public key was installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_keys_path: Option<PathBuf>,
    /// Host-side worker binary path used to run `worker authorized-keys remove`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_bin: Option<String>,
    /// Self-contained SSH target so deferred revocation works even after the
    /// Move retires its source or another Move runs on the same session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_target: Option<crate::targets::SshTarget>,
}

/// Durable ownership record for a shared EC2 security-group ingress grant used by a Move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedEc2Rule {
    pub security_group_id: String,
    pub cidr_ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveSelection {
    #[serde(default)]
    pub consent: crate::suspend_consent::SuspendConsent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_keep_paths: Vec<PathBuf>,
    #[serde(default)]
    pub clear_resource_allocation: bool,
    pub session_id: String,
    pub profile_id: Option<String>,
    pub target_template_id: Option<String>,
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    /// Delegation policy for the destination. `None` keeps the session's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<crate::subagent::SubagentPolicy>,
    /// Oracle choice for the destination; omitted retains the session value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<bool>,
    /// Supervisors choice for the destination; omitted retains the session value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisors: Option<bool>,
    /// Destination oracle model override; omitted retains the session value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_choice: Option<crate::config::SubagentChoice>,
}

/// What moving a local checkout into an isolated workspace will do, shown
/// before anything is stopped or provisioned.
///
/// Every field is read from the host checkout and its remote. The dirty counts
/// are deliberately not part of a move fingerprint: a running local session has
/// an agent editing files, so they change under the confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConversionPreview {
    /// The checkout that is snapshotted: a managed worktree, or the user's own
    /// directory when the session opened one directly.
    pub checkout: PathBuf,
    /// Where the checkout lands inside the target.
    pub destination: PathBuf,
    /// The branch the session continues on, or `None` for a detached head.
    pub branch: Option<String>,
    pub fetch_url: String,
    pub push_urls: Vec<String>,
    /// The branch the remote's `HEAD` names, which is what a fresh clone
    /// starts on before the session's own branch is restored.
    pub default_branch: String,
    /// Commits reachable from `HEAD` that are on no origin ref, and so have to
    /// travel in the conversion archive.
    pub unpushed_commits: u64,
    pub staged_files: u64,
    pub unstaged_files: u64,
    pub untracked_files: u64,
    pub untracked_bytes: u64,
    /// True when the session opened the user's own checkout, which stays on
    /// this machine untouched after the move.
    pub host_checkout_retained: bool,
}

impl RawConversionPreview {
    /// The one line every surface shows: what is cloned, where it lands, which
    /// branch the session continues on, and where `git push` goes.
    pub fn summary_line(&self) -> String {
        let push = if self.push_urls.is_empty() {
            self.fetch_url.clone()
        } else {
            self.push_urls.join(", ")
        };
        format!(
            "Clone {} (default branch {}) into {} on branch {}; push to {push}.",
            self.fetch_url,
            self.default_branch,
            self.destination.display(),
            self.branch.as_deref().unwrap_or("a detached head"),
        )
    }

    /// What a person needs to read before the checkout moves: which
    /// uncommitted work travels, which commits travel, and what stays behind.
    ///
    /// Every surface renders these in its own warning style, so the wording
    /// lives here rather than in each of the TUI, the CLI, and the browser.
    pub fn warning_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let dirty = self.staged_files + self.unstaged_files + self.untracked_files;
        if dirty > 0 {
            lines.push(format!(
                "{} staged, {} unstaged, and {} untracked {} ({}) will be copied into the container. \
                 Ignored files such as build output, .env, and node_modules will not.",
                self.staged_files,
                self.unstaged_files,
                self.untracked_files,
                if dirty == 1 { "file" } else { "files" },
                format_conversion_bytes(self.untracked_bytes),
            ));
        }
        if self.unpushed_commits > 0 {
            let mut line = format!(
                "{} {} not on {} {} in the checkpoint.",
                self.unpushed_commits,
                if self.unpushed_commits == 1 {
                    "commit"
                } else {
                    "commits"
                },
                self.fetch_url,
                if self.unpushed_commits == 1 {
                    "travels"
                } else {
                    "travel"
                },
            );
            if self.unpushed_commits > 200 {
                line.push_str(" That is a large history; consider pushing first.");
            }
            lines.push(line);
        }
        if self.host_checkout_retained {
            lines.push(format!(
                "{} stays on this machine and will no longer track this session. \
                 Edits made in the container do not come back automatically; \
                 push the branch or move the session back.",
                self.checkout.display(),
            ));
        }
        lines
    }
}

/// Untracked size as a person reads it. Only KB and MB appear: a checkout's
/// untracked work is never usefully described in bytes, and anything above a
/// gigabyte is already a warning in megabytes.
fn format_conversion_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes = bytes as f64;
    if bytes < MB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes / MB)
    }
}

pub const EC2_MOVE_PREPARATION_NOTICE: &str =
    "The EC2 instance will be created and checked after you confirm Move.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestinationChecks {
    #[default]
    Checked,
    AfterProvisioning,
}

/// The launch request is immutable for an attempt, including its client token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedMoveDestination {
    pub launch_args: Vec<String>,
    pub runtime: TargetRuntimeSettings,
    pub state: PreparedDestinationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparedDestinationState {
    LaunchPending,
    Created { instance_id: String },
    Checked { target: TargetLocator },
    Adopted { target: TargetLocator },
    CleanupPending { instance_id: Option<String> },
    Released,
}

impl PreparedMoveDestination {
    pub fn target(&self) -> Option<&TargetLocator> {
        match &self.state {
            PreparedDestinationState::Checked { target }
            | PreparedDestinationState::Adopted { target } => Some(target),
            _ => None,
        }
    }

    pub fn instance_id(&self) -> Option<&str> {
        match &self.state {
            PreparedDestinationState::Created { instance_id }
            | PreparedDestinationState::CleanupPending {
                instance_id: Some(instance_id),
            } => Some(instance_id),
            _ => match self.target() {
                Some(TargetLocator::AwsEc2 { instance_id, .. }) => Some(instance_id),
                _ => None,
            },
        }
    }

    pub fn owns_resource(&self) -> bool {
        !matches!(self.state, PreparedDestinationState::Released)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePreparation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_upgrade: Option<Box<super::ContainerUpgradePreview>>,
    #[serde(default)]
    pub destination_checks: DestinationChecks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_preview: Option<RoutePreview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend_preflight: Option<Box<crate::suspend_consent::SuspendPreflight>>,
    /// Skipped `.mj/keep/` paths and their sizes in bytes, for `--dry-run` and dialogs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_keep_sizes: Vec<(PathBuf, u64)>,
    #[serde(default)]
    pub source_unavailable: bool,
    /// The destination is the source target: only the harness is replaced;
    /// the container or worker root and the workspace are kept.
    #[serde(default)]
    pub in_place: bool,
    /// Move stops the parent's live sub-agents before it closes the source.
    /// An in-place Move keeps them running when the destination harness has
    /// the mj-agents parent tools and the source worker can drain sub-agent
    /// requests.
    #[serde(default)]
    pub stops_subagents: bool,
    /// Present only when this move converts a local checkout into an isolated
    /// workspace. Boxed because this preparation travels inside several
    /// request enums whose other variants are far smaller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversion: Option<Box<RawConversionPreview>>,
    pub selection: MoveSelection,
    /// The selection's size is the destination's default, which a clear
    /// request resolved to. The sealed Move owns this fact so that retrying
    /// the same command compares against what this Move decided rather than
    /// against host state other sessions keep changing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sized_by_destination_default: bool,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    /// The destination cannot continue the native conversation, because its
    /// harness or model service differs (see `Config::native_continuity`), so
    /// the transcript is handed over as compacted text. Stored move records
    /// keep this name.
    pub cross_harness: bool,
    pub active: bool,
    pub queued_commands: Vec<MaterializedQueuedPrompt>,
    pub fingerprint: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveSessionRequest {
    pub preparation: MovePreparation,
    pub queue: Option<ResumeQueueDisposition>,
    pub acknowledge_interruption: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveOutcome {
    pub operation_id: String,
    pub session_id: String,
    pub profile_id: String,
    pub target_template_id: String,
    pub outcome: String,
    pub error: Option<String>,
    pub recovery: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    Capturing,
    Transferring,
    Restoring,
    DestinationStarting,
    DestinationOwns,
    SourceRetired,
    QueueAdmitted,
    DispatchReleased,
    Completed,
    Failed,
    Cancelled,
}

impl MovePhase {
    /// True once the cross-target Move has crossed its commit point and the
    /// destination durably owns the session.
    pub const fn destination_owns(self) -> bool {
        matches!(
            self,
            Self::DestinationOwns | Self::SourceRetired | Self::QueueAdmitted
        )
    }
}

/// Pre-148 `MovePhase` variants kept solely for startup recovery of in-flight
/// legacy `MoveOperation` records across an upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyMovePhase {
    Preparing,
    ClosingSource,
    ResumingDestination,
    StartingQueue,
    Completed,
    Failed,
    Cancelled,
}

impl LegacyMovePhase {
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::ClosingSource | Self::ResumingDestination | Self::StartingQueue
        )
    }
}

/// Pre-149 `MoveSelection` fields preserved when converting a retryable legacy
/// Move across an upgrade (ignores removed legacy fields such as `workspace`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LegacyMoveSelection {
    pub session_id: String,
    pub profile_id: Option<String>,
    pub target_template_id: Option<String>,
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub clear_resource_allocation: bool,
    pub subagents: Option<crate::subagent::SubagentPolicy>,
    pub oracle: Option<bool>,
    pub oracle_choice: Option<crate::config::SubagentChoice>,
}

/// Pre-149 `MoveOperation` shape decoded only during startup recovery so an
/// upgraded daemon can roll back source-owned legacy moves, finish forward
/// destination-owned legacy moves, or preserve retryable failed/cancelled moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyMoveOperation {
    #[serde(default)]
    pub selection: Option<LegacyMoveSelection>,
    #[serde(default)]
    pub quota_stop_reason: Option<String>,
    #[serde(default)]
    pub quota_continuation: Option<QuotaMoveContinuation>,
    #[serde(default)]
    pub prepared_destination: Option<PreparedMoveDestination>,
    #[serde(default)]
    pub acknowledge_interruption: bool,
    #[serde(default)]
    pub handoff: Option<CheckpointMetadata>,
    #[serde(default)]
    pub source_checkpoint_only: bool,
    #[serde(default)]
    pub in_place: bool,
    pub operation_id: String,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub source_target: Option<TargetLocator>,
    pub source_native_session_id: Option<String>,
    #[serde(default)]
    pub inherited_resource_allocation: Option<SessionResourceAllocation>,
    #[serde(default)]
    pub inherited_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub source_additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub source_resource_allocation: Option<SessionResourceAllocation>,
    pub destination_target: Option<TargetLocator>,
    pub destination_native_session_id: Option<String>,
    pub destination_store_id: Option<String>,
    #[serde(default)]
    pub configuration_fingerprint: String,
    #[serde(default)]
    pub checkpoint: Option<CheckpointMetadata>,
    #[serde(default)]
    pub recovery_session: Option<SessionRecord>,
    pub queue: ResumeQueueDisposition,
    pub phase: LegacyMovePhase,
    #[serde(default)]
    pub queue_admission_started: bool,
    #[serde(default)]
    pub queue_admission_finished: bool,
    #[serde(default)]
    pub cancellation_requested: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub error: Option<String>,
}

impl LegacyMoveOperation {
    /// Every checkpoint archive this legacy Move still references for retry or recovery.
    pub fn retained_archives(&self) -> Vec<&CheckpointMetadata> {
        if self.is_resolved() {
            return Vec::new();
        }
        let mut archives = Vec::new();
        if let Some(handoff) = self.handoff.as_ref() {
            archives.push(handoff);
        }
        if let Some(checkpoint) = self.checkpoint.as_ref()
            && !archives
                .iter()
                .any(|existing| existing.archive_path == checkpoint.archive_path)
        {
            archives.push(checkpoint);
        }
        archives
    }

    /// Whether this legacy Move has no remaining work, retryable archive, or
    /// prepared destination and can safely be deleted on upgrade.
    pub fn is_resolved(&self) -> bool {
        match self.phase {
            LegacyMovePhase::Completed => true,
            LegacyMovePhase::Failed | LegacyMovePhase::Cancelled => {
                !self.queue_admission_started
                    && self.handoff.is_none()
                    && self.checkpoint.is_none()
                    && self.prepared_destination.is_none()
            }
            _ => false,
        }
    }

    /// Whether startup recovery must actively roll back or finish forward this legacy Move.
    pub fn needs_recovery(&self) -> bool {
        self.phase.is_active() || (self.queue_admission_started && !self.queue_admission_finished)
    }

    /// Convert a retryable inactive (`Failed` / `Cancelled`) legacy Move into a
    /// current `MoveOperation` so retry state and archive retention survive upgrade.
    pub fn into_retryable_move_operation(&self, session_id: &str) -> MoveOperation {
        let sel = self.selection.as_ref();
        MoveOperation {
            quota_stop_reason: self.quota_stop_reason.clone(),
            quota_continuation: self.quota_continuation,
            prepared_destination: self.prepared_destination.clone(),
            accepted_preparation: None,
            acknowledge_interruption: self.acknowledge_interruption,
            transfer_route: None,
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
            granted_keys: Vec::new(),
            granted_ec2_rules: Vec::new(),
            in_place_expected_store_id: None,
            in_place_continuation_command_id: None,
            in_place_held_command_ids: Vec::new(),
            in_place_harness_restarted: false,
            deferred_environment_changes: Vec::new(),
            source_snapshot: None,
            source_checkpoint_only: self.source_checkpoint_only,
            in_place: self.in_place,
            operation_id: self.operation_id.clone(),
            selection: MoveSelection {
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
                clear_resource_allocation: sel.is_some_and(|s| s.clear_resource_allocation),
                session_id: sel
                    .map(|s| s.session_id.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(session_id)
                    .to_owned(),
                profile_id: sel.and_then(|s| s.profile_id.clone()),
                target_template_id: sel.and_then(|s| s.target_template_id.clone()),
                additional_mounts: sel.and_then(|s| s.additional_mounts.clone()),
                resource_allocation: sel.and_then(|s| s.resource_allocation.clone()),
                subagents: sel.and_then(|s| s.subagents.clone()),
                oracle: sel.and_then(|s| s.oracle),
                supervisors: None,
                oracle_choice: sel.and_then(|s| s.oracle_choice.clone()),
            },
            source_profile_id: self.source_profile_id.clone(),
            source_target_template_id: self.source_target_template_id.clone(),
            source_target: self.source_target.clone(),
            source_native_session_id: self.source_native_session_id.clone(),
            inherited_resource_allocation: self.inherited_resource_allocation.clone(),
            inherited_additional_mounts: self.inherited_additional_mounts.clone(),
            source_additional_mounts: self.source_additional_mounts.clone(),
            source_resource_allocation: self.source_resource_allocation.clone(),
            destination_target: self.destination_target.clone(),
            destination_native_session_id: self.destination_native_session_id.clone(),
            destination_store_id: self.destination_store_id.clone(),
            configuration_fingerprint: self.configuration_fingerprint.clone(),
            checkpoint: self.handoff.clone().or_else(|| self.checkpoint.clone()),
            recovery_session: self.recovery_session.clone(),
            queue: self.queue,
            phase: match self.phase {
                LegacyMovePhase::Cancelled => MovePhase::Cancelled,
                _ => MovePhase::Failed,
            },
            queue_admission_started: self.queue_admission_started,
            queue_admission_finished: self.queue_admission_finished,
            cancellation_requested: self.cancellation_requested,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            error: self.error.clone(),
        }
    }
}

/// Result of decoding a stored `MoveOperation` JSON row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredMoveOperation {
    Current(Box<MoveOperation>),
    Legacy(Box<LegacyMoveOperation>),
}

impl StoredMoveOperation {
    pub fn decode_json(json: &str) -> anyhow::Result<Self> {
        match serde_json::from_str::<MoveOperation>(json) {
            Ok(op) => Ok(Self::Current(Box::new(op))),
            Err(current_err) => match serde_json::from_str::<LegacyMoveOperation>(json) {
                Ok(legacy) => Ok(Self::Legacy(Box::new(legacy))),
                Err(_) => Err(current_err.into()),
            },
        }
    }

    pub fn operation_id(&self) -> &str {
        match self {
            Self::Current(op) => &op.operation_id,
            Self::Legacy(op) => &op.operation_id,
        }
    }

    pub fn is_active(&self) -> bool {
        match self {
            Self::Current(op) => op.is_active(),
            Self::Legacy(op) => op.phase.is_active(),
        }
    }
}

/// The continuation chosen once on the destination before queue admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaMoveContinuation {
    Prompt,
    ResumeGoal,
}

/// Durable metadata for a cross-target Move archive that remains on the source
/// target and streams directly to the destination (never installed on the
/// daemon as `session.checkpoint`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveSourceSnapshot {
    /// Archive path on the source target.
    pub archive_path: PathBuf,
    /// Verified SHA-256 digest of the source-target archive.
    pub sha256: String,
    /// Archive manifest returned by the source worker at export time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<crate::archive::ArchiveManifest>,
    /// Queued prompts captured at the source barrier for post-commit admission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<crate::archive::CanonicalQueuedPrompt>,
    /// Pending worker mail captured at the source barrier for post-commit admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<crate::archive::CanonicalMailbox>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveOperation {
    /// Daemon-owned continuation after an automatic quota profile switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_continuation: Option<QuotaMoveContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_destination: Option<PreparedMoveDestination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_preparation: Option<Box<MovePreparation>>,
    #[serde(default)]
    pub acknowledge_interruption: bool,
    /// Route chosen for cross-target archive + `.mj/keep/` transfer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_route: Option<TransferRoute>,
    /// Typed suspend consent captured for this Move.
    #[serde(default)]
    pub consent: crate::suspend_consent::SuspendConsent,
    /// Relative `.mj/keep/` paths excluded from transfer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_keep_paths: Vec<PathBuf>,
    /// Ephemeral SSH keys / known_hosts files owned by this Move (persisted before install).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub granted_keys: Vec<GrantedMoveKey>,
    /// Shared EC2 security-group ingress rules used by this Move (persisted before install).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub granted_ec2_rules: Vec<GrantedEc2Rule>,
    /// In-place Move: relay `store_id` captured before holding dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_place_expected_store_id: Option<String>,
    /// In-place Move: operation-derived command ID for `SubmitFront` continuation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_place_continuation_command_id: Option<String>,
    /// In-place Move: pending command IDs recorded when `HoldInPlaceMoveDispatch` ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_place_held_command_ids: Vec<String>,
    /// In-place Move: whether `RestartHarnessInPlace` has completed.
    #[serde(default)]
    pub in_place_harness_restarted: bool,
    /// Environment changes deferred until the session's environment is next recreated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred_environment_changes: Vec<String>,
    /// Source-target Move snapshot streamed directly to the destination without
    /// staging on daemon disk or overwriting `session.checkpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot: Option<Box<MoveSourceSnapshot>>,
    /// Keep the source harness stopped across recovery until destination restoration.
    #[serde(default)]
    pub source_checkpoint_only: bool,
    /// The destination is the source target: only the harness is replaced;
    /// the container or worker root and the workspace are kept.
    #[serde(default)]
    pub in_place: bool,
    pub operation_id: String,
    pub selection: MoveSelection,
    pub source_profile_id: String,
    pub source_target_template_id: String,
    pub source_target: Option<TargetLocator>,
    pub source_native_session_id: Option<String>,
    /// Frozen request inherited by the destination when selection says keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_resource_allocation: Option<SessionResourceAllocation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_additional_mounts: Vec<AdditionalMount>,
    /// Applied source settings, used only for source recovery.
    pub source_additional_mounts: Vec<AdditionalMount>,
    pub source_resource_allocation: Option<SessionResourceAllocation>,
    pub destination_target: Option<TargetLocator>,
    pub destination_native_session_id: Option<String>,
    pub destination_store_id: Option<String>,
    pub configuration_fingerprint: String,
    pub checkpoint: Option<CheckpointMetadata>,
    /// Stopped identity retained across partially written resume conversions.
    pub recovery_session: Option<SessionRecord>,
    pub queue: ResumeQueueDisposition,
    pub phase: MovePhase,
    /// A durable boundary: once set, never restore or replay on another relay.
    pub queue_admission_started: bool,
    pub queue_admission_finished: bool,
    pub cancellation_requested: bool,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<String>,
}

impl MoveOperation {
    /// Every command a retained Move queue can resubmit, including quota continuation.
    pub fn queue_command_ids(
        &self,
        canonical: &crate::archive::CanonicalSessionSnapshot,
    ) -> std::collections::BTreeSet<String> {
        self.queue_command_ids_from_prompts(&canonical.queued_prompts)
    }

    pub fn queue_command_ids_from_prompts(
        &self,
        queued_prompts: &[crate::archive::CanonicalQueuedPrompt],
    ) -> std::collections::BTreeSet<String> {
        let mut ids = queued_prompts
            .iter()
            .map(|queued| queued.command_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if self.quota_stop_reason.is_some() {
            ids.insert(
                crate::continuation::GeneratedPrompt::QuotaRetry.command_id(&self.operation_id),
            );
        }
        ids
    }

    /// Capture the two independently owned facts at acceptance of each attempt.
    pub fn capture_container_settings(&mut self, source: &SessionRecord) {
        let applied = super::applied_container_settings(source);
        self.source_resource_allocation = applied.resource_allocation.cloned();
        self.source_additional_mounts = applied.additional_mounts.to_vec();
        self.inherited_resource_allocation = if self.selection.resource_allocation.is_none()
            && !self.selection.clear_resource_allocation
        {
            source.resource_allocation.clone()
        } else {
            None
        };
        self.inherited_additional_mounts = if self.selection.additional_mounts.is_none() {
            source.additional_mounts.clone()
        } else {
            Vec::new()
        };
    }

    /// Explicit Resume resolves a cancelled Move by finishing its queue
    /// without ever admitting it. Its archives are then historical, just as
    /// for a completed Move, and cannot make a later Move a retry.
    pub fn is_resolved(&self) -> bool {
        self.phase == MovePhase::Completed
            || (self.phase == MovePhase::Cancelled
                && !self.queue_admission_started
                && self.queue_admission_finished)
    }

    /// Whether this Move retains its source environment before destination
    /// ownership commits. Both in-place Moves (which never recreate the target)
    /// and cross-target Moves before `DestinationOwns` keep the source intact so
    /// any pre-commit interruption or failure rolls back to the source instead
    /// of tearing it down before a destination exists.
    pub fn retains_source_environment(&self) -> bool {
        self.in_place || (!self.phase.destination_owns() && !self.queue_admission_started)
    }

    pub fn restore_artifact(&self) -> Option<&CheckpointMetadata> {
        self.checkpoint.as_ref()
    }

    pub fn retains_checkpoint(&self) -> bool {
        (!self.is_resolved() && self.phase != MovePhase::Cancelled)
            || (self.queue_admission_started && !self.queue_admission_finished)
    }

    /// Every checkpoint archive this Move keeps on disk. The startup archive
    /// sweep and the superseded-checkpoint prune both ask this one question, so
    /// neither can delete an archive the Move still restores from.
    pub fn retained_archives(&self) -> impl Iterator<Item = &CheckpointMetadata> {
        self.retains_checkpoint()
            .then_some(self.checkpoint.as_ref())
            .into_iter()
            .flatten()
    }

    /// Whether an unfinished Move still has the checkpoint a retry restores.
    ///
    /// The Move record owns this fact. When its archive is found missing, the
    /// reference is removed from the record, so the recovery guidance, the API
    /// and the retry admission all read the loss from here.
    pub fn checkpoint_retained(&self) -> bool {
        !self.is_resolved() && (self.restore_artifact().is_some() || self.source_snapshot.is_some())
    }

    /// Whether a surface should offer to retry this failed or cancelled Move:
    /// it still has its checkpoint, or queued work already began on the
    /// destination and only this Move may finish admitting it.
    pub fn offers_retry(&self) -> bool {
        matches!(self.phase, MovePhase::Failed | MovePhase::Cancelled)
            && (self.checkpoint_retained()
                || (self.queue_admission_started && !self.queue_admission_finished))
    }

    /// Whether this unfinished Move holds the source environment (the target,
    /// its worker root and the checkout) for an explicit retry. While it does,
    /// Resume would recreate what the Move promised to keep, so only a retried
    /// Move or Destroy may act on the session.
    pub fn holds_source_environment(&self) -> bool {
        self.retains_source_environment() && !self.is_resolved() && self.recovery_session.is_some()
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            MovePhase::Capturing
                | MovePhase::Transferring
                | MovePhase::Restoring
                | MovePhase::DestinationStarting
                | MovePhase::DestinationOwns
                | MovePhase::SourceRetired
                | MovePhase::QueueAdmitted
                | MovePhase::DispatchReleased
        )
    }
}
