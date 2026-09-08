//! Git initialization for independent, network-backed session checkouts.

use anyhow::{Context, Result, ensure};
use hel::hel_targets::{self, CommandExecutor};

use super::Controller;

impl Controller {
    /// Fresh workspaces start at the remote's advertised default branch. A
    /// matching marker makes retries harmless without resetting session work.
    pub(super) fn initialize_network_workspaces(
        &self,
        session_id: &str,
        backend: &hel_targets::TargetLocator,
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
        let root = workspace_root(backend);
        for repository in &bundle.repositories {
            let directory = std::path::Path::new(root).join(&repository.destination);
            initialize_workspace(executor, backend, session_id, &directory)?;
        }
        Ok(())
    }
}

pub(super) fn workspace_root(backend: &hel_targets::TargetLocator) -> &str {
    match backend {
        hel_targets::TargetLocator::LocalPodman { .. }
        | hel_targets::TargetLocator::LocalDocker { .. }
        | hel_targets::TargetLocator::AppleContainer { .. }
        | hel_targets::TargetLocator::SshPodman { .. }
        | hel_targets::TargetLocator::SshDocker { .. } => "/workspace",
        hel_targets::TargetLocator::AwsEc2 { workspace, .. }
        | hel_targets::TargetLocator::SshBare { workspace, .. } => workspace,
        hel_targets::TargetLocator::LocalBare { worker_root } => worker_root,
    }
}

fn initialize_workspace(
    executor: &impl CommandExecutor,
    backend: &hel_targets::TargetLocator,
    session_id: &str,
    directory: &std::path::Path,
) -> Result<()> {
    let git = |arguments: &[&str]| -> Result<hel_targets::CommandOutput> {
        let mut args = vec![
            "git".into(),
            "-C".into(),
            directory.to_string_lossy().into_owned(),
        ];
        args.extend(arguments.iter().map(|argument| (*argument).to_owned()));
        executor.execute(&hel_targets::command_on_locator(
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
    // Clone obtains origin/HEAD from the server, independent of host HEAD and
    // init.defaultBranch. An empty or misconfigured remote cannot seed work.
    let default_ref = checked(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .context("the network remote has no usable default branch")?;
    ensure!(
        default_ref.starts_with("refs/remotes/origin/"),
        "invalid default remote branch"
    );
    let base = checked(&[
        "rev-parse",
        "--verify",
        &format!("{default_ref}^{{commit}}"),
    ])?;
    let branch = format!("mj/{session_id}");
    checked(&["switch", "--no-track", "-c", &branch, &base])?;
    for (key, value) in [
        ("push.default", "current"),
        ("push.autoSetupRemote", "true"),
        ("remote.pushDefault", "origin"),
        ("remote.origin.mirror", "false"),
        ("mj.baseCommit", base.as_str()),
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

/// Resume uses archived network provenance, never the current host checkout.
pub(super) fn checkpoint_bundle(
    session: &hel::hel_state::SessionRecord,
) -> Result<hel_targets::ProjectBundleSpec> {
    let checkpoint = session
        .checkpoint
        .as_ref()
        .context("session has no checkpoint")?;
    let archive = hel::hel_archive::verify_archive_streaming(&checkpoint.archive_path)?;
    ensure!(
        archive.archive_sha256 == checkpoint.sha256 && archive.manifest.session.id == session.id,
        "persisted checkpoint verification failed"
    );
    bundle_from_manifest(&archive.manifest)
}

pub(super) fn bundle_from_manifest(
    manifest: &hel::hel_archive::ArchiveManifest,
) -> Result<hel_targets::ProjectBundleSpec> {
    ensure!(
        !manifest.repositories.is_empty(),
        "checkpoint has no network repository provenance; start a new network-backed session"
    );
    let repositories = manifest.repositories.iter().map(|repository| {
        let metadata = &repository.metadata;
        ensure!(metadata.remote_workspace,
            "resuming legacy isolated or raw-local sessions into an isolated workspace is not supported; start a new network-backed session");
        hel::hel_remote_git::validate_network_url(&metadata.origin)?;
        for url in &metadata.push_urls {
            hel::hel_remote_git::validate_network_url(url)?;
        }
        Ok(hel_targets::RepositorySpec {
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
    Ok(hel_targets::ProjectBundleSpec {
        primary,
        repositories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_controller::test_support::{
        committed_repository, local_bundle, resume_compatibility_config, test_git,
    };
    use hel::hel_targets::{CommandOutput, CommandSpec, ProcessExecutor};
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
        let mut archive = hel::hel_archive::verify_archive_streaming(
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
        for session_id in [
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
        ] {
            let plan = hel_targets::provision_plan(&template, session_id, &bundle, &[]).unwrap();
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
            let backend = hel_targets::TargetLocator::LocalBare {
                worker_root: destination.to_string_lossy().into_owned(),
            };
            initialize_workspace(&executor, &backend, session_id, &destination).unwrap();
            assert_eq!(test_git(&destination, &["rev-parse", "HEAD"]), initial);
            assert_eq!(
                test_git(&destination, &["branch", "--show-current"]),
                format!("mj/{session_id}")
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
            checked(&executor, &destination, &["push"]);
            let head = test_git(&destination, &["rev-parse", "HEAD"]);
            assert_eq!(
                test_git(&executor.push, &["rev-parse", &format!("mj/{session_id}")]),
                head
            );
            test_git(&destination, &["switch", "-c", "user-selected"]);
            initialize_workspace(&executor, &backend, session_id, &destination).unwrap();
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
        assert_eq!(test_git(&executor.push, &["rev-parse", "trunk"]), initial);
    }
}
