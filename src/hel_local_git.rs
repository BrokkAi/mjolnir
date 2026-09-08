//! Controller-side support for repositories configured with `local` sources.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::hel_config::ProjectBundle;
use crate::hel_remote_git::{NetworkGitSource, display_url, validate_network_url};
use crate::hel_targets::{CommandExecutor, CommandOutput, CommandSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyLocalRepository {
    pub id: String,
    pub path: PathBuf,
    pub summary: String,
}

pub fn dirty_local_repositories(bundle: &ProjectBundle) -> Result<Vec<DirtyLocalRepository>> {
    bundle
        .repositories
        .iter()
        .filter_map(|repository| repository.local.as_ref().map(|path| (repository, path)))
        .filter_map(|(repository, path)| match local_status(path) {
            Ok(Some(summary)) => Some(Ok(DirtyLocalRepository {
                id: repository.id.clone(),
                path: path.clone(),
                summary,
            })),
            Ok(None) => None,
            Err(error) => Some(Err(
                error.context(format!("inspect local repository {:?}", repository.id))
            )),
        })
        .collect()
}

pub fn canonical_repository(path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .with_context(|| format!("start git in {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} is not a Git repository with a readable worktree: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root = String::from_utf8(output.stdout).context("decode Git repository root")?;
    let root = PathBuf::from(root.trim());
    let root = std::fs::canonicalize(&root)
        .with_context(|| format!("canonicalize local repository {}", root.display()))?;
    main_worktree_root(&root)
}

/// Map a repository top level to the top level of its main working tree.
///
/// A linked worktree created by `git worktree add` reports its own top level,
/// but Hel treats it as the same repository as the main working tree.
pub fn main_worktree_root(root: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(root)
        .output()
        .with_context(|| format!("start git in {}", root.display()))?;
    if !output.status.success() {
        bail!(
            "could not read the common Git directory for {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let common = String::from_utf8(output.stdout).context("decode common Git directory")?;
    let common = PathBuf::from(common.trim());
    // A bare or otherwise unusual layout has no `.git` parent to fall back to.
    if common.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return Ok(root.to_path_buf());
    }
    let Some(parent) = common.parent().filter(|parent| parent.is_dir()) else {
        return Ok(root.to_path_buf());
    };
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("canonicalize main worktree {}", parent.display()))?;
    if parent == root {
        return Ok(root.to_path_buf());
    }
    Ok(parent)
}

fn local_status(path: &Path) -> Result<Option<String>> {
    let root = canonical_repository(path)?;
    let head = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(&root)
        .output()
        .with_context(|| format!("read HEAD in {}", root.display()))?;
    if !head.status.success() {
        bail!("local repository {} has no commit at HEAD", root.display());
    }
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=normal"])
        .current_dir(&root)
        .output()
        .with_context(|| format!("read Git status in {}", root.display()))?;
    if !output.status.success() {
        bail!(
            "git status failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let status = String::from_utf8(output.stdout).context("decode Git status")?;
    let mut lines = status.lines();
    let Some(first) = lines.next() else {
        return Ok(None);
    };
    let remaining = lines.count();
    let summary = if remaining == 0 {
        first.to_owned()
    } else {
        format!("{first} (and {remaining} more)")
    };
    Ok(Some(summary))
}

/// Resolve the fetch and push endpoints Git would use for a host checkout.
///
/// Git performs URL rewriting while answering `remote get-url`, so the
/// values returned here are the actual network endpoints that should be
/// persisted for the target checkout. The local checkout may be dirty or
/// have commits that are not published; neither affects endpoint selection.
pub fn resolve_local_repository(
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<NetworkGitSource> {
    let branch = git_text(
        path,
        ["branch", "--show-current"],
        executor,
        "read current branch",
    )?;
    let branch = branch.trim();
    let branch = (!branch.is_empty()).then_some(branch);

    let remote_output = git_output(path, ["remote"], executor, "list Git remotes")?;
    let remotes = parse_lines(&remote_output.stdout, "Git remote names")?;
    let branch_remote = branch
        .map(|branch| format!("branch.{branch}.remote"))
        .map(|key| git_config(path, &key, executor));
    let branch_remote = match branch_remote {
        Some(result) => result?,
        None => None,
    };

    let fetch_remote = if let Some(remote) = branch_remote {
        ensure_remote_name(&remote, "current branch")?;
        if !remotes.iter().any(|name| name == &remote) {
            bail!("current branch names Git remote {remote:?}, but that remote is not configured");
        }
        remote
    } else {
        match remotes.as_slice() {
            [] => bail!("repository has no configured Git remotes"),
            [remote] => remote.clone(),
            _ if remotes.iter().any(|remote| remote == "origin") => "origin".to_owned(),
            _ => bail!(
                "repository has multiple Git remotes but no current-branch remote or `origin` to select"
            ),
        }
    };

    let push_remote = if let Some(branch) = branch {
        match git_config(path, &format!("branch.{branch}.pushRemote"), executor)? {
            Some(remote) => remote,
            None => git_config(path, "remote.pushDefault", executor)?
                .unwrap_or_else(|| fetch_remote.clone()),
        }
    } else {
        git_config(path, "remote.pushDefault", executor)?.unwrap_or_else(|| fetch_remote.clone())
    };
    ensure_remote_name(&push_remote, "push")?;
    if !remotes.iter().any(|name| name == &push_remote) {
        bail!(
            "push configuration names Git remote {push_remote:?}, but that remote is not configured"
        );
    }

    let fetch_url = remote_url(path, &fetch_remote, false, executor)?;
    let push_urls = remote_urls(path, &push_remote, true, executor)?;
    validate_network_url(&fetch_url).with_context(|| {
        format!(
            "fetch URL {} for Git remote {fetch_remote:?}",
            display_url(&fetch_url)
        )
    })?;
    for push_url in &push_urls {
        validate_network_url(push_url).with_context(|| {
            format!(
                "push URL {} for Git remote {push_remote:?}",
                display_url(push_url)
            )
        })?;
    }
    Ok(NetworkGitSource {
        fetch_url,
        push_urls,
    })
}

fn git_command(path: &Path, args: impl IntoIterator<Item = impl Into<String>>) -> CommandSpec {
    let mut all = vec!["-C".to_owned(), path.to_string_lossy().into_owned()];
    all.extend(args.into_iter().map(Into::into));
    let mut command = CommandSpec::new("git", all);
    command
        .env
        .insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    command
        .env
        .insert("GIT_NO_LAZY_FETCH".to_owned(), "1".to_owned());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env.insert(
            "GIT_SSH_COMMAND".to_owned(),
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"
                .to_owned(),
        );
    }
    command
}

fn git_output(
    path: &Path,
    args: impl IntoIterator<Item = impl Into<String>>,
    executor: &impl CommandExecutor,
    purpose: &str,
) -> Result<CommandOutput> {
    if executor.cancellation_requested() {
        bail!("operation cancelled while {purpose}");
    }
    let command = git_command(path, args);
    let output = executor
        .execute(&command)
        .with_context(|| format!("{purpose} in {}", path.display()))?;
    if executor.cancellation_requested() {
        bail!("operation cancelled while {purpose}");
    }
    if output.status != 0 {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!(
                "{purpose} in {} failed with status {}",
                path.display(),
                output.status
            );
        }
        bail!("{purpose} in {} failed: {detail}", path.display());
    }
    Ok(output)
}

fn git_text(
    path: &Path,
    args: impl IntoIterator<Item = impl Into<String>>,
    executor: &impl CommandExecutor,
    purpose: &str,
) -> Result<String> {
    let output = git_output(path, args, executor, purpose)?;
    String::from_utf8(output.stdout).with_context(|| format!("decode {purpose} output"))
}

fn git_config(path: &Path, key: &str, executor: &impl CommandExecutor) -> Result<Option<String>> {
    let output = git_command(path, ["config", "--get", key]);
    if executor.cancellation_requested() {
        bail!("operation cancelled while read Git configuration");
    }
    let output = executor
        .execute(&output)
        .with_context(|| format!("read Git configuration key {key:?}"))?;
    if executor.cancellation_requested() {
        bail!("operation cancelled while read Git configuration");
    }
    match output.status {
        0 => Ok(Some(
            String::from_utf8(output.stdout)
                .with_context(|| format!("decode Git configuration key {key:?}"))?
                .trim()
                .to_owned(),
        )),
        1 => Ok(None),
        status => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            if detail.is_empty() {
                bail!("read Git configuration key {key:?} failed with status {status}");
            }
            bail!("read Git configuration key {key:?} failed: {detail}");
        }
    }
}

fn parse_lines(bytes: &[u8], kind: &str) -> Result<Vec<String>> {
    let text = String::from_utf8(bytes.to_vec()).with_context(|| format!("decode {kind}"))?;
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            if line.chars().any(char::is_whitespace) {
                bail!("{kind} contains an invalid whitespace-bearing entry");
            }
            Ok(line.to_owned())
        })
        .collect()
}

fn ensure_remote_name(name: &str, role: &str) -> Result<()> {
    if name.is_empty() || name == "." || name.chars().any(char::is_whitespace) {
        bail!("{role} Git remote selection is empty or refers to the local repository");
    }
    Ok(())
}

fn remote_url(
    path: &Path,
    remote: &str,
    push: bool,
    executor: &impl CommandExecutor,
) -> Result<String> {
    let urls = remote_urls(path, remote, push, executor)?;
    urls.into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Git remote {remote:?} has no configured URL"))
}

fn remote_urls(
    path: &Path,
    remote: &str,
    push: bool,
    executor: &impl CommandExecutor,
) -> Result<Vec<String>> {
    let args = if push {
        vec![
            "remote".to_owned(),
            "get-url".to_owned(),
            "--push".to_owned(),
            "--all".to_owned(),
            remote.to_owned(),
        ]
    } else {
        vec!["remote".to_owned(), "get-url".to_owned(), remote.to_owned()]
    };
    let output = git_output(
        path,
        args,
        executor,
        if push {
            "read Git push URL"
        } else {
            "read Git fetch URL"
        },
    )?;
    let urls = parse_lines(
        &output.stdout,
        if push {
            "Git push URLs"
        } else {
            "Git fetch URL"
        },
    )?;
    if urls.is_empty() {
        bail!(
            "Git remote {remote:?} has no {} URL",
            if push { "push" } else { "fetch" }
        );
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_targets::ProcessExecutor;
    use std::fs;

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn local_status_distinguishes_clean_and_dirty_repositories() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q", "-b", "main"]);
        git(directory.path(), &["config", "user.name", "Hel Test"]);
        git(
            directory.path(),
            &["config", "user.email", "hel@example.test"],
        );
        fs::write(directory.path().join("tracked"), "clean").unwrap();
        git(directory.path(), &["add", "."]);
        git(directory.path(), &["commit", "-qm", "base"]);
        assert_eq!(local_status(directory.path()).unwrap(), None);
        fs::write(directory.path().join("untracked"), "dirty").unwrap();
        assert!(
            local_status(directory.path())
                .unwrap()
                .unwrap()
                .contains("untracked")
        );
    }

    #[test]
    fn canonical_repository_maps_a_linked_worktree_to_its_main_repository() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("main");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["config", "user.name", "Hel Test"]);
        git(&main, &["config", "user.email", "hel@example.test"]);
        fs::write(main.join("tracked"), "clean").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "base"]);
        let worktree = directory.path().join("main2");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                worktree.to_str().unwrap(),
            ],
        );

        let expected = fs::canonicalize(&main).unwrap();
        assert_eq!(canonical_repository(&main).unwrap(), expected);
        assert_eq!(canonical_repository(&worktree).unwrap(), expected);
    }

    fn initialized_repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q", "-b", "main"]);
        directory
    }

    #[test]
    fn resolver_uses_a_nonorigin_sole_remote_for_fetch_and_push() {
        let directory = initialized_repository();
        git(
            directory.path(),
            &[
                "remote",
                "add",
                "upstream",
                "https://example.com/upstream.git",
            ],
        );

        let source = resolve_local_repository(directory.path(), &ProcessExecutor).unwrap();
        assert_eq!(source.fetch_url, "https://example.com/upstream.git");
        assert_eq!(source.push_urls, ["https://example.com/upstream.git"]);
    }

    #[test]
    fn resolver_honors_branch_push_remote_and_multiple_push_urls() {
        let directory = initialized_repository();
        git(
            directory.path(),
            &["remote", "add", "fetch", "https://example.com/fetch.git"],
        );
        git(
            directory.path(),
            &[
                "remote",
                "add",
                "publish",
                "https://example.com/publish.git",
            ],
        );
        git(directory.path(), &["config", "branch.main.remote", "fetch"]);
        git(
            directory.path(),
            &["config", "branch.main.pushRemote", "publish"],
        );
        git(
            directory.path(),
            &[
                "config",
                "--add",
                "remote.publish.pushurl",
                "ssh://git@example.com/one.git",
            ],
        );
        git(
            directory.path(),
            &[
                "config",
                "--add",
                "remote.publish.pushurl",
                "ssh://git@example.com/two.git",
            ],
        );

        let source = resolve_local_repository(directory.path(), &ProcessExecutor).unwrap();
        assert_eq!(source.fetch_url, "https://example.com/fetch.git");
        assert_eq!(
            source.push_urls,
            [
                "ssh://git@example.com/one.git",
                "ssh://git@example.com/two.git"
            ]
        );
    }

    #[test]
    fn resolver_uses_remote_push_default_when_branch_has_no_push_remote() {
        let directory = initialized_repository();
        git(
            directory.path(),
            &["remote", "add", "fetch", "https://example.com/fetch.git"],
        );
        git(
            directory.path(),
            &[
                "remote",
                "add",
                "publish",
                "https://example.com/publish.git",
            ],
        );
        git(directory.path(), &["config", "branch.main.remote", "fetch"]);
        git(
            directory.path(),
            &["config", "remote.pushDefault", "publish"],
        );

        let source = resolve_local_repository(directory.path(), &ProcessExecutor).unwrap();
        assert_eq!(source.fetch_url, "https://example.com/fetch.git");
        assert_eq!(source.push_urls, ["https://example.com/publish.git"]);
    }

    #[test]
    fn resolver_applies_fetch_and_push_url_rewrites() {
        let directory = initialized_repository();
        git(
            directory.path(),
            &["remote", "add", "fetch", "fetch:org/repo.git"],
        );
        git(
            directory.path(),
            &["remote", "add", "publish", "publish:org/repo.git"],
        );
        git(directory.path(), &["config", "branch.main.remote", "fetch"]);
        git(
            directory.path(),
            &["config", "branch.main.pushRemote", "publish"],
        );
        git(
            directory.path(),
            &["config", "url.https://example.com/.insteadOf", "fetch:"],
        );
        git(
            directory.path(),
            &[
                "config",
                "url.ssh://git@example.com/.pushInsteadOf",
                "publish:",
            ],
        );

        let source = resolve_local_repository(directory.path(), &ProcessExecutor).unwrap();
        assert_eq!(source.fetch_url, "https://example.com/org/repo.git");
        assert_eq!(source.push_urls, ["ssh://git@example.com/org/repo.git"]);
    }

    #[test]
    fn resolver_rejects_local_paths_and_ignores_dirty_or_unpublished_state() {
        let directory = initialized_repository();
        fs::write(directory.path().join("untracked"), "work").unwrap();
        git(
            directory.path(),
            &["remote", "add", "origin", "../another-repository"],
        );
        let error = resolve_local_repository(directory.path(), &ProcessExecutor).unwrap_err();
        assert!(
            format!("{error:#}").contains("local repository path"),
            "{error:#}"
        );
    }

    #[test]
    fn resolver_reports_missing_and_ambiguous_remote_selection() {
        let no_remote = initialized_repository();
        let error = resolve_local_repository(no_remote.path(), &ProcessExecutor).unwrap_err();
        assert!(error.to_string().contains("no configured Git remotes"));

        let ambiguous = initialized_repository();
        git(
            ambiguous.path(),
            &["remote", "add", "first", "https://example.com/first.git"],
        );
        git(
            ambiguous.path(),
            &["remote", "add", "second", "https://example.com/second.git"],
        );
        let error = resolve_local_repository(ambiguous.path(), &ProcessExecutor).unwrap_err();
        assert!(error.to_string().contains("multiple Git remotes"));
    }
}
