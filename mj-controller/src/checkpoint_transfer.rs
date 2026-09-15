//! Controller-side checkpoint transport and verified teardown gates.
use crate::targets::{
    CommandExecutor, CommandPlan, CommandSpec, SshTarget, TargetLocator, join_remote_command,
    worker_root,
};
use anyhow::{Context, Result, bail, ensure};
use mj_checkpoint::archive::validate_component;
use mj_checkpoint::checkpoint::*;
use std::fs;
use std::path::{Path, PathBuf};
/// Export by streaming the spec to the worker's standard input.
///
/// Every wrapper this builds keeps the target's stdin attached: the container
/// engines are invoked with `exec -i` and `ssh` forwards stdin by default.
pub fn export_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    export_command(locator, session_id, EXPORT_SPEC_STDIN)
}

pub fn capture_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "capture-checkpoint",
        "capture target checkpoint",
    )
}

pub fn pack_stdin_command(locator: &TargetLocator, session_id: &str) -> Result<CommandSpec> {
    checkpoint_stdin_command(
        locator,
        session_id,
        "pack-checkpoint",
        "pack target checkpoint",
    )
}

fn checkpoint_stdin_command(
    locator: &TargetLocator,
    session_id: &str,
    subcommand: &str,
    purpose: &str,
) -> Result<CommandSpec> {
    let root = worker_root(locator, session_id)?;
    let args = vec![format!("{root}/hel"), "worker".into(), subcommand.into()];
    crate::targets::command_on_locator(locator, session_id, args, purpose)
}

pub fn export_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_remote_path(spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "export-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    crate::targets::command_on_locator(locator, session_id, args, "export target checkpoint")
}

pub fn restore_command(
    locator: &TargetLocator,
    session_id: &str,
    spec_path: &str,
) -> Result<CommandSpec> {
    validate_remote_path(spec_path)?;
    let root = worker_root(locator, session_id)?;
    let args = vec![
        format!("{root}/hel"),
        "worker".into(),
        "restore-checkpoint".into(),
        "--spec".into(),
        spec_path.into(),
    ];
    crate::targets::command_on_locator(locator, session_id, args, "restore target checkpoint")
}

#[derive(Debug, Clone)]
pub struct CheckpointTransfer<'a> {
    pub locator: &'a TargetLocator,
    pub session_id: &'a str,
    pub operation_id: &'a str,
    pub remote_archive: &'a str,
    pub destination: &'a Path,
    pub expected_sha256: &'a str,
    pub expected_event_frontier: u64,
    pub expected_event_frontier_digest: &'a str,
}

/// Unforgeable outside this module: proof that a controller-local archive has
/// the exact digest reported by the target after its atomic install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCheckpoint {
    session_id: String,
    archive_path: PathBuf,
    sha256: String,
    event_frontier: u64,
    event_frontier_digest: String,
}

impl VerifiedCheckpoint {
    pub fn archive_path(&self) -> &Path {
        &self.archive_path
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn event_frontier(&self) -> u64 {
        self.event_frontier
    }
    pub fn event_frontier_digest(&self) -> &str {
        &self.event_frontier_digest
    }
    pub const fn teardown_allowed(&self) -> bool {
        true
    }
}

impl CheckpointTransfer<'_> {
    pub fn execute(&self, executor: &impl CommandExecutor) -> Result<VerifiedCheckpoint> {
        validate_remote_path(self.remote_archive)?;
        let parent = self.destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".hel-checkpoint-")
            .tempfile_in(parent)?;
        let path = temporary.path().to_path_buf();
        let staging = remote_staging_path(self.session_id, self.operation_id)?;
        let transfer_result = transfer_plan(
            self.locator,
            self.session_id,
            self.remote_archive,
            &path,
            &staging,
        )?
        .execute(executor)
        .context("download target checkpoint");
        let staging_cleanup_result = cleanup_transfer_staging(self.locator, &staging, executor);
        if let Err(error) = transfer_result {
            return match staging_cleanup_result {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "clean target checkpoint host staging also failed: {cleanup:#}"
                ))),
            };
        }
        staging_cleanup_result.context("clean target checkpoint host staging")?;
        let sha256 = checkpoint_sha256(&path).context("hash downloaded checkpoint")?;
        ensure!(
            sha256 == self.expected_sha256,
            "target and controller checkpoint checksums differ for complete checkpoint archive: \
             session={}, operation={}, expected_sha256={}, downloaded_sha256={}, downloaded_bytes={}; \
             target archive retained at {}. The previous verified checkpoint was not replaced and \
             the source workspace was not removed. Preserve the source and retry a fresh export; \
             if this repeats, inspect the retained archive and its transfer path",
            self.session_id,
            self.operation_id,
            self.expected_sha256,
            sha256,
            fs::metadata(&path)
                .context("stat downloaded checkpoint")?
                .len(),
            self.remote_archive,
        );
        temporary
            .persist(self.destination)
            .map_err(|error| error.error)?;
        // The bytes were already checksum-verified in this same directory and
        // the rename is atomic, so installation only has to make the copy
        // private and durable; re-reading it would hash the same archive again.
        let post_install = (|| -> Result<()> {
            restrict_permissions(self.destination)?;
            sync_directory(parent)
        })();
        if let Err(error) = post_install {
            return Err(remove_failed_checkpoint_install(self.destination, error));
        }
        Ok(VerifiedCheckpoint {
            session_id: self.session_id.to_owned(),
            archive_path: self.destination.to_path_buf(),
            sha256,
            event_frontier: self.expected_event_frontier,
            event_frontier_digest: self.expected_event_frontier_digest.to_owned(),
        })
    }

    pub fn cleanup_plan(&self, gate: &VerifiedCheckpoint) -> Result<CommandPlan> {
        ensure!(
            gate.session_id == self.session_id,
            "checkpoint gate belongs to another session"
        );
        cleanup_plan(self.locator, self.session_id, self.remote_archive)
    }
}

/// SSH container transfers use a host-side copy because `scp` cannot address
/// a path inside the container. The copy is disposable and must be removed as
/// soon as it has been downloaded; the in-container archive remains gated by
/// [`CheckpointTransfer::cleanup_plan`] until the local copy is verified.
fn cleanup_transfer_staging(
    locator: &TargetLocator,
    staging: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    validate_remote_path(staging)?;
    let command = match locator {
        TargetLocator::SshPodman { ssh, .. } | TargetLocator::SshDocker { ssh, .. } => Some(
            ssh_command(ssh, ["rm", "-f", "--", staging])
                .purpose("remove remote checkpoint staging"),
        ),
        _ => None,
    };
    if let Some(command) = command {
        let output = executor.execute(&command)?;
        if output.status != 0 {
            bail!(
                "{} failed with status {}: {}",
                command.purpose,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

pub fn transfer_plan(
    locator: &TargetLocator,
    session_id: &str,
    remote_archive: &str,
    local_temporary: &Path,
    staging: &str,
) -> Result<CommandPlan> {
    validate_remote_path(remote_archive)?;
    validate_remote_path(staging)?;
    ensure!(
        local_temporary.is_absolute(),
        "local temporary path must be absolute"
    );
    worker_root(locator, session_id)?;
    let local = local_temporary.to_string_lossy().into_owned();
    let mut commands = match locator {
        TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new("cp", [remote_archive, local.as_str()])
                .purpose("copy local bare checkpoint"),
        ],
        TargetLocator::LocalPodman { container_id, .. } => vec![
            CommandSpec::new(
                "podman",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from local Podman"),
        ],
        TargetLocator::LocalDocker { container_id } => vec![
            CommandSpec::new(
                "docker",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from local Docker"),
        ],
        TargetLocator::AppleContainer { container_id } => vec![
            CommandSpec::new(
                "container",
                ["cp", &format!("{container_id}:{remote_archive}"), &local],
            )
            .purpose("download checkpoint from Apple container"),
        ],
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            vec![scp_command(ssh, remote_archive, &local).purpose("download checkpoint over SSH")]
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker { ssh, container_id } => {
            vec![
                ssh_command(ssh, ["mkdir", "-p", ".local/share/hel/transfers"])
                    .purpose("create remote checkpoint staging directory"),
                ssh_command(
                    ssh,
                    [
                        locator.container_engine().expect("remote container"),
                        "cp",
                        &format!("{container_id}:{remote_archive}"),
                        staging,
                    ],
                )
                .purpose("stage remote container checkpoint"),
            ]
        }
    };
    if let TargetLocator::SshPodman { ssh, .. } | TargetLocator::SshDocker { ssh, .. } = locator {
        commands.push(
            scp_command(ssh, staging, &local)
                .purpose("download remote container checkpoint over SSH"),
        );
    }
    Ok(CommandPlan {
        description: format!("download checkpoint for {session_id}"),
        commands,
    })
}

fn cleanup_plan(locator: &TargetLocator, session_id: &str, remote: &str) -> Result<CommandPlan> {
    validate_remote_path(remote)?;
    worker_root(locator, session_id)?;
    let commands = match locator {
        TargetLocator::LocalBare { .. } => vec![
            CommandSpec::new("rm", ["-f", "--", remote])
                .purpose("remove local bare checkpoint staging"),
        ],
        TargetLocator::LocalPodman { container_id, .. } => vec![container_exec(
            "podman",
            container_id,
            ["rm", "-f", "--", remote],
        )],
        TargetLocator::LocalDocker { container_id } => vec![container_exec(
            "docker",
            container_id,
            ["rm", "-f", "--", remote],
        )],
        TargetLocator::AppleContainer { container_id } => vec![container_exec(
            "container",
            container_id,
            ["rm", "-f", "--", remote],
        )],
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => {
            vec![ssh_command(ssh, ["rm", "-f", "--", remote])]
        }
        TargetLocator::SshPodman {
            ssh, container_id, ..
        }
        | TargetLocator::SshDocker { ssh, container_id } => vec![ssh_command(
            ssh,
            [
                locator.container_engine().expect("remote container"),
                "exec",
                container_id,
                "rm",
                "-f",
                "--",
                remote,
            ],
        )],
    };
    Ok(CommandPlan {
        description: format!("clean checkpoint for {session_id}"),
        commands,
    })
}

fn remote_staging_path(session_id: &str, operation_id: &str) -> Result<String> {
    validate_component(session_id, "session ID")?;
    validate_component(operation_id, "checkpoint operation ID")?;
    Ok(format!(
        ".local/share/hel/transfers/{session_id}-{operation_id}.hel.zip"
    ))
}

fn scp_command(ssh: &SshTarget, remote: &str, local: &str) -> CommandSpec {
    let mut args = ssh.ssh_args.clone();
    for argument in &mut args {
        if argument == "-p" {
            *argument = "-P".into();
        }
    }
    args.push(format!("{}:{remote}", ssh.destination));
    args.push(local.into());
    // `scp` opens its own connection to the same host, so it competes for the
    // same pre-auth budget and is admitted and retried the same way.
    CommandSpec::new("scp", args).ssh_destination(ssh.destination.clone())
}

fn ssh_command(ssh: &SshTarget, args: impl IntoIterator<Item = impl AsRef<str>>) -> CommandSpec {
    let remote = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let mut command = ssh.ssh_args.clone();
    command.push(ssh.destination.clone());
    command.push(join_remote_command(&remote));
    CommandSpec::new("ssh", command).ssh_destination(ssh.destination.clone())
}

fn container_exec(
    engine: &str,
    id: &str,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> CommandSpec {
    let mut command = vec!["exec".into(), "-i".into(), id.into()];
    command.extend(args.into_iter().map(Into::into));
    CommandSpec::new(engine, command)
}

fn validate_remote_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty());
    ensure!(
        path.bytes()
            .all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'~' | b'.' | b'-' | b'_')),
        "unsafe remote path"
    );
    ensure!(
        !path.split('/').any(|component| component == ".."),
        "remote path traverses parent"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use mj_core::config::HarnessKind;
    use serde_json::json;

    use super::*;
    use mj_checkpoint::archive::*;
    use mj_worker::checkpoint::*;
    use std::cell::RefCell;
    use std::process::Command;

    use mj_checkpoint::archive::{
        CanonicalExecutionState, CanonicalQueuedCommandKind, CanonicalQueuedPrompt,
        CanonicalSessionState, CanonicalTranscriptItem,
    };
    use mj_core::targets::CommandOutput;

    const SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

    /// A checkpoint download opens its own connection to the host, so it has
    /// to be admitted and retried the same way an `ssh` invocation is.
    #[test]
    fn a_checkpoint_scp_is_tagged_with_the_connection_destination() {
        let ssh = SshTarget {
            destination: "build@10.0.0.1".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        };

        let download = scp_command(&ssh, "remote/archive.zip", "/tmp/local.zip");

        assert_eq!(download.program, "scp");
        assert_eq!(download.ssh_destination.as_deref(), Some("build@10.0.0.1"));
    }
    const NATIVE: &str = "0190aabb-ccdd-7eef-9000-abcdef012345";

    fn ssh() -> SshTarget {
        SshTarget {
            destination: "dev@example.test".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        }
    }

    fn locators() -> Vec<TargetLocator> {
        let name = mj_core::targets::resource_name(SESSION).unwrap();
        vec![
            TargetLocator::LocalBare {
                worker_root: format!("/var/lib/hel/workers/{SESSION}"),
            },
            TargetLocator::LocalPodman {
                container_id: name.clone(),
                workspace_storage: Default::default(),
            },
            TargetLocator::AppleContainer {
                container_id: name.clone(),
            },
            TargetLocator::AwsEc2 {
                profile: "default".into(),
                region: "us-east-1".into(),
                instance_id: "i-0123456789abcdef0".into(),
                ssh: ssh(),
                workspace: format!("~/hel/{SESSION}"),
            },
            TargetLocator::SshBare {
                worker_id: None,
                ssh: ssh(),
                workspace: format!("~/hel/{SESSION}"),
            },
            TargetLocator::SshPodman {
                ssh: ssh(),
                container_id: name,
                workspace_storage: Default::default(),
            },
        ]
    }

    #[test]
    fn transfer_plans_cover_all_target_boundaries() {
        let locators = locators();
        let plans = locators
            .iter()
            .map(|locator| {
                transfer_plan(
                    locator,
                    SESSION,
                    "/var/lib/hel/workers/checkpoint.hel.zip",
                    Path::new("/var/tmp/checkpoint.zip"),
                    &remote_staging_path(SESSION, "test-transfer").unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(plans[0].commands[0].program, "cp");
        assert_eq!(plans[1].commands[0].program, "podman");
        assert_eq!(plans[2].commands[0].program, "container");
        assert_eq!(plans[3].commands[0].program, "scp");
        assert_eq!(plans[4].commands[0].program, "scp");
        assert_eq!(plans[5].commands.len(), 3);
        assert!(
            plans[5].commands[1]
                .args
                .last()
                .unwrap()
                .contains("'podman' 'cp'")
        );
        assert!(
            !plans[5]
                .commands
                .iter()
                .flat_map(|command| &command.args)
                .any(|arg| arg == "--remote")
        );
        assert!(plans[3].commands[0].args.contains(&"-P".into()));
    }

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn fixture(temp: &Path) -> (CheckpointExportSpec, PathBuf) {
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
                },
                output_path: output.clone(),
            },
            output,
        )
    }

    struct CopyExecutor {
        source: PathBuf,
        calls: RefCell<usize>,
    }
    impl CommandExecutor for CopyExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            *self.calls.borrow_mut() += 1;
            fs::copy(
                &self.source,
                command.args.last().context("missing destination")?,
            )?;
            Ok(CommandOutput {
                status: 0,
                stdout: vec![],
                stderr: vec![],
            })
        }
    }

    struct SshDockerTransferExecutor {
        archive: Vec<u8>,
        commands: RefCell<Vec<CommandSpec>>,
        fail_download: bool,
        fail_staging_cleanup: bool,
    }

    impl SshDockerTransferExecutor {
        fn new(archive: Vec<u8>) -> Self {
            Self {
                archive,
                commands: RefCell::new(Vec::new()),
                fail_download: false,
                fail_staging_cleanup: false,
            }
        }
    }

    impl CommandExecutor for SshDockerTransferExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            let remote = command.args.last().map(String::as_str).unwrap_or_default();
            if command.program == "scp" {
                if self.fail_download {
                    return Ok(CommandOutput {
                        status: 23,
                        stdout: Vec::new(),
                        stderr: b"scp unavailable".to_vec(),
                    });
                }
                fs::write(
                    command
                        .args
                        .last()
                        .context("missing local checkpoint path")?,
                    &self.archive,
                )?;
            }
            if command.program == "ssh"
                && remote.contains("'rm' '-f' '--' '.local/share/hel/transfers/")
                && self.fail_staging_cleanup
            {
                return Ok(CommandOutput {
                    status: 19,
                    stdout: Vec::new(),
                    stderr: b"staging cleanup unavailable".to_vec(),
                });
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn ssh_docker_locator() -> TargetLocator {
        TargetLocator::SshDocker {
            ssh: ssh(),
            container_id: mj_core::targets::resource_name(SESSION).unwrap(),
        }
    }

    #[test]
    fn export_and_transfer_only_gate_after_local_verification() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, source) = fixture(temp.path());
        let target = export_checkpoint(&spec).unwrap();
        assert_eq!(target.event_frontier, 1);
        assert_eq!(
            target.event_frontier_digest,
            spec.canonical_session.event_frontier_digest
        );
        let destination = temp.path().join("controller/session.hel.zip");
        let locator = &locators()[0];
        let gate = CheckpointTransfer {
            locator,
            session_id: SESSION,
            operation_id: "test-transfer",
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_sha256: &target.sha256,
            expected_event_frontier: 1,
            expected_event_frontier_digest: &spec.canonical_session.event_frontier_digest,
        }
        .execute(&CopyExecutor {
            source,
            calls: RefCell::new(0),
        })
        .unwrap();
        assert!(gate.teardown_allowed());
        assert_eq!(gate.event_frontier(), 1);
        assert_eq!(
            gate.event_frontier_digest(),
            spec.canonical_session.event_frontier_digest
        );
        assert_eq!(
            read_archive_verified(&destination).unwrap().archive_sha256,
            gate.sha256()
        );
    }

    /// The controller streams the export spec to save a round trip to the
    /// target. Both spellings have to produce the same archive.
    #[test]
    fn a_streamed_spec_exports_the_same_archive_as_a_spec_file() {
        let temp = tempfile::tempdir().unwrap();
        let (mut spec, _) = fixture(temp.path());
        let from_file = export_from_spec_file(&spec.output_path.with_extension("spec.json"))
            .err()
            .map(|error| format!("{error:#}"));
        assert!(
            from_file.is_some_and(|error| error.contains("read checkpoint export spec")),
            "a missing spec file must still be reported as a read failure"
        );

        let spec_path = temp.path().join("checkpoint-spec.json");
        spec.write(&spec_path).unwrap();
        let from_file = export_from_spec_file(&spec_path).unwrap();
        let file_archive = fs::read(&spec.output_path).unwrap();

        spec.output_path = temp.path().join("worker/streamed.hel.zip");
        let body = serde_json::to_vec(&spec).unwrap();
        let streamed = export_from_spec_reader(&mut body.as_slice()).unwrap();
        let streamed_archive = fs::read(&spec.output_path).unwrap();

        assert_eq!(streamed.sha256, from_file.sha256);
        assert_eq!(streamed.event_frontier, from_file.event_frontier);
        assert_eq!(
            streamed.event_frontier_digest,
            from_file.event_frontier_digest
        );
        assert_eq!(streamed_archive, file_archive);
        assert_eq!(
            read_archive_verified(&spec.output_path)
                .unwrap()
                .archive_sha256,
            streamed.sha256
        );
    }

    #[test]
    fn transfer_rejects_a_target_checksum_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let (spec, source) = fixture(temp.path());
        export_checkpoint(&spec).unwrap();
        let destination = temp.path().join("controller/session.hel.zip");
        let unexpected_sha256 = "b".repeat(64);

        let error = CheckpointTransfer {
            locator: &locators()[0],
            session_id: SESSION,
            operation_id: "test-transfer",
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_sha256: &unexpected_sha256,
            expected_event_frontier: 1,
            expected_event_frontier_digest: &spec.canonical_session.event_frontier_digest,
        }
        .execute(&CopyExecutor {
            source,
            calls: RefCell::new(0),
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("checkpoint checksums differ"));
        assert!(!destination.exists());
    }

    #[test]
    fn ssh_docker_failed_hash_cleans_host_staging_but_preserves_container_archive() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("controller/session.hel.zip");
        let executor = SshDockerTransferExecutor::new(vec![b'x'; 128 * 1024]);
        let error = CheckpointTransfer {
            locator: &ssh_docker_locator(),
            session_id: SESSION,
            operation_id: "test-transfer",
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_sha256: &"0".repeat(64),
            expected_event_frontier: 1,
            expected_event_frontier_digest: &"a".repeat(64),
        }
        .execute(&executor)
        .unwrap_err();

        assert!(format!("{error:#}").contains("checkpoint checksums differ"));
        assert!(!destination.exists());
        let commands = executor.commands.borrow();
        assert!(commands.iter().any(|command| {
            command.program == "ssh"
                && command.args.last().is_some_and(|remote| {
                    remote.contains("'rm' '-f' '--' '.local/share/hel/transfers/")
                })
        }));
        assert!(!commands.iter().any(|command| {
            command
                .args
                .last()
                .is_some_and(|remote| remote.contains("'docker' 'exec'"))
        }));
    }

    #[test]
    fn ssh_docker_download_and_staging_cleanup_errors_keep_the_original_failure() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("controller/session.hel.zip");
        let mut executor = SshDockerTransferExecutor::new(vec![b'x'; 128 * 1024]);
        executor.fail_download = true;
        executor.fail_staging_cleanup = true;
        let error = CheckpointTransfer {
            locator: &ssh_docker_locator(),
            session_id: SESSION,
            operation_id: "test-transfer",
            remote_archive: "/var/lib/hel/workers/source.hel.zip",
            destination: &destination,
            expected_sha256: &"0".repeat(64),
            expected_event_frontier: 1,
            expected_event_frontier_digest: &"a".repeat(64),
        }
        .execute(&executor)
        .unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("download target checkpoint"), "{text}");
        assert!(text.contains("scp unavailable"), "{text}");
        assert!(
            text.contains("clean target checkpoint host staging also failed")
                && text.contains("staging cleanup unavailable"),
            "{text}"
        );
        let commands = executor.commands.borrow();
        assert!(commands.iter().any(|command| {
            command.program == "ssh"
                && command.args.last().is_some_and(|remote| {
                    remote.contains("'rm' '-f' '--' '.local/share/hel/transfers/")
                })
        }));
        assert!(!commands.iter().any(|command| {
            command
                .args
                .last()
                .is_some_and(|remote| remote.contains("'docker' 'exec'"))
        }));
    }

    #[test]
    fn overlapping_remote_transfers_keep_their_own_bytes_and_cleanup() {
        use std::collections::BTreeMap;
        use std::sync::{Condvar, Mutex};
        use std::time::Duration;

        #[derive(Default)]
        struct Staging {
            files: BTreeMap<String, Vec<u8>>,
            copies: usize,
            first_cleaned: bool,
        }
        struct InterleavedExecutor<'a> {
            staging: &'a (Mutex<Staging>, Condvar),
            archive: &'a [u8],
            first: bool,
        }
        impl CommandExecutor for InterleavedExecutor<'_> {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                let (mutex, changed) = self.staging;
                let remote = command.args.last().unwrap();
                // Fixture paths contain no quotes or spaces. Inspect the final
                // remote argument, so a shared staging path really collides.
                let remote_path = || {
                    remote
                        .rsplit(' ')
                        .next()
                        .unwrap()
                        .trim_matches('\'')
                        .to_owned()
                };
                match command.purpose.as_str() {
                    "stage remote container checkpoint" => {
                        let mut state = mutex.lock().unwrap();
                        state.files.insert(remote_path(), self.archive.to_vec());
                        state.copies += 1;
                        changed.notify_all();
                    }
                    "download remote container checkpoint over SSH" => {
                        let (state, timeout) = changed
                            .wait_timeout_while(
                                mutex.lock().unwrap(),
                                Duration::from_secs(5),
                                |state| state.copies < 2 || (!self.first && !state.first_cleaned),
                            )
                            .unwrap();
                        ensure!(!timeout.timed_out(), "interleaved transfer stalled");
                        let source = command.args[command.args.len() - 2]
                            .split_once(':')
                            .unwrap()
                            .1;
                        let bytes = state
                            .files
                            .get(source)
                            .context("other transfer removed staging")?;
                        fs::write(remote, bytes)?;
                    }
                    "remove remote checkpoint staging" => {
                        let mut state = mutex.lock().unwrap();
                        state.files.remove(&remote_path());
                        if self.first {
                            state.first_cleaned = true;
                            changed.notify_all();
                        }
                    }
                    "create remote checkpoint staging directory" => {}
                    purpose => bail!("unexpected transfer command: {purpose}"),
                }
                Ok(CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        for locator in [locators().pop().unwrap(), ssh_docker_locator()] {
            let directory = tempfile::tempdir().unwrap();
            let staging = (Mutex::new(Staging::default()), Condvar::new());
            let first = vec![b'a'; 192 * 1024];
            let second = vec![b'b'; 256 * 1024];
            std::thread::scope(|scope| {
                let run = |operation, bytes: &[u8], is_first| {
                    let source = directory.path().join(format!("{operation}-source.zip"));
                    fs::write(&source, bytes).unwrap();
                    let destination = directory.path().join(format!("{operation}-verified.zip"));
                    let digest = checkpoint_sha256(&source).unwrap();
                    let gate = CheckpointTransfer {
                        locator: &locator,
                        session_id: SESSION,
                        operation_id: operation,
                        remote_archive: &format!("/workers/{operation}.zip"),
                        destination: &destination,
                        expected_sha256: &digest,
                        expected_event_frontier: 1,
                        expected_event_frontier_digest: &"a".repeat(64),
                    }
                    .execute(&InterleavedExecutor {
                        staging: &staging,
                        archive: bytes,
                        first: is_first,
                    })
                    .unwrap();
                    assert_eq!(fs::read(gate.archive_path()).unwrap(), bytes);
                    assert_eq!(gate.sha256(), digest);
                };
                let first_run = scope.spawn(move || run("first", &first, true));
                let second_run = scope.spawn(move || run("second", &second, false));
                first_run.join().unwrap();
                second_run.join().unwrap();
            });
            assert!(staging.0.lock().unwrap().files.is_empty());
        }
    }

    #[test]
    fn corrupt_or_truncated_transfer_preserves_previous_checkpoint_and_reports_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.zip");
        let original = vec![b'a'; 192 * 1024];
        fs::write(&source, &original).unwrap();
        let expected_sha256 = checkpoint_sha256(&source).unwrap();
        let mut corrupt = original.clone();
        corrupt[80 * 1024] = b'b';
        for bytes in [corrupt, original[..1024].to_vec()] {
            let downloaded = temp.path().join("download.zip");
            fs::write(&downloaded, &bytes).unwrap();
            let actual_sha256 = checkpoint_sha256(&downloaded).unwrap();
            let destination = temp.path().join("previous.zip");
            fs::write(&destination, b"previous verified checkpoint").unwrap();
            let error = CheckpointTransfer {
                locator: &ssh_docker_locator(),
                session_id: SESSION,
                operation_id: "failed-export",
                remote_archive: "/workers/failed-export.zip",
                destination: &destination,
                expected_sha256: &expected_sha256,
                expected_event_frontier: 1,
                expected_event_frontier_digest: &"a".repeat(64),
            }
            .execute(&SshDockerTransferExecutor::new(bytes.clone()))
            .unwrap_err();
            let detail = format!("{error:#}");
            for expected in [
                "complete checkpoint archive",
                SESSION,
                "failed-export",
                &expected_sha256,
                &actual_sha256,
                &format!("downloaded_bytes={}", bytes.len()),
                "target archive retained at /workers/failed-export.zip",
                "retry a fresh export",
            ] {
                assert!(
                    detail.contains(expected),
                    "missing {expected:?} from {detail}"
                );
            }
            assert_eq!(
                fs::read(&destination).unwrap(),
                b"previous verified checkpoint"
            );
            assert_eq!(fs::read(&source).unwrap(), original);
        }
    }
}
