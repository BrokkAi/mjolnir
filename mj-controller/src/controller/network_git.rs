//! Git initialization for independent, network-backed session checkouts.

use crate::targets::{self, CommandExecutor};
use anyhow::{Context, Result, ensure};
use mj_core::targets::SshTrafficClass;

pub(crate) use crate::targets::workspace_root;

use super::Controller;

impl Controller {
    /// Restores and retained profile switches share the launch cutoff policy.
    /// This runs after checkpoint restoration and before starting the worker.
    pub(super) fn verify_history_cutoff_workspace(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session is missing")?;
        let Some(cutoff) = super::backend::session_history_cutoff(session)? else {
            return Ok(());
        };
        let checkout = session
            .checkout
            .as_ref()
            .context("cutoff checkout is missing")?;
        let repository = session
            .project_bundle(&self.config)
            .context("session bundle is missing")?
            .repositories
            .iter()
            .find(|repo| repo.id == checkout.repository_id)
            .context("cutoff repository is missing")?;
        let workspace_directory = mj_core::target_path::join(
            std::path::Path::new(&workspace_root(
                backend,
                session.container_workspace.as_deref(),
            )),
            &repository.destination,
        );
        let directory = session
            .checkout()
            .project_directory()
            .map(std::borrow::Cow::into_owned)
            .unwrap_or(workspace_directory);
        let checked = |arguments: &[&str]| -> Result<String> {
            let mut args = vec![
                "git".into(),
                "-C".into(),
                directory.to_string_lossy().into_owned(),
            ];
            args.extend(arguments.iter().map(|argument| (*argument).to_owned()));
            let output = executor.execute(&targets::command_on_locator(
                SshTrafficClass::Control,
                backend,
                session_id,
                args,
                "verify workspace history cutoff",
            )?)?;
            ensure!(
                output.status == 0,
                "repository {}: Git {} failed: {}",
                directory.display(),
                arguments[0],
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        configure_workspace_without_origin(
            &checked,
            None,
            cutoff.base.as_deref().unwrap_or(&cutoff.head),
        )
    }

    /// Fresh workspaces start at the remote's advertised default branch. A
    /// matching marker makes retries harmless without resetting session work.
    pub(super) fn initialize_network_workspaces(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .context("session is missing")?;
        if self
            .state
            .checkout(session_id)?
            .project_directory()
            .is_some()
        {
            return Ok(());
        }
        let bundle = session
            .project_bundle(&self.config)
            .context("session bundle is missing")?;
        if let Some(checkout) = &session.checkout {
            ensure!(
                bundle
                    .repositories
                    .iter()
                    .any(|repo| repo.id == checkout.repository_id),
                "checkout repository is no longer in the configured bundle"
            );
        }
        let root = workspace_root(backend, session.container_workspace.as_deref());
        let initialize = |repository: &mj_core::config::ProjectRepository| {
            let directory = mj_core::target_path::join(
                std::path::Path::new(&root),
                std::path::Path::new(&repository.destination),
            );
            // With an exact checkout, the base belongs to the checked-out
            // repository alone; the others start at their default branch.
            let checkout = session
                .checkout
                .as_ref()
                .filter(|checkout| checkout.repository_id == repository.id);
            let launch_base = if session.checkout.is_some() && checkout.is_none() {
                None
            } else {
                session.launch_base.as_deref()
            };
            initialize_workspace(
                executor,
                backend,
                session_id,
                &directory,
                launch_base,
                session.launch_branch.as_deref(),
                checkout.map(|v| &**v),
            )
        };
        if let [repository] = bundle.repositories.as_slice() {
            return initialize(repository);
        }
        // Repositories are independent directories; every one is awaited, and
        // the first failure in bundle order wins.
        let owner = crate::worker_lifecycle::capture();
        let runtime = tokio::runtime::Handle::try_current().ok();
        std::thread::scope(|scope| {
            let handles = bundle
                .repositories
                .iter()
                .map(|repository| {
                    let (owner, runtime, initialize) = (&owner, &runtime, &initialize);
                    scope.spawn(move || {
                        let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
                        match owner {
                            Some(owner) => owner.scope_blocking(|| initialize(repository)),
                            None => initialize(repository),
                        }
                    })
                })
                .collect::<Vec<_>>();
            let mut first_error = None;
            for handle in handles {
                let result = handle.join().unwrap_or_else(|panic| {
                    Err(anyhow::anyhow!(
                        "concurrent repository initialization panicked: {}",
                        targets::command_thread_panic_message(panic.as_ref())
                    ))
                });
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
            }
            first_error.map_or(Ok(()), Err)
        })
    }
}

fn initialize_workspace(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    session_id: &str,
    directory: &std::path::Path,
    launch_base: Option<&str>,
    launch_branch: Option<&str>,
    checkout: Option<&mj_core::remote_git::ExactCheckout>,
) -> Result<()> {
    let git = |arguments: &[&str]| -> Result<targets::CommandOutput> {
        let mut args = vec![
            "git".into(),
            "-C".into(),
            directory.to_string_lossy().into_owned(),
        ];
        args.extend(arguments.iter().map(|argument| (*argument).to_owned()));
        executor.execute(&targets::command_on_locator(
            SshTrafficClass::Bulk,
            backend,
            session_id,
            args,
            "initialize network session Git checkout",
        )?)
    };
    let checked = |arguments: &[&str]| -> Result<String> {
        let output = git(arguments)?;
        ensure!(
            output.status == 0,
            "repository {}: Git {} failed: {}",
            directory.display(),
            arguments[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    };
    // A named base must resolve in the clone; the clone holds only what the
    // remote sent, so a host-only branch name is not there to resolve. Say so
    // rather than repeat Git's "unknown revision".
    let resolve_base = |revision: &str| {
        checked(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])
        .with_context(|| {
            format!(
                "base {revision:?} is not in the clone; name a commit SHA, a tag, or origin/<branch>"
            )
        })
    };
    if let Some(checkout) = checkout {
        prepare_exact_checkout(&git, &checked, session_id, checkout)?;
        let base = match launch_base {
            Some(revision) => resolve_base(revision)?,
            None => checkout.commit.clone(),
        };
        if checkout.no_later_history {
            return configure_workspace_without_origin(&checked, Some(&checkout.commit), &base);
        }
        return configure_workspace(executor, backend, session_id, directory, &base);
    }
    initialize_fresh_workspace(
        executor,
        backend,
        session_id,
        directory,
        launch_base,
        launch_branch,
    )
}

/// Every question a fresh workspace asks before it is written, answered by one
/// command. Directory, branch and base reach the script as positional
/// arguments, never as shell text. A completed marker ends the script at once,
/// so a retry costs this one command.
const FRESH_WORKSPACE_PROBES: &str = r#"probe marker git -C "$1" config --local --get mj.remoteWorkspace
case $? in 1) ;; *) exit 0 ;; esac
probe default_ref git -C "$1" symbolic-ref --quiet refs/remotes/origin/HEAD || exit 0
if [ "$2" = 1 ]; then
    probe check_branch git -C "$1" check-ref-format --branch "$3" || exit 0
    branch=$3
else
    case "$out" in
        refs/remotes/origin/*) branch=${out#refs/remotes/origin/} ;;
        *) branch= ;;
    esac
fi
[ -n "$branch" ] || exit 0
probe branch_tip git -C "$1" rev-parse --verify "refs/remotes/origin/${branch}^{commit}" || exit 0
if [ "$4" = 1 ]; then
    probe base git -C "$1" rev-parse --verify --end-of-options "${5}^{commit}" || exit 0
fi
probe local_branch git -C "$1" show-ref --verify --quiet "refs/heads/${branch}"
exit 0
"#;

/// One probe command, then one write command.
fn initialize_fresh_workspace(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    session_id: &str,
    directory: &std::path::Path,
    launch_base: Option<&str>,
    launch_branch: Option<&str>,
) -> Result<()> {
    const PURPOSE: &str = "initialize network session Git checkout";
    let script = format!("{}{FRESH_WORKSPACE_PROBES}", targets::PROBE_HELPER_SH);
    let flag = |present: bool| if present { "1" } else { "0" }.to_owned();
    let probe_command = targets::command_on_locator(
        SshTrafficClass::Bulk,
        backend,
        session_id,
        vec![
            "sh".into(),
            "-c".into(),
            script,
            "mj-init-workspace".into(),
            directory.to_string_lossy().into_owned(),
            flag(launch_branch.is_some()),
            launch_branch.unwrap_or_default().to_owned(),
            flag(launch_base.is_some()),
            launch_base.unwrap_or_default().to_owned(),
        ],
        PURPOSE,
    )?;
    let output = executor.execute(&probe_command)?;
    // The script exits 0 whatever its probes did, so another status means the
    // shell or the connection failed before the probes could speak.
    ensure!(
        output.status == 0,
        "{PURPOSE} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let probes = targets::parse_framed_probes(&output.stdout, targets::DEFAULT_PROBE_FRAMES);
    let answer = |name: &str| -> Result<&targets::CommandOutput> {
        probes
            .get(name)
            .with_context(|| format!("the workspace probe returned no `{name}` answer"))
    };
    let checked = |name: &str, verb: &str| -> Result<String> {
        let output = answer(name)?;
        ensure!(
            output.status == 0,
            "repository {}: Git {verb} failed: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout.clone())?.trim().to_owned())
    };
    let marker = answer("marker")?;
    ensure!(
        matches!(marker.status, 0 | 1),
        "could not inspect network workspace marker"
    );
    if marker.status == 0 {
        ensure!(
            String::from_utf8(marker.stdout.clone())?.trim() == "true",
            "invalid network workspace marker"
        );
        return Ok(());
    }
    let default_ref = checked("default_ref", "symbolic-ref")
        .context("the network remote has no usable default branch")?;
    let branch = match launch_branch {
        Some(branch) => {
            checked("check_branch", "check-ref-format")?;
            branch.to_owned()
        }
        None => default_ref
            .strip_prefix("refs/remotes/origin/")
            .filter(|branch| !branch.is_empty())
            .context("invalid default remote branch")?
            .to_owned(),
    };
    let branch_tip = checked("branch_tip", "rev-parse")?;
    let base = match launch_base {
        // A named base must resolve in the clone; the clone holds only what the
        // remote sent, so a host-only branch name is not there to resolve. Say
        // so rather than repeat Git's "unknown revision".
        Some(revision) => checked("base", "rev-parse").with_context(|| {
            format!(
                "base {revision:?} is not in the clone; name a commit SHA, a tag, or origin/<branch>"
            )
        })?,
        // Clone obtains origin/HEAD from the server, independent of host HEAD
        // and init.defaultBranch. An empty or misconfigured remote cannot seed
        // work.
        None => branch_tip.clone(),
    };
    let switch = match answer("local_branch")?.status {
        0 => vec!["switch".to_owned(), branch],
        1 => vec![
            "switch".into(),
            "--no-track".into(),
            "-c".into(),
            branch,
            branch_tip,
        ],
        _ => anyhow::bail!("could not inspect the selected local branch"),
    };
    let mut steps = vec![targets::CommandStep::new(
        format!("repository {}: Git switch", directory.display()),
        git_argv(
            directory,
            &switch.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
    )];
    steps.extend(configure_workspace_steps(directory, &base)?);
    run_steps(executor, backend, session_id, steps)
}

fn git_argv(directory: &std::path::Path, arguments: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git".into(),
        "-C".into(),
        directory.to_string_lossy().into_owned(),
    ];
    argv.extend(arguments.iter().map(|argument| (*argument).to_owned()));
    argv
}

pub(super) fn configure_workspace(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    session_id: &str,
    directory: &std::path::Path,
    base: &str,
) -> Result<()> {
    run_steps(
        executor,
        backend,
        session_id,
        configure_workspace_steps(directory, base)?,
    )
}

/// The writes that mark a workspace complete, ready to follow other steps in
/// one command.
fn configure_workspace_steps(
    directory: &std::path::Path,
    base: &str,
) -> Result<Vec<targets::CommandStep>> {
    let mut steps = Vec::new();
    mj_core::remote_git::finalize_managed_checkout(base, true, |arguments| {
        steps.push(targets::CommandStep::new(
            "initialize network session Git checkout",
            git_argv(directory, arguments),
        ));
        Ok(())
    })?;
    steps.push(
        targets::CommandStep::new(
            "initialize network session Git checkout",
            git_argv(
                directory,
                &["config", "--local", "--unset-all", "remote.origin.push"],
            ),
        )
        .accepted_statuses([0, 5]),
    );
    Ok(steps)
}

fn run_steps(
    executor: &impl CommandExecutor,
    backend: &targets::TargetLocator,
    session_id: &str,
    steps: Vec<targets::CommandStep>,
) -> Result<()> {
    let command = targets::command_on_locator(
        SshTrafficClass::Bulk,
        backend,
        session_id,
        targets::step_script(steps),
        "initialize network session Git checkout",
    )?;
    targets::checked_command_output(&command, executor.execute(&command)?)?;
    Ok(())
}

/// The markers [`configure_workspace`] writes, without the `origin` settings:
/// a checkout without later history has no remote, and naming one would make
/// `git remote` list it.
fn configure_workspace_without_origin(
    checked: &impl Fn(&[&str]) -> Result<String>,
    launch_head: Option<&str>,
    base: &str,
) -> Result<()> {
    // Checkpoint restore writes archived origin configuration, but performs
    // no network fetch. Remove it before the worker can issue Git commands.
    for remote in checked(&["remote"])?.lines() {
        checked(&["remote", "remove", remote])?;
    }
    ensure!(
        checked(&["remote"])?.is_empty(),
        "history cutoff workspace still has a remote"
    );
    ensure!(
        checked(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/remotes",
            "refs/tags"
        ])?
        .is_empty(),
        "history cutoff workspace contains remote refs or tags"
    );
    ensure!(
        checked(&["rev-parse", "--is-shallow-repository"])? == "false",
        "history cutoff workspace is shallow"
    );
    // Launch proves the initial history is clean. Later steps rely on cutoff
    // provisioning and archive provenance to prove mj added no upstream history:
    // the agent's discarded commits and imported objects cannot be distinguished.
    if let Some(head) = launch_head {
        let allowed = checked(&["rev-list", "HEAD", head, base])?;
        let allowed = allowed.lines().collect::<std::collections::HashSet<_>>();
        let objects = checked(&[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])?;
        for object in objects.lines() {
            if let Some(commit) = object.strip_suffix(" commit") {
                ensure!(
                    allowed.contains(commit),
                    "history cutoff workspace contains commit {commit} outside the launch ancestry"
                );
            }
        }
    }
    for (key, value) in [("mj.baseCommit", base), ("mj.remoteWorkspace", "true")] {
        checked(&["config", "--local", "--replace-all", key, value])?;
    }
    Ok(())
}

fn prepare_exact_checkout(
    git: &impl Fn(&[&str]) -> Result<targets::CommandOutput>,
    checked: &impl Fn(&[&str]) -> Result<String>,
    session_id: &str,
    checkout: &mj_core::remote_git::ExactCheckout,
) -> Result<()> {
    checkout.validate()?;
    let current_head = checked(&["rev-parse", "HEAD"])?;
    let current_branch = checked(&["branch", "--show-current"])?;
    let marker = git(&["config", "--local", "--get", "mj.exactCheckout"])?;
    ensure!(
        matches!(marker.status, 0 | 1),
        "could not inspect exact checkout marker"
    );
    let retry = marker.status == 0;
    if retry {
        let (owner, selection, original_head, original_branch): (
            String,
            mj_core::remote_git::ExactCheckout,
            String,
            String,
        ) = serde_json::from_slice(&marker.stdout).context("invalid exact checkout marker")?;
        ensure!(
            owner == session_id && selection == *checkout,
            "workspace belongs to a different exact checkout selection"
        );
        ensure!(
            (current_head == original_head && current_branch == original_branch)
                || (current_head == checkout.commit
                    && current_branch == checkout.branch.as_deref().unwrap_or("")),
            "checkout moved after interrupted preparation; existing work was retained"
        );
    } else {
        let completed = git(&["config", "--local", "--get", "mj.remoteWorkspace"])?;
        ensure!(
            completed.status == 1,
            "workspace is already occupied or its marker cannot be read"
        );
    }
    ensure!(
        checked(&["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty(),
        "exact checkout requires a clean working tree; existing work was retained"
    );
    if let Some(branch) = &checkout.branch {
        checked(&["check-ref-format", "--branch", branch])?;
        let existing = git(&[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])?;
        ensure!(
            matches!(existing.status, 0 | 1),
            "could not inspect private branch"
        );
        if existing.status == 0 {
            ensure!(retry, "checkout branch {branch:?} is already occupied");
            ensure!(
                checked(&["rev-parse", "--verify", &format!("refs/heads/{branch}")])?
                    == checkout.commit,
                "private branch moved after preparation; existing work was retained"
            );
        }
    }
    let object = git(&["cat-file", "-t", &checkout.commit])?;
    // A checkout without later history fetched its commits when it was
    // created, and has no origin to fetch from now.
    ensure!(
        object.status == 0 || !checkout.no_later_history,
        "exact checkout commit {} is missing from the checkout without later history",
        checkout.commit
    );
    if object.status != 0 {
        checked(&["fetch", "--no-tags", "origin", &checkout.commit]).with_context(|| {
            format!(
                "exact checkout commit {} is unavailable from origin",
                checkout.commit
            )
        })?;
    }
    ensure!(
        checked(&["cat-file", "-t", &checkout.commit])? == "commit",
        "exact checkout object is not a commit"
    );
    // Write intent before switching. A retry may finish only this session's
    // selection; neither retry nor completion can overwrite subsequent work.
    let completed = git(&["config", "--local", "--get", "mj.remoteWorkspace"])?;
    ensure!(
        matches!(completed.status, 0 | 1),
        "could not inspect completion marker"
    );
    if completed.status == 0 {
        ensure!(
            String::from_utf8(completed.stdout)?.trim() == "true",
            "invalid network workspace completion marker"
        );
    }
    if completed.status == 1 {
        if !retry {
            let identity =
                serde_json::to_string(&(session_id, checkout, current_head, current_branch))?;
            checked(&[
                "config",
                "--local",
                "--replace-all",
                "mj.exactCheckout",
                &identity,
            ])?;
        }
        if let Some(branch) = &checkout.branch {
            let existing = git(&[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])?;
            match existing.status {
                0 => {
                    checked(&["switch", "--no-guess", branch])?;
                }
                1 => {
                    checked(&["switch", "--no-track", "-c", branch, &checkout.commit])?;
                }
                _ => anyhow::bail!("could not inspect private branch"),
            }
        } else {
            checked(&["switch", "--detach", &checkout.commit])?;
        }
    }
    ensure!(
        checked(&["rev-parse", "HEAD"])? == checkout.commit,
        "exact checkout HEAD moved; existing work was retained"
    );
    ensure!(
        checked(&["branch", "--show-current"])? == checkout.branch.as_deref().unwrap_or(""),
        "exact checkout branch changed; existing work was retained"
    );
    ensure!(
        checked(&["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty(),
        "exact checkout working tree changed during preparation"
    );
    Ok(())
}

/// Resume uses archived network provenance, never the current host checkout.
pub(super) fn manifest_has_history_cutoff_origin_delta(
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> bool {
    let mut saw_origin_delta = false;
    for repository in &manifest.repositories {
        if let Some(delta) = repository.metadata.origin_delta.as_deref() {
            if !delta.history_cutoff {
                return false;
            }
            saw_origin_delta = true;
        }
    }
    saw_origin_delta
}

pub(super) fn checkpoint_has_history_cutoff_origin_delta(
    session: &mj_core::state::SessionRecord,
) -> Result<bool> {
    let checkpoint = session
        .checkpoint
        .as_ref()
        .context("session has no checkpoint")?;
    let archive = mj_checkpoint::archive::verify_archive_streaming(&checkpoint.archive_path)?;
    ensure!(
        archive.archive_sha256 == checkpoint.sha256 && archive.manifest.session.id == session.id,
        "persisted checkpoint verification failed"
    );
    Ok(manifest_has_history_cutoff_origin_delta(&archive.manifest))
}

pub(super) fn checkpoint_bundle(
    session: &mj_core::state::SessionRecord,
    config: &mj_core::config::Config,
) -> Result<targets::ProjectBundleSpec> {
    let checkpoint = session
        .checkpoint
        .as_ref()
        .context("session has no checkpoint")?;
    let archive = mj_checkpoint::archive::verify_archive_streaming(&checkpoint.archive_path)?;
    ensure!(
        archive.archive_sha256 == checkpoint.sha256 && archive.manifest.session.id == session.id,
        "persisted checkpoint verification failed"
    );
    bundle_from_manifest_for_session(&archive.manifest, Some(session), Some(config))
}

#[cfg(test)]
pub(super) fn bundle_from_manifest(
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> Result<targets::ProjectBundleSpec> {
    bundle_from_manifest_for_session(manifest, None, None)
}

pub(super) fn bundle_from_manifest_for_session(
    manifest: &mj_checkpoint::archive::ArchiveManifest,
    session: Option<&mj_core::state::SessionRecord>,
    config: Option<&mj_core::config::Config>,
) -> Result<targets::ProjectBundleSpec> {
    ensure!(
        !manifest.repositories.is_empty(),
        "checkpoint has no network repository provenance; start a new network-backed session"
    );
    let session_primary_destination = session.and_then(|session| {
        let bundle = session
            .project
            .as_deref()
            .map(|project| &project.bundle)
            .or_else(|| config.and_then(|config| session.project_bundle(config)));
        bundle.and_then(|bundle| {
            bundle
                .repositories
                .iter()
                .find(|repository| repository.id == bundle.primary_repo)
                .map(|repository| repository.destination.to_string_lossy().into_owned())
        })
    });
    let mut primary_destination = None;
    let repositories = manifest
        .repositories
        .iter()
        .map(|repository| {
            let metadata = &repository.metadata;
            let origin_backed_delta = metadata
                .origin_delta
                .as_deref()
                .is_some_and(|delta| !delta.history_cutoff && !metadata.origin.is_empty());
            if !metadata.remote_workspace && !origin_backed_delta {
                if metadata.origin_delta.is_some() && metadata.origin.is_empty() {
                    anyhow::bail!(
                        "This raw session has no Git origin remote and can only be resumed on its original target."
                    );
                }
                anyhow::bail!(
                    "resuming legacy isolated or raw-local sessions into an isolated workspace is not supported; start a new network-backed session"
                );
            }
            mj_core::remote_git::validate_network_url(&metadata.origin)?;
            for url in &metadata.push_urls {
                mj_core::remote_git::validate_network_url(url)?;
            }
            let destination = if metadata.id == crate::controller::checkpoint::HOST_CHECKOUT_REPOSITORY
            {
                session_primary_destination.clone().unwrap_or_else(|| {
                    metadata.relative_destination.to_string_lossy().into_owned()
                })
            } else {
                metadata.relative_destination.to_string_lossy().into_owned()
            };
            if metadata.id == manifest.bundle.primary_repository {
                primary_destination = Some(destination.clone());
            }
            Ok(targets::RepositorySpec {
                url: Some(metadata.origin.clone()),
                push_urls: metadata.push_urls.clone(),
                destination,
                git_ref: None,
                reference: None,
                history_cutoff: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let primary = primary_destination.context("checkpoint primary repository is missing")?;
    Ok(targets::ProjectBundleSpec {
        primary,
        repositories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::{
        committed_repository, local_bundle, resume_compatibility_config, test_git,
    };
    use crate::targets::{CommandOutput, CommandSpec, ProcessExecutor};
    use std::path::{Path, PathBuf};

    /// Substitute transport only: production clone/init/push Git arguments
    /// operate on real repositories without depending on an external server.
    struct FixtureTransport {
        fetch: PathBuf,
        push: PathBuf,
    }

    impl CommandExecutor for FixtureTransport {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let mut command = command.clone();
            ensure!(
                matches!(command.program.as_str(), "git" | "sh"),
                "fixture executes Git and its step scripts"
            );
            // Environment configuration reaches Git inside the configuration
            // batch too; transport rewrites have one owner for both surfaces.
            command.env.insert("GIT_CONFIG_COUNT".into(), "2".into());
            for (index, (path, url)) in [
                (&self.fetch, "https://fetch.example.test/repo.git"),
                (&self.push, "https://push.example.test/repo.git"),
            ]
            .into_iter()
            .enumerate()
            {
                command.env.insert(
                    format!("GIT_CONFIG_KEY_{index}"),
                    format!("url.{}.insteadOf", path.display()),
                );
                command
                    .env
                    .insert(format!("GIT_CONFIG_VALUE_{index}"), url.into());
            }
            ProcessExecutor.execute(&command)
        }
    }

    fn checked(executor: &impl CommandExecutor, directory: &Path, arguments: &[&str]) -> String {
        let mut args = vec!["-C".to_owned(), directory.to_string_lossy().into_owned()];
        args.extend(arguments.iter().map(|arg| (*arg).to_owned()));
        let output = executor.execute(&CommandSpec::new("git", args)).unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn resume_clone_uses_archived_destinations_and_rejects_legacy_provenance() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "11111111-1111-4111-8111-111111111111";
        let mut session = super::super::test_support::checkpoint_test_session(session_id);
        session.checkpoint = Some(Box::new(
            super::super::test_support::write_network_checkpoint_archive(
                root.path(),
                session_id,
                0,
            ),
        ));
        let bundle = checkpoint_bundle(&session, &mj_core::config::Config::default()).unwrap();
        assert_eq!(bundle.primary, "project");
        assert_eq!(
            bundle.repositories[0].url.as_deref(),
            Some("https://fetch.example.test/project.git")
        );
        assert_eq!(
            bundle.repositories[0].push_urls,
            ["https://push.example.test/project.git"]
        );
        let mut archive = mj_checkpoint::archive::verify_archive_streaming(
            &session.checkpoint.as_ref().unwrap().archive_path,
        )
        .unwrap();
        archive.manifest.repositories[0].metadata.remote_workspace = false;
        archive.manifest.repositories[0].metadata.origin = "mj-local:project".into();
        assert!(
            bundle_from_manifest(&archive.manifest)
                .unwrap_err()
                .to_string()
                .contains("legacy")
        );
        assert!(session.checkpoint.as_ref().unwrap().archive_path.is_file());
    }

    struct ExactFixture {
        _host: tempfile::TempDir,
        _root: tempfile::TempDir,
        directory: PathBuf,
        backend: targets::TargetLocator,
        selection: mj_core::remote_git::ExactCheckout,
        later: String,
    }

    const EXACT_SESSION: &str = "11111111-1111-4111-8111-111111111111";

    impl ExactFixture {
        fn new() -> Self {
            let host = committed_repository();
            let commit = test_git(host.path(), &["rev-parse", "HEAD"]);
            std::fs::write(host.path().join("nested/file.txt"), "later\n").unwrap();
            test_git(host.path(), &["commit", "-am", "later"]);
            let later = test_git(host.path(), &["rev-parse", "HEAD"]);
            let root = tempfile::tempdir().unwrap();
            let directory = root.path().join(EXACT_SESSION);
            test_git(
                root.path(),
                &[
                    "clone",
                    host.path().to_str().unwrap(),
                    directory.to_str().unwrap(),
                ],
            );
            Self {
                _host: host,
                _root: root,
                backend: targets::TargetLocator::LocalBare {
                    worker_root: directory.to_string_lossy().into_owned(),
                },
                directory,
                selection: mj_core::remote_git::ExactCheckout {
                    repository_id: "project".into(),
                    commit,
                    branch: Some("town/run-123".into()),
                    no_later_history: false,
                },
                later,
            }
        }
        fn prepare(&self, executor: &impl CommandExecutor) -> Result<()> {
            initialize_workspace(
                executor,
                &self.backend,
                EXACT_SESSION,
                &self.directory,
                None,
                None,
                Some(&self.selection),
            )
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn exact_checkout_starts_at_a_on_a_new_private_branch_while_origin_is_at_b() {
        let fixture = ExactFixture::new();
        let source_refs = test_git(fixture._host.path(), &["show-ref"]);
        fixture.prepare(&ProcessExecutor).unwrap();
        assert_eq!(
            test_git(&fixture.directory, &["rev-parse", "HEAD"]),
            fixture.selection.commit
        );
        assert_eq!(
            test_git(&fixture.directory, &["branch", "--show-current"]),
            "town/run-123"
        );
        assert_eq!(
            test_git(&fixture.directory, &["rev-parse", "origin/master"]),
            fixture.later
        );
        assert_eq!(
            test_git(&fixture.directory, &["config", "mj.baseCommit"]),
            fixture.selection.commit
        );
        assert!(test_git(&fixture.directory, &["status", "--porcelain"]).is_empty());
        assert_eq!(test_git(fixture._host.path(), &["show-ref"]), source_refs);
        fixture.prepare(&ProcessExecutor).unwrap();
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn exact_checkout_fetches_a_new_exact_object_and_can_leave_head_detached() {
        let mut fixture = ExactFixture::new();
        test_git(
            fixture._host.path(),
            &["commit", "--allow-empty", "-m", "after clone"],
        );
        fixture.selection.commit = test_git(fixture._host.path(), &["rev-parse", "HEAD"]);
        fixture.selection.branch = None;
        fixture.prepare(&ProcessExecutor).unwrap();
        assert_eq!(
            test_git(&fixture.directory, &["rev-parse", "HEAD"]),
            fixture.selection.commit
        );
        assert!(test_git(&fixture.directory, &["branch", "--show-current"]).is_empty());
        assert_eq!(
            test_git(&fixture.directory, &["rev-parse", "origin/master"]),
            fixture.later
        );
    }

    #[test]
    fn exact_checkout_refuses_unavailable_invalid_and_occupied_selections_without_moving_head() {
        for case in ["unknown", "invalid_branch", "occupied", "dirty", "tag"] {
            let mut fixture = ExactFixture::new();
            match case {
                "unknown" => fixture.selection.commit = "f".repeat(40),
                "invalid_branch" => fixture.selection.branch = Some("bad..branch".into()),
                "occupied" => {
                    test_git(
                        &fixture.directory,
                        &["branch", "town/run-123", &fixture.selection.commit],
                    );
                }
                "dirty" => {
                    std::fs::write(fixture.directory.join("untracked"), "keep me").unwrap();
                }
                "tag" => {
                    test_git(
                        &fixture.directory,
                        &[
                            "-c",
                            "user.name=Test",
                            "-c",
                            "user.email=test@example.test",
                            "tag",
                            "-a",
                            "test-tag",
                            "-m",
                            "tag",
                        ],
                    );
                    fixture.selection.commit =
                        test_git(&fixture.directory, &["rev-parse", "test-tag"]);
                }
                _ => unreachable!(),
            }
            assert!(fixture.prepare(&ProcessExecutor).is_err(), "{case}");
            assert_eq!(
                test_git(&fixture.directory, &["rev-parse", "HEAD"]),
                fixture.later,
                "{case}"
            );
        }
    }

    struct InterruptAfterSwitch;
    impl CommandExecutor for InterruptAfterSwitch {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let output = ProcessExecutor.execute(command)?;
            if command.args.iter().any(|arg| arg == "switch") {
                anyhow::bail!("preparation interrupted after switch");
            }
            Ok(output)
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1296: process cancellation and relay recovery assume Unix"
    )]
    fn exact_checkout_retries_interruption_but_refuses_subsequent_branch_movement_or_dirty_work() {
        for change in ["none", "commit", "dirty", "selection"] {
            let mut fixture = ExactFixture::new();
            assert!(
                fixture
                    .prepare(&InterruptAfterSwitch)
                    .unwrap_err()
                    .to_string()
                    .contains("interrupted")
            );
            match change {
                "commit" => {
                    test_git(
                        &fixture.directory,
                        &[
                            "-c",
                            "user.name=Test",
                            "-c",
                            "user.email=test@example.test",
                            "commit",
                            "--allow-empty",
                            "-m",
                            "new work",
                        ],
                    );
                }
                "dirty" => {
                    std::fs::write(fixture.directory.join("work"), "retain").unwrap();
                }
                "selection" => fixture.selection.commit = fixture.later.clone(),
                _ => {}
            }
            let head = test_git(&fixture.directory, &["rev-parse", "HEAD"]);
            assert_eq!(
                fixture.prepare(&ProcessExecutor).is_ok(),
                change == "none",
                "{change}"
            );
            assert_eq!(test_git(&fixture.directory, &["rev-parse", "HEAD"]), head);
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn completed_exact_checkout_does_not_reset_a_moved_private_branch() {
        let fixture = ExactFixture::new();
        fixture.prepare(&ProcessExecutor).unwrap();
        test_git(
            &fixture.directory,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--allow-empty",
                "-m",
                "new work",
            ],
        );
        let head = test_git(&fixture.directory, &["rev-parse", "HEAD"]);
        assert!(fixture.prepare(&ProcessExecutor).is_err());
        assert_eq!(test_git(&fixture.directory, &["rev-parse", "HEAD"]), head);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn exact_checkout_and_its_base_apply_only_to_the_checked_out_repository() {
        let fixture = ExactFixture::new();
        let other = committed_repository();
        let other_commit = test_git(other.path(), &["rev-parse", "HEAD"]);
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join(EXACT_SESSION);
        std::fs::create_dir(&workspace).unwrap();
        for (source, name) in [(fixture._host.path(), "project"), (other.path(), "other")] {
            test_git(&workspace, &["clone", source.to_str().unwrap(), name]);
        }
        let mut bundle = local_bundle(fixture._host.path());
        let mut secondary = bundle.repositories[0].clone();
        secondary.id = "other".into();
        secondary.destination = "other".into();
        secondary.local = Some(other.path().to_path_buf());
        bundle.repositories.push(secondary);
        let mut config = resume_compatibility_config();
        config.bundles.insert("project".into(), bundle);
        let mut session = super::super::test_support::checkpoint_test_session(EXACT_SESSION);
        session.checkout = Some(Box::new(fixture.selection.clone()));
        // A base the other repository does not hold: applying it there would fail.
        session.launch_base = Some(fixture.later.clone());
        let mut state = mj_core::state::State::default();
        state
            .environment_owners
            .insert(EXACT_SESSION.into(), EXACT_SESSION.into());
        state.sessions.insert(EXACT_SESSION.into(), session);
        let controller = Controller { config, state };
        controller
            .initialize_network_workspaces(
                EXACT_SESSION,
                &targets::TargetLocator::LocalBare {
                    worker_root: workspace.to_string_lossy().into_owned(),
                },
                &ProcessExecutor,
            )
            .unwrap();
        assert_eq!(
            test_git(&workspace.join("project"), &["rev-parse", "HEAD"]),
            fixture.selection.commit
        );
        assert_eq!(
            test_git(&workspace.join("other"), &["rev-parse", "HEAD"]),
            other_commit
        );
        assert_eq!(
            test_git(&workspace.join("other"), &["branch", "--show-current"]),
            "master"
        );
        assert_eq!(
            test_git(&workspace.join("project"), &["config", "mj.baseCommit"]),
            fixture.later
        );
        assert_eq!(
            test_git(&workspace.join("other"), &["config", "mj.baseCommit"]),
            other_commit
        );
    }

    /// Delegates to real processes while counting commands and, when a latch
    /// is set, holding each command until two have started together.
    struct CountingExecutor {
        commands: std::sync::Mutex<Vec<Vec<String>>>,
        latch: Option<(std::sync::Mutex<usize>, std::sync::Condvar)>,
        overlapped: std::sync::atomic::AtomicBool,
    }

    impl CountingExecutor {
        fn new(latch: bool) -> Self {
            Self {
                commands: Default::default(),
                latch: latch.then(Default::default),
                overlapped: Default::default(),
            }
        }

        fn count(&self) -> usize {
            self.commands.lock().unwrap().len()
        }
    }

    impl CommandExecutor for CountingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.commands.lock().unwrap().push(
                std::iter::once(command.program.clone())
                    .chain(command.args.iter().cloned())
                    .collect(),
            );
            if let Some((started, changed)) = &self.latch {
                let mut started = started.lock().unwrap();
                *started += 1;
                changed.notify_all();
                // Two commands that are ever in flight together prove overlap;
                // a serial caller times out here and is reported by the test.
                let (started, wait) = changed
                    .wait_timeout_while(started, std::time::Duration::from_secs(20), |started| {
                        *started < 2
                    })
                    .unwrap();
                if !wait.timed_out() && *started >= 2 {
                    self.overlapped
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            ProcessExecutor.execute(command)
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn a_fresh_workspace_takes_two_commands_and_a_retry_one() {
        let host = committed_repository();
        let root = tempfile::tempdir().unwrap();
        let session_id = "11111111-1111-4111-8111-111111111111";
        let destination = root.path().join(session_id);
        test_git(
            root.path(),
            &[
                "clone",
                host.path().to_str().unwrap(),
                destination.to_str().unwrap(),
            ],
        );
        let backend = targets::TargetLocator::LocalBare {
            worker_root: destination.to_string_lossy().into_owned(),
        };
        let executor = CountingExecutor::new(false);
        let initialize = |base: Option<&str>, branch: Option<&str>| {
            initialize_workspace(
                &executor,
                &backend,
                session_id,
                &destination,
                base,
                branch,
                None,
            )
        };
        initialize(None, None).unwrap();
        // One probe command and one write command.
        assert_eq!(executor.count(), 2);
        assert_eq!(
            test_git(&destination, &["config", "mj.remoteWorkspace"]),
            "true"
        );
        // A completed marker ends the probe script before it asks anything else.
        initialize(None, None).unwrap();
        assert_eq!(executor.count(), 3);

        // Each refusal keeps its diagnostic. Values reach the script as
        // arguments, so shell syntax in them is only text.
        let refused = |mark: &str, base: Option<&str>, branch: Option<&str>, expected: &str| {
            let clone = root
                .path()
                .join(format!("{mark}-22222222-2222-4222-8222-222222222222"));
            let id = clone.file_name().unwrap().to_string_lossy().into_owned();
            test_git(
                root.path(),
                &[
                    "clone",
                    host.path().to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
            (
                clone,
                id,
                base.map(str::to_owned),
                branch.map(str::to_owned),
                expected.to_owned(),
            )
        };
        let injected = root.path().join("injected");
        let hostile = format!("x'; touch '{}'; '", injected.display());
        let cases = [
            refused("base", Some(&hostile), None, "is not in the clone"),
            refused(
                "branch",
                None,
                Some("bad..branch"),
                "check-ref-format failed",
            ),
            refused("empty", None, Some(""), "check-ref-format failed"),
            refused("unset-head", None, None, "no usable default branch"),
            refused("marker", None, None, "invalid network workspace marker"),
        ];
        for (clone, id, base, branch, expected) in cases {
            if expected.contains("no usable default branch") {
                test_git(&clone, &["remote", "set-head", "origin", "-d"]);
            }
            if expected.contains("workspace marker") {
                test_git(&clone, &["config", "mj.remoteWorkspace", "nope"]);
            }
            let backend = targets::TargetLocator::LocalBare {
                worker_root: clone.to_string_lossy().into_owned(),
            };
            let before = test_git(&clone, &["rev-parse", "HEAD"]);
            let error = initialize_workspace(
                &ProcessExecutor,
                &backend,
                &id,
                &clone,
                base.as_deref(),
                branch.as_deref(),
                None,
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains(&expected),
                "{expected}: {error:#}"
            );
            assert_eq!(test_git(&clone, &["rev-parse", "HEAD"]), before);
        }
        assert!(!injected.exists(), "a base reached the shell as code");

        // A directory that is not a repository cannot answer the marker probe.
        let missing = root.path().join("33333333-3333-4333-8333-333333333333");
        let error = initialize_workspace(
            &ProcessExecutor,
            &targets::TargetLocator::LocalBare {
                worker_root: missing.to_string_lossy().into_owned(),
            },
            "33333333-3333-4333-8333-333333333333",
            &missing,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not inspect network workspace marker"),
            "{error:#}"
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn the_repositories_of_a_bundle_initialize_concurrently() {
        let first = committed_repository();
        let second = committed_repository();
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join(EXACT_SESSION);
        std::fs::create_dir(&workspace).unwrap();
        for (source, name) in [(first.path(), "project"), (second.path(), "other")] {
            test_git(&workspace, &["clone", source.to_str().unwrap(), name]);
        }
        let mut bundle = local_bundle(first.path());
        let mut secondary = bundle.repositories[0].clone();
        secondary.id = "other".into();
        secondary.destination = "other".into();
        secondary.local = Some(second.path().to_path_buf());
        bundle.repositories.push(secondary);
        let mut config = resume_compatibility_config();
        config.bundles.insert("project".into(), bundle);
        let mut session = super::super::test_support::checkpoint_test_session(EXACT_SESSION);
        session.launch_base = None;
        let mut state = mj_core::state::State::default();
        state
            .environment_owners
            .insert(EXACT_SESSION.into(), EXACT_SESSION.into());
        state.sessions.insert(EXACT_SESSION.into(), session);
        let controller = Controller { config, state };
        let executor = CountingExecutor::new(true);
        controller
            .initialize_network_workspaces(
                EXACT_SESSION,
                &targets::TargetLocator::LocalBare {
                    worker_root: workspace.to_string_lossy().into_owned(),
                },
                &executor,
            )
            .unwrap();
        assert!(
            executor
                .overlapped
                .load(std::sync::atomic::Ordering::SeqCst),
            "repositories were initialized one after another"
        );
        for name in ["project", "other"] {
            assert_eq!(
                test_git(&workspace.join(name), &["config", "mj.remoteWorkspace"]),
                "true"
            );
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn a_launch_base_sets_diff_base_without_moving_selected_branch() {
        let host = committed_repository();
        let initial = test_git(host.path(), &["rev-parse", "HEAD"]);
        std::fs::write(host.path().join("nested/file.txt"), "later\n").unwrap();
        test_git(host.path(), &["commit", "-am", "later"]);
        let later = test_git(host.path(), &["rev-parse", "HEAD"]);
        let root = tempfile::tempdir().unwrap();

        // A local bare worker root must end with the session id it serves.
        let clone_into = |session_id: &str| -> PathBuf {
            let destination = root.path().join(session_id);
            test_git(
                root.path(),
                &[
                    "clone",
                    host.path().to_str().unwrap(),
                    destination.to_str().unwrap(),
                ],
            );
            destination
        };

        // A commit SHA becomes the diff base without moving the branch.
        let pinned = clone_into("11111111-1111-4111-8111-111111111111");
        initialize_workspace(
            &ProcessExecutor,
            &targets::TargetLocator::LocalBare {
                worker_root: pinned.to_string_lossy().into_owned(),
            },
            "11111111-1111-4111-8111-111111111111",
            &pinned,
            Some(&initial),
            None,
            None,
        )
        .unwrap();
        assert_eq!(test_git(&pinned, &["rev-parse", "HEAD"]), later);
        assert_eq!(test_git(&pinned, &["config", "mj.baseCommit"]), initial);
        assert_eq!(test_git(&pinned, &["branch", "--show-current"]), "master");

        // A remote-tracking branch names the same thing the clone knows about.
        let tracked = clone_into("22222222-2222-4222-8222-222222222222");
        initialize_workspace(
            &ProcessExecutor,
            &targets::TargetLocator::LocalBare {
                worker_root: tracked.to_string_lossy().into_owned(),
            },
            "22222222-2222-4222-8222-222222222222",
            &tracked,
            Some("origin/master"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(test_git(&tracked, &["rev-parse", "HEAD"]), later);
        assert_eq!(test_git(&tracked, &["config", "mj.baseCommit"]), later);

        // A branch that only the host checkout holds was never fetched, so the
        // refusal says where to look instead of repeating Git's wording.
        test_git(host.path(), &["branch", "host-only"]);
        let missing = clone_into("33333333-3333-4333-8333-333333333333");
        let error = initialize_workspace(
            &ProcessExecutor,
            &targets::TargetLocator::LocalBare {
                worker_root: missing.to_string_lossy().into_owned(),
            },
            "33333333-3333-4333-8333-333333333333",
            &missing,
            Some("host-only"),
            None,
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("is not in the clone"),
            "unexpected error: {error:#}"
        );
        // The refusal happens before the checkout is switched or marked.
        assert_eq!(test_git(&missing, &["branch", "--show-current"]), "master");
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn isolated_sessions_start_at_remote_default_and_publish_without_changing_the_host() {
        let host = committed_repository();
        let root = tempfile::tempdir().unwrap();
        let fetch = root.path().join("fetch.git");
        let push = root.path().join("push.git");
        test_git(host.path(), &["branch", "-m", "trunk"]);
        let initial = test_git(host.path(), &["rev-parse", "HEAD"]);
        test_git(
            root.path(),
            &[
                "clone",
                "--bare",
                host.path().to_str().unwrap(),
                fetch.to_str().unwrap(),
            ],
        );
        test_git(
            root.path(),
            &[
                "clone",
                "--bare",
                fetch.to_str().unwrap(),
                push.to_str().unwrap(),
            ],
        );
        test_git(
            host.path(),
            &[
                "remote",
                "add",
                "upstream",
                "https://fetch.example.test/repo.git",
            ],
        );
        test_git(
            host.path(),
            &[
                "remote",
                "add",
                "fork",
                "https://push.example.test/repo.git",
            ],
        );
        test_git(host.path(), &["switch", "-c", "private"]);
        test_git(
            host.path(),
            &["config", "branch.private.remote", "upstream"],
        );
        test_git(host.path(), &["config", "remote.pushDefault", "fork"]);
        std::fs::write(host.path().join("unpublished"), "host only").unwrap();
        test_git(host.path(), &["add", "unpublished"]);
        test_git(host.path(), &["commit", "-m", "unpublished host work"]);
        std::fs::write(host.path().join("dirty"), vec![b'x'; 128 * 1024]).unwrap();
        std::fs::write(host.path().join("staged"), "staged host work").unwrap();
        test_git(host.path(), &["add", "staged"]);
        std::fs::write(host.path().join("nested/file.txt"), "unstaged host work").unwrap();
        let host_status = test_git(host.path(), &["status", "--porcelain"]);
        let host_head = test_git(host.path(), &["rev-parse", "HEAD"]);
        let host_refs = test_git(host.path(), &["show-ref"]);
        let source = local_bundle(host.path());
        let bundle = super::super::backend::backend_bundle(&source, &ProcessExecutor).unwrap();
        assert_eq!(
            bundle.repositories[0].url.as_deref(),
            Some("https://fetch.example.test/repo.git")
        );
        assert_eq!(
            bundle.repositories[0].push_urls,
            ["https://push.example.test/repo.git"]
        );
        let template = super::super::backend::backend_target(
            &resume_compatibility_config().targets["podman"],
            None,
            super::super::backend::ContainerOverrides::default(),
        )
        .unwrap();
        let executor = FixtureTransport { fetch, push };
        let mut first_pushed_head = None;
        for session_id in [
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ] {
            let plan = targets::provision_plan(&template, session_id, &bundle, &[], None).unwrap();
            let clone = plan
                .commands
                .iter()
                .find(|command| command.purpose == "clone project")
                .unwrap();
            let git_index = clone
                .args
                .iter()
                .position(|argument| argument == "git")
                .unwrap();
            let mut args = clone.args[git_index + 1..].to_vec();
            let destination = root.path().join(session_id);
            *args.last_mut().unwrap() = destination.to_string_lossy().into_owned();
            let output = executor.execute(&CommandSpec::new("git", args)).unwrap();
            assert_eq!(
                output.status,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let backend = targets::TargetLocator::LocalBare {
                worker_root: destination.to_string_lossy().into_owned(),
            };
            initialize_workspace(
                &executor,
                &backend,
                session_id,
                &destination,
                None,
                None,
                None,
            )
            .unwrap();
            assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), initial);
            assert_eq!(
                test_git(&destination, &["branch", "--show-current"]),
                "trunk"
            );
            assert!(!destination.join("unpublished").exists());
            assert!(!destination.join("dirty").exists());
            assert!(!destination.join("staged").exists());
            assert_eq!(
                std::fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
                "base\n"
            );
            assert!(test_git(&destination, &["status", "--porcelain"]).is_empty());
            test_git(&destination, &["config", "user.name", "Session Test"]);
            test_git(
                &destination,
                &["config", "user.email", "session@example.test"],
            );
            std::fs::write(destination.join("session-work"), session_id.repeat(4096)).unwrap();
            test_git(&destination, &["add", "session-work"]);
            test_git(&destination, &["commit", "-m", "session work"]);
            if first_pushed_head.is_none() {
                checked(&executor, &destination, &["push"]);
            } else {
                let output = executor
                    .execute(&CommandSpec::new(
                        "git",
                        [
                            "-C".to_owned(),
                            destination.to_string_lossy().into_owned(),
                            "push".to_owned(),
                        ],
                    ))
                    .unwrap();
                assert_ne!(
                    output.status, 0,
                    "a concurrent clone must see a normal non-fast-forward push conflict"
                );
            }
            let head = test_git(&destination, &["rev-parse", "HEAD"]);
            if first_pushed_head.is_none() {
                assert_eq!(test_git(&executor.push, &["rev-parse", "trunk"]), head);
                first_pushed_head = Some(head.clone());
            }
            test_git(&destination, &["switch", "-c", "user-selected"]);
            initialize_workspace(
                &executor,
                &backend,
                session_id,
                &destination,
                None,
                None,
                None,
            )
            .unwrap();
            assert_eq!(
                test_git(&destination, &["branch", "--show-current"]),
                "user-selected"
            );
            assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), head);
        }
        assert_eq!(test_git(host.path(), &["rev-parse", "HEAD"]), host_head);
        assert_eq!(test_git(host.path(), &["show-ref"]), host_refs);
        assert_eq!(
            test_git(host.path(), &["status", "--porcelain"]),
            host_status
        );
        assert_eq!(
            std::fs::read_to_string(host.path().join("nested/file.txt")).unwrap(),
            "unstaged host work"
        );
        assert_eq!(
            std::fs::read(host.path().join("dirty")).unwrap(),
            vec![b'x'; 128 * 1024]
        );
        assert_eq!(test_git(&executor.fetch, &["rev-parse", "trunk"]), initial);
        assert_eq!(
            test_git(&executor.push, &["rev-parse", "trunk"]),
            first_pushed_head.unwrap()
        );
    }

    /// A host repository whose history runs past the cutoff in every way a
    /// reviewer could look: a later commit on the head's branch, a later
    /// commit on the base's branch, an annotated tag, and another pull
    /// request's ref.
    struct CutoffFixture {
        host: tempfile::TempDir,
        _root: tempfile::TempDir,
        workspace: PathBuf,
        directory: PathBuf,
        head: String,
        base: String,
        later: Vec<String>,
    }

    impl CutoffFixture {
        fn new() -> Self {
            let host = committed_repository();
            let commit = |message: &str| {
                std::fs::write(host.path().join("nested/file.txt"), format!("{message}\n"))
                    .unwrap();
                test_git(host.path(), &["commit", "-qam", message]);
                test_git(host.path(), &["rev-parse", "HEAD"])
            };
            test_git(host.path(), &["switch", "-qc", "trunk"]);
            let base = commit("base branch at review time");
            test_git(host.path(), &["switch", "-q", "master"]);
            let head = commit("reviewed head");
            let later_head = commit("fix found later");
            test_git(
                host.path(),
                &["tag", "-a", "v-later", "-m", "later release"],
            );
            let later_tag = test_git(host.path(), &["rev-parse", "v-later"]);
            test_git(host.path(), &["switch", "-q", "trunk"]);
            let later_base = commit("trunk moved on");
            test_git(host.path(), &["switch", "-qc", "other-pr", &head]);
            let other_pr = commit("another pull request");
            test_git(host.path(), &["update-ref", "refs/pull/2/head", &other_pr]);
            test_git(host.path(), &["switch", "-q", "master"]);
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join(EXACT_SESSION);
            Self {
                directory: workspace.join("project"),
                workspace,
                _root: root,
                host,
                head,
                base,
                later: vec![later_head, later_tag, later_base, other_pr],
            }
        }

        fn selection(&self, branch: Option<&str>) -> mj_core::remote_git::ExactCheckout {
            mj_core::remote_git::ExactCheckout {
                repository_id: "project".into(),
                commit: self.head.clone(),
                branch: branch.map(str::to_owned),
                no_later_history: true,
            }
        }

        fn locator(&self) -> targets::TargetLocator {
            targets::TargetLocator::LocalBare {
                worker_root: self.workspace.to_string_lossy().into_owned(),
            }
        }

        /// Run the production clone step against the host repository, which
        /// the fixture transport serves at the network URL.
        fn clone(&self, base: Option<&str>, reference: Option<&Path>) {
            let bundle = targets::ProjectBundleSpec {
                primary: "project".into(),
                repositories: vec![targets::RepositorySpec {
                    url: Some("https://fetch.example.test/repo.git".into()),
                    push_urls: Vec::new(),
                    destination: "project".into(),
                    git_ref: None,
                    reference: reference.map(|path| path.to_string_lossy().into_owned()),
                    history_cutoff: Some(targets::HistoryCutoff {
                        head: self.head.clone(),
                        base: base.map(str::to_owned),
                    }),
                }],
            };
            bundle.validate().unwrap();
            let transport = FixtureTransport {
                fetch: self.host.path().to_path_buf(),
                push: self.host.path().to_path_buf(),
            };
            for command in targets::provision_local_bundle_plan(
                &bundle,
                &self.workspace.to_string_lossy(),
                None,
            )
            .unwrap()
            .commands
            {
                let output = if command.program == "git" {
                    transport.execute(&command)
                } else {
                    ProcessExecutor.execute(&command)
                }
                .unwrap();
                assert_eq!(
                    output.status,
                    0,
                    "{}: {}",
                    command.purpose,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        fn initialize(&self, base: Option<&str>, branch: Option<&str>) -> Result<()> {
            initialize_workspace(
                &ProcessExecutor,
                &self.locator(),
                EXACT_SESSION,
                &self.directory,
                base,
                None,
                Some(&self.selection(branch)),
            )
        }
    }

    fn has_object(directory: &Path, object: &str) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["cat-file", "-e", object])
            .status()
            .unwrap()
            .success()
    }

    /// Every object the repository stores, and every object reachable from
    /// `commits`, each sorted.
    fn inventory(directory: &Path, commits: &[&str]) -> (Vec<String>, Vec<String>) {
        let mut present = test_git(
            directory,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        present.sort();
        let mut arguments = vec!["rev-list", "--objects"];
        arguments.extend(commits);
        let mut reachable = test_git(directory, &arguments)
            .lines()
            .map(|line| line.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>();
        reachable.sort();
        reachable.dedup();
        (present, reachable)
    }

    #[test]
    #[cfg_attr(windows, ignore = "local bare Git fixture")]
    fn cutoff_workspace_survives_profile_move_and_checkpoint_resume() {
        use mj_checkpoint::archive::{
            GitCollectionSpec, GitHistoryMode, SystemGit, collect_git_snapshot,
            restore_git_snapshot,
        };
        let fixture = CutoffFixture::new();
        fixture.clone(Some(&fixture.base), None);
        fixture
            .initialize(Some(&fixture.base), Some("town/run-123"))
            .unwrap();
        test_git(&fixture.directory, &["config", "user.name", "Session"]);
        test_git(
            &fixture.directory,
            &["config", "user.email", "session@example.invalid"],
        );
        std::fs::write(fixture.directory.join("session.txt"), "session work").unwrap();
        test_git(&fixture.directory, &["add", "session.txt"]);
        test_git(&fixture.directory, &["commit", "-qm", "session work"]);
        let session_head = test_git(&fixture.directory, &["rev-parse", "HEAD"]);
        std::fs::write(fixture.directory.join("session.txt"), "dirty session work").unwrap();
        let mut config = resume_compatibility_config();
        let mut bundle = local_bundle(fixture.host.path());
        bundle.repositories[0].destination = "project".into();
        config.bundles.insert("project".into(), bundle.clone());
        let mut session = super::super::test_support::checkpoint_test_session(EXACT_SESSION);
        session.bundle_id = "project".into();
        session.checkout = Some(Box::new(fixture.selection(Some("town/run-123"))));
        session.launch_base = Some(fixture.base.clone());
        session.container_workspace = None;
        let mut controller = Controller {
            config,
            state: Default::default(),
        };
        controller.config.targets.insert(
            "cutoff-local-bare".into(),
            mj_core::config::TargetTemplate::LocalBare,
        );
        assert_eq!(
            mj_client::target::resume_compatibility(
                &session,
                &controller.config,
                "cutoff-local-bare"
            )
            .unwrap(),
            mj_client::target::ResumePlan::InPlace
        );
        controller
            .state
            .sessions
            .insert(EXACT_SESSION.into(), session.clone());
        // The retained profile-only Move checks the existing workspace, without
        // positioning HEAD or requiring the agent's working tree to be clean.
        controller
            .verify_history_cutoff_workspace(EXACT_SESSION, &fixture.locator(), &ProcessExecutor)
            .unwrap();
        let mut snapshot = collect_git_snapshot(
            &SystemGit,
            &fixture.directory,
            &GitCollectionSpec {
                id: "project".into(),
                relative_destination: "project".into(),
                history: GitHistoryMode::CloneFrom(fixture.head.clone()),
                origin_override: Some("https://fetch.example.test/repo.git".into()),
            },
        )
        .unwrap();
        snapshot.metadata.remote_workspace = true;
        snapshot.metadata.base_commit = fixture.base.clone();
        // Reconstructed Resume and cross-target Move prepare their archived
        // bundle with the same policy before importing the session delta.
        let mut spec = targets::ProjectBundleSpec {
            primary: "project".into(),
            repositories: vec![targets::RepositorySpec {
                url: Some("https://fetch.example.test/repo.git".into()),
                push_urls: vec![],
                destination: "project".into(),
                git_ref: None,
                reference: None,
                history_cutoff: None,
            }],
        };
        super::super::backend::apply_session_history_cutoff(
            &session,
            &controller.config,
            &mut spec,
        )
        .unwrap();
        assert_eq!(
            spec.repositories[0].history_cutoff.as_ref().unwrap().head,
            fixture.head
        );
        std::fs::remove_dir_all(&fixture.directory).unwrap();
        fixture.clone(Some(&fixture.base), None);
        restore_git_snapshot(&SystemGit, &fixture.directory, &snapshot).unwrap();
        assert!(!test_git(&fixture.directory, &["remote"]).is_empty());
        controller
            .verify_history_cutoff_workspace(EXACT_SESSION, &fixture.locator(), &ProcessExecutor)
            .unwrap();
        assert_eq!(
            test_git(&fixture.directory, &["rev-parse", "HEAD"]),
            session_head
        );
        assert_eq!(
            std::fs::read_to_string(fixture.directory.join("session.txt")).unwrap(),
            "dirty session work"
        );
        assert_eq!(test_git(&fixture.directory, &["remote"]), "");
        assert_eq!(
            test_git(
                &fixture.directory,
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/remotes",
                    "refs/tags"
                ]
            ),
            ""
        );
        for later in &fixture.later {
            assert!(!has_object(&fixture.directory, later));
        }
        let (present, reachable) = inventory(&fixture.directory, &[&session_head, &fixture.base]);
        assert_eq!(present, reachable);
        test_git(&fixture.directory, &["tag", "unexpected"]);
        let error = controller
            .verify_history_cutoff_workspace(EXACT_SESSION, &fixture.locator(), &ProcessExecutor)
            .unwrap_err();
        assert!(
            error.to_string().contains("remote refs or tags"),
            "{error:#}"
        );
        // Launch still rejects upstream objects outside the selected ancestry.
        let contaminated = CutoffFixture::new();
        contaminated.clone(Some(&contaminated.base), None);
        test_git(
            &contaminated.directory,
            &[
                "fetch",
                "--no-tags",
                contaminated.host.path().to_str().unwrap(),
                &contaminated.later[0],
            ],
        );
        let error = contaminated
            .initialize(Some(&contaminated.base), Some("town/run-123"))
            .unwrap_err();
        assert!(
            error.to_string().contains("outside the launch ancestry"),
            "{error:#}"
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn a_checkout_without_later_history_holds_exactly_the_head_and_base_ancestry() {
        let fixture = CutoffFixture::new();
        let host_refs = test_git(fixture.host.path(), &["show-ref"]);
        fixture.clone(Some(&fixture.base), None);
        fixture
            .initialize(Some(&fixture.base), Some("town/run-123"))
            .unwrap();
        let directory = &fixture.directory;

        assert_eq!(test_git(directory, &["rev-parse", "HEAD"]), fixture.head);
        assert_eq!(
            test_git(directory, &["branch", "--show-current"]),
            "town/run-123"
        );
        assert_eq!(
            test_git(directory, &["config", "mj.baseCommit"]),
            fixture.base
        );
        assert_eq!(
            test_git(directory, &["config", "mj.remoteWorkspace"]),
            "true"
        );
        assert_eq!(test_git(directory, &["remote"]), "");
        assert_eq!(
            test_git(
                directory,
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            format!(
                "refs/heads/town/run-123 {}\nrefs/mj/base {}",
                fixture.head, fixture.base
            )
        );
        assert!(!directory.join(".git/objects/info/alternates").exists());
        // The real object IDs, so a reviewer can name them in findings.
        let (present, reachable) = inventory(directory, &[&fixture.head, &fixture.base]);
        assert_eq!(present, reachable);
        // The whole history up to the head, not a shallow fetch.
        assert_eq!(
            test_git(directory, &["rev-parse", "--is-shallow-repository"]),
            "false"
        );
        assert_eq!(
            test_git(directory, &["log", "--format=%H", "HEAD"]),
            test_git(fixture.host.path(), &["log", "--format=%H", &fixture.head])
        );
        for later in &fixture.later {
            assert!(!has_object(directory, later), "{later} is reachable");
        }
        // The base is not an ancestor of the head, and is still diffable.
        assert!(
            !std::process::Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(["merge-base", "--is-ancestor", &fixture.base, &fixture.head])
                .status()
                .unwrap()
                .success()
        );
        test_git(directory, &["diff", "--stat", &fixture.base, "HEAD"]);
        // With no remote, `git fetch` does nothing, and `git fetch origin`
        // and `git pull` fail.
        test_git(directory, &["fetch"]);
        assert_eq!(
            inventory(directory, &[&fixture.head, &fixture.base]).0,
            present
        );
        for arguments in [&["fetch", "origin"][..], &["pull"]] {
            assert!(
                !std::process::Command::new("git")
                    .arg("-C")
                    .arg(directory)
                    .args(arguments)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {arguments:?} succeeded"
            );
        }
        assert_eq!(test_git(fixture.host.path(), &["show-ref"]), host_refs);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn a_detached_checkout_without_later_history_and_base_keeps_no_refs() {
        let fixture = CutoffFixture::new();
        fixture.clone(None, None);
        fixture.initialize(None, None).unwrap();
        let directory = &fixture.directory;
        assert_eq!(test_git(directory, &["rev-parse", "HEAD"]), fixture.head);
        assert_eq!(test_git(directory, &["branch", "--show-current"]), "");
        assert_eq!(test_git(directory, &["for-each-ref"]), "");
        assert_eq!(
            test_git(directory, &["config", "mj.baseCommit"]),
            fixture.head
        );
        let (present, reachable) = inventory(directory, &[&fixture.head]);
        assert_eq!(present, reachable);
        assert!(!has_object(directory, &fixture.base));
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn a_checkout_fetched_from_a_cutoff_bundle_holds_exactly_the_head_and_base_ancestry() {
        let fixture = CutoffFixture::new();
        let cache = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        test_git(repository.path(), &["init", "--quiet", "--bare"]);
        for (commit, refname) in [
            (&fixture.head, "refs/mj/head"),
            (&fixture.base, "refs/mj/base"),
        ] {
            test_git(
                repository.path(),
                &[
                    "fetch",
                    "--quiet",
                    fixture.host.path().to_str().unwrap(),
                    &format!("{commit}:{refname}"),
                ],
            );
        }
        let bundle = cache.path().join("key.bundle");
        test_git(
            repository.path(),
            &[
                "bundle",
                "create",
                bundle.to_str().unwrap(),
                "refs/mj/head",
                "refs/mj/base",
            ],
        );
        fixture.clone(Some(&fixture.base), Some(&bundle));
        fixture
            .initialize(Some(&fixture.base), Some("town/run-123"))
            .unwrap();
        let (present, reachable) = inventory(&fixture.directory, &[&fixture.head, &fixture.base]);
        assert_eq!(present, reachable);
        assert_eq!(test_git(&fixture.directory, &["remote"]), "");
        assert!(
            !fixture
                .directory
                .join(".git/objects/info/alternates")
                .exists()
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    fn initialization_of_a_checkout_without_later_history_never_fetches_from_origin() {
        let fixture = CutoffFixture::new();
        test_git(
            fixture._root.path(),
            &["init", "--quiet", fixture.directory.to_str().unwrap()],
        );
        test_git(
            &fixture.directory,
            &[
                "remote",
                "add",
                "origin",
                fixture.host.path().to_str().unwrap(),
            ],
        );
        test_git(
            &fixture.directory,
            &["fetch", "--quiet", "origin", &fixture.base],
        );
        test_git(
            &fixture.directory,
            &["checkout", "--quiet", "--detach", &fixture.base],
        );
        let error = fixture.initialize(None, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("missing from the checkout without later history"),
            "{error:#}"
        );
        assert!(!has_object(&fixture.directory, &fixture.head));
    }

    #[test]
    fn a_checkpoint_of_a_checkout_without_later_history_records_the_session_source_as_origin() {
        let mut config = resume_compatibility_config();
        config
            .bundles
            .insert("project".into(), local_bundle(Path::new("/unused")));
        let mut session = super::super::test_support::checkpoint_test_session(EXACT_SESSION);
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalDocker {
            container_id: "hel-session".into(),
            borrowed_from: None,
        }));
        session.project = Some(
            mj_core::repository::ProjectBundleSnapshot {
                bundle: config.bundles["project"].clone(),
                identities: Default::default(),
                network_sources: [(
                    "project".to_owned(),
                    mj_core::remote_git::NetworkGitSource {
                        fetch_url: "https://github.com/example/project.git".into(),
                        push_urls: vec!["https://github.com/example/project.git".into()],
                    },
                )]
                .into(),
                plain_directories: Default::default(),
            }
            .into(),
        );
        let mut selection = mj_core::remote_git::ExactCheckout {
            repository_id: "project".into(),
            commit: "a".repeat(40),
            branch: None,
            no_later_history: true,
        };
        // A sub-agent works in its parent's workspace and records no checkout
        // of its own, so it asks the parent how that workspace began.
        let origin = |selection: &mj_core::remote_git::ExactCheckout, session_id: &str| {
            let mut session = session.clone();
            session.checkout = Some(Box::new(selection.clone()));
            let mut child = session.clone();
            child.id = "child-session".into();
            child.checkout = None;
            let mut state = mj_core::state::State::default();
            state.sessions.insert(EXACT_SESSION.into(), session);
            state.sessions.insert(child.id.clone(), child);
            state
                .environment_owners
                .insert(EXACT_SESSION.into(), EXACT_SESSION.into());
            state
                .environment_owners
                .insert("child-session".into(), EXACT_SESSION.into());
            state.subagents.insert(
                "child-session".into(),
                mj_core::subagent::SubagentMetadata {
                    kind: mj_core::subagent::SubagentKind::Worker,
                    child_session_id: "child-session".into(),
                    parent_session_id: EXACT_SESSION.into(),
                    task_name: "inspect".into(),
                    profile_id: "codex".into(),
                    model: None,
                    effort: None,
                    working_directory: PathBuf::new(),
                    request_key: "request-1".into(),
                    created_at: "2026-11-07T00:00:00Z".into(),
                    noticed_turn: None,
                    reported_finish: None,
                    handback_tool: false,
                    orchestrator: false,
                },
            );
            let controller = Controller {
                config: config.clone(),
                state,
            };
            let layout = controller
                .session_export_layout(session_id, &ProcessExecutor)
                .unwrap();
            assert_eq!(layout.repositories.len(), 1);
            layout.repositories[0].origin_override.clone()
        };
        let source = "https://github.com/example/project.git";
        assert_eq!(origin(&selection, EXACT_SESSION).as_deref(), Some(source));
        // Hard-won: #1464: a sub-agent's explicit checkpoint read its own empty checkout record and demanded an origin its parent's trimmed workspace lacks.
        assert_eq!(origin(&selection, "child-session").as_deref(), Some(source));
        // A normal exact checkout keeps its origin remote, which capture reads.
        selection.no_later_history = false;
        assert_eq!(origin(&selection, EXACT_SESSION), None);
        assert_eq!(origin(&selection, "child-session"), None);
    }
}
