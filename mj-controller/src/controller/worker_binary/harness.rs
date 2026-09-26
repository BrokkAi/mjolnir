use super::*;
use mj_core::harness_runtime::{GROK_VERSION, KIMI_VERSION};

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
        | targets::TargetLocator::SshDocker { .. } => {
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

/// The Kimi and Grok fallbacks install the pinned release before starting
/// the ACP server. The installer writes progress on stdout, which is the ACP
/// stream, so its stdout goes to stderr, where the worker keeps the bridge's
/// diagnostics (#1136).
pub(in crate::controller) fn bridge_launch(
    harness: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
) -> (String, Vec<String>) {
    match harness {
        mj_core::config::HarnessKind::Muse => ("muse-acp".into(), Vec::new()),
        mj_core::config::HarnessKind::Codex | mj_core::config::HarnessKind::Claude => {
            let bridge =
                mj_core::harness_runtime::npm_bridge(harness).expect("npm harness has a bridge");
            ("sh".into(), vec!["-c".into(), bridge.bootstrap_script()])
        }
        mj_core::config::HarnessKind::Kimi => (
            "sh".into(),
            vec![
                "-c".into(),
                format!(
                    "if command -v kimi >/dev/null 2>&1; then exec kimi acp; elif [ -x \"$HOME/.kimi-code/bin/kimi\" ]; then exec \"$HOME/.kimi-code/bin/kimi\" acp; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://code.kimi.com/kimi-code/install.sh | KIMI_VERSION={KIMI_VERSION} bash >&2 && exec \"$HOME/.kimi-code/bin/kimi\" acp; else echo 'Mjolnir needs compatible Kimi Code or curl for its official installer; add the tool to PATH' >&2; exit 127; fi"
                ),
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
                        "if command -v grok >/dev/null 2>&1; then exec grok {acp}; elif [ -x \"$GROK_HOME/bin/grok\" ]; then exec \"$GROK_HOME/bin/grok\" {acp}; elif [ -x \"$HOME/.grok/bin/grok\" ]; then exec \"$HOME/.grok/bin/grok\" {acp}; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://x.ai/cli/install.sh | bash -s {GROK_VERSION} >&2 && exec \"$HOME/.grok/bin/grok\" {acp}; else echo 'Mjolnir needs compatible Grok Build or curl for its official installer; add the tool to PATH' >&2; exit 127; fi"
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
    // Mjolnir installs its own pinned copy of the agent with Node.js and npm,
    // so the agent's own command is not a prerequisite. When Node.js or npm
    // is missing, though, the failure first says whether the agent itself is
    // missing too, because that is what a person on a fresh machine needs to
    // hear (launch finding R13-1).
    let cli = profile.kind.cli_binary_name();
    let script = format!(
        "status=0; if ! command -v node >/dev/null 2>&1; then echo 'Node.js is missing from PATH; install Node.js 22 or newer in the target environment' >&2; status=127; elif ! node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)'; then echo 'Node.js 22 or newer is required in the target environment' >&2; status=1; elif ! command -v npm >/dev/null 2>&1 || ! npm --version >/dev/null; then echo 'npm is missing or unusable; install npm in the target environment' >&2; status=127; fi; if [ \"$status\" -ne 0 ] && ! command -v {cli} >/dev/null 2>&1; then exit {HARNESS_CLI_MISSING_STATUS}; fi; exit \"$status\""
    );
    let mut args = if profile.environment.contains_key("PATH") {
        vec![
            "-c".to_owned(),
            format!("export PATH=\"$1\"; {script}"),
            "mj-node-preflight".into(),
            profile.environment["PATH"].clone(),
        ]
    } else {
        vec!["-lc".to_owned(), script]
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
    let command = command.purpose("preflight managed harness Node.js and npm");
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
    let name = profile.kind.display_name();
    Err(if output.status == HARNESS_CLI_MISSING_STATUS {
        failure.context(format!(
            "{name} is not installed on {destination}: `{cli}` is not on PATH. {}, sign in to it, then retry the launch",
            profile.kind.install_advice()
        ))
    } else {
        failure.context(format!(
            "{name} launch preflight failed on {destination}; Node.js 22+ and npm must be available on the target PATH"
        ))
    })
}

/// The exit status the harness preflight script uses when Node.js or npm is
/// unusable and the agent's own command is missing as well.
const HARNESS_CLI_MISSING_STATUS: i32 = 3;

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
    // What the harness maintains itself inside an allowlisted entry, such as
    // the skills Claude Code syncs from the user's claude.ai account, stays
    // behind: the harness provisions its own copy in the session home.
    let harness_owned = harness
        .harness_owned_skill_paths()
        .iter()
        .map(|owned| source.join(owned))
        .collect::<Vec<_>>();
    allowlist.par_iter().try_for_each(|name| -> Result<()> {
        let from = source.join(name);
        if from.exists() {
            copy_profile_entry_except(&from, &destination.join(name), &harness_owned)?;
        }
        Ok(())
    })?;
    Ok(())
}
