use super::*;
use crate::pollers::QUOTA_REFRESH_INTERVAL;
use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions,
};
use mj_core::config::{
    CONFIG_VERSION, Config, HarnessKind, ProjectBundle, ProjectRepository, TargetTemplate,
};
use mj_core::state::SessionState;

#[test]
fn viewer_config_options_publish_current_advertised_values() {
    let make_options = |model, effort| {
        vec![
            SessionConfigOption::select(
                "model_selector",
                "Model",
                model,
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("sonnet", "Claude Sonnet"),
                    SessionConfigSelectOption::new("opus", "Claude Opus"),
                ]),
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "reasoning_effort",
                "Effort",
                effort,
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("high", "High"),
                    SessionConfigSelectOption::new("max", "Maximum"),
                ]),
            ),
        ]
    };
    let options = make_options("sonnet", "high");
    let defaults = mj_core::acp::AcpSessionFacts::from_operational(
        HarnessKind::Claude,
        &BTreeMap::new(),
        &options,
        None,
    );
    let projected = crate::server::viewer_config_options(&options, &defaults);
    assert_eq!(
        projected
            .iter()
            .map(|option| (option.key.as_str(), option.current.as_deref()))
            .collect::<Vec<_>>(),
        [("model", Some("sonnet")), ("effort", Some("high"))]
    );

    let options = make_options("opus", "max");
    let updated = mj_core::acp::AcpSessionFacts::from_operational(
        HarnessKind::Claude,
        &BTreeMap::new(),
        &options,
        None,
    );
    let projected = crate::server::viewer_config_options(&options, &updated);
    assert_eq!(projected[0].current.as_deref(), Some("opus"));
    assert_eq!(projected[1].current.as_deref(), Some("max"));
}

#[tokio::test]
async fn explicit_tls_takes_precedence_over_tailscale_detection() {
    let resolved = resolve_server_args(
        ServerArgs {
            bind: "0.0.0.0:4443".into(),
            tailscale_detect: true,
            tls_cert: Some(PathBuf::from("configured-cert.pem")),
            tls_key: Some(PathBuf::from("configured-key.pem")),
        },
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(resolved.bind, "0.0.0.0:4443".parse().unwrap());
    assert_eq!(resolved.viewer_url, "https://0.0.0.0:4443");
    assert_eq!(
        resolved.tls_files,
        Some((
            PathBuf::from("configured-cert.pem"),
            PathBuf::from("configured-key.pem")
        ))
    );
    assert!(resolved.tailscale.is_none());
    assert!(resolved.fallback_reason.is_none());
}

#[tokio::test]
async fn disabling_detection_keeps_the_viewer_on_loopback() {
    let resolved = resolve_server_args(
        ServerArgs {
            bind: "127.0.0.1:4765".into(),
            tailscale_detect: false,
            tls_cert: None,
            tls_key: None,
        },
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(resolved.bind, "127.0.0.1:4765".parse().unwrap());
    assert_eq!(resolved.viewer_url, "http://127.0.0.1:4765");
    assert!(
        resolved
            .fallback_reason
            .unwrap()
            .contains("detection is disabled")
    );
}

fn bare_preflight_config() -> Config {
    let mut config = Config::default();
    config
        .targets
        .insert("raw".into(), TargetTemplate::LocalBare);
    config
}

#[test]
fn move_recovery_projection_exposes_safe_retry_settings_only() {
    let operation = mj_core::state::MoveOperation {
        in_place: false,
        source_checkpoint_only: false,
        operation_id: "move-1".into(),
        selection: mj_core::state::MoveSelection {
            clear_resource_allocation: true,
            session_id: "session-1".into(),
            profile_id: Some("destination-profile".into()),
            target_template_id: Some("destination-target".into()),
            additional_mounts: Some(vec![crate::targets::AdditionalMount {
                source: "/destination/source".into(),
                destination: "/destination/target".into(),
                access: crate::targets::MountAccess::Ro,
            }]),
            resource_allocation: None,
        },
        source_profile_id: "source-profile".into(),
        source_target_template_id: "source-target".into(),
        source_target: None,
        source_native_session_id: Some("private-native-id".into()),
        source_additional_mounts: vec![crate::targets::AdditionalMount {
            source: "/source/source".into(),
            destination: "/source/target".into(),
            access: crate::targets::MountAccess::Cow,
        }],
        source_resource_allocation: Some(mj_core::state::SessionResourceAllocation::Container {
            cpus: 2,
            memory_bytes: 4096,
        }),
        destination_target: None,
        destination_native_session_id: None,
        destination_store_id: None,
        configuration_fingerprint: "private-fingerprint".into(),
        checkpoint: None,
        recovery_session: None,
        queue: mj_core::state::ResumeQueueDisposition::Start,
        phase: mj_core::state::MovePhase::Cancelled,
        queue_admission_started: false,
        queue_admission_finished: false,
        cancellation_requested: true,
        created_at: "now".into(),
        updated_at: "now".into(),
        error: Some("private path and token".into()),
    };
    let recovery = ViewerMoveRecovery::from_operation(&operation).unwrap();
    assert_eq!(recovery.phase, "cancelled");
    assert_eq!(recovery.source_profile_id, "source-profile");
    assert!(recovery.clear_resource_allocation);
    assert_eq!(recovery.source_additional_mounts.len(), 1);
    assert!(recovery.source_resource_allocation.is_some());
    assert_eq!(recovery.destination_additional_mounts.len(), 1);
    assert!(recovery.destination_resource_allocation.is_none());
    let json = serde_json::to_string(&recovery).unwrap();
    assert!(!json.contains("private-native-id"));
    assert!(!json.contains("private-fingerprint"));
    assert!(!json.contains("private path and token"));
}

#[test]
fn new_preflight_rejects_a_bare_project_without_a_git_head() {
    let error = run_new_preflight(
        bare_preflight_config(),
        "hel".into(),
        "raw".into(),
        Some(PathBuf::from("/definitely/not/a/project")),
    )
    .expect_err("a missing project directory must fail preflight");

    assert!(
        error
            .to_string()
            .contains("project directory does not exist or is not a directory")
    );
}

#[test]
fn new_preflight_accepts_a_git_project_for_a_bare_target() {
    let directory = std::env::current_dir().expect("the test has a working directory");
    let answer = run_new_preflight(
        bare_preflight_config(),
        "hel".into(),
        "raw".into(),
        Some(directory.clone()),
    )
    .expect("the repository running the test has a valid Git HEAD");

    assert!(answer.dirty_repositories.is_empty());
    assert_eq!(answer.project_directory, Some(directory));
}

#[test]
fn new_preflight_requires_network_sources_for_isolated_targets() {
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: mj_core::config::ContainerTemplate {
                build_cache: None,
                image: "test-image".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        },
    );
    config.bundles.insert(
        "hel".into(),
        ProjectBundle {
            primary_repo: "hel".into(),
            repositories: vec![ProjectRepository {
                id: "hel".into(),
                github: None,
                local: Some(PathBuf::from("/definitely/not/a/repository")),
                destination: "hel".into(),
                git_ref: None,
            }],
        },
    );
    let error = run_new_preflight(config, "hel".into(), "podman".into(), None)
        .expect_err("an isolated bundle cannot use a local source");
    assert!(error.to_string().contains("repository"));
}

#[test]
fn a_phone_prompt_becomes_its_text_then_its_images() {
    use agent_client_protocol::schema::v1::ContentBlock;

    let image = |data: &str| crate::server::ViewerPromptImage {
        attachment: None,
        data_base64: data.into(),
        mime_type: "image/png".into(),
        width: 32,
        height: 24,
    };
    let blocks = phone_prompt_blocks(
        "look at this".into(),
        vec![image("aW1hZ2U="), image("c2Vjb25k")],
    );
    let ContentBlock::Text(text) = &blocks[0] else {
        panic!("the prompt leads with its text");
    };
    assert_eq!(text.text, "look at this");
    let ContentBlock::Image(first) = &blocks[1] else {
        panic!("each attachment travels as an image block");
    };
    assert_eq!(first.data, "aW1hZ2U=");
    assert_eq!(first.mime_type, "image/png");
    assert!(matches!(blocks[2], ContentBlock::Image(_)));
    assert_eq!(blocks.len(), 3);

    // An image needs no words with it, and an empty text block would be a
    // message the user never wrote.
    let images_only = phone_prompt_blocks(String::new(), vec![image("aW1hZ2U=")]);
    assert_eq!(images_only.len(), 1);
    assert!(matches!(images_only[0], ContentBlock::Image(_)));
}

#[test]
fn image_prompts_are_offered_only_after_the_agent_advertises_them() {
    use agent_client_protocol::schema::v1::AgentCapabilities;
    use mj_core::relay::{RelayExecutionState, RelayOperationalState};

    let operational = |agent_capabilities| RelayOperationalState {
        goal: Default::default(),
        capacity_retry: None,
        activity_turn_started_at_ms: None,
        session_id: "session-1".into(),
        store_id: None,
        idle_since_ms: None,
        execution: RelayExecutionState::Idle,
        latest_ordinal: 0,
        latest_digest: String::new(),
        acknowledged_through: 0,
        acknowledged_digest: String::new(),
        recovery_floor_ordinal: 0,
        recovery_floor_digest: String::new(),
        native_session_id: None,
        native_continuity_lost: false,
        checkpoint_only: false,
        acp_ready: None,
        agent_capabilities,
        agent_info: None,
        steering_supported: None,
        config_options: Vec::new(),
        modes: None,
        available_commands: Vec::new(),
        config: std::collections::BTreeMap::new(),
        active_prompt: None,
        queued_prompts: Vec::new(),
        active_user_shells: Vec::new(),
        active_agent_terminals: Vec::new(),
        checkpoint_barrier: None,
        checkpoint_ready: None,
        last_acp_activity_at_ms: None,
        current_step_started_at_ms: None,
        foreground_tool_started_at_ms: None,
        tools_in_flight: Vec::new(),
        activity: None,
        harness_turn: None,
        last_harness_turn_started_ordinal: None,
        background_commands: Vec::new(),
        background_work_known: None,
    };

    // A session whose agent has not answered `initialize` has advertised
    // nothing, so the phone is not offered a control the agent may refuse.
    assert!(!agent_accepts_prompt_images(&operational(None)));
    assert!(!agent_accepts_prompt_images(&operational(Some(Box::new(
        AgentCapabilities::default()
    )))));
    let mut capabilities = AgentCapabilities::default();
    capabilities.prompt_capabilities.image = true;
    assert!(agent_accepts_prompt_images(&operational(Some(Box::new(
        capabilities
    )))));
}

/// Issue #1025: while the daemon has no live view of a worker, every session
/// reported `chat_phase: idle` — not a stale value, but the default value of
/// an enum — even when its own durable record said a turn was running. An
/// evaluation driver reading `chat_phase == idle` treated those turns as
/// finished.
#[test]
fn a_session_whose_turn_outlives_the_daemon_is_not_reported_idle() {
    let mut controller = controller_with_profiles(&["claude"]);
    let mut record = phone_session("session-1", 0);
    record.harness_kind = HarnessKind::Claude;
    record.last_profile = "claude".into();
    record.state = SessionState::Running;
    controller.state.sessions.insert(record.id.clone(), record);

    let project = |execution| {
        let materialized_activity = std::collections::BTreeMap::from([(
            "session-1".to_owned(),
            crate::server_runtime::snapshot::MaterializedActivity {
                last_activity_at_ms: Some(7_777),
                execution,
            },
        )]);
        viewer_snapshot(
            &controller,
            &[],
            &std::collections::BTreeMap::new(),
            &PhoneSessionViews {
                conversations: &std::collections::BTreeMap::new(),
                queued_prompts: &std::collections::BTreeMap::new(),
                active_user_shells: &std::collections::BTreeMap::new(),
                pending_elicitations: &std::collections::BTreeMap::new(),
                prompt_images: &std::collections::BTreeSet::new(),
                // No live operational state: this is exactly the window after
                // a daemon restart, before it has reattached to the worker.
                operational: &std::collections::BTreeMap::new(),
                materialized_activity: &materialized_activity,
                project_sources: &PhoneProjectSources::default(),
                operations: &std::collections::BTreeMap::new(),
                move_recoveries: &std::collections::BTreeMap::new(),
                capacity: &[],
                launch_failures: &[],
                reviews: &std::collections::BTreeMap::new(),
            },
            1,
        )
    };

    let running = project(mj_core::state::MaterializedExecutionState::Running {
        started_at_ms: 1_000,
    });
    let session = &running.sessions[0];
    assert_eq!(
        session.chat_phase,
        crate::server::ViewerChatPhase::Running,
        "a turn the durable record knows about is still running"
    );
    assert!(!session.is_idle);
    assert_eq!(
        session
            .activity_state
            .as_ref()
            .map(mj_core::activity::ActivityState::last_known),
        Some(&mj_core::activity::ActivityState::Turn {
            started_at_ms: Some(1_000),
            last_activity_at_ms: None,
        }),
        "the summary says what was last known, and that it is no longer live"
    );

    // A session the daemon cannot see and whose record shows no turn is still
    // not *confirmed* idle, so automation cannot read completion into it.
    let quiet = project(mj_core::state::MaterializedExecutionState::Idle);
    let session = &quiet.sessions[0];
    assert_eq!(session.chat_phase, crate::server::ViewerChatPhase::Idle);
    assert!(
        !session.is_idle,
        "missing operational state is not confirmed idle"
    );
}

#[test]
fn phone_snapshot_projects_capability_gated_and_agent_commands_with_provenance() {
    use agent_client_protocol::schema::v1::{
        AvailableCommand, AvailableCommandInput, SessionMode, SessionModeState,
        UnstructuredCommandInput,
    };
    use mj_core::relay::{RelayExecutionState, RelayOperationalState};

    use crate::server::ViewerCommandSource;

    let mut controller = controller_with_profiles(&["claude"]);
    let mut record = phone_session("session-1", 0);
    record.harness_kind = HarnessKind::Claude;
    record.last_profile = "claude".into();
    record.state = SessionState::Running;
    controller.state.sessions.insert(record.id.clone(), record);
    let operational = RelayOperationalState {
        goal: Default::default(),
        capacity_retry: None,
        activity_turn_started_at_ms: None,
        session_id: "session-1".into(),
        store_id: None,
        idle_since_ms: None,
        execution: RelayExecutionState::Idle,
        latest_ordinal: 0,
        latest_digest: String::new(),
        acknowledged_through: 0,
        acknowledged_digest: String::new(),
        recovery_floor_ordinal: 0,
        recovery_floor_digest: String::new(),
        native_session_id: None,
        native_continuity_lost: false,
        checkpoint_only: false,
        acp_ready: None,
        agent_capabilities: None,
        agent_info: None,
        steering_supported: None,
        config_options: Vec::new(),
        modes: Some(SessionModeState::new(
            "default",
            vec![
                SessionMode::new("default", "Default"),
                SessionMode::new("plan", "Plan"),
            ],
        )),
        available_commands: vec![
            AvailableCommand::new("inspect", " Inspect the workspace ").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(" query ")),
            ),
            AvailableCommand::new("Review", "agent collision"),
            AvailableCommand::new("INSPECT", "duplicate agent command"),
        ],
        config: std::collections::BTreeMap::new(),
        active_prompt: None,
        queued_prompts: Vec::new(),
        active_user_shells: Vec::new(),
        active_agent_terminals: Vec::new(),
        checkpoint_barrier: None,
        checkpoint_ready: None,
        last_acp_activity_at_ms: None,
        current_step_started_at_ms: None,
        foreground_tool_started_at_ms: None,
        tools_in_flight: Vec::new(),
        activity: None,
        harness_turn: None,
        last_harness_turn_started_ordinal: None,
        background_commands: Vec::new(),
        background_work_known: None,
    };
    let mut operational = std::collections::BTreeMap::from([("session-1".into(), operational)]);
    let materialized_activity = std::collections::BTreeMap::from([(
        "session-1".to_owned(),
        crate::server_runtime::snapshot::MaterializedActivity {
            last_activity_at_ms: Some(7_777_i64),
            execution: mj_core::state::MaterializedExecutionState::Idle,
        },
    )]);
    let project = |operational: &std::collections::BTreeMap<String, RelayOperationalState>| {
        viewer_snapshot(
            &controller,
            &[],
            &std::collections::BTreeMap::new(),
            &PhoneSessionViews {
                conversations: &std::collections::BTreeMap::new(),
                queued_prompts: &std::collections::BTreeMap::new(),
                active_user_shells: &std::collections::BTreeMap::new(),
                pending_elicitations: &std::collections::BTreeMap::new(),
                prompt_images: &std::collections::BTreeSet::new(),
                operational,
                materialized_activity: &materialized_activity,
                project_sources: &PhoneProjectSources::default(),
                operations: &std::collections::BTreeMap::new(),
                move_recoveries: &std::collections::BTreeMap::new(),
                capacity: &[],
                launch_failures: &[],
                reviews: &std::collections::BTreeMap::new(),
            },
            1,
        )
    };
    let snapshot = project(&operational);
    let session = &snapshot.sessions[0];

    assert_eq!(session.display_location, "podman");
    assert_eq!(session.last_activity_at_ms, Some(7_777));
    assert_eq!(
        session.activity_details,
        Some(crate::server::ViewerActivityDetails {
            kind: ViewerActivityKind::Idle,
            turn_started_at_ms: None,
            step_started_at_ms: None,
            background_started_at_ms: None,
            idle_since_ms: None,
            last_activity_at_ms: None,
            label: None,
        })
    );

    assert!(session.capabilities.prompt);
    assert!(session.capabilities.set_plan_mode);
    assert_eq!(
        session
            .available_commands
            .iter()
            .map(|command| (command.name.as_str(), command.source))
            .collect::<Vec<_>>(),
        vec![
            ("help", ViewerCommandSource::Mj),
            ("detach", ViewerCommandSource::Mj),
            ("plan", ViewerCommandSource::Mj),
            ("implement", ViewerCommandSource::Mj),
            ("review", ViewerCommandSource::Mj),
            ("inspect", ViewerCommandSource::Agent),
        ]
    );
    let inspect = session.available_commands.last().unwrap();
    assert_eq!(inspect.description, "Inspect the workspace");
    assert_eq!(inspect.argument.as_deref(), Some("query"));

    assert!(session.is_idle);
    assert_eq!(session.activity, "[idle]");
    let state = operational.get_mut("session-1").unwrap();
    state
        .background_commands
        .push(mj_core::relay::BackgroundCommand {
            id: "background-1".into(),
            started_at_ms: 1_000,
            command: "background check".into(),
            can_stop: true,
        });
    let background = project(&operational);
    assert_eq!(
        background.sessions[0].background_tasks,
        vec![ViewerBackgroundTask {
            id: "background-1".into(),
            command: "background check".into(),
            started_at_ms: 1_000,
            can_stop: true,
        }]
    );
    assert!(!background.sessions[0].is_idle);
    assert!(background.sessions[0].activity.starts_with("BG "));
    let state = operational.get_mut("session-1").unwrap();
    state.background_commands.clear();
    // A phase flag alone can be stale; a current SDK step proves work.
    state.execution = RelayExecutionState::Running;
    let stale_running = project(&operational);
    assert!(stale_running.sessions[0].is_idle);
    operational
        .get_mut("session-1")
        .unwrap()
        .current_step_started_at_ms = Some(1_000);
    let running = project(&operational);
    assert!(!running.sessions[0].is_idle);
    assert_ne!(running.sessions[0].activity, "[idle]");
    operational.get_mut("session-1").unwrap().execution = RelayExecutionState::Idle;
    let idle_again = project(&operational);
    assert!(idle_again.sessions[0].is_idle);
    assert_eq!(idle_again.sessions[0].activity, "[idle]");
    let unknown = project(&std::collections::BTreeMap::new());
    assert!(!unknown.sessions[0].is_idle);
    assert!(unknown.sessions[0].activity.is_empty());
    assert!(unknown.sessions[0].activity_details.is_none());
}

#[test]
fn tailscale_listener_preserves_the_configured_port() {
    assert_eq!(
        tailscale_bind("127.0.0.1:4765".parse().unwrap()),
        "0.0.0.0:4765".parse().unwrap()
    );
}

fn controller_with_profiles(ids: &[&str]) -> Controller {
    Controller {
        config: Config {
            build_cache: Default::default(),
            subagents: Default::default(),
            version: CONFIG_VERSION,
            sessions_side: Default::default(),
            advanced: Default::default(),
            show_stopped_sessions: false,
            spinner: Default::default(),
            theme: Default::default(),
            phone: Default::default(),
            review: Default::default(),
            sessionwiki: Default::default(),
            legacy_startup: (),
            profiles: ids
                .iter()
                .map(|id| {
                    (
                        (*id).to_owned(),
                        HarnessProfile {
                            enabled: true,
                            context_window_bytes: None,
                            guardian_review_model: None,
                            kind: HarnessKind::Codex,
                            home: PathBuf::from("/home/agent").join(id),
                            environment: std::collections::BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            bundles: std::collections::BTreeMap::new(),
            targets: std::collections::BTreeMap::new(),
        },
        state: State::default(),
    }
}

fn snapshot_with_project_sources(
    controller: &Controller,
    sources: &PhoneProjectSources,
) -> ViewerSnapshot {
    viewer_snapshot(
        controller,
        &[],
        &Default::default(),
        &PhoneSessionViews {
            conversations: &Default::default(),
            queued_prompts: &Default::default(),
            active_user_shells: &Default::default(),
            pending_elicitations: &Default::default(),
            prompt_images: &Default::default(),
            operational: &Default::default(),
            materialized_activity: &Default::default(),
            project_sources: sources,
            operations: &Default::default(),
            move_recoveries: &Default::default(),
            capacity: &[],
            launch_failures: &[],
            reviews: &Default::default(),
        },
        1,
    )
}

#[tokio::test]
async fn phone_projects_resolve_origins_and_discard_results_after_location_changes() {
    let root = tempfile::tempdir().unwrap();
    let mut controller = controller_with_profiles(&["codex"]);
    controller
        .config
        .targets
        .insert("local".into(), TargetTemplate::LocalBare);
    for (id, origin) in [
        ("first-checkout", "git@github.com:BrokkAi/hel.git"),
        ("second-checkout", "https://github.com/BrokkAi/hel.git"),
    ] {
        let directory = root.path().join(id);
        std::fs::create_dir(&directory).unwrap();
        for args in [vec!["init"], vec!["remote", "add", "origin", origin]] {
            let command = crate::targets::CommandSpec::new(
                "git",
                ["-C".to_owned(), directory.to_string_lossy().into_owned()]
                    .into_iter()
                    .chain(args.into_iter().map(str::to_owned)),
            );
            assert_eq!(ProcessExecutor.execute(&command).unwrap().status, 0);
        }
        let mut record = phone_session(id, 0);
        record.project_directory = Some(directory);
        record.target_template_id = "local".into();
        controller.state.sessions.insert(id.into(), record);
    }
    let mut sources = PhoneProjectSources::default();
    sources.synchronize(&controller);
    assert_eq!(sources.jobs.len(), 2);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = sources.jobs.join_next().await {
            sources.complete(result.unwrap());
        }
    })
    .await
    .unwrap();
    let snapshot = snapshot_with_project_sources(&controller, &sources);
    assert_eq!(
        snapshot.sessions[0].project_key,
        snapshot.sessions[1].project_key
    );
    assert!(
        snapshot
            .sessions
            .iter()
            .all(|session| session.project_label == "hel")
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains(&root.path().to_string_lossy().to_string())
    );
    sources.synchronize(&controller);
    assert!(
        sources.jobs.is_empty(),
        "unchanged inputs reuse the resolved origin"
    );

    let previous = &sources.entries["first-checkout"];
    let late = ProjectSourceResolved {
        cancelled: previous.cancelled.clone(),
        session_id: "first-checkout".into(),
        key: previous.key.clone(),
        result: Ok(ProjectSourceIdentity::git_remote("old/wrong").unwrap()),
    };
    controller
        .state
        .sessions
        .get_mut("first-checkout")
        .unwrap()
        .project_directory = None;
    let snapshot = snapshot_with_project_sources(&controller, &sources);
    assert_ne!(
        snapshot.sessions[0].project_key,
        snapshot.sessions[1].project_key
    );
    sources.synchronize(&controller);
    assert!(late.cancelled.load(Ordering::Acquire));
    sources.complete(late);
    assert!(!sources.entries.contains_key("first-checkout"));
    assert_eq!(snapshot.sessions[0].project_label, "project");
}

#[test]
fn capacity_target_publication_skips_unchanged_targets_and_preserves_readings() {
    let mut controller = controller_with_profiles(&[]);
    controller
        .config
        .targets
        .insert("raw".into(), TargetTemplate::LocalBare);
    let (targets_tx, mut targets_rx) = tokio::sync::watch::channel(Vec::new());
    let mut state = std::collections::BTreeMap::new();

    publish_capacity_targets(&controller, &targets_tx, &mut state);
    assert!(targets_rx.has_changed().expect("target sender is alive"));
    assert_eq!(targets_rx.borrow_and_update().len(), 1);

    let usage = crate::targets::DeploymentCapacityUsage {
        cpu_percent: Some(37),
        memory_used_bytes: 3,
        memory_total_bytes: 4,
        logical_cores: 8,
        disk_total_bytes: Some(5),
    };
    let local = state.get_mut("local").expect("local capacity state");
    local.usage = Some(usage.clone());
    local.on_demand = true;
    local.sampled_at_epoch_seconds = Some(42);
    local.refreshing = false;

    publish_capacity_targets(&controller, &targets_tx, &mut state);
    assert!(!targets_rx.has_changed().expect("target sender is alive"));
    let local_capacity = viewer_capacity(&state)
        .into_iter()
        .find(|capacity| capacity.id == "local")
        .expect("local viewer capacity");
    assert_eq!(local_capacity.cpu_percent, usage.cpu_percent);
    assert_eq!(
        local_capacity.memory_used_bytes,
        Some(usage.memory_used_bytes)
    );
    assert_eq!(local_capacity.logical_cores, Some(usage.logical_cores));
    assert_eq!(local_capacity.sampled_at_epoch_seconds, Some(42));

    controller
        .config
        .targets
        .insert("second-local".into(), TargetTemplate::LocalBare);
    publish_capacity_targets(&controller, &targets_tx, &mut state);
    assert!(targets_rx.has_changed().expect("target sender is alive"));
    assert_eq!(targets_rx.borrow_and_update().len(), 1);
    assert_eq!(state["local"].usage, Some(usage.clone()));

    controller.config.targets.insert(
        "fleet".into(),
        TargetTemplate::AwsEc2 {
            aws_profile: None,
            region: "us-east-1".into(),
            launch_template: "hel-runson".into(),
            launch_template_version: None,
            ssh_user: "ubuntu".into(),
            address_source: Default::default(),
            identity_file: None,
            ssh_args: Vec::new(),
        },
    );
    publish_capacity_targets(&controller, &targets_tx, &mut state);
    assert!(targets_rx.has_changed().expect("target sender is alive"));
    assert_eq!(targets_rx.borrow_and_update().len(), 2);
    assert!(state.contains_key("aws:fleet"));
    assert_eq!(state["local"].usage, Some(usage.clone()));

    controller.config.targets.remove("fleet");
    publish_capacity_targets(&controller, &targets_tx, &mut state);
    assert!(targets_rx.has_changed().expect("target sender is alive"));
    assert_eq!(targets_rx.borrow_and_update().len(), 1);
    assert!(!state.contains_key("aws:fleet"));
    assert_eq!(state["local"].usage, Some(usage));
}

fn prompt_action() -> ControllerAction {
    ControllerAction::Prompt {
        session_id: "session-1".into(),
        text: "ship it".into(),
        images: Vec::new(),
    }
}

fn new_action() -> ControllerAction {
    ControllerAction::New {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: String::new(),
        profile_id: "codex".into(),
        bundle_id: "project".into(),
        target_id: "podman".into(),
        title: Some("Phone launch".into()),
        project_directory: None,
        dirty_ack: Vec::new(),
    }
}

fn phone_session(id: &str, viewed_through_event_ordinal: u64) -> SessionRecord {
    SessionRecord {
        build_cache: None,
        container_workspace: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: id.into(),
        title: "Phone launch".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Provisioning,
        target: None,
        native_session_id: None,
        acp_session_title: None,
        session_title_override: Some("Phone launch".into()),
        created_at: "2026-08-14T00:00:00Z".into(),
        updated_at: "2026-08-14T00:00:00Z".into(),
        viewed_through_event_ordinal,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

#[tokio::test]
async fn controller_reload_does_not_block_the_phone_control_loop() {
    let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let loaded = controller_with_profiles(&[]);
    let release = std::thread::spawn(move || {
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(250));
        release_tx.send(()).unwrap();
    });

    let started = Instant::now();
    spawn_controller_reload_with(completed_tx, move || {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(loaded)
    });
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "scheduling a controller reload occupied the control loop for {:?}",
        started.elapsed()
    );

    let completed = tokio::time::timeout(Duration::from_secs(1), completed_rx.recv())
        .await
        .expect("background reload timed out")
        .expect("background reload channel closed");
    assert!(completed.result.is_ok());
    release.join().unwrap();
}

#[test]
fn read_receipt_only_persists_and_refreshes_when_the_cursor_advances() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut state = State::default();
    state
        .sessions
        .insert(session_id.into(), phone_session(session_id, 5));

    // The viewer re-posts its cursor after every refresh; a repeat must
    // not reach the database and must not move the revision.
    assert_eq!(
        plan_read_receipt(&state, session_id, 5),
        ReadReceiptPlan::AlreadyRead
    );
    assert_eq!(
        plan_read_receipt(&state, session_id, 4),
        ReadReceiptPlan::AlreadyRead
    );
    assert_eq!(
        plan_read_receipt(&state, "missing", 9),
        ReadReceiptPlan::UnknownSession
    );
    assert_eq!(
        plan_read_receipt(&state, session_id, 9),
        ReadReceiptPlan::Persist
    );

    assert!(apply_read_receipt(&mut state, session_id, 9));
    assert_eq!(state.sessions[session_id].viewed_through_event_ordinal, 9);
    assert!(!apply_read_receipt(&mut state, session_id, 9));
    assert!(!apply_read_receipt(&mut state, session_id, 7));
    assert!(!apply_read_receipt(&mut state, "missing", 9));
    assert_eq!(
        plan_read_receipt(&state, session_id, 9),
        ReadReceiptPlan::AlreadyRead
    );
}

#[tokio::test]
async fn an_admitted_action_answers_its_phone_before_the_work_runs() {
    let mut replies = PendingActionReplies::default();
    let (reply, answer) = tokio::sync::oneshot::channel();

    replies.accept(1, &prompt_action(), reply);

    // No completion has been reported, and the phone already has its
    // answer: holding it until the action finished is what mobile
    // networks time out on.
    assert_eq!(answer.await.unwrap(), ActionOutcome::accepted());
}

#[tokio::test]
async fn a_new_action_answers_once_its_provisional_session_is_published() {
    let mut replies = PendingActionReplies::default();
    let (reply, mut answer) = tokio::sync::oneshot::channel();

    replies.accept(7, &new_action(), reply);
    assert!(
        matches!(
            answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "a new session has no id to report before it is published"
    );

    replies.resolve(7, ActionOutcome::accepted());
    assert_eq!(answer.await.unwrap(), ActionOutcome::accepted());
}

#[tokio::test]
async fn a_new_action_that_never_publishes_still_answers_its_phone() {
    let mut replies = PendingActionReplies::default();
    let (reply, answer) = tokio::sync::oneshot::channel();
    replies.accept(7, &new_action(), reply);

    // Registration failed before the session reached the loop, which is
    // the completion path rather than the publication path.
    let failed = ActionOutcome::Failed {
        reference: "1234-7".to_owned(),
    };
    replies.resolve(7, failed.clone());

    assert_eq!(answer.await.unwrap(), failed);
    // A second resolution is a no-op, so a completion after a publication
    // cannot overwrite the answer already sent.
    replies.resolve(7, ActionOutcome::accepted());
}

#[test]
fn force_close_is_admitted_like_close() {
    let mut active = std::collections::BTreeSet::from(["session-1".to_owned()]);
    let force_close = ControllerAction::ForceClose {
        session_id: "session-1".into(),
        delete_branch: false,
    };
    // A full action pool and a session already busy with a stuck close
    // are both exactly when a force close has to get through.
    assert_eq!(
        admit_phone_action(&force_close, MAX_CONCURRENT_PHONE_ACTIONS, &mut active),
        Ok(Some("session-1".into()))
    );
    assert_eq!(
        controller_action_session_id(&force_close),
        Some("session-1".to_owned())
    );
}

#[test]
fn close_is_admitted_while_provisioning_occupies_a_full_action_pool() {
    let mut active = std::collections::BTreeSet::from(["session-1".to_owned()]);
    let close = ControllerAction::Close {
        session_id: "session-1".into(),
    };
    assert_eq!(
        admit_phone_action(&close, MAX_CONCURRENT_PHONE_ACTIONS, &mut active),
        Ok(Some("session-1".into()))
    );
    assert_eq!(
        admit_phone_action(&prompt_action(), 0, &mut active),
        Err(ActionOutcome::SessionBusy)
    );
}

#[test]
fn a_refused_action_reports_the_reason_the_phone_can_act_on() {
    let mut active = std::collections::BTreeSet::new();

    assert_eq!(
        admit_phone_action(&prompt_action(), 0, &mut active),
        Ok(Some("session-1".to_owned()))
    );
    assert_eq!(
        admit_phone_action(&prompt_action(), 1, &mut active),
        Err(ActionOutcome::SessionBusy)
    );
    assert_eq!(
        admit_phone_action(&new_action(), MAX_CONCURRENT_PHONE_ACTIONS, &mut active),
        Err(ActionOutcome::Busy)
    );
    // A refusal must not consume the session slot it did not take.
    assert_eq!(active.len(), 1);
    assert_eq!(admit_phone_action(&new_action(), 1, &mut active), Ok(None));
}

#[test]
fn a_feed_that_ends_outside_shutdown_names_the_failure() {
    assert!(feed_stopped(true, "the session manager stopped").is_none());
    let failure = feed_stopped(false, "the session manager stopped").expect("named failure");
    assert!(failure.to_string().contains("session manager"));
}

#[test]
fn a_profile_added_while_the_server_runs_reaches_the_quota_refresher() {
    let (profiles_tx, profiles_rx) = tokio::sync::watch::channel(QuotaRefreshBatch::default());
    let mut published = std::collections::BTreeMap::new();
    let mut batch = QuotaRefreshBatch::default();
    let controller = controller_with_profiles(&["codex"]);

    assert!(republish_quota_profiles(
        &controller,
        &mut published,
        &mut batch,
        &profiles_tx
    ));
    assert_eq!(
        profiles_rx
            .borrow()
            .profiles
            .iter()
            .map(|profile| profile.profile_id.clone())
            .collect::<Vec<_>>(),
        vec!["codex".to_owned()]
    );
    let first_generation = profiles_rx.borrow().generation;

    // Every finished action reloads the configuration; an unchanged one
    // must not restart a harness process per profile.
    assert!(!republish_quota_profiles(
        &controller,
        &mut published,
        &mut batch,
        &profiles_tx
    ));
    assert_eq!(profiles_rx.borrow().generation, first_generation);

    let grown = controller_with_profiles(&["claude", "codex"]);
    assert!(republish_quota_profiles(
        &grown,
        &mut published,
        &mut batch,
        &profiles_tx
    ));
    assert_eq!(
        profiles_rx
            .borrow()
            .profiles
            .iter()
            .map(|profile| profile.profile_id.clone())
            .collect::<Vec<_>>(),
        vec!["claude".to_owned(), "codex".to_owned()]
    );
    assert!(profiles_rx.borrow().generation > first_generation);
}

#[test]
fn a_quota_reads_stale_only_once_its_next_refresh_is_overdue() {
    let controller = controller_with_profiles(&["codex"]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let quota_refreshed = |age: Duration| {
        let quotas = std::collections::BTreeMap::from([(
            "codex".to_owned(),
            ProfileQuota {
                profile_id: "codex".into(),
                harness: HarnessKind::Codex,
                windows: Vec::new(),
                extra: None,
                error: None,
                refreshed_at_epoch_seconds: now - age.as_secs(),
            },
        )]);
        viewer_snapshot(
            &controller,
            &[],
            &quotas,
            &PhoneSessionViews {
                conversations: &std::collections::BTreeMap::new(),
                queued_prompts: &std::collections::BTreeMap::new(),
                active_user_shells: &std::collections::BTreeMap::new(),
                pending_elicitations: &std::collections::BTreeMap::new(),
                prompt_images: &std::collections::BTreeSet::new(),
                operational: &std::collections::BTreeMap::new(),
                materialized_activity: &std::collections::BTreeMap::new(),
                project_sources: &PhoneProjectSources::default(),
                operations: &std::collections::BTreeMap::new(),
                move_recoveries: &std::collections::BTreeMap::new(),
                capacity: &[],
                launch_failures: &[],
                reviews: &std::collections::BTreeMap::new(),
            },
            1,
        )
        .profiles[0]
            .quota
            .as_ref()
            .expect("the profile carries its quota")
            .stale
    };

    // A reading taken one refresh interval ago is exactly what a healthy
    // refresher produces, so it must not be labelled stale.
    assert!(!quota_refreshed(QUOTA_REFRESH_INTERVAL));
    assert!(!quota_refreshed(QUOTA_STALE_AFTER));
    assert!(quota_refreshed(QUOTA_STALE_AFTER + Duration::from_secs(1)));
}

#[test]
fn phone_action_capacity_is_bounded() {
    assert!(phone_action_capacity_available(
        MAX_CONCURRENT_PHONE_ACTIONS - 1
    ));
    assert!(!phone_action_capacity_available(
        MAX_CONCURRENT_PHONE_ACTIONS
    ));
}

#[test]
fn started_phone_session_is_visible_and_mapped_before_provisioning() {
    let session_id = "0123456789abcdef0123456789abcdef";
    let session = phone_session(session_id, 0);
    let mut state = State::default();
    let mut active_actions = std::collections::BTreeSet::new();
    let mut action_sessions = std::collections::BTreeMap::new();

    track_started_phone_session(
        &mut state,
        &mut active_actions,
        &mut action_sessions,
        7,
        session,
    )
    .unwrap();

    assert_eq!(state.sessions[session_id].state, SessionState::Provisioning);
    assert_eq!(state.sessions[session_id].display_title(), "Phone launch");
    assert!(active_actions.contains(session_id));
    assert_eq!(
        action_sessions.get(&7).map(String::as_str),
        Some(session_id)
    );
}

#[test]
fn failed_launch_notice_survives_session_rollback_and_history_is_bounded() {
    let controller = controller_with_profiles(&["codex"]);
    let mut failures = Vec::new();
    for index in 0..20 {
        record_launch_failure(
            &mut failures,
            index,
            format!("workspace-{index}"),
            Some(format!("session-{index}")),
            Some(format!("worker bootstrap failed for {index}")),
        );
    }
    let snapshot = viewer_snapshot(
        &controller,
        &[],
        &std::collections::BTreeMap::new(),
        &PhoneSessionViews {
            conversations: &std::collections::BTreeMap::new(),
            queued_prompts: &std::collections::BTreeMap::new(),
            active_user_shells: &std::collections::BTreeMap::new(),
            pending_elicitations: &std::collections::BTreeMap::new(),
            prompt_images: &std::collections::BTreeSet::new(),
            operational: &std::collections::BTreeMap::new(),
            materialized_activity: &std::collections::BTreeMap::new(),
            project_sources: &PhoneProjectSources::default(),
            operations: &std::collections::BTreeMap::new(),
            move_recoveries: &std::collections::BTreeMap::new(),
            capacity: &[],
            launch_failures: &failures,
            reviews: &std::collections::BTreeMap::new(),
        },
        1,
    );
    assert!(snapshot.sessions.is_empty());
    assert_eq!(failures.len(), 16);
    assert_eq!(failures[0].workspace_id, "workspace-4");
    assert_eq!(failures[15].workspace_id, "workspace-19");
    assert_ne!(failures[0].id, failures[1].id);
    assert_eq!(
        failures[15].session_id.as_deref(),
        Some("session-19"),
        "a wait on that session has to be able to recognize its own launch failure"
    );
    let json = serde_json::to_value(snapshot).unwrap();
    assert_eq!(json["launch_failures"][15]["workspace_id"], "workspace-19");
    assert_eq!(
        json["launch_failures"][15]["error"], "worker bootstrap failed for 19",
        "the recorded failure carries its reason so a client can show it"
    );
    assert_eq!(json["launch_failures"][15].as_object().unwrap().len(), 4);
}

#[test]
fn a_refused_action_reports_its_reason_and_any_other_failure_reports_a_reference() {
    let refused = PhoneActionFailure::of(
        &anyhow::Error::new(Refusal::precondition(
            "this instance has no workspace yet; create one before starting a session",
        ))
        .context("start a phone session"),
    );
    assert_eq!(
        refused.outcome("77-4"),
        ActionOutcome::Refused(Refusal::precondition(
            "this instance has no workspace yet; create one before starting a session"
        ))
    );

    let internal = PhoneActionFailure::of(
        &anyhow::anyhow!("ssh host build-07 refused the connection")
            .context("provision the target"),
    );
    assert_eq!(
        internal.outcome("77-4"),
        ActionOutcome::Failed {
            reference: "77-4".to_owned()
        },
        "an unmarked failure must not put its own text on the wire"
    );
    assert!(
        internal.detail.contains("build-07"),
        "the daemon log still gets the whole chain: {}",
        internal.detail
    );
}

#[test]
fn a_later_successful_action_clears_a_session_s_recorded_failure() {
    let mut pending = std::collections::BTreeMap::new();

    record_action_result(
        &mut pending,
        Some("session-1"),
        &Err(PhoneActionFailure::internal("relay hiccup")),
    );
    assert_eq!(
        pending.get("session-1").map(String::as_str),
        Some("relay hiccup")
    );

    record_action_result(&mut pending, Some("session-2"), &Ok(()));
    record_action_result(&mut pending, Some("session-1"), &Ok(()));
    assert!(
        pending.is_empty(),
        "the overlay has no other expiry, so a stale error would badge the session forever"
    );

    // A completion with no session cannot clear or record anything.
    record_action_result(
        &mut pending,
        None,
        &Err(PhoneActionFailure::internal("orphaned")),
    );
    assert!(pending.is_empty());
}

#[test]
fn phone_cancel_targets_the_matching_background_action() {
    let first = PhoneActionControl {
        cancelled: Arc::new(AtomicBool::new(false)),
        create: None,
    };
    let second = PhoneActionControl {
        cancelled: Arc::new(AtomicBool::new(false)),
        create: None,
    };
    let action_sessions =
        std::collections::BTreeMap::from([(1, "session-1".into()), (2, "session-2".into())]);
    let cancellations = std::collections::BTreeMap::from([(1, first.clone()), (2, second.clone())]);

    assert!(request_phone_action_cancellation(
        "session-2",
        &action_sessions,
        &cancellations,
    ));
    assert!(!first.cancelled.load(Ordering::Acquire));
    assert!(second.cancelled.load(Ordering::Acquire));
    assert!(!request_phone_action_cancellation(
        "missing",
        &action_sessions,
        &cancellations,
    ));
}

#[test]
fn phone_new_cancel_and_running_commit_have_one_atomic_winner() {
    for _ in 0..100 {
        let create = CreateSessionControl::default();
        let control = PhoneActionControl {
            cancelled: create.cancelled.clone(),
            create: Some(create),
        };
        let cancelling = control.clone();
        let committing = control.clone();
        let (cancelled, committed) = std::thread::scope(|scope| {
            let cancel = scope.spawn(move || cancelling.request_cancel());
            let commit = scope.spawn(move || committing.grant_new_commit());
            (cancel.join().unwrap(), commit.join().unwrap())
        });

        assert_ne!(cancelled, committed);
        assert_eq!(control.cancelled.load(Ordering::Acquire), cancelled);
        assert!(!control.request_cancel());
        assert!(!control.grant_new_commit());
    }
}

fn materialized_at(ordinal: u64) -> MaterializedSession {
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = ordinal;
    materialized.applied_event_digest = format!("digest-{ordinal}");
    materialized
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn browser_projection_enqueue_does_not_wait_for_a_blocking_permit() {
    let (results, mut completed) = tokio::sync::mpsc::channel(1);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut dispatcher = ConversationProjectionDispatcher::with_permits(results, shutdown, 0);

    dispatcher.enqueue(materialized_at(1));
    let (progress, progress_done) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        progress.send(()).expect("control task is still alive");
    });

    tokio::time::timeout(Duration::from_millis(100), progress_done)
        .await
        .expect("control progress was starved by transcript projection")
        .expect("progress task did not report");
    assert!(completed.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn browser_projection_keeps_only_the_newest_pending_snapshot() {
    let (results, mut completed) = tokio::sync::mpsc::channel(4);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut dispatcher = ConversationProjectionDispatcher::with_permits(results, shutdown, 0);

    dispatcher.enqueue(materialized_at(1));
    dispatcher.enqueue(materialized_at(3));
    dispatcher.enqueue(materialized_at(2));
    assert_eq!(dispatcher.pending["session-1"].key.ordinal, 3);

    dispatcher.permits.add_permits(1);
    let first = tokio::time::timeout(Duration::from_secs(1), completed.recv())
        .await
        .expect("first projection did not complete")
        .expect("projection result channel closed");
    assert_eq!(first.key.ordinal, 1);
    assert!(dispatcher.finish(first, true).is_some());

    let second = tokio::time::timeout(Duration::from_secs(1), completed.recv())
        .await
        .expect("coalesced projection did not complete")
        .expect("projection result channel closed");
    assert_eq!(second.key.ordinal, 3);
    assert!(dispatcher.finish(second, true).is_some());
    assert!(dispatcher.pending.is_empty());
    assert!(dispatcher.in_flight.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn forgotten_projection_cannot_repopulate_after_a_same_cursor_resume() {
    let (results, mut completed) = tokio::sync::mpsc::channel(4);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut dispatcher = ConversationProjectionDispatcher::with_permits(results, shutdown, 0);
    let snapshot = materialized_at(7);

    dispatcher.enqueue(snapshot.clone());
    dispatcher.forget("session-1");
    dispatcher.enqueue(snapshot);
    dispatcher.permits.add_permits(1);

    let old = tokio::time::timeout(Duration::from_secs(1), completed.recv())
        .await
        .expect("old projection did not complete")
        .expect("projection result channel closed");
    assert!(dispatcher.finish(old, true).is_none());

    let current = tokio::time::timeout(Duration::from_secs(1), completed.recv())
        .await
        .expect("resumed projection did not complete")
        .expect("projection result channel closed");
    assert!(dispatcher.finish(current, true).is_some());
    assert!(dispatcher.in_flight.is_empty());
}
