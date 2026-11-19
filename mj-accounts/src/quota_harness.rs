//! Host CLI resolution shared by quota probes and login renewals.

#[cfg(unix)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{Context, Result};
#[cfg(unix)]
use mj_core::config::ExecutionPolicy;
use mj_core::config::HarnessKind;
#[cfg(unix)]
type Installation = mj_core::harness_install::Located;
#[cfg(not(unix))]
type Installation = std::convert::Infallible;

use crate::quota::QuotaRefreshRequest;

pub(crate) struct QuotaProgram {
    pub command: String,
    pub environment: HashMap<String, String>,
    // Keep the harness and managed Node leases until the child exits.
    pub _installation: Option<Installation>,
}

/// CommandSpec carries UTF-8 text. Reject paths it cannot represent instead
/// of changing their bytes or asking the child's PATH to choose again.
pub(crate) fn command_text(command: &Path) -> Result<String> {
    command
        .to_str()
        .map(str::to_owned)
        .with_context(|| format!("quota harness path is not UTF-8: {:?}", command.as_os_str()))
}

fn environment_value(
    environment: &HashMap<String, String>,
    name: &str,
) -> Option<std::ffi::OsString> {
    environment
        .iter()
        .filter(|(key, _)| {
            if cfg!(windows) {
                key.eq_ignore_ascii_case(name)
            } else {
                *key == name
            }
        })
        // CommandSpec applies its BTreeMap environment in key order;
        // Windows folds aliases, so the last spelling wins there too.
        .max_by(|left, right| left.0.cmp(right.0))
        .map(|(_, value)| std::ffi::OsString::from(value))
        .or_else(|| std::env::var_os(name))
}

/// The host's own CLI for `harness`, when it has one: the program a quota
/// check runs without any installation. The one lookup both [`locate`] and
/// [`prepare_managed`] use, so a prewarm never installs what a check would
/// not.
async fn host_program(
    harness: HarnessKind,
    environment: &HashMap<String, String>,
) -> Result<Option<String>> {
    let name = harness.cli_binary_name();
    let path = environment_value(environment, "PATH");
    let home = environment_value(environment, "GROK_HOME");
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let selected = mj_core::program_path::find_executable_on_path(name, path.as_deref())
            .or_else(|| {
                if harness != HarnessKind::Grok {
                    return None;
                }
                // The official installer also uses the inherited profile home.
                let bin = PathBuf::from(home?).join("bin");
                mj_core::program_path::find_executable_on_path(name, Some(bin.as_os_str()))
            });
        selected
            .map(|path| {
                let absolute =
                    std::path::absolute(path).context("resolve absolute quota harness path")?;
                command_text(&absolute)
            })
            .transpose()
    })
    .await
    .context("find host quota harness task failed")?
}

/// The pinned managed installation a quota check runs when the host has no
/// CLI. The first call on a host does the cold install.
#[cfg(unix)]
async fn managed_installation(
    harness: HarnessKind,
    environment: &HashMap<String, String>,
) -> Result<Installation> {
    let name = harness.cli_binary_name();
    let search: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .chain(environment.clone())
        .collect();
    let policy = ExecutionPolicy::ConfiguredApprovals;
    let (command, args) = mj_core::harness_runtime::fallback_launch(harness, policy);
    mj_core::harness_install::locate(
        mj_core::placement::local_harness_runtime(),
        harness,
        policy,
        Path::new(&command),
        &args,
        &search,
    )
    .await
    .with_context(|| format!("no {name} to run for quota: could not prepare the pinned harness"))
}

/// Install, ahead of the first quota check, the pinned harness each request
/// would otherwise install inline. Only requests whose check runs a host
/// harness, on a host that does not provide it, are prepared, once per
/// harness; the daemon container's image carries its harnesses and installs
/// nothing. The leases are released on return: a check takes its own, and an
/// installation in progress is shared through the installer's own lock.
/// Failures are returned with their harness; nothing is retried here, because
/// the next check that needs the harness prepares it again.
#[cfg(unix)]
pub async fn prepare_managed(
    requests: &[QuotaRefreshRequest],
) -> Vec<(HarnessKind, anyhow::Error)> {
    if mj_core::placement::daemon_container().is_some() {
        return Vec::new();
    }
    let mut attempted = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    for request in requests {
        let harness = request.harness;
        if !request.runs_host_harness() || attempted.contains(&harness) {
            continue;
        }
        let environment: HashMap<String, String> =
            request.environment.clone().into_iter().collect();
        let prepared = match host_program(harness, &environment).await {
            Ok(Some(_)) => continue,
            Ok(None) => managed_installation(harness, &environment).await.map(drop),
            Err(error) => Err(error),
        };
        // Another profile on a different PATH may still need the install.
        if let Err(error) = prepared {
            failures.push((harness, error));
        }
        attempted.insert(harness);
    }
    failures
}

pub(crate) async fn locate(
    harness: HarnessKind,
    environment: &HashMap<String, String>,
) -> Result<QuotaProgram> {
    let name = harness.cli_binary_name();
    if let Some(command) = host_program(harness, environment).await? {
        return Ok(QuotaProgram {
            command,
            environment: HashMap::new(),
            _installation: None,
        });
    }
    #[cfg(not(unix))]
    anyhow::bail!(
        "no {name} to run for quota: Mjolnir has no managed installation on this platform and the CLI is not on PATH"
    );
    #[cfg(unix)]
    {
        let located = managed_installation(harness, environment).await?;
        let command = located
            .cli_entrypoint
            .clone()
            .with_context(|| format!("no {name} to run for quota: installation names no CLI"))?;
        Ok(QuotaProgram {
            command: command_text(&command)?,
            environment: located.environment.clone().into_iter().collect(),
            _installation: Some(located),
        })
    }
}
