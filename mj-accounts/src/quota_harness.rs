//! Host CLI resolution shared by quota probes and login renewals.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mj_core::config::{ExecutionPolicy, HarnessKind};
use mj_core::harness_install::Located;

pub(crate) struct QuotaProgram {
    pub command: PathBuf,
    pub environment: HashMap<String, String>,
    // Keep the harness and managed Node leases until the child exits.
    pub _installation: Option<Located>,
}

pub(crate) async fn locate(
    harness: HarnessKind,
    environment: &HashMap<String, String>,
) -> Result<QuotaProgram> {
    let search: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .chain(environment.clone())
        .collect();
    let name = harness.cli_binary_name();
    let program_search = search.clone();
    let command = tokio::task::spawn_blocking(move || {
        mj_core::program_path::find_executable_on_path(
            name,
            program_search.get("PATH").map(std::ffi::OsStr::new),
        )
        .or_else(|| {
            // Grok's official installer also installs under the profile home.
            if harness != HarnessKind::Grok {
                return None;
            }
            let bin = PathBuf::from(program_search.get("GROK_HOME")?).join("bin");
            mj_core::program_path::find_executable_on_path(name, Some(bin.as_os_str()))
        })
    })
    .await
    .context("find host quota harness task failed")?;
    if let Some(command) = command {
        return Ok(QuotaProgram {
            command,
            environment: HashMap::new(),
            _installation: None,
        });
    }
    let policy = ExecutionPolicy::ConfiguredApprovals;
    let (command, args) = mj_core::harness_runtime::fallback_launch(harness, policy);
    let located = mj_core::harness_install::locate(
        mj_core::placement::local_harness_runtime(),
        harness,
        policy,
        Path::new(&command),
        &args,
        &search,
    )
    .await
    .with_context(|| format!("no {name} to run for quota: could not prepare the pinned harness"))?;
    let command = located
        .cli_entrypoint
        .clone()
        .with_context(|| format!("no {name} to run for quota: installation names no CLI"))?;
    Ok(QuotaProgram {
        command,
        environment: located.environment.clone().into_iter().collect(),
        _installation: Some(located),
    })
}
