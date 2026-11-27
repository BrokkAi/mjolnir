//! Launching a worker: what a session's worker is given to run with, and the
//! staging that puts it on the target, from the configuration and the records.

use mj_worker_staging::worker_binary::launch::*;
use mj_worker_staging::worker_binary::*;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::targets::{self, CommandExecutor, backend_locator, target_profile_home};
use mj_core::aws_credentials::AwsCredentialSource;
use mj_core::config::Config;
use mj_core::state::{SessionRecord, State};
use mj_core::targets::SshTrafficClass;
use mj_core::worker_build::worker_build_id;
use mj_core::worker_launch::ReviewMcpServer;
use mj_core::worker_launch::{
    ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery, WorkerLaunchConfig, WorkerOwnership,
};
use mj_session_types::{ProjectMemorySyncTarget, WorkerRecoveryPlan};
use mj_targets::command_checks::execute_checked;

pub fn aws_credential_source(
    config: &Config,
    state: &State,
    session_id: &str,
) -> Result<Option<mj_core::aws_credentials::AwsCredentialSource>> {
    let (profile, backend, environment) = aws_session_inputs(config, state, session_id)?;
    crate::aws_credentials::source(profile, &backend, &environment)
}

/// [`aws_credential_source`] for the credential sync: the same for a
/// profile without a role, and for one with a role every session that may
/// hold Mjolnir's credentials stays managed (see
/// [`crate::aws_credentials::sync_source`]).
pub fn aws_credential_sync_source(
    config: &Config,
    state: &State,
    session_id: &str,
) -> Result<Option<mj_core::aws_credentials::AwsCredentialSource>> {
    let (profile, backend, environment) = aws_session_inputs(config, state, session_id)?;
    crate::aws_credentials::sync_source(
        profile,
        &backend,
        &environment,
        state.sessions[session_id].staged_aws_profile.as_deref(),
    )
}

/// A notice for the person when the profile's `[profiles.<id>.aws]` role
/// cannot apply to this session, and why: its target is raw local or EC2,
/// or sets its own AWS credentials, or the profile is not a Bedrock one.
pub fn aws_role_notice(config: &Config, state: &State, session_id: &str) -> Result<Option<String>> {
    let (profile, backend, environment) = aws_session_inputs(config, state, session_id)?;
    let profile_id = &state.sessions[session_id].last_profile;
    Ok(crate::aws_credentials::unused_role_notice(
        profile_id,
        profile,
        crate::aws_credentials::SessionHost::from(&backend),
        &environment,
    ))
}

/// What decides whether this session receives synced AWS credentials.
pub fn aws_session_inputs<'a>(
    config: &'a Config,
    state: &State,
    session_id: &str,
) -> Result<(
    &'a mj_core::config::HarnessProfile,
    targets::TargetLocator,
    std::collections::BTreeMap<String, String>,
)> {
    let session = state
        .sessions
        .get(session_id)
        .context("session is missing")?;
    let profile = config
        .profiles
        .get(&session.last_profile)
        .context("session profile is missing")?;
    let locator = session
        .target
        .as_ref()
        .context("session target is missing")?;
    let backend = backend_locator(locator, session, config)?;
    let runtime = session.target_runtime_settings(config)?;
    Ok((profile, backend, runtime.launch_environment()?))
}

/// Where this session's worker lives. This is decided from the session
/// record and configuration alone, so a caller can name the worker root
/// before anything is installed into it.
pub fn worker_placement(
    config: &Config,
    state: &State,
    session_id: &str,
) -> Result<(targets::TargetLocator, String)> {
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let locator = session
        .target
        .as_ref()
        .context("session target is missing")?;
    if let Some(owner) = crate::worker_lifecycle::current(session_id) {
        owner.verify_target(locator)?;
    }
    let backend = backend_locator(locator, session, config)?;
    let worker_root = targets::worker_root(&backend, session_id)?;
    Ok((backend, worker_root))
}

pub struct StagedWorkerProfile {
    pub login: Option<mj_core::credentials::LoginFingerprint>,
    pub aws_profile: Option<String>,
}

pub fn prepare_worker_files(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
    history: LaunchHistory<'_>,
) -> Result<Option<String>> {
    prepare_worker_files_for(
        config,
        state,
        session_id,
        backend,
        worker_root,
        executor,
        WorkerFilePreparation::Install,
        history,
    )
    .map(|profile| profile.aws_profile)
}

// `config` and `state` stand in for the `Controller` this was a method on.
#[allow(clippy::too_many_arguments)]
pub fn prepare_worker_files_for(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
    preparation: WorkerFilePreparation,
    history: LaunchHistory<'_>,
) -> Result<StagedWorkerProfile> {
    let candidate = matches!(&preparation, WorkerFilePreparation::StageCandidate { .. });
    let (launch, staged_profile) = stage_worker_files_for(
        config,
        state,
        session_id,
        backend,
        worker_root,
        executor,
        preparation,
        history,
        None,
    )?;
    if let Some(launch) = launch
        && !candidate
    {
        prepare_installed_managed_harness(executor, backend, worker_root, &launch)?;
    }
    Ok(staged_profile)
}

/// Return the pending harness preflight so a new checkout can finish first,
/// and the AWS profile and login identity staged for the worker.
// `config` and `state` stand in for the `Controller` this was a method on.
#[allow(clippy::too_many_arguments)]
pub fn stage_worker_files_for(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
    preparation: WorkerFilePreparation,
    history: LaunchHistory<'_>,
    created_container: Option<&targets::CreationObservation>,
) -> Result<(Option<WorkerLaunchConfig>, StagedWorkerProfile)> {
    let _timing = crate::launch_timing::LaunchPhaseTimer::new(session_id, "prepare worker files");
    let owner = crate::worker_lifecycle::require(session_id)?;
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let target = session
        .target
        .as_ref()
        .context("worker preparation has no committed placement")?;
    owner.verify_target(target)?;
    crate::database::admit_environment_worker(session_id, target)?;
    // A busy worker can outlive a daemon upgrade. Reusing its history
    // does not imply that its executable can read this launch format.
    let upgrade_retained_worker = if preparation == WorkerFilePreparation::RefreshProfile {
        let executable = mj_core::target_path::join(Path::new(worker_root), Path::new("hel"));
        let output = execute_checked(
            executor,
            targets::command_on_locator(
                SshTrafficClass::Control,
                backend,
                session_id,
                vec![
                    mj_core::target_path::text(&executable),
                    "worker".into(),
                    "build-id".into(),
                ],
                "read retained worker build",
            )?,
        )?;
        let build = std::str::from_utf8(&output.stdout)
            .context("decode retained worker build")?
            .trim();
        ensure!(
            mj_core::worker_build::is_worker_build(build),
            "retained worker returned an invalid build identity"
        );
        build != worker_build_id()
    } else {
        false
    };
    let profile = config
        .profiles
        .get(&session.last_profile)
        .context("session profile is missing")?;
    profile.ensure_ready(&session.last_profile)?;
    let target_environment = session
        .target_runtime_settings(config)?
        .launch_environment()?;
    let aws_source = crate::aws_credentials::source(profile, backend, &target_environment)?;
    let (mut launch, _project_memory, target_profile_home) = session_launch_config_with_source(
        config,
        state,
        session_id,
        backend,
        history,
        &target_environment,
        aws_source.as_ref(),
    )?;

    if session.native_session_id.is_some() && profile.kind == mj_core::config::HarnessKind::Codex {
        launch.goal_resume_request = Some(mj_core::state::new_session_id()?);
    }
    if let WorkerFilePreparation::StageCandidate {
        operation_id,
        candidate_id,
        ..
    } = &preparation
    {
        mj_core::worker_launch::validate_candidate_id(candidate_id)?;
        launch.accepted_reload_operation_id = Some(operation_id.clone());
    }
    let staging = tempfile::tempdir().context("create worker staging directory")?;
    let seed_stage = staging.path().join("project-memory-seed");
    if let Some(memory) = &launch.project_memory {
        stage_memory_seed(memory, &seed_stage)?;
    }
    let project_memory = launch.project_memory.clone();
    let launch_path = staging.path().join("launch.json");
    launch.write(&launch_path)?;
    let ownership_path = staging.path().join("ownership.json");
    WorkerOwnership {
        version: WorkerOwnership::VERSION,
        no_github_credentials: session.no_github_credentials,
        workspace_id: session.workspace_id.clone(),
        session_id: session_id.to_string(),
        profile_id: session.last_profile.clone(),
        bundle_id: session.bundle_id.clone(),
        target_template_id: session.target_template_id.clone(),
        instance_id: Some(crate::database::ownership_identity()?.as_str().to_owned()),
    }
    .write(&ownership_path)?;
    // Every session runs from a staged copy of its profile, on every target,
    // so `target_profile_home` is always a home the session owns and the
    // stage is always installed there.
    let profile_stage = staging.path().join("profile");
    let staging_timing =
        crate::launch_timing::LaunchPhaseTimer::new(session_id, "stage harness profile");
    let result = stage_profile_with_catalog(
        &session.last_profile,
        profile,
        &profile_stage,
        &fetch_catalog_over_https,
        &SharedCatalogCache,
    );
    drop(staging_timing);
    result?;
    let mut staged_profile = StagedWorkerProfile {
        login: None,
        aws_profile: None,
    };
    if let Some(source) = &aws_source {
        let credentials = crate::aws_assume_role::obtain(session_id, source, executor)?;
        credentials.install(&profile_stage, Path::new(&target_profile_home))?;
        staged_profile.aws_profile = Some(credentials.profile().to_owned());
    } else if let Some(notice) = crate::aws_credentials::unused_role_notice(
        &session.last_profile,
        profile,
        crate::aws_credentials::SessionHost::from(backend),
        &target_environment,
    ) {
        executor.notify_notice(&notice);
    }
    stage_claude_keychain_login(
        profile,
        mj_core::config::HarnessHost::current(),
        &profile_stage,
        &launch.environment,
        executor,
    )?;
    stage_managed_skills(
        profile.kind,
        &profile_stage,
        session
            .target
            .as_ref()
            .context("session target is missing")?
            .skills_scope(),
    )?;
    append_hel_target_environment(profile.kind, &profile_stage, backend)?;
    // Edits to attachment settings apply on the next container creation;
    // describe this container's creation snapshot while it is retained.
    let attachments = mj_core::state::applied_container_settings(session).additional_mounts;
    let storage = targets::inspect_container_storage_observed(
        backend,
        attachments,
        created_container,
        executor,
    )?;
    append_container_storage_guidance(
        profile.kind,
        &profile_stage,
        backend,
        session.container_workspace.as_deref(),
        storage.managed_temporary_volume,
        &storage.attachments,
    )?;
    append_session_state_instructions(profile.kind, &profile_stage)?;
    append_subagent_policy(
        profile.kind,
        &profile_stage,
        &launch.subagents,
        config.subagents.max_concurrent,
        launch.subtle_subagents,
    )?;
    apply_staged_execution_setting(profile.kind, launch.execution_policy, &profile_stage)?;
    if let Some(reviewer) = &launch.reviewer_launch
        && mj_core::worker_launch::ReviewMcpDelivery::for_harness(profile.kind)
            == mj_core::worker_launch::ReviewMcpDelivery::HarnessProfile
    {
        configure_staged_review_mcp(profile.kind, &profile_stage, &reviewer.mcp_servers)?;
    }
    // The `mj-agents` registration is not staged here: the worker writes
    // it on every start, because an upgrade or recovery replaces the
    // launch configuration without restaging this profile.
    if profile.kind == mj_core::config::HarnessKind::Claude {
        configure_claude_mailbox_hook(&profile_stage, worker_root)?;
    }
    let mut shared_paths: Vec<&Path> = shared_profile_paths(profile.kind);
    if let (
        WorkerFilePreparation::StageCandidate {
            worker_protocol, ..
        },
        Some(memory),
    ) = (&preparation, &project_memory)
        && *worker_protocol < mj_core::relay::RELAY_PROJECT_MEMORY_SEED_PROTOCOL
    {
        shared_paths.extend(stage_legacy_candidate_memory(
            memory,
            Path::new(&target_profile_home),
            &profile_stage,
            &seed_stage,
        )?);
    }
    // A session without project memory gets no replica, no baseline and
    // no history server: nothing of the workspace's memory reaches it.
    if let Some(project_memory) = &project_memory
        && project_memory.mcp_delivery == ProjectMemoryMcpDelivery::HarnessProfile
    {
        configure_kimi_history_mcp(
            &profile_stage,
            worker_root,
            project_memory.history_socket.as_deref(),
        )?;
    }
    if mj_core::session_instructions::delivered_by_staging(profile.kind) {
        let index = if let Some(memory) = &project_memory {
            let index = staged_memory_index(&seed_stage)
                .map(|contents| contents.unwrap_or_default())
                .map_err(|error| format!("{error:#}"));
            if let Err(reason) = &index {
                executor.notify_notice(&format!(
                    "Warning: Project memory index {} unavailable: {reason}",
                    memory
                        .root
                        .join(mj_core::project_memory::MEMORY_INDEX)
                        .display()
                ));
            }
            index
        } else {
            Ok(String::new())
        };
        append_composed_session_instructions(
            &launch,
            &profile_stage,
            index.as_deref().map_err(String::as_str),
        )?;
    }
    // The notes above can make instructions the profile alone could
    // deliver too long, so the staged file is checked before it travels.
    profile.ensure_staged_instructions_deliverable(&session.last_profile, &profile_stage)?;
    let shared_login = shared_profile_login(profile, backend);
    if let WorkerFilePreparation::StageCandidate {
        operation_id,
        expected_store_id,
        candidate_id,
        worker_protocol,
    } = &preparation
    {
        use mj_core::worker_launch::{ReloadCandidateManifest, ReloadSharedLogin};
        // In-place Move reuses the running environment's mbx binary/config/shims.
        // Phase B's link_build_cache_configuration links the destination XDG config after commit.
        let (clear_paths, protected_paths) = profile_clear_spec(profile.kind);
        let manifest = ReloadCandidateManifest {
            operation_id: operation_id.clone(),
            expected_store_id: expected_store_id.clone(),
            candidate_id: candidate_id.clone(),
            harness: profile.kind,
            target_profile_home: target_profile_home.clone(),
            clear_paths,
            protected_paths,
            shared_login: shared_login.as_ref().map(|login| ReloadSharedLogin {
                relative_marker: login.name.to_owned(),
                source_path: login.file.to_string_lossy().into_owned(),
            }),
            clear_github_token: session.no_github_credentials,
        };
        let manifest_path = staging.path().join("manifest.json");
        std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        install_reload_candidate(
            executor,
            backend,
            session_id,
            worker_root,
            candidate_id,
            &manifest_path,
            mj_worker_staging::worker_binary::StagedWorkerProfile {
                launch: &launch_path,
                ownership: &ownership_path,
                profile: &profile_stage,
                shared_login: None,
                shared_paths: &shared_paths,
            },
        )?;
        if *worker_protocol >= mj_core::relay::RELAY_PROJECT_MEMORY_SEED_PROTOCOL
            && let Some(memory) = &project_memory
        {
            install_memory_seed(
                executor,
                backend,
                session_id,
                &seed_stage,
                &mj_core::worker_launch::project_memory_seed_root(&memory.baseline_root),
            )?;
        }
        return Ok((Some(launch), staged_profile));
    }
    if preparation != WorkerFilePreparation::Install {
        // A shared symlink stays live across boot. Only a copied login
        // identifies the credentials staged for this specific launch.
        staged_profile.login = if preparation == WorkerFilePreparation::RefreshProfileOnly
            && profile.auth_scheme().uses_native_login_file()
            && shared_login.is_none()
        {
            mj_core::credentials::LoginFingerprint::read_file(
                profile.kind,
                &mj_core::config::harness_authentication_marker(profile.kind, &profile_stage),
            )?
        } else {
            None
        };
        // The profile install below also replaces `launch.json`, whose
        // `MJ_MBX_CONFIG_DIR` names this session's private policy snapshot.
        // Stage it, and the mbx the launchers run, before the file names them.
        install_build_cache_shim(session, backend, &launch, executor)
            .context("stage the session's mbx and configuration")?;
        if preparation == WorkerFilePreparation::RefreshProfileOnly {
            unlink_host_bare_profile_home(executor, backend, session_id, &target_profile_home)?;
        }
        clear_profile_configuration(
            executor,
            backend,
            session_id,
            &target_profile_home,
            profile.kind,
        )?;
        install_worker_profile_files(
            executor,
            backend,
            session_id,
            worker_root,
            &target_profile_home,
            mj_worker_staging::worker_binary::StagedWorkerProfile {
                launch: &launch_path,
                ownership: &ownership_path,
                profile: &profile_stage,
                shared_login: shared_login.as_ref(),
                shared_paths: &shared_paths,
            },
        )?;
        if let Some(memory) = &project_memory {
            install_memory_seed(
                executor,
                backend,
                session_id,
                &seed_stage,
                &mj_core::worker_launch::project_memory_seed_root(&memory.baseline_root),
            )?;
        }
        if upgrade_retained_worker {
            let worker = WorkerInstall::for_target(backend, executor)?;
            install_worker_binary(executor, backend, session_id, &worker, "hel")?;
            return Ok((Some(launch), staged_profile));
        }
        return Ok((None, staged_profile));
    }
    let worker = WorkerInstall::for_target(backend, executor)?;
    install_worker_files(
        executor,
        backend,
        session_id,
        worker_root,
        &target_profile_home,
        &worker,
        &launch_path,
        &ownership_path,
        &profile_stage,
        shared_login.as_ref(),
        &shared_paths,
    )?;
    if let Some(memory) = &project_memory {
        install_memory_seed(
            executor,
            backend,
            session_id,
            &seed_stage,
            &mj_core::worker_launch::project_memory_seed_root(&memory.baseline_root),
        )?;
    }
    if session.build_cache.is_some() {
        install_build_cache_shim(session, backend, &launch, executor)
            .context("install shared machine build cache configuration")?;
    }
    Ok((Some(launch), staged_profile))
}

// Remove this compatibility path once workers older than seed installation are gone.
fn stage_legacy_candidate_memory<'a>(
    memory: &'a ProjectMemoryLaunchConfig,
    target_profile_home: &Path,
    profile_stage: &Path,
    seed: &Path,
) -> Result<[&'a Path; 2]> {
    let replica = memory.root.strip_prefix(target_profile_home)?;
    let baseline = memory.baseline_root.strip_prefix(target_profile_home)?;
    copy_profile_entry(seed, &profile_stage.join(replica))?;
    copy_profile_entry(seed, &profile_stage.join(baseline))?;
    Ok([replica, baseline])
}

/// Stage and verify the session's mbx, then install marked launchers in
/// the session's `bin` directory on the worker PATH.
pub fn install_build_cache_shim(
    session: &mj_core::state::SessionRecord,
    backend: &targets::TargetLocator,
    launch: &WorkerLaunchConfig,
    executor: &impl CommandExecutor,
) -> Result<()> {
    if session.build_cache.is_none() {
        return Ok(());
    }
    let worker_root = targets::worker_root(backend, &session.id)?;
    if matches!(backend, targets::TargetLocator::SshNitro { .. }) {
        let _timing = crate::launch_timing::LaunchPhaseTimer::new(&session.id, "stage Nitro mbx");
        let staged = (|| {
            let platform = target_platform(backend, executor)?;
            ensure!(
                platform.os == targets::TargetOs::Linux,
                "Nitro guests require Linux mbx"
            );
            let artifact =
                mj_build_cache::mbx::install::pinned_linux_binary(platform.architecture, executor)?;
            let binary =
                install_session_executable(executor, backend, &session.id, &artifact, "mbx")?;
            verify_mbx_binary(executor, backend, &binary, mj_build_cache::mbx::MBX_VERSION)?;
            Ok::<_, anyhow::Error>(binary)
        })();
        return match staged {
            Ok(binary) => install_mbx_shims(executor, backend, &worker_root, &binary),
            Err(error) => {
                disable_mbx_cache_shims(
                    &session.id,
                    backend,
                    &worker_root,
                    executor,
                    &format!("{error:#}"),
                );
                Ok(())
            }
        };
    }
    let cache = session
        .build_cache
        .as_ref()
        .expect("checked above that the session has a build cache");
    let config_roots = build_cache_config_roots(launch);
    // The build cache is optional: whatever keeps the host's policy or
    // executable from reaching the container turns the cache off for this
    // session, with a notice, instead of blocking the session from starting.
    if matches!(
        backend,
        targets::TargetLocator::SshPodman { .. } | targets::TargetLocator::SshDocker { .. }
    ) {
        // Over SSH, every command is a round trip: read the host's policy and
        // mbx in one, and stage everything in the container with one more.
        let staged = (|| -> Result<()> {
            let (configuration, native) =
                mj_build_cache::mbx::native_mbx_and_config_for_container(backend, cache, executor)
                    .context("read the container host's mbx configuration")?;
            let host_mbx = match &native {
                mj_build_cache::mbx::NativeMbxLookup::Ready(native) => Some(HostMbx {
                    program: &native.program,
                    version: &native.version,
                }),
                mj_build_cache::mbx::NativeMbxLookup::Unavailable(_) => None,
            };
            match stage_ssh_container_mbx(
                executor,
                backend,
                &worker_root,
                &configuration,
                &config_roots,
                host_mbx,
            )? {
                // An older private mbx keeps its executable and now has its policy.
                SshContainerMbx::Legacy | SshContainerMbx::Installed(_) => Ok(()),
                SshContainerMbx::HostMbxUnavailable => match native {
                    mj_build_cache::mbx::NativeMbxLookup::Unavailable(reason) => bail!("{reason}"),
                    mj_build_cache::mbx::NativeMbxLookup::Ready(_) => {
                        bail!("staging found no host mbx although one was supplied")
                    }
                },
            }
        })();
        if let Err(error) = staged {
            disable_mbx_cache_shims(
                &session.id,
                backend,
                &worker_root,
                executor,
                &format!("{error:#}"),
            );
        }
        return Ok(());
    }
    let staged = (|| -> Result<Option<PathBuf>> {
        // The policy snapshot goes first: an older private mbx, which keeps its
        // own executable, is linked to it below.
        let configuration =
            mj_build_cache::mbx::native_config_for_container(backend, cache, executor)
                .context("read the container host's mbx configuration")?;
        let configuration_directory =
            stage_mbx_configuration(executor, backend, &configuration, &config_roots)?;
        if link_legacy_mbx_configuration(
            executor,
            backend,
            &worker_root,
            &configuration_directory,
            &config_roots,
        )? {
            return Ok(None);
        }
        let native = match mj_build_cache::mbx::native_mbx_for_container(backend, executor)? {
            mj_build_cache::mbx::NativeMbxLookup::Ready(native) => native,
            mj_build_cache::mbx::NativeMbxLookup::Unavailable(reason) => bail!("{reason}"),
        };
        stage_container_mbx(
            executor,
            backend,
            &worker_root,
            &native.program,
            &native.version,
        )
        .map(Some)
    })();
    match staged {
        Ok(Some(binary)) => {
            install_mbx_shims(executor, backend, &worker_root, &binary).or_else(|error| {
                disable_mbx_cache_shims(
                    &session.id,
                    backend,
                    &worker_root,
                    executor,
                    &format!("{error:#}"),
                );
                Ok(())
            })
        }
        // An older private mbx keeps its executable and now has its policy.
        Ok(None) => Ok(()),
        Err(error) => {
            disable_mbx_cache_shims(
                &session.id,
                backend,
                &worker_root,
                executor,
                &format!("{error:#}"),
            );
            Ok(())
        }
    }
}

pub fn prepare_build_cache_links(
    session: &mj_core::state::SessionRecord,
    backend: &targets::TargetLocator,
    launch: &WorkerLaunchConfig,
    executor: &impl CommandExecutor,
) -> Result<()> {
    install_build_cache_shim(session, backend, launch, executor)
}

/// Probe the installed binary and the worker's recorded state after a
/// session becomes unreachable. Returns `None` only when the session has
/// no target to probe; a probe that fails says why.
pub fn diagnose_worker(config: &Config, state: &State, session_id: &str) -> Option<String> {
    diagnose_worker_controlled(config, state, session_id, &crate::targets::ProcessExecutor)
}

pub fn diagnose_worker_controlled(
    config: &Config,
    state: &State,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Option<String> {
    let session = state.sessions.get(session_id)?;
    let locator = session.target.as_ref()?;
    let backend = match backend_locator(locator, session, config) {
        Ok(backend) => backend,
        Err(error) => {
            tracing::debug!(
                session_id,
                error = format!("{error:#}"),
                "could not construct a worker diagnostic probe"
            );
            return None;
        }
    };
    let worker_root = match targets::worker_root(&backend, session_id) {
        Ok(root) => root,
        Err(error) => {
            tracing::debug!(
                session_id,
                error = format!("{error:#}"),
                "could not derive the worker diagnostic root"
            );
            return None;
        }
    };
    let binary_failure = worker_binary_probe_failure(executor, &backend, &worker_root);
    let probe = match diagnose_worker_read_only(executor, &backend, &worker_root) {
        Ok(probe) => probe.to_string(),
        Err(error) => format!("the worker could not be probed: {error:#}"),
    };
    Some(match binary_failure {
        Some(binary_failure) => format!("{binary_failure}; {probe}"),
        None => probe,
    })
}

/// A non-destructive liveness probe plus commands that replace a confirmed
/// dead session worker without touching its durable relay files. The
/// session manager runs both off its async actor.
pub fn worker_recovery_plan(
    config: &Config,
    state: &State,
    session_id: &str,
    operation: Option<&mj_core::state::MoveOperation>,
) -> Result<WorkerRecoveryPlan> {
    let (backend, worker_root) = worker_placement(config, state, session_id)?;
    let launch = worker_launch_config_for_move(config, state, session_id, &backend, operation)?;
    let workspace = worker_workspace_for_recovery(&backend, &launch.cwd);
    Ok(WorkerRecoveryPlan {
        source_target: *state.sessions[session_id]
            .target
            .clone()
            .context("session target is missing")?,
        target: targets::target_recovery_plan(&backend, session_id)?,
        workspace,
        liveness_probe: targets::WorkerControlPlan::new(
            format!("probe the worker of session {session_id}"),
            vec![worker_liveness_command(&backend, &worker_root)],
            &backend,
            session_id,
        )?,
        exit_record: Some(worker_exit_record_command(&backend, &worker_root)),
        binary_refresh: worker_binary_refresh_plan(&backend, session_id)?,
        launch_refresh: Some(worker_launch_refresh_plan(&backend, session_id, &launch)?),
        restart: targets::WorkerControlPlan::new(
            format!("restart Mjolnir worker for session {session_id}"),
            vec![
                stop_worker_command(&backend, &worker_root),
                start_worker_command(&backend, &worker_root).into(),
            ],
            &backend,
            session_id,
        )?,
    })
}

/// The launch config for a session, including what depends on its
/// sub-agent role. The first launch and every relaunch use this, so a
/// relaunched worker keeps its delegation tools and a relaunched child
/// keeps its parent's workspace.
pub fn session_launch_config(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    history: LaunchHistory<'_>,
) -> Result<(
    WorkerLaunchConfig,
    Option<ProjectMemoryLaunchConfig>,
    String,
)> {
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let profile = config
        .profiles
        .get(&session.last_profile)
        .context("session profile is missing")?;
    let target_environment = session
        .target_runtime_settings(config)?
        .launch_environment()?;
    let aws_source = crate::aws_credentials::source_for_staged_profile(
        profile,
        backend,
        &target_environment,
        session.staged_aws_profile.as_deref(),
    )?;
    session_launch_config_with_source(
        config,
        state,
        session_id,
        backend,
        history,
        &target_environment,
        aws_source.as_ref(),
    )
}

fn session_launch_config_with_source(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    history: LaunchHistory<'_>,
    // The target environment resolved once for this launch.
    target_environment: &std::collections::BTreeMap<String, String>,
    aws_source: Option<&AwsCredentialSource>,
) -> Result<(
    WorkerLaunchConfig,
    Option<ProjectMemoryLaunchConfig>,
    String,
)> {
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let profile = config
        .profiles
        .get(&session.last_profile)
        .context("session profile is missing")?;
    let owner = state.workspace_owner(session_id)?;
    let checkout = owner.checkout();
    session.validate_configuration_in(&checkout, config)?;
    let bundle = checkout
        .project_directory()
        .is_none()
        .then(|| owner.project_bundle(config))
        .flatten();
    let target = session.target_runtime_settings(config)?;
    let subagent = state.subagents.get(session_id);
    let mut working_directories = Vec::new();
    let mut cursor = session_id;
    let mut visited = std::collections::BTreeSet::new();
    while cursor != owner.id {
        ensure!(
            visited.insert(cursor),
            "cycle in borrowed workspace ancestry"
        );
        let child = state
            .subagents
            .get(cursor)
            .context("borrowed workspace is missing its parent relation")?;
        if !child.working_directory.as_os_str().is_empty() {
            working_directories.push(child.working_directory.as_path());
        }
        cursor = &child.parent_session_id;
        ensure!(
            state.environment_owner_id(cursor) == Some(owner.id.as_str()),
            "borrowed workspace parent is outside its environment"
        );
    }
    working_directories.reverse();
    let pinned_memory_key = pinned_legacy_memory_key(session, owner)?;
    let (mut launch, project_memory, target_profile_home) = worker_launch_config_with_checkout(
        session,
        &checkout,
        profile,
        bundle,
        backend,
        LaunchWorkspace {
            session_id: &owner.id,
            container: owner.container_workspace.as_deref(),
            parent_worktree: owner.managed_worktree.as_deref(),
            owner: Some(owner),
            working_directories: &working_directories,
        },
        &target,
        target_environment.clone(),
        config,
        aws_source,
        pinned_memory_key.as_deref(),
    )?;
    launch.reviewer_launch = if subagent.is_some() {
        crate::database::load_subagent(session_id)?
            .context("sub-agent launch record is missing")?
            .reviewer_launch
    } else {
        None
    };
    if let Some(reviewer) = &launch.reviewer_launch {
        // The record's own bridge command is the script this build's daemon
        // generated when the review started. Ask the generator again: a
        // script saved before a harness pin bump names the old pinned
        // version, and a worker treats an unrecognized one as a custom
        // command and runs it as given.
        let (bridge_command, bridge_args) =
            mj_core::harness_runtime::fallback_launch(reviewer.harness, reviewer.execution_policy);
        launch.bridge_command = bridge_command.into();
        launch.bridge_args = bridge_args;
        launch.execution_policy = reviewer.execution_policy;
        if let Some(source) = aws_source {
            apply_aws_launch_environment(&mut launch.environment, &target_profile_home, source);
        }
        launch.excluded_environment = reviewer.excluded_environment.clone();
    }
    // Read by the target-side MCP server from launch.json beside its socket,
    // independently of how each harness starts or registers that server.
    for name in [
        mj_core::subagent::CHILD_REPORT_DIR_ENV,
        mj_core::subagent::CHILD_REPORT_LIMIT_ENV,
    ] {
        launch.environment.remove(name);
    }
    if subagent.is_some()
        && let Some(report_dir) = crate::database::load_subagent_report(session_id)?.report_dir
    {
        launch
            .environment
            .insert(mj_core::subagent::CHILD_REPORT_DIR_ENV.into(), report_dir);
        launch.environment.insert(
            mj_core::subagent::CHILD_REPORT_LIMIT_ENV.into(),
            mj_core::subagent::max_child_report_bytes(config.subagents.max_concurrent).to_string(),
        );
    }
    if subagent.is_some() {
        launch.environment.insert(
            mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV.into(),
            "1".into(),
        );
        launch
            .environment
            .remove(mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV);
    } else {
        launch
            .environment
            .remove(mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV);
        // This controller grants the message-only server to every
        // top-level session; the worker trusts only this marker.
        launch.environment.insert(
            mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV.into(),
            "1".into(),
        );
    }
    if subagent.is_none()
        && matches!(checkout, mj_core::state::Checkout::Attached { .. })
        && !session
            .project
            .as_ref()
            .is_some_and(|p| p.primary_is_plain_directory())
        && matches!(history, LaunchHistory::Stored)
        && session.checkpoint.is_none()
        && session.native_session_id.is_none()
    {
        launch.environment.insert(
            mj_core::worker_launch::SESSION_RAW_LAUNCH_RECORD_ENV.into(),
            "1".into(),
        );
    } else {
        launch
            .environment
            .remove(mj_core::worker_launch::SESSION_RAW_LAUNCH_RECORD_ENV);
    }
    apply_jev_settings(&mut launch, &config.jev);
    launch.agent_mailboxes_enabled = true;
    let subagent_view = subagent.map(|child| child.to_record_view());
    launch.subagents = session
        .subagents
        .clone()
        .unwrap_or_default()
        .for_launch(profile.kind, subagent_view.as_ref());
    // Only a delegating parent's spawn tool reads this. Other sessions keep
    // the default, so their launch files do not change.
    launch.subtle_subagents =
        !launch.subagents.uses_mjolnir() || config.subagents.offers_subtle(&launch.subagents);
    let owns_environment = state.owns_environment(session_id) == Some(true);
    launch.oracle = oracle_launch_enabled(session, owns_environment, &config.subagents);
    // Registration decided whether this child can be given the tool.
    launch.handback_tool = subagent.as_ref().is_some_and(|child| child.handback_tool);
    launch.subagent_kind = subagent.as_ref().map(|child| child.kind);
    launch.orchestrator = subagent.as_ref().is_some_and(|child| child.orchestrator);
    launch.orchestrators_enabled = subagent.is_none() && launch.subagents.orchestrators_enabled();

    // The child's harness opens on its spawn model instead of switching
    // to it from the profile default before the first turn (issue 1217).
    launch.initial_model = None;
    if let Some(child) = subagent.as_ref() {
        let group = mj_core::continuation::GeneratedPrompt::subagent_spawn_id(
            &child.kind.startup_request_key(&child.request_key),
        );
        let steps = crate::database::load_startup_group(session_id, &group)?;
        // The assignment is delivered at most once per session history.
        // A restore deletes the worker's command receipts, so the history
        // decides: once the assignment has started, the conversation
        // already holds it and the settings it was sent with.
        let prompt_id = crate::database::startup_step_id(&group, "prompt");
        let assignment_started = prompt_started(session_id, &history, &prompt_id)?;
        let same_spawn_service = session.last_profile == child.profile_id
            || config
                .profiles
                .get(&child.profile_id)
                .and_then(|spawn| {
                    config
                        .native_continuity(spawn.kind, &child.profile_id, &session.last_profile)
                        .ok()
                })
                .unwrap_or(false);
        if !assignment_started && same_spawn_service {
            launch.initial_model = child.model.clone();
        }
        if !assignment_started {
            launch.initial_commands = steps
                .into_iter()
                .filter(|step| {
                    same_spawn_service
                        || !matches!(
                            serde_json::from_str::<crate::database::StartupStep>(&step.step_json),
                            Ok(crate::database::StartupStep::Configure { key, .. })
                                if matches!(key.as_str(), "model" | "effort" | "thought_level")
                        )
                })
                .map(|step| {
                    let (command, optional) = match serde_json::from_str::<
                        crate::database::StartupStep,
                    >(&step.step_json)?
                    {
                        crate::database::StartupStep::Configure {
                            key,
                            value,
                            optional,
                        } => (
                            mj_core::relay::RelayCommand::SetConfig { key, value },
                            optional,
                        ),
                        crate::database::StartupStep::ApiPrompt { text } => (
                            mj_core::relay::RelayCommand::Prompt {
                                prompt: vec![
                                    agent_client_protocol::v1::schema::ContentBlock::Text(
                                        agent_client_protocol::v1::schema::TextContent::new(text),
                                    ),
                                ],
                            },
                            false,
                        ),
                        _ => anyhow::bail!("sub-agent startup contains a non-input step"),
                    };
                    Ok(mj_core::worker_launch::InitialWorkerCommand {
                        command_id: step.command_id,
                        command,
                        optional,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        }
    }
    // The workspace owner requests review before pushing. The worker derives
    // the review tools and the push gate from this one value.
    launch.review_trigger = if owns_environment {
        if let Err(missing) = state.effective_review(session_id) {
            tracing::warn!(session_id, missing = %missing,
                "review choice cannot be resolved; launching without review tools");
        }
        state
            .effective_review_config(&config.review, session_id)
            .trigger()
    } else {
        Default::default()
    };
    Ok((launch, project_memory, target_profile_home))
}

pub fn current_worker_launch_config(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
) -> Result<WorkerLaunchConfig> {
    let operation = crate::database::load_move_operation(session_id)?;
    worker_launch_config_for_move(config, state, session_id, backend, operation.as_ref())
}

pub fn worker_launch_config_for_move(
    config: &Config,
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    operation: Option<&mj_core::state::MoveOperation>,
) -> Result<WorkerLaunchConfig> {
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let (mut launch, _, _) =
        session_launch_config(config, state, session_id, backend, LaunchHistory::Stored)?;
    if operation.is_some_and(|operation| {
        operation.source_checkpoint_only
            && operation.destination_target.is_none()
            && matches!(
                operation.phase,
                mj_core::state::MovePhase::Capturing
                    | mj_core::state::MovePhase::Transferring
                    | mj_core::state::MovePhase::Restoring
                    | mj_core::state::MovePhase::DestinationStarting
                    | mj_core::state::MovePhase::Failed
                    | mj_core::state::MovePhase::Cancelled
            )
            && session.last_profile == operation.source_profile_id
            && session.target.as_deref() == operation.source_target.as_ref()
            && matches!(
                session.state,
                mj_core::state::SessionState::Running
                    | mj_core::state::SessionState::Disconnected
                    | mj_core::state::SessionState::Closing
            )
    }) {
        launch.run_mode = mj_core::worker_launch::WorkerRunMode::CheckpointOnly;
    }
    Ok(launch)
}

/// Where the session's project memory is synchronized, or `None` for a
/// session that has none.
pub fn project_memory_sync_target(
    config: &Config,
    state: &State,
    session_id: &str,
) -> Result<Option<ProjectMemorySyncTarget>> {
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    if session.no_project_memory {
        return Ok(None);
    }
    let locator = session
        .target
        .as_ref()
        .context("session target is missing")?;
    let backend = backend_locator(locator, session, config)?;
    let profile = config
        .profiles
        .get(&session.last_profile)
        .context("session profile is missing")?;
    let owner = state.workspace_owner(session_id)?;
    let checkout = owner.checkout();
    session.validate_configuration_in(&checkout, config)?;
    let bundle = checkout
        .project_directory()
        .is_none()
        .then(|| owner.project_bundle(config))
        .flatten();
    let workspace = launch_workspace_paths(
        &checkout,
        bundle,
        &backend,
        &LaunchWorkspace {
            session_id: &owner.id,
            container: owner.container_workspace.as_deref(),
            parent_worktree: owner.managed_worktree.as_deref(),
            owner: Some(owner),
            working_directories: &[],
        },
    )?;
    let target_home = target_profile_home(&backend, session_id, profile);
    let pinned_memory_key = pinned_legacy_memory_key(session, owner)?;
    let launch = project_memory_launch(
        session,
        &checkout,
        bundle,
        &workspace,
        &target_home,
        owner.managed_worktree.as_deref(),
        pinned_memory_key.as_deref(),
    )?;
    Ok(Some(ProjectMemorySyncTarget {
        canonical_root: canonical_memory_root(&launch.project_key),
        relay: mj_targets::targets::project_memory_relay_command(&backend, session_id)?,
    }))
}

fn pinned_legacy_memory_key(
    session: &SessionRecord,
    owner: &SessionRecord,
) -> Result<Option<String>> {
    if session.project.is_some() {
        return Ok(None);
    }
    let key = crate::database::read_pinned_memory_key(&session.id)?;
    if key.is_some() || session.id == owner.id {
        Ok(key)
    } else {
        crate::database::read_pinned_memory_key(&owner.id)
    }
}

/// Writes `servers` into the staged profile of a harness that reads its MCP
/// configuration from disk.
///
/// Claude Code reads `mcpServers` from `.claude.json` in its config directory;
/// Kimi reads `mcpServers` from `mcp.json` in its home, and needs the runtime
/// id its own schema carries. Both files are the reviewer's private copy, so
/// nothing here can reach the user's own configuration.
pub fn configure_staged_review_mcp(
    harness: mj_core::config::HarnessKind,
    profile_stage: &Path,
    servers: &[ReviewMcpServer],
) -> Result<()> {
    let Some(file) = harness.mcp_config_file() else {
        bail!("{harness:?} does not read MCP servers from its profile");
    };
    let kimi = harness == mj_core::config::HarnessKind::Kimi;
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
    mj_core::config::atomic_write(&path, &body)
        .with_context(|| format!("write staged reviewer configuration {}", path.display()))
}

/// The command that opens a relay connection to a session's worker.
pub fn reconnect_command(
    config: &Config,
    state: &State,
    session_id: &str,
) -> Result<targets::CommandSpec> {
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    state.validate_session_configuration(session, config)?;
    let locator = session.target.as_ref().context("session has no target")?;
    let backend = backend_locator(locator, session, config)?;
    targets::reconnect_plan(&backend, session_id)?
        .commands
        .into_iter()
        .next()
        .context("reconnect plan is empty")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mj_core::config::{HarnessKind, HarnessProfile, TargetTemplate};
    use mj_core::state::{SessionState, TargetLocator};
    use mj_core::subagent::{CHILD_REPORT_DIR_ENV, SubagentKind, SubagentRecord};
    use mj_targets::test_isolation::{IsolatedTest, test_name};

    // A sub-agent records no checkout of its own, so its configuration must
    // be judged by its owner's: a plain project directory needs no bundle.
    #[test]
    fn reconnect_accepts_a_borrowed_sub_agent_of_a_bare_raw_session() {
        let mut config = Config::default();
        config
            .targets
            .insert("localhost".into(), TargetTemplate::LocalBare);
        config.profiles.insert(
            "codex".into(),
            HarnessProfile::new(HarnessKind::Codex, PathBuf::from("/profile")),
        );
        let session = |id: &str, directory: Option<&str>| {
            let mut session = crate::database::test_fixtures::checkpoint_test_session(id);
            session.state = SessionState::Running;
            session.target_template_id = "localhost".into();
            session.bundle_id = "no-such-bundle".into();
            session.project = None;
            session.project_directory = directory.map(PathBuf::from);
            session.target = Some(Box::new(TargetLocator::LocalBare {
                worker_root: PathBuf::from("/workers").join(id),
            }));
            session
        };
        let parent = session("11111111111111111111111111111111", Some("/work/raw"));
        let child = session("22222222222222222222222222222222", None);
        let mut state = State::default();
        for record in [&parent, &child] {
            state.sessions.insert(record.id.clone(), record.clone());
        }
        state.environment_owners = [
            (parent.id.clone(), parent.id.clone()),
            (child.id.clone(), parent.id.clone()),
        ]
        .into_iter()
        .collect();
        reconnect_command(&config, &state, &child.id).unwrap();
    }

    #[test]
    fn stage_candidate_preserves_live_files() {
        const MARKER: &str = "MJ_TEST_PRIVATE_RELOAD";
        if std::env::var_os(MARKER).is_none() {
            let dir = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "stage_candidate_preserves_live_files",
            ))
            .env(MARKER, "1")
            .isolated_store(dir.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("config.toml"), "model = \"candidate\"\n").unwrap();
        std::fs::write(source.join("auth.json"), r#"{"OPENAI_API_KEY":"test-key"}"#).unwrap();
        let mut config = Config::default().with_local_targets();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile::new(HarnessKind::Codex, &source),
        );
        let id = "55555555555555555555555555555555";
        let root = dir.path().join(id);
        std::fs::create_dir_all(&root).unwrap();
        let backend = targets::TargetLocator::LocalBare {
            worker_root: root.to_string_lossy().into_owned(),
        };
        let mut session = crate::database::test_fixtures::checkpoint_test_session(id);
        session.build_cache = Some(mj_core::state::SessionBuildCache {
            host: "localhost".into(),
            directory: dir.path().join("cache"),
            max_size: None,
            target_root: None,
        });
        session.last_profile = "codex".into();
        session.harness_kind = HarnessKind::Codex;
        session.native_session_id = None;
        session.project_directory = Some(dir.path().to_owned());
        session.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: root.clone(),
        }));
        session.target_template_id = "localhost".into();
        crate::database::save_session(&session).unwrap();
        let home = targets::target_profile_home(&backend, id, &config.profiles["codex"]);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(Path::new(&home).join("config.toml"), "live profile").unwrap();
        for name in ["launch.json", "ownership.json"] {
            std::fs::write(root.join(name), "live").unwrap();
        }
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for name in ["cargo", "mbx"] {
            std::fs::write(root.join("bin").join(name), "live shim").unwrap();
        }
        let state = crate::database::load_state().unwrap();
        // Candidate staging must not consult host mbx or alter live cache shims.
        #[cfg(unix)]
        {
            let no_host_mbx = dir.path().join("no-host-mbx");
            std::fs::create_dir(&no_host_mbx).unwrap();
            for utility in [
                "mkdir", "cp", "chmod", "sh", "rm", "ln", "find", "dirname", "cat", "tar",
            ] {
                let executable = ["/usr/bin", "/bin"]
                    .into_iter()
                    .map(|directory| Path::new(directory).join(utility))
                    .find(|path| path.exists())
                    .expect("system filesystem utility");
                std::os::unix::fs::symlink(executable, no_host_mbx.join(utility)).unwrap();
            }
            unsafe {
                std::env::set_var("PATH", &no_host_mbx);
            }
        }
        crate::database::test_fixtures::worker_test_scope(id, || {
            prepare_worker_files_for(
                &config,
                &state,
                id,
                &backend,
                root.to_str().unwrap(),
                &mj_core::targets::ProcessExecutor,
                WorkerFilePreparation::StageCandidate {
                    operation_id: "move".into(),
                    expected_store_id: "store".into(),
                    candidate_id: "candidate".into(),
                    worker_protocol: mj_core::relay::RELAY_PROTOCOL_VERSION,
                },
                LaunchHistory::Stored,
            )
            .unwrap();
        });
        for name in ["cargo", "mbx"] {
            assert_eq!(
                std::fs::read(root.join("bin").join(name)).unwrap(),
                b"live shim"
            );
        }
        let candidate = root.join("reload-candidates/candidate");
        let launch = WorkerLaunchConfig::read(&candidate.join("launch.json")).unwrap();
        let memory = launch.project_memory.as_ref().unwrap();
        let canonical = canonical_memory_root(&memory.project_key);
        let current = mj_core::project_memory::read_canonical_state(&canonical).unwrap();
        let tree = mj_core::project_memory::ProjectMemorySnapshot {
            files: [("/MEMORY.md".into(), "staged memory".into())].into(),
        };
        mj_core::project_memory::commit_canonical(&canonical, &current, id, &tree, None).unwrap();
        // Seed installation was introduced at this protocol boundary.
        for (candidate_id, protocol, legacy) in [
            (
                "old-worker",
                mj_core::relay::RELAY_PROJECT_MEMORY_SEED_PROTOCOL - 1,
                true,
            ),
            (
                "seed-worker",
                mj_core::relay::RELAY_PROJECT_MEMORY_SEED_PROTOCOL,
                false,
            ),
            ("new-worker", mj_core::relay::RELAY_PROTOCOL_VERSION, false),
        ] {
            crate::database::test_fixtures::worker_test_scope(id, || {
                prepare_worker_files_for(
                    &config,
                    &state,
                    id,
                    &backend,
                    root.to_str().unwrap(),
                    &mj_core::targets::ProcessExecutor,
                    WorkerFilePreparation::StageCandidate {
                        operation_id: "move".into(),
                        expected_store_id: "store".into(),
                        candidate_id: candidate_id.into(),
                        worker_protocol: protocol,
                    },
                    LaunchHistory::Stored,
                )
                .unwrap();
            });
            let overlay = root
                .join("reload-candidates")
                .join(candidate_id)
                .join("profile");
            for path in [&memory.root, &memory.baseline_root] {
                let staged_tree = overlay.join(path.strip_prefix(&home).unwrap());
                if legacy {
                    assert_eq!(
                        std::fs::read_to_string(staged_tree.join("MEMORY.md")).unwrap(),
                        "staged memory"
                    );
                } else {
                    assert!(!staged_tree.exists());
                }
            }
        }
        assert_eq!(launch.accepted_reload_operation_id.as_deref(), Some("move"));
        for name in [
            "launch.json",
            "ownership.json",
            "manifest.json",
            "profile/config.toml",
        ] {
            assert!(candidate.join(name).exists(), "{name}");
        }
        for name in ["launch.json", "ownership.json"] {
            assert_eq!(std::fs::read(root.join(name)).unwrap(), b"live");
        }
        assert_eq!(
            std::fs::read(Path::new(&home).join("config.toml")).unwrap(),
            b"live profile"
        );
    }

    #[test]
    fn current_worker_launch_config_preserves_staged_aws_profile() {
        const TEST: &str = "current_worker_launch_config_preserves_staged_aws_profile";
        const MARKER: &str = "MJ_TEST_STAGED_AWS_PROFILE";
        if std::env::var_os(MARKER).is_none() {
            let dir = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), TEST))
                .env(MARKER, "1")
                .isolated_store(dir.path())
                .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default().with_local_targets();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile::new(HarnessKind::Codex, dir.path()),
        );
        let id = "33333333333333333333333333333333";
        let mut session = crate::database::test_fixtures::checkpoint_test_session(id);
        session.native_session_id = None;
        session.project_directory = Some(dir.path().to_owned());
        session.container_workspace = Some(crate::targets::new_container_workspace(id).unwrap());
        session.target_template_id = "docker".into();
        let locator = TargetLocator::LocalDocker {
            container_id: "c".repeat(64),
            borrowed_from: None,
        };
        let backend = backend_locator(&locator, &session, &config).unwrap();
        session.target = Some(Box::new(locator));
        session.staged_aws_profile = Some("original".into());
        crate::database::save_session(&session).unwrap();
        let state = crate::database::load_state().unwrap();
        for edited_profile in ["original", "changed"] {
            std::fs::write(
                dir.path().join("config.toml"),
                format!("model_provider = \"amazon-bedrock-runtime\"\n[model_providers.amazon-bedrock-runtime.aws]\nprofile = \"{edited_profile}\""),
            ).unwrap();
            let launch = current_worker_launch_config(&config, &state, id, &backend).unwrap();
            assert_eq!(launch.environment["AWS_PROFILE"], "original");
            assert!(launch.environment["AWS_CONFIG_FILE"].ends_with("/aws-config"));
        }
    }

    /// The reviewer record holds the launch script generated when the review
    /// started. After a harness pin bump the worker must still start the
    /// current built-in launcher, not the saved one.
    #[test]
    fn reviewer_launch_regenerates_a_saved_built_in_script_after_a_pin_bump() {
        const TEST: &str = "reviewer_launch_regenerates_a_saved_built_in_script_after_a_pin_bump";
        const MARKER: &str = "MJ_TEST_REVIEWER_LAUNCH_REGENERATES";
        // The launcher v2.76.0 generated for dsh-acp 0.8.0, byte for byte.
        const SHIPPED_0_8_0: &str =
            include_str!("../tests/fixtures/dsh_acp_0_8_0_launch_v2_76_0.sh");
        if std::env::var_os(MARKER).is_none() {
            let dir = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), TEST))
                .env(MARKER, "1")
                .isolated_store(dir.path())
                .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let layout = mj_core::config::DataLayout::current();
        let mut config = Config::default();
        config
            .targets
            .insert("podman".into(), TargetTemplate::LocalBare);
        config.profiles.insert(
            "dsh".into(),
            HarnessProfile::new(HarnessKind::Dsh, layout.root().join("profile")),
        );
        let policy = mj_core::config::ExecutionPolicy::ConfiguredApprovals;
        let current = mj_core::harness_runtime::fallback_launch(HarnessKind::Dsh, policy);
        assert_ne!(
            current.1,
            ["-c".to_owned(), SHIPPED_0_8_0.to_owned()],
            "the fixture must be an earlier release's script"
        );
        let parent_id = "11111111111111111111111111111111";
        let child_id = "22222222222222222222222222222222";
        let mut parent = crate::database::test_fixtures::checkpoint_test_session(parent_id);
        parent.last_profile = "dsh".into();
        parent.harness_kind = HarnessKind::Dsh;
        crate::database::save_session(&parent).unwrap();
        let mut child = crate::database::test_fixtures::checkpoint_test_session(child_id);
        child.last_profile = "dsh".into();
        child.harness_kind = HarnessKind::Dsh;
        child.project_directory = Some(layout.root().join("child-workspace"));
        child.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: layout.worker_root(child_id),
        }));
        let relation = SubagentRecord {
            child_session_id: child_id.into(),
            parent_session_id: parent_id.into(),
            task_name: "review".into(),
            profile_id: "dsh".into(),
            working_directory: PathBuf::new(),
            request_key: "saved-reviewer".into(),
            created_at: child.created_at.clone(),
            kind: SubagentKind::Worker,
            reviewer_launch: Some(mj_core::worker_launch::ReviewerLaunchConfig {
                role: Default::default(),
                profile_id: "dsh".into(),
                harness: HarnessKind::Dsh,
                bridge_command: "sh".into(),
                bridge_args: vec!["-c".into(), SHIPPED_0_8_0.into()],
                environment: Default::default(),
                excluded_environment: Vec::new(),
                execution_policy: policy,
                model: None,
                effort: None,
                fast_mode: None,
                generation: Default::default(),
                mcp_servers: Vec::new(),
            }),
            model: None,
            effort: None,
            initial_prompt: "review".into(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: true,
            orchestrator: false,
            resume_with_parent: false,
        };
        crate::database::register_subagent_with_startup(
            &child,
            &relation,
            Vec::new(),
            None,
            None,
            true,
        )
        .unwrap();
        parent.state = SessionState::Stopped;
        crate::database::save_lifecycle_session(&parent).unwrap();
        let state = crate::database::load_state().unwrap();
        let (backend, _) = worker_placement(&config, &state, child_id).unwrap();
        let (launch, _, _) =
            session_launch_config(&config, &state, child_id, &backend, LaunchHistory::Stored)
                .unwrap();
        assert!(launch.reviewer_launch.is_some());
        assert_eq!(launch.bridge_command, Path::new(&current.0));
        assert_eq!(launch.bridge_args, current.1);
        assert!(mj_core::harness_runtime::is_fallback_launch(
            HarnessKind::Dsh,
            policy,
            &launch.bridge_command,
            &launch.bridge_args
        ));
    }

    #[test]
    fn owned_child_launch_uses_its_checkout_and_report_without_parent_placement() {
        const TEST: &str =
            "owned_child_launch_uses_its_checkout_and_report_without_parent_placement";
        const MARKER: &str = "MJ_TEST_OWNED_CHILD_LAUNCH";
        if std::env::var_os(MARKER).is_none() {
            let dir = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), TEST))
                .env(MARKER, "1")
                .isolated_store(dir.path())
                .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let mut config = Config::default();
        config
            .targets
            .insert("podman".into(), TargetTemplate::LocalBare);
        config.profiles.insert(
            "codex".into(),
            HarnessProfile::new(
                HarnessKind::Codex,
                mj_core::config::DataLayout::current()
                    .root()
                    .join("profile"),
            ),
        );
        let choice = mj_core::config::SubagentChoice {
            model: "test-model".into(),
            effort: None,
        };
        config.review.reviewers = vec![choice.clone()];
        let parent_id = "11111111111111111111111111111111";
        let child_id = "22222222222222222222222222222222";
        let mut parent = crate::database::test_fixtures::checkpoint_test_session(parent_id);
        parent.last_profile = "missing-profile".into();
        crate::database::save_session(&parent).unwrap();
        let mut child = crate::database::test_fixtures::checkpoint_test_session(child_id);
        child.oracle = true;
        child.oracle_choice = Some(Box::new(choice));
        child.review = Some(Box::new(mj_core::config::SessionReview::Push {
            reviewers: None,
        }));
        child.subagents = Some(mj_core::subagent::SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        });
        let workspace = mj_core::config::DataLayout::current()
            .root()
            .join("child-workspace");
        child.project_directory = Some(workspace.clone());
        child.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: mj_core::config::DataLayout::current().worker_root(child_id),
        }));
        let relation = SubagentRecord {
            child_session_id: child_id.into(),
            parent_session_id: parent_id.into(),
            task_name: "isolated assignment".into(),
            profile_id: "codex".into(),
            working_directory: PathBuf::from("relative-to-parent"),
            request_key: "owned-child-launch".into(),
            created_at: child.created_at.clone(),
            kind: SubagentKind::Worker,
            reviewer_launch: None,
            model: Some("codex-spawn-model".into()),
            effort: None,
            initial_prompt: "inspect".into(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: true,
            orchestrator: true,
            resume_with_parent: false,
        };
        let group = mj_core::continuation::GeneratedPrompt::subagent_spawn_id(
            &relation.kind.startup_request_key(&relation.request_key),
        );
        let startup = [
            (
                "model",
                crate::database::StartupStep::Configure {
                    key: "model".into(),
                    value: "codex-spawn-model".into(),
                    optional: false,
                },
            ),
            (
                "effort",
                crate::database::StartupStep::Configure {
                    key: "effort".into(),
                    value: "high".into(),
                    optional: false,
                },
            ),
            (
                "thought",
                crate::database::StartupStep::Configure {
                    key: "thought_level".into(),
                    value: "high".into(),
                    optional: true,
                },
            ),
            (
                "approval",
                crate::database::StartupStep::Configure {
                    key: "approval-policy".into(),
                    value: "never".into(),
                    optional: true,
                },
            ),
            (
                "prompt",
                crate::database::StartupStep::ApiPrompt {
                    text: "inspect".into(),
                },
            ),
        ]
        .into_iter()
        .map(|(suffix, step)| crate::database::StartupDelivery {
            session_id: child_id.into(),
            command_id: crate::database::startup_step_id(&group, suffix),
            step_json: serde_json::to_string(&step).unwrap(),
            phase: "pending".into(),
            group_id: Some(group.clone()),
            accepted_ordinal: None,
            error: None,
        })
        .collect();
        crate::database::register_subagent_with_startup(
            &child, &relation, startup, None, None, true,
        )
        .unwrap();
        parent.state = SessionState::Stopped;
        crate::database::save_lifecycle_session(&parent).unwrap();
        let report = workspace.join(".mj/agents").join(child_id);
        crate::database::record_subagent_report_dir(child_id, report.to_str().unwrap()).unwrap();
        let owner_key = "c".repeat(64);
        crate::database::pin_session_memory_key(parent_id, &owner_key).unwrap();
        assert_eq!(
            pinned_legacy_memory_key(&child, &parent).unwrap(),
            Some(owner_key)
        );
        let pinned_key = "a".repeat(64);
        crate::database::pin_session_memory_key(child_id, &pinned_key).unwrap();
        let state = crate::database::load_state().unwrap();
        let (backend, _) = worker_placement(&config, &state, child_id).unwrap();
        let (launch, memory, _) =
            session_launch_config(&config, &state, child_id, &backend, LaunchHistory::Stored)
                .unwrap();
        assert_eq!(memory.unwrap().project_key, pinned_key);
        assert_eq!(launch.cwd, workspace);
        assert_eq!(
            launch.environment[CHILD_REPORT_DIR_ENV],
            report.to_str().unwrap()
        );
        assert!(launch.oracle);
        assert!(launch.review_tools() && launch.review_gate() && launch.prepares_review());
        assert_eq!(
            launch.subagent_mcp_role(),
            Some(mj_core::subagent::SubagentMcpRole::Orchestrator)
        );
        let saved = serde_json::to_value(&launch).unwrap();
        assert!(saved.get("owns_environment").is_none());
        let launch: WorkerLaunchConfig = serde_json::from_value(saved).unwrap();
        assert!(launch.oracle && launch.review_gate());
        assert_eq!(launch.subagent_kind, Some(SubagentKind::Worker));
        let state = crate::database::load_state().unwrap();
        let (launch, _, _) =
            session_launch_config(&config, &state, child_id, &backend, LaunchHistory::Stored)
                .unwrap();
        assert_eq!(launch.initial_model.as_deref(), Some("codex-spawn-model"));
        assert!(
            !prompt_started(
                child_id,
                &LaunchHistory::Stored,
                &crate::database::startup_step_id(&group, "prompt")
            )
            .unwrap()
        );
        let original_commands = launch.initial_commands.clone();
        config
            .profiles
            .insert("same-service".into(), config.profiles["codex"].clone());
        let mut same_service = state.clone();
        same_service
            .sessions
            .get_mut(child_id)
            .unwrap()
            .last_profile = "same-service".into();
        let (same_launch, _, _) = session_launch_config(
            &config,
            &same_service,
            child_id,
            &backend,
            LaunchHistory::Stored,
        )
        .unwrap();
        assert_eq!(
            same_launch.initial_model.as_deref(),
            Some("codex-spawn-model")
        );
        assert_eq!(same_launch.initial_commands, original_commands);

        let deepseek_home = mj_core::config::DataLayout::current()
            .root()
            .join("deepseek-profile");
        std::fs::create_dir_all(&deepseek_home).unwrap();
        std::fs::write(
            deepseek_home.join("config.toml"),
            r#"model_provider = "deepseek"
[model_providers.deepseek]
base_url = "https://api.deepseek.com/v1"
env_key = "DEEPSEEK_API_KEY"
wire_api = "responses"
"#,
        )
        .unwrap();
        config.profiles.insert(
            "deepseek".into(),
            HarnessProfile::new(HarnessKind::Codex, deepseek_home),
        );
        assert!(
            !config
                .native_continuity(HarnessKind::Codex, "codex", "deepseek")
                .unwrap()
        );
        let mut different_service = state.clone();
        different_service
            .sessions
            .get_mut(child_id)
            .unwrap()
            .last_profile = "deepseek".into();
        let (different_launch, _, _) = session_launch_config(
            &config,
            &different_service,
            child_id,
            &backend,
            LaunchHistory::Stored,
        )
        .unwrap();
        assert_eq!(different_launch.initial_model, None);
        assert_eq!(
            different_launch
                .initial_commands
                .iter()
                .map(|input| input.command.clone())
                .collect::<Vec<_>>(),
            vec![
                mj_core::relay::RelayCommand::SetConfig {
                    key: "approval-policy".into(),
                    value: "never".into()
                },
                mj_core::relay::RelayCommand::Prompt {
                    prompt: vec![agent_client_protocol::v1::schema::ContentBlock::from(
                        "inspect"
                    )]
                },
            ]
        );

        config.profiles.insert(
            "claude".into(),
            HarnessProfile::new(
                HarnessKind::Claude,
                mj_core::config::DataLayout::current().root().join("claude"),
            ),
        );
        let mut moved = state.clone();
        moved.sessions.get_mut(child_id).unwrap().last_profile = "claude".into();
        moved.sessions.get_mut(child_id).unwrap().harness_kind = HarnessKind::Claude;
        let (launch, _, _) =
            session_launch_config(&config, &moved, child_id, &backend, LaunchHistory::Stored)
                .unwrap();
        assert_eq!(launch.initial_model, None);
    }
}
