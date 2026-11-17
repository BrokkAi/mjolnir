//! Container project recipes run after clone; failures remain repairable in-session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use mj_core::relay::RelayObservation;
use mj_core::session_image::RECIPE_FILES;

use crate::relay::RelayCell;
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
    // The build-cache launchers (tool_cache::prepare) wrap tools such as cmake
    // and must stay ahead of the project's tools; they find the real tool,
    // here a mise shim, after their own directory.
    let wrappers = environment
        .get("MJ_BUILD_TOOL_BIN")
        .map(PathBuf::from)
        .filter(|bin| std::env::split_paths(&old).any(|entry| &entry == bin));
    let mise_dir = mise.and_then(Path::parent);
    let path = std::env::join_paths(
        wrappers
            .clone()
            .into_iter()
            .chain(std::iter::once(shims.clone()))
            .chain(mise_dir.map(Path::to_owned))
            .chain(std::env::split_paths(&old).filter(|entry| {
                entry != &shims
                    && mise_dir != Some(entry.as_path())
                    && wrappers.as_ref() != Some(entry)
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

/// What one recipe checkout still needs, from mise's own status commands.
pub(super) struct Missing {
    root: PathBuf,
    /// Recipe files present in the checkout.
    files: Vec<&'static str>,
    /// `tool@version` requests that are not installed.
    tools: Vec<String>,
    /// `manager:package` entries of `[bootstrap.packages]` that are missing.
    packages: Vec<String>,
    /// Set when a status command failed; the message then names the recipe
    /// files instead of guessing tool names.
    status_failure: Option<String>,
}

impl Missing {
    fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.packages.is_empty() && self.status_failure.is_none()
    }

    fn describe(&self) -> String {
        let files = self.files.join(", ");
        let mut items = self.tools.clone();
        items.extend(self.packages.iter().cloned());
        if self.status_failure.is_some() || items.is_empty() {
            format!(
                "everything the recipe in {} declares ({files}; mise could not say what is missing)",
                self.root.display()
            )
        } else {
            format!(
                "{} (from {files} in {})",
                items.join(", "),
                self.root.display()
            )
        }
    }
}

/// The fast part of project tools, finished before the harness starts: mise is
/// present, the session environment is applied, and mise has said what is
/// missing. `None` when nothing is missing.
pub(super) struct Install {
    mise: PathBuf,
    environment: BTreeMap<String, String>,
    missing: Vec<Missing>,
    /// Unique per worker start, so a resumed session's messages are never
    /// mistaken for an earlier launch's already-delivered ones.
    launch_ms: i64,
}

async fn status_output(
    mise: &Path,
    root: &Path,
    environment: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new(mise);
    command
        .args(args)
        .current_dir(root)
        .env_clear()
        .envs(environment);
    let output = mj_core::subprocess::capture(&mut command)
        .await
        .with_context(|| format!("mise {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "mise {} failed: {}: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

async fn status(
    mise: &Path,
    root: &Path,
    environment: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<serde_json::Value> {
    serde_json::from_slice(&status_output(mise, root, environment, args).await?)
        .with_context(|| format!("read mise {} output", args.join(" ")))
}

async fn missing_in(
    mise: &Path,
    root: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<Missing> {
    let mut files = Vec::new();
    for file in RECIPE_FILES {
        if tokio::fs::try_exists(root.join(file))
            .await
            .with_context(|| format!("check project recipe {}", root.join(file).display()))?
        {
            files.push(*file);
        }
    }
    let mut missing = Missing {
        root: root.to_owned(),
        files,
        tools: Vec::new(),
        packages: Vec::new(),
        status_failure: None,
    };
    // `mise ls --missing --json`: an object keyed by tool name whose records
    // list the requested versions that are not installed.
    // `mise bootstrap packages status --json`: package managers with packages
    // whose `state` is "missing" when not installed. Other states (a version
    // mismatch for a "*" request) are left to `apply`, which is a quick no-op.
    let result = async {
        // `ls --missing` silently omits a request mise cannot resolve (an
        // unknown tool); a dry run fails for it, and the failure then makes the
        // installation run so the broken recipe is reported.
        status_output(mise, root, environment, &["install", "--dry-run"]).await?;
        let tools = status(mise, root, environment, &["ls", "--missing", "--json"]).await?;
        for (tool, records) in tools.as_object().into_iter().flatten() {
            for record in records.as_array().into_iter().flatten() {
                if record["installed"] == false {
                    let version = record["requested_version"]
                        .as_str()
                        .or_else(|| record["version"].as_str())
                        .unwrap_or("latest");
                    missing.tools.push(format!("{tool}@{version}"));
                }
            }
        }
        let packages = status(
            mise,
            root,
            environment,
            &["bootstrap", "packages", "status", "--json"],
        )
        .await?;
        for (manager, entry) in packages.as_object().into_iter().flatten() {
            for package in entry["packages"].as_array().into_iter().flatten() {
                if package["desired_state"] == "present" && package["state"] == "missing" {
                    missing.packages.push(format!(
                        "{manager}:{}",
                        package["package"].as_str().unwrap_or("?")
                    ));
                }
            }
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = result {
        missing.tools.clear();
        missing.packages.clear();
        missing.status_failure = Some(format!("{error:#}"));
    }
    Ok(missing)
}

/// Install mise if the image has none, apply the session environment and ask
/// mise what each recipe checkout is missing.
pub(super) async fn plan(
    worker_root: &Path,
    roots: &[PathBuf],
    environment: &mut BTreeMap<String, String>,
) -> Result<Option<Install>> {
    let mise = mj_core::harness_install::ensure_mise_runtime(environment)
        .await
        .context("install mise runtime")?;
    apply_environment(environment, roots, Some(&mise))?;
    let mut missing = Vec::new();
    for root in roots {
        let found = missing_in(&mise, root, environment).await?;
        if let Some(failure) = &found.status_failure {
            append_log(
                worker_root,
                &format!(
                    "\nmise could not report what is missing in {}: {failure}\n",
                    root.display()
                ),
            )
            .await?;
        }
        if !found.is_empty() {
            missing.push(found);
        }
    }
    Ok((!missing.is_empty()).then(|| Install {
        mise,
        environment: environment.clone(),
        missing,
        launch_ms: chrono::Utc::now().timestamp_millis(),
    }))
}

async fn append_log(worker_root: &Path, text: &str) -> Result<()> {
    let mut log = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(worker_root.join(LOG_FILE))
        .await
        .context("open project tools log")?;
    log.write_all(text.as_bytes()).await?;
    Ok(())
}

const MESSAGE_SOURCE: &str = "project-tools";

impl Install {
    /// Tell the agent, before it starts, what is being installed.
    pub(super) fn tell_agent_started(&self, relay: &RelayCell) -> Result<()> {
        let items = self
            .missing
            .iter()
            .map(Missing::describe)
            .collect::<Vec<_>>()
            .join("; ");
        let text = format!(
            "Mjolnir: this session's project tools are being installed in the background: {items}. \
             Commands that need those tools may wait or fail until Mjolnir sends a second message saying the install finished."
        );
        relay
            .lock()
            .expect("relay state lock poisoned")
            .deliver_agent_message(
                MESSAGE_SOURCE,
                &format!("project-tools-{}-started", self.launch_ms),
                text,
            )
    }

    /// Run the installation with no deadline, then tell the agent the result.
    /// Dropping the future terminates the running command's process group.
    pub(super) async fn run(mut self, worker_root: &Path, relay: &RelayCell) {
        let key = format!("project-tools-{}-finished", self.launch_ms);
        let (notices, failures) = match self.install(worker_root).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let failure = format!("project tools installation could not run: {error:#}");
                (
                    vec![format!(
                        "Project tools: {failure}. See {}. The session will continue.",
                        worker_root.join(LOG_FILE).display()
                    )],
                    vec![failure],
                )
            }
        };
        let log_path = worker_root.join(LOG_FILE);
        let text = if failures.is_empty() {
            "Mjolnir: the project tools finished installing. Everything the recipe declares is now available.".to_owned()
        } else {
            format!(
                "Mjolnir: the project tools did not finish installing: {}. Output is in {}. \
                 The session is otherwise unaffected: repair the recipe and rerun `mise install` (and `mise bootstrap packages apply --yes` for package failures) in the checkout.",
                failures.join("; "),
                log_path.display()
            )
        };
        let mut relay = relay.lock().expect("relay state lock poisoned");
        for message in notices {
            if let Err(error) = relay.record_observation(RelayObservation::Notice { message }) {
                tracing::warn!(%error, "record project tools failure notice");
            }
        }
        if let Err(error) = relay.deliver_agent_message(MESSAGE_SOURCE, &key, text.clone()) {
            // The agent cannot be told, so the user is.
            tracing::warn!(%error, "queue project tools result for the agent");
            if let Err(error) = relay.record_observation(RelayObservation::Notice { message: text })
            {
                tracing::warn!(%error, "record project tools result notice");
            }
        }
    }

    async fn install(&mut self, worker_root: &Path) -> Result<(Vec<String>, Vec<String>)> {
        let log_path = worker_root.join(LOG_FILE);
        let mut log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
            .context("open project tools log")?;
        let mut notices = Vec::new();
        let mut failures = Vec::new();
        for missing in &self.missing {
            for args in [
                &["bootstrap", "packages", "apply", "--yes"][..],
                &["install"][..],
            ] {
                let label = format!("mise {} (in {})", args.join(" "), missing.root.display());
                log.write_all(format!("\n$ {label}\n").as_bytes()).await?;
                log.flush().await?;
                let mut command = tokio::process::Command::new(&self.mise);
                command
                    .args(args)
                    .current_dir(&missing.root)
                    .env_clear()
                    .envs(&self.environment);
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
                    failures.push(failure);
                    // A broken package recipe must not block independent tool installs.
                }
            }
        }
        Ok((notices, failures))
    }
}

/// The background installation, owned by the running session: dropping it
/// (session close, worker exit) aborts the task, which terminates the running
/// command's process group.
pub(super) struct BackgroundInstall(tokio::task::JoinHandle<()>);

impl BackgroundInstall {
    pub(super) fn spawn(install: Install, worker_root: PathBuf, relay: Arc<RelayCell>) -> Self {
        Self(tokio::spawn(async move {
            install.run(&worker_root, &relay).await;
        }))
    }
}

impl Drop for BackgroundInstall {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: review of b0f32d6: restoring the mise shims put them ahead of the build-cache launchers, bypassing mbx for mise-installed cmake.
    #[test]
    fn build_cache_launchers_stay_ahead_of_project_shims() {
        let mut environment = BTreeMap::from([
            (
                "PATH".to_owned(),
                "/managed-node/bin:/wrap:/usr/bin".to_owned(),
            ),
            ("MJ_BUILD_TOOL_BIN".to_owned(), "/wrap".to_owned()),
            ("MISE_DATA_DIR".to_owned(), "/data".to_owned()),
        ]);
        apply_environment(&mut environment, &[PathBuf::from("/workspace/repo")], None).unwrap();
        assert_eq!(
            environment["PATH"],
            "/wrap:/data/shims:/managed-node/bin:/usr/bin"
        );
    }
}
