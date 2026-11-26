//! Container project recipes run after clone; failures remain repairable in-session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use mj_core::relay::RelayObservation;
use mj_harness_install::session_image::{IMAGE_RECIPE_DIR, RECIPE_FILES};

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
    // Launcher wrappers must precede mise's toolchain shims.
    let wrappers: Vec<_> = super::LAUNCHER_BIN_ENVS
        .iter()
        .filter_map(|name| environment.get(*name))
        .filter(|bin| !bin.is_empty())
        .map(PathBuf::from)
        .collect();
    let mise_dir = mise.and_then(Path::parent);
    let path = std::env::join_paths(
        wrappers
            .iter()
            .cloned()
            .chain(std::iter::once(shims.clone()))
            .chain(mise_dir.map(Path::to_owned))
            .chain(std::env::split_paths(&old).filter(|entry| {
                entry != &shims && mise_dir != Some(entry.as_path()) && !wrappers.contains(entry)
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
    /// The recipe file whose `[hooks] postinstall` still has to run.
    hook: Option<&'static str>,
}

impl Missing {
    fn is_empty(&self) -> bool {
        self.tools.is_empty()
            && self.packages.is_empty()
            && self.status_failure.is_none()
            && self.hook.is_none()
    }

    fn describe(&self) -> String {
        let files = self.files.join(", ");
        let mut items = self.tools.clone();
        items.extend(self.packages.iter().cloned());
        if let Some(hook) = self.hook {
            items.push(format!("the postinstall hook in {hook}"));
        }
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
        hook: None,
    };
    // `mise ls --missing --json`: an object keyed by tool name whose records
    // list the requested versions that are not installed.
    // `mise bootstrap packages status --json`: see [`unsatisfied_packages`].
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
        missing.packages = unsatisfied_packages(&packages);
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = result {
        missing.tools.clear();
        missing.packages.clear();
        missing.status_failure = Some(format!("{error:#}"));
    }
    if missing.is_empty() {
        missing.hook = pending_postinstall_hook(root, Path::new(IMAGE_RECIPE_DIR)).await?;
    }
    Ok(missing)
}

/// A global `[hooks] postinstall` runs on every `mise install`, and no status
/// command says whether it is satisfied. An image built from exactly this
/// recipe ran it at build time; anywhere else (a stale image, or a container
/// whose earlier install stopped during the hook) it still has to run. Returns
/// the recipe file that declares the hook in that case.
async fn pending_postinstall_hook(root: &Path, baked: &Path) -> Result<Option<&'static str>> {
    let mut hook = None;
    let mut built_from_this_recipe = true;
    for file in RECIPE_FILES {
        let read = |path: PathBuf| async move {
            match tokio::fs::read(&path).await {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
            }
        };
        let contents = read(root.join(file)).await?;
        if contents != read(baked.join(file)).await? {
            built_from_this_recipe = false;
        }
        let is_mise_config = file.ends_with(".toml") && *file != "rust-toolchain.toml";
        if let (true, Some(contents)) = (is_mise_config, contents) {
            let table: toml::Table = std::str::from_utf8(&contents)
                .ok()
                .and_then(|text| text.parse().ok())
                .with_context(|| format!("parse {}", root.join(file).display()))?;
            if table
                .get("hooks")
                .and_then(|hooks| hooks.get("postinstall"))
                .is_some()
            {
                hook.get_or_insert(*file);
            }
        }
    }
    Ok(hook.filter(|_| !built_from_this_recipe))
}

/// `[bootstrap.packages]` entries that `mise bootstrap packages apply` would
/// change, from `mise bootstrap packages status --json`: package managers whose
/// packages carry a `state` of "installed", "missing" or "version_mismatch".
/// mise 2026.11.9 reports a `*` request as a version mismatch whatever version
/// is installed, so that one mismatch counts as satisfied.
fn unsatisfied_packages(status: &serde_json::Value) -> Vec<String> {
    let mut unsatisfied = Vec::new();
    for (manager, entry) in status.as_object().into_iter().flatten() {
        for package in entry["packages"].as_array().into_iter().flatten() {
            let requested = package["requested_version"].as_str().unwrap_or("latest");
            let needed = package["desired_state"] == "present"
                && match package["state"].as_str() {
                    Some("installed") => false,
                    Some("version_mismatch") => requested != "*",
                    _ => true,
                };
            if needed {
                let name = package["package"].as_str().unwrap_or("?");
                unsatisfied.push(match requested {
                    "latest" | "*" => format!("{manager}:{name}"),
                    version => format!("{manager}:{name}={version}"),
                });
            }
        }
    }
    unsatisfied
}

/// Install mise if the image has none, apply the session environment and ask
/// mise what each recipe checkout is missing.
pub(super) async fn plan(
    worker_root: &Path,
    roots: &[PathBuf],
    environment: &mut BTreeMap<String, String>,
) -> Result<Option<Install>> {
    let mise = mj_harness_install::harness_install::ensure_mise_runtime(environment)
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
             Until Mjolnir sends a second message saying the install finished, commands that need those tools may wait, fail, \
             or run a different version the image already has, so hold off on builds and tests that depend on them."
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

/// The background installation, owned by the running harness: stopping or
/// dropping it aborts the task, which terminates the running command's process
/// group.
pub(super) struct BackgroundInstall(tokio::task::JoinHandle<()>);

impl BackgroundInstall {
    pub(super) fn spawn(install: Install, worker_root: PathBuf, relay: Arc<RelayCell>) -> Self {
        Self(mj_core::runtime::spawn(async move {
            install.run(&worker_root, &relay).await;
        }))
    }
}

impl BackgroundInstall {
    /// Stop a running installation and wait until its task, and with it the
    /// command's process group, is gone.
    pub(super) async fn stop(&mut self) {
        self.0.abort();
        if let Err(error) = (&mut self.0).await
            && error.is_panic()
        {
            tracing::warn!(%error, "project tools installation panicked");
        }
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

    // Hard-won: review of 562a30751: an image whose tools were current skipped `mise install`, and with it a changed postinstall hook.
    #[tokio::test]
    async fn postinstall_hook_runs_unless_the_image_was_built_from_this_recipe() {
        let checkout = tempfile::tempdir().unwrap();
        let baked = tempfile::tempdir().unwrap();
        let recipe =
            "[tools]\nnode = \"24\"\n[hooks]\npostinstall = \"npx playwright@1.62.1 install\"\n";
        std::fs::write(checkout.path().join("mise.toml"), recipe).unwrap();
        std::fs::write(checkout.path().join(".nvmrc"), "24\n").unwrap();
        std::fs::write(baked.path().join("mise.toml"), recipe).unwrap();
        // Same hook, but the image lacks a recipe file the checkout has.
        let pending = pending_postinstall_hook(checkout.path(), baked.path());
        assert_eq!(pending.await.unwrap(), Some("mise.toml"));
        std::fs::write(baked.path().join(".nvmrc"), "24\n").unwrap();
        let pending = pending_postinstall_hook(checkout.path(), baked.path());
        assert_eq!(pending.await.unwrap(), None);
        std::fs::write(
            checkout.path().join("mise.toml"),
            recipe.replace("1.62.1", "1.63.0"),
        )
        .unwrap();
        let pending = pending_postinstall_hook(checkout.path(), baked.path());
        assert_eq!(pending.await.unwrap(), Some("mise.toml"));
        let no_image_recipe = baked.path().join("absent");
        let pending = pending_postinstall_hook(checkout.path(), &no_image_recipe);
        assert_eq!(pending.await.unwrap(), Some("mise.toml"));
    }

    // Hard-won: review of a045f704a: a version mismatch was not counted, so a recipe pinning a package the image has at another version skipped `apply`.
    #[test]
    fn package_version_mismatches_need_apply_except_for_any_version() {
        // Output of mise 2026.11.9 on Ubuntu noble.
        let status = serde_json::json!({"apt": {"available": true, "packages": [
            {"package": "curl", "requested_version": "latest", "desired_state": "present",
             "state": "installed", "installed_version": "8.5.0-2ubuntu10.16"},
            {"package": "git", "requested_version": "1:2.30.0-1", "desired_state": "present",
             "state": "version_mismatch", "installed_version": "1:2.43.0-1ubuntu7.4"},
            {"package": "cowsay", "requested_version": "latest", "desired_state": "present",
             "state": "missing", "installed_version": ""},
            {"package": "make", "requested_version": "*", "desired_state": "present",
             "state": "version_mismatch", "installed_version": "4.3-4.1build2"}
        ]}});
        assert_eq!(
            unsatisfied_packages(&status),
            ["apt:git=1:2.30.0-1", "apt:cowsay"]
        );
    }

    // Hard-won: review of b0f32d6: restoring the mise shims put them ahead of the build-cache launchers, bypassing mbx for mise-installed cmake.
    #[test]
    fn build_cache_launchers_stay_ahead_of_project_shims() {
        let mut environment = BTreeMap::from([
            (
                "PATH".to_owned(),
                "/managed-node/bin:/worker/bin:/wrap:/usr/bin".to_owned(),
            ),
            ("MJ_BUILD_TOOL_BIN".to_owned(), "/wrap".to_owned()),
            ("MJ_GITHUB_CLI_BIN".to_owned(), "/worker/bin".to_owned()),
            ("MBX_SHIMS_DIR".to_owned(), "/worker/mbx-shims".to_owned()),
            ("MISE_DATA_DIR".to_owned(), "/data".to_owned()),
        ]);
        apply_environment(&mut environment, &[PathBuf::from("/workspace/repo")], None).unwrap();
        assert_eq!(
            environment["PATH"],
            "/wrap:/worker/bin:/data/shims:/managed-node/bin:/usr/bin"
        );
    }
    // Hard-won: #1589: isolated Git setup hid the stable cargo and mbx launchers.
    #[test]
    fn mbx_launchers_survive_shell_reset_and_git_reload() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        for tool in ["cargo", "mbx", "gh"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mise_shims = root.join("mise/shims");
        std::fs::create_dir_all(&mise_shims).unwrap();
        std::fs::write(mise_shims.join("cargo"), "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(
            mise_shims.join("cargo"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut environment = BTreeMap::from([
            ("PATH".into(), "/opt/mise/shims:/usr/bin:/bin".into()),
            (
                "MBX_SHIMS_DIR".into(),
                root.join("mbx-shims").display().to_string(),
            ),
            (
                "MISE_DATA_DIR".into(),
                root.join("mise").display().to_string(),
            ),
        ]);
        let mut bare = BTreeMap::new();
        super::super::restore_mbx_launcher(root, None, &mut bare).unwrap();
        assert!(!bare.contains_key("MJ_MBX_BIN"));
        for inherited_path in [None, Some("")] {
            let mut env = BTreeMap::from([("MJ_MBX_BIN".into(), bin.display().to_string())]);
            if let Some(path) = inherited_path {
                env.insert("PATH".into(), path.into());
            }
            super::super::restore_mbx_launcher(root, None, &mut env).unwrap();
            assert_eq!(env["PATH"], bin.display().to_string());
            if let Some(path) = inherited_path {
                env.insert("PATH".into(), path.into());
            } else {
                env.remove("PATH");
            }
            let output = std::process::Command::new("/bin/sh")
                .env_clear()
                .envs(&env)
                .args([
                    "-c",
                    &(format!(
                        "{}{}",
                        if inherited_path.is_none() {
                            "unset PATH\n"
                        } else {
                            ""
                        },
                        super::super::launcher_path_shell()
                    ) + "printf '%s' \"$PATH\""),
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, bin.to_str().unwrap().as_bytes());
        }
        let relative = ".local/share/mjolnir/workers/sess/bin";
        for home in [Some(root), None, Some(Path::new("relative-home"))] {
            let mut env = BTreeMap::from([("MJ_MBX_BIN".into(), relative.into())]);
            super::super::restore_mbx_launcher(root, home, &mut env).unwrap();
            let expected = if home == Some(root) {
                root.join(relative)
            } else {
                bin.clone()
            };
            assert_eq!(env["MJ_MBX_BIN"], expected.to_str().unwrap());
            assert_eq!(env["PATH"], expected.to_str().unwrap());
        }
        for generation in ["harness-git-first", "harness-git-reload"] {
            let git_bin = root.join(generation).join("bin");
            std::fs::create_dir_all(&git_bin).unwrap();
            let git_gh = git_bin.join("gh");
            std::fs::write(&git_gh, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&git_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            environment.insert("MJ_GITHUB_CLI_BIN".into(), git_bin.display().to_string());
            super::super::restore_mbx_launcher(root, None, &mut environment).unwrap();
            let check = |env: &BTreeMap<String, String>, reset: bool| {
                let script = "command -v cargo; command -v mbx";
                let mut command = std::process::Command::new("/bin/bash");
                command.env_clear().envs(env).args(["-c", script]);
                if reset {
                    command.env("PATH", "/opt/mise/shims:/usr/bin:/bin");
                }
                let output = command.output().unwrap();
                assert!(output.status.success());
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    format!("{}/cargo\n{}/mbx\n", bin.display(), bin.display())
                );
            };
            check(&environment, false);
            apply_environment(&mut environment, &[root.to_owned()], None).unwrap();
            check(&environment, false);
            super::super::shell_environment::configure(&root.join("bash-env"), &mut environment)
                .unwrap();
            check(&environment, true);
            let mut command = std::process::Command::new("/bin/bash");
            let output = command.env_clear().envs(&environment)
                .env("PATH", format!("/opt/mise/shims:{}:/usr/bin:/bin:{}", bin.display(), bin.display()))
                .args(["-c", ". \"$MJ_SESSION_BASH_ENV\"; . \"$MJ_SESSION_BASH_ENV\"; command -v gh; command -v cargo; printf '%s' \"$PATH\""])
                .output().unwrap();
            assert!(output.status.success());
            let output = String::from_utf8(output.stdout).unwrap();
            let mut lines = output.splitn(3, '\n');
            assert_eq!(lines.next().unwrap(), git_gh.to_str().unwrap());
            assert_eq!(lines.next().unwrap(), bin.join("cargo").to_str().unwrap());
            let path = lines.next().unwrap();
            assert_eq!(
                std::env::split_paths(path)
                    .filter(|entry| entry == &bin)
                    .count(),
                1
            );
            let mut path_entries = std::env::split_paths(path);
            assert_eq!(path_entries.next().unwrap(), git_bin);
            assert_eq!(path_entries.next().unwrap(), bin);
        }
    }
}
