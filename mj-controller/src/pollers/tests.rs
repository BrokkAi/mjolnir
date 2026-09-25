/// The poller used to re-read and re-deserialise every live session's whole
/// transcript on every runtime snapshot, then compare ordinals to discover
/// that nothing had moved. On a real session that is 28,066 rows and
/// 635 MiB, per poll. The comparison has to happen before the read.
#[test]
fn an_unchanged_session_is_recognised_without_reading_its_transcript() {
    let runtime = runtime_view("session-1", 42, "digest-42");
    let published = PublishedView::of(&runtime);

    assert!(
        published.matches(&runtime),
        "an identical snapshot was treated as a change, so it would be re-read"
    );

    // Anything a viewer would notice has to defeat the skip.
    let advanced = runtime_view("session-1", 43, "digest-43");
    assert!(
        !published.matches(&advanced),
        "a moved projection was mistaken for an unchanged one"
    );

    // A digest change at the same ordinal is a rewritten projection, not a
    // quiet one: the convergence path exists precisely for this.
    let rewritten = runtime_view("session-1", 42, "digest-other");
    assert!(
        !published.matches(&rewritten),
        "a rewritten projection at the same ordinal was skipped"
    );

    // The transcript can stand still while the agent starts a turn, and a
    // viewer has to see that.
    let mut busy = runtime_view("session-1", 42, "digest-42");
    busy.connected = false;
    assert!(
        !published.matches(&busy),
        "a disconnect was skipped as unchanged"
    );
}

fn runtime_view(
    session_id: &str,
    projection_ordinal: u64,
    projection_digest: &str,
) -> crate::daemon::RuntimeSessionView {
    crate::daemon::RuntimeSessionView {
        session_id: session_id.to_owned(),
        projection_ordinal,
        projection_digest: projection_digest.to_owned(),
        operational: None,
        latest_credential_sync_signal: None,
        connected: true,
        error: None,
    }
}
use super::*;

fn podman_controller(state: SessionState) -> Controller {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        mj_core::config::HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: PathBuf::from("/home/dev/.codex"),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
        },
    );
    config.targets.insert(
        "podman".into(),
        mj_core::config::TargetTemplate::LocalPodman {
            container: mj_core::config::ContainerTemplate {
                build_cache: None,
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: std::collections::BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        },
    );
    config.bundles.insert(
        "project".into(),
        mj_core::config::ProjectBundle {
            primary_repo: "project".into(),
            repositories: vec![mj_core::config::ProjectRepository {
                id: "project".into(),
                github: Some("owner/project".into()),
                local: None,
                destination: "project".into(),
                git_ref: None,
            }],
        },
    );
    let mut app_state = State::default();
    app_state.sessions.insert(
        session_id.into(),
        mj_core::state::SessionRecord {
            target_runtime: None,
            launch_base: None,
            launch_branch: None,
            publication: None,
            build_cache: None,
            container_workspace: None,
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
            title: "poll target".into(),
            harness_kind: mj_core::config::HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state,
            target: Some(mj_core::state::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: "a".repeat(64),
                workspace_storage: Default::default(),
            }),
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-27T00:00:00Z".into(),
            updated_at: "2026-08-27T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        },
    );
    Controller {
        config,
        state: app_state,
    }
}

#[test]
fn recoverable_error_session_stays_out_of_live_target_pollers() {
    let running = podman_controller(SessionState::Running);
    assert_eq!(dashboard_worker_targets(&running).len(), 1);
    assert_eq!(dashboard_resource_targets(&running).len(), 1);
    assert_eq!(credential_sync_targets(&running).len(), 1);

    let recoverable_error = podman_controller(SessionState::Error);
    assert!(
        recoverable_error
            .state
            .sessions
            .values()
            .all(|session| session.target.is_some()),
        "the test session keeps its target so the exclusion is about its state"
    );
    assert!(
        !recoverable_error
            .state
            .sessions
            .values()
            .any(session_target_is_pollable),
        "an errored session is not dialed even while its target exists"
    );
    assert!(dashboard_worker_targets(&recoverable_error).is_empty());
    assert!(dashboard_resource_targets(&recoverable_error).is_empty());
    assert!(credential_sync_targets(&recoverable_error).is_empty());
}

/// R7-3: a bare target has no container to sample, so the resource poller
/// has nothing to ask it. It must skip the session without a warning: the
/// dashboard rebuilds these targets on every poll, and each bare session
/// used to log one warning each time (455 in 15 minutes with 12 sessions).
/// Its worker is still polled.
#[test]
fn a_bare_session_is_left_out_of_resource_sampling_without_a_warning() {
    let mut controller = podman_controller(SessionState::Running);
    controller.config.targets.insert(
        "local-bare".into(),
        mj_core::config::TargetTemplate::LocalBare,
    );
    for session in controller.state.sessions.values_mut() {
        session.target_template_id = "local-bare".into();
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: PathBuf::from("/tmp/mj-workers").join(&session.id),
        });
    }

    let log = crate::test_log::CapturedLog::default();
    let resource_targets =
        tracing::subscriber::with_default(log.clone(), || dashboard_resource_targets(&controller));
    assert!(resource_targets.is_empty());
    let warnings = log.at_or_above(tracing::Level::WARN);
    assert!(warnings.is_empty(), "{warnings:#?}");
    assert_eq!(
        dashboard_worker_targets(&controller).len(),
        1,
        "the worker of a bare session is still polled"
    );
}

/// A session gets its `target` as soon as the target exists, which is
/// before its worker binary has finished being copied into place. Polling
/// that window runs `execve` on a file `cp` still holds open for writing:
/// `ETXTBSY`, and a session recorded as unreachable while it was merely
/// still being built.
#[test]
fn a_provisioning_session_is_not_polled_before_its_worker_exists() {
    let provisioning = podman_controller(SessionState::Provisioning);
    assert!(
        provisioning
            .state
            .sessions
            .values()
            .all(|session| session.target.is_some())
    );

    assert!(dashboard_worker_targets(&provisioning).is_empty());
    assert!(dashboard_resource_targets(&provisioning).is_empty());

    // Provisioning connects to its own worker and then marks the session
    // running, which is when there is something to poll.
    let running = podman_controller(SessionState::Running);
    assert_eq!(dashboard_worker_targets(&running).len(), 1);
    assert_eq!(dashboard_resource_targets(&running).len(), 1);
}

#[test]
fn failed_destruction_stays_out_of_pollers_without_an_active_lifecycle() {
    let mut controller = podman_controller(SessionState::Destroying);
    for session in controller.state.sessions.values_mut() {
        session.last_error =
            Some("verified checkpoint retained; cleanup is safely retryable".into());
    }
    // No in-flight lifecycle exclusion survives a failed close or restart.
    let excluded = std::collections::BTreeSet::new();
    for _ in 0..3 {
        assert!(dashboard_worker_targets_excluding(&controller, &excluded).is_empty());
        assert!(dashboard_resource_targets(&controller).is_empty());
        assert!(credential_sync_targets(&controller).is_empty());
    }
    let closing = podman_controller(SessionState::Closing);
    assert_eq!(dashboard_worker_targets(&closing).len(), 1);
}

#[test]
fn lifecycle_owned_session_stays_out_of_worker_targets() {
    let controller = podman_controller(SessionState::Running);
    assert_eq!(dashboard_worker_targets(&controller).len(), 1);

    let excluded = controller
        .state
        .sessions
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    assert!(dashboard_worker_targets_excluding(&controller, &excluded).is_empty());
}

#[test]
fn projection_rollback_race_retries_before_reporting_integrity_failure() {
    let mismatch = ProjectionMismatch {
        published_ordinal: 39,
        published_digest: "published".into(),
        durable_ordinal: 36,
        durable_digest: "durable".into(),
    };
    let mut convergence = ProjectionConvergence::default();

    for _ in 0..PROJECTION_CONVERGENCE_RETRIES {
        assert!(convergence.should_retry("session-1", mismatch.clone()));
    }
    assert!(
        !convergence.should_retry("session-1", mismatch),
        "a persistent mismatch must still become an integrity error"
    );

    convergence.converged("session-1");
    assert!(convergence.attempts.is_empty());
}

#[test]
fn a_changed_projection_mismatch_gets_its_own_convergence_window() {
    let mut convergence = ProjectionConvergence::default();
    let stale_lineage = ProjectionMismatch {
        published_ordinal: 39,
        published_digest: "old-lineage".into(),
        durable_ordinal: 36,
        durable_digest: "checkpoint".into(),
    };
    for _ in 0..=PROJECTION_CONVERGENCE_RETRIES {
        convergence.should_retry("session-1", stale_lineage.clone());
    }
    let equal_frontier_different_lineage = ProjectionMismatch {
        published_ordinal: 39,
        published_digest: "old-lineage".into(),
        durable_ordinal: 39,
        durable_digest: "new-lineage".into(),
    };

    assert!(convergence.should_retry("session-1", equal_frontier_different_lineage));
}

#[test]
fn worker_diagnosis_is_coalesced_for_one_unreachable_episode() {
    let mut tracker = WorkerDiagnosisTracker::default();
    let episode = tracker
        .observe("session-1", false, Some("connection refused".into()))
        .unwrap();

    assert_eq!(
        tracker.observe("session-1", false, Some("still unreachable".into())),
        None
    );
    assert_eq!(
        tracker.finish("session-1", episode),
        WorkerDiagnosisCompletion {
            display_error: Some("still unreachable".into()),
            restart_episode: None,
        }
    );
    assert_eq!(
        tracker.observe("session-1", false, Some("third poll".into())),
        None
    );
}

#[test]
fn stale_worker_diagnosis_is_not_published_after_reconnect() {
    let mut tracker = WorkerDiagnosisTracker::default();
    let first = tracker
        .observe("session-1", false, Some("first outage".into()))
        .unwrap();
    assert_eq!(tracker.observe("session-1", true, None), None);
    assert_eq!(
        tracker.observe("session-1", false, Some("new outage".into())),
        None
    );

    let completion = tracker.finish("session-1", first);
    assert_eq!(completion.display_error, None);
    let second = completion.restart_episode.unwrap();
    assert_eq!(
        tracker.finish("session-1", second).display_error.as_deref(),
        Some("new outage")
    );
}

#[test]
fn stale_worker_diagnosis_is_not_published_after_a_terminal_poll_error() {
    let mut tracker = WorkerDiagnosisTracker::default();
    let episode = tracker
        .observe("session-1", false, Some("relay failed".into()))
        .unwrap();

    assert_eq!(tracker.observe("session-1", false, None), None);
    assert_eq!(
        tracker.finish("session-1", episode),
        WorkerDiagnosisCompletion::default()
    );
}

#[tokio::test]
async fn quota_refresh_completion_keeps_its_generation() {
    let mut quotas = QuotaManager::default();
    let (updates, mut received) = tokio::sync::mpsc::channel(4);
    assert!(refresh_profile_quotas(&mut quotas, 42, &[], &updates).await);
    assert!(matches!(
        received.recv().await,
        Some(QuotaUpdate::Refreshing {
            profile_ids,
        }) if profile_ids.is_empty()
    ));
    assert!(matches!(
        received.recv().await,
        Some(QuotaUpdate::Finished { generation: 42 })
    ));

    let mut pending = Some(43);
    assert!(!complete_manual_quota_refresh(&mut pending, 42));
    assert_eq!(pending, Some(43));
    assert!(complete_manual_quota_refresh(&mut pending, 43));
    assert_eq!(pending, None);
    quotas.shutdown().await;
}

#[test]
fn quota_refresh_requests_exclude_disabled_profiles() {
    let mut controller = podman_controller(SessionState::Stopped);
    let mut disabled = controller.config.profiles["codex"].clone();
    disabled.enabled = false;
    controller
        .config
        .profiles
        .insert("reserve".into(), disabled);

    let requests = quota_refresh_profiles(&controller);

    assert_eq!(
        requests
            .iter()
            .map(|request| request.profile_id.as_str())
            .collect::<Vec<_>>(),
        ["codex"]
    );
}

#[test]
fn resource_samples_are_throttled_to_one_per_minute() {
    let started = tokio::time::Instant::now();
    assert!(!resource_sample_is_due(
        Some(&started),
        started + Duration::from_secs(59),
    ));
    assert!(resource_sample_is_due(
        Some(&started),
        started + RESOURCE_POLL_INTERVAL,
    ));
}

struct PendingCapacityProbe {
    target: DeploymentCapacityTarget,
    finish: tokio::sync::oneshot::Sender<Result<Option<DeploymentCapacityUsage>>>,
}

struct CapacityPollerFixture {
    targets: tokio::sync::watch::Sender<Vec<DeploymentCapacityTarget>>,
    triggers: tokio::sync::mpsc::Sender<()>,
    updates: tokio::sync::mpsc::Receiver<CapacityPollUpdate>,
    started: tokio::sync::mpsc::UnboundedReceiver<PendingCapacityProbe>,
}

impl CapacityPollerFixture {
    fn new() -> Self {
        let (started_tx, started) = tokio::sync::mpsc::unbounded_channel();
        let (targets, triggers, updates) = spawn_capacity_poller_with(move |target| {
            let started_tx = started_tx.clone();
            async move {
                let (finish, result) = tokio::sync::oneshot::channel();
                started_tx
                    .send(PendingCapacityProbe { target, finish })
                    .unwrap();
                result.await.context("test probe completion dropped")?
            }
        });
        Self {
            targets,
            triggers,
            updates,
            started,
        }
    }

    async fn assert_no_start(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(1), self.started.recv())
                .await
                .is_err()
        );
    }
}

fn capacity_target(id: &str) -> DeploymentCapacityTarget {
    DeploymentCapacityTarget {
        id: id.into(),
        host: id.into(),
        target_ids: vec![id.into()],
        kind: DeploymentCapacityKind::Host,
        local: true,
        probes: Vec::new(),
        probe_error: None,
    }
}

#[tokio::test(start_paused = true)]
async fn capacity_samples_follow_timer_and_manual_refresh_not_unchanged_publications() {
    let mut fixture = CapacityPollerFixture::new();
    let targets = vec![capacity_target("local")];
    fixture.targets.send_replace(targets.clone());
    fixture
        .started
        .recv()
        .await
        .unwrap()
        .finish
        .send(Ok(None))
        .unwrap();
    assert!(fixture.updates.recv().await.unwrap().result.is_ok());

    fixture.targets.send_replace(targets);
    fixture.assert_no_start().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    fixture.assert_no_start().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    fixture
        .started
        .recv()
        .await
        .unwrap()
        .finish
        .send(Ok(None))
        .unwrap();
    assert!(fixture.updates.recv().await.unwrap().result.is_ok());

    fixture.triggers.send(()).await.unwrap();
    fixture
        .started
        .recv()
        .await
        .unwrap()
        .finish
        .send(Ok(None))
        .unwrap();
    assert!(fixture.updates.recv().await.unwrap().result.is_ok());
    fixture.assert_no_start().await;
}

#[tokio::test(start_paused = true)]
async fn capacity_busy_targets_coalesce_requests_without_blocking_other_targets() {
    let mut fixture = CapacityPollerFixture::new();
    let first_target = capacity_target("first");
    fixture.targets.send_replace(vec![first_target.clone()]);
    let first = fixture.started.recv().await.unwrap();
    fixture
        .targets
        .send_replace(vec![first_target, capacity_target("second")]);
    let second = fixture.started.recv().await.unwrap();
    assert_eq!(second.target.id, "second");

    fixture.triggers.send(()).await.unwrap();
    fixture.assert_no_start().await;
    tokio::time::advance(CAPACITY_POLL_INTERVAL).await;
    fixture.assert_no_start().await;
    first.finish.send(Ok(None)).unwrap();
    second.finish.send(Ok(None)).unwrap();
    assert!(fixture.updates.recv().await.unwrap().result.is_ok());
    assert!(fixture.updates.recv().await.unwrap().result.is_ok());
    fixture.assert_no_start().await;
}

#[tokio::test(start_paused = true)]
async fn capacity_changed_targets_get_one_follow_up_and_removed_results_are_discarded() {
    let mut fixture = CapacityPollerFixture::new();
    let mut target = capacity_target("local");
    fixture.targets.send_replace(vec![target.clone()]);
    let first = fixture.started.recv().await.unwrap();
    target.host = "new-host".into();
    fixture.targets.send_replace(vec![target.clone()]);
    fixture.assert_no_start().await;
    first.finish.send(Ok(None)).unwrap();
    let changed = fixture.started.recv().await.unwrap();
    assert_eq!(changed.target, target);
    assert!(
        fixture.updates.try_recv().is_err(),
        "old configuration result escaped"
    );
    changed
        .finish
        .send(Err(anyhow::anyhow!("new host unavailable")))
        .unwrap();
    assert!(
        fixture
            .updates
            .recv()
            .await
            .unwrap()
            .result
            .unwrap_err()
            .contains("new host unavailable")
    );

    fixture.triggers.send(()).await.unwrap();
    let removed = fixture.started.recv().await.unwrap();
    fixture.targets.send_replace(Vec::new());
    fixture.assert_no_start().await;
    removed.finish.send(Ok(None)).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), fixture.updates.recv())
            .await
            .is_err()
    );
    fixture.targets.send_replace(vec![target]);
    let mut last = fixture.started.recv().await.unwrap();
    drop(fixture.updates);
    last.finish.closed().await;
}

#[tokio::test]
async fn capacity_probe_panics_are_reported_and_do_not_prevent_retry() {
    let first = AtomicBool::new(true);
    let (targets, triggers, mut updates) = spawn_capacity_poller_with(move |_| {
        if first.swap(false, Ordering::SeqCst) {
            panic!("test capacity probe panic");
        }
        async { Ok(None) }
    });
    targets.send_replace(vec![capacity_target("local")]);
    let failure = updates.recv().await.unwrap();
    assert_eq!(failure.target_id, "local");
    assert!(
        failure
            .result
            .unwrap_err()
            .contains("test capacity probe panic")
    );
    triggers.send(()).await.unwrap();
    assert!(updates.recv().await.unwrap().result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn capacity_results_are_revalidated_after_output_backpressure() {
    let mut fixture = CapacityPollerFixture::new();
    let mut targets: Vec<_> = (0..65).map(|id| capacity_target(&id.to_string())).collect();
    fixture.targets.send_replace(targets.clone());
    let mut pending = Vec::new();
    for _ in 0..65 {
        pending.push(fixture.started.recv().await.unwrap());
    }
    let last = pending.pop().unwrap();
    let last_id = last.target.id;
    for probe in pending {
        probe.finish.send(Ok(None)).unwrap();
    }
    fixture.assert_no_start().await;
    assert_eq!(fixture.updates.len(), 64);
    last.finish.send(Ok(None)).unwrap();
    fixture.assert_no_start().await;

    targets
        .iter_mut()
        .find(|target| target.id == last_id)
        .unwrap()
        .host = "changed".into();
    fixture.targets.send_replace(targets);
    for _ in 0..64 {
        let update = fixture.updates.recv().await.unwrap();
        assert_ne!(update.target_id, last_id);
    }
    let changed = fixture.started.recv().await.unwrap();
    assert_eq!(changed.target.id, last_id);
    assert_eq!(changed.target.host, "changed");
    assert!(
        fixture.updates.try_recv().is_err(),
        "stale blocked result escaped"
    );
    changed.finish.send(Ok(None)).unwrap();
    assert_eq!(fixture.updates.recv().await.unwrap().target_id, last_id);
    fixture.assert_no_start().await;
}

/// Launch finding R3-5: while SSH-bare sessions were starting, the capacity
/// probe was refused a session on a shared connection ("Session open refused
/// by peer", MaxSessions) and failed without a retry, where the relay and
/// every executor-run ssh command retry. The command never ran, so it is
/// retried; a probe that fails for its own reasons is not.
#[cfg(unix)]
#[tokio::test]
async fn a_capacity_probe_refused_a_shared_session_is_retried() {
    mj_core::targets::set_ssh_retry_backoff_for_test(Some(Duration::from_millis(5)));
    let directory = tempfile::tempdir().unwrap();
    let probe = |name: &str, refusal: &str, exit: u8| {
        let counter = directory.path().join(name);
        let script = format!(
            r#"
count=$(cat {counter} 2>/dev/null || echo 0)
echo $((count + 1)) > {counter}
if [ "$count" -eq 0 ]; then
  echo '{refusal}' >&2
  exit {exit}
fi
echo sampled
"#,
            counter = counter.display()
        );
        let command = CommandSpec::new("sh", ["-c".to_owned(), script])
            .ssh_destination("ubuntu@203.0.113.9")
            .purpose("sample deployment host capacity");
        (command, counter)
    };
    let attempts = |counter: &PathBuf| std::fs::read_to_string(counter).unwrap().trim().to_owned();

    let (command, counter) = probe(
        "refused",
        "mux_client_request_session: session request failed: Session open refused by peer",
        255,
    );
    let output = execute_resource_command(&command)
        .await
        .expect("a refused session is retried, not reported as a failed probe");
    assert_eq!(output.stdout, b"sampled\n");
    assert_eq!(attempts(&counter), "2");

    let (command, counter) = probe("broken", "sh: free: not found", 1);
    execute_resource_command(&command)
        .await
        .expect_err("a probe that fails by itself is reported");
    assert_eq!(attempts(&counter), "1");
    mj_core::targets::set_ssh_retry_backoff_for_test(None);
}

#[tokio::test(start_paused = true)]
async fn capacity_timeout_retains_blocking_sample_until_it_exits() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let sample = tokio::spawn(collect_local_capacity_with(move || {
        started_tx.send(()).unwrap();
        // Dropping finish_tx on a test failure also releases this thread.
        finish_rx.recv().context("test sample was cancelled")?;
        Ok(DeploymentCapacityUsage {
            cpu_percent: Some(10),
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            logical_cores: 4,
            disk_total_bytes: None,
        })
    }));
    started_rx.await.unwrap();
    tokio::time::advance(RESOURCE_POLL_TIMEOUT + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(
        !sample.is_finished(),
        "timeout released a still-running blocking sample"
    );
    finish_tx.send(()).unwrap();
    assert!(
        sample
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
}

#[test]
fn a_new_credential_signal_waits_out_the_cooldown_without_being_lost() {
    let signal = |ordinal, reason| CredentialSyncSignal { ordinal, reason };
    let mut tracker = CredentialSyncSignalTracker::default();
    let started = Instant::now();
    tracker.observe(
        "session",
        "work",
        signal(41, CredentialSyncReason::AuthenticationFailure),
    );
    assert_eq!(
        tracker.drain_due(started),
        vec![(
            "session".into(),
            "work".into(),
            CredentialSyncReason::AuthenticationFailure
        )]
    );

    tracker.observe(
        "session",
        "work",
        signal(42, CredentialSyncReason::AuthenticationFailure),
    );
    assert!(
        tracker
            .drain_due(started + Duration::from_secs(60))
            .is_empty()
    );
    tracker.observe(
        "session",
        "new-profile",
        signal(43, CredentialSyncReason::EmptyPromptResponse),
    );
    assert_eq!(tracker.pending["session"].signal.ordinal, 43);

    // No repeated observation is needed: the loop timer drains the sticky
    // failure once its cooldown expires.
    assert_eq!(
        tracker.drain_due(started + IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN),
        vec![(
            "session".into(),
            "new-profile".into(),
            CredentialSyncReason::EmptyPromptResponse
        )]
    );
    tracker.observe(
        "session",
        "new-profile",
        signal(43, CredentialSyncReason::EmptyPromptResponse),
    );
    assert!(
        tracker
            .drain_due(started + (IMMEDIATE_CREDENTIAL_SYNC_COOLDOWN * 2))
            .is_empty()
    );

    tracker.observe(
        "other",
        "personal",
        signal(1, CredentialSyncReason::AuthenticationFailure),
    );
    assert_eq!(
        tracker.drain_due(started + Duration::from_secs(60)),
        vec![(
            "other".into(),
            "personal".into(),
            CredentialSyncReason::AuthenticationFailure
        )]
    );
}

#[test]
fn a_healthy_credential_cycle_stays_out_of_the_ui() {
    let result = mj_core::credentials::CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: Vec::new(),
    };
    assert_eq!(CredentialSyncNotices::default().notice(&result, None), None);
}

#[test]
fn github_tokens_sync_to_every_remote_target_but_raw_localhost() {
    use mj_core::state::TargetLocator;

    let remotes = [
        TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: "podman".into(),
            workspace_storage: Default::default(),
        },
        TargetLocator::AppleContainer {
            borrowed_from: None,
            container_id: "apple".into(),
        },
        TargetLocator::AwsEc2 {
            instance_id: "i-123".into(),
            address: Some("example.invalid".into()),
        },
        TargetLocator::SshBare {
            host: "ssh.example".into(),
            workspace: "/workspace".into(),
            worker_id: None,
        },
        TargetLocator::SshPodman {
            borrowed_from: None,
            host: "ssh.example".into(),
            container_id: "remote-podman".into(),
            workspace_storage: Default::default(),
        },
        TargetLocator::SshDocker {
            borrowed_from: None,
            host: "ssh.example".into(),
            container_id: "remote-docker".into(),
        },
    ];
    for target in &remotes {
        assert!(target_syncs_github_token(Some(target)), "{target:?}");
    }
    assert!(!target_syncs_github_token(Some(
        &TargetLocator::LocalBare {
            worker_root: "/tmp/worker".into(),
        }
    )));
    assert!(!target_syncs_github_token(None));
}

#[test]
fn an_authentication_failure_notice_says_whether_anything_was_pushed() {
    use mj_core::credentials::{CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult};

    let mut notices = CredentialSyncNotices::default();
    let pushed = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: Some(CredentialSyncCause {
            session_id: "018f9dd2-a3b4".into(),
            reason: CredentialSyncReason::AuthenticationFailure,
        }),
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(vec![CredentialSyncAction::Pushed]),
        }],
    };
    let notice = notices.notice(&pushed, None).unwrap();
    assert!(notice.contains("were pushed"), "{notice}");
    assert!(notice.contains("mj login --profile work"), "{notice}");

    let nothing_to_push = CredentialSyncResult {
        trigger: Some(CredentialSyncCause {
            session_id: "018f9dd2-a3b4".into(),
            reason: CredentialSyncReason::AuthenticationFailure,
        }),
        outcomes: Vec::new(),
        ..pushed
    };
    let notice = notices.notice(&nothing_to_push, None).unwrap();
    assert!(notice.contains("nothing fresher"), "{notice}");
    assert!(notice.contains("mj login --profile work"), "{notice}");
    // The per-session cooldown upstream limits these; the dedup must not.
    assert_eq!(notices.notice(&nothing_to_push, None), Some(notice));
}

#[test]
fn a_claude_authentication_failure_offers_the_long_lived_token() {
    use mj_core::config::HarnessKind;
    use mj_core::credentials::{CredentialSyncOutcome, CredentialSyncResult};

    let result = CredentialSyncResult {
        profile_id: "claude-max".into(),
        trigger: Some(CredentialSyncCause {
            session_id: "018f9dd2-a3b4".into(),
            reason: CredentialSyncReason::AuthenticationFailure,
        }),
        failure: None,
        outcomes: Vec::new(),
    };

    let claude = CredentialSyncNotices::default()
        .notice(&result, Some(HarnessKind::Claude))
        .unwrap();
    assert!(
        claude.ends_with(
            "Run `mj login --profile claude-max`, or store a long-lived token with `mj login --profile claude-max --setup-token`."
        ),
        "{claude}"
    );

    // Only Claude can rotate ahead of expiry this way.
    let codex = CredentialSyncNotices::default()
        .notice(&result, Some(HarnessKind::Codex))
        .unwrap();
    assert!(
        codex.ends_with("Run `mj login --profile claude-max`."),
        "{codex}"
    );

    // The advice also reaches a failed reconciliation, not only a clean one.
    let failed = CredentialSyncResult {
        outcomes: vec![CredentialSyncOutcome {
            session_id: "018f9dd2-a3b4".into(),
            outcome: Err("worker proxy disconnected".into()),
        }],
        ..result
    };
    let claude_failure = CredentialSyncNotices::default()
        .notice(&failed, Some(HarnessKind::Claude))
        .unwrap();
    assert!(
        claude_failure.contains("--setup-token`."),
        "{claude_failure}"
    );
}

#[test]
fn an_empty_prompt_notice_does_not_claim_authentication_failed() {
    use mj_core::credentials::{CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult};

    let result = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: Some(CredentialSyncCause {
            session_id: "018f9dd2-a3b4".into(),
            reason: CredentialSyncReason::EmptyPromptResponse,
        }),
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(vec![CredentialSyncAction::Pushed]),
        }],
    };
    let notice = CredentialSyncNotices::default()
        .notice(&result, None)
        .unwrap();
    assert!(notice.contains("returned no response"), "{notice}");
    assert!(notice.contains("were pushed"), "{notice}");
    assert!(!notice.contains("Auth failure"), "{notice}");
}

#[test]
fn an_immediate_sync_failure_is_not_reported_as_no_new_credentials() {
    use mj_core::credentials::CredentialSyncResult;

    let result = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: Some(CredentialSyncCause {
            session_id: "018f9dd2-a3b4".into(),
            reason: CredentialSyncReason::AuthenticationFailure,
        }),
        failure: Some("controller credential file is unreadable".into()),
        outcomes: Vec::new(),
    };
    let notice = CredentialSyncNotices::default()
        .notice(&result, None)
        .unwrap();
    assert!(notice.contains("reconciliation failed"), "{notice}");
    assert!(notice.contains("credential file is unreadable"), "{notice}");
    assert!(!notice.contains("nothing fresher"), "{notice}");
}

#[test]
fn a_failed_credential_sync_is_reported() {
    use mj_core::credentials::{CredentialSyncOutcome, CredentialSyncResult};

    let result = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            session_id: "018f9dd2-a3b4".into(),
            outcome: Err("worker proxy disconnected".into()),
        }],
    };
    let notice = CredentialSyncNotices::default()
        .notice(&result, None)
        .unwrap();
    assert!(notice.contains("worker proxy disconnected"), "{notice}");
}

#[test]
fn a_repeated_credential_failure_is_reported_once_until_it_changes() {
    use mj_core::credentials::{CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult};

    let failed = |detail: &str| CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            session_id: "018f9dd2-a3b4".into(),
            outcome: Err(detail.to_owned()),
        }],
    };
    let mut notices = CredentialSyncNotices::default();

    assert!(
        notices
            .notice(&failed("worker proxy disconnected"), None)
            .is_some()
    );
    assert_eq!(
        notices.notice(&failed("worker proxy disconnected"), None),
        None
    );

    let changed = notices.notice(&failed("container is gone"), None).unwrap();
    assert!(changed.contains("container is gone"), "{changed}");
    assert_eq!(notices.notice(&failed("container is gone"), None), None);

    // A clean cycle forgets the failure, so a recurrence is reported again.
    let healthy = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(vec![CredentialSyncAction::Pushed]),
        }],
    };
    assert_eq!(notices.notice(&healthy, None), None);
    assert!(notices.notice(&failed("container is gone"), None).is_some());
}

#[test]
fn a_repeated_whole_sync_failure_is_reported_once_per_profile() {
    use mj_core::credentials::CredentialSyncResult;

    let failed = |profile_id: &str| CredentialSyncResult {
        profile_id: profile_id.to_owned(),
        trigger: None,
        failure: Some("controller home is unreadable".into()),
        outcomes: Vec::new(),
    };
    let mut notices = CredentialSyncNotices::default();

    let notice = notices.notice(&failed("work"), None).unwrap();
    assert!(notice.contains("profile work"), "{notice}");
    assert_eq!(notices.notice(&failed("work"), None), None);
    // Another profile failing the same way is its own key.
    assert!(notices.notice(&failed("personal"), None).is_some());
    assert_eq!(notices.notice(&failed("work"), None), None);
}

#[test]
fn skills_and_github_syncs_speak_while_harness_credentials_stay_out_of_the_notice() {
    use mj_core::credentials::{CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult};

    let result = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![
            CredentialSyncOutcome {
                session_id: "018f9dd2-a3b4".into(),
                outcome: Ok(vec![
                    CredentialSyncAction::Pushed,
                    CredentialSyncAction::SkillsPushed,
                    CredentialSyncAction::GithubTokenPushed,
                ]),
            },
            CredentialSyncOutcome {
                session_id: "018f9dd2-bbbb".into(),
                outcome: Ok(vec![
                    CredentialSyncAction::SkillsPushed,
                    CredentialSyncAction::GithubTokenRemoved,
                ]),
            },
        ],
    };
    let notice = CredentialSyncNotices::default()
        .notice(&result, None)
        .unwrap();
    assert!(!notice.contains("harness credentials"), "{notice}");
    assert!(
        notice.contains("Synced skills for profile work to 2 session(s)."),
        "{notice}"
    );
    assert!(
        notice.contains("Synced the GitHub CLI token to 1 session(s)."),
        "{notice}"
    );
    assert!(
        notice.contains("Removed the GitHub CLI token from 1 session(s)."),
        "{notice}"
    );
}

#[test]
fn aws_capacity_sums_live_instance_allocations() {
    let total = aggregate_aws_capacity(&[
        DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 8,
            logical_cores: 2,
            disk_total_bytes: Some(100),
        },
        DeploymentCapacityUsage {
            cpu_percent: None,
            memory_used_bytes: 0,
            memory_total_bytes: 16,
            logical_cores: 4,
            disk_total_bytes: Some(200),
        },
    ])
    .unwrap();

    assert_eq!(total.memory_total_bytes, 24);
    assert_eq!(total.logical_cores, 6);
    assert_eq!(total.disk_total_bytes, Some(300));
}

/// Collects what the daemon would have told the user about a download.
#[derive(Default)]
struct RefreshReports(std::sync::Mutex<Vec<ImageRefreshReport>>);

impl RefreshReports {
    fn record(&self) -> impl Fn(ImageRefreshReport) + '_ {
        |report| self.0.lock().unwrap().push(report)
    }

    fn taken(&self) -> Vec<ImageRefreshReport> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// The pre-pull only helps if it starts before the person opens the New
/// Session wizard, so the first refresh is a startup refresh and the hourly
/// interval follows it.
#[tokio::test(start_paused = true)]
async fn the_first_refresh_runs_at_startup() {
    assert!(
        IMAGE_REFRESH_DELAY <= Duration::from_secs(5),
        "the first refresh is the pre-pull for the first session: {IMAGE_REFRESH_DELAY:?}"
    );

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancellation = tokio_util::sync::CancellationToken::new();
    let refresher = spawn_image_refresher(
        {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::Release);
                Vec::new()
            }
        },
        |_report| {},
        cancellation.clone(),
    );

    // Let the task reach its first await so the interval's deadline is set
    // from the same instant the test then advances past.
    tokio::task::yield_now().await;
    tokio::time::advance(IMAGE_REFRESH_DELAY + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::Acquire),
        1,
        "the refresher should have planned a refresh at startup"
    );

    tokio::time::advance(IMAGE_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::Acquire),
        2,
        "the hourly interval should follow the startup refresh"
    );

    cancellation.cancel();
    refresher.await.expect("the refresher stops when cancelled");
}

/// The pre-pull's whole point: an image the host does not have is downloaded
/// once, and the hourly refresh after that only checks that it is still there.
#[test]
fn a_missing_image_is_pulled_once_and_not_again_when_present() {
    /// Reports the image as absent until a pull has run, the way a host
    /// behaves the first time it sees an image.
    struct FirstPullExecutor {
        commands: std::sync::Mutex<Vec<Vec<String>>>,
        pulled: std::sync::atomic::AtomicBool,
    }

    impl CommandExecutor for FirstPullExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.args.clone());
            if command.args.first().map(String::as_str) == Some("pull") {
                self.pulled.store(true, Ordering::Release);
                return Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if command.args.contains(&"inspect".to_owned()) && !self.pulled.load(Ordering::Acquire)
            {
                return Ok(CommandOutput {
                    status: 125,
                    stdout: Vec::new(),
                    stderr: b"no such image".to_vec(),
                });
            }
            Ok(CommandOutput {
                status: 0,
                stdout: b"sha256:1111\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let refresh = crate::targets::image_refresh(
        crate::targets::ImageHost::LocalPodman,
        "ghcr.io/example/dev:1.2.3",
        None,
        mj_core::config::ImagePullPolicy::Auto,
    )
    .expect("a versioned tag is downloaded when the host lacks it");
    assert_eq!(refresh.when, crate::targets::RefreshWhen::WhenAbsent);

    let executor = FirstPullExecutor {
        commands: std::sync::Mutex::new(Vec::new()),
        pulled: std::sync::atomic::AtomicBool::new(false),
    };
    let reports = RefreshReports::default();
    assert_eq!(
        refresh_host_image(&refresh, &executor, &reports.record())
            .expect("the first refresh downloads the image"),
        ImageRefreshOutcome::Pulled {
            id: "sha256:1111".to_owned()
        }
    );
    assert_eq!(
        reports.taken(),
        vec![
            ImageRefreshReport::Started {
                host: "local podman".to_owned(),
                image: "ghcr.io/example/dev:1.2.3".to_owned(),
            },
            ImageRefreshReport::Pulled {
                host: "local podman".to_owned(),
                image: "ghcr.io/example/dev:1.2.3".to_owned(),
            },
        ],
        "the user should hear about the download and about it finishing"
    );
    assert_eq!(
        refresh_host_image(&refresh, &executor, &reports.record())
            .expect("the second refresh finds it present"),
        ImageRefreshOutcome::Present
    );
    assert!(
        reports.taken().is_empty(),
        "an hourly check that downloads nothing has nothing to say"
    );

    let commands = executor.commands.lock().unwrap();
    let pulls = commands
        .iter()
        .filter(|args| args.first().map(String::as_str) == Some("pull"))
        .count();
    assert_eq!(pulls, 1, "the image was downloaded twice: {commands:?}");
    let prunes = commands
        .iter()
        .filter(|args| args.contains(&"prune".to_owned()))
        .count();
    assert_eq!(
        prunes, 1,
        "only the refresh that downloaded the image has anything to prune: {commands:?}"
    );
    assert!(
        commands
            .last()
            .is_some_and(|args| args.contains(&"inspect".to_owned())),
        "the second refresh should stop after finding the image present: {commands:?}"
    );
}

/// A moving tag keeps its hourly pull: a present image is not the same as a
/// current one.
#[test]
fn an_always_refresh_pulls_even_when_the_image_is_present() {
    struct PresentExecutor {
        commands: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl CommandExecutor for PresentExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(command.args.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: b"sha256:2222\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let refresh = crate::targets::image_refresh(
        crate::targets::ImageHost::LocalPodman,
        "ghcr.io/example/dev:latest",
        None,
        mj_core::config::ImagePullPolicy::Auto,
    )
    .expect("a remote latest image is refreshed");
    assert_eq!(refresh.when, crate::targets::RefreshWhen::Always);

    let executor = PresentExecutor {
        commands: std::sync::Mutex::new(Vec::new()),
    };
    assert_eq!(
        refresh_host_image(&refresh, &executor, &|_report| {}).expect("the refresh runs"),
        ImageRefreshOutcome::Unchanged
    );

    let commands = executor.commands.lock().unwrap();
    assert!(
        commands
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("pull")),
        "a moving tag must still be pulled: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|args| args.contains(&"prune".to_owned())),
        "a pull that ran still prunes: {commands:?}"
    );
}

/// A background refresh is a chore, not a launch. One host that cannot
/// reach its registry must not cost the other hosts their pull, and the
/// failure has to say which host, which image, and what the engine
/// reported. `refresh_images` gives every host its own task for the same
/// reason.
#[test]
fn a_failed_pull_is_reported_and_leaves_the_other_host_alone() {
    struct FailingPullExecutor {
        failing_image: String,
        commands: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl CommandExecutor for FailingPullExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands
                .lock()
                .unwrap()
                .push((command.program.clone(), command.args.clone()));
            if command.args.contains(&"pull".to_owned())
                && command.args.contains(&self.failing_image)
            {
                return Ok(CommandOutput {
                    status: 125,
                    stdout: Vec::new(),
                    stderr: b"short-name resolution failed".to_vec(),
                });
            }
            Ok(CommandOutput {
                status: 0,
                stdout: b"sha256:1111\n".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    let failing_image = "ghcr.io/example/broken:latest";
    let broken = crate::targets::image_refresh(
        crate::targets::ImageHost::LocalPodman,
        failing_image,
        None,
        mj_core::config::ImagePullPolicy::Auto,
    )
    .expect("a remote latest image is refreshed");
    let healthy = crate::targets::image_refresh(
        crate::targets::ImageHost::LocalDocker,
        "ghcr.io/example/dev:latest",
        None,
        mj_core::config::ImagePullPolicy::Auto,
    )
    .expect("a remote latest image is refreshed");
    let executor = FailingPullExecutor {
        failing_image: failing_image.to_owned(),
        commands: std::sync::Mutex::new(Vec::new()),
    };

    let reports = RefreshReports::default();
    let mut last_failures = BTreeMap::new();

    let reported = refresh_host_image(&broken, &executor, &reports.record())
        .expect_err("a failed pull has to reach the caller");
    let reported = format!("{reported:#}");
    assert!(
        reported.contains("short-name resolution failed"),
        "{reported}"
    );
    assert!(reported.contains(failing_image), "{reported}");
    record_refresh_result(
        &mut last_failures,
        &broken.host.label(),
        &broken.image,
        Some(reported.clone()),
        &reports.record(),
    );

    refresh_host_image(&healthy, &executor, &reports.record())
        .expect("the second host still refreshes");
    record_refresh_result(
        &mut last_failures,
        &healthy.host.label(),
        &healthy.image,
        None,
        &reports.record(),
    );

    let failures = reports
        .taken()
        .into_iter()
        .filter(|report| matches!(report, ImageRefreshReport::Failed { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        failures,
        vec![ImageRefreshReport::Failed {
            host: "local podman".to_owned(),
            image: failing_image.to_owned(),
            error: reported,
        }],
        "only the host that could not pull is reported as failed"
    );

    let commands = executor.commands.lock().unwrap();
    let ran = |program: &str, args: &[&str]| {
        commands
            .iter()
            .any(|(command, arguments)| command == program && arguments == args)
    };
    assert!(ran("podman", &["pull", failing_image]), "{commands:?}");
    assert!(
        !ran("podman", &["image", "prune", "-f"]),
        "a host that could not pull has nothing to prune: {commands:?}"
    );
    assert!(
        ran("docker", &["pull", "ghcr.io/example/dev:latest"]),
        "{commands:?}"
    );
    assert!(ran("docker", &["image", "prune", "-f"]), "{commands:?}");
}

/// An unreachable host fails the same way every hour. The user hears about it
/// once, and hears again only when something changes.
#[test]
fn a_failed_pull_is_reported_once_until_the_error_changes() {
    let reports = RefreshReports::default();
    let mut last_failures = BTreeMap::new();
    let host = "podman on builder.example.test";
    let image = "ghcr.io/example/dev:latest";
    let fail = |last_failures: &mut BTreeMap<String, String>, error: &str| {
        record_refresh_result(
            last_failures,
            host,
            image,
            Some(error.to_owned()),
            &reports.record(),
        );
    };

    fail(
        &mut last_failures,
        "ssh: connect to host builder: timed out",
    );
    assert_eq!(reports.taken().len(), 1, "the first failure is news");

    fail(
        &mut last_failures,
        "ssh: connect to host builder: timed out",
    );
    assert!(
        reports.taken().is_empty(),
        "the same failure an hour later is not news"
    );

    fail(&mut last_failures, "podman: no space left on device");
    assert_eq!(
        reports.taken(),
        vec![ImageRefreshReport::Failed {
            host: host.to_owned(),
            image: image.to_owned(),
            error: "podman: no space left on device".to_owned(),
        }],
        "a different failure is news again"
    );

    // A success clears the record, so the next failure is news even if it
    // reads exactly like the last one.
    record_refresh_result(&mut last_failures, host, image, None, &reports.record());
    assert!(reports.taken().is_empty(), "a success says nothing here");
    fail(&mut last_failures, "podman: no space left on device");
    assert_eq!(
        reports.taken().len(),
        1,
        "a failure after a success is news again"
    );
}

/// The default configuration names every local engine, installed or not. An
/// engine that is not on the machine is skipped, not reported as a failed
/// download; a remote host is always tried, because its engine is elsewhere.
#[test]
fn an_uninstalled_local_engine_is_skipped_by_the_image_refresh() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("podman"), "").unwrap();
    let path = std::env::join_paths([directory.path()]).unwrap();

    assert!(local_engine_installed(&ImageHost::LocalPodman, Some(&path)));
    assert!(!local_engine_installed(
        &ImageHost::LocalDocker,
        Some(&path)
    ));
    assert!(!local_engine_installed(&ImageHost::LocalPodman, None));
    assert!(local_engine_installed(
        &ImageHost::SshDocker(crate::targets::SshTarget {
            destination: "build@example".into(),
            ssh_args: Vec::new(),
        }),
        None,
    ));
}
