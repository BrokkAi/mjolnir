//! Fixtures shared by the controller submodule test suites.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use mj_checkpoint::archive::{
    ArchiveInput, BundleManifest, SessionManifest, TargetManifest, write_archive_atomic,
};
use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, SshConnection, TargetTemplate,
};
use mj_core::state::{
    CheckpointMetadata, ManagedCheckoutKind, ManagedWorktree, ManagedWorktreeTarget, SessionRecord,
    SessionState,
};

#[cfg(unix)]
pub use crate::database::test_fixtures::worker_test_observation;
pub use crate::database::test_fixtures::{
    checkpoint_test_session, local_bundle, relay_admission, session_admission, test_git,
    worker_test_gate, worker_test_permit, worker_test_run_blocking, worker_test_scope,
    worker_test_scope_async,
};
use crate::targets::ProcessExecutor;
pub use crate::test_isolation::{IsolatedTest, test_name};

use super::worktree::{
    PrimaryCheckoutRequirement, create_managed_worktree, managed_worktree_target,
};

pub fn write_checkpoint_gate_archive(
    directory: &Path,
    session_id: &str,
    event_frontier: u64,
) -> CheckpointMetadata {
    write_checkpoint_archive(
        directory,
        session_id,
        event_frontier,
        Vec::new(),
        Vec::new(),
    )
}

/// The same archive for a session whose harness is not Codex. Whether a resume
/// keeps native continuity is decided by the archived harness kind, so a test
/// about a same-harness move has to be able to name it. Its only callers are
/// Unix-gated, so on Windows it would be dead code.
///
/// With `native_history`, the archive also holds a conversation file of its
/// native session where that harness keeps one, so a restore asks the harness
/// to load the archived session. Without it, the archive is that of a session
/// that never received a prompt.
#[cfg(unix)]
pub(super) fn write_checkpoint_gate_archive_for_harness(
    directory: &Path,
    session_id: &str,
    event_frontier: u64,
    harness_kind: mj_core::config::HarnessKind,
    profile_id: &str,
    native_history: bool,
) -> CheckpointMetadata {
    let native_artifacts = if native_history {
        vec![mj_checkpoint::archive::NativeArtifact {
            relative_path: Path::new(harness_kind.native_session_dirs()[0])
                .join("native-session.jsonl"),
            data: (b"{\"type\":\"message\"}\n".to_vec()).into(),
            mode: 0o600,
        }]
    } else {
        Vec::new()
    };
    let mut input =
        checkpoint_archive_input(session_id, event_frontier, Vec::new(), native_artifacts);
    input.session.harness_kind = harness_kind;
    input.session.profile_id = profile_id.to_owned();
    write_checkpoint_archive_input(directory, session_id, &input)
}

/// A raw session's archive with native harness state in it, which is what a
/// conversion has to carry across unchanged.
pub fn write_checkpoint_archive_with_native_state(
    directory: &Path,
    session_id: &str,
    event_frontier: u64,
) -> CheckpointMetadata {
    write_checkpoint_archive(
        directory,
        session_id,
        event_frontier,
        Vec::new(),
        vec![mj_checkpoint::archive::NativeArtifact {
            relative_path: PathBuf::from("sessions/native-session.jsonl"),
            data: (b"{\"type\":\"message\"}\n".to_vec()).into(),
            mode: 0o600,
        }],
    )
}

pub(super) fn write_network_checkpoint_archive(
    directory: &Path,
    session_id: &str,
    event_frontier: u64,
) -> CheckpointMetadata {
    use mj_checkpoint::archive::{RepositoryMetadata, RepositorySnapshot};
    write_checkpoint_archive(
        directory,
        session_id,
        event_frontier,
        vec![RepositorySnapshot {
            metadata: RepositoryMetadata {
                review_baseline_ref: None,
                review_refs_metadata_only: false,
                saved_refs: Default::default(),
                stash_stack: Vec::new(),
                git_operation: None,
                id: "project".into(),
                relative_destination: "project".into(),
                checkout_subdirectory: None,
                origin: "https://fetch.example.test/project.git".into(),
                push_urls: vec!["https://push.example.test/project.git".into()],
                remote_workspace: true,
                plain_directory: false,
                base_commit: "a".repeat(40),
                head_commit: "a".repeat(40),
                branch: Some(format!("mj/{session_id}")),
            },
            committed_bundle: Vec::new(),
            staged_patch: Vec::new(),
            unstaged_patch: Vec::new(),
            untracked_tar: Vec::new(),
            operation_payloads: None,
            linked_worktrees: Vec::new(),
        }],
        Vec::new(),
    )
}

fn write_checkpoint_archive(
    directory: &Path,
    session_id: &str,
    event_frontier: u64,
    repositories: Vec<mj_checkpoint::archive::RepositorySnapshot>,
    native_artifacts: Vec<mj_checkpoint::archive::NativeArtifact>,
) -> CheckpointMetadata {
    let input =
        checkpoint_archive_input(session_id, event_frontier, repositories, native_artifacts);
    write_checkpoint_archive_input(directory, session_id, &input)
}

pub fn write_checkpoint_archive_input(
    directory: &Path,
    session_id: &str,
    input: &ArchiveInput,
) -> CheckpointMetadata {
    let archive_path = directory.join(format!("{session_id}.hel.zip"));
    let verified = write_archive_atomic(&archive_path, input).unwrap();
    CheckpointMetadata {
        archive_path,
        sha256: verified.archive_sha256,
        created_at: "2026-08-14T12:00:00Z".into(),
        event_frontier: input.canonical_session.event_frontier,
    }
}

pub fn checkpoint_archive_input(
    session_id: &str,
    event_frontier: u64,
    repositories: Vec<mj_checkpoint::archive::RepositorySnapshot>,
    native_artifacts: Vec<mj_checkpoint::archive::NativeArtifact>,
) -> ArchiveInput {
    ArchiveInput {
        session: SessionManifest {
            id: session_id.into(),
            title: "checkpoint gate".into(),
            harness_kind: mj_core::config::HarnessKind::Codex,
            profile_id: "codex".into(),
            native_session_id: "native-session".into(),
            created_at: "2026-08-12T00:00:00Z".into(),
            checkpointed_at: "2026-08-14T12:00:00Z".into(),
            hel_version: "test".into(),
            relay_version: "test".into(),
            adapter_version: "test".into(),
        },
        target: TargetManifest {
            template_id: "local".into(),
            target_kind: "local-bare".into(),
            details: BTreeMap::new(),
        },
        bundle: BundleManifest {
            id: "project".into(),
            primary_repository: "project".into(),
        },
        canonical_session: mj_checkpoint::archive::CanonicalSessionSnapshot {
            command_ledger: None,
            notification_stack: None,
            assessment_state: Some(
                serde_json::to_value(mj_core::assessment::Checkpoint {
                    open_request: None,
                    version: 1,
                    context: Some(Default::default()),
                    assessment: None,
                    continuation: mj_core::continuation::ContinuationState {
                        suppressed: true,
                        ..Default::default()
                    },
                    capacity_retry: None,
                    turn_completion: None,
                })
                .unwrap(),
            ),
            event_frontier,
            event_frontier_digest: if event_frontier == 0 {
                mj_checkpoint::archive::EVENT_FRONTIER_GENESIS_DIGEST.into()
            } else {
                "a".repeat(64)
            },
            session: mj_checkpoint::archive::CanonicalSessionState {
                execution: mj_checkpoint::archive::CanonicalExecutionState::Idle,
                last_activity_at_ms: (event_frontier > 0).then_some(1_234),
                session_title: None,
                configuration: Default::default(),
            },
            transcript: Vec::new(),
            queued_prompts: Vec::new(),
        },
        native_artifacts,
        repositories,
    }
}

/// Config with one container target, one local bare target, and one SSH
/// bare target, which is every shape `resume_compatibility` distinguishes.
pub fn resume_compatibility_config() -> Config {
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "example.invalid/hel-test:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        },
    );
    config
        .targets
        .insert("local-bare".into(), TargetTemplate::LocalBare);
    config.targets.insert(
        "ssh-bare".into(),
        TargetTemplate::SshBare {
            ssh: SshConnection {
                host: "builder".into(),
                user: Some("dev".into()),
                identity_file: None,
                extra_args: Vec::new(),
            },
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        },
    );
    config
}

pub fn raw_session_on(target_template_id: &str, directory: &str) -> SessionRecord {
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.state = SessionState::Stopped;
    session.bundle_id = "remote-project-abcdef".into();
    session.project_directory = Some(PathBuf::from(directory));
    session.target_template_id = target_template_id.into();
    session
}

pub fn managed_raw_session(target: ManagedWorktreeTarget) -> SessionRecord {
    let session_id = "0123456789abcdef0123456789abcdef";
    let repository = PathBuf::from("/home/dev/project");
    let worktree_root = repository.join(".mj").join("worktrees").join(session_id);
    let mut session = raw_session_on(
        match target {
            ManagedWorktreeTarget::Local => "local-bare",
            ManagedWorktreeTarget::Ssh { .. } => "ssh-bare",
        },
        &worktree_root.to_string_lossy(),
    );
    session.managed_worktree = Some(Box::new(ManagedWorktree {
        kind: Default::default(),
        source_project_directory: repository.clone(),
        source_repository: repository,
        worktree_root,
        branch: format!("mj/{session_id}"),
        target,
        base_commit: None,
    }));
    session
}

pub fn ssh_worktree_target() -> ManagedWorktreeTarget {
    managed_worktree_target(&resume_compatibility_config().targets["ssh-bare"]).unwrap()
}

pub fn committed_repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    test_git(directory.path(), &["init", "--initial-branch=master"]);
    test_git(directory.path(), &["config", "user.name", "Hel Tests"]);
    test_git(
        directory.path(),
        &["config", "user.email", "hel@example.invalid"],
    );
    std::fs::create_dir(directory.path().join("nested")).unwrap();
    std::fs::write(directory.path().join("nested/file.txt"), "base\n").unwrap();
    test_git(directory.path(), &["add", "."]);
    test_git(directory.path(), &["commit", "-m", "base"]);
    directory
}

/// The network URL a fixture checkout records for its remote. Git reaches the
/// bare repository on disk through `insteadOf`, so the code under test sees
/// real network provenance without a network.
pub const FIXTURE_FETCH_URL: &str = "https://fetch.example.test/repo.git";

/// A committed checkout whose `origin` is a bare repository on disk behind
/// [`FIXTURE_FETCH_URL`], with `master` already pushed.
pub fn checkout_with_network_remote() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let checkout = committed_repository();
    let (remote_parent, remote) = network_remote_for(checkout.path());
    (checkout, remote_parent, remote)
}

/// Give an existing checkout the pushed network remote a conversion requires.
pub fn network_remote_for(checkout: &Path) -> (tempfile::TempDir, PathBuf) {
    let remote_parent = tempfile::tempdir().unwrap();
    let remote = remote_parent.path().join("remote.git");
    let output = Command::new("git")
        .args(["init", "--bare", "--initial-branch=master"])
        .arg(&remote)
        .output()
        .unwrap();
    assert!(output.status.success());
    test_git(
        checkout,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    test_git(checkout, &["push", "--set-upstream", "origin", "master"]);
    // `git remote get-url` applies `insteadOf` rewrites, so the rewrite cannot
    // live in this repository's configuration: the checkout records the network
    // URL, and only the commands that really reach a remote are rewritten, by
    // `FixtureRemoteExecutor`.
    test_git(
        checkout,
        &["remote", "set-url", "origin", FIXTURE_FETCH_URL],
    );
    (remote_parent, remote)
}

/// Rewrites the fixture's network URL for the steps that really contact a
/// remote, the way `FixtureTransport` does in `network_git`.
pub struct FixtureRemoteExecutor {
    pub(super) remote: PathBuf,
}

impl crate::targets::CommandExecutor for FixtureRemoteExecutor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(
        &self,
        command: &crate::targets::CommandSpec,
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        let mut command = command.clone();
        assert_eq!(command.program, "git", "the fixture executes only Git");
        // Rewrite only the commands that contact the remote. Rewriting
        // `remote get-url` would hide the network URL the checkout records.
        if command
            .args
            .iter()
            .any(|argument| argument == "ls-remote" || argument == "fetch")
        {
            let mut args = vec![
                "-c".to_owned(),
                format!(
                    "url.{}.insteadOf={FIXTURE_FETCH_URL}",
                    self.remote.display()
                ),
            ];
            args.extend(command.args);
            command.args = args;
        }
        ProcessExecutor.execute(&command)
    }
}

/// Git 2.31 does not support NUL-delimited worktree lists.
pub(super) struct Git231Executor;

impl Git231Executor {
    fn refusal(command: &crate::targets::CommandSpec) -> Option<crate::targets::CommandOutput> {
        let has = |option: &str| command.args.iter().any(|argument| argument == option);
        if !has("worktree") || !has("-z") {
            return None;
        }
        Some(crate::targets::CommandOutput {
            status: 129,
            stdout: Vec::new(),
            stderr: b"error: unknown switch `z'".to_vec(),
        })
    }
}

impl crate::targets::CommandExecutor for Git231Executor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(
        &self,
        command: &crate::targets::CommandSpec,
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        match Self::refusal(command) {
            Some(refused) => Ok(refused),
            None => ProcessExecutor.execute(command),
        }
    }

    fn execute_with_stdin(
        &self,
        command: &crate::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        match Self::refusal(command) {
            Some(refused) => Ok(refused),
            None => ProcessExecutor.execute_with_stdin(command, input),
        }
    }
}

/// A managed raw session whose worktree really exists in `repository`.
pub fn managed_worktree_session(repository: &Path, session_id: &str) -> SessionRecord {
    managed_checkout_session(repository, session_id, ManagedCheckoutKind::Worktree)
}

/// A managed raw session on an isolated clone of `repository`, created the
/// way a session's clone is, so its `origin` is the one `repository` has.
#[cfg(unix)]
pub fn managed_clone_session(repository: &Path, session_id: &str) -> SessionRecord {
    managed_checkout_session(repository, session_id, ManagedCheckoutKind::Clone)
}

fn managed_checkout_session(
    repository: &Path,
    session_id: &str,
    kind: ManagedCheckoutKind,
) -> SessionRecord {
    let directory = match kind {
        ManagedCheckoutKind::Worktree => ".mj/worktrees",
        ManagedCheckoutKind::Clone => ".mj/clones",
    };
    let worktree = ManagedWorktree {
        kind,
        source_project_directory: repository.to_path_buf(),
        source_repository: repository.to_path_buf(),
        worktree_root: repository.join(directory).join(session_id),
        branch: format!("mj/{session_id}"),
        target: ManagedWorktreeTarget::Local,
        // Production records the owning repository's head when the worktree is
        // created; tests need the same base.
        base_commit: Some(test_git(repository, &["rev-parse", "HEAD"])),
    };
    create_managed_worktree(
        &ProcessExecutor,
        &worktree,
        None,
        PrimaryCheckoutRequirement::Clean,
    )
    .unwrap();
    let mut session = checkpoint_test_session(session_id);
    session.state = SessionState::Stopped;
    session.bundle_id = "remote-project-abcdef".into();
    session.target_template_id = "local-bare".into();
    session.project_directory = Some(worktree.worktree_root.clone());
    session.managed_worktree = Some(Box::new(worktree));
    session
}

/// An executor that fails the test if anything runs a command through it.
///
/// `reason` names the step that was supposed to avoid running anything, so a
/// failure says which expectation broke as well as which command ran.
pub struct RefusingExecutor(pub &'static str);

impl crate::targets::CommandExecutor for RefusingExecutor {
    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        mj_core::targets::DeadlinePause::none()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.cancellation_requested()
    }

    fn execute_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute(command)
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &mj_core::targets::CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> anyhow::Result<mj_core::targets::CommandOutput> {
        self.execute_with_stdin(command, input)
    }

    fn execute(
        &self,
        command: &crate::targets::CommandSpec,
    ) -> anyhow::Result<crate::targets::CommandOutput> {
        panic!(
            "{} unexpectedly ran {}: {command:?}",
            self.0, command.program
        );
    }
}
