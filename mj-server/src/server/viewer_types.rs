use super::*;
use mj_client::availability::{AvailabilityNote, AvailabilityVerdict};
use std::collections::BTreeSet;

thread_local! {
    static VIEWER_ROW_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VIEWER_SESSION_SCAN_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[doc(hidden)]
pub fn take_viewer_row_visits() -> usize {
    VIEWER_ROW_VISITS.with(|visits| visits.replace(0))
}

#[cfg(test)]
pub fn take_viewer_session_scan_visits() -> usize {
    VIEWER_SESSION_SCAN_VISITS.with(|visits| visits.replace(0))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSnapshot {
    #[serde(default)]
    pub operator_away: bool,
    #[serde(default)]
    pub auto_afk_minutes: u32,
    #[serde(default)]
    pub profile_capabilities: mj_core::snapshot_map::SharedSnapshot<
        mj_core::profile_capabilities::ProfileCapabilitiesSnapshot,
    >,
    #[serde(
        default,
        alias = "last_subagent_policy",
        deserialize_with = "mj_core::subagent::deserialize_policy_map"
    )]
    pub last_subagent_policies:
        std::collections::BTreeMap<String, mj_core::subagent::SubagentPolicy>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub last_oracle_choices: std::collections::BTreeMap<String, bool>,
    #[serde(default)]
    pub oracle_configured: bool,
    #[serde(default)]
    pub orchestrators_configured: bool,
    #[serde(default)]
    pub primary_configured: bool,
    #[serde(default)]
    pub subagents_configured: bool,
    pub revision: u64,
    pub generated_at: String,
    /// Unix time in milliseconds, refreshed when serving the projection.
    /// Clients use this as the clock for live activity cards.
    #[serde(default)]
    pub server_time_ms: i64,
    /// The controller build serving this viewer, so a browser or the desktop
    /// window can name the Mjolnir it is talking to. Absent from a snapshot
    /// written by an older controller.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server_version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<ViewerWorkspace>,
    pub sessions: ViewerSessions,
    /// Controller-private IDs that can still be destroyed through the API.
    #[serde(skip)]
    pub retired_session_ids: mj_core::snapshot_map::SharedSnapshot<BTreeSet<String>>,
    pub profiles: Vec<ViewerProfile>,
    /// Configured profiles left out of `profiles` because they are disabled.
    /// Pickers never offer them, but a request that names one explicitly may
    /// use it. Controller-private: never sent to clients.
    #[serde(skip)]
    pub disabled_profile_ids: std::collections::BTreeSet<String>,
    pub targets: Vec<ViewerTarget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_availability_notes: Vec<AvailabilityNote>,
    pub bundles: Vec<ViewerBundle>,
    /// The bounded part of `[review]` needed to report whether review is
    /// armed. Reviewer model and effort remain controller-private.
    #[serde(default)]
    pub review_config: ViewerReviewConfig,
    /// One entry per host or fleet that can be probed. Empty until the phone
    /// server's capacity poller has published a reading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity: Vec<ViewerTargetCapacity>,
    /// Recent failed launches, independent of provisional session rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_failures: Vec<ViewerLaunchFailure>,
    /// Safe background notices, retained even after a session is destroyed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<mj_client::daemon::RuntimeNotice>,
}

/// The public wire shape remains an array; in-process publications share rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "Vec<ViewerSession>", into = "Vec<ViewerSession>")]
pub struct ViewerSessions(pub mj_core::snapshot_map::SnapshotMap<String, ViewerSession>);

impl ViewerSessions {
    pub fn get(&self, id: &str) -> Option<&ViewerSession> {
        self.0.get(id)
    }

    pub(super) fn get_shared(&self, id: &str) -> Option<Arc<ViewerSession>> {
        self.0.get_shared(id)
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ViewerSession> + ExactSizeIterator {
        self.0.values().inspect(|_| {
            if cfg!(test) || mj_core::test_hooks::test_fixtures_enabled() {
                VIEWER_SESSION_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
            }
        })
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn push(&mut self, session: ViewerSession) {
        self.0.insert(session.id.clone(), session);
    }
}

impl From<Vec<ViewerSession>> for ViewerSessions {
    fn from(rows: Vec<ViewerSession>) -> Self {
        rows.into_iter().collect()
    }
}

impl From<ViewerSessions> for Vec<ViewerSession> {
    fn from(rows: ViewerSessions) -> Self {
        rows.0.into_values().collect()
    }
}

impl FromIterator<ViewerSession> for ViewerSessions {
    fn from_iter<T: IntoIterator<Item = ViewerSession>>(rows: T) -> Self {
        Self(rows.into_iter().map(|row| (row.id.clone(), row)).collect())
    }
}

impl IntoIterator for ViewerSessions {
    type Item = ViewerSession;
    type IntoIter = std::vec::IntoIter<ViewerSession>;
    fn into_iter(self) -> Self::IntoIter {
        Vec::from(self).into_iter()
    }
}

impl std::ops::Index<usize> for ViewerSessions {
    type Output = ViewerSession;
    fn index(&self, index: usize) -> &Self::Output {
        self.iter().nth(index).expect("viewer row index")
    }
}

impl std::ops::IndexMut<usize> for ViewerSessions {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        let id = self.0.keys().nth(index).expect("viewer row index").clone();
        self.0.get_mut(&id).expect("viewer row exists")
    }
}

/// Each parent's direct children with their durable kind, so the projection
/// can tell reviewers from workers without loading the child records.
pub type ViewerChildren = mj_core::snapshot_map::SnapshotMap<
    String,
    mj_core::snapshot_map::SnapshotMap<String, mj_core::subagent::SubagentKind>,
>;

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
        let mut children = ViewerChildren::new();
        for relation in state.subagents.values() {
            children
                .entry(relation.parent_session_id.clone())
                .or_insert_with(Default::default)
                .insert(relation.child_session_id.clone(), relation.kind);
        }
        let snapshot = Self::from_config_records(
            config,
            state,
            revision,
            state.sessions.values(),
            &children,
            None,
        );
        #[cfg(test)]
        let snapshot = {
            let mut snapshot = snapshot;
            for (id, record) in &state.sessions {
                let session = snapshot.sessions.0.get_mut(id).expect("fixture row");
                session.verdict.live_dashboard = record.state.is_active();
                session.verdict.transition = record.state.transition_kind();
                session.transitioning = session.verdict.transition.is_some();
                session.capabilities.suspend = session.lifecycle.is_dashboard_visible();
                session.capabilities.resume =
                    !session.lifecycle.is_dashboard_visible() && session.has_resume_source();
            }
            snapshot
        };
        snapshot
    }

    pub fn from_config_records<'a>(
        config: &Config,
        state: &AppState,
        revision: u64,
        records: impl Iterator<Item = &'a mj_core::state::SessionRecord>,
        children: &ViewerChildren,
        failed_launch_retries: Option<&BTreeSet<String>>,
    ) -> Self {
        // Snapshot-local interning shares storage without caching compatibility decisions.
        let targets: Vec<Arc<str>> = config
            .targets
            .keys()
            .map(|id| Arc::from(id.as_str()))
            .collect();
        let mut reasons = BTreeSet::<Arc<str>>::new();
        let mut target_lists = BTreeSet::<Arc<[Arc<str>]>>::new();
        let mut intern_list = |ids: Vec<Arc<str>>| {
            if let Some(existing) = target_lists.get(ids.as_slice()) {
                return existing.clone();
            }
            let shared: Arc<[Arc<str>]> = ids.into();
            target_lists.insert(shared.clone());
            shared
        };
        let sessions = records
            .map(|session| {
                if cfg!(test) || mj_core::test_hooks::test_fixtures_enabled() {
                    VIEWER_ROW_VISITS.with(|visits| visits.set(visits.get() + 1));
                }
                let checkout = state
                    .checkout(&session.id)
                    .expect("viewer session has a valid checkout owner");
                let resume_refusals = targets
                    .iter()
                    .filter_map(|target_id| {
                        mj_client::target::resume_compatibility_with_checkout(
                            session, &checkout, config, target_id,
                        )
                        .err()
                        .map(|reason| {
                            let shared = if let Some(existing) = reasons.get(reason.as_str()) {
                                existing.clone()
                            } else {
                                let shared: Arc<str> = reason.into();
                                reasons.insert(shared.clone());
                                shared
                            };
                            (target_id.clone(), shared)
                        })
                    })
                    .collect::<BTreeMap<_, _>>();
                let incompatible = intern_list(resume_refusals.keys().cloned().collect());
                let compatible = intern_list(
                    targets
                        .iter()
                        .filter(|id| !resume_refusals.contains_key(*id))
                        .cloned()
                        .collect(),
                );
                // A sub-agent child works in its parent's checkout and owns no
                // worktree, so its project identity has to come from the
                // parent; its own record would name the parent's session id.
                let project = state.project_identity_session(session);
                let source = project.project_source(config);
                let subagent = state.subagents.get(&session.id);
                let subagent_session_ids: Vec<String> = children
                    .get(&session.id)
                    .map(|children| children.keys().cloned().collect())
                    .unwrap_or_default();
                let reviewer_session_ids = children
                    .get(&session.id)
                    .map(|children| {
                        children
                            .iter()
                            .filter(|(_, kind)| **kind == mj_core::subagent::SubagentKind::Reviewer)
                            .map(|(id, _)| id.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                let start = session.start_selection();
                let review = state.effective_review_config(&config.review, &session.id);
                let failed_launch_retry = failed_launch_retries.map_or_else(
                    || {
                        mj_store::database::failed_first_launch(
                            session,
                            state.is_subagent_session(&session.id),
                            None,
                        )
                    },
                    |eligible| eligible.contains(&session.id),
                );
                let lifecycle = if failed_launch_retry {
                    ViewerLifecycleCategory::Failed
                } else {
                    ViewerLifecycleCategory::of(session.state)
                };
                ViewerSession {
                    attention: Default::default(),
                    unread_frontier: None,
                    notification_dismissal: None,
                    notification_card: None,
                    form_holder: None,
                    notifications_ignored: session.notifications_ignored,
                    oracle: session.oracle,
                    oracle_choice: session.oracle_choice.clone().map(|v| *v),
                    subagents: session.subagents.clone().unwrap_or_default(),
                    at: start.at,
                    branch: start.branch,
                    base: start.base,
                    no_later_history: start.no_later_history,
                    container_upgrade_supported:
                        mj_core::state::ContainerSettings::upgrade_supported(session),
                    targeted_turn_control_supported: false,
                    turn_controls: None,
                    native_subagents: Vec::new(),
                    native_subagent_attention: Default::default(),
                    steering: None,
                    active_prompt_id: None,
                    cancelling_prompt_id: None,
                    capacity_retry: None,
                    retry_assessment_pending: false,
                    quota_recovery: None,
                    is_subagent_session: state.is_subagent_session(&session.id),
                    owning_children_to_suspend: state
                        .suspendable_owned_child_ids(&session.id)
                        .len(),
                    supports_open_request: false,
                    relay_state: None,
                    id: session.id.clone(),
                    publication_state: session.publication_state(),
                    managed_checkout_kind: checkout.managed_worktree().map(|owned| owned.kind),
                    workspace_id: session.workspace_id.clone(),
                    title: session.listed_title().to_owned(),
                    given_title: session.given_name().map(str::to_owned),
                    subagent_parent_id: subagent.map(|child| child.parent_session_id.clone()),
                    subagent_task_name: subagent.map(|child| child.task_name.clone()),
                    subagent_session_ids,
                    reviewer_session_ids,
                    harness_kind: session.harness_kind.id().into(),
                    profile_id: session.last_profile.clone(),
                    bundle_id: session.bundle_id.clone(),
                    target_id: session.target_template_id.clone(),
                    state: match session.state {
                        SessionState::Closing => "suspending",
                        SessionState::Stopped => "suspended",
                        _ => session.state.as_str(),
                    }
                    .into(),
                    active: session.state.is_active(),
                    created_at: session.created_at.clone(),
                    updated_at: session.updated_at.clone(),
                    has_error: session.last_error.is_some()
                        || session.configuration_issue(config).is_some(),
                    has_checkpoint: session.checkpoint.is_some(),
                    failed_launch_retry,
                    retained_target: session.target.is_some(),
                    configuration_issue: session.configuration_issue(config),
                    // A session that failed to launch (or a close that left it
                    // dead) carries its reason here so a client need not open
                    // the local diagnostic to learn why. A failed resume rolls
                    // the record back to stopped and leaves its reason in the
                    // same field, so that state reports it too; every
                    // successful transition clears `last_error`. A new turn
                    // also retires a live session's public lifecycle failure.
                    //
                    // A live session's `last_error` is not published here: it
                    // can hold a raw provisioning chain naming profile homes
                    // and SSH hosts. A failed lifecycle operation can leave the
                    // session alive and still owes the person a reason, so the
                    // controller's safe sentence is published whatever state
                    // the session is in (#1081).
                    launch_error: (failed_launch_retry
                        || matches!(session.state, SessionState::Error | SessionState::Stopped))
                    .then(|| session.last_error.clone())
                    .flatten()
                    .or_else(|| session.public_error().map(str::to_owned)),
                    review_trigger: review.trigger(),
                    review_count: review.reviewers.len(),
                    review_unavailable: session.review_unavailable().map(str::to_owned),
                    storage_problem: None,
                    preview: Vec::new(),
                    queued_prompts: Vec::new(),
                    active_user_shells: Vec::new(),
                    background_tasks: Vec::new(),
                    pending_elicitations: Vec::new(),
                    conversation_available: false,
                    prompt_images_supported: false,
                    incompatible_resume_targets: incompatible,
                    resume_refusals,
                    compatible_resume_targets: compatible,
                    project_label: source.short,
                    project_key: project_key(&source.key),
                    display_location: project.project_target(config, &session.target_template_id),
                    additional_mounts: session.additional_mounts.clone(),
                    resource_allocation: session.resource_allocation.clone(),
                    lifecycle,
                    transitioning: false,
                    verdict: Default::default(),
                    latest_event_ordinal: 0,
                    last_activity_at_ms: None,
                    last_message_at_ms: None,
                    activity_details: None,
                    activity: String::new(),
                    operation: None,
                    move_recovery: None,
                    // Both are replaced for every session by the phone
                    // projection, from the one shared activity state.
                    chat_phase: ViewerChatPhase::default(),
                    is_idle: false,
                    activity_state: None,
                    question_message: None,
                    execution_enforcement: session.execution_enforcement(config),
                    policy_config_keys: Vec::new(),
                    config_options: Vec::new(),
                    plan_mode_active: None,
                    turn_review: None,
                    available_commands: Default::default(),
                    // What the durable record alone can justify. The phone server
                    // widens these once it knows whether the session manager holds
                    // the session and what the agent has advertised.
                    capabilities: ViewerSessionCapabilities {
                        dismiss_notification: false,
                        clear_context: false,
                        open: false,
                        read_history: false,
                        prompt: false,
                        run_shell: false,
                        interrupt_turn: false,
                        cancel_operation: false,
                        suspend: false,
                        destroy: true,
                        rename: true,
                        resume: false,
                        move_session: false,
                        set_config: false,
                        set_plan_mode: false,
                        // All four need facts the durable record alone does
                        // not hold -- the workspace count, runtime ownership
                        // -- so the phone projection widens them later.
                        change_workspace: false,
                        container_settings: false,
                        restart: false,
                        interrupt_all: false,
                    },
                }
            })
            .collect();
        let profiles = config
            .enabled_profiles()
            .map(|(id, profile)| ViewerProfile {
                availability: Default::default(),
                id: id.to_owned(),
                harness_kind: profile.kind.id().into(),
                quota_pools: if profile.kind == mj_core::config::HarnessKind::Antigravity {
                    profile
                        .antigravity_pools()
                        .into_iter()
                        .map(|pool| pool.label().to_owned())
                        .collect()
                } else {
                    Vec::new()
                },
                capabilities_key: profile.capabilities_key(id),
                quota: None,
            })
            .collect();
        let disabled_profile_ids = config
            .profiles
            .iter()
            .filter(|(_, profile)| !profile.enabled)
            .map(|(id, _)| id.clone())
            .collect();
        let targets: Vec<_> = config
            .enabled_targets()
            .map(|(id, target)| ViewerTarget {
                id: id.to_owned(),
                kind: target.kind_name().into(),
                permission_mode: target.permission_mode(),
                yolo: target.is_yolo(),
                subagents_configured: config.subagents.is_configured(),
                oracle_configured: config.subagents.oracle_configured(),
                resource_allocation_kind: target.into(),
                requires_project_directory: matches!(
                    target,
                    TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
                ),
                remembered_container_size: mj_core::config::container_size_host(target)
                    .and_then(|host| state.container_sizes.get(host))
                    .copied(),
                container_host_limits: None,
                container_capacity: None,
                container_memory_hard_limit_reason:
                    mj_core::config::container_memory_hard_limit_reason(target)
                        .map(|reason| reason.explanation().to_owned()),
                default_resource_allocation: if mj_core::config::is_container_target(target) {
                    let size = mj_core::state::resolve_default_container_size(
                        target
                            .container()
                            .and_then(|c| c.default_host_container_size()),
                        mj_core::config::container_size_host(target)
                            .and_then(|host| state.container_sizes.get(host))
                            .copied(),
                        None,
                    );
                    Some(SessionResourceAllocation::Container {
                        cpus: size.cpus,
                        memory_bytes: size.memory_bytes,
                    })
                } else {
                    None
                },
                runtime_missing: false,
                default_candidate: config.is_default_target(id),
                availability: Default::default(),
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
            profile_capabilities: Default::default(),
            last_subagent_policies: state.last_subagent_policies.clone(),
            last_oracle_choices: state.last_oracle_choices.clone(),
            oracle_configured: config.subagents.oracle_configured(),
            orchestrators_configured: config.orchestrators_configured(),
            primary_configured: config.subagents.primary_configured(),
            operator_away: state.operator_presence.away,
            auto_afk_minutes: config.advanced.auto_afk_minutes,
            subagents_configured: config.subagents.is_configured(),
            revision,
            generated_at: now_unix().to_string(),
            server_time_ms: mj_core::clock::epoch_millis(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            workspaces: Vec::new(),
            sessions,
            retired_session_ids: Default::default(),
            profiles,
            disabled_profile_ids,
            targets,
            target_availability_notes: Vec::new(),
            bundles,
            review_config: ViewerReviewConfig {
                trigger: config.review.trigger(),
                reviewers: config.review.reviewers.clone(),
            },
            capacity: Vec::new(),
            launch_failures: Vec::new(),
            notices: Vec::new(),
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

fn resume_source_available(
    state: &str,
    has_checkpoint: bool,
    retained_target: bool,
    failed_launch_retry: bool,
) -> bool {
    failed_launch_retry || has_checkpoint || (state == "error" && retained_target)
}

impl ViewerSession {
    /// Recorded repair/restore candidates; the environment owner proves existence.
    pub fn has_resume_source(&self) -> bool {
        resume_source_available(
            &self.state,
            self.has_checkpoint,
            self.retained_target,
            self.failed_launch_retry,
        )
    }

    /// One interpretation for both current-state persistence and live API delivery.
    pub fn api_activity(&self) -> mj_core::storage::ApiActivityState {
        mj_core::storage::ApiActivityState {
            state: self.state.clone(),
            details: self.activity_details.clone(),
            is_idle: self.is_idle,
            waiting_for_input: !self.pending_elicitations.is_empty(),
            capacity_retry: self.capacity_retry.is_some(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerSession {
    #[serde(default)]
    pub attention: mj_core::attention::SessionAttention,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread_frontier: Option<u64>,
    #[serde(default)]
    pub notification_dismissal: Option<mj_core::attention::NotificationDismissal>,
    #[serde(default)]
    pub notification_card: Option<mj_client::runtime_feed::NotificationCard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_holder: Option<String>,
    /// The session raises no notifications; its attention still renders.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notifications_ignored: bool,
    #[serde(default = "oracle_default_enabled")]
    pub oracle: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_choice: Option<mj_core::config::SubagentChoice>,
    #[serde(default)]
    pub review_trigger: mj_core::config::ReviewTrigger,
    #[serde(default)]
    pub review_count: usize,
    /// Why review cannot run for this session, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_unavailable: Option<String>,
    /// The daemon's creation-settings comparison, also used by Restart itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub container_upgrade_supported: bool,
    /// In-process identity used by `mj sessions`; deliberately not part of the
    /// viewer or API JSON contract.
    #[serde(skip, default)]
    pub(crate) is_subagent_session: bool,
    #[serde(skip, default)]
    pub(crate) owning_children_to_suspend: usize,
    /// Whether the session's worker keeps a persistent open request (relay
    /// protocol 64 and later). In-process only, like `is_subagent_session`:
    /// the notification message rule needs it, the JSON contract does not.
    #[serde(skip, default)]
    pub supports_open_request: bool,
    /// Session-detail diagnostics from the same observation as `is_idle`.
    #[serde(skip)]
    pub relay_state: Option<crate::server::api::RelayState>,
    #[serde(default)]
    pub subagents: mj_core::subagent::SubagentPolicy,
    /// Commit the workspace started checked out at, when one was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// Branch created at `at`, or the existing branch checked out without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Diff base the session was started with; `at` unless another was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Whether the workspace started with `at`, `base` and their ancestors
    /// only, as `StartSessionRequest::no_later_history` asked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_later_history: bool,
    #[serde(default)]
    pub targeted_turn_control_supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_controls: Option<Box<mj_core::relay::TurnControls>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_subagents: Vec<mj_core::native_agent::NativeAgent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native_subagent_attention: BTreeMap<String, mj_core::attention::SessionAttention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<Box<mj_core::relay::SteeringOperation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelling_prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<Box<mj_core::relay::CapacityRetry>>,
    #[serde(default)]
    pub retry_assessment_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_recovery: Option<Box<mj_core::continuation::QuotaRecovery>>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_state: Option<mj_core::state::PublicationState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_checkout_kind: Option<mj_core::state::ManagedCheckoutKind>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id: String,
    pub title: String,
    /// The name a person or the harness gave the session, which a rename
    /// prompt starts from. Absent while the listed `title` is only the title
    /// the session was created with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub given_title: Option<String>,
    /// Parent ownership for a borrowed-target child session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_parent_id: Option<String>,
    /// The stable task label chosen by the parent when it spawned this child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_task_name: Option<String>,
    /// Direct children of this parent, workers and reviewers alike. Children
    /// are deliberately never nested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_session_ids: Vec<String>,
    /// The children in `subagent_session_ids` that are reviewers. The rest are
    /// workers, so a client lists each kind from this one field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewer_session_ids: Vec<String>,
    pub harness_kind: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_id: String,
    pub state: String,
    /// `SessionState::is_active`: the one definition of an open session, which
    /// the terminal also uses for its reviewer count.
    #[serde(default)]
    pub active: bool,
    pub created_at: String,
    pub updated_at: String,
    pub has_error: bool,
    /// Whether an archive is available when the environment is absent.
    #[serde(default)]
    pub has_checkpoint: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed_launch_retry: bool,
    /// Internal placement evidence for API admission; existence is probed by Resume.
    #[serde(skip)]
    pub retained_target: bool,
    /// Public identifiers and repair guidance only; never raw runtime errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_issue: Option<String>,
    /// Why a launch failed, for a session that ended in the error state. This
    /// is the same provisioning error text `last_error` already publishes
    /// through `mj events`, surfaced here so `mj sessions`/`mj wait` can show
    /// the reason instead of a bare "failed to launch".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_error: Option<String>,
    /// "disk full: precision-3260 has 0 B free on / …" while the disk this
    /// session's target writes to is full. The daemon's storage owner decides;
    /// automatic recovery waits on the same fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_problem: Option<String>,

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
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub incompatible_resume_targets: Arc<[Arc<str>]>,
    /// The controller's reason for each id in `incompatible_resume_targets`.
    /// It names project paths and SSH hosts, so it is never serialized: only
    /// in-process validation reads it, to tell the caller why a resume or move
    /// was refused.
    #[serde(skip)]
    pub resume_refusals: BTreeMap<Arc<str>, Arc<str>>,
    /// Target ids this session can resume on, so the browser never has to
    /// subtract one set from another to find out.
    #[serde(default, skip_serializing_if = "<[_]>::is_empty")]
    pub compatible_resume_targets: Arc<[Arc<str>]>,
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
    /// Extra directories attached to this session's container, in the same
    /// persisted shape the terminal's container dialog edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_mounts: Vec<AdditionalMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub lifecycle: ViewerLifecycleCategory,
    /// A lifecycle transition temporarily owns this session's conversation.
    /// This remains separate from the coarse lifecycle category so Move can
    /// hide the old transcript while its durable record is still `Running`.
    #[serde(default)]
    pub transitioning: bool,
    #[serde(default)]
    pub verdict: mj_client::runtime_feed::SessionVerdict,
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
    /// When the newest top-level user or agent message in the browser
    /// transcript was recorded, in epoch milliseconds. Tool calls and status
    /// lines do not move it. Absent when the transcript holds no such message
    /// with a recording time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_at_ms: Option<i64>,
    /// Structured live activity, absent when no operational relay snapshot is
    /// available for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_details: Option<ViewerActivityDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Box<ViewerOperation>>,
    /// Safe recovery choices for a failed or cancelled Move. Diagnostics and
    /// checkpoint paths remain on the controller; this contains only the
    /// settings a person may choose again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_recovery: Option<Box<ViewerMoveRecovery>>,
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
    /// Transcript entry containing the current unanswered question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_message: Option<String>,
    /// What this session is doing, in the words the dashboard row uses:
    /// `Turn 43m36s  Step 12s`, `BG 43m36s`, or `[idle]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// The settings the harness advertised, with the values it accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<ViewerConfigOption>,
    #[serde(skip)]
    pub execution_enforcement: Option<mj_core::config::ExecutionEnforcement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_config_keys: Vec<String>,
    /// Whether plan mode is on, or `None` when this harness has no plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_active: Option<bool>,
    /// The review the daemon is running for this session, if any. A phone
    /// shows its status as the daemon delivers the verdict automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_review: Option<Box<ViewerTurnReview>>,
    /// The Mjolnir commands this session accepts, published rather than hardcoded
    /// in the browser: a command list kept in two places is a command list that
    /// drifts, which is how `/review` went missing from the phone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_commands: Arc<Vec<ViewerMjCommand>>,
    pub capabilities: ViewerSessionCapabilities,
}

fn oracle_default_enabled() -> bool {
    true
}

/// The Move recovery a session view carries; the controller derives it from
/// the [`MoveOperation`] it owns.
pub use mj_session_types::ViewerMoveRecovery;

impl ViewerSession {
    /// Apply a resolved controller source while keeping paths and remotes out
    /// of the public projection.
    pub fn set_project_source(&mut self, source: &ProjectSourceIdentity) {
        self.project_label = source.short.clone();
        self.project_key = project_key(&source.key);
    }
}

// One wire representation for the UI and native API activity facts.
pub use mj_store::database::{
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
    pub trigger: mj_core::config::ReviewTrigger,
    #[serde(default)]
    pub reviewers: Vec<mj_core::config::SubagentChoice>,
}

/// A turn review as a phone renders it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTurnReview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    /// What the review is doing, in one line.
    pub status: String,
    /// One row per reviewing agent: its label and where it has got to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<ViewerReviewRole>,
    /// Present once the review has reached a verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ViewerReviewVerdict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewRole {
    pub label: String,
    /// `pending`, `running`, `done`, `findings`, or `failed`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerReviewVerdict {
    /// `clean`, `findings`, or `failed`.
    pub kind: String,
    /// The findings, or the failure's reason.
    pub text: String,
}

impl ViewerTurnReview {
    /// The phone's view of one review the daemon is running.
    #[must_use]
    pub fn from_runtime(review: &mj_client::review::RuntimeReviewView) -> Self {
        Self {
            child_session_id: review.child_session_id.clone(),
            status: review.status.clone(),
            roles: review
                .roles
                .iter()
                .map(|role| ViewerReviewRole {
                    label: role.label.clone(),
                    state: role.state.label().to_owned(),
                    child_session_id: role.child_session_id.clone(),
                })
                .collect(),
            verdict: review.verdict.as_ref().map(|verdict| ViewerReviewVerdict {
                kind: match verdict.kind {
                    mj_client::review::VerdictKind::Clean => "clean",
                    mj_client::review::VerdictKind::Findings => "findings",
                    mj_client::review::VerdictKind::Failed => "failed",
                }
                .to_owned(),
                text: verdict.text.clone(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerWorkspace {
    pub id: String,
    pub name: String,
    /// Workspaces arrive in name order; the viewer opens the most recently
    /// opened one when the route names none.
    pub last_opened_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerQueuedPrompt {
    pub id: String,
    #[serde(
        default,
        skip_serializing_if = "mj_core::state::QueuedCommandKind::is_prompt"
    )]
    pub kind: mj_core::state::QueuedCommandKind,
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
    #[serde(default)]
    pub availability: AvailabilityVerdict,
    #[serde(default)]
    pub capabilities_key: String,
    pub harness_kind: String,
    /// Rows exist before the first reading and survive a failed quota probe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_pools: Vec<String>,
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
    /// Independent model pool, displayed as a separate row for the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_epoch_seconds: Option<i64>,
    #[serde(default)]
    pub reset_countdown_style: mj_client::quota::ResetCountdownStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banked_resets: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
    /// Claude's remaining Fable allowance, attached only to the master weekly
    /// window so surfaces can show both readings without another meter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fable_remaining_percent: Option<u8>,
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
    /// The provider said to wait: no probe before this time. The windows are
    /// the last good reading, taken at `refreshed_at_epoch_seconds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_until_epoch_seconds: Option<u64>,
    /// When the next check runs after a refused one, from
    /// `ProfileQuota::next_check_at`; the viewer only formats it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check_at_epoch_seconds: Option<u64>,
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
    /// Hard CPU and RAM commitments for the container targets on this host.
    /// AWS fleets do not have one shared container budget and leave this
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_capacity: Option<mj_core::state::HostContainerCapacity>,
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
    /// The storage owner's verdict on each filesystem Mjolnir writes to
    /// here; for a fleet, on each instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<ViewerTargetStorage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTargetStorage {
    /// The machine, for a fleet; absent for a plain host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub mount: String,
    pub available_bytes: u64,
    /// Free bytes the filesystem keeps for root, which Mjolnir cannot use.
    pub reserved_bytes: u64,
    pub condition: mj_core::targets::storage::StorageCondition,
    /// The sentence a full or low filesystem owes the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ViewerTargetStorage {
    pub fn list(views: &[&mj_core::targets::storage::TargetStorageView], fleet: bool) -> Vec<Self> {
        use mj_core::targets::storage::StorageCondition;
        views
            .iter()
            .flat_map(|view| {
                view.filesystems.iter().map(move |filesystem| Self {
                    machine: fleet.then(|| view.host.clone()),
                    mount: filesystem.space.mount.clone(),
                    available_bytes: filesystem.space.available_bytes,
                    reserved_bytes: filesystem.space.reserved_bytes,
                    condition: filesystem.condition,
                    detail: (filesystem.condition != StorageCondition::Ok)
                        .then(|| view.explanation(filesystem)),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTarget {
    pub id: String,
    pub kind: String,
    /// The approval mode of a bare target; `None` for an isolated runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<mj_core::config::PermissionMode>,
    /// Whether the harness runs without approvals here, which selects the
    /// YOLO sub-agent and oracle choices.
    #[serde(default)]
    pub yolo: bool,
    /// Whether the Mjolnir sub-agent choices this target would use are
    /// configured. A YOLO target reads its own set.
    #[serde(default)]
    pub subagents_configured: bool,
    /// Whether the oracle this target would use is configured.
    #[serde(default)]
    pub oracle_configured: bool,
    /// Target sizing class, used for create validation and the size picker.
    #[serde(default)]
    pub resource_allocation_kind: ResourceAllocationKind,
    pub requires_project_directory: bool,
    /// Size last selected for this target's physical container host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remembered_container_size: Option<HostContainerSize>,
    /// The latest reported host CPU and memory totals, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_host_limits: Option<HostContainerSize>,
    /// Prefer this hard-budget projection for new-session admission and for
    /// showing the RAM left. `container_host_limits` remains for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_capacity: Option<mj_core::state::HostContainerCapacity>,
    /// Why the target refuses to overcommit RAM, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_memory_hard_limit_reason: Option<String>,
    /// Shared default selected for this container target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_resource_allocation: Option<SessionResourceAllocation>,
    /// The engine command is absent on the daemon host. HTTP admission
    /// checks this installation fact independently of the last probe.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub runtime_missing: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default_candidate: bool,
    #[serde(default)]
    pub availability: AvailabilityVerdict,
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
    #[serde(default)]
    pub clear_context: bool,
    pub open: bool,
    /// A stopped sub-agent whose stored conversation can be read but not
    /// opened live. `open` still gates the live conversation and composer.
    #[serde(default)]
    pub read_history: bool,
    pub prompt: bool,
    pub run_shell: bool,
    /// Cancel the turn the agent is working on now, leaving the session alive.
    pub interrupt_turn: bool,
    /// Cancel the provision, resume or stop currently running.
    pub cancel_operation: bool,
    pub suspend: bool,
    pub destroy: bool,
    pub rename: bool,
    #[serde(default)]
    pub dismiss_notification: bool,
    pub resume: bool,
    /// Prepare and confirm a daemon-owned move to a compatible profile or
    /// target. The browser must never compose Stop and Resume itself.
    #[serde(default)]
    pub move_session: bool,
    pub set_config: bool,
    pub set_plan_mode: bool,
    /// Move the session, and the sub-agents under it, to another workspace.
    #[serde(default)]
    pub change_workspace: bool,
    /// Record the session's container size overrides and attached
    /// directories. Only a session whose target runs in a container has
    /// these, and they apply the next time its container is created.
    #[serde(default)]
    pub container_settings: bool,
    /// Suspend the session if it is live and immediately resume it with the
    /// profile, target and mounts it last ran with. A suspended session needs
    /// a recovery copy to come back from.
    #[serde(default)]
    pub restart: bool,
    /// Stop the turn of this session and of every sub-agent under it,
    /// leaving the sessions alive.
    #[serde(default)]
    pub interrupt_all: bool,
}

/// The published state of a session that has been provisioned and whose
/// start is still connecting its worker. Its record says disconnected, which
/// reads as a fault while it is only launching (F-12).
pub const LAUNCHING_STATE: &str = "launching";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewerOperationKind {
    Create,
    Resume,
    Move,
    Suspend,
    Destroy,
    /// A sub-agent stopped because its parent is being suspended.
    Stop,
    Cleanup,
    Checkpoint,
}

impl ViewerOperationKind {
    pub const fn transition_kind(self) -> Option<SessionTransitionKind> {
        match self {
            Self::Create => Some(SessionTransitionKind::Starting),
            Self::Resume => Some(SessionTransitionKind::Resuming),
            Self::Move => Some(SessionTransitionKind::Moving),
            Self::Suspend => Some(SessionTransitionKind::Suspending),
            Self::Destroy | Self::Cleanup => Some(SessionTransitionKind::Destroying),
            Self::Stop => Some(SessionTransitionKind::Stopping),
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
