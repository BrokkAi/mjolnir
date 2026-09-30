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

/// The executor can run on the controller or translate Git commands to SSH.
/// Preserve the selected checkout while collapsing its identity to the owner
/// of the common Git directory.
pub fn resolve_directory(
    path: &Path,
    host: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<ResolvedDirectory> {
    let output = executor.execute(
        &CommandSpec::new(
            "git",
            [
                "-C".to_owned(),
                path.to_string_lossy().into_owned(),
                "rev-parse".into(),
                "--path-format=absolute".into(),
                "--show-toplevel".into(),
                "--git-common-dir".into(),
            ],
        )
        .purpose("resolve project repository and checkout"),
    )?;
    anyhow::ensure!(
        output.status == 0,
        "{} is not a Git repository with a readable checkout: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout).context("decode project Git roots")?;
    let mut roots = text.lines();
    let checkout_root = PathBuf::from(roots.next().context("Git omitted the checkout root")?);
    let common = PathBuf::from(
        roots
            .next()
            .context("Git omitted the shared repository directory")?,
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
    fn github_url_forms_share_identity_without_credentials() {
        let expected = Some(RepositoryIdentity::Github("acme".into(), "app".into()));
        for source in [
            "Acme/App",
            "https://github.com/Acme/App.git",
            "git@github.com:Acme/App.git",
            "ssh://git@github.com/Acme/App.git",
        ] {
            assert_eq!(RepositoryIdentity::from_remote(source), expected);
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
    #[test]
    fn project_reuse_preserves_primary_and_destination_shape_but_memory_unifies_a_repository() {
        use crate::config::{ProjectBundle, ProjectRepository};
        let repository = |id: &str, source: &str| ProjectRepository {
            id: id.into(),
            github: Some(source.into()),
            local: None,
            destination: id.into(),
            git_ref: None,
        };
        let mut project = ProjectBundleSnapshot {
            bundle: ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![repository("app", "acme/app")],
            },
            identities: BTreeMap::from([(
                "app".into(),
                RepositoryIdentity::Github("acme".into(), "app".into()),
            )]),
            network_sources: BTreeMap::new(),
        };
        let single = project.clone();
        project.bundle.repositories[0].destination = "custom/app".into();
        assert_ne!(single.key().unwrap(), project.key().unwrap());
        assert_eq!(
            single.memory_identity().unwrap(),
            project.memory_identity().unwrap()
        );
        project
            .bundle
            .repositories
            .push(repository("shared", "acme/shared"));
        project.identities.insert(
            "shared".into(),
            RepositoryIdentity::Github("acme".into(), "shared".into()),
        );
        assert_ne!(single.source_key().unwrap(), project.source_key().unwrap());
        let first = project.key().unwrap();
        project.bundle.repositories.reverse();
        assert_eq!(first, project.key().unwrap());
        project.bundle.primary_repo = "shared".into();
        assert_ne!(first, project.key().unwrap());
    }
}
