//! Git setup inside managed workspaces. Host repositories are read-only inputs.

use super::*;
use hel::hel_git_proxy::LOCAL_SOURCE_REMOTE;

/// Resolve only a network remote; copying a local path would recreate host writes.
fn network_url(value: &str) -> Option<String> {
    if value.contains("://") {
        let mut url = url::Url::parse(value).ok()?;
        if !matches!(url.scheme(), "https" | "http" | "ssh" | "git") || url.host_str().is_none() {
            return None;
        }
        url.set_password(None).ok()?;
        if matches!(url.scheme(), "http" | "https") {
            url.set_username("").ok()?;
        }
        if url.host_str() == Some("github.com") {
            let github = crate::hel_setup::github_repository_from_origin(&format!(
                "https://github.com{}",
                url.path()
            ))?;
            return Some(format!(
                "https://github.com/{}/{}.git",
                github.owner, github.repository
            ));
        }
        return Some(url.into());
    }
    // Git's SCP-style SSH syntax, excluding paths and external helpers.
    let (host, path) = value.split_once(':')?;
    if host.len() <= 1
        || host.contains(['/', '\\', ' '])
        || path.is_empty()
        || path.starts_with(':')
    {
        return None;
    }
    if matches!(host, "git@github.com" | "github.com") {
        let github =
            crate::hel_setup::github_repository_from_origin(&format!("git@github.com:{path}"))?;
        return Some(format!(
            "https://github.com/{}/{}.git",
            github.owner, github.repository
        ));
    }
    Some(value.to_owned())
}

fn host_config(source: &Path, key: &str) -> Result<Option<String>> {
    let mut command = Command::new("git");
    command.args(["config", "--get", key]).current_dir(source);
    let output = hel::hel_subprocess::run_with_input(&mut command, &[])
        .context("read source repository's upstream configuration")?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned())),
        Some(1) => Ok(None),
        _ => bail!("could not read source repository's {key}"),
    }
}

pub(super) fn workspace_root(backend: &hel_targets::TargetLocator) -> PathBuf {
    match backend {
        hel_targets::TargetLocator::LocalPodman { .. }
        | hel_targets::TargetLocator::LocalDocker { .. }
        | hel_targets::TargetLocator::AppleContainer { .. }
        | hel_targets::TargetLocator::SshPodman { .. }
        | hel_targets::TargetLocator::SshDocker { .. } => PathBuf::from("/workspace"),
        hel_targets::TargetLocator::AwsEc2 { workspace, .. }
        | hel_targets::TargetLocator::SshBare { workspace, .. } => PathBuf::from(workspace),
        hel_targets::TargetLocator::LocalBare { worker_root } => PathBuf::from(worker_root),
    }
}

pub(super) struct WorkspaceGit<'a, E> {
    pub executor: &'a E,
    pub backend: &'a hel_targets::TargetLocator,
    pub session_id: &'a str,
    pub directory: PathBuf,
}

impl<E: CommandExecutor> WorkspaceGit<'_, E> {
    fn directory_argument(&self) -> String {
        let path = self.directory.to_string_lossy().into_owned();
        // Remote targets use POSIX paths, including from Windows controllers.
        #[cfg(windows)]
        if !matches!(self.backend, hel_targets::TargetLocator::LocalBare { .. }) {
            return path.replace('\\', "/");
        }
        path
    }

    fn run(&self, args: &[&str]) -> Result<CommandOutput> {
        let mut command = vec!["git".to_owned(), "-C".into(), self.directory_argument()];
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.executor.execute(&hel_targets::command_on_locator(
            self.backend,
            self.session_id,
            command,
            "configure managed repository Git",
        )?)
    }

    fn checked(&self, args: &[&str]) -> Result<String> {
        let output = self.run(args)?;
        ensure!(
            output.status == 0,
            "managed repository Git {} failed: {}",
            args.first().unwrap_or(&"command"),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn config(&self, key: &str, value: &str) -> Result<()> {
        self.checked(&["config", "--local", "--replace-all", key, value])?;
        Ok(())
    }

    fn unset(&self, key: &str) -> Result<()> {
        let output = self.run(&["config", "--local", "--unset-all", key])?;
        ensure!(
            matches!(output.status, 0 | 5),
            "could not clear managed Git setting {key}"
        );
        Ok(())
    }

    pub(super) fn has_head(&self) -> Result<bool> {
        Ok(self.run(&["rev-parse", "--verify", "HEAD"])?.status == 0)
    }

    pub(super) fn connect_source(&self, source: &Path, bridge: &str) -> Result<()> {
        self.config("protocol.ext.allow", "always")?;
        self.config(&format!("remote.{LOCAL_SOURCE_REMOTE}.url"), bridge)?;
        self.config(
            &format!("remote.{LOCAL_SOURCE_REMOTE}.fetch"),
            &format!("+refs/heads/*:refs/remotes/{LOCAL_SOURCE_REMOTE}/*"),
        )?;
        // Include detached source HEAD so seeding also works while the host rebases.
        self.checked(&[
            "config",
            "--local",
            "--add",
            &format!("remote.{LOCAL_SOURCE_REMOTE}.fetch"),
            &format!("+HEAD:refs/remotes/{LOCAL_SOURCE_REMOTE}/HEAD"),
        ])?;
        self.config(&format!("remote.{LOCAL_SOURCE_REMOTE}.tagOpt"), "--no-tags")?;
        let fetch = host_config(source, "remote.origin.url")?.and_then(|url| network_url(&url));
        let explicit_push = host_config(source, "remote.origin.pushurl")?;
        let push = explicit_push.as_deref().and_then(network_url);
        match fetch {
            Some(url) => {
                self.config("remote.origin.url", &url)?;
                self.config("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
                if let Some(push) = push {
                    self.config("remote.origin.pushurl", &push)?;
                } else if explicit_push.is_some() {
                    // An unsafe explicit push destination must not silently fall
                    // back to a different network repository.
                    self.config(
                        "remote.origin.pushurl",
                        "mj-no-upstream://configure-a-network-push-url",
                    )?;
                } else {
                    self.unset("remote.origin.pushurl")?;
                }
            }
            None => {
                let removed =
                    self.run(&["config", "--local", "--remove-section", "remote.origin"])?;
                ensure!(
                    matches!(removed.status, 0 | 128),
                    "could not remove unavailable Git origin"
                );
                tracing::info!(
                    session_id = self.session_id,
                    "local repository has no network origin; configure an upstream to publish session work"
                );
            }
        }
        self.checked(&["fetch", LOCAL_SOURCE_REMOTE])?;
        Ok(())
    }

    pub(super) fn initialize_branch(&self) -> Result<()> {
        self.config("push.default", "current")?;
        self.config("push.autoSetupRemote", "true")?;
        self.config("remote.pushDefault", "origin")?;
        self.unset("remote.origin.push")?;
        self.config("remote.origin.mirror", "false")?;
        let branch = format!("mj/{}", self.session_id);
        let marker = self.run(&["config", "--get", "mj.sessionBranch"])?;
        ensure!(
            matches!(marker.status, 0 | 1),
            "could not inspect managed branch policy"
        );
        // A seed can come from another managed checkout. Only this session's
        // own marker can preserve a subsequently user-selected branch.
        if marker.status == 0 && String::from_utf8(marker.stdout)?.trim() == branch {
            return Ok(());
        }
        self.checked(&["rev-parse", "--verify", "HEAD"])?;
        let inspection = hel_targets::command_on_locator(self.backend, self.session_id, vec![
            "sh".into(), "-c".into(),
            "for state in rebase-merge rebase-apply sequencer MERGE_HEAD CHERRY_PICK_HEAD REVERT_HEAD REBASE_HEAD; do path=$(git -C \"$1\" rev-parse --path-format=absolute --git-path \"$state\") || exit 2; if test -e \"$path\"; then exit 1; fi; done".into(),
            "mj-git-operation-check".into(), self.directory_argument(),
        ], "inspect active Git operations before branch migration")?;
        ensure!(
            self.executor.execute(&inspection)?.status == 0,
            "finish the repository's active Git operation before migrating its session branch"
        );
        let current = self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        ensure!(
            matches!(current.status, 0 | 1),
            "could not inspect managed checkout branch"
        );
        if String::from_utf8(current.stdout)?.trim() != branch {
            // -b fails on a collision, unlike -B which would reset existing work.
            self.checked(&["checkout", "--no-track", "-b", &branch])?;
        }
        self.unset(&format!("branch.{branch}.pushRemote"))?;
        self.unset(&format!("branch.{branch}.remote"))?;
        self.unset(&format!("branch.{branch}.merge"))?;
        self.config("mj.sessionBranch", &branch)?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use hel::hel_archive::{
        GitCollectionSpec, GitHistoryMode, SystemGit, collect_git_snapshot, restore_git_snapshot,
    };
    use hel::hel_targets::ProcessExecutor;

    fn git(path: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.args(args).current_dir(path);
        let result = hel::hel_subprocess::run_with_input(&mut command, &[]).unwrap();
        assert!(
            result.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }

    fn repository(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        identity(path);
        std::fs::write(path.join("tracked"), "base\n").unwrap();
        git(path, &["add", "tracked"]);
        git(path, &["commit", "-qm", "base"]);
    }

    fn identity(path: &Path) {
        git(path, &["config", "user.name", "Git Isolation Test"]);
        git(
            path,
            &["config", "user.email", "git-isolation@example.test"],
        );
    }

    #[test]
    fn sessions_push_distinct_branches_and_restore_published_work_without_touching_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let original_head = git(&source, &["rev-parse", "HEAD"]);
        let original_index = std::fs::read(source.join(".git/index")).unwrap();
        let upstream = temp.path().join("upstream.git");
        git(
            temp.path(),
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                upstream.to_str().unwrap(),
            ],
        );
        git(
            &source,
            &[
                "remote",
                "add",
                "origin",
                "https://upstream.example.test/repo.git",
            ],
        );
        for id in ["a".repeat(32), "b".repeat(32)] {
            let backend = hel_targets::TargetLocator::LocalBare {
                worker_root: temp.path().join(&id).to_string_lossy().into_owned(),
            };
            let target = temp.path().join(&id);
            git(
                temp.path(),
                &["clone", source.to_str().unwrap(), target.to_str().unwrap()],
            );
            identity(&target);
            // Resolve the fixture's network URL locally without involving a server.
            git(
                &target,
                &[
                    "config",
                    &format!("url.{}.insteadOf", upstream.display()),
                    "https://upstream.example.test/repo.git",
                ],
            );
            let setup = WorkspaceGit {
                executor: &ProcessExecutor,
                backend: &backend,
                session_id: &id,
                directory: target.clone(),
            };
            setup
                .connect_source(&source, source.to_str().unwrap())
                .unwrap();
            std::fs::write(target.join("tracked"), "staged\n").unwrap();
            git(&target, &["add", "tracked"]);
            std::fs::write(target.join("tracked"), "unstaged\n").unwrap();
            git(
                &target,
                &["config", "mj.sessionBranch", "mj/another-session"],
            );
            let staged = git(&target, &["diff", "--cached"]);
            let unstaged = git(&target, &["diff"]);
            setup.initialize_branch().unwrap();
            setup.initialize_branch().unwrap();
            assert_eq!(git(&target, &["diff", "--cached"]), staged);
            assert_eq!(git(&target, &["diff"]), unstaged);
            git(&target, &["commit", "-qam", "session changes"]);
            git(&target, &["push"]);
            let branch = format!("mj/{id}");
            let head = git(&target, &["rev-parse", "HEAD"]);
            assert_eq!(git(&upstream, &["rev-parse", &branch]), head);
            assert_eq!(git(&upstream, &["rev-parse", "main"]), original_head);
            assert_eq!(git(&source, &["rev-parse", "HEAD"]), original_head);
            assert_eq!(
                std::fs::read(source.join(".git/index")).unwrap(),
                original_index
            );
            assert_eq!(
                std::fs::read_to_string(source.join("tracked")).unwrap(),
                "base\n"
            );
            // A user-selected branch survives checkpoint/restore and setup.
            git(&target, &["checkout", "-qb", "user-chosen"]);
            std::fs::write(target.join("untracked"), vec![b'x'; 192 * 1024]).unwrap();
            let snapshot = collect_git_snapshot(
                &SystemGit,
                &target,
                &GitCollectionSpec {
                    id: "repo".into(),
                    relative_destination: "repo".into(),
                    history: GitHistoryMode::SessionDeltaFromRemote(LOCAL_SOURCE_REMOTE.into()),
                    origin_override: Some("mj-local:repo".into()),
                },
            )
            .unwrap();
            assert!(
                !snapshot.committed_bundle.is_empty(),
                "published work still belongs in the local-source checkpoint"
            );
            assert_eq!(
                snapshot.metadata.session_branch.as_deref(),
                Some(branch.as_str())
            );
            let restored = temp.path().join(format!("restored-{id}"));
            git(
                temp.path(),
                &[
                    "clone",
                    source.to_str().unwrap(),
                    restored.to_str().unwrap(),
                ],
            );
            restore_git_snapshot(&SystemGit, &restored, &snapshot).unwrap();
            WorkspaceGit {
                directory: restored.clone(),
                ..setup
            }
            .initialize_branch()
            .unwrap();
            assert_eq!(git(&restored, &["branch", "--show-current"]), "user-chosen");
            assert_eq!(git(&restored, &["rev-parse", "HEAD"]), head);
            assert_eq!(
                std::fs::read(restored.join("untracked")).unwrap().len(),
                192 * 1024
            );
        }
    }

    #[test]
    fn branch_migration_refuses_an_active_rebase_and_existing_branch_collision() {
        let temp = tempfile::tempdir().unwrap();
        repository(temp.path());
        let backend = hel_targets::TargetLocator::LocalBare {
            worker_root: temp.path().join("legacy-id").to_string_lossy().into_owned(),
        };
        let setup = WorkspaceGit {
            executor: &ProcessExecutor,
            backend: &backend,
            session_id: "legacy-id",
            directory: temp.path().to_path_buf(),
        };
        let index = std::fs::read(temp.path().join(".git/index")).unwrap();
        std::fs::create_dir(temp.path().join(".git/rebase-merge")).unwrap();
        assert!(
            setup
                .initialize_branch()
                .unwrap_err()
                .to_string()
                .contains("active Git operation")
        );
        assert_eq!(git(temp.path(), &["branch", "--show-current"]), "main");
        std::fs::remove_dir(temp.path().join(".git/rebase-merge")).unwrap();
        git(temp.path(), &["branch", "mj/legacy-id"]);
        assert!(setup.initialize_branch().is_err());
        assert_eq!(git(temp.path(), &["branch", "--show-current"]), "main");
        assert_eq!(
            std::fs::read(temp.path().join(".git/index")).unwrap(),
            index
        );
    }

    #[test]
    fn a_repository_without_network_upstream_remains_usable_locally() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        repository(&source);
        let target = temp.path().join("target");
        git(
            temp.path(),
            &["clone", source.to_str().unwrap(), target.to_str().unwrap()],
        );
        let backend = hel_targets::TargetLocator::LocalBare {
            worker_root: temp
                .path()
                .join("offline-id")
                .to_string_lossy()
                .into_owned(),
        };
        let setup = WorkspaceGit {
            executor: &ProcessExecutor,
            backend: &backend,
            session_id: "offline-id",
            directory: target.clone(),
        };
        setup
            .connect_source(&source, source.to_str().unwrap())
            .unwrap();
        setup.initialize_branch().unwrap();
        assert_eq!(git(&target, &["branch", "--show-current"]), "mj/offline-id");
        assert_eq!(
            setup
                .run(&["config", "--get", "remote.origin.url"])
                .unwrap()
                .status,
            1
        );
        assert_ne!(setup.run(&["push"]).unwrap().status, 0);
        assert_eq!(git(&source, &["branch", "--show-current"]), "main");
    }

    #[test]
    fn network_upstreams_preserve_ssh_users_and_strip_credentials() {
        assert_eq!(
            network_url("https://user:secret@example.com/repo.git"),
            Some("https://example.com/repo.git".into())
        );
        assert_eq!(
            network_url("ssh://git@example.com/repo.git"),
            Some("ssh://git@example.com/repo.git".into())
        );
        assert_eq!(
            network_url("git@github.com:owner/repo.git"),
            Some("https://github.com/owner/repo.git".into())
        );
        for source in [
            "/host/repo",
            "../repo",
            "C:\\repo",
            "file:///host/repo",
            "ext::git receive-pack /host/repo",
        ] {
            assert_eq!(network_url(source), None, "{source}");
        }
    }
}
