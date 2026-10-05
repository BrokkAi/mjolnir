//! Repository identity is independent of a checkout's location and push settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::targets::{CommandExecutor, CommandSpec};

/// A session's accepted project shape. Catalog merges change its project ID,
/// not repository IDs, checkout layout, or source settings already accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBundleSnapshot {
    pub bundle: crate::config::ProjectBundle,
    pub identities: BTreeMap<String, RepositoryIdentity>,
    #[serde(default)]
    pub network_sources: BTreeMap<String, crate::remote_git::NetworkGitSource>,
}

impl ProjectBundleSnapshot {
    pub fn key(&self) -> Result<String> {
        anyhow::ensure!(
            self.identities.len() == self.bundle.repositories.len(),
            "project identities do not cover its repositories"
        );
        let primary = self
            .identities
            .get(&self.bundle.primary_repo)
            .context("project primary identity is missing")?;
        let mut members = self
            .bundle
            .repositories
            .iter()
            .map(|repository| {
                Ok((
                    self.identities
                        .get(&repository.id)
                        .context("project member identity is missing")?
                        .key(),
                    repository.destination.clone(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        members.sort();
        Ok(format!(
            "bundle:{}",
            serde_json::to_string(&(primary.key(), members))?
        ))
    }

    pub fn source_key(&self) -> Result<String> {
        if self.bundle.repositories.len() == 1 {
            return Ok(self
                .identities
                .get(&self.bundle.primary_repo)
                .context("project primary identity is missing")?
                .key());
        }
        self.key()
    }

    pub fn name(&self) -> String {
        let mut names = self
            .bundle
            .repositories
            .iter()
            .filter_map(|repository| {
                self.identities
                    .get(&repository.id)
                    .map(RepositoryIdentity::name)
            })
            .collect::<Vec<_>>();
        names.dedup();
        names.join(" + ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryIdentity {
    Github(String, String),
    Network(String),
    Local(PathBuf),
    RemoteDirectory { host: String, root: PathBuf },
}

fn github_path(source: &str) -> Option<&str> {
    source
        .strip_prefix("https://github.com/")
        .or_else(|| source.strip_prefix("http://github.com/"))
        .or_else(|| source.strip_prefix("git@github.com:"))
        .or_else(|| source.strip_prefix("github.com:"))
        .or_else(|| source.strip_prefix("ssh://github.com/"))
        .or_else(|| (!source.contains("://") && !source.contains(['@', ':'])).then_some(source))
}

impl RepositoryIdentity {
    /// Normalize only equivalences guaranteed by the remote provider. Other
    /// servers can have case-sensitive paths or different transport namespaces.
    pub fn from_remote(source: &str) -> Option<Self> {
        let source = crate::remote_git::display_url(source.trim());
        let source = source.as_str();
        let github = github_path(source);
        if let Some(path) = github {
            let path = path.trim_end_matches('/').trim_end_matches(".git");
            if let Some((owner, repository)) = path.split_once('/')
                && !owner.is_empty()
                && !repository.is_empty()
                && !repository.contains('/')
                && !path.chars().any(char::is_whitespace)
                && owner
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
                && !matches!(repository, "." | "..")
            {
                return Some(Self::Github(
                    owner.to_ascii_lowercase(),
                    repository.to_ascii_lowercase(),
                ));
            }
        }
        crate::remote_git::validate_network_url(source).ok()?;
        Some(Self::Network(
            crate::remote_git::display_url(source)
                .trim_end_matches('/')
                .trim_end_matches(".git")
                .to_owned(),
        ))
    }

    pub fn remote_label(source: &str) -> Option<String> {
        let clean = crate::remote_git::display_url(source.trim());
        match Self::from_remote(&clean)? {
            Self::Github(_, _) => Some(
                github_path(&clean)?
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .to_owned(),
            ),
            Self::Network(url) => Some(url),
            _ => None,
        }
    }

    pub fn key(&self) -> String {
        match self {
            Self::Github(owner, repository) => format!("github:{owner}/{repository}"),
            Self::Network(url) => format!("git:{url}"),
            Self::Local(root) => format!("path:{}", root.display()),
            Self::RemoteDirectory { host, root } => format!("path:{host}:{}", root.display()),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Github(_, repository) => repository.clone(),
            Self::Network(url) => url.rsplit(['/', ':']).next().unwrap_or(url).to_owned(),
            Self::Local(root) | Self::RemoteDirectory { root, .. } => root
                .file_name()
                .unwrap_or(root.as_os_str())
                .to_string_lossy()
                .into_owned(),
        }
    }
}

/// Resolve identity without using the current feature branch or altering Git
/// configuration. The caller's executor bounds any advertised-HEAD request.
pub fn local_identity(path: &Path, executor: &impl CommandExecutor) -> Result<RepositoryIdentity> {
    Ok(resolve_directory(path, None, executor)?.identity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDirectory {
    pub checkout_root: PathBuf,
    pub repository_root: PathBuf,
    pub identity: RepositoryIdentity,
}

/// Expected absence when discovering historical directories. Other resolver
/// failures still indicate an error that discovery must report and retry.
#[derive(Debug)]
pub enum RepositoryUnavailable {
    Directory(PathBuf),
    NotRepository(PathBuf),
}

impl std::fmt::Display for RepositoryUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Directory(path) => write!(
                formatter,
                "project directory {} is unavailable",
                path.display()
            ),
            Self::NotRepository(path) => {
                write!(formatter, "{} is not a Git repository", path.display())
            }
        }
    }
}

impl std::error::Error for RepositoryUnavailable {}

/// The executor can run on the controller or translate Git commands to SSH.
/// Preserve the selected checkout while collapsing its identity to the owner
/// of the common Git directory.
pub fn resolve_directory(
    path: &Path,
    host: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<ResolvedDirectory> {
    let directory = if host.is_none() {
        std::fs::canonicalize(path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                    anyhow::Error::new(RepositoryUnavailable::Directory(path.to_owned()))
                }
                _ => anyhow::Error::new(error),
            })
            .with_context(|| format!("resolve project directory {}", path.display()))?
    } else {
        anyhow::ensure!(
            path.is_absolute(),
            "remote project directory must be absolute"
        );
        path.to_owned()
    };
    if host.is_none()
        && !std::fs::metadata(&directory)
            .with_context(|| format!("inspect project directory {}", directory.display()))?
            .is_dir()
    {
        return Err(RepositoryUnavailable::Directory(path.to_owned()).into());
    }
    let mut command = CommandSpec::new(
        "git",
        [
            "-C".to_owned(),
            directory.to_string_lossy().into_owned(),
            "rev-parse".into(),
            "--show-toplevel".into(),
            "--git-common-dir".into(),
        ],
    )
    .purpose("resolve project repository and checkout");
    command.env.insert("LC_ALL".into(), "C".into());
    let output = executor.execute(&command)?;
    if output.status == 128 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let diagnostic = stderr.trim();
        if diagnostic.starts_with("fatal: not a git repository") {
            return Err(RepositoryUnavailable::NotRepository(path.to_owned()).into());
        }
        // Remote paths cannot be inspected on the controller. Git's explicit
        // absence diagnosis is distinct from SSH or filesystem access errors.
        if diagnostic.starts_with("fatal: cannot change to '")
            && (diagnostic.ends_with("': No such file or directory")
                || diagnostic.ends_with("': Not a directory"))
        {
            return Err(RepositoryUnavailable::Directory(path.to_owned()).into());
        }
    }
    anyhow::ensure!(
        output.status == 0,
        "{} is not a Git repository with a readable checkout: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout).context("decode project Git roots")?;
    let mut roots = text.lines();
    let checkout_root = PathBuf::from(roots.next().context("Git omitted the checkout root")?);
    anyhow::ensure!(
        checkout_root.is_absolute(),
        "Git returned a non-absolute checkout root: {}",
        checkout_root.display()
    );
    let common = crate::local_git::resolve_git_path(
        &directory,
        roots
            .next()
            .context("Git omitted the shared repository directory")?,
    )?;
    anyhow::ensure!(
        roots.next().is_none(),
        "Git returned unexpected project roots: {text:?}"
    );
    let root = if common.file_name() == Some(std::ffi::OsStr::new(".git")) {
        common
            .parent()
            .context("Git common directory has no parent")?
            .to_path_buf()
    } else {
        checkout_root.clone()
    };
    let repository_root = if host.is_none() {
        std::fs::canonicalize(&root)?
    } else {
        root
    };
    let identity = match crate::local_git::identity_fetch_url(path, executor)? {
        Some(url) => RepositoryIdentity::from_remote(&url).with_context(|| {
            format!("invalid repository identity remote for {}", path.display())
        })?,
        None => match host {
            Some(host) => RepositoryIdentity::RemoteDirectory {
                host: host.to_owned(),
                root: repository_root.clone(),
            },
            None => RepositoryIdentity::Local(repository_root.clone()),
        },
    };
    Ok(ResolvedDirectory {
        checkout_root,
        repository_root,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_local_history_is_distinct_from_git_failures() {
        use crate::targets::{CommandOutput, ProcessExecutor};

        struct RefusedGit;
        impl CommandExecutor for RefusedGit {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                assert_eq!(command.env.get("LC_ALL").map(String::as_str), Some("C"));
                Ok(CommandOutput {
                    status: 128,
                    stdout: Vec::new(),
                    stderr: b"fatal: detected dubious ownership in repository".to_vec(),
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let missing = resolve_directory(&directory.path().join("missing"), None, &ProcessExecutor)
            .unwrap_err();
        assert!(matches!(
            missing.downcast_ref::<RepositoryUnavailable>(),
            Some(RepositoryUnavailable::Directory(_))
        ));
        let not_repository =
            resolve_directory(directory.path(), None, &ProcessExecutor).unwrap_err();
        assert!(matches!(
            not_repository.downcast_ref::<RepositoryUnavailable>(),
            Some(RepositoryUnavailable::NotRepository(_))
        ));
        let refused = resolve_directory(directory.path(), None, &RefusedGit).unwrap_err();
        assert!(refused.downcast_ref::<RepositoryUnavailable>().is_none());
        assert!(refused.to_string().contains("dubious ownership"));
    }

    #[test]
    fn unavailable_remote_history_preserves_transport_and_access_failures() {
        struct FailedGit {
            status: i32,
            diagnostic: &'static str,
        }
        impl CommandExecutor for FailedGit {
            fn execute(&self, command: &CommandSpec) -> Result<crate::targets::CommandOutput> {
                assert_eq!(command.env.get("LC_ALL").map(String::as_str), Some("C"));
                Ok(crate::targets::CommandOutput {
                    status: self.status,
                    stdout: Vec::new(),
                    stderr: self.diagnostic.as_bytes().to_vec(),
                })
            }
        }
        for (status, diagnostic, unavailable) in [
            (
                128,
                "fatal: cannot change to '/projects/deleted': No such file or directory",
                true,
            ),
            (
                128,
                "fatal: cannot change to '/projects/file/child': Not a directory",
                true,
            ),
            (
                128,
                "fatal: not a git repository (or any of the parent directories): .git",
                true,
            ),
            (
                128,
                "fatal: cannot change to '/projects/private': Permission denied",
                false,
            ),
            (
                128,
                "fatal: detected dubious ownership in repository",
                false,
            ),
            (
                255,
                "ssh: connect to host remote port 22: No route to host",
                false,
            ),
            (
                255,
                "fatal: cannot change to '/projects/deleted': No such file or directory",
                false,
            ),
        ] {
            let error = resolve_directory(
                Path::new("/projects/checkout"),
                Some("remote"),
                &FailedGit { status, diagnostic },
            )
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<RepositoryUnavailable>().is_some(),
                unavailable,
                "{status}: {diagnostic}"
            );
        }
    }

    struct OldGit {
        roots: &'static str,
    }

    impl CommandExecutor for OldGit {
        fn execute(&self, command: &CommandSpec) -> Result<crate::targets::CommandOutput> {
            let stdout = if command.args.iter().any(|argument| argument == "rev-parse") {
                // Git 2.25 prints unsupported options as results and exits zero.
                let echoed = if command
                    .args
                    .iter()
                    .any(|argument| argument == "--path-format=absolute")
                {
                    "--path-format=absolute\n"
                } else {
                    ""
                };
                format!("{echoed}{}", self.roots)
            } else {
                String::new()
            };
            Ok(crate::targets::CommandOutput {
                status: 0,
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn remote_directory_resolution_supports_old_git_and_linked_checkouts() {
        for (selected, output, checkout, repository) in [
            (
                "/projects/app/subdir",
                "/projects/app\n../.git\n",
                "/projects/app",
                "/projects/app",
            ),
            (
                "/worktrees/app-side",
                "/worktrees/app-side\n/projects/app/.git\n",
                "/worktrees/app-side",
                "/projects/app",
            ),
        ] {
            let resolved = resolve_directory(
                Path::new(selected),
                Some("old-git-host"),
                &OldGit { roots: output },
            )
            .unwrap();
            assert_eq!(resolved.checkout_root, Path::new(checkout));
            assert_eq!(resolved.repository_root, Path::new(repository));
            assert_eq!(
                resolved.identity,
                RepositoryIdentity::RemoteDirectory {
                    host: "old-git-host".into(),
                    root: repository.into()
                }
            );
        }
    }

    #[test]
    fn successful_git_exit_does_not_accept_invalid_project_roots() {
        for roots in [
            "--path-format=absolute\n/projects/app\n/projects/app/.git\n",
            "/projects/app\n--path-format=absolute\n",
            "/projects/app\n.git\nextra\n",
            "/projects/app\n",
            "\n.git\n",
        ] {
            assert!(
                resolve_directory(Path::new("/projects/app"), Some("host"), &OldGit { roots })
                    .is_err(),
                "accepted {roots:?}"
            );
        }
    }

    #[test]
    fn network_identity_keeps_case_sensitive_paths_and_removes_credentials() {
        let first = RepositoryIdentity::from_remote("https://user:secret@example.com/Team/App.git")
            .unwrap();
        assert_eq!(first.key(), "git:https://example.com/Team/App");
        assert_ne!(
            first,
            RepositoryIdentity::from_remote("https://example.com/team/app.git").unwrap()
        );
        assert!(RepositoryIdentity::from_remote("../local").is_none());
    }
}
