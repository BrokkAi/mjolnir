//! Git initialization for independent, network-backed session checkouts.

use crate::targets::{self, CommandExecutor};
use anyhow::{Context, Result, ensure};

use super::Controller;

impl Controller {
    /// Fresh workspaces start at the remote's advertised default branch. A
    /// matching marker makes retries harmless without resetting session work.
    pub(super) fn initialize_network_workspaces(
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
        if session.project_directory.is_some() {
            return Ok(());
        }
        let bundle = self
            .config
            .bundles
            .get(&session.bundle_id)
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
        for repository in &bundle.repositories {
            let directory = std::path::Path::new(&root).join(&repository.destination);
            initialize_workspace(
                executor,
                backend,
                session_id,
                &directory,
                session.launch_base.as_deref(),
                session.launch_branch.as_deref(),
                session
                    .checkout
                    .as_ref()
                    .filter(|checkout| checkout.repository_id == repository.id),
            )?;
        }
        Ok(())
    }
}

/// The directory the session's repositories are checked out under.
/// `container_workspace` is the session record's recorded container workspace,
/// which only container locators use.
pub(super) fn workspace_root(
    backend: &targets::TargetLocator,
    container_workspace: Option<&std::path::Path>,
) -> String {
    match backend {
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => {
            targets::container_workspace_root(container_workspace)
        }
        targets::TargetLocator::AwsEc2 { workspace, .. }
        | targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
        targets::TargetLocator::LocalBare { worker_root } => worker_root.clone(),
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
    if let Some(checkout) = checkout {
        prepare_exact_checkout(&git, &checked, session_id, checkout)?;
        return configure_workspace(&git, &checked, &checkout.commit);
    }
    let marker = git(&["config", "--local", "--get", "mj.remoteWorkspace"])?;
    ensure!(
        matches!(marker.status, 0 | 1),
        "could not inspect network workspace marker"
    );
    if marker.status == 0 {
        ensure!(
            String::from_utf8(marker.stdout)?.trim() == "true",
            "invalid network workspace marker"
        );
        return Ok(());
    }
    let default_ref = checked(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .context("the network remote has no usable default branch")?;
    let branch = match launch_branch {
        Some(branch) => {
            checked(&["check-ref-format", "--branch", branch])?;
            branch.to_owned()
        }
        None => default_ref
            .strip_prefix("refs/remotes/origin/")
            .context("invalid default remote branch")?
            .to_owned(),
    };
    let branch_tip = checked(&[
        "rev-parse",
        "--verify",
        &format!("refs/remotes/origin/{branch}^{{commit}}"),
    ])?;
    let base = match launch_base {
        // The clone holds only what the remote sent, so a host-only branch
        // name is not there to resolve. Say so rather than repeat Git's
        // "unknown revision".
        Some(revision) => checked(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])
        .with_context(|| {
            format!(
                "launch base {revision:?} is not in the clone; name a commit SHA, a tag, or origin/<branch>"
            )
        })?,
        // Clone obtains origin/HEAD from the server, independent of host HEAD
        // and init.defaultBranch. An empty or misconfigured remote cannot seed
        // work.
        None => branch_tip.clone(),
    };
    let local_branch = git(&[
        "show-ref",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])?;
    match local_branch.status {
        0 => {
            checked(&["switch", &branch])?;
        }
        1 => {
            checked(&["switch", "--no-track", "-c", &branch, &branch_tip])?;
        }
        _ => anyhow::bail!("could not inspect the selected local branch"),
    }
    configure_workspace(&git, &checked, &base)
}

fn configure_workspace(
    git: &impl Fn(&[&str]) -> Result<targets::CommandOutput>,
    checked: &impl Fn(&[&str]) -> Result<String>,
    base: &str,
) -> Result<()> {
    for (key, value) in [
        ("push.default", "current"),
        ("push.autoSetupRemote", "true"),
        ("remote.pushDefault", "origin"),
        ("remote.origin.mirror", "false"),
        ("mj.baseCommit", base),
        ("mj.remoteWorkspace", "true"),
    ] {
        checked(&["config", "--local", "--replace-all", key, value])?;
    }
    let removed = git(&["config", "--local", "--unset-all", "remote.origin.push"])?;
    ensure!(
        matches!(removed.status, 0 | 5),
        "could not clear inherited push refspec"
    );
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
pub(super) fn checkpoint_bundle(
    session: &mj_core::state::SessionRecord,
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
    bundle_from_manifest(&archive.manifest)
}

pub(super) fn bundle_from_manifest(
    manifest: &mj_checkpoint::archive::ArchiveManifest,
) -> Result<targets::ProjectBundleSpec> {
    ensure!(
        !manifest.repositories.is_empty(),
        "checkpoint has no network repository provenance; start a new network-backed session"
    );
    let repositories = manifest.repositories.iter().map(|repository| {
        let metadata = &repository.metadata;
        ensure!(metadata.remote_workspace,
            "resuming legacy isolated or raw-local sessions into an isolated workspace is not supported; start a new network-backed session");
        mj_core::remote_git::validate_network_url(&metadata.origin)?;
        for url in &metadata.push_urls {
            mj_core::remote_git::validate_network_url(url)?;
        }
        Ok(targets::RepositorySpec {
            url: Some(metadata.origin.clone()),
            push_urls: metadata.push_urls.clone(),
            destination: metadata.relative_destination.to_string_lossy().into_owned(),
            git_ref: None,
            reference: None,
        })
    }).collect::<Result<Vec<_>>>()?;
    let primary = manifest
        .repositories
        .iter()
        .find(|repository| repository.metadata.id == manifest.bundle.primary_repository)
        .context("checkpoint primary repository is missing")?
        .metadata
        .relative_destination
        .to_string_lossy()
        .into_owned();
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
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let mut command = command.clone();
            ensure!(command.program == "git", "fixture executes only Git");
            let mut arguments = vec![
                "-c".into(),
                format!(
                    "url.{}.insteadOf=https://fetch.example.test/repo.git",
                    self.fetch.display()
                ),
                "-c".into(),
                format!(
                    "url.{}.insteadOf=https://push.example.test/repo.git",
                    self.push.display()
                ),
            ];
            arguments.extend(command.args);
            command.args = arguments;
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
        session.checkpoint = Some(
            super::super::test_support::write_network_checkpoint_archive(
                root.path(),
                session_id,
                0,
            ),
        );
        let bundle = checkpoint_bundle(&session).unwrap();
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
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let output = ProcessExecutor.execute(command)?;
            if command.args.iter().any(|arg| arg == "switch") {
                anyhow::bail!("preparation interrupted after switch");
            }
            Ok(output)
        }
    }

    #[test]
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
    fn exact_checkout_applies_only_to_the_named_bundle_repository() {
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
        session.checkout = Some(fixture.selection.clone());
        let mut state = mj_core::state::State::default();
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
    }

    #[test]
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
            let plan =
                targets::provision_plan(&template, session_id, &bundle, &[], None, None).unwrap();
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
}
