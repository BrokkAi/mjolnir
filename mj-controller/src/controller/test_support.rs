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
    CheckpointMetadata, ManagedWorktree, ManagedWorktreeTarget, SessionRecord, SessionState,
};

use crate::targets::ProcessExecutor;

use super::worktree::{
    PrimaryCheckoutRequirement, create_managed_worktree, managed_worktree_target,
};

pub(super) fn checkpoint_test_session(session_id: &str) -> SessionRecord {
    SessionRecord {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: session_id.into(),
        title: "checkpoint transition".into(),
        harness_kind: mj_core::config::HarnessKind::Codex,
        last_profile: "codex".into(),
        bundle_id: "project".into(),
        project_directory: None,
        managed_worktree: None,
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
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

pub(super) fn write_checkpoint_gate_archive(
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

/// A raw session's archive with native harness state in it, which is what a
/// conversion has to carry across unchanged.
pub(super) fn write_checkpoint_archive_with_native_state(
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
            data: b"{\"type\":\"message\"}\n".to_vec(),
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
                id: "project".into(),
                relative_destination: "project".into(),
                origin: "https://fetch.example.test/project.git".into(),
                push_urls: vec!["https://push.example.test/project.git".into()],
                remote_workspace: true,
                base_commit: "a".repeat(40),
                head_commit: "a".repeat(40),
                branch: Some(format!("mj/{session_id}")),
            },
            committed_bundle: Vec::new(),
            staged_patch: Vec::new(),
            unstaged_patch: Vec::new(),
            untracked_tar: Vec::new(),
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
    let archive_path = directory.join(format!("{session_id}.hel.zip"));
    let verified = write_archive_atomic(
        &archive_path,
        &ArchiveInput {
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
                    configuration: BTreeMap::new(),
                },
                transcript: Vec::new(),
                queued_prompts: Vec::new(),
            },
            native_artifacts,
            repositories,
        },
    )
    .unwrap();
    CheckpointMetadata {
        archive_path,
        sha256: verified.archive_sha256,
        created_at: "2026-08-14T12:00:00Z".into(),
        event_frontier,
    }
}

/// Config with one container target, one local bare target, and one SSH
/// bare target, which is every shape `resume_compatibility` distinguishes.
pub(super) fn resume_compatibility_config() -> Config {
    let mut config = Config::default();
    config.targets.insert(
        "podman".into(),
        TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "example.invalid/hel-test:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
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

pub(super) fn raw_session_on(target_template_id: &str, directory: &str) -> SessionRecord {
    let mut session = checkpoint_test_session("0123456789abcdef0123456789abcdef");
    session.state = SessionState::Stopped;
    session.bundle_id = "remote-project-abcdef".into();
    session.project_directory = Some(PathBuf::from(directory));
    session.target_template_id = target_template_id.into();
    session
}

pub(super) fn managed_raw_session(target: ManagedWorktreeTarget) -> SessionRecord {
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
    session.managed_worktree = Some(ManagedWorktree {
        source_project_directory: repository.clone(),
        source_repository: repository,
        worktree_root,
        branch: format!("mj/{session_id}"),
        target,
        base_commit: None,
    });
    session
}

pub(super) fn ssh_worktree_target() -> ManagedWorktreeTarget {
    managed_worktree_target(&resume_compatibility_config().targets["ssh-bare"]).unwrap()
}

pub(super) fn local_bundle(repository: &Path) -> ProjectBundle {
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

pub(super) fn test_git(directory: &Path, args: &[&str]) -> String {
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

pub(super) fn committed_repository() -> tempfile::TempDir {
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
pub(super) const FIXTURE_FETCH_URL: &str = "https://fetch.example.test/repo.git";

/// A committed checkout whose `origin` is a bare repository on disk behind
/// [`FIXTURE_FETCH_URL`], with `master` already pushed.
pub(super) fn checkout_with_network_remote() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let checkout = committed_repository();
    let (remote_parent, remote) = network_remote_for(checkout.path());
    (checkout, remote_parent, remote)
}

/// Give an existing checkout the pushed network remote a conversion requires.
pub(super) fn network_remote_for(checkout: &Path) -> (tempfile::TempDir, PathBuf) {
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
pub(super) struct FixtureRemoteExecutor {
    pub(super) remote: PathBuf,
}

impl crate::targets::CommandExecutor for FixtureRemoteExecutor {
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

/// A managed raw session whose worktree really exists in `repository`.
pub(super) fn managed_worktree_session(repository: &Path, session_id: &str) -> SessionRecord {
    let worktree = ManagedWorktree {
        source_project_directory: repository.to_path_buf(),
        source_repository: repository.to_path_buf(),
        worktree_root: repository.join(".mj/worktrees").join(session_id),
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
    session.managed_worktree = Some(worktree);
    session
}

/// Install a shell stand-in, under `program`, for a command the code under
/// test looks up on `PATH` in `directory`.
///
/// No test may exec a file it has just written. `execve` answers `ETXTBSY`
/// ("Text file busy") while any process still holds that file open for
/// writing, and a test binary is multi-threaded: if another thread forks
/// between the write and the exec, its child inherits the still-open write
/// descriptor and keeps the file busy past the point where the writer closed
/// it. That is the race behind issue #1036. Writing under a temporary name and
/// renaming does not fix it, because a rename keeps the same inode.
///
/// So the name on `PATH` is a symlink to a dispatcher checked in at
/// `mj-controller/tests/fixtures/fake-command.sh`, which this process never
/// opens for writing, and the behaviour goes in `<program>.script`, which only
/// `/bin/sh` ever reads. Nothing execs a written file at any point.
#[cfg(unix)]
pub(crate) fn install_fake_command(directory: &Path, program: &str, script: &str) {
    let dispatcher = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-command.sh");
    assert!(
        dispatcher.is_file(),
        "fake command dispatcher is missing at {}",
        dispatcher.display()
    );
    std::fs::write(directory.join(format!("{program}.script")), script)
        .unwrap_or_else(|error| panic!("write the {program} stand-in: {error}"));
    let installed = directory.join(program);
    // A directory may host several fakes, and a test may replace one.
    match std::fs::remove_file(&installed) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("replace the {program} stand-in: {error}"),
    }
    std::os::unix::fs::symlink(&dispatcher, &installed)
        .unwrap_or_else(|error| panic!("link the {program} stand-in: {error}"));
}
