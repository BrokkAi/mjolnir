use super::*;

#[tokio::test(start_paused = true)]
async fn stopping_timeout_retains_the_runtime_and_admission_until_actual_completion() {
    let root = tempfile::tempdir().unwrap();
    let placement = ReviewerPlacement {
        target_environment: Default::default(),
        worker_root: root.path().join("worker"),
        session_id: "reviewer-lifecycle-test".into(),
        cwd: root.path().to_path_buf(),
        additional_directories: Vec::new(),
        worker_executable: PathBuf::from("/unused"),
        harness_runtime: HarnessRuntimePolicy::Ambient,
        review_capture: false,
        untracked_at_start: Default::default(),
    };
    let primary = Arc::new(Mutex::new(
        DurableRelay::open(
            root.path().join("primary-relay"),
            "reviewer-lifecycle-test",
            "test",
        )
        .unwrap(),
    ));
    {
        let mut primary = primary.lock().unwrap();
        primary.set_turn_verdict_harness(mj_core::config::HarnessKind::Claude);
        primary
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
    }
    let sidecar = Arc::new(ReviewerSidecar::new(placement, primary.clone()));
    let handle = sidecar.role(DEFAULT_ROLE);
    let mut role = handle.lock().await;
    let (finish, finished) = tokio::sync::oneshot::channel();
    let (commands, _commands_rx) = mpsc::channel(1);
    let (coordinator_wake, _wake_rx) = mpsc::channel(1);
    let shutdown = CancellationToken::new();
    role.lifecycle = ReviewerLifecycle::Stopping(RunningReviewer {
        config: ReviewerLaunchConfig {
            profile_id: "fixture".into(),
            harness: mj_core::config::HarnessKind::Kimi,
            bridge_command: PathBuf::from("/unused"),
            bridge_args: Vec::new(),
            environment: Default::default(),
            excluded_environment: Vec::new(),
            execution_policy: mj_core::config::ExecutionPolicy::ConfiguredApprovals,
            model: None,
            effort: None,
            fast_mode: None,
            generation: 7,
            mcp_servers: Vec::new(),
        },
        commands,
        coordinator_wake,
        runtime: tokio::spawn(async move {
            finished.await.context("finish test runtime")?;
            Ok(())
        }),
        shutdown,
        _admission: ReviewerAdmission::acquire(primary.clone(), "test runtime".into()).unwrap(),
    });
    assert!(
        role.pause()
            .await
            .unwrap_err()
            .to_string()
            .contains("still stopping")
    );
    assert_eq!(role.lifecycle.generation(), Some(7));
    let reserve = |command_id: &str| {
        let response = primary.lock().unwrap().handle(RelayRequestEnvelope {
            request_id: "stopping-reservation".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::ReserveIdle {
                command_id: command_id.into(),
            },
        });
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::IdleReservation { ordinal },
        } = response.body
        else {
            panic!("unexpected reservation response");
        };
        ordinal
    };
    assert!(reserve("upgrade-while-stopping").is_none());
    drop(role);
    let cleanup_sidecar = sidecar.clone();
    let cleanup = tokio::spawn(async move { cleanup_sidecar.pause_all().await });
    tokio::task::yield_now().await;
    tokio::time::advance(PAUSE_TIMEOUT + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(
        !cleanup.is_finished(),
        "worker exit retains the runtime beyond a pause response timeout"
    );
    assert!(reserve("upgrade-during-final-cleanup").is_none());
    finish.send(()).unwrap();
    cleanup.await.unwrap();
    assert!(matches!(
        handle.lock().await.lifecycle,
        ReviewerLifecycle::Stopped
    ));
    assert!(reserve("upgrade-after-stopping").is_some());
}

/// A reviewer role's home keeps the staged link to the profile's own login,
/// so the reviewer's harness refreshes the one login every local session
/// shares instead of a copy. Every other link is still copied as its target's
/// content, so a role's home never points into a directory another role owns.
#[test]
fn a_role_home_keeps_the_login_link_and_copies_every_other_link() {
    let root = tempfile::tempdir().unwrap();
    let profile_login = root.path().join("profile-auth.json");
    std::fs::write(&profile_login, "profile login").unwrap();
    let shared = root.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("rule"), "rule").unwrap();
    let stage = root.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    let login = harness_authentication_marker(HarnessKind::Codex, &stage);
    std::os::unix::fs::symlink(&profile_login, &login).unwrap();
    std::os::unix::fs::symlink(&shared, stage.join("rules")).unwrap();

    let role = root.path().join("role");
    copy_tree(&stage, &role, &login).unwrap();

    assert_eq!(
        std::fs::read_link(role.join("auth.json")).unwrap(),
        profile_login
    );
    let rules = std::fs::symlink_metadata(role.join("rules")).unwrap();
    assert!(rules.is_dir() && !rules.file_type().is_symlink());
    assert_eq!(
        std::fs::read_to_string(role.join("rules/rule")).unwrap(),
        "rule"
    );
}
