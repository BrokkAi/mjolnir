use super::*;

pub(super) fn workspace_paths(
    locator: &targets::TargetLocator,
    bundle: &ProjectBundle,
    session_id: &str,
) -> Result<(String, Vec<String>)> {
    let root = match locator {
        targets::TargetLocator::LocalBare { .. } => {
            bail!("local bare projects use their selected directory")
        }
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => "/workspace".to_string(),
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

pub(in crate::controller) fn bridge_launch(
    harness: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
) -> (String, Vec<String>) {
    match harness {
        mj_core::config::HarnessKind::Muse => ("muse-acp".into(), Vec::new()),
        mj_core::config::HarnessKind::Codex => (
            "sh".into(),
            vec![
                "-c".into(),
                format!("if command -v codex-acp >/dev/null 2>&1 && [ \"$(codex-acp --version 2>/dev/null)\" = \"{CODEX_ACP_PACKAGE} {CODEX_ACP_VERSION}\" ]; then exec codex-acp; fi; {}; exec npx -y {CODEX_ACP_PACKAGE}@{CODEX_ACP_VERSION}", ensure_node_script()),
            ],
        ),
        mj_core::config::HarnessKind::Claude => (
            "sh".into(),
            vec![
                "-c".into(),
                format!("if command -v claude-agent-acp >/dev/null 2>&1; then exec claude-agent-acp; fi; {}; exec npx -y @agentclientprotocol/claude-agent-acp@{CLAUDE_ACP_VERSION}", ensure_node_script()),
            ],
        ),
        mj_core::config::HarnessKind::Kimi => (
            "sh".into(),
            vec![
                "-c".into(),
                "if command -v kimi >/dev/null 2>&1; then exec kimi acp; elif [ -x \"$HOME/.kimi-code/bin/kimi\" ]; then exec \"$HOME/.kimi-code/bin/kimi\" acp; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash && exec \"$HOME/.kimi-code/bin/kimi\" acp; else echo 'Mjolnir needs compatible Kimi Code or curl for its official installer; add the tool to PATH' >&2; exit 127; fi".into(),
            ],
        ),
        mj_core::config::HarnessKind::Grok => {
            let acp = mj_core::config::HarnessKind::Grok
                .bridge_args(policy)
                .join(" ");
            (
                "sh".into(),
                vec![
                    "-c".into(),
                    format!(
                        "if command -v grok >/dev/null 2>&1; then exec grok {acp}; elif [ -x \"$GROK_HOME/bin/grok\" ]; then exec \"$GROK_HOME/bin/grok\" {acp}; elif [ -x \"$HOME/.grok/bin/grok\" ]; then exec \"$HOME/.grok/bin/grok\" {acp}; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://x.ai/cli/install.sh | bash && exec \"$HOME/.grok/bin/grok\" {acp}; else echo 'Mjolnir needs compatible Grok Build or curl for its official installer; add the tool to PATH' >&2; exit 127; fi"
                    ),
                ],
            )
        }
    }
}

pub(in crate::controller) fn preflight_harness(
    template: &mj_core::config::TargetTemplate,
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> Result<()> {
    use mj_core::config::TargetTemplate;
    if !matches!(profile.kind, HarnessKind::Codex | HarnessKind::Claude) {
        return Ok(());
    }
    if !matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    ) {
        return Ok(());
    }
    let script = "if ! command -v node >/dev/null 2>&1; then echo 'Node.js is missing from PATH; install Node.js 22 or newer in the target environment' >&2; exit 127; fi; if ! node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)'; then echo 'Node.js 22 or newer is required in the target environment' >&2; exit 1; fi; if ! command -v npm >/dev/null 2>&1 || ! npm --version >/dev/null; then echo 'npm is missing or unusable; install npm in the target environment' >&2; exit 127; fi";
    let mut args = if profile.environment.contains_key("PATH") {
        vec![
            "-c".to_owned(),
            format!("export PATH=\"$1\"; {script}"),
            "mj-node-preflight".into(),
            profile.environment["PATH"].clone(),
        ]
    } else {
        vec!["-lc".to_owned(), script.to_owned()]
    };
    let (command, destination) = match template {
        TargetTemplate::LocalBare => (CommandSpec::new("sh", args), "local host".to_owned()),
        TargetTemplate::SshBare { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            args.insert(0, "sh".into());
            (crate::targets::ssh_command(&ssh, args), ssh.destination)
        }
        _ => unreachable!(),
    };
    execute_checked(executor, command.purpose("preflight managed harness Node.js and npm"))
        .with_context(|| format!("{} launch preflight failed on {destination}; Node.js 22+ and npm must be available on the target PATH", profile.kind.display_name()))?;
    Ok(())
}

pub(super) fn ensure_node_script() -> &'static str {
    "if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1 || ! command -v npx >/dev/null 2>&1; then echo 'Mjolnir needs Node.js, npm, and npx on PATH; install Node in the target environment' >&2; exit 127; fi"
}

pub(super) const MJ_CONTAINER_ENVIRONMENT: &str = "## Mjolnir disposable environment\n\nThis session runs in a disposable Mjolnir container. When the session closes, Mjolnir checkpoints everything in project workspace directories under `/workspace`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then removes the container.\n\nEverything outside `/workspace`, including installed packages, `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n";

pub(in crate::controller) fn stage_profile(
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
) -> Result<()> {
    let harness = profile.kind;
    let source = profile.home.as_path();
    std::fs::create_dir_all(destination)?;
    // Only the files peculiar to a harness are listed per harness. The
    // instruction file and the synced skill directories are the same facts the
    // rest of Hel reads off `HarnessKind`, so they are appended from there
    // rather than repeated in all five arms.
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
    allowlist.par_iter().try_for_each(|name| -> Result<()> {
        let from = source.join(name);
        if from.exists() {
            copy_profile_entry(&from, &destination.join(name))?;
        }
        Ok(())
    })?;
    Ok(())
}
