//! Host CLI resolution shared by quota probes and login renewals.

#[cfg(unix)]
use std::collections::BTreeMap;
use std::collections::HashMap;
#[cfg(unix)]
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

pub(crate) struct QuotaProgram {
    pub command: PathBuf,
    pub environment: HashMap<String, String>,
    // Keep the harness and managed Node leases until the child exits.
    pub _installation: Option<Installation>,
}

pub(crate) async fn locate(
    harness: HarnessKind,
    environment: &HashMap<String, String>,
) -> Result<QuotaProgram> {
    let name = harness.cli_binary_name();
    let program_search = environment.clone();
    let inherited_path = std::env::var_os("PATH");
    let command = tokio::task::spawn_blocking(move || {
        mj_core::program_path::find_executable_on_path(
            name,
            program_search
                .iter()
                .filter(|(key, _)| {
                    if cfg!(windows) {
                        key.eq_ignore_ascii_case("PATH")
                    } else {
                        *key == "PATH"
                    }
                })
                // CommandSpec passes its BTreeMap environment in key order;
                // Windows folds aliases, so the last spelling wins there too.
                .max_by(|left, right| left.0.cmp(right.0))
                .map(|(_, value)| std::ffi::OsStr::new(value))
                .or(inherited_path.as_deref()),
        )
        .map(|selected| {
            // Ambient programs launch through the child's native PATH. Keep
            // non-UTF-8 directories out of CommandSpec's UTF-8 program field;
            // retain the selected suffix for Windows npm shims.
            PathBuf::from(selected.file_name().expect("CLI lookup names a file"))
        })
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
    #[cfg(not(unix))]
    anyhow::bail!(
        "no {name} to run for quota: Mjolnir has no managed installation on this platform and the CLI is not on PATH"
    );
    #[cfg(unix)]
    {
        let search: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .chain(environment.clone())
            .collect();
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
        .with_context(|| {
            format!("no {name} to run for quota: could not prepare the pinned harness")
        })?;
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
}
