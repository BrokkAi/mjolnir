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

#[cfg(unix)]
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

/// The environment the installer searches and caches under: this process's,
/// overlaid with the profile's.
#[cfg(unix)]
fn search_environment(environment: &HashMap<String, String>) -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .chain(environment.clone())
        .collect()
}

/// The pinned managed installation a quota check runs when the host has no
/// CLI. The first call on a host does the cold install.
#[cfg(unix)]
async fn managed_installation(
    harness: HarnessKind,
    environment: &HashMap<String, String>,
) -> Result<Installation> {
    let name = harness.cli_binary_name();
    let search = search_environment(environment);
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
/// would otherwise install inline. The caller passes only requests that run a
/// host harness, filtered with [`QuotaRefreshRequest::runs_host_harness`] on a
/// blocking thread, because it may read the profile's login file; they are prepared
/// only on a host that does not provide the harness. The installer
/// caches per `XDG_CACHE_HOME`/`HOME`, so work is grouped by (harness, cache
/// root): groups run concurrently, and within a group the profiles are tried
/// in order until one preparation succeeds, so one profile's broken
/// environment does not hide the others. The daemon container's image carries
/// its harnesses and installs nothing. The leases are released on return: a
/// check takes its own, and an installation in progress is shared through the
/// installer's own lock. Every failure is returned with its harness; nothing
/// is retried here, because the next check that needs the harness prepares it
/// again. Dropping the future cancels the installers' process groups.
#[cfg(unix)]
pub async fn prepare_managed(
    requests: &[QuotaRefreshRequest],
) -> Vec<(HarnessKind, anyhow::Error)> {
    if mj_core::placement::daemon_container().is_some() {
        return Vec::new();
    }
    let mut failures = Vec::new();
    let mut groups: BTreeMap<(HarnessKind, PathBuf), Vec<HashMap<String, String>>> =
        BTreeMap::new();
    for request in requests {
        let environment: HashMap<String, String> =
            request.environment.clone().into_iter().collect();
        let search = search_environment(&environment);
        match mj_core::harness_runtime::managed_harness_cache_root(
            search.get("XDG_CACHE_HOME").map(std::ffi::OsStr::new),
            search.get("HOME").map(std::ffi::OsStr::new),
        ) {
            Ok(root) => groups
                .entry((request.harness, root))
                .or_default()
                .push(environment),
            Err(error) => failures.push((request.harness, error)),
        }
    }
    let cancellation = mj_core::subprocess::preparation_cancellation();
    let results = futures::future::join_all(groups.into_iter().map(
        |((harness, _), environments)| {
            let cancellation = cancellation.clone();
            async move {
            let mut failures = Vec::new();
            for environment in environments {
                // The PATH lookup is blocking filesystem reads that a stalled
                // mount can hang, and shutdown must not wait for them. It only
                // reads, so abandoning it on cancellation is safe. Racing here
                // leaves the quota check's own `locate` unchanged.
                let host = tokio::select! {
                    () = cancellation.cancelled() => {
                        failures.push((harness, mj_core::subprocess::PreparationCancelled.into()));
                        break;
                    }
                    host = host_program(harness, &environment) => host,
                };
                match host {
                    Ok(Some(_)) => continue,
                    Ok(None) => match managed_installation(harness, &environment).await {
                        Ok(_lease) => break,
                        Err(error) => failures.push((harness, error)),
                    },
                    Err(error) => failures.push((harness, error)),
                }
            }
            failures
            }
        },
    ))
    .await;
    failures.extend(results.into_iter().flatten());
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
