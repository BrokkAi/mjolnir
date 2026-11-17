//! Container project recipes run after clone; failures remain repairable in-session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use mj_core::session_image::RECIPE_FILES;
pub(super) const LOG_FILE: &str = "project-tools.log";

/// Only the controller's explicit launch directive opts a worker in. Bare
/// targets never receive it; the harness never receives the internal directive.
pub(super) fn take_roots(
    environment: &mut BTreeMap<String, String>,
) -> Result<Option<Vec<PathBuf>>> {
    environment
        .remove(mj_core::worker_launch::SESSION_PROJECT_TOOLS_ROOTS)
        .map(|value| serde_json::from_str(&value).context("read project tool checkout roots"))
        .transpose()
}

/// Single owner for both preparation commands and the final harness environment,
/// applied only when a checkout has a recipe. Image-provided values win so
/// prebuilt installs remain reusable.
pub(super) fn apply_environment(
    environment: &mut BTreeMap<String, String>,
    roots: &[PathBuf],
    mise: Option<&Path>,
) -> Result<()> {
    let data = PathBuf::from(
        environment
            .entry("MISE_DATA_DIR".into())
            .or_insert_with(|| "/var/lib/hel/mise".into())
            .clone(),
    );
    // RUSTUP_HOME and CARGO_HOME are left to the image: unset, rustup uses
    // $HOME/.rustup and $HOME/.cargo, the same HOME for this step and the
    // harness, so an image's existing toolchain stays visible.
    let trusted = environment
        .get("MISE_TRUSTED_CONFIG_PATHS")
        .cloned()
        .unwrap_or_default();
    let trusted = std::env::join_paths(
        std::env::split_paths(&trusted)
            .filter(|path| !path.as_os_str().is_empty() && !roots.contains(path))
            .chain(roots.iter().cloned()),
    )?;
    environment.insert(
        "MISE_TRUSTED_CONFIG_PATHS".into(),
        trusted
            .into_string()
            .map_err(|_| anyhow::anyhow!("mise trusted paths are not UTF-8"))?,
    );
    environment.insert(
        "MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS".into(),
        "rust,node,python,ruby,go".into(),
    );
    let shims = data.join("shims");
    let old = environment.get("PATH").cloned().unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(shims.clone())
            .chain(mise.and_then(Path::parent).map(Path::to_owned))
            .chain(std::env::split_paths(&old).filter(|entry| {
                entry != &shims && mise.and_then(Path::parent) != Some(entry.as_path())
            })),
    )?;
    environment.insert(
        "PATH".into(),
        path.into_string()
            .map_err(|_| anyhow::anyhow!("mise PATH is not UTF-8"))?,
    );
    Ok(())
}

pub(super) async fn recipe_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for root in roots {
        for file in RECIPE_FILES {
            if tokio::fs::try_exists(root.join(file))
                .await
                .with_context(|| format!("check project recipe {}", root.join(file).display()))?
            {
                found.push(root.clone());
                break;
            }
        }
    }
    Ok(found)
}

pub(super) async fn install(
    worker_root: &Path,
    roots: &[PathBuf],
    environment: &mut BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let log_path = worker_root.join(LOG_FILE);
    let mut log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .context("open project tools log")?;
    let mise = mj_core::harness_install::ensure_mise_runtime(environment)
        .await
        .context("install mise runtime")?;
    apply_environment(environment, roots, Some(&mise))?;
    let mut notices = Vec::new();
    for root in roots {
        for args in [
            &["bootstrap", "packages", "apply", "--yes"][..],
            &["install"][..],
        ] {
            let label = format!("mise {} (in {})", args.join(" "), root.display());
            log.write_all(format!("\n$ {label}\n").as_bytes()).await?;
            log.flush().await?;
            let mut command = tokio::process::Command::new(&mise);
            command
                .args(args)
                .current_dir(root)
                .env_clear()
                .envs(&*environment);
            let output = log.try_clone().await?.into_std().await;
            let result = mj_core::subprocess::run_to_file_async(&mut command, output).await;
            let failure = match result {
                Ok(status) if status.success() => None,
                Ok(status) => Some(format!("{label} failed: {status}")),
                Err(error) => Some(format!("{label} failed: {error:#}")),
            };
            if let Some(failure) = failure {
                log.write_all(format!("{failure}\n").as_bytes()).await?;
                notices.push(format!("Project tools: {failure}. See {}. The session will continue; repair the recipe and rerun the command.", log_path.display()));
                // A broken package recipe must not block independent tool installs.
            }
        }
    }
    Ok(notices)
}
