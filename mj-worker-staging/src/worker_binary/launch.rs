use super::*;

/// Every top-level session of every harness gets the message-only `mj-agents`
/// server, and that server carries ask_oracle; a child never does.
pub fn oracle_launch_enabled(
    session: &mj_core::state::SessionRecord,
    managed_child: bool,
    choices: &mj_core::config::SubagentConfig,
    yolo: bool,
) -> bool {
    !managed_child && session.effective_oracle(choices, yolo).is_some()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WorkerFilePreparation {
    Install,
    /// Restage the profile into a retained home, and replace the worker
    /// binary when the retained worker's build differs.
    RefreshProfile,
    /// Restage the profile into a retained home before a stopped worker
    /// starts, so it starts with the profile's current login (#1547). The
    /// worker binary is the caller's business.
    RefreshProfileOnly,
}

/// The session history a launch continues. A restore deletes the worker's
/// relay state, so the history the restore brings back decides what the
/// launch must not repeat. Everywhere else the stored projection is the live
/// history.
#[derive(Clone, Copy)]
pub enum LaunchHistory<'a> {
    Stored,
    Restored(&'a mj_core::archive::CanonicalSessionSnapshot),
}

/// Whether this history shows the prompt with `command_id` as started. A
/// transcript item `user:<command id>` appears when a prompt starts; a prompt
/// that was only queued has none, so its input never ran.
pub fn prompt_started(
    session_id: &str,
    history: &LaunchHistory<'_>,
    command_id: &str,
) -> Result<bool> {
    let stable_id = format!("user:{command_id}");
    match history {
        LaunchHistory::Stored => {
            crate::database::materialized_transcript_item_exists(session_id, &stable_id)
        }
        LaunchHistory::Restored(snapshot) => Ok(snapshot
            .transcript
            .iter()
            .any(|item| item.stable_id == stable_id)),
    }
}

pub fn disable_mbx_cache_shims(
    session_id: &str,
    backend: &targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
    reason: &str,
) {
    let cleanup = remove_generated_mbx_shims(executor, backend, worker_root);
    if let Err(error) = &cleanup {
        tracing::warn!(
            session_id,
            "could not remove marked shared-mbx launchers: {error:#}"
        );
    }
    let cleanup_note = cleanup
        .err()
        .map(|error| format!(" Mjolnir could not remove its marked launchers: {error:#}."))
        .unwrap_or_default();
    tracing::warn!(
        session_id,
        "shared mbx is unavailable in this container: {reason}"
    );
    executor.notify_notice(&format!(
        "The Rust build cache is unavailable in this container: {reason}.{cleanup_note} The session will start without it."
    ));
}

pub fn build_cache_config_roots(launch: &WorkerLaunchConfig) -> Vec<PathBuf> {
    [&launch.target_environment, &launch.environment]
        .into_iter()
        .filter_map(|environment| environment.get("XDG_CONFIG_HOME").map(PathBuf::from))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Carries `[jev] enabled = false` to the worker, which reads it from its
/// launch environment and then makes no classifier calls.
pub fn apply_jev_switch(launch: &mut WorkerLaunchConfig, enabled: bool) {
    if enabled {
        return;
    }
    for environment in [&mut launch.target_environment, &mut launch.environment] {
        environment.insert(
            mj_core::jev::DISABLED_ENVIRONMENT.to_owned(),
            "1".to_owned(),
        );
    }
}

/// Whether this session gets Mjolnir's delegation tools in place of its
/// harness's own. The session's stored choice governs; `None` means native
/// sub-agents, so a session created before the per-session choice existed (or
/// through `mj new` with neither flag given) gets its harness's own
/// sub-agents. A child never gets them, and only a harness whose
/// `HarnessKind::supports_delegation_tools` holds can receive them at all.
#[cfg(any(test, feature = "test-support"))]
pub fn subagent_tools_enabled(session: &mj_core::state::SessionRecord, is_child: bool) -> bool {
    session
        .subagents
        .clone()
        .unwrap_or_default()
        .for_launch(session.harness_kind, is_child)
        .uses_mjolnir()
}

pub fn worker_workspace_for_recovery(
    backend: &targets::TargetLocator,
    directory: &Path,
) -> Option<WorkerWorkspace> {
    let target = match backend {
        targets::TargetLocator::LocalBare { .. } => mj_core::state::ManagedWorktreeTarget::Local,
        targets::TargetLocator::SshBare { ssh, .. } => mj_core::state::ManagedWorktreeTarget::Ssh {
            destination: ssh.destination.clone(),
            ssh_args: ssh.ssh_args.clone(),
        },
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::AwsEc2 { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. }
        | targets::TargetLocator::SshAppleContainer { .. }
        | targets::TargetLocator::SshNitro { .. } => return None,
    };
    Some(WorkerWorkspace {
        target,
        directory: directory.to_path_buf(),
    })
}

pub struct LaunchWorkspace<'a> {
    pub session_id: &'a str,
    pub container: Option<&'a Path>,
    pub parent_worktree: Option<&'a mj_core::state::ManagedWorktree>,
}

#[cfg(any(test, feature = "test-support"))]
pub fn worker_launch_config(
    session: &mj_core::state::SessionRecord,
    profile: &mj_core::config::HarnessProfile,
    bundle: Option<&ProjectBundle>,
    backend: &targets::TargetLocator,
    worker_workspace: LaunchWorkspace<'_>,
    target: &mj_core::state::TargetRuntimeSettings,
) -> Result<(
    WorkerLaunchConfig,
    Option<ProjectMemoryLaunchConfig>,
    String,
)> {
    worker_launch_config_with_checkout(
        session,
        &session.checkout(),
        profile,
        bundle,
        backend,
        worker_workspace,
        target,
    )
}

pub fn worker_launch_config_with_checkout(
    session: &mj_core::state::SessionRecord,
    checkout: &mj_core::state::Checkout<'_>,
    profile: &mj_core::config::HarnessProfile,
    bundle: Option<&ProjectBundle>,
    backend: &targets::TargetLocator,
    worker_workspace: LaunchWorkspace<'_>,
    target: &mj_core::state::TargetRuntimeSettings,
) -> Result<(
    WorkerLaunchConfig,
    Option<ProjectMemoryLaunchConfig>,
    String,
)> {
    let session_id = session.id.as_str();
    let execution_policy = target.execution_policy;
    let target_profile_home = target_profile_home(backend, session_id, profile);
    let workspace = if let Some(project_directory) = checkout.project_directory() {
        (project_directory.to_string_lossy().into_owned(), Vec::new())
    } else {
        workspace_paths(
            backend,
            bundle.context("session bundle is missing")?,
            worker_workspace.session_id,
            worker_workspace.container,
        )?
    };
    let mut additional_directories = workspace.1.iter().map(PathBuf::from).collect::<Vec<_>>();
    additional_directories.extend(
        mj_core::state::applied_container_settings(session)
            .additional_mounts
            .iter()
            .map(|resource| resource.destination.clone()),
    );
    if profile.kind == mj_core::config::HarnessKind::Muse && !additional_directories.is_empty() {
        bail!(
            "{} ACP does not support multiple workspace roots; use a single-repository bundle",
            profile.kind.display_name()
        );
    }
    let (bridge_command, bridge_args) = bridge_launch(profile.kind, execution_policy);
    let mut target_environment = target.environment.clone();
    let podman_container = backend.container_engine() == Some("podman");
    if podman_container {
        // Provisioning writes inherited settings to this file under the
        // container's configured home. A root Podman worker can have
        // HOME=/root, so the session Git config includes the file by path.
        target_environment.insert(
            mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH.into(),
            "/home/hel/.gitconfig".into(),
        );
    }
    // The turn bounds are read by the worker process, which re-execs with a
    // cleared environment, so a value set for the daemon cannot reach it by
    // inheritance. Carry the two knobs explicitly when the daemon was started
    // with them, so shortening a timeout for a test works on every target and
    // not only on the container targets that can set it in configuration.
    // `RUST_LOG` travels the same way and for the same reason: a worker that
    // has gone quiet is diagnosed from its own log, and the log level cannot
    // be raised after the fact on a worker that re-execs with a cleared
    // environment.
    for name in [
        "MJ_TURN_STALL_TIMEOUT_MS",
        "MJ_TURN_TOOL_STALL_TIMEOUT_MS",
        "RUST_LOG",
    ] {
        if let Ok(value) = std::env::var(name) {
            target_environment.insert(name.to_owned(), value);
        }
    }
    // The build cache reaches the harness, its terminals, and the reviewer
    // sidecar, all of which run Cargo through the mbx shim.
    if let Some(build_cache) = &session.build_cache {
        target_environment.insert(
            "MBX_CACHE_DIR".into(),
            build_cache.directory.to_string_lossy().into_owned(),
        );
        target_environment.insert(
            "MJ_MBX_CONFIG_DIR".into(),
            mj_core::config::build_cache_configuration_directory(&build_cache.directory)
                .to_string_lossy()
                .into_owned(),
        );
        // Compiler symlinks name this worker's private executable. Sharing
        // them lets another container replace them with an unreachable path.
        target_environment.insert(
            "MBX_SHIMS_DIR".into(),
            mj_core::target_path::text(&mj_core::target_path::join(
                Path::new(&targets::worker_root(backend, session_id)?),
                Path::new("mbx-shims"),
            )),
        );
        // The per-build summary and savings lines are for a human at a
        // terminal; in a harness session they only add noise to Cargo output.
        target_environment.insert("MBX_SUMMARY".into(), "off".into());
        target_environment.insert("MBX_SAVINGS".into(), "off".into());
    }
    let mut environment = target_environment.clone();
    environment.extend(profile.environment.resolved().clone());
    if let Some(source) = crate::aws_credentials::source(profile, backend, &target.environment)? {
        environment.remove("AWS_DEFAULT_PROFILE");
        environment.insert(
            "AWS_PROFILE".into(),
            source.environment["AWS_PROFILE"].clone(),
        );
        for (key, name) in [
            (
                "AWS_SHARED_CREDENTIALS_FILE",
                mj_core::aws_credentials::EMPTY_CREDENTIALS_FILE,
            ),
            ("AWS_CONFIG_FILE", mj_core::aws_credentials::CONFIG_FILE),
        ] {
            environment.insert(
                key.into(),
                mj_core::target_path::text(&mj_core::target_path::join(
                    Path::new(&target_profile_home),
                    Path::new(name),
                )),
            );
        }
    }
    // This ownership fact is generated by the controller, so a profile cannot
    // impersonate another session or turn a child into a top-level sender.
    environment.insert("MJ_SESSION_ID".into(), session_id.to_owned());
    if session
        .target
        .as_ref()
        .is_some_and(|target| target.skills_scope() == mj_core::skills::SkillsScope::Localhost)
    {
        // Harnesses clear their environment. Host CLI commands must still
        // address the daemon that created this session, including named instances.
        environment.insert(
            "MJ_CONFIG_DIR".into(),
            std::path::absolute(mj_core::config::config_dir())
                .context("resolve host Mjolnir configuration directory")?
                .to_string_lossy()
                .into_owned(),
        );
        environment.insert(
            "MJ_DATA_DIR".into(),
            std::path::absolute(data_dir())
                .context("resolve host Mjolnir data directory")?
                .to_string_lossy()
                .into_owned(),
        );
        if let Some(instance) = mj_core::config::instance_name() {
            environment.insert("MJ_INSTANCE".into(), instance);
        } else {
            environment.remove("MJ_INSTANCE");
        }
    }
    profile
        .kind
        .configure_staged_home_environment(Path::new(&target_profile_home), &mut environment);
    profile.configure_execution_environment(execution_policy, &mut environment)?;
    // The project's identity also keys the shared tool cache and names the
    // Claude project directory, so it is worked out for every session. Only
    // the memory launch config below depends on the session's choice.
    let mut project_memory = project_memory_launch(
        session,
        bundle,
        &workspace,
        &target_profile_home,
        worker_workspace.parent_worktree,
    )?;
    project_memory.mcp_delivery = project_memory_mcp_delivery(profile.kind, backend);
    project_memory.history_socket = Some(mj_core::target_path::join(
        Path::new(&targets::worker_root(backend, &session.id)?),
        Path::new("control.sock"),
    ));
    if let Some(cache) = &target.tool_cache {
        let private = mj_core::target_path::join(
            Path::new(&targets::worker_root(backend, session_id)?),
            Path::new("tool-cache"),
        );
        for (name, value) in [
            (
                "MJ_TOOL_CACHE_DIR",
                cache.directory.to_string_lossy().into_owned(),
            ),
            ("MJ_TOOL_CACHE_PROJECT", project_memory.project_key.clone()),
            ("MJ_TOOL_CACHE_HOME", mj_core::target_path::text(&private)),
            (
                "MJ_NX_HOME_DIR",
                cache.nx_home.to_string_lossy().into_owned(),
            ),
        ] {
            target_environment.insert(name.into(), value.clone());
            environment.insert(name.into(), value);
        }
    }
    if profile.kind == mj_core::config::HarnessKind::Claude {
        environment.insert(
            "CLAUDE_CODE_PROJECT_DIR_NAME".into(),
            project_memory_replica_slug(&project_memory.project_key, session_id),
        );
        if session.no_project_memory {
            // Claude Code reads and offers its auto-memory directory unless
            // told not to. Set after the profile's own environment so the
            // session's choice wins.
            environment.insert(CLAUDE_DISABLE_AUTO_MEMORY_ENV.into(), "1".into());
        }
    }
    let project_memory = (!session.no_project_memory).then_some(project_memory);
    let _ = apply_claude_setup_token(
        &mut environment,
        profile.kind,
        &mj_core::credentials::claude_oauth_token_path(&session.last_profile),
    );
    let excluded_environment =
        exclude_harness_environment(&session.last_profile, profile, &mut environment);
    if session.no_github_credentials {
        mj_core::credentials::remove_github_authentication_environment(&mut environment);
        mj_core::credentials::remove_github_authentication_environment(&mut target_environment);
    }
    if backend.container_engine().is_some() && profile.kind == mj_core::config::HarnessKind::Claude
    {
        // Claude Code rejects bypassPermissions under uid 0 unless it is
        // told the process is already running in an isolated sandbox. Every
        // container engine can run the session as root: Podman always does,
        // and Docker, Apple container and Nitro do whenever the image's user
        // is root, as on the default buildpack-deps image.
        environment.insert("IS_SANDBOX".into(), "1".into());
    }
    // Placement owns this directive; profile/target environment cannot enable
    // system-package installation on a bare target.
    environment.remove(mj_core::worker_launch::SESSION_PROJECT_TOOLS_ROOTS);
    target_environment.remove(mj_core::worker_launch::SESSION_PROJECT_TOOLS_ROOTS);
    if backend.container_engine().is_some() {
        let roots = std::iter::once(&workspace.0)
            .chain(workspace.1.iter())
            .collect::<Vec<_>>();
        environment.insert(
            mj_core::worker_launch::SESSION_PROJECT_TOOLS_ROOTS.into(),
            serde_json::to_string(&roots)?,
        );
    }
    let approval_choices = profile.approval_choices();
    Ok((
        WorkerLaunchConfig {
            cpu_accounting: backend.cpu_accounting(),
            no_github_credentials: session.no_github_credentials,
            oracle: false,
            initial_commands: Vec::new(),
            reviewer_launch: None,
            goal_resume_request: None,
            target_environment,
            seed_image_environment: worker_runs_in_image(backend),
            run_mode: Default::default(),
            session_id: session_id.to_string(),
            subagents: mj_core::subagent::SubagentPolicy::Native,
            handback_tool: false,
            subagent_kind: None,
            agent_mailboxes_enabled: true,
            subtle_subagents: true,
            initial_model: None,
            review_capture: false,
            review_trigger: Default::default(),
            harness: profile.kind,
            harness_home: PathBuf::from(&target_profile_home),
            // The staged home mirrors the profile home, so the marker's path
            // within the profile home is the one the worker must check and
            // the credential sync must write. Kimi's is nested
            // (`credentials/kimi-code.json`); its file name alone named a file
            // at the top of the staged home that Kimi never reads.
            authentication_marker: profile
                .authentication_marker()
                .strip_prefix(&profile.home)
                .ok()
                .map(|name| name.to_string_lossy().into_owned()),
            bridge_command: PathBuf::from(bridge_command),
            bridge_args,
            harness_runtime: harness_runtime_policy(backend),
            environment,
            excluded_environment,
            cwd: PathBuf::from(&workspace.0),
            additional_directories,
            native_session_id: session.native_session_id.clone(),
            project_memory: project_memory.clone(),
            execution_policy,
            claude_permission_mode: approval_choices.claude_permission_mode,
            mjolnir_guardian: approval_choices.mjolnir_guardian,
            observe_policy: None,
        },
        project_memory,
        target_profile_home,
    ))
}

/// Leave out of a launch every variable the profile's harness must never see,
/// and name them for the worker, which removes them again once it has added
/// the target's own login environment.
///
/// Saying so once per profile is enough: the launch config is rebuilt for
/// every recovery check, and the variables do not change between them.
pub fn exclude_harness_environment(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    environment: &mut std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    static REPORTED: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let before = environment.clone();
    let excluded = profile.exclude_harness_environment(environment);
    let removed = excluded
        .iter()
        .filter(|name| before.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !removed.is_empty()
        && REPORTED
            .lock()
            .map(|mut reported| reported.insert(profile_id.to_owned()))
            .unwrap_or(true)
    {
        tracing::info!(
            profile_id,
            removed = removed.join(", "),
            "left API key settings out of the harness environment: this Codex profile must not use an OpenAI API key"
        );
    }
    excluded
}

/// Where a worker on `backend` gets its harness. Local work follows
/// [`mj_core::placement::local_harness_runtime`]: the image's harnesses in the
/// daemon container, the managed installation on a host.
pub fn harness_runtime_policy(backend: &targets::TargetLocator) -> HarnessRuntimePolicy {
    match backend {
        targets::TargetLocator::LocalBare { .. } => mj_core::placement::local_harness_runtime(),
        targets::TargetLocator::AwsEc2 { .. } | targets::TargetLocator::SshBare { .. } => {
            HarnessRuntimePolicy::Managed
        }
        _ => HarnessRuntimePolicy::Ambient,
    }
}

/// Whether the worker's own environment is an image's: a container target, or
/// a local worker beside a daemon in its container.
pub fn worker_runs_in_image(backend: &targets::TargetLocator) -> bool {
    backend.container_engine().is_some()
        || harness_runtime_policy(backend) == HarnessRuntimePolicy::Image
}
