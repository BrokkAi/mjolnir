use super::*;
use crate::config::{
    CONFIG_VERSION, ContainerTemplate, HarnessProfile, ProjectBundle, ProjectRepository,
    TargetTemplate,
};
use crate::targets::MountAccess;

fn user_item(position: u64, text: &str) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        stable_id: format!("user:{position}"),
        position,
        latest_content_event_ordinal: None,
        created_at_ms: 1_000,
        last_changed_at_ms: 1_000,
        body: TranscriptBody::User {
            content: vec![serde_json::json!({"type": "text", "text": text})],
        },
    })
}

fn agent_item(position: u64) -> Arc<TranscriptItem> {
    Arc::new(TranscriptItem {
        stable_id: format!("agent:{position}"),
        position,
        latest_content_event_ordinal: Some(position),
        created_at_ms: 1_000,
        last_changed_at_ms: 1_000,
        body: TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": "working"},
                "messageId": "answer"
            })],
            streaming: false,
        },
    })
}

fn snapshot(session: MaterializedSession, window: ProjectionWindow) -> ManagedSessionSnapshot {
    ManagedSessionSnapshot {
        materialized: session,
        window,
        worker_build: None,
        subagent_requests: Vec::new(),
        subagent_results: Vec::new(),
        operational: serde_json::from_value(serde_json::json!({
            "session_id": "session-1",
            "execution": "idle",
            "latest_ordinal": 0,
            "latest_digest": crate::relay::RELAY_EVENT_GENESIS_DIGEST,
            "acknowledged_through": 0,
            "acknowledged_digest": crate::relay::RELAY_EVENT_GENESIS_DIGEST,
            "recovery_floor_ordinal": 0,
            "recovery_floor_digest": crate::relay::RELAY_EVENT_GENESIS_DIGEST,
            "native_session_id": null,
            "agent_capabilities": null,
            "agent_info": null,
            "config_options": [],
            "available_commands": [],
            "config": {},
            "active_prompt": null,
            "queued_prompts": [],
            "checkpoint_barrier": null,
            "checkpoint_ready": null,
        }))
        .expect("an idle operational state"),
        latest_credential_sync_signal: None,
    }
}

/// A polled projection carries only the end of the transcript. The title
/// comes from the first user message and the completed turn from the last
/// one, so both have to survive the head being outside the window.
#[test]
fn a_windowed_projection_answers_the_same_title_and_turn_as_a_whole_one() {
    let mut whole = MaterializedSession::empty("session-1");
    whole.transcript = vec![
        user_item(1, "build the relay"),
        agent_item(2),
        agent_item(3),
        user_item(4, "now test it"),
        agent_item(5),
    ];
    let complete = snapshot(whole.clone(), ProjectionWindow::of(&whole));

    // The same session, loaded as a two-item window: the head is gone and
    // so is the last user message.
    let mut windowed_session = whole.clone();
    windowed_session.transcript = whole.transcript[3..].to_vec();
    let mut windowed = snapshot(windowed_session, ProjectionWindow::of(&whole));
    windowed.window.omitted_items = 3;

    assert_eq!(
        complete.resolved_title().as_deref(),
        Some("build the relay")
    );
    assert_eq!(windowed.resolved_title(), complete.resolved_title());
    assert_eq!(complete.latest_completed_turn_ordinal(), Some(4));
    assert_eq!(
        windowed.latest_completed_turn_ordinal(),
        complete.latest_completed_turn_ordinal()
    );
}

/// A session still working has not completed a turn, whatever its
/// transcript says.
#[test]
fn a_running_session_reports_no_completed_turn() {
    let mut session = MaterializedSession::empty("session-1");
    session.transcript = vec![user_item(1, "build it")];
    session.execution = MaterializedExecutionState::Running { started_at_ms: 1 };
    let window = ProjectionWindow::of(&session);

    assert_eq!(
        snapshot(session, window).latest_completed_turn_ordinal(),
        None
    );
}

#[test]
fn fast_mode_configuration_uses_its_user_facing_toggle_command() {
    assert_eq!(config_command_text("fast-mode", "on"), "/fast");
    assert_eq!(config_command_text("fast-mode", "off"), "/fast");
    assert_eq!(config_command_text("model", "sol"), "/model sol");
}

fn sample_state() -> State {
    let session = SessionRecord {
        build_cache: None,
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: crate::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        container_workspace: None,
        id: "0123456789abcdef".into(),
        title: "Build Hel".into(),
        harness_kind: HarnessKind::Codex,
        last_profile: "codex-1".into(),
        bundle_id: "hel".into(),
        project_directory: None,
        managed_worktree: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: vec![AdditionalMount {
            source: PathBuf::from("/home/test/cache"),
            destination: PathBuf::from("/mnt/cache"),
            access: MountAccess::Cow,
        }],
        state: SessionState::Running,
        target: Some(TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "afb67d".into(),
            workspace_storage: Default::default(),
        }),
        native_session_id: Some("native-1".into()),
        acp_session_title: Some("Build Hel".into()),
        session_title_override: None,
        created_at: "2026-08-09T12:00:00Z".into(),
        updated_at: "2026-08-09T12:01:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: Some(CheckpointMetadata {
            archive_path: PathBuf::from("sessions/0123456789abcdef.hel.zip"),
            sha256: "a".repeat(64),
            created_at: "2026-08-09T12:01:00Z".into(),
            event_frontier: 42,
        }),
    };
    State {
        version: STATE_VERSION,
        sessions: BTreeMap::from([(session.id.clone(), session)]),
        subagents: BTreeMap::new(),
        mount_history: BTreeMap::from([("local".into(), vec![PathBuf::from("/home/test/cache")])]),
        container_sizes: BTreeMap::new(),
    }
}

fn sample_config() -> Config {
    Config {
        build_cache: Default::default(),
        advanced: Default::default(),
        version: CONFIG_VERSION,
        sessions_side: Default::default(),
        show_stopped_sessions: false,
        spinner: Default::default(),
        theme: Default::default(),
        phone: Default::default(),
        review: Default::default(),
        sessionwiki: Default::default(),
        subagents: Default::default(),
        legacy_startup: (),
        profiles: BTreeMap::from([(
            "codex-1".into(),
            HarnessProfile {
                enabled: true,
                context_window_bytes: None,
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/test/.codex"),
                environment: BTreeMap::new(),
                guardian_review_model: None,
            },
        )]),
        bundles: BTreeMap::from([(
            "hel".into(),
            ProjectBundle {
                primary_repo: "hel".into(),
                repositories: vec![ProjectRepository {
                    id: "hel".into(),
                    github: Some("BrokkAi/hel".into()),
                    local: None,
                    destination: PathBuf::from("hel"),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    build_cache: None,
                    image: "ubuntu:24.04".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        )]),
    }
}

fn sample_session() -> SessionRecord {
    sample_state()
        .sessions
        .remove("0123456789abcdef")
        .expect("sample session")
}

#[test]
fn session_records_written_before_container_overrides_still_load() {
    let session = sample_session();
    let mut json = serde_json::to_value(&session).expect("serialize session");
    let object = json.as_object_mut().expect("session object");
    assert!(object.remove("container_cpus").is_none());
    assert!(object.remove("container_memory").is_none());

    let loaded: SessionRecord = serde_json::from_value(json).expect("load older session");
    assert_eq!(loaded.container_cpus, None);
    assert_eq!(loaded.container_memory, None);
    assert_eq!(loaded, session);

    let mut edited = session.clone();
    edited.container_cpus = Some("4".into());
    edited.container_memory = Some("8g".into());
    let round_tripped: SessionRecord =
        serde_json::from_str(&serde_json::to_string(&edited).expect("serialize"))
            .expect("reload edited session");
    assert_eq!(round_tripped, edited);
}

#[test]
fn container_size_history_rejects_invalid_keys_and_values() {
    let mut state = State::default();
    state.container_sizes.insert(
        String::new(),
        HostContainerSize {
            cpus: 8,
            memory_bytes: 32,
        },
    );
    assert!(
        state
            .validate()
            .unwrap_err()
            .to_string()
            .contains("empty host")
    );

    state.container_sizes = BTreeMap::from([(
        "local".into(),
        HostContainerSize {
            cpus: 0,
            memory_bytes: 32,
        },
    )]);
    assert!(state.validate().unwrap_err().to_string().contains("zero"));
}

#[test]
fn project_name_prefers_a_worktree_source_then_a_project_directory_then_the_bundle() {
    let mut config = sample_config();
    config
        .bundles
        .get_mut("hel")
        .expect("bundle")
        .repositories
        .push(ProjectRepository {
            id: "docs".into(),
            github: Some("BrokkAi/docs".into()),
            local: None,
            destination: PathBuf::from("documentation"),
            git_ref: None,
        });
    let mut session = sample_session();

    assert_eq!(session.project_name(&config), "docs + hel");

    session.project_directory = Some(PathBuf::from("/home/test/Projects/raw-project"));
    assert_eq!(session.project_name(&config), "raw-project");

    session.project_directory = Some(PathBuf::from(
        "/home/test/Projects/source/.mj/worktrees/0123456789abcdef",
    ));
    session.managed_worktree = Some(ManagedWorktree {
        source_project_directory: PathBuf::from("/home/test/Projects/source"),
        source_repository: PathBuf::from("/home/test/Projects/source"),
        worktree_root: PathBuf::from("/home/test/Projects/source/.mj/worktrees/0123456789abcdef"),
        branch: "mj/0123456789abcdef".into(),
        target: ManagedWorktreeTarget::Local,
        base_commit: None,
    });
    assert_eq!(session.project_name(&config), "source");
}

#[test]
fn bundle_project_name_uses_the_primary_github_repository_name() {
    let mut config = sample_config();
    config.bundles.insert(
        "bifrost".into(),
        ProjectBundle {
            primary_repo: "bifrost".into(),
            repositories: vec![ProjectRepository {
                id: "bifrost".into(),
                github: Some("BrokkAi/bifrost-dev".into()),
                local: None,
                destination: PathBuf::from("bifrost"),
                git_ref: None,
            }],
        },
    );
    let mut session = sample_session();
    session.bundle_id = "bifrost".into();

    assert_eq!(session.project_name(&config), "bifrost-dev");
    assert_eq!(
        session.project_source(&config),
        ProjectSourceIdentity {
            key: "github:brokkai/bifrost-dev".into(),
            short: "bifrost-dev".into(),
            full: "BrokkAi/bifrost-dev".into(),
        }
    );
}

#[test]
fn bundle_project_name_uses_a_local_source_or_bundle_id_fallback() {
    let mut config = sample_config();
    config.bundles.insert(
        "local-bundle".into(),
        ProjectBundle {
            primary_repo: "local".into(),
            repositories: vec![ProjectRepository {
                id: "local".into(),
                github: None,
                local: Some(PathBuf::from("/home/test/Projects/bifrost-dev")),
                destination: PathBuf::from("bifrost"),
                git_ref: None,
            }],
        },
    );
    let mut session = sample_session();
    session.bundle_id = "local-bundle".into();

    assert_eq!(session.project_name(&config), "bifrost-dev");
    assert_eq!(
        session.project_source(&config),
        ProjectSourceIdentity {
            key: "path:/home/test/Projects/bifrost-dev".into(),
            short: "bifrost-dev".into(),
            full: "/home/test/Projects/bifrost-dev".into(),
        }
    );

    session.bundle_id = "missing-bundle".into();
    assert_eq!(session.project_name(&config), "missing-bundle");
    assert_eq!(
        session.project_source(&config),
        ProjectSourceIdentity {
            key: "bundle:missing-bundle".into(),
            short: "missing-bundle".into(),
            full: "missing-bundle".into(),
        }
    );

    let mut other_missing = session.clone();
    other_missing.bundle_id = "another-missing-bundle".into();
    assert_ne!(
        session.project_source(&config).key,
        other_missing.project_source(&config).key
    );
}

#[test]
fn project_target_adds_the_raw_project_name_only_for_bare_targets() {
    let mut config = sample_config();
    config
        .targets
        .insert("localhost".into(), TargetTemplate::LocalBare);
    let mut session = sample_session();
    session.project_directory = Some(PathBuf::from("/mnt/optane/bifrost-fird"));

    assert_eq!(session.project_target(&config, "podman"), "podman");
    assert_eq!(
        session.project_target(&config, "localhost"),
        "localhost/bifrost-fird"
    );
    assert_eq!(
        session.project_target(&config, "retired-target"),
        "retired-target"
    );
}

#[test]
fn project_source_uses_bundle_repository_and_ignores_managed_worktree_destinations() {
    let config = sample_config();
    let mut session = sample_session();
    let source = session.project_source(&config);
    assert_eq!(source.key, "github:brokkai/hel");
    assert_eq!(source.short, "hel");
    assert_eq!(source.full, "BrokkAi/hel");
    assert_eq!(
        ProjectSourceIdentity::git_remote("git@github.com:BrokkAi/bifrost-dev.git"),
        ProjectSourceIdentity::git_remote("https://github.com/BrokkAi/bifrost-dev.git")
    );
    assert_ne!(
        ProjectSourceIdentity::git_remote("BrokkAi/bifrost-dev"),
        ProjectSourceIdentity::git_remote("OtherOrg/bifrost-dev")
    );

    session.project_directory = Some(PathBuf::from(
        "/home/test/Projects/source/.mj/worktrees/0123456789abcdef",
    ));
    session.managed_worktree = Some(ManagedWorktree {
        source_project_directory: PathBuf::from("/home/test/Projects/source/crate"),
        source_repository: PathBuf::from("/home/test/Projects/source"),
        worktree_root: PathBuf::from("/home/test/Projects/source/.mj/worktrees/0123456789abcdef"),
        branch: "mj/0123456789abcdef".into(),
        target: ManagedWorktreeTarget::Local,
        base_commit: None,
    });
    let source = session.project_source(&config);
    assert_eq!(source.short, "source");
    assert_eq!(source.full, "/home/test/Projects/source");
    assert!(!source.full.contains(".mj/worktrees"));
}

#[test]
fn single_repository_bundle_uses_the_standalone_repository_identity() {
    let mut config = sample_config();
    let shared_bundle = config.bundles["hel"].clone();
    config.bundles.insert("other".into(), shared_bundle);

    let first = sample_session();
    let mut second = first.clone();
    second.bundle_id = "other".into();

    assert_eq!(
        config.bundles["hel"].primary_repo,
        config.bundles["other"].primary_repo
    );
    let first_source = first.project_source(&config);
    let second_source = second.project_source(&config);
    let standalone = ProjectSourceIdentity::git_remote("BrokkAi/hel").unwrap();
    assert_eq!(first_source, standalone);
    assert_eq!(second_source, standalone);
}

#[test]
fn multi_repository_bundles_include_all_repositories_in_sorted_identity_order() {
    let mut config = sample_config();
    let primary = config.bundles["hel"].repositories[0].clone();
    let secondary = ProjectRepository {
        id: "docs".into(),
        github: Some("BrokkAi/docs".into()),
        local: None,
        destination: PathBuf::from("docs"),
        git_ref: None,
    };
    config.bundles.insert(
        "with-docs".into(),
        ProjectBundle {
            primary_repo: primary.id.clone(),
            repositories: vec![primary.clone(), secondary.clone()],
        },
    );
    let mut session = sample_session();
    session.bundle_id = "with-docs".into();

    assert_eq!(session.project_name(&config), "docs + hel");
    assert_eq!(
        session.project_source(&config),
        ProjectSourceIdentity {
            key: "bundle:[\"github:brokkai/docs\",\"github:brokkai/hel\"]".into(),
            short: "docs + hel".into(),
            full: "BrokkAi/docs + BrokkAi/hel".into(),
        }
    );

    let mut other_secondary = secondary;
    other_secondary.github = Some("OtherOrg/docs".into());
    config.bundles.insert(
        "with-other-docs".into(),
        ProjectBundle {
            primary_repo: primary.id.clone(),
            repositories: vec![primary, other_secondary],
        },
    );
    let mut other_session = session.clone();
    other_session.bundle_id = "with-other-docs".into();
    assert_ne!(
        session.project_source(&config).key,
        other_session.project_source(&config).key
    );
}

#[test]
fn multi_repository_bundle_identity_ignores_repository_order_and_primary_selection() {
    let mut config = sample_config();
    let primary = config.bundles["hel"].repositories[0].clone();
    let secondary = ProjectRepository {
        id: "docs".into(),
        github: Some("BrokkAi/docs".into()),
        local: None,
        destination: PathBuf::from("docs"),
        git_ref: None,
    };
    config.bundles.insert(
        "first-order".into(),
        ProjectBundle {
            primary_repo: primary.id.clone(),
            repositories: vec![primary.clone(), secondary.clone()],
        },
    );
    config.bundles.insert(
        "second-order".into(),
        ProjectBundle {
            primary_repo: secondary.id.clone(),
            repositories: vec![secondary, primary],
        },
    );

    let mut first = sample_session();
    first.bundle_id = "first-order".into();
    let mut second = first.clone();
    second.bundle_id = "second-order".into();
    assert_eq!(
        first.project_source(&config),
        second.project_source(&config)
    );
}

#[test]
fn duplicate_repository_sources_collapse_to_the_single_repository_identity() {
    let mut config = sample_config();
    let primary = config.bundles["hel"].repositories[0].clone();
    let duplicate = ProjectRepository {
        id: "hel-copy".into(),
        github: primary.github.clone(),
        local: None,
        destination: PathBuf::from("hel-copy"),
        git_ref: None,
    };
    config.bundles.insert(
        "duplicate".into(),
        ProjectBundle {
            primary_repo: primary.id.clone(),
            repositories: vec![primary, duplicate],
        },
    );
    let mut session = sample_session();
    session.bundle_id = "duplicate".into();

    let source = session.project_source(&config);
    assert_eq!(
        source,
        ProjectSourceIdentity::git_remote("BrokkAi/hel").unwrap()
    );
}

#[test]
fn unresolved_bundle_repository_uses_the_bundle_fallback() {
    let mut config = sample_config();
    config.bundles.insert(
        "incomplete".into(),
        ProjectBundle {
            primary_repo: "broken".into(),
            repositories: vec![ProjectRepository {
                id: "broken".into(),
                github: None,
                local: None,
                destination: PathBuf::from("broken"),
                git_ref: None,
            }],
        },
    );
    let mut session = sample_session();
    session.bundle_id = "incomplete".into();

    assert_eq!(session.project_name(&config), "incomplete");
    assert_eq!(
        session.project_source(&config),
        ProjectSourceIdentity {
            key: "bundle:incomplete".into(),
            short: "incomplete".into(),
            full: "incomplete".into(),
        }
    );
}

#[test]
fn sessions_order_by_creation_time_and_fall_back_to_the_id() {
    let older = sample_session();
    let mut newer = sample_session();
    newer.id = "0000000000000001".into();
    newer.created_at = "2026-08-09T13:00:00Z".into();
    let mut unparsable = sample_session();
    unparsable.id = "0000000000000002".into();
    unparsable.created_at = "not a timestamp".into();
    let mut same_time = sample_session();
    same_time.id = "zzzzzzzzzzzzzzzz".into();

    let mut sessions = [&unparsable, &newer, &same_time, &older];
    sessions.sort_by(|left, right| left.compare_by_creation(right));

    assert_eq!(
        sessions
            .iter()
            .map(|session| &session.id)
            .collect::<Vec<_>>(),
        [&older.id, &same_time.id, &newer.id, &unparsable.id]
    );
}

#[test]
fn retired_checkpoint_and_detach_cursor_names_are_rejected() {
    let session_id = "0123456789abcdef";

    let mut old_checkpoint = serde_json::to_value(sample_state()).unwrap();
    let checkpoint = old_checkpoint["sessions"][session_id]["checkpoint"]
        .as_object_mut()
        .unwrap();
    let frontier = checkpoint.remove("event_frontier").unwrap();
    checkpoint.insert("event_sequence".into(), frontier);
    assert!(serde_json::from_value::<State>(old_checkpoint).is_err());

    let mut old_detach_cursor = serde_json::to_value(sample_state()).unwrap();
    let session = old_detach_cursor["sessions"][session_id]
        .as_object_mut()
        .unwrap();
    let ordinal = session.remove("viewed_through_event_ordinal").unwrap();
    session.insert("last_viewed_event_sequence".into(), ordinal);
    assert!(serde_json::from_value::<State>(old_detach_cursor).is_err());
}

#[test]
fn detached_cursor_field_loads_as_the_viewed_cursor() {
    let session_id = "0123456789abcdef";
    let mut legacy = serde_json::to_value(sample_state()).unwrap();
    let session = legacy["sessions"][session_id].as_object_mut().unwrap();
    let ordinal = session.remove("viewed_through_event_ordinal").unwrap();
    session.insert("detached_after_event_ordinal".into(), ordinal);

    let loaded: State = serde_json::from_value(legacy).unwrap();
    assert_eq!(
        loaded.sessions[session_id].viewed_through_event_ordinal,
        sample_state().sessions[session_id].viewed_through_event_ordinal
    );
}

#[test]
fn state_written_before_drafts_loads_with_an_empty_draft() {
    let session_id = "0123456789abcdef";
    let mut without_draft = serde_json::to_value(sample_state()).unwrap();
    let session = without_draft["sessions"][session_id]
        .as_object_mut()
        .unwrap();
    session.remove("draft_input");

    let state = serde_json::from_value::<State>(without_draft).unwrap();
    assert_eq!(state.sessions[session_id].draft_input, "");
}

#[test]
fn mount_history_keeps_unique_recent_sources_per_host() {
    let mut state = State::default();
    state.remember_mount_sources(
        "builder.example.test",
        &[
            AdditionalMount {
                source: "/srv/first".into(),
                destination: "/mnt/first".into(),
                access: MountAccess::Cow,
            },
            AdditionalMount {
                source: "/srv/second".into(),
                destination: "/mnt/second".into(),
                access: MountAccess::Cow,
            },
        ],
    );
    state.remember_mount_sources(
        "builder.example.test",
        &[AdditionalMount {
            source: "/srv/first".into(),
            destination: "/mnt/again".into(),
            access: MountAccess::Cow,
        }],
    );

    assert_eq!(
        state.mount_history["builder.example.test"],
        vec![PathBuf::from("/srv/first"), PathBuf::from("/srv/second")]
    );
}

#[test]
fn materialized_activity_watermark_does_not_regress_when_detail_is_removed() {
    let mut materialized = MaterializedSession::empty("session-1");
    assert_eq!(materialized.last_activity_at_ms(), None);

    materialized.execution = MaterializedExecutionState::Running { started_at_ms: 300 };
    materialized.transcript.push(Arc::new(TranscriptItem {
        stable_id: "system:1".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 350,
        last_changed_at_ms: 400,
        body: TranscriptBody::System {
            text: "working".into(),
        },
    }));
    materialized.queued_prompts.push(MaterializedQueuedPrompt {
        accepted_ordinal: None,
        command_id: "prompt-2".into(),
        kind: QueuedCommandKind::Prompt,
        content: Vec::new(),
        queued_at_ms: 500,
    });
    materialized.last_activity_at_ms = Some(500);
    assert_eq!(materialized.last_activity_at_ms(), Some(500));

    materialized.queued_prompts.clear();
    assert_eq!(materialized.last_activity_at_ms(), Some(500));
    materialized.transcript.clear();
    assert_eq!(materialized.last_activity_at_ms(), Some(500));
    materialized.execution = MaterializedExecutionState::Idle;
    assert_eq!(materialized.last_activity_at_ms(), Some(500));
}

/// Shared transcript items must stay plain JSON on the wire: sharing is a
/// controller memory concern, not part of the serialized shape.
#[test]
fn shared_transcript_items_serialize_as_plain_items() {
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.applied_event_ordinal = 1;
    materialized.applied_event_digest = "a".repeat(64);
    let item = Arc::new(TranscriptItem {
        stable_id: "system:1".into(),
        position: 1,
        latest_content_event_ordinal: None,
        created_at_ms: 10,
        last_changed_at_ms: 10,
        body: TranscriptBody::System {
            text: "started".into(),
        },
    });
    // The same item twice would be deduplicated by serde's pointer-aware
    // encodings; stable ids keep it a legal transcript.
    materialized.transcript.push(Arc::clone(&item));
    let mut second = TranscriptItem::clone(&item);
    second.stable_id = "system:2".into();
    materialized.transcript.push(Arc::new(second));
    materialized.validate().unwrap();

    let encoded = serde_json::to_value(&materialized).unwrap();
    assert_eq!(encoded["transcript"][0]["stable_id"], "system:1");
    assert_eq!(encoded["transcript"][0]["body"]["kind"], "system");
    assert_eq!(encoded["transcript"][0]["body"]["text"], "started");
    assert_eq!(encoded["transcript"][1]["stable_id"], "system:2");

    let restored: MaterializedSession = serde_json::from_value(encoded).unwrap();
    assert_eq!(restored, materialized);
}

#[test]
fn materialized_event_frontier_requires_the_matching_digest_kind() {
    let mut materialized = MaterializedSession::empty("session-1");
    materialized.validate().unwrap();

    materialized.applied_event_ordinal = 1;
    assert!(
        materialized
            .validate()
            .unwrap_err()
            .to_string()
            .contains("inconsistent ordinal")
    );

    materialized.applied_event_digest = "A".repeat(64);
    assert!(
        materialized
            .validate()
            .unwrap_err()
            .to_string()
            .contains("lowercase SHA-256")
    );
}

#[test]
fn project_directory_history_is_recent_and_isolated_per_remote_host() {
    let mut state = State::default();
    state.remember_project_directory("builder-a", Path::new("/srv/one"));
    state.remember_project_directory("builder-a", Path::new("/srv/two"));
    state.remember_project_directory("builder-a", Path::new("/srv/one"));
    state.remember_project_directory("builder-b", Path::new("/work/other"));

    assert_eq!(
        state.project_directories("builder-a"),
        [PathBuf::from("/srv/one"), PathBuf::from("/srv/two")]
    );
    assert_eq!(
        state.project_directories("builder-b"),
        [PathBuf::from("/work/other")]
    );
}

#[test]
fn setup_protects_active_dependencies_but_allows_additions_repairs_and_defaults() {
    let state = sample_state();
    let before = sample_config();
    let session = state.sessions.values().next().unwrap();
    for section in ["profile", "bundle", "target"] {
        let mut after = before.clone();
        match section {
            "profile" => {
                after.profiles.remove(&session.last_profile);
            }
            "bundle" => {
                after.bundles.remove(&session.bundle_id);
            }
            _ => {
                after.targets.remove(&session.target_template_id);
            }
        }
        assert!(
            state
                .validate_setup_update(&before, &after)
                .unwrap_err()
                .to_string()
                .contains("active session")
        );
        // Restoring a removed entry is always permitted.
        state.validate_setup_update(&after, &before).unwrap();
    }
    let mut after = before.clone();
    after.profiles.get_mut(&session.last_profile).unwrap().home = PathBuf::from("/new/home");
    assert!(state.validate_setup_update(&before, &after).is_err());
    let mut after = before.clone();
    after
        .profiles
        .get_mut(&session.last_profile)
        .unwrap()
        .enabled = false;
    after.advanced.show_stopped_sessions = !before.advanced.show_stopped_sessions;
    after.targets.insert(
        "alternative".into(),
        crate::config::TargetTemplate::LocalBare,
    );
    state.validate_setup_update(&before, &after).unwrap();
    let mut stopped = state.clone();
    stopped.sessions.values_mut().next().unwrap().state = SessionState::Stopped;
    stopped
        .validate_setup_update(&before, &Config::default())
        .unwrap();
}

#[test]
fn configuration_repair_reports_all_missing_entries_and_clears_after_restoration() {
    let state = sample_state();
    let session = state.sessions.values().next().unwrap();
    let mut config = sample_config();
    config.profiles.clear();
    config.bundles.clear();
    config.targets.clear();
    let issue = session.configuration_issue(&config).unwrap();
    assert!(issue.contains("missing profile"));
    assert!(issue.contains("missing bundle"));
    assert!(issue.contains("missing target template"));
    assert!(issue.contains("config.toml"));
    assert!(session.configuration_issue(&sample_config()).is_none());
    let mut raw = session.clone();
    raw.project_directory = Some(PathBuf::from("/project"));
    let mut config = sample_config();
    config.bundles.clear();
    assert!(raw.configuration_issue(&config).is_none());
    let mut stopped = session.clone();
    stopped.state = SessionState::Stopped;
    assert!(stopped.configuration_issue(&Config::default()).is_none());
}

#[test]
fn active_state_validates_references_and_harness_kind() {
    let state = sample_state();
    state.validate_against_config(&sample_config()).unwrap();

    let mut config = sample_config();
    config.profiles.get_mut("codex-1").unwrap().kind = HarnessKind::Claude;
    assert!(
        state
            .validate_against_config(&config)
            .unwrap_err()
            .to_string()
            .contains("expects Codex")
    );
}

/// A child's working directory is a launch choice, not a containment
/// boundary, so a stored record may name any path on the parent's target.
#[test]
fn a_stored_subagent_may_launch_outside_the_parent_workspace() {
    let mut state = sample_state();
    let parent_id = state.sessions.keys().next().unwrap().clone();
    let child_id = "fedcba9876543210".to_owned();
    let mut child = state.sessions[&parent_id].clone();
    child.id = child_id.clone();
    state.sessions.insert(child_id.clone(), child);
    state.subagents.insert(
        child_id.clone(),
        SubagentRecord {
            child_session_id: child_id.clone(),
            parent_session_id: parent_id,
            task_name: "lane".into(),
            profile_id: "codex-1".into(),
            model: None,
            effort: None,
            working_directory: PathBuf::new(),
            initial_prompt: "work in the lane".into(),
            request_key: "request-1".into(),
            created_at: "2026-09-16T00:00:00Z".into(),
            noticed_turn: None,
        },
    );
    for working_directory in [
        PathBuf::from("/mnt/optane/bifrost-sg-c2"),
        PathBuf::from("../shared-checkout"),
    ] {
        state
            .subagents
            .get_mut(&child_id)
            .unwrap()
            .working_directory = working_directory;
        state.validate().unwrap();
    }
}

/// Records written before the verb was renamed say "archived". They must
/// still load, and they must be written back with the new name.
#[test]
fn the_stopped_state_reads_the_retired_archived_name_and_writes_the_new_one() {
    assert_eq!(
        serde_json::from_str::<SessionState>("\"archived\"").unwrap(),
        SessionState::Stopped
    );
    assert_eq!(
        serde_json::from_str::<SessionState>("\"stopped\"").unwrap(),
        SessionState::Stopped
    );
    assert_eq!(
        serde_json::to_string(&SessionState::Stopped).unwrap(),
        "\"stopped\""
    );
    assert!(!SessionState::Stopped.is_active());
}

/// The archived flag is a later addition, so records written without it
/// load as visible and stay out of the serialized form until it is set.
#[test]
fn the_archived_flag_defaults_off_and_is_omitted_when_it_is_off() {
    let mut state = sample_state();
    let session = state.sessions.values_mut().next().unwrap();
    assert!(!session.archived);
    let json = serde_json::to_string(&*session).unwrap();
    assert!(!json.contains("archived"), "{json}");

    session.archived = true;
    let json = serde_json::to_string(&*session).unwrap();
    assert!(json.contains("\"archived\":true"), "{json}");
    assert!(
        serde_json::from_str::<SessionRecord>(&json)
            .unwrap()
            .archived
    );
}

#[test]
fn stopped_session_does_not_pin_renamed_config_entries() {
    let mut state = sample_state();
    state.sessions.values_mut().next().unwrap().state = SessionState::Stopped;
    state.validate_against_config(&Config::default()).unwrap();
}

#[test]
fn only_inactive_sessions_can_be_removed_from_the_archive() {
    let mut state = sample_state();
    assert!(
        state
            .destroy_stopped_session("0123456789abcdef")
            .unwrap_err()
            .to_string()
            .contains("active session")
    );
    assert!(state.sessions.contains_key("0123456789abcdef"));

    state.sessions.values_mut().next().unwrap().state = SessionState::Stopped;
    let removed = state.destroy_stopped_session("0123456789abcdef").unwrap();
    assert_eq!(removed.id, "0123456789abcdef");
    assert!(state.sessions.is_empty());
}

#[test]
fn force_removal_permits_an_active_session() {
    let mut state = sample_state();
    let removed = state.destroy_session_force("0123456789abcdef").unwrap();
    assert_eq!(removed.id, "0123456789abcdef");
    assert!(state.sessions.is_empty());
    assert!(
        state
            .destroy_session_force("0123456789abcdef")
            .unwrap_err()
            .to_string()
            .contains("unknown session")
    );
}

#[test]
fn harness_title_prefers_the_newest_session_info_update() {
    let events = vec![
        SequencedEvent {
            seq: 1,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::json!({
                    "type": "session_update",
                    "update": {
                        "sessionUpdate": "session_info_update",
                        "title": "First title"
                    }
                }),
            },
        },
        SequencedEvent {
            seq: 2,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::json!({
                    "type": "session_update",
                    "update": {
                        "sessionUpdate": "session_summary",
                        "summary": "  Build   the dashboard  "
                    }
                }),
            },
        },
    ];

    assert_eq!(
        harness_session_title(&events).as_deref(),
        Some("First title")
    );
}

#[test]
fn extension_session_title_is_cleaned_without_losing_available_text() {
    let first_prompt = format!("{}overflow", "word ".repeat(20));
    let expected = first_prompt.trim().to_string();
    let events = vec![
        SequencedEvent {
            seq: 1,
            recorded_at_ms: None,
            request_id: Some("prompt-1".into()),
            event: WorkerEvent::PromptAccepted {
                request_id: "prompt-1".into(),
                text: format!("  {first_prompt}\n"),
                attachments: vec![],
            },
        },
        SequencedEvent {
            seq: 2,
            recorded_at_ms: None,
            request_id: None,
            event: WorkerEvent::Adapter {
                kind: "session_update".into(),
                payload: serde_json::json!({
                    "type": "session_update",
                    "update": {
                        "sessionUpdate": "session_title",
                        "title": first_prompt
                    }
                }),
            },
        },
    ];

    assert_eq!(
        harness_session_title(&events).as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn first_prompt_is_not_used_as_an_acp_session_title() {
    let events = vec![SequencedEvent {
        seq: 1,
        recorded_at_ms: None,
        request_id: Some("prompt-1".into()),
        event: WorkerEvent::PromptAccepted {
            request_id: "prompt-1".into(),
            text: "Do not use me as a title".into(),
            attachments: vec![],
        },
    }];

    assert_eq!(harness_session_title(&events), None);
}

#[test]
fn provisional_title_is_cleaned_and_bounded() {
    assert_eq!(
        provisional_session_title(concat!(
            "<mj-project-memory>private</mj-project-memory> ",
            "  fix the flaky\nresume test  "
        ))
        .as_deref(),
        Some("fix the flaky resume test")
    );

    let prompt = format!("{}overflow", "word ".repeat(20));
    assert_eq!(
        provisional_session_title(&prompt).as_deref(),
        Some(format!("{}word…", "word ".repeat(11)).as_str())
    );
}

#[test]
fn harness_title_elides_hidden_context_instead_of_naming_the_session_from_it() {
    let titled = |title: &str| SequencedEvent {
        seq: 1,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::Adapter {
            kind: "session_update".into(),
            payload: serde_json::json!({
                "type": "session_update",
                "update": {
                    "sessionUpdate": "session_title",
                    "title": title
                }
            }),
        },
    };

    assert_eq!(
        harness_session_title(&[titled(concat!(
            "<mj-project-memory>private</mj-project-memory> ",
            "Visible session name"
        ))])
        .as_deref(),
        Some("Visible session name")
    );
    assert_eq!(
        harness_session_title(&[titled("<mj-project-memory>truncated")]),
        None
    );
}

#[test]
fn harness_titles_are_normalized_to_one_complete_line() {
    let events = vec![SequencedEvent {
        seq: 1,
        recorded_at_ms: None,
        request_id: None,
        event: WorkerEvent::Adapter {
            kind: "session_update".into(),
            payload: serde_json::json!({
                "type": "session_update",
                "update": {
                    "sessionUpdate": "session_title",
                    "title": "first\nsecond\tthird fourth fifth sixth seventh eighth ninth tenth eleventh twelfth thirteenth"
                }
            }),
        },
    }];

    assert_eq!(
        harness_session_title(&events).as_deref(),
        Some(
            "first second third fourth fifth sixth seventh eighth ninth tenth eleventh twelfth thirteenth"
        )
    );
}

#[test]
fn locator_rejects_parent_traversal() {
    let mut state = sample_state();
    state.sessions.values_mut().next().unwrap().target = Some(TargetLocator::SshBare {
        host: "builder".into(),
        workspace: PathBuf::from("~/hel/../other"),
        worker_id: None,
    });
    assert!(
        state
            .validate()
            .unwrap_err()
            .to_string()
            .contains("safe path ending")
    );
}

#[test]
fn generated_session_ids_are_valid_and_distinct() {
    let first = new_session_id().unwrap();
    let second = new_session_id().unwrap();
    validate_id("session", &first).unwrap();
    assert_eq!(first.len(), 32);
    assert_ne!(first, second);
}
