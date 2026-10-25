use super::*;

/// The repository paths a worker opens. `session_id` and `container_workspace`
/// identify the session whose workspace is used, which for a sub-agent child is
/// its parent.
pub(super) fn workspace_paths(
    locator: &targets::TargetLocator,
    bundle: &ProjectBundle,
    session_id: &str,
    container_workspace: Option<&Path>,
) -> Result<(String, Vec<String>)> {
    let root = match locator {
        targets::TargetLocator::LocalBare { .. } => {
            bail!("local bare projects use their selected directory")
        }
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. }
        | targets::TargetLocator::SshAppleContainer { .. } => {
            targets::container_workspace_root(container_workspace)
        }
        targets::TargetLocator::AwsEc2 { workspace, .. }
        | targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
    };
    if matches!(locator, targets::TargetLocator::AwsEc2 { .. }) {
        let expected = format!(".local/share/hel/workspaces/{session_id}");
        if root != expected {
            bail!("AWS workspace does not match session")
        }
    }
    let primary = bundle.primary().context("bundle primary is missing")?;
    let primary_path = format!("{root}/{}", primary.destination.to_string_lossy());
    let additional = bundle
        .repositories
        .iter()
        .filter(|repository| repository.id != bundle.primary_repo)
        .map(|repository| format!("{root}/{}", repository.destination.to_string_lossy()))
        .collect();
    Ok((primary_path, additional))
}

// Package versions for ACP bridges and their harnesses. Keep these in lockstep with the global
// npm installs in containers/Containerfile.agent-dev; bridge_pins_match_containerfile() below
// fails the build when they drift.
// Codex 0.148 reuses pending MCP startups during runtime reconciliation. Older
// releases could cancel the first project-memory startup while immediately
// replacing it with an equivalent connection, leaving a false failed-tool
// event at the beginning of every session.
/// Stage shown after the worker is reachable and while its ACP bridge becomes
/// ready. Every remaining harness launches through a default launcher that can
/// fetch it, so the stage names the harness being installed.
pub(in crate::controller) fn bridge_readiness_stage(profile: &HarnessProfile) -> ProvisionStage {
    ProvisionStage::Installing(profile.kind)
}

/// The target-provided launcher used before the worker decides whether to
/// replace it with an exact managed installation.
pub(in crate::controller) fn bridge_launch(
    harness: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
) -> (String, Vec<String>) {
    mj_core::harness_runtime::fallback_launch(harness, policy)
}

pub(in crate::controller) fn preflight_harness(
    template: &mj_core::config::TargetTemplate,
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> Result<()> {
    use mj_core::config::TargetTemplate;
    if let TargetTemplate::SshBare { ssh, .. } = template {
        let ssh = SshTarget::from(ssh);
        execute_checked(executor, crate::targets::ssh_login_script(&ssh,
            "git --version >/dev/null || { echo 'Git is missing or unusable; install Git and, on macOS, the Xcode Command Line Tools on the target' >&2; exit 1; }",
        ).purpose("preflight remote Git"))?;
    }
    if !mj_core::harness_runtime::harness_uses_node_runtime(profile.kind) {
        return Ok(());
    }
    if !matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    ) {
        return Ok(());
    }
    // Mjolnir installs its own pinned Node.js when the target has no
    // compatible runtime. The worker performs that download, so the
    // controller only needs to establish that one of the two paths exists
    // before provisioning creates target state.
    let script = if profile.kind == HarnessKind::Dsh {
        format!(
            "if command -v dsh-acp >/dev/null 2>&1; then exit 0; fi; {}",
            node_or_downloader_preflight_script()
        )
    } else {
        node_or_downloader_preflight_script()
    };
    let mut args = if profile.environment.contains_key("PATH") {
        vec![
            "-c".to_owned(),
            format!("export PATH=\"$1\"; {script}"),
            "mj-node-preflight".into(),
            profile.environment["PATH"].clone(),
        ]
    } else {
        vec!["-lc".to_owned(), script.clone()]
    };
    let (command, destination) = match template {
        TargetTemplate::LocalBare => (CommandSpec::new("sh", args), "local host".to_owned()),
        TargetTemplate::SshBare { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            if profile.environment.contains_key("PATH") {
                args.insert(0, "sh".into());
                (crate::targets::ssh_command(&ssh, args), ssh.destination)
            } else {
                (
                    crate::targets::ssh_login_script(&ssh, &script),
                    ssh.destination,
                )
            }
        }
        _ => unreachable!(),
    };
    let command = command.purpose("preflight managed harness Node.js");
    let output = executor.execute(&command)?;
    if output.status == 0 {
        return Ok(());
    }
    let detail = crate::controller::command_error_detail(&output.stderr);
    let failure = if detail.is_empty() {
        anyhow::anyhow!("{} failed with status {}", command.purpose, output.status)
    } else {
        anyhow::anyhow!(detail)
    };
    Err(failure.context(format!(
        "{} launch preflight failed on {destination}; provide Node.js 22+ with npm and npx on the target PATH, or use a target where Mjolnir's managed runtime can run",
        profile.kind.display_name()
    )))
}

fn node_or_downloader_preflight_script() -> String {
    format!(
        "if command -v node >/dev/null 2>&1 && node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)' && command -v npm >/dev/null 2>&1 && npm --version >/dev/null 2>&1 && command -v npx >/dev/null 2>&1 && npx --version >/dev/null 2>&1; then exit 0; fi; managed_target=''; loader=''; system=$(uname -s 2>/dev/null) || system=''; machine=$(uname -m 2>/dev/null) || machine=''; case \"$system:$machine\" in Linux:x86_64) managed_target=linux-x64; loader=/lib64/ld-linux-x86-64.so.2 ;; Linux:aarch64|Linux:arm64) managed_target=linux-arm64; loader=/lib/ld-linux-aarch64.so.1 ;; Darwin:x86_64) managed_target=darwin-x64 ;; Darwin:arm64) managed_target=darwin-arm64 ;; *) echo \"Mjolnir's managed Node.js has no artifact for $system $machine; install Node.js 22 or newer with npm and npx on the target PATH\" >&2; exit 127 ;; esac; managed=\"${{XDG_CACHE_HOME:-$HOME/.cache}}/mjolnir/node/node-v{}-$managed_target\"; if [ -x \"$managed/bin/node\" ] && \"$managed/bin/node\" -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)' && [ -x \"$managed/bin/npm\" ] && \"$managed/bin/npm\" --version >/dev/null 2>&1 && [ -x \"$managed/bin/npx\" ] && \"$managed/bin/npx\" --version >/dev/null 2>&1; then exit 0; fi; if [ -n \"$loader\" ] && [ ! -f \"$loader\" ]; then echo \"Mjolnir's pinned Node.js for this target is a glibc build, but the required program interpreter $loader is missing; install Node.js 22 or newer with npm and npx in the target environment\" >&2; exit 127; fi; exit 0",
        mj_core::harness_runtime::NODE_VERSION
    )
}

pub(super) const MJ_CONTAINER_ENVIRONMENT: &str = "## Mjolnir disposable environment\n\nThis session runs in a disposable Mjolnir container. When the session closes, Mjolnir checkpoints everything in project workspace directories under `/workspace`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then removes the container.\n\nEverything outside `/workspace`, including installed packages, `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n";

/// The profile's own files only. Outside this module, stage through
/// [`stage_profile_with_catalog`].
pub(super) fn stage_profile(
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
) -> Result<()> {
    let harness = profile.kind;
    let source = profile.home.as_path();
    std::fs::create_dir_all(destination)?;
    // Only the files peculiar to a harness are listed per harness. The
    // instruction file and the synced skill directories are the same facts the
    // rest of Hel reads off `HarnessKind`, so they are appended from there
    // rather than repeated in every arm.
    let harness_files: &[&str] = match harness {
        mj_core::config::HarnessKind::Muse => {
            &["auth.json", "settings.json", "trust.json", "rules"]
        }
        mj_core::config::HarnessKind::Codex => {
            &["auth.json", "config.toml", "instructions.md", "rules"]
        }
        mj_core::config::HarnessKind::Claude => &[
            ".claude.json",
            ".credentials.json",
            "settings.json",
            "plugins",
        ],
        mj_core::config::HarnessKind::Kimi => &[
            "credentials",
            "config.toml",
            "device_id",
            "SYSTEM.md",
            "mcp.json",
            "agents",
            "plugins",
        ],
        mj_core::config::HarnessKind::Grok => &["auth.json", "config.toml", "agent_id", "plugins"],
        // OpenCode's logins live in its database, not its configuration
        // directory; they are exported below.
        mj_core::config::HarnessKind::OpenCode => &["opencode.json", "opencode.jsonc"],
        mj_core::config::HarnessKind::Mimo => {
            // MiMo Code's configuration and login may live in separate trees
            // (see `configuration_sources` and `data_source`), so they are
            // staged below.
            &[]
        }
        // dsh-acp keeps no configuration or login of its own; only the
        // instruction file and skills tree below are staged.
        mj_core::config::HarnessKind::Dsh => &[],
    };
    let allowlist: Vec<&str> = harness_files
        .iter()
        .copied()
        .chain(std::iter::once(harness.agent_instructions_file()))
        .chain(harness.synced_skill_dirs().iter().copied())
        .collect();
    // Allowlist entries (and, within each, a copied directory's children) are
    // independent of one another, so copying them concurrently shortens the
    // stage step for profiles with large skills/plugins trees.
    // What the harness maintains itself inside an allowlisted entry, such as
    // the skills Claude Code syncs from the user's claude.ai account, stays
    // behind: the harness provisions its own copy in the session home.
    let harness_owned = harness
        .harness_owned_skill_paths()
        .iter()
        .map(|owned| source.join(owned))
        .collect::<Vec<_>>();
    // MiMo Code's stock installation splits its configuration directory from
    // its data directory. Stage both into the `MIMOCODE_HOME` root the session
    // runs from, but carry only the login from the data directory: the native
    // session database and logs belong to the profile, not to a new session.
    // This runs before the allowlist below so Mjolnir's synced skills, which
    // live under `config/skills`, overwrite any same-named stock entries.
    if harness == mj_core::config::HarnessKind::Mimo {
        for config in harness.configuration_sources(source) {
            if config.is_dir() {
                copy_profile_entry_except(&config, &destination.join("config"), &harness_owned)?;
            }
        }
        let login = harness.data_source(source).join("auth.json");
        if login.is_file() {
            copy_profile_entry(&login, &destination.join(harness.credential_file_name()))?;
        }
    }
    allowlist.par_iter().try_for_each(|name| -> Result<()> {
        let from = source.join(name);
        if from.exists() {
            copy_profile_entry_except(&from, &destination.join(name), &harness_owned)?;
        }
        Ok(())
    })?;
    // `codex app-server` refuses `--profile`, so a Codex profile overlay
    // reaches the session already layered into the staged `config.toml`.
    if profile.codex_profile.is_some()
        && let Some(config) = profile.codex_config()?
    {
        std::fs::write(destination.join("config.toml"), config).with_context(|| {
            format!(
                "stage Codex configuration {}",
                profile.describe_codex_config()
            )
        })?;
    }
    if harness == mj_core::config::HarnessKind::OpenCode
        && let Some(logins) = crate::opencode_logins::profile_logins(profile)?
    {
        mj_core::config::atomic_write(&destination.join(harness.credential_file_name()), &logins)
            .context("stage OpenCode logins")?;
    }
    // `muse serve` never fetches its model list: it reads the list the
    // person's own `muse` last saved in its data directory, and without one
    // offers only its built-in model. A staged home starts with empty data, so
    // it gets that saved list.
    if harness == mj_core::config::HarnessKind::Muse {
        let catalog = harness
            .nested_data_dir(source)
            .expect("Muse has an XDG home")
            .join("model-catalog");
        if catalog.exists() {
            copy_profile_entry(&catalog, &destination.join(".data/muse/model-catalog"))?;
        }
    }
    Ok(())
}

/// Stage `profile` into `destination` for one harness run, together with the
/// model catalog of its Codex provider. Every staged copy outside this module
/// goes through here, so a session, a reviewer and a capability probe all
/// offer the same models: a reviewer staged without the catalog offered
/// OpenAI's built-in model names on a DeepSeek profile (#1320).
pub(in crate::controller) fn stage_profile_with_catalog(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
    fetch: CatalogFetch<'_>,
    cache: &dyn CatalogCache,
) -> Result<()> {
    stage_profile(profile, destination)?;
    stage_codex_catalog(profile_id, profile, destination, fetch, cache)
}
