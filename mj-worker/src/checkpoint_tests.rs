use crate::test_support::git;
use mj_checkpoint::archive::*;
use mj_checkpoint::checkpoint::*;
use mj_core::config::HarnessKind;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";
const NATIVE: &str = "0190aabb-ccdd-7eef-9000-abcdef012345";
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires pinned MJ_MUSE_ACP_TEST_BINARY, MJ_MUSE_TEST_BINARY and authenticated MJ_MUSE_TEST_HOME; uses two provider turns"]
async fn live_muse_checkpoint_restore_relocates_native_context_and_preserves_queued_work() {
    let required_path = |name| PathBuf::from(std::env::var_os(name).expect(name));
    let adapter = required_path("MJ_MUSE_ACP_TEST_BINARY");
    let muse = required_path("MJ_MUSE_TEST_BINARY");
    let account = required_path("MJ_MUSE_TEST_HOME");
    let temp = tempfile::tempdir().unwrap();
    let (mut spec, _) = fixture(temp.path());
    spec.session.harness_kind = HarnessKind::Muse;
    spec.session.profile_id = "muse-smoke".into();
    spec.harness_home = temp.path().join("original/muse");
    fs::create_dir_all(&spec.harness_home).unwrap();
    fs::copy(
        account.join("auth.json"),
        spec.harness_home.join("auth.json"),
    )
    .unwrap();
    let cwd = spec.workspace_root.join("app");
    let token = format!(
        "MUSE_{}",
        temp.path().file_name().unwrap().to_string_lossy()
    );
    let prompt = format!(
        "Remember this exact token for the next turn: {token}. Reply with only that token. Do not use tools."
    );
    let (session_id, reply) = crate::acp::muse_tests::native_muse_turn(
        &adapter,
        &muse,
        &spec.harness_home,
        &cwd,
        None,
        &prompt,
    )
    .await;
    assert!(
        reply.contains(&token),
        "provider did not acknowledge the smoke token"
    );
    spec.session.native_session_id = session_id.clone();
    spec.canonical_session.transcript[0].body = CanonicalTranscriptBody::User {
        content: vec![json!({"type":"text", "text":prompt})],
    };
    export_checkpoint(&spec).unwrap();
    let archive = read_archive_verified(&spec.output_path).unwrap();
    assert!(
        archive
            .manifest
            .payloads
            .iter()
            .all(|payload| !payload.path.ends_with("auth.json"))
    );
    let restored_home = temp.path().join("restored/muse");
    let restored_relay = temp.path().join("restored-relay");
    let restored_workspace = temp.path().join("restored-workspace");
    fs::create_dir_all(&restored_workspace).unwrap();
    git(
        &restored_workspace,
        &["clone", "--quiet", cwd.to_str().unwrap(), "app"],
    );
    restore_checkpoint(
        &CheckpointRestoreSpec {
            report_root: None,
            archive_path: spec.output_path.clone(),
            workspace_root: restored_workspace.clone(),
            relay_root: restored_relay.clone(),
            harness_home: restored_home.clone(),
            restore_repositories: true,
            restore_native: true,
            queue_policy: mj_checkpoint::checkpoint::QueueRestorePolicy::Restore,
            primary_repository_root: None,
        },
        &SystemGit,
    )
    .unwrap();
    assert!(
        !restored_home.join("auth.json").exists(),
        "credentials entered the archive"
    );
    assert_eq!(
        restored_seed(&restored_relay).queued_prompts,
        spec.canonical_session.queued_prompts
    );
    fs::copy(
        spec.harness_home.join("auth.json"),
        restored_home.join("auth.json"),
    )
    .unwrap();
    let (resumed_id, reply) = crate::acp::muse_tests::native_muse_turn(
            &adapter, &muse, &restored_home, &restored_workspace.join("app"), Some(session_id.clone()),
            "What exact token did I ask you to remember in the previous turn? Reply only with the token. Do not use tools.",
        ).await;
    assert_eq!(resumed_id, session_id);
    assert!(reply.contains(&token), "native context was not restored");
}
pub(crate) fn fixture(temp: &Path) -> (CheckpointExportSpec, PathBuf) {
    let worker_root = temp.join("worker");
    fs::create_dir_all(&worker_root).unwrap();
    let harness_home = temp.join("codex");
    let native = harness_home.join("sessions/2026/08/09");
    fs::create_dir_all(&native).unwrap();
    fs::write(native.join(format!("rollout-{NATIVE}.jsonl")), b"native").unwrap();
    let workspace = temp.join("workspace");
    let repository = workspace.join("app");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init"]);
    git(&repository, &["config", "user.email", "hel@example.test"]);
    git(&repository, &["config", "user.name", "Hel Test"]);
    fs::write(repository.join("README.md"), b"hello").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "base"]);
    git(
        &repository,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/app.git",
        ],
    );
    let base = git(&repository, &["rev-parse", "HEAD"]);
    let output = worker_root.join("source.hel.zip");
    (
        CheckpointExportSpec {
            consent: Default::default(),
            report_directories: None,
            protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
            session: SessionManifest {
                id: SESSION.into(),
                title: "test".into(),
                harness_kind: HarnessKind::Codex,
                profile_id: "codex-1".into(),
                native_session_id: NATIVE.into(),
                created_at: "2026-08-09T00:00:00Z".into(),
                checkpointed_at: "2026-08-09T00:01:00Z".into(),
                hel_version: "0.1.0".into(),
                relay_version: "0.1.0".into(),
                adapter_version: "test".into(),
            },
            target: TargetManifest {
                template_id: "local".into(),
                target_kind: "podman".into(),
                details: Default::default(),
            },
            bundle: BundleManifest {
                id: "bundle".into(),
                primary_repository: "app".into(),
            },
            relay_root: worker_root,
            harness_home,
            workspace_root: workspace,
            repositories: vec![CheckpointRepositorySpec {
                id: "app".into(),
                relative_destination: "app".into(),
                capture: CheckpointRepositoryCapture::DeltaFrom { base_commit: base },
                origin_override: None,
            }],
            canonical_session: CanonicalSessionSnapshot {
                pending_prompt_context: None,
                command_ledger: None,
                assessment_state: None,
                notification_stack: None,
                native_use_evidence: None,
                event_frontier: 1,
                event_frontier_digest: "a".repeat(64),
                session: CanonicalSessionState {
                    execution: CanonicalExecutionState::Idle,
                    last_activity_at_ms: Some(1),
                    session_title: Some("test".into()),
                    configuration: Default::default(),
                },
                transcript: vec![CanonicalTranscriptItem {
                    stable_id: "user-1".into(),
                    position: 1,
                    latest_content_event_ordinal: None,
                    created_at_ms: 1,
                    last_changed_at_ms: 1,
                    body: CanonicalTranscriptBody::User {
                        content: vec![json!({"type": "text", "text": "hello"})],
                    },
                }],
                queued_prompts: vec![CanonicalQueuedPrompt {
                    command_id: "queued-1".into(),
                    kind: CanonicalQueuedCommandKind::Prompt,
                    content: vec![json!({"type": "text", "text": "next"})],
                    queued_at_ms: 2,
                }],
                mailbox: None,
            },
            output_path: output.clone(),
        },
        output,
    )
}
fn restored_seed(relay_root: &Path) -> mj_core::relay::RestoredRelaySeed {
    serde_json::from_slice(&fs::read(mj_core::relay::restored_relay_seed_path(relay_root)).unwrap())
        .unwrap()
}

// Hard-won: 055d8a772 review: recovery of one component must preserve newer parent state and current files.
#[test]
fn checkout_recovery_publishes_only_the_staged_checkout_and_preserves_a_new_current_directory() {
    let temp = tempfile::tempdir().unwrap();
    let (mut source, archive_path) = fixture(temp.path());
    git(
        &source.workspace_root,
        &["clone", "--quiet", "app", "second"],
    );
    source.repositories.push(CheckpointRepositorySpec {
        id: "second".into(),
        relative_destination: "second".into(),
        capture: CheckpointRepositoryCapture::DeltaFrom {
            base_commit: git(
                &source.workspace_root.join("second"),
                &["rev-parse", "HEAD"],
            )
            .trim()
            .into(),
        },
        origin_override: None,
    });
    fs::write(
        source.workspace_root.join("second/README.md"),
        b"archived second repository work",
    )
    .unwrap();
    export_checkpoint(&source).unwrap();
    let stage = temp.path().join("checkout-stage");
    fs::create_dir(&stage).unwrap();
    git(
        &stage,
        &[
            "clone",
            "--quiet",
            source.workspace_root.join("app").to_str().unwrap(),
            "app",
        ],
    );
    git(
        &stage,
        &[
            "clone",
            "--quiet",
            source.workspace_root.join("second").to_str().unwrap(),
            "second",
        ],
    );
    let parent_native = temp.path().join("current-native");
    fs::create_dir(&parent_native).unwrap();
    fs::write(parent_native.join("sentinel"), b"newer native state").unwrap();
    let parent_relay = temp.path().join("current-relay");
    fs::create_dir(&parent_relay).unwrap();
    fs::write(
        parent_relay.join(mj_core::relay::RELAY_STATE_FILE),
        b"current relay",
    )
    .unwrap();
    let destination = temp.path().join("checkout-current");
    let spec = CheckoutRecoverySpec {
        version: 1,
        checkpoint_sha256: verify_archive_streaming(&archive_path)
            .unwrap()
            .archive_sha256,
        staging_root: stage.clone(),
        staged_checkout: stage.join("app"),
        destination_checkout: destination.join("app"),
        additional_checkouts: vec![CheckoutRecoveryPublication {
            staged_checkout: stage.join("second"),
            destination_checkout: destination.join("second"),
        }],
        restore: CheckpointRestoreSpec {
            archive_path,
            workspace_root: stage.clone(),
            report_root: None,
            relay_root: parent_relay.clone(),
            harness_home: parent_native.clone(),
            restore_repositories: true,
            restore_native: false,
            queue_policy: QueueRestorePolicy::Restore,
            primary_repository_root: None,
        },
    };
    let spec_path = temp.path().join("checkout-recovery.json");
    fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();
    crate::checkpoint::restore_checkout_from_spec_file(&spec_path).unwrap();
    assert!(destination.join("app/.git").is_dir());
    assert!(!stage.join("app").exists());
    assert!(!stage.join("second").exists());
    assert_eq!(
        fs::read(destination.join("second/README.md")).unwrap(),
        b"archived second repository work"
    );
    fs::write(destination.join("app/current-work"), b"new current files").unwrap();
    // An interrupted caller can retry after publication; it must not replay
    // the archive over the now-current checkout, even without its old staging.
    crate::checkpoint::restore_checkout_from_spec_file(&spec_path).unwrap();
    assert_eq!(
        fs::read(destination.join("app/current-work")).unwrap(),
        b"new current files"
    );
    assert_eq!(
        fs::read(parent_native.join("sentinel")).unwrap(),
        b"newer native state"
    );
    assert_eq!(
        fs::read(parent_relay.join(mj_core::relay::RELAY_STATE_FILE)).unwrap(),
        b"current relay"
    );
}

fn parent_mail(key: &str, wake: bool) -> mj_core::mailbox::MailboxEvent {
    mj_core::mailbox::MailboxEvent {
        key: key.into(),
        source: "parent".into(),
        wake,
        created_at_ms: 1,
        body: mj_core::mailbox::MailboxEventBody::ParentMessage {
            text: format!("message {key}"),
        },
    }
}

/// Mail the worker accepted before a checkpoint cut, but had not handed to the
/// agent, is archived from the relay at that cut and arrives exactly once from
/// whichever relay restores it, in its order. Queue policies that defer mail
/// leave it for the controller's admission, while a queued prompt's claimed
/// mail is bound to its command ID before that prompt can start.
#[test]
fn accepted_mail_crosses_a_checkpoint_into_the_restored_relay_exactly_once() {
    use crate::relay::DurableRelay;
    use crate::relay::test_support::{ready_checkpoint, relay_request, submit_relay};
    use mj_core::relay::{
        RelayCommand, RelayCommandOutcome, RelayErrorCode, RelayObservation, RelayRequest,
        RelayResponseBody, RelayResponsePayload,
    };
    let deliver = |event| RelayCommand::DeliverMailboxEvent { event };
    let configured = |root: &Path| {
        let mut relay = DurableRelay::open(root, SESSION, "1.0.0").unwrap();
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: Vec::new(),
            })
            .unwrap();
        relay
    };
    let temp = tempfile::tempdir().unwrap();

    // The source delivered one waking message and holds two that wait for the
    // agent's next prompt.
    let mut source = configured(&temp.path().join("source-relay"));
    submit_relay(
        &mut source,
        "deliver-delivered",
        deliver(parent_mail("delivered", true)),
    );
    let wake = source.claim_pending_commands(true).unwrap();
    assert!(matches!(wake[0].command, RelayCommand::MailboxWake { .. }));
    source
        .record_command_completed(
            &wake[0].command_id,
            RelayCommandOutcome::Prompt {
                stop_reason: "end_turn".into(),
                usage: None,
                work_facts: None,
                diagnostic: None,
            },
        )
        .unwrap();
    submit_relay(
        &mut source,
        "deliver-first",
        deliver(parent_mail("first", false)),
    );
    submit_relay(
        &mut source,
        "deliver-second",
        deliver(parent_mail("second", false)),
    );
    let cut = ready_checkpoint(&mut source, "suspend-barrier");
    let read_at_cut = |relay: &mut DurableRelay| {
        relay
            .handle(relay_request(
                "read-mail",
                RelayRequest::ReadCheckpointMailbox {
                    barrier_command_id: "suspend-barrier".into(),
                    cut: cut.clone(),
                },
            ))
            .body
    };
    let RelayResponseBody::Ok {
        payload: RelayResponsePayload::CheckpointMailbox { mailbox },
    } = read_at_cut(&mut source)
    else {
        panic!("the relay refused to report its mail at the ready cut");
    };
    let pending = vec![parent_mail("first", false), parent_mail("second", false)];
    assert_eq!(mailbox.pending_events, pending);
    assert!(mailbox.delivered_event_keys.contains_key("delivered"));
    // Mail accepted after the cut moves the relay past it, as it would refuse
    // a Close there; the cut's mail can no longer be read.
    submit_relay(
        &mut source,
        "deliver-late",
        deliver(parent_mail("late", false)),
    );
    let RelayResponseBody::Error { error } = read_at_cut(&mut source) else {
        panic!("the relay reported mail for a cut it has moved past");
    };
    assert_eq!(error.code, RelayErrorCode::InvalidState);

    // The fixture's queued prompt carries claimed mail, as a recovery prompt
    // does after a worker restart.
    let (mut spec, _) = fixture(temp.path());
    let claimed = "claimed mail for the queued prompt";
    spec.canonical_session.mailbox = Some(CanonicalMailbox {
        prompt_contexts: [("queued-1".to_owned(), claimed.to_owned())].into(),
        ..mailbox
    });
    export_checkpoint(&spec).unwrap();
    assert_eq!(
        read_archive_verified(&spec.output_path)
            .unwrap()
            .manifest
            .schema_version,
        ARCHIVE_SCHEMA_VERSION_MAILBOX
    );
    let restore = |name: &str, queue_policy| {
        let workspace = temp.path().join(format!("{name}-workspace"));
        fs::create_dir_all(&workspace).unwrap();
        git(
            &workspace,
            &[
                "clone",
                "--quiet",
                spec.workspace_root.join("app").to_str().unwrap(),
                "app",
            ],
        );
        let relay_root = temp.path().join(format!("{name}-relay"));
        restore_checkpoint(
            &CheckpointRestoreSpec {
                report_root: None,
                archive_path: spec.output_path.clone(),
                workspace_root: workspace,
                relay_root: relay_root.clone(),
                harness_home: temp.path().join(format!("{name}-home")),
                restore_repositories: false,
                restore_native: true,
                queue_policy,
                primary_repository_root: None,
            },
            &SystemGit,
        )
        .unwrap();
        configured(&relay_root)
    };
    let pending_keys =
        |relay: &DurableRelay| relay.operational_state().undelivered_mailbox_event_keys;
    let next_prompt = || RelayCommand::Prompt {
        prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
            "next",
        )],
    };

    // The restored worker owns the mail at once. The queued prompt starts with
    // its claimed mail and leaves the waiting messages for the next one. A
    // producer's retry of a message under any command ID is acknowledged
    // without a second copy, whether it is pending or was delivered.
    let mut restored = restore("restored", QueueRestorePolicy::Restore);
    assert_eq!(pending_keys(&restored), ["first", "second"]);
    let started = restored.claim_pending_commands(true).unwrap();
    assert_eq!(started[0].command_id, "queued-1");
    assert!(
        started[0]
            .hidden_prompt_context
            .as_deref()
            .unwrap()
            .contains(claimed)
    );
    submit_relay(
        &mut restored,
        "retry-first",
        deliver(parent_mail("first", false)),
    );
    submit_relay(
        &mut restored,
        "retry-delivered",
        deliver(parent_mail("delivered", true)),
    );
    assert_eq!(pending_keys(&restored), ["first", "second"]);

    // Deferred: nothing pending until the controller admits it. The prompt it
    // admits under its original ID finds its claimed mail before it starts.
    let mut deferred = restore("deferred", QueueRestorePolicy::Defer);
    assert!(pending_keys(&deferred).is_empty());
    assert!(deferred.claim_pending_commands(true).unwrap().is_empty());
    submit_relay(&mut deferred, "queued-1", next_prompt());
    let started = deferred.claim_pending_commands(true).unwrap();
    assert!(
        started[0]
            .hidden_prompt_context
            .as_deref()
            .unwrap()
            .contains(claimed)
    );
    for event in &pending {
        submit_relay(
            &mut deferred,
            &format!("admit-{}", event.key),
            deliver(event.clone()),
        );
        submit_relay(
            &mut deferred,
            &format!("again-{}", event.key),
            deliver(event.clone()),
        );
    }
    assert_eq!(pending_keys(&deferred), ["first", "second"]);

    // A discarded queue takes its claimed mail with it; pending mail still
    // waits for the controller, and delivered keys still deduplicate.
    let mut discarded = restore("discarded", QueueRestorePolicy::DiscardDeferringMail);
    assert!(pending_keys(&discarded).is_empty());
    submit_relay(
        &mut discarded,
        "retry-delivered",
        deliver(parent_mail("delivered", true)),
    );
    assert!(pending_keys(&discarded).is_empty());
    submit_relay(&mut discarded, "queued-1", next_prompt());
    let started = discarded.claim_pending_commands(true).unwrap();
    assert_eq!(started[0].command_id, "queued-1");
    assert!(started[0].hidden_prompt_context.is_none());
}

#[test]
fn worker_suspend_preflight_reports_untracked_keep_dir_and_dry_run() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    fs::write(repo.join("tracked.txt"), "base\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "initial"]);
    let head = git(&["rev-parse", "HEAD"]);

    fs::write(repo.join(".git/info/exclude"), "/.mj/keep/\n").unwrap();
    fs::write(repo.join("scratch.txt"), "untracked\n").unwrap();
    let keep_dir = repo.join(".mj/keep/s1");
    fs::create_dir_all(&keep_dir).unwrap();
    fs::write(keep_dir.join("evidence.txt"), "keep\n").unwrap();

    let spec = mj_core::suspend_consent::WorkerSuspendPreflightSpec {
        session_id: "s1".to_owned(),
        repositories: vec![mj_core::suspend_consent::WorkerPreflightRepo {
            id: "workspace".to_owned(),
            path: repo.clone(),
            raw: false,
            exclude_commits: Some(vec![head]),
            keep_dir: Some(keep_dir),
            origin_override: None,
        }],
        dry_run: true,
    };
    let result =
        crate::checkpoint::run_suspend_preflight(&spec, &mj_checkpoint::archive::SystemGit)
            .unwrap();
    assert!(
        result
            .untracked
            .iter()
            .any(|u| u.path == std::path::Path::new("scratch.txt")),
        "expected scratch.txt in untracked: {:?}",
        result.untracked
    );
    assert_eq!(result.keep_dir_entries.len(), 1);
    assert_eq!(
        result.keep_dir_entries[0].path,
        PathBuf::from("evidence.txt")
    );
    assert!(result.keep_dir_bytes > 0);
    let dry_run = result.dry_run.expect("dry_run estimate");
    assert_eq!(dry_run.session_state_bytes, None);
    assert!(dry_run.git_delta_bytes.is_some());
}
