//! Fixtures shared by the controller submodule test suites.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use mj_checkpoint::archive::{
    ArchiveInput, BundleManifest, SessionManifest, TargetManifest, write_archive_atomic,
};
use mj_core::config::{
    Config, ContainerTemplate as ConfigContainer, ProjectBundle, ProjectRepository, SshConnection,
    TargetTemplate,
};
use mj_core::state::{
    CheckpointMetadata, ManagedCheckoutKind, ManagedWorktree, ManagedWorktreeTarget, SessionRecord,
    SessionState,
};

use crate::targets::ProcessExecutor;

use super::worktree::{
    PrimaryCheckoutRequirement, create_managed_worktree, managed_worktree_target,
};

pub fn checkpoint_test_session(session_id: &str) -> SessionRecord {
    SessionRecord {
        oracle_choice: None,
        oracle: true,
        notifications_ignored: false,
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
        id: session_id.into(),
        title: "checkpoint transition".into(),
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
        state: SessionState::Running,
        target: None,
        native_session_id: Some("native-session".into()),
        acp_session_title: None,
        session_title_override: None,
        created_at: "2026-08-12T00:00:00Z".into(),
        updated_at: "2026-08-12T00:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        dismissed_failure: None,
        failure_generation: 0,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

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
/// about a same-harness move has to be able to name it. Its only caller is the
/// Unix-gated in-place move fixture, so on Windows it would be dead code.
#[cfg(unix)]
pub(super) fn write_checkpoint_gate_archive_for_harness(
    directory: &Path,
    session_id: &str,
    event_frontier: u64,
    harness_kind: mj_core::config::HarnessKind,
    profile_id: &str,
) -> CheckpointMetadata {
    let mut input = checkpoint_archive_input(session_id, event_frontier, Vec::new(), Vec::new());
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

pub fn local_bundle(repository: &Path) -> ProjectBundle {
    ProjectBundle {
        primary_repo: "project".into(),
        repositories: vec![ProjectRepository {
            id: "project".into(),
            github: None,
            local: Some(repository.to_path_buf()),
            destination: PathBuf::from("project"),
            git_ref: None,
        }],
    }
}

pub fn test_git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
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

/// The `--exact` name of a test in this binary, given its `module_path!()`.
///
/// `module_path!()` carries the crate name, which libtest's filter does not.
/// mj-daemon's tests use this too, so any crate name is stripped.
pub fn test_name(module_path: &str, test: &str) -> String {
    let module = module_path
        .split_once("::")
        .map_or(module_path, |(_, module)| module);
    format!("{module}::{test}")
}

/// Re-run one test alone, in a child of this test binary.
///
/// The data directory, the installed database writer, the tracing subscriber
/// and the process working directory are all process-global, so a test that
/// needs its own has to be the only test in its process. The child runs the
/// named test with `--exact`, and [`IsolatedTest::run`] fails the parent with
/// the child's own output when it does not pass.
///
/// Build the name with [`test_name`], which strips the crate prefix that
/// `module_path!()` carries and libtest's filter does not accept.
pub struct IsolatedTest {
    name: String,
    command: Command,
}

impl IsolatedTest {
    /// A child runs one whole flow on one test thread. An in-place Move, for
    /// example, polls the checkpoint and provisioning chain inline, whose
    /// debug-build frames are tens of kilobytes each; at the 8 MiB default
    /// that sits within a few pages of the limit, so an unrelated change can
    /// overflow a child that passed before. Give the child room instead of
    /// leaving every isolated test to the edge of its stack.
    const CHILD_STACK_BYTES: &'static str = "33554432";

    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let mut command = Command::new(std::env::current_exe().expect("this test binary"));
        command
            .args(["--exact", &name, "--nocapture"])
            .env("RUST_MIN_STACK", Self::CHILD_STACK_BYTES)
            // The instance name selects the default viewer port and the
            // store layout, so a child owns it: an inherited one changes
            // output the child's goldens pin. A test that needs an instance
            // sets it with `env`.
            .env_remove("MJ_INSTANCE");
        // Control commands run a staged current worker, so a child that
        // exercises them needs a worker source. A test names its own with
        // `env`, which replaces this one.
        #[cfg(unix)]
        command.env(
            "MJ_WORKER_BINARY",
            mj_core::test_hooks::fake_worker_dispatcher(),
        );
        Self { name, command }
    }

    /// Set a variable for the child. The marker a test uses to tell the child
    /// apart from the parent goes here too.
    pub fn env(
        mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> Self {
        self.command.env(key, value);
        self
    }

    /// Remove a variable from the child, such as the default worker source.
    pub(crate) fn env_remove(mut self, key: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env_remove(key);
        self
    }

    /// Include an ignored integration case in the isolated child process.
    #[cfg(unix)]
    pub fn include_ignored(mut self) -> Self {
        self.command.arg("--ignored");
        self
    }

    /// Give the child its own configuration and data directories under `root`.
    pub fn isolated_store(self, root: &Path) -> Self {
        self.env("MJ_DATA_DIR", root.join("data"))
            .env("MJ_CONFIG_DIR", root.join("config"))
    }

    /// Run the child and return its output without judging it.
    pub fn output(mut self) -> std::process::Output {
        self.command
            .output()
            .unwrap_or_else(|error| panic!("run isolated {}: {error}", self.name))
    }

    /// Run the child and fail this test with its output if it did not pass.
    pub fn run(self) -> std::process::Output {
        let name = self.name.clone();
        let output = self.output();
        assert!(
            output.status.success(),
            "isolated {name} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

/// Explicit owner capability for controller fixtures that run without a daemon.
pub fn session_admission(session_id: &str) -> crate::recovery_gate::SessionAdmission {
    let gate = std::sync::Arc::new(crate::recovery_gate::RecoveryGate::default());
    crate::recovery_gate::SessionAdmission::Foreground(
        futures::executor::block_on(gate.enter_foreground(
            session_id,
            "test lifecycle",
            crate::recovery_gate::LifecycleOrigin::Person,
            &std::sync::atomic::AtomicBool::new(false),
            &crate::targets::ProcessExecutor,
        ))
        .unwrap(),
    )
}
pub(crate) fn relay_admission(session_id: &str) -> crate::recovery_gate::RelayLeaseAdmission {
    futures::executor::block_on(session_admission(session_id).lease(session_id)).unwrap()
}

/// Shared owner for fixtures exercising process mutation without a daemon.
pub(crate) fn worker_test_gate() -> std::sync::Arc<crate::recovery_gate::RecoveryGate> {
    use std::sync::{Arc, Mutex, OnceLock};
    static GATES: OnceLock<
        Mutex<
            std::collections::BTreeMap<std::path::PathBuf, Arc<crate::recovery_gate::RecoveryGate>>,
        >,
    > = OnceLock::new();
    GATES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .entry(mj_core::config::data_dir())
        .or_default()
        .clone()
}
pub fn worker_test_permit(
    id: &str,
    _reason: &'static str,
) -> anyhow::Result<Option<crate::worker_lifecycle::WorkerPermit>> {
    if let Some(owner) = crate::worker_lifecycle::current(id) {
        return Ok(Some(owner));
    }
    let Ok(admission) =
        worker_test_gate().try_start(id, crate::recovery_gate::BackgroundWork::WorkerReplacement)
    else {
        return Ok(None);
    };
    crate::worker_lifecycle::WorkerPermit::acquire(id, &admission).map(Some)
}
pub(crate) fn worker_test_observation(
    id: &str,
) -> anyhow::Result<Option<crate::worker_lifecycle::WorkerPermit>> {
    let Ok(admission) =
        worker_test_gate().try_start(id, crate::recovery_gate::BackgroundWork::WorkerObservation)
    else {
        return Ok(None);
    };
    crate::worker_lifecycle::WorkerPermit::acquire(id, &admission).map(Some)
}

/// Enter a fixture's operation exactly as the daemon registry does.
pub fn worker_test_scope<T>(id: &str, work: impl FnOnce() -> T) -> T {
    if let Some(owner) = crate::worker_lifecycle::current(id) {
        return owner.scope_blocking(work);
    }
    let gate = worker_test_gate();
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let enter = gate.enter_foreground(
        id,
        "test lifecycle",
        crate::recovery_gate::LifecycleOrigin::Person,
        &cancelled,
        &crate::targets::ProcessExecutor,
    );
    let admission = if tokio::runtime::Handle::try_current().is_ok() {
        futures::executor::block_on(enter)
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(enter)
    }
    .unwrap();
    crate::worker_lifecycle::WorkerPermit::acquire(id, &admission)
        .unwrap()
        .scope_blocking(work)
}
pub async fn worker_test_scope_async<T>(id: &str, work: impl std::future::Future<Output = T>) -> T {
    if let Some(owner) = crate::worker_lifecycle::current(id) {
        return owner.scope(work).await;
    }
    let admission = worker_test_gate()
        .enter_foreground(
            id,
            "test lifecycle",
            crate::recovery_gate::LifecycleOrigin::Person,
            &std::sync::atomic::AtomicBool::new(false),
            &crate::targets::ProcessExecutor,
        )
        .await
        .unwrap();
    crate::worker_lifecycle::WorkerPermit::acquire(id, &admission)
        .unwrap()
        .scope(work)
        .await
}
pub(crate) fn worker_test_run_blocking<T>(
    id: &str,
    reason: &'static str,
    executor: &impl crate::targets::CommandExecutor,
    work: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    worker_test_scope(id, || {
        crate::worker_lifecycle::run_blocking(id, reason, executor, work)
    })
}
