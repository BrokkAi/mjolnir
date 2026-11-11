/// The poller used to re-read and re-deserialise every live session's whole
/// transcript on every runtime snapshot, then compare ordinals to discover
/// that nothing had moved. On a real session that is 28,066 rows and
/// 635 MiB, per poll. The comparison has to happen before the read.
// Hard-won: 8ba5512a: every unchanged poll reread a large transcript and delayed the prompt
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
        policy_config_keys: Vec::new(),
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
use mj_core::config::Config;

fn podman_controller(state: SessionState) -> Controller {
    let session_id = "0123456789abcdef0123456789abcdef";
    let mut config = Config::default();
    config.profiles.insert(
        "codex".into(),
        mj_core::config::HarnessProfile {
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                PathBuf::from("/home/dev/.codex"),
            )
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
                environment: Default::default(),
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
            oracle_choice: None,
            oracle: true,
            project: None,
            target_runtime: None,
            launch_base: None,
            launch_branch: None,
            checkout: None,
            publication: None,
            build_cache: None,
            container_workspace: None,
            subagents: None,
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
            review: None,
            no_project_memory: false,
            no_github_events: false,
            no_github_credentials: false,
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
    assert!(credential_sync_targets(&recoverable_error).is_empty());
}

/// #1161: a parked sub-agent's worker was stopped on purpose. Nothing that
/// dials, samples or credentials live workers may reach it, and the startup
/// repair of interrupted lifecycles leaves it parked.
// Hard-won: 6927da2b: stopped parked children consumed container process slots and were restarted
#[test]
fn a_parked_sub_agent_stays_out_of_live_target_pollers_and_startup_repair() {
    let parked = podman_controller(SessionState::Parked);
    assert!(
        parked
            .state
            .sessions
            .values()
            .all(|session| session.target.is_some() && session.state.is_active()),
        "the parked session keeps its target and its place on the dashboard"
    );
    assert!(
        !parked
            .state
            .sessions
            .values()
            .any(session_target_is_pollable)
    );
    assert!(dashboard_worker_targets(&parked).is_empty());
    assert!(credential_sync_targets(&parked).is_empty());
    assert!(interrupted_suspend_session_ids(&parked).is_empty());
    assert!(unowned_interrupted_lifecycles(&parked, &Default::default()).is_empty());
}

/// I2-9: a destroy that failed leaves the session `Error` with its target and
/// the recorded destruction failure; startup finishes it, and nothing else in
/// `Error` is destroyed on the person's behalf.
// Hard-won: 4119688f: failed destroy left the session Running and caused repeated relay spawns
#[test]
fn startup_finishes_only_destroys_that_failed() {
    let mut failed = podman_controller(SessionState::Error);
    let id = "0123456789abcdef0123456789abcdef";
    failed.state.sessions.get_mut(id).unwrap().last_error = Some(format!(
        "{}; the daemon log records the reason",
        mj_core::state::DESTRUCTION_FAILURE_PREFIX
    ));
    assert_eq!(interrupted_destroy_session_ids(&failed).len(), 1);
    failed.state.sessions.get_mut(id).unwrap().last_error = Some(format!(
        "{}; the daemon log records the reason",
        mj_core::state::CLOSE_FAILURE_PREFIX
    ));
    assert!(interrupted_destroy_session_ids(&failed).is_empty());
    let running = podman_controller(SessionState::Running);
    assert!(interrupted_destroy_session_ids(&running).is_empty());
}

/// R7-3: a bare target has no container to sample, so the resource poller
/// has nothing to ask it. It must skip the session without a warning: the
/// dashboard rebuilds these targets on every poll, and each bare session
/// used to log one warning each time (455 in 15 minutes with 12 sessions).
/// Its worker is still polled.
// Hard-won: e53a282f: polling raced worker installation and hit ETXTBSY
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

    // Provisioning connects to its own worker and then marks the session
    // running, which is when there is something to poll.
    let running = podman_controller(SessionState::Running);
    assert_eq!(dashboard_worker_targets(&running).len(), 1);
}

#[test]
fn failed_destruction_stays_out_of_pollers_without_an_active_lifecycle() {
    let mut controller = podman_controller(SessionState::Destroying);
    for id in controller
        .state
        .sessions
        .keys()
        .cloned()
        .collect::<Vec<_>>()
    {
        let session = controller
            .state
            .sessions
            .get_mut(&id)
            .expect("collected session");
        session.last_error =
            Some("verified checkpoint retained; cleanup is safely retryable".into());
    }
    // No in-flight lifecycle exclusion survives a failed close or restart.
    let excluded = std::collections::BTreeSet::new();
    for _ in 0..3 {
        assert!(dashboard_worker_targets_excluding(&controller, &excluded).is_empty());
        assert!(credential_sync_targets(&controller).is_empty());
    }
    let closing = podman_controller(SessionState::Closing);
    assert_eq!(dashboard_worker_targets(&closing).len(), 1);
}

// Hard-won: 17824a09: Luna found a lifecycle rollback race that falsely reported projection corruption
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

/// A profile whose probe needs no network: a custom provider that publishes no
/// quota endpoint reports usage pricing.
fn probeless_request(id: &str) -> QuotaRefreshRequest {
    QuotaRefreshRequest {
        profile_id: id.into(),
        harness: mj_core::config::HarnessKind::Codex,
        auth_scheme: mj_core::config::AuthScheme::ApiKey {
            env_key: "PROVIDER_API_KEY".into(),
        },
        source_home: PathBuf::from("/nonexistent-quota-home"),
        environment: Default::default(),
        cwd: PathBuf::from("."),
        codex_source: crate::quota::CodexQuotaSource::Provider(crate::quota::ProviderCredential {
            id: "other".into(),
            host: "example.invalid".into(),
            api_key: "key".into(),
        }),
        runtime: crate::quota::QuotaRuntimeConfig::host(),
    }
}

fn stored_report(id: &str, age_seconds: u64) -> crate::quota::ProfileQuota {
    crate::quota::ProfileQuota {
        paid_credits: None,
        banked_resets: None,
        profile_id: id.into(),
        harness: mj_core::config::HarnessKind::Codex,
        windows: vec![crate::quota::QuotaWindow {
            label: "Week".into(),
            remaining_percent: Some(42),
            duration_seconds: None,
            model_scope: None,
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: None,
        }],
        extra: None,
        error: None,
        refreshed_at_epoch_seconds: epoch_seconds() - age_seconds,
        rate_limited_until_epoch_seconds: None,
    }
}

fn poller_with_store(
    stored: Vec<crate::quota::ProfileQuota>,
) -> (
    tokio::sync::watch::Sender<QuotaRefreshBatch>,
    tokio::sync::mpsc::Receiver<QuotaUpdate>,
) {
    poller_with_launcher(stored, crate::claude_usage::ClaudeLauncher::Managed)
}

/// [`poller_with_store`] whose Claude grant renewals run `launcher`, so a test
/// never reaches the managed Claude Code installation.
fn poller_with_launcher(
    stored: Vec<crate::quota::ProfileQuota>,
    launcher: crate::claude_usage::ClaudeLauncher,
) -> (
    tokio::sync::watch::Sender<QuotaRefreshBatch>,
    tokio::sync::mpsc::Receiver<QuotaUpdate>,
) {
    spawn_quota_refresher_with(
        Arc::new(move |request: &QuotaRefreshRequest| {
            stored
                .iter()
                .find(|report| report.profile_id == request.profile_id)
                .cloned()
        }),
        launcher,
    )
}

/// The renewal program of a [`renewable_claude_profile`] in `directory`.
#[cfg(all(unix, not(target_os = "macos")))]
fn fake_claude(directory: &std::path::Path) -> crate::claude_usage::ClaudeLauncher {
    crate::claude_usage::ClaudeLauncher::Program(directory.join("bin/claude"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_gate_renewal(directory: &std::path::Path) -> GrantRenewal {
    GrantRenewal {
        launcher: fake_claude(directory),
        gate: crate::upgrade::gate().clone(),
    }
}

fn batch(generation: u64, refresh: bool, ids: &[&str]) -> QuotaRefreshBatch {
    QuotaRefreshBatch {
        generation,
        refresh,
        profiles: ids.iter().map(|id| probeless_request(id)).collect(),
        grants: Vec::new(),
    }
}

async fn next_update(updates: &mut tokio::sync::mpsc::Receiver<QuotaUpdate>) -> QuotaUpdate {
    tokio::time::timeout(Duration::from_secs(20), updates.recv())
        .await
        .expect("an update arrives")
        .expect("the poller runs")
}

async fn assert_quiet(updates: &mut tokio::sync::mpsc::Receiver<QuotaUpdate>) {
    if let Ok(update) = tokio::time::timeout(Duration::from_millis(300), updates.recv()).await {
        panic!("expected no probe, got {update:?}");
    }
}

#[tokio::test]
async fn a_report_younger_than_the_interval_is_published_and_not_probed() {
    let stored = stored_report("a", 60);
    let (profiles, mut updates) = poller_with_store(vec![stored.clone()]);
    profiles.send_replace(batch(1, false, &["a"]));
    let QuotaUpdate::Report(outcome) = next_update(&mut updates).await else {
        panic!("the stored report is published first");
    };
    assert_eq!(outcome.report, stored);
    assert_quiet(&mut updates).await;
    // The next probe waits for the report to age out.
    assert_eq!(
        next_probe_at(Some(&stored)),
        stored.refreshed_at_epoch_seconds + QUOTA_REFRESH_INTERVAL.as_secs()
    );
    assert_eq!(next_probe_at(None), 0);
}

#[tokio::test]
async fn a_report_older_than_the_interval_is_probed_at_start() {
    let stored = stored_report("a", QUOTA_REFRESH_INTERVAL.as_secs() + 60);
    let (profiles, mut updates) = poller_with_store(vec![stored.clone()]);
    profiles.send_replace(batch(1, false, &["a"]));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Refreshing { profile_ids } if profile_ids == ["a"]
    ));
    let QuotaUpdate::Report(outcome) = next_update(&mut updates).await else {
        panic!("the probe reports");
    };
    assert!(outcome.report.refreshed_at_epoch_seconds > stored.refreshed_at_epoch_seconds);
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Finished { generation: 1 }
    ));
}

#[tokio::test]
async fn a_reload_probes_only_the_profile_it_added() {
    let (profiles, mut updates) = poller_with_store(vec![stored_report("a", 60)]);
    profiles.send_replace(batch(1, false, &["a"]));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Report(_)
    ));
    assert_quiet(&mut updates).await;

    profiles.send_replace(batch(2, false, &["a", "b"]));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Refreshing { profile_ids } if profile_ids == ["b"]
    ));
    let QuotaUpdate::Report(outcome) = next_update(&mut updates).await else {
        panic!("the new profile reports");
    };
    assert_eq!(outcome.report.profile_id, "b");
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Finished { generation: 2 }
    ));
    assert_quiet(&mut updates).await;
}

#[tokio::test]
async fn an_explicit_refresh_probes_every_profile_even_when_fresh() {
    let (profiles, mut updates) = poller_with_store(vec![stored_report("a", 60)]);
    profiles.send_replace(batch(1, false, &["a"]));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Report(_)
    ));
    profiles.send_replace(batch(2, true, &["a"]));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Refreshing { profile_ids } if profile_ids == ["a"]
    ));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Report(_)
    ));
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Finished { generation: 2 }
    ));
}

#[tokio::test]
async fn a_held_profile_is_not_probed_by_the_schedule_or_by_a_refresh() {
    // Its last good reading is old, so only the hold keeps it from a probe.
    let mut held = stored_report("a", QUOTA_REFRESH_INTERVAL.as_secs() + 600);
    held.rate_limited_until_epoch_seconds = Some(epoch_seconds() + 300);
    assert!(next_probe_at(Some(&held)) > epoch_seconds());

    let (profiles, mut updates) = poller_with_store(vec![held.clone()]);
    profiles.send_replace(batch(1, false, &["a", "b"]));
    // The held profile is published with its hold and its old reading.
    let QuotaUpdate::Report(outcome) = next_update(&mut updates).await else {
        panic!("the held report is published");
    };
    assert_eq!(outcome.report, held);
    // Only the profile that is not held is probed.
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Refreshing { profile_ids } if profile_ids == ["b"]
    ));
    assert!(
        matches!(next_update(&mut updates).await, QuotaUpdate::Report(o) if o.report.profile_id == "b")
    );
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Finished { .. }
    ));

    // An explicit refresh asks the profile that is not held, and not the held one.
    profiles.send_replace(batch(2, true, &["a", "b"]));
    let update = next_update(&mut updates).await;
    assert!(
        matches!(&update, QuotaUpdate::Refreshing { profile_ids } if profile_ids == &["b"]),
        "{update:?}"
    );
    assert!(
        matches!(next_update(&mut updates).await, QuotaUpdate::Report(o) if o.report.profile_id == "b")
    );
    assert!(matches!(
        next_update(&mut updates).await,
        QuotaUpdate::Finished { generation: 2 }
    ));
}

#[cfg(all(unix, not(target_os = "macos")))]
const EIGHT_HOURS_MS: i64 = 8 * 3_600_000;
#[cfg(all(unix, not(target_os = "macos")))]
const ONE_HOUR_MS: i64 = 3_600_000;

/// A stand-in for `claude auth login` in `directory/bin` that exchanges the
/// refresh token it is given for a login lasting `renewed_lifetime_ms` (the real
/// command grants 8 h), numbering its tokens so each renewal needs the last one,
/// and a profile home whose login expires in `expires_in_ms` (an hour is inside
/// the 4 h lead).
/// Returns the request for that profile and its credential file.
#[cfg(all(unix, not(target_os = "macos")))]
fn renewable_claude_profile(
    directory: &std::path::Path,
    id: &str,
    renewed_lifetime_ms: i64,
    expires_in_ms: i64,
) -> (QuotaRefreshRequest, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let bin = directory.join("bin");
    let home = directory.join(format!("home-{id}"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let claude = bin.join("claude");
    if !claude.exists() {
        std::fs::write(
            &claude,
            format!(
                r#"#!/bin/sh
n=$(cat "$CLAUDE_CONFIG_DIR/renewals" 2>/dev/null || echo 0)
n=$((n + 1))
test "$CLAUDE_CODE_OAUTH_REFRESH_TOKEN" = "refresh-$n" || exit 3
echo "$n" > "$CLAUDE_CONFIG_DIR/renewals"
expires=$(( $(date +%s) * 1000 + {renewed_lifetime_ms} ))
printf '{{"claudeAiOauth":{{"accessToken":"access-%s","refreshToken":"refresh-%s","expiresAt":%s,"scopes":["user:inference"]}}}}' "$((n + 1))" "$((n + 1))" "$expires" > "$CLAUDE_CONFIG_DIR/.credentials.json"
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let credentials = home.join(".credentials.json");
    let expires_in_an_hour = mj_core::clock::epoch_millis() + expires_in_ms;
    std::fs::write(
        &credentials,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"access-1","refreshToken":"refresh-1","expiresAt":{expires_in_an_hour},"scopes":["user:inference"]}}}}"#
        ),
    )
    .unwrap();
    let request = QuotaRefreshRequest {
        profile_id: id.into(),
        harness: mj_core::config::HarnessKind::Claude,
        auth_scheme: mj_core::config::AuthScheme::NativeLogin,
        source_home: home,
        environment: [(
            "PATH".to_owned(),
            format!("{}:/usr/bin:/bin", bin.display()),
        )]
        .into(),
        cwd: PathBuf::from("."),
        codex_source: crate::quota::CodexQuotaSource::UsagePriced {
            provider: String::new(),
        },
        runtime: crate::quota::QuotaRuntimeConfig::host(),
    };
    (request, credentials)
}

/// #1490: the daemon renews a Claude grant when under half of its 8 h access
/// token remains, even while the usage API holds the profile's probes, and
/// also for a disabled profile that only has live sessions (`grants`), because
/// session copies hold no refresh token and cannot do it themselves.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn a_held_claude_profile_still_renews_its_grant_early() {
    let directory = tempfile::tempdir().unwrap();
    let (held_request, held_credentials) =
        renewable_claude_profile(directory.path(), "claude1", EIGHT_HOURS_MS, ONE_HOUR_MS);
    let (grant_only, grant_only_credentials) =
        renewable_claude_profile(directory.path(), "claude2", EIGHT_HOURS_MS, ONE_HOUR_MS);

    let mut held = stored_report("claude1", QUOTA_REFRESH_INTERVAL.as_secs() + 600);
    held.harness = mj_core::config::HarnessKind::Claude;
    held.rate_limited_until_epoch_seconds = Some(epoch_seconds() + 3_600);
    let (profiles, mut updates) =
        poller_with_launcher(vec![held.clone()], fake_claude(directory.path()));
    profiles.send_replace(QuotaRefreshBatch {
        generation: 1,
        refresh: false,
        profiles: vec![held_request],
        grants: vec![grant_only],
    });
    // The held report is published and the usage API is not asked.
    let QuotaUpdate::Report(outcome) = next_update(&mut updates).await else {
        panic!("the held report is published");
    };
    assert_eq!(outcome.report, held);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    for credentials in [&held_credentials, &grant_only_credentials] {
        while std::fs::read_to_string(credentials)
            .unwrap()
            .contains("access-1")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the grant was not renewed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            std::fs::read_to_string(credentials)
                .unwrap()
                .contains("refresh-2")
        );
    }
    // The grant-only profile is never probed or published.
    assert_quiet(&mut updates).await;
}

/// #1490 review: with no enabled profile to drive probe wakeups (only a
/// grant-only one), the poller still wakes for each renewal. The stand-in
/// grants a login due again within seconds, so the second renewal shows the
/// schedule survived the first.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn the_poller_wakes_for_every_renewal_with_only_grant_only_profiles() {
    let directory = tempfile::tempdir().unwrap();
    // Due again 3 s after each renewal: expiry = lead + 3 s.
    let lifetime =
        i64::try_from(crate::claude_usage::GRANT_REFRESH_LEAD.as_millis()).unwrap() + 3_000;
    let (request, credentials) =
        renewable_claude_profile(directory.path(), "claude1", lifetime, ONE_HOUR_MS);
    let (profiles, mut updates) = poller_with_launcher(Vec::new(), fake_claude(directory.path()));
    profiles.send_replace(QuotaRefreshBatch {
        generation: 1,
        refresh: false,
        profiles: Vec::new(),
        grants: vec![request],
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for renewal in ["access-2", "access-3"] {
        while !std::fs::read_to_string(&credentials)
            .unwrap()
            .contains(renewal)
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the poller did not wake for {renewal}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    assert_quiet(&mut updates).await;
}

/// #1490 review: once `claude auth login` has started it runs to its end even
/// when the caller is dropped (daemon shutdown, a cancelled task): Anthropic
/// has spent the refresh token by then, and a kill before the new login is
/// written would lose the grant.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn a_started_claude_login_exchange_survives_its_caller_being_dropped() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    // A slow exchange: announces itself, then takes a second to persist.
    let bin = directory.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        r#"#!/bin/sh
touch "$CLAUDE_CONFIG_DIR/started"
sleep 1
printf '{"claudeAiOauth":{"accessToken":"access-2","refreshToken":"refresh-2","expiresAt":%s,"scopes":["user:inference"]}}' "$(( $(date +%s) * 1000 + 28800000 ))" > "$CLAUDE_CONFIG_DIR/.credentials.json"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (request, credentials) =
        renewable_claude_profile(directory.path(), "claude1", EIGHT_HOURS_MS, ONE_HOUR_MS);
    let started = credentials.with_file_name("started");
    let launcher = fake_claude(directory.path());
    let gate = crate::upgrade::gate().clone();
    let caller = tokio::spawn(async move {
        crate::quota::refresh_claude_grant(
            &request,
            mj_core::clock::epoch_millis(),
            &launcher,
            &gate,
        )
        .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the exchange never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    caller.abort();
    while std::fs::read_to_string(&credentials)
        .unwrap()
        .contains("access-1")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the exchange was cut short with the caller"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// #1490 review: a renewal that has not begun is skipped while a daemon
/// handoff waits (its admission is refused), so the handoff is never held for
/// one and a refresh-token exchange is never cut short by it. The next daemon
/// decides again from the file.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn a_due_claude_renewal_is_skipped_while_a_handoff_waits() {
    let directory = tempfile::tempdir().unwrap();
    let (request, credentials) =
        renewable_claude_profile(directory.path(), "claude1", EIGHT_HOURS_MS, ONE_HOUR_MS);
    let gate = Arc::new(crate::upgrade::Gate::default());
    // Control work is running and a handoff asks to close: the gate drains.
    let running = gate.enter("session lifecycle").unwrap();
    assert!(!gate.try_close());
    let (_batches, receiver) = tokio::sync::watch::channel(QuotaRefreshBatch {
        generation: 1,
        refresh: false,
        profiles: Vec::new(),
        grants: vec![request],
    });
    spawn_claude_grant_refresher(
        receiver,
        Duration::from_millis(300),
        GrantRenewal {
            launcher: fake_claude(directory.path()),
            gate: gate.clone(),
        },
    );
    // Several rechecks pass; none starts the exchange.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        std::fs::read_to_string(&credentials)
            .unwrap()
            .contains("access-1"),
        "a renewal started while a handoff was waiting"
    );
    assert!(
        !credentials.with_file_name("renewals").exists(),
        "the renewal program ran"
    );
    drop(running);
}

/// #1490 review: a profile reconfigured under the same ID (another home) is
/// renewed at once under its new home, not after the old home's schedule.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn a_reconfigured_claude_profile_is_renewed_under_its_new_home_at_once() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    // The old home is not due for hours, so its task sleeps a full interval.
    let (old_home, old_credentials) =
        renewable_claude_profile(first.path(), "claude1", EIGHT_HOURS_MS, 7 * ONE_HOUR_MS);
    let (new_home, new_credentials) =
        renewable_claude_profile(second.path(), "claude1", EIGHT_HOURS_MS, ONE_HOUR_MS);
    let (profiles, _updates) = poller_with_launcher(Vec::new(), fake_claude(first.path()));
    let batch = |request| QuotaRefreshBatch {
        generation: 1,
        refresh: false,
        profiles: Vec::new(),
        // Grant-only, so the poller never asks the usage API about it.
        grants: vec![request],
    };
    profiles.send_replace(batch(old_home));
    tokio::time::sleep(Duration::from_millis(500)).await;
    profiles.send_replace(batch(new_home));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while std::fs::read_to_string(&new_credentials)
        .unwrap()
        .contains("access-1")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the new home kept the old home's schedule"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        std::fs::read_to_string(&old_credentials)
            .unwrap()
            .contains("access-1")
    );
}

/// #1490 review: a login file replaced behind the poller's back (`mj login`
/// with a sooner-expiring login) is read again within one recheck interval;
/// no deadline is cached across it.
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn a_replaced_claude_login_with_a_sooner_expiry_is_renewed_within_a_recheck_interval() {
    let directory = tempfile::tempdir().unwrap();
    let (request, credentials) =
        renewable_claude_profile(directory.path(), "claude1", EIGHT_HOURS_MS, 7 * ONE_HOUR_MS);
    let (batches, receiver) = tokio::sync::watch::channel(QuotaRefreshBatch {
        generation: 1,
        refresh: false,
        profiles: vec![request],
        grants: Vec::new(),
    });
    spawn_claude_grant_refresher(
        receiver,
        Duration::from_secs(1),
        open_gate_renewal(directory.path()),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        std::fs::read_to_string(&credentials)
            .unwrap()
            .contains("access-1"),
        "not due yet"
    );

    let soon = mj_core::clock::epoch_millis() + ONE_HOUR_MS;
    std::fs::write(
        &credentials,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"access-1","refreshToken":"refresh-1","expiresAt":{soon},"scopes":["user:inference"]}}}}"#
        ),
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while std::fs::read_to_string(&credentials)
        .unwrap()
        .contains("access-1")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the replaced login kept its old deadline"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(batches);
}

#[test]
fn a_manual_quota_refresh_completes_when_the_daemon_finishes_a_later_cycle() {
    // The surface remembers the daemon's cycle count when it asks.
    let mut pending = Some(7);
    assert!(!complete_manual_quota_refresh(&mut pending, 7));
    assert_eq!(pending, Some(7));
    assert!(complete_manual_quota_refresh(&mut pending, 8));
    assert_eq!(pending, None);
    assert!(!complete_manual_quota_refresh(&mut pending, 9));
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
fn golden_aws_capacity_aggregates_real_probe_rows_and_rejects_overflow() {
    // These are the key/value rows emitted by the EC2 capacity probe script.
    let first = crate::targets::parse_aws_allocated_capacity(
        b"memory.total=17179869184\nlogical.cores=4\ndisk.total=42949672960\n",
        "i-0123456789abcdef0",
    )
    .expect("first EC2 probe row");
    let second = crate::targets::parse_aws_allocated_capacity(
        b"memory.total=34359738368\nlogical.cores=8\ndisk.total=85899345920\n",
        "i-0abcdef0123456789",
    )
    .expect("second EC2 probe row");
    let aggregate = aggregate_aws_capacity(&[first, second]).expect("fleet capacity totals");
    let rendered = format!(
        "Fleet memory: {} bytes\nFleet logical cores: {}\nFleet disk: {} bytes\nCPU usage: {}\nMemory used: {} bytes\nStorage samples: {}",
        aggregate.memory_total_bytes,
        aggregate.logical_cores,
        aggregate.disk_total_bytes.unwrap_or_default(),
        aggregate
            .cpu_percent
            .map_or_else(|| "unknown".to_owned(), |value| format!("{value}%")),
        aggregate.memory_used_bytes,
        aggregate.storage.len(),
    );
    mj_core::golden::assert_golden(
        env!("CARGO_MANIFEST_DIR"),
        "aws-capacity-aggregate",
        &rendered,
    );

    let usage = |memory_total_bytes, logical_cores, disk_total_bytes| DeploymentCapacityUsage {
        memory_pool: false,
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes,
        logical_cores,
        disk_total_bytes: Some(disk_total_bytes),
        storage: Vec::new(),
    };
    for (samples, expected) in [
        (
            vec![usage(u64::MAX, 0, 0), usage(1, 0, 0)],
            "aggregate EC2 RAM overflow",
        ),
        (
            vec![usage(0, u64::MAX, 0), usage(0, 1, 0)],
            "aggregate EC2 core count overflow",
        ),
        (
            vec![usage(0, 0, u64::MAX), usage(0, 0, 1)],
            "aggregate EC2 disk overflow",
        ),
    ] {
        assert_eq!(
            aggregate_aws_capacity(&samples)
                .expect_err("overflow must not wrap")
                .to_string(),
            expected
        );
    }
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
        local_storage_paths: Vec::new(),
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
// Hard-won: 1b36c736: SSH MaxSessions refusal abandoned capacity probing for the session
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
            memory_pool: false,
            cpu_percent: Some(10),
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            logical_cores: 4,
            disk_total_bytes: None,
            storage: Vec::new(),
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

/// Every harness's session on this machine is reconciled the way its session in
/// a container is: against the same canonical profile home and GitHub source.
/// A local session an earlier release started from the
/// profile home itself is left out while its staged home is a link to that
/// home, because a push would go through the link into the person's own home.
// Hard-won: #1432: removing a PAT override left its old copy in local workers.
#[test]
#[cfg_attr(windows, ignore = "#1300: fails on a Windows controller")]
fn credential_sync_covers_every_harness_on_this_machine_as_in_a_container() {
    let directory = tempfile::tempdir().unwrap();
    for kind in mj_core::config::HarnessKind::ALL {
        let mut controller = podman_controller(SessionState::Running);
        let home = directory.path().join(kind.id());
        std::fs::create_dir_all(&home).unwrap();
        controller.config.profiles.insert(
            kind.id().into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                environment: Default::default(),
                context_window_bytes: None,
                models: Default::default(),
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                codex_profile: None,
                muse_auto_review: None,
                aws: None,
                ..mj_core::config::HarnessProfile::new(kind, home.clone())
            },
        );
        controller.config.targets.insert(
            "local-bare".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        let session = controller.state.sessions.first_value_mut().unwrap();
        session.harness_kind = kind;
        session.last_profile = kind.id().into();
        let session_id = session.id.clone();

        let in_container = credential_sync_targets(&controller);
        assert_eq!(in_container.len(), 1, "{kind:?}");
        assert_eq!(in_container[0].profile_home, home, "{kind:?}");
        assert_eq!(in_container[0].harness, kind);
        assert!(in_container[0].sync_github_token, "{kind:?}");
        assert_eq!(
            in_container[0].skills_scope,
            mj_core::skills::SkillsScope::Isolated
        );

        let worker_root = directory
            .path()
            .join("workers")
            .join(kind.id())
            .join(&session_id);
        std::fs::create_dir_all(worker_root.join("profile")).unwrap();
        let session = controller.state.sessions.get_mut(&session_id).unwrap();
        session.target_template_id = "local-bare".into();
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        });

        let on_this_machine = credential_sync_targets(&controller);
        assert_eq!(on_this_machine.len(), 1, "{kind:?}");
        assert_eq!(on_this_machine[0].profile_home, home, "{kind:?}");
        assert_eq!(on_this_machine[0].harness, kind);
        assert_eq!(
            on_this_machine[0].skips_login_file_sync,
            in_container[0].skips_login_file_sync
        );
        assert!(on_this_machine[0].sync_github_token, "{kind:?}");
        controller.config.github.session_token_path = Some(directory.path().join("agent-pat"));
        let with_pat = credential_sync_targets(&controller);
        assert!(with_pat[0].sync_github_token, "{kind:?}");
        assert_ne!(
            with_pat, on_this_machine,
            "config reload must publish the new source"
        );
        assert_eq!(
            on_this_machine[0].skills_scope,
            mj_core::skills::SkillsScope::Localhost
        );

        // A nested home (Muse, OpenCode) always ran from a per-session root
        // under the data directory, so it has no linked form.
        #[cfg(unix)]
        if !kind.nested_home() {
            std::fs::remove_dir(worker_root.join("profile")).unwrap();
            std::os::unix::fs::symlink(&home, worker_root.join("profile")).unwrap();
            assert!(credential_sync_targets(&controller).is_empty(), "{kind:?}");
        }
    }
}

/// A local Codex session whose login file links to the profile's own has no
/// copy to reconcile, so the sync leaves its login file alone. A push would be
/// refused at the link, and a pull could replace a login another Codex process
/// just refreshed. A session staged with a copy is still reconciled, and a
/// harness that cannot share its login keeps its sync whatever its file is.
#[cfg(unix)]
#[test]
fn credential_sync_skips_a_local_login_file_linked_to_the_profile() {
    let directory = tempfile::tempdir().unwrap();
    for kind in [
        mj_core::config::HarnessKind::Codex,
        mj_core::config::HarnessKind::Claude,
    ] {
        let mut controller = podman_controller(SessionState::Running);
        let home = directory.path().join(kind.id());
        std::fs::create_dir_all(&home).unwrap();
        let login = mj_core::config::harness_authentication_marker(kind, &home);
        std::fs::write(&login, "{}").unwrap();
        controller.config.profiles.insert(
            kind.id().into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                environment: Default::default(),
                context_window_bytes: None,
                models: Default::default(),
                legacy_subagents: Default::default(),
                guardian_review_model: None,
                codex_profile: None,
                muse_auto_review: None,
                aws: None,
                ..mj_core::config::HarnessProfile::new(kind, home.clone())
            },
        );
        controller.config.targets.insert(
            "local-bare".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        let session = controller.state.sessions.first_value_mut().unwrap();
        let worker_root = directory
            .path()
            .join("workers")
            .join(kind.id())
            .join(&session.id);
        let staged = worker_root.join("profile");
        std::fs::create_dir_all(&staged).unwrap();
        session.harness_kind = kind;
        session.last_profile = kind.id().into();
        session.target_template_id = "local-bare".into();
        session.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: worker_root.clone(),
        });

        let staged_login = mj_core::config::harness_authentication_marker(kind, &staged);
        std::fs::write(&staged_login, "{}").unwrap();
        let copied = credential_sync_targets(&controller);
        assert_eq!(copied.len(), 1, "{kind:?}");
        assert!(!copied[0].skips_login_file_sync, "{kind:?}");

        std::fs::remove_file(&staged_login).unwrap();
        std::os::unix::fs::symlink(&login, &staged_login).unwrap();
        let linked = credential_sync_targets(&controller);
        assert_eq!(linked.len(), 1, "{kind:?}");
        assert_eq!(
            linked[0].skips_login_file_sync,
            kind.shares_local_login(),
            "{kind:?}"
        );
    }
}

/// #1160: after `mj login` for codex4, the refreshed login has to reach every
/// live session of the profile, including a sub-agent child that runs inside
/// another session's container on a remote machine. Every such session is on
/// the credential sync's list, and the child's entry reaches the child's own
/// worker inside its parent's container, where its staged home is.
#[test]
fn every_live_session_of_a_profile_is_synced_including_a_child_in_a_remote_container() {
    use mj_core::state::TargetLocator;

    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("auth.json"), r#"{"auth_mode":"chatgpt"}"#).unwrap();
    let mut controller = podman_controller(SessionState::Running);
    controller.config.profiles.insert(
        "codex4".into(),
        mj_core::config::HarnessProfile {
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                home.path().to_path_buf(),
            )
        },
    );
    controller.config.targets.insert(
        "morannon-podman".into(),
        mj_core::config::TargetTemplate::SshPodman {
            ssh: mj_core::config::SshConnection {
                host: "morannon".into(),
                user: None,
                identity_file: None,
                extra_args: Vec::new(),
            },
            container: mj_core::config::ContainerTemplate {
                build_cache: None,
                image: "ghcr.io/brokkai/mjolnir/agent-dev:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        },
    );
    let parent_id = "0123456789abcdef0123456789abcdef";
    let child_id = "1123456789abcdef0123456789abcdef";
    let sibling_id = "2123456789abcdef0123456789abcdef";
    let parent_container = "c".repeat(64);
    let on_morannon = |borrowed_from: Option<&str>, container_id: &str| TargetLocator::SshPodman {
        borrowed_from: borrowed_from.map(str::to_owned),
        host: "morannon".into(),
        container_id: container_id.into(),
        workspace_storage: Default::default(),
    };
    // A Claude parent on morannon, its Codex child in the same container,
    // and another codex4 session in a container of its own.
    let mut parent = controller.state.sessions.remove(parent_id).unwrap();
    parent.harness_kind = mj_core::config::HarnessKind::Claude;
    parent.target_template_id = "morannon-podman".into();
    parent.target = Some(on_morannon(None, &parent_container));
    let mut child = parent.clone();
    child.id = child_id.into();
    child.harness_kind = mj_core::config::HarnessKind::Codex;
    child.last_profile = "codex4".into();
    child.target = Some(on_morannon(Some(parent_id), &parent_container));
    let mut sibling = child.clone();
    sibling.id = sibling_id.into();
    sibling.target = Some(on_morannon(None, &"d".repeat(64)));
    for session in [parent, child, sibling] {
        controller
            .state
            .sessions
            .insert(session.id.clone(), session);
    }

    let targets = credential_sync_targets(&controller)
        .into_iter()
        .filter(|target| target.profile_id == "codex4")
        .collect::<Vec<_>>();
    assert_eq!(
        targets
            .iter()
            .map(|target| target.session_id.as_str())
            .collect::<Vec<_>>(),
        [child_id, sibling_id]
    );
    for target in &targets {
        assert_eq!(target.profile_home, home.path());
        assert!(!target.skips_login_file_sync);
    }
    let child_command = std::iter::once(targets[0].spec.program.clone())
        .chain(targets[0].spec.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(child_command.contains("morannon"), "{child_command}");
    assert!(child_command.contains(&parent_container), "{child_command}");
    assert!(
        child_command.contains(&format!("workers/{child_id}")),
        "{child_command}"
    );
}

#[test]
fn github_tokens_sync_to_every_session_target() {
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
    assert!(target_syncs_github_token(Some(&TargetLocator::LocalBare {
        worker_root: "/tmp/worker".into(),
    })));
    assert!(!target_syncs_github_token(None));
}

// Hard-won: c94c68c0: the shared Claude refresh grant could expire during a long-running session
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
        outcomes: vec![CredentialSyncOutcome {
            unchanged_login: None,
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(Vec::new()),
            step_failures: Vec::new(),
        }],
        partial: false,
    };

    let claude = CredentialSyncNotices::default()
        .notice(&result, Some(HarnessKind::Claude), &State::default())
        .unwrap();
    assert!(
        claude.ends_with(
            "Run `mj login --profile claude-max`, or store a long-lived token with `mj login --profile claude-max --setup-token`."
        ),
        "{claude}"
    );

    // Only Claude can rotate ahead of expiry this way.
    let codex = CredentialSyncNotices::default()
        .notice(&result, Some(HarnessKind::Codex), &State::default())
        .unwrap();
    assert!(
        codex.ends_with("Run `mj login --profile claude-max`."),
        "{codex}"
    );

    // The advice also reaches a failed reconciliation, not only a clean one.
    let failed = CredentialSyncResult {
        outcomes: vec![CredentialSyncOutcome {
            unchanged_login: None,
            session_id: "018f9dd2-a3b4".into(),
            outcome: Err("worker proxy disconnected".into()),
            step_failures: Vec::new(),
        }],
        partial: false,
        ..result
    };
    let claude_failure = CredentialSyncNotices::default()
        .notice(&failed, Some(HarnessKind::Claude), &State::default())
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
            unchanged_login: None,
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(vec![CredentialSyncAction::Pushed]),
            step_failures: Vec::new(),
        }],
        partial: false,
    };
    let notice = CredentialSyncNotices::default()
        .notice(&result, None, &State::default())
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
        partial: false,
    };
    let leveled = CredentialSyncNotices::default()
        .leveled_notice(&result, None, &State::default())
        .unwrap();
    assert!(!leveled.routine, "a failed sync stays a warning");
    let notice = leveled.text;
    assert!(notice.contains("reconciliation failed"), "{notice}");
    assert!(notice.contains("credential file is unreadable"), "{notice}");
    assert!(!notice.contains("nothing fresher"), "{notice}");
}

// Hard-won: 9805bf67: a dead worker repeated the same credential-sync failure notice every poll
#[test]
fn a_repeated_credential_failure_is_reported_once_until_it_changes() {
    use mj_core::credentials::{CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult};

    let failed = |detail: &str| CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            unchanged_login: None,
            session_id: "018f9dd2-a3b4".into(),
            outcome: Err(detail.to_owned()),
            step_failures: Vec::new(),
        }],
        partial: false,
    };
    let mut notices = CredentialSyncNotices::default();

    assert!(
        notices
            .notice(
                &failed("worker proxy disconnected"),
                None,
                &State::default()
            )
            .is_some()
    );
    assert_eq!(
        notices.notice(
            &failed("worker proxy disconnected"),
            None,
            &State::default()
        ),
        None
    );

    let changed = notices
        .notice(&failed("container is gone"), None, &State::default())
        .unwrap();
    assert!(changed.contains("container is gone"), "{changed}");
    assert_eq!(
        notices.notice(&failed("container is gone"), None, &State::default()),
        None
    );

    // A clean cycle forgets the failure, so a recurrence is reported again.
    let healthy = CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            unchanged_login: None,
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(vec![CredentialSyncAction::Pushed]),
            step_failures: Vec::new(),
        }],
        partial: false,
    };
    assert_eq!(notices.notice(&healthy, None, &State::default()), None);
    assert!(
        notices
            .notice(&failed("container is gone"), None, &State::default())
            .is_some()
    );
}

#[test]
fn a_result_clears_only_the_failures_of_the_steps_it_checked() {
    use mj_core::credentials::{
        CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult, StepFailure, SyncStep,
    };

    let result = |partial: bool, step_failures: Vec<StepFailure>| CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes: vec![CredentialSyncOutcome {
            unchanged_login: None,
            session_id: "018f9dd2-a3b4".into(),
            outcome: Ok(vec![CredentialSyncAction::Pushed]),
            step_failures,
        }],
        partial,
    };
    let failure = |step, detail: &str| {
        vec![StepFailure {
            step,
            detail: detail.into(),
        }]
    };
    // The session's own AWS renewal reports (a partial result); the profile's
    // pass reports the login (a full one).
    let aws_failed = || result(true, failure(SyncStep::Aws, "AWS credentials: refused"));
    let aws_fine = || result(true, Vec::new());
    let login_failed = || {
        result(
            false,
            failure(SyncStep::HarnessLogin, "harness login: refused"),
        )
    };
    let login_fine = || result(false, Vec::new());
    let mut notices = CredentialSyncNotices::default();
    let state = State::default();

    assert!(notices.notice(&aws_failed(), None, &state).is_some());
    assert!(notices.notice(&login_failed(), None, &state).is_some());

    // The AWS success ends the AWS failure only: the login failure is not
    // reported again, and a recurrence of the AWS one is.
    assert_eq!(notices.notice(&aws_fine(), None, &state), None);
    assert_eq!(notices.notice(&login_failed(), None, &state), None);
    assert!(notices.notice(&aws_failed(), None, &state).is_some());

    // The login success ends the login failure only.
    assert_eq!(notices.notice(&login_fine(), None, &state), None);
    assert_eq!(notices.notice(&aws_failed(), None, &state), None);
    assert!(notices.notice(&login_failed(), None, &state).is_some());

    // Two steps failing in one pass are both said, in one notice.
    let mut two_steps = login_failed();
    two_steps.outcomes[0].step_failures.push(StepFailure {
        step: SyncStep::Skills,
        detail: "skills: archive unreadable".into(),
    });
    let said = CredentialSyncNotices::default()
        .notice(&two_steps, None, &state)
        .unwrap();
    assert!(
        said.contains("harness login: refused") && said.contains("skills: archive unreadable"),
        "{said}"
    );

    // Two failures of one step are both said.
    let mut both = aws_failed();
    both.outcomes[0].step_failures.push(StepFailure {
        step: SyncStep::Aws,
        detail: "AWS credentials withdrawal: worker too old".into(),
    });
    let said = CredentialSyncNotices::default()
        .notice(&both, None, &state)
        .unwrap();
    assert!(
        said.contains("AWS credentials: refused") && said.contains("worker too old"),
        "{said}"
    );
}

// Hard-won: 9805bf67: a dead worker repeated the same credential-sync failure notice every poll
#[test]
fn a_repeated_whole_sync_failure_is_reported_once_per_profile() {
    use mj_core::credentials::CredentialSyncResult;

    let failed = |profile_id: &str| CredentialSyncResult {
        profile_id: profile_id.to_owned(),
        trigger: None,
        failure: Some("controller home is unreadable".into()),
        outcomes: Vec::new(),
        partial: false,
    };
    let mut notices = CredentialSyncNotices::default();

    let notice = notices
        .notice(&failed("work"), None, &State::default())
        .unwrap();
    assert!(notice.contains("profile work"), "{notice}");
    assert_eq!(
        notices.notice(&failed("work"), None, &State::default()),
        None
    );
    // Another profile failing the same way is its own key.
    assert!(
        notices
            .notice(&failed("personal"), None, &State::default())
            .is_some()
    );
    assert_eq!(
        notices.notice(&failed("work"), None, &State::default()),
        None
    );
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

// Hard-won: #1358: the image refresher restarted an Apple container service the user had stopped
// Hard-won: #1476: Apple image refreshes omitted cleanup and accumulated 128 GB of old data.
#[test]
fn the_image_refresher_skips_a_stopped_apple_container_service_without_starting_it() {
    struct AppleExecutor {
        service_running: bool,
        failed_command: Option<&'static str>,
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl CommandExecutor for AppleExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let line = format!("{} {}", command.program, command.args.join(" "));
            let stopped = line == "container system status" && !self.service_running;
            let failed = self.failed_command == Some(line.as_str());
            self.commands.lock().unwrap().push(line);
            Ok(CommandOutput {
                status: i32::from(stopped || failed),
                stdout: b"sha256:1111\n".to_vec(),
                stderr: if failed {
                    b"image store unavailable".to_vec()
                } else {
                    Vec::new()
                },
            })
        }
    }

    let refresh = || {
        crate::targets::image_refresh(
            crate::targets::ImageHost::AppleContainer,
            "ghcr.io/example/dev:latest",
            None,
            mj_core::config::ImagePullPolicy::Auto,
        )
        .expect("a remote latest image is refreshed")
    };
    let reports = RefreshReports::default();

    let stopped = AppleExecutor {
        service_running: false,
        failed_command: None,
        commands: std::sync::Mutex::new(Vec::new()),
    };
    assert_eq!(
        refresh_one_host(refresh(), &stopped, &reports.record()),
        None
    );
    assert_eq!(
        *stopped.commands.lock().unwrap(),
        ["container system status"],
        "a stopped service is observed, never started, and nothing is pulled"
    );
    assert!(reports.taken().is_empty());

    let running = AppleExecutor {
        service_running: true,
        failed_command: None,
        commands: std::sync::Mutex::new(Vec::new()),
    };
    assert_eq!(
        refresh_one_host(refresh(), &running, &reports.record()),
        Some((
            "apple container".to_owned(),
            "ghcr.io/example/dev:latest".to_owned(),
            None
        ))
    );
    let commands = running.commands.lock().unwrap();
    assert_eq!(
        *commands,
        [
            "container system status",
            "container image inspect ghcr.io/example/dev:latest",
            "container image pull ghcr.io/example/dev:latest",
            "container image inspect ghcr.io/example/dev:latest",
            "container image prune",
        ],
        "a successful pull must reclaim orphaned data without deleting tagged preloads"
    );
    drop(commands);

    for failed_command in [
        "container image pull ghcr.io/example/dev:latest",
        "container image prune",
    ] {
        let failing = AppleExecutor {
            service_running: true,
            failed_command: Some(failed_command),
            commands: std::sync::Mutex::new(Vec::new()),
        };
        let (_, _, error) = refresh_one_host(refresh(), &failing, &reports.record())
            .expect("the failure is returned for the daemon to report");
        assert!(error.unwrap().contains("image store unavailable"));
        let commands = failing.commands.lock().unwrap();
        assert_eq!(
            commands.last().map(String::as_str),
            Some(failed_command),
            "a failed download must not proceed to cleanup"
        );
    }
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
// Hard-won: a2d660f1: absent local engines caused startup image-refresh warnings on every daemon start
#[test]
#[cfg_attr(windows, ignore = "#1300: fails on a Windows controller")]
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

/// R8-3: every credential-sync notice named its session by short id ("Session
/// 790051c5 returned no response; ...", tmux/047). A notice names a session
/// the way the session list does, and by short id only when it has no title
/// or its record is gone (the R5-4/R5-5 rule).
// Hard-won: 3ce5c293: credential notices showed short IDs instead of the title users saw
#[test]
fn credential_sync_notices_name_the_session_by_its_listed_title() {
    use mj_core::credentials::{CredentialSyncAction, CredentialSyncOutcome, CredentialSyncResult};

    let session_id = "790051c5fe0447276d78ee1e5548bd4e";
    let mut state = State::default();
    let mut session = crate::controller::test_support::checkpoint_test_session(session_id);
    session.acp_session_title = Some("claude-a".into());
    state.sessions.insert(session_id.into(), session);

    let triggered =
        |reason: CredentialSyncReason,
         failure: Option<&str>,
         outcome: Option<Result<Vec<CredentialSyncAction>, String>>| CredentialSyncResult {
            profile_id: "claude".into(),
            trigger: Some(CredentialSyncCause {
                session_id: session_id.into(),
                reason,
            }),
            failure: failure.map(str::to_owned),
            outcomes: outcome
                .map(|outcome| {
                    vec![CredentialSyncOutcome {
                        unchanged_login: None,
                        session_id: session_id.into(),
                        outcome,
                        step_failures: Vec::new(),
                    }]
                })
                .unwrap_or_default(),
            partial: false,
        };
    let results = [
        triggered(
            CredentialSyncReason::AuthenticationFailure,
            Some("credential file is unreadable"),
            None,
        ),
        triggered(
            CredentialSyncReason::EmptyPromptResponse,
            Some("credential file is unreadable"),
            None,
        ),
        triggered(
            CredentialSyncReason::AuthenticationFailure,
            None,
            Some(Ok(vec![CredentialSyncAction::Pushed])),
        ),
        triggered(
            CredentialSyncReason::AuthenticationFailure,
            None,
            Some(Ok(Vec::new())),
        ),
        triggered(
            CredentialSyncReason::EmptyPromptResponse,
            None,
            Some(Ok(vec![CredentialSyncAction::Pushed])),
        ),
        triggered(
            CredentialSyncReason::EmptyPromptResponse,
            None,
            Some(Ok(Vec::new())),
        ),
        CredentialSyncResult {
            profile_id: "claude".into(),
            trigger: None,
            failure: None,
            outcomes: vec![CredentialSyncOutcome {
                unchanged_login: None,
                session_id: session_id.into(),
                outcome: Err("worker proxy disconnected".into()),
                step_failures: Vec::new(),
            }],
            partial: false,
        },
    ];
    for result in &results {
        let notice = CredentialSyncNotices::default()
            .notice(result, None, &state)
            .unwrap();
        assert!(notice.contains("claude-a"), "{notice}");
        assert!(!notice.contains("790051c5"), "{notice}");
    }

    // A session whose record is gone is still named, by its short id.
    let notice = CredentialSyncNotices::default()
        .notice(&results[5], None, &State::default())
        .unwrap();
    assert!(
        notice.starts_with("Session 790051c5 returned no response"),
        "{notice}"
    );
}

// Hard-won: c8d27f3c: a stale coordinator tried a parked worker after the daemon replaced it
#[test]
fn credential_sync_rejects_targets_captured_before_parking_or_replacement() {
    let running = podman_controller(SessionState::Running);
    let target = credential_sync_targets(&running).pop().unwrap();
    assert!(credential_sync_target_is_current(running, &target));
    assert!(!credential_sync_target_is_current(
        podman_controller(SessionState::Parked),
        &target,
    ));
    let mut replaced = podman_controller(SessionState::Running);
    replaced
        .state
        .sessions
        .get_mut(&target.session_id)
        .unwrap()
        .target = Some(mj_core::state::TargetLocator::LocalPodman {
        borrowed_from: None,
        container_id: "b".repeat(64),
        workspace_storage: Default::default(),
    });
    assert!(!credential_sync_target_is_current(replaced, &target));
}

#[test]
fn a_deferred_credential_sync_does_not_claim_the_login_was_checked() {
    use mj_core::credentials::CredentialSyncResult;
    for reason in [
        CredentialSyncReason::AuthenticationFailure,
        CredentialSyncReason::EmptyPromptResponse,
    ] {
        let result = CredentialSyncResult {
            profile_id: "work".into(),
            trigger: Some(CredentialSyncCause {
                session_id: "parked-child".into(),
                reason,
            }),
            failure: None,
            outcomes: Vec::new(),
            partial: false,
        };
        assert!(
            CredentialSyncNotices::default()
                .notice(&result, None, &State::default())
                .is_none()
        );
    }
}

/// A TUI handle used to need both a view from the daemon and the session in a
/// target list the dashboard derived from its records minus its own lifecycle
/// operations. A lifecycle that failed left an idle session in the list but
/// without a view, so it could never be opened again. The daemon's views are
/// now the only input: a handle exists exactly while one is published.
// Hard-won: 8b3a7286: a failed Move left the session unopenable until relay retry timed out
#[tokio::test]
async fn a_remote_handle_exists_exactly_while_the_daemon_publishes_its_view() {
    let channels = spawn_remote_session_manager().unwrap();
    let wait = Duration::from_secs(1);
    let view = || ManagedSessionView {
        snapshot: None,
        connected: true,
        error: None,
    };

    super::remote::mirror_daemon_view(
        &channels.targets,
        &channels.publisher,
        "session-1".into(),
        view(),
    )
    .await
    .unwrap();
    channels
        .control
        .wait_for_session("session-1", wait)
        .await
        .unwrap();

    super::remote::mirror_daemon_removal(&channels.targets, "session-1");
    tokio::time::timeout(wait, async {
        while channels.control.session("session-1").await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a session the daemon stopped publishing kept its handle");

    super::remote::mirror_daemon_view(
        &channels.targets,
        &channels.publisher,
        "session-1".into(),
        view(),
    )
    .await
    .unwrap();
    channels
        .control
        .wait_for_session("session-1", wait)
        .await
        .unwrap();
}

#[test]
fn cancelled_operation_leaves_the_running_recovery_copy_alone() {
    let (observations, _receiver) = crate::recovery_gate::observation_channel();
    let observer = crate::recovery_gate::RecoveryObserver {
        observations,
        gate: Arc::new(crate::recovery_gate::RecoveryGate::default()),
    };
    let copy = observer.gate.try_start("session").unwrap();

    let cancelled = AtomicBool::new(true);
    let error = reserve_recovery_or_cancel(&observer, "session", &cancelled)
        .err()
        .expect("a cancelled operation does not reserve");
    assert!(error.to_string().contains("operation cancelled"));
    assert!(observer.is_busy("session"));
    assert!(!copy.load(Ordering::Acquire));
    // The failed call also left no reservation behind.
    drop(copy);
    assert!(observer.gate.try_start("session").is_some());

    // An operation cancelled while it waits still preempts the copy first.
    let copy = observer.gate.try_start("session").unwrap();
    let cancelled = AtomicBool::new(false);
    let reservation = std::thread::scope(|scope| {
        let waiting = scope.spawn(|| reserve_recovery_or_cancel(&observer, "session", &cancelled));
        while !copy.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(copy);
        waiting.join().unwrap()
    });
    assert!(reservation.is_ok());
}

// Hard-won: c2d74db67: the refresher's first look could run while daemon startup was still starting Apple's container service, read it as stopped, and skip a whole interval.
#[tokio::test(start_paused = true)]
async fn only_the_apple_container_refresh_waits_for_its_startup() {
    let refresh = |host| {
        crate::targets::image_refresh(
            host,
            "ghcr.io/example/dev:latest",
            None,
            mj_core::config::ImagePullPolicy::Auto,
        )
        .expect("a remote latest image is refreshed")
    };
    let settled = tokio_util::sync::CancellationToken::new();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let ran = |name: &'static str| move |_| Some((name.to_owned(), String::new(), None));
    let apple = tokio::spawn(refresh_host_after_startup(
        refresh(crate::targets::ImageHost::AppleContainer),
        settled.clone(),
        cancellation.clone(),
        ran("apple"),
    ));
    // A slow Apple start holds no other host's refresh.
    let docker = refresh_host_after_startup(
        refresh(crate::targets::ImageHost::LocalDocker),
        settled.clone(),
        cancellation.clone(),
        ran("docker"),
    )
    .await;
    assert_eq!(docker.map(|(host, ..)| host).as_deref(), Some("docker"));

    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(
        !apple.is_finished(),
        "the Apple refresh looked before startup finished"
    );
    settled.cancel();
    let apple = apple.await.unwrap();
    assert_eq!(apple.map(|(host, ..)| host).as_deref(), Some("apple"));
}

#[test]
fn a_result_about_one_sessions_aws_renewal_does_not_end_other_sessions_failures() {
    use mj_core::credentials::{
        CredentialSyncOutcome, CredentialSyncResult, StepFailure, SyncStep,
    };

    let outcome = |session: &str, detail: Option<&str>| CredentialSyncOutcome {
        unchanged_login: None,
        session_id: session.into(),
        outcome: Ok(Vec::new()),
        step_failures: detail
            .map(|detail| StepFailure {
                step: SyncStep::Aws,
                detail: detail.to_owned(),
            })
            .into_iter()
            .collect(),
    };
    let result = |partial, outcomes| CredentialSyncResult {
        profile_id: "work".into(),
        trigger: None,
        failure: None,
        outcomes,
        partial,
    };
    let mut notices = CredentialSyncNotices::default();
    // The first session sorts before the second, so if its failure were forgotten it
    // would be the notice's first new message.
    let both_failing = || {
        result(
            false,
            vec![
                outcome("0-first-session", Some("container is gone")),
                outcome("a-second-session", Some("worker is gone")),
            ],
        )
    };
    assert!(
        notices
            .notice(&both_failing(), None, &State::default())
            .is_some()
    );
    assert_eq!(
        notices.notice(&both_failing(), None, &State::default()),
        None
    );

    // The second session's AWS renewal succeeds. That ends its failure and says nothing
    // about the first's, which must not be announced again.
    assert_eq!(
        notices.notice(
            &result(true, vec![outcome("a-second-session", None)]),
            None,
            &State::default()
        ),
        None
    );
    let again = notices
        .notice(&both_failing(), None, &State::default())
        .unwrap();
    assert!(again.contains("a-second"), "{again}");
    assert!(!again.contains("0-first-"), "{again}");
}
