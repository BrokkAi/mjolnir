//! Staging the second-opinion reviewer's profile onto a session's target.
//!
//! The reviewer runs a different configured profile in the primary session's
//! own target. Its harness home is a fresh copy of that profile, placed inside
//! the primary worker root, so the reviewer never reads or writes the
//! primary's home and nothing outside the worker root has to be provisioned.
//!
//! Nothing here creates a session record, a target, a checkout, or any target
//! lifecycle operation. Staging a reviewer is a file copy into a directory the
//! session's worker already owns.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::worker_binary::{bridge_launch, stage_profile};
use super::{Controller, execute_checked, scp_command_spec, ssh_command_spec};
use hel::hel_targets::{self, CommandExecutor, CommandSpec, ProcessExecutor};
use hel::hel_worker_launch::{
    REVIEWER_DIR, ReviewMcpDelivery, ReviewMcpServer, ReviewerLaunchConfig,
    reviewer_staging_profile_home,
};

impl Controller {
    /// Copy `profile_id`'s home into the session worker's reviewer directory
    /// and describe how the worker should launch it.
    ///
    /// `generation` distinguishes reviewer lifetimes: bumping it tells the
    /// worker to start a new conversation instead of reloading the last one.
    pub fn stage_reviewer_profile(
        &self,
        session_id: &str,
        profile_id: &str,
        generation: u64,
    ) -> Result<ReviewerLaunchConfig> {
        self.stage_reviewer_profile_controlled(
            session_id,
            profile_id,
            generation,
            &[],
            &ProcessExecutor,
        )
    }

    /// Stage a reviewer that also gets `mcp_servers`, which is how a turn
    /// review attaches its analyzer tools.
    ///
    /// `dispatch_tool` adds the review supervisor's own tool, which is this
    /// worker's binary in another mode. Only the controller knows where that
    /// binary and its socket sit on the target, so it is built here rather
    /// than by the caller.
    pub fn stage_reviewer_profile_with_mcp(
        &self,
        session_id: &str,
        profile_id: &str,
        generation: u64,
        mcp_servers: &[ReviewMcpServer],
        dispatch_tool: bool,
    ) -> Result<ReviewerLaunchConfig> {
        let mut servers = mcp_servers.to_vec();
        if dispatch_tool {
            let (_, worker_root) = self.worker_placement(session_id)?;
            servers.push(review_dispatch_server(&worker_root));
        }
        self.stage_reviewer_profile_controlled(
            session_id,
            profile_id,
            generation,
            &servers,
            &ProcessExecutor,
        )
    }

    pub fn stage_reviewer_profile_controlled(
        &self,
        session_id: &str,
        profile_id: &str,
        generation: u64,
        mcp_servers: &[ReviewMcpServer],
        executor: &impl CommandExecutor,
    ) -> Result<ReviewerLaunchConfig> {
        let profile = self
            .config
            .profiles
            .get(profile_id)
            .with_context(|| format!("unknown profile {profile_id:?}"))?;
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let target = self
            .config
            .targets
            .get(&session.target_template_id)
            .context("session target template is missing")?;
        let execution_policy = target.execution_policy();
        let (backend, worker_root) = self.worker_placement(session_id)?;

        let staging = tempfile::tempdir().context("create reviewer staging directory")?;
        let local = staging.path().join("profile");
        stage_profile(profile, &local).with_context(|| format!("stage profile {profile_id:?}"))?;
        // Harnesses that ignore MCP servers offered over ACP read their own
        // configuration instead, so the servers are written into the copy
        // being staged, before it is uploaded.
        if !mcp_servers.is_empty()
            && ReviewMcpDelivery::for_harness(profile.kind) == ReviewMcpDelivery::HarnessProfile
        {
            configure_staged_review_mcp(profile.kind, &local, mcp_servers)
                .with_context(|| format!("configure reviewer MCP servers for {profile_id:?}"))?;
        }
        upload_reviewer_profile(executor, &backend, &worker_root, generation, &local)?;

        let (bridge_command, bridge_args) = bridge_launch(profile.kind, execution_policy);
        let mut environment = profile.environment.clone();
        // The worker sets the harness home from the directory it staged, so
        // sending one here could only point the reviewer somewhere it must not
        // read.
        environment.remove(profile.home_env());
        Ok(ReviewerLaunchConfig {
            profile_id: profile_id.to_owned(),
            harness: profile.kind,
            bridge_command: bridge_command.into(),
            bridge_args,
            environment,
            execution_policy,
            model: None,
            effort: None,
            generation,
            // A harness that reads its servers from the staged profile must
            // not also be offered them over ACP: it would either duplicate the
            // server or reject the request.
            mcp_servers: match ReviewMcpDelivery::for_harness(profile.kind) {
                ReviewMcpDelivery::Acp => mcp_servers.to_vec(),
                ReviewMcpDelivery::HarnessProfile => Vec::new(),
            },
        })
    }
}

/// Writes `servers` into the staged profile of a harness that reads its MCP
/// configuration from disk.
///
/// Claude Code reads `mcpServers` from `.claude.json` in its config directory;
/// Kimi reads `mcpServers` from `mcp.json` in its home, and needs the runtime
/// id its own schema carries. Both files are the reviewer's private copy, so
/// nothing here can reach the user's own configuration.
fn configure_staged_review_mcp(
    harness: hel::hel_config::HarnessKind,
    profile_stage: &Path,
    servers: &[ReviewMcpServer],
) -> Result<()> {
    let (file, kimi) = match harness {
        hel::hel_config::HarnessKind::Claude => (".claude.json", false),
        hel::hel_config::HarnessKind::Kimi => ("mcp.json", true),
        other => bail!("{other:?} does not read MCP servers from its profile"),
    };
    let path = profile_stage.join(file);
    let mut document = match std::fs::read(&path) {
        Ok(body) => serde_json::from_slice::<serde_json::Value>(&body)
            .with_context(|| format!("parse staged reviewer configuration {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read staged reviewer configuration {}", path.display()));
        }
    };
    let root = document.as_object_mut().with_context(|| {
        format!(
            "staged reviewer configuration {} must contain a JSON object",
            path.display()
        )
    })?;
    let configured = root
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .with_context(|| {
            format!(
                "mcpServers in staged reviewer configuration {} must be a JSON object",
                path.display()
            )
        })?;
    for server in servers {
        let mut entry = serde_json::json!({
            "command": server.command,
            "args": server.args,
        });
        if kimi {
            let object = entry
                .as_object_mut()
                .expect("the server entry is a JSON object");
            object.insert("transport".into(), "stdio".into());
            object.insert("runtime_id".into(), "local".into());
        } else {
            let object = entry
                .as_object_mut()
                .expect("the server entry is a JSON object");
            object.insert("type".into(), "stdio".into());
        }
        configured.insert(server.name.clone(), entry);
    }
    let mut body = serde_json::to_vec_pretty(&document)?;
    body.push(b'\n');
    hel::hel_config::atomic_write(&path, &body)
        .with_context(|| format!("write staged reviewer configuration {}", path.display()))
}

/// The review supervisor's dispatch tool, as it runs inside the container:
/// this worker's own binary in `review-mcp` mode, talking to the socket the
/// worker serves in its reviewer directory.
fn review_dispatch_server(worker_root: &str) -> ReviewMcpServer {
    let socket = format!(
        "{worker_root}/{}/{}",
        REVIEWER_DIR,
        hel::hel_review::mcp::REVIEW_DISPATCH_SOCKET
    );
    ReviewMcpServer {
        name: hel::hel_review::mcp::REVIEW_MCP_SERVER_NAME.to_owned(),
        command: Path::new(worker_root).join("hel"),
        args: vec![
            "worker".to_owned(),
            "review-mcp".to_owned(),
            "--socket".to_owned(),
            socket,
        ],
    }
}

/// Where one immutable reviewer profile snapshot lives on the target.
///
/// The worker copies this source into each role's private harness home before
/// launch. Keeping generations in separate directories means staging a lane
/// cannot replace the profile a running role is using.
fn reviewer_profile_home(worker_root: &str, generation: u64) -> String {
    reviewer_staging_profile_home(Path::new(worker_root), generation)
        .to_string_lossy()
        .into_owned()
}

/// Replace this generation's staged profile with a fresh copy of `local`.
///
/// The previous snapshot for this generation is removed first: a reviewer
/// profile is a snapshot of the user's configured home, and merging a new copy
/// over an old one would leave credentials and skills the source no longer has.
fn upload_reviewer_profile(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    worker_root: &str,
    generation: u64,
    local: &Path,
) -> Result<()> {
    let home = reviewer_profile_home(worker_root, generation);
    match locator {
        hel_targets::TargetLocator::LocalBare { .. } => {
            for command in [
                CommandSpec::new("rm", ["-rf", "--", &home])
                    .purpose("clear the local reviewer profile"),
                CommandSpec::new("mkdir", ["-p", &home])
                    .purpose("create the local reviewer profile directory"),
                CommandSpec::new(
                    "cp",
                    [
                        "-R".to_owned(),
                        format!("{}/.", local.display()),
                        home.clone(),
                    ],
                )
                .purpose("install the local reviewer profile"),
                CommandSpec::new("chmod", ["-R", "go-rwx", &home])
                    .purpose("restrict local reviewer profile permissions"),
            ] {
                execute_checked(executor, command)?;
            }
        }
        hel_targets::TargetLocator::LocalPodman { container_id, .. }
        | hel_targets::TargetLocator::LocalDocker { container_id }
        | hel_targets::TargetLocator::AppleContainer { container_id } => {
            let engine = match locator {
                hel_targets::TargetLocator::LocalPodman { .. } => "podman",
                hel_targets::TargetLocator::LocalDocker { .. } => "docker",
                hel_targets::TargetLocator::AppleContainer { .. } => "container",
                _ => unreachable!("matched local container target"),
            };
            for arguments in [
                vec![
                    "exec".to_owned(),
                    container_id.clone(),
                    "rm".to_owned(),
                    "-rf".to_owned(),
                    "--".to_owned(),
                    home.clone(),
                ],
                vec![
                    "exec".to_owned(),
                    container_id.clone(),
                    "mkdir".to_owned(),
                    "-p".to_owned(),
                    home.clone(),
                ],
                vec![
                    "cp".to_owned(),
                    format!("{}/.", local.display()),
                    format!("{container_id}:{home}"),
                ],
                vec![
                    "exec".to_owned(),
                    container_id.clone(),
                    "chmod".to_owned(),
                    "-R".to_owned(),
                    "go-rwx".to_owned(),
                    home.clone(),
                ],
            ] {
                execute_checked(
                    executor,
                    CommandSpec::new(engine, arguments).purpose("stage the reviewer profile"),
                )?;
            }
        }
        hel_targets::TargetLocator::AwsEc2 { ssh, .. }
        | hel_targets::TargetLocator::SshBare { ssh, .. } => {
            let incoming = format!("{home}.incoming");
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["mkdir", "-p", worker_root])
                    .purpose("create the reviewer directory"),
            )?;
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["rm", "-rf", "--", &incoming, &home])
                    .purpose("clear the reviewer profile"),
            )?;
            execute_checked(
                executor,
                scp_command_spec(ssh, local, &incoming, true)
                    .purpose("upload the reviewer profile"),
            )?;
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["mv", &incoming, &home])
                    .purpose("install the reviewer profile"),
            )?;
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["chmod", "-R", "go-rwx", &home])
                    .purpose("restrict reviewer profile permissions"),
            )?;
        }
        hel_targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        } => {
            let upload = format!("{worker_root}/.reviewer-upload-{generation}");
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["rm", "-rf", "--", &upload])
                    .purpose("clear remote reviewer staging"),
            )?;
            execute_checked(
                executor,
                scp_command_spec(ssh, local, &upload, true)
                    .purpose("upload the remote reviewer profile"),
            )?;
            for arguments in [
                vec![
                    "podman".to_owned(),
                    "exec".to_owned(),
                    container_id.clone(),
                    "rm".to_owned(),
                    "-rf".to_owned(),
                    "--".to_owned(),
                    home.clone(),
                ],
                vec![
                    "podman".to_owned(),
                    "exec".to_owned(),
                    container_id.clone(),
                    "mkdir".to_owned(),
                    "-p".to_owned(),
                    home.clone(),
                ],
                vec![
                    "podman".to_owned(),
                    "cp".to_owned(),
                    format!("{upload}/."),
                    format!("{container_id}:{home}"),
                ],
                vec![
                    "podman".to_owned(),
                    "exec".to_owned(),
                    container_id.clone(),
                    "chmod".to_owned(),
                    "-R".to_owned(),
                    "go-rwx".to_owned(),
                    home.clone(),
                ],
            ] {
                execute_checked(
                    executor,
                    ssh_command_spec(ssh, arguments).purpose("stage the remote reviewer profile"),
                )?;
            }
            execute_checked(
                executor,
                ssh_command_spec(ssh, ["rm", "-rf", "--", &upload])
                    .purpose("remove remote reviewer staging"),
            )?;
        }
    }
    if home.trim().is_empty() {
        bail!("the reviewer profile home resolved to an empty path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::hel_controller::test_support::checkpoint_test_session;
    use hel::hel_config::{HarnessKind, HarnessProfile, HelConfig, TargetTemplate};
    use hel::hel_state::{HelState, SessionState};
    use hel::hel_targets::CommandOutput;

    struct RecordingExecutor {
        commands: RefCell<Vec<CommandSpec>>,
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self {
                commands: RefCell::new(Vec::new()),
            }
        }

        /// Every command as one line, for order-sensitive assertions.
        fn script(&self) -> Vec<String> {
            self.commands
                .borrow()
                .iter()
                .map(|command| format!("{} {}", command.program, command.args.join(" ")))
                .collect()
        }
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.borrow_mut().push(command.clone());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// A controller with one running session and two configured profiles: the
    /// session's own and a second one to review with.
    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

    fn fixture(directory: &Path, locator: hel::hel_state::TargetLocator) -> (Controller, String) {
        let session_id = SESSION_ID;
        let mut session = checkpoint_test_session(session_id);
        session.target_template_id = "local".into();
        session.state = SessionState::Running;
        session.target = Some(locator);
        let mut config = HelConfig::default();
        config
            .targets
            .insert("local".into(), TargetTemplate::LocalBare);
        for (id, kind) in [
            ("codex", HarnessKind::Codex),
            ("claude", HarnessKind::Claude),
        ] {
            let home = directory.join(id);
            std::fs::create_dir_all(&home).unwrap();
            config.profiles.insert(
                id.to_owned(),
                HarnessProfile {
                    kind,
                    home,
                    environment: BTreeMap::from([("EXTRA".into(), "1".into())]),
                    context_window_bytes: None,
                },
            );
        }
        (
            Controller {
                config,
                state: HelState {
                    sessions: BTreeMap::from([(session_id.into(), session)]),
                    ..HelState::default()
                },
            },
            session_id.to_owned(),
        )
    }

    #[test]
    fn staging_copies_the_chosen_profile_into_the_worker_root() {
        let directory = tempfile::tempdir().unwrap();
        let worker_root = directory.path().join(SESSION_ID);
        std::fs::create_dir_all(directory.path().join("claude")).unwrap();
        // A file the allowlist copies, so the stage has something to move.
        std::fs::write(directory.path().join("claude/CLAUDE.md"), b"reviewer").unwrap();
        let (controller, session_id) = fixture(
            directory.path(),
            hel::hel_state::TargetLocator::LocalBare {
                worker_root: worker_root.clone(),
            },
        );
        let executor = RecordingExecutor::new();

        let config = controller
            .stage_reviewer_profile_controlled(&session_id, "claude", 0, &[], &executor)
            .unwrap();

        assert_eq!(config.profile_id, "claude");
        assert_eq!(config.harness, HarnessKind::Claude);
        assert_eq!(config.generation, 0);
        assert_eq!(config.model, None);
        assert_eq!(config.effort, None);
        // The worker owns the harness home, so the controller never sends one.
        assert!(
            !config
                .environment
                .contains_key(HarnessKind::Claude.home_env())
        );
        assert_eq!(
            config.environment.get("EXTRA").map(String::as_str),
            Some("1")
        );

        let home = format!("{}/reviewer/profile", worker_root.display());
        let script = executor.script();
        let cleared = script
            .iter()
            .position(|line| line.starts_with("rm ") && line.contains(&home))
            .expect("the previous reviewer profile is cleared");
        let copied = script
            .iter()
            .position(|line| line.starts_with("cp ") && line.ends_with(&home))
            .expect("the staged profile is installed");
        assert!(
            cleared < copied,
            "a stale profile must go before the new one lands: {script:?}"
        );
        assert!(
            script
                .iter()
                .any(|line| line.contains("go-rwx") && line.contains(&home)),
            "the reviewer profile must not be world readable: {script:?}"
        );
    }

    #[test]
    fn local_container_targets_stage_the_reviewer_through_their_engine() {
        let directory = tempfile::tempdir().unwrap();
        let container_id = hel::hel_targets::resource_name(SESSION_ID).unwrap();
        for (locator, engine) in [
            (
                hel::hel_state::TargetLocator::LocalPodman {
                    container_id: container_id.clone(),
                    workspace_storage: Default::default(),
                },
                "podman",
            ),
            (
                hel::hel_state::TargetLocator::LocalDocker {
                    container_id: container_id.clone(),
                },
                "docker",
            ),
        ] {
            let (controller, session_id) = fixture(directory.path(), locator);
            let executor = RecordingExecutor::new();

            controller
                .stage_reviewer_profile_controlled(&session_id, "codex", 3, &[], &executor)
                .unwrap();

            let script = executor.script();
            assert!(
                script
                    .iter()
                    .all(|line| line.starts_with(&format!("{engine} "))),
                "a container target is reached only through its engine: {script:?}"
            );
            let home = script
                .iter()
                .find_map(|line| {
                    line.split(' ')
                        .find(|word| word.contains("/reviewer/profile"))
                })
                .expect("the reviewer profile is placed")
                .to_owned();
            assert!(
                home.contains(&format!("/{session_id}")),
                "the reviewer lives under this session's worker root: {home}"
            );
            // Nothing here provisions a target, a checkout, or another session.
            assert!(
                !script.iter().any(|line| {
                    line.contains("run") || line.contains("git") || line.contains("create")
                }),
                "staging a reviewer provisions nothing: {script:?}"
            );
        }
    }

    #[test]
    fn an_unknown_profile_is_refused_before_anything_is_copied() {
        let directory = tempfile::tempdir().unwrap();
        let (controller, session_id) = fixture(
            directory.path(),
            hel::hel_state::TargetLocator::LocalBare {
                worker_root: directory.path().join(SESSION_ID),
            },
        );
        let executor = RecordingExecutor::new();

        let error = controller
            .stage_reviewer_profile_controlled(&session_id, "missing", 0, &[], &executor)
            .unwrap_err();

        assert!(format!("{error:#}").contains("unknown profile"));
        assert!(executor.commands.borrow().is_empty());
    }

    #[test]
    fn a_new_generation_travels_to_the_worker_so_it_starts_a_fresh_reviewer() {
        let directory = tempfile::tempdir().unwrap();
        let (controller, session_id) = fixture(
            directory.path(),
            hel::hel_state::TargetLocator::LocalBare {
                worker_root: directory.path().join(SESSION_ID),
            },
        );
        let executor = RecordingExecutor::new();

        let first = controller
            .stage_reviewer_profile_controlled(&session_id, "codex", 0, &[], &executor)
            .unwrap();
        let second = controller
            .stage_reviewer_profile_controlled(&session_id, "codex", 1, &[], &executor)
            .unwrap();

        assert!(first.reusable_for(&first));
        assert!(
            !first.reusable_for(&second),
            "a new generation must not reload the old conversation"
        );
    }
}
