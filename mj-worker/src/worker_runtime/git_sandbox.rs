//! Keep Git usable inside the harness sandboxes of Codex and Claude Code.
//!
//! Both sandboxes treat a repository's Git directory as read-only (Codex) or
//! outside the writable roots (Claude Code, for a linked worktree whose
//! administrative directory and common directory live in the primary clone).
//! Each harness gets the same answer here: the worker resolves the Git
//! directories on the machine that runs the harness, then grants them writable
//! while keeping hooks and configuration read-only, because those run or steer
//! code outside the sandbox. Only session-private staged configuration is
//! edited.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mj_core::config::ExecutionPolicy;

/// The name of the Codex permission profile Mjolnir stages.
pub(super) const CODEX_PROFILE: &str = "mjolnir_workspace";

/// Mjolnir-staged `sandbox.filesystem` entries in the Claude home, so a reload
/// or a changed workspace removes exactly what an earlier launch added.
const CLAUDE_GRANTS_FILE: &str = "mj-sandbox-git-grants.json";

/// What a harness sandbox profile is built from.
pub(super) struct HarnessSandbox {
    /// Every directory the ACP session may write, tool caches included.
    pub directories: Vec<PathBuf>,
    /// The session's working directory and workspace roots; the Git
    /// directories enclosing them are the ones granted.
    pub git_roots: Vec<PathBuf>,
}

/// The Git directories of one working tree.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct GitSandboxTarget {
    /// The directory holding this working tree's `HEAD` and index: `.git`
    /// itself for a clone, `<common>/worktrees/<name>` for a linked worktree.
    pub git_dir: PathBuf,
    /// The directory holding objects, refs, hooks and configuration.
    pub common_dir: PathBuf,
    /// `.git` is a file pointing elsewhere (a linked worktree or submodule),
    /// so the Git directories are outside the working tree.
    pub is_worktree: bool,
}

impl GitSandboxTarget {
    /// Paths that must stay read-only: hooks and configuration run or steer
    /// code outside the sandbox.
    pub(super) fn protected_paths(&self) -> Vec<PathBuf> {
        vec![
            self.common_dir.join("hooks"),
            self.common_dir.join("config"),
            self.common_dir.join("config.lock"),
            self.git_dir.join("config.worktree"),
            self.git_dir.join("commondir"),
        ]
    }

    /// The read-only entries of a Codex profile. Hooks, configuration and a
    /// worktree's `config.worktree` are listed even when absent, so the
    /// sandbox cannot create one that later runs outside it. `commondir`
    /// (read by Git, so an empty mount breaks it) and `config.lock` are listed
    /// only when present.
    fn codex_read_only_paths(&self) -> Vec<PathBuf> {
        let config_worktree = self.git_dir.join("config.worktree");
        let mut paths = vec![
            self.common_dir.join("hooks"),
            self.common_dir.join("config"),
        ];
        if self.is_worktree || config_worktree.exists() {
            paths.push(config_worktree);
        }
        paths.extend(
            [
                self.common_dir.join("config.lock"),
                self.git_dir.join("commondir"),
            ]
            .into_iter()
            .filter(|path| path.exists()),
        );
        paths
    }
}

/// Resolve the Git directories enclosing `roots`. A root that does not exist,
/// lies in no repository, or names Git directories the workspace has no claim
/// to contributes nothing.
///
/// A workspace controls its own `.git` file and the `commondir` file of a
/// Git directory inside it, so neither is trusted to point anywhere. Every
/// target must be a real Git directory; one outside `roots` must also be the
/// administrative directory its own repository made for this working tree
/// (`<common>/worktrees/<name>`, whose `gitdir` file points back at the
/// `.git` file that names it).
pub(super) fn resolve_git_sandbox_targets(roots: &[PathBuf]) -> Vec<GitSandboxTarget> {
    let approved = roots.iter().map(|root| canonical(root)).collect::<Vec<_>>();
    let mut targets = BTreeSet::new();
    for root in roots {
        if !root.is_absolute() || !root.is_dir() {
            continue;
        }
        let Some((dotgit, base)) = root.ancestors().find_map(|ancestor| {
            let dotgit = ancestor.join(".git");
            std::fs::metadata(&dotgit)
                .is_ok()
                .then(|| (dotgit, ancestor.to_owned()))
        }) else {
            continue;
        };
        match resolve_dotgit(&dotgit, &base, &approved) {
            Ok(Some(target)) => {
                targets.insert(target);
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                path = %dotgit.display(),
                error = format!("{error:#}"),
                "skipping a Git directory the harness sandbox cannot be told about"
            ),
        }
    }
    targets.into_iter().collect()
}

fn is_git_directory(directory: &Path) -> bool {
    directory.join("HEAD").is_file()
}

fn is_common_directory(directory: &Path) -> bool {
    is_git_directory(directory)
        && directory.join("objects").is_dir()
        && directory.join("refs").is_dir()
}

fn resolve_dotgit(
    dotgit: &Path,
    base: &Path,
    approved: &[PathBuf],
) -> Result<Option<GitSandboxTarget>> {
    let metadata = std::fs::metadata(dotgit)?;
    if metadata.is_dir() {
        let git_dir = canonical(dotgit);
        ensure!(
            is_common_directory(&git_dir),
            "{} is not a Git directory",
            dotgit.display()
        );
        // A `.git` symlink is the workspace's to plant, so it may lead only to
        // a Git directory inside the approved roots; a real directory found
        // above a subdirectory session is the repository itself.
        ensure!(
            !std::fs::symlink_metadata(dotgit)?.file_type().is_symlink()
                || approved.iter().any(|root| git_dir.starts_with(root)),
            "{} is a symlink to a Git directory outside the workspace",
            dotgit.display()
        );
        return Ok(Some(GitSandboxTarget {
            common_dir: git_dir.clone(),
            git_dir,
            is_worktree: false,
        }));
    }
    // A gitfile is never a writable directory: Codex's sandbox cannot mount
    // over a file, so only the directory it names is granted.
    let text = std::fs::read_to_string(dotgit)
        .with_context(|| format!("read gitfile {}", dotgit.display()))?;
    let Some(pointer) = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|pointer| !pointer.is_empty())
    else {
        bail!("gitfile {} has no gitdir line", dotgit.display());
    };
    let git_dir = base.join(pointer);
    let Ok(git_dir) = std::fs::canonicalize(&git_dir) else {
        return Ok(None);
    };
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            let Ok(common) = std::fs::canonicalize(git_dir.join(text.trim())) else {
                bail!("commondir of {} names no directory", git_dir.display());
            };
            common
        }
        Err(_) => git_dir.clone(),
    };
    ensure!(
        is_git_directory(&git_dir) && is_common_directory(&common_dir),
        "{} does not lead to a Git directory",
        dotgit.display()
    );
    let inside = |directory: &Path| approved.iter().any(|root| directory.starts_with(root));
    if !(inside(&git_dir) && inside(&common_dir)) {
        let administrative = git_dir.parent() == Some(common_dir.join("worktrees").as_path());
        let backlink = std::fs::read_to_string(git_dir.join("gitdir"))
            .ok()
            .and_then(|text| std::fs::canonicalize(git_dir.join(text.trim())).ok());
        ensure!(
            administrative && backlink.is_some() && backlink == std::fs::canonicalize(dotgit).ok(),
            "{} points outside the workspace at a Git directory that does not belong to it",
            dotgit.display()
        );
    }
    Ok(Some(GitSandboxTarget {
        git_dir,
        common_dir,
        is_worktree: true,
    }))
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Stage the Codex permission profile in the session's `config.toml`.
///
/// Under configured approvals, Codex runs `default_permissions =
/// "mjolnir_workspace"`: the built-in `:workspace` profile plus network
/// access (Codex's restricted-network seccomp filter also refuses
/// `shutdown(2)` on a Unix socket, which breaks Node's `spawnSync`), a write
/// grant for every Git directory the session works in, and for every
/// directory the ACP session was given (a custom profile ignores
/// `sandbox_workspace_write.writable_roots`). A permission choice the person
/// made, a profile other than this one (`:read-only`, a custom one) or
/// `sandbox_mode = "read-only"`, is left alone. `danger-full-access`, as a
/// sandbox mode or as `default_permissions = ":danger-full-access"`, is not
/// such a choice: codex-acp clamps it to `workspace-write` in a configured
/// session, so this profile replaces it. Any other execution policy removes
/// what an earlier launch staged. Returns whether `config` changed.
pub(super) fn configure_codex_permissions(
    config: &mut toml::Table,
    path: &Path,
    policy: ExecutionPolicy,
    sandbox: &HarnessSandbox,
) -> Result<bool> {
    let before = config.clone();
    let owned = config
        .get("default_permissions")
        .and_then(toml::Value::as_str)
        == Some(CODEX_PROFILE);
    // codex-acp clamps `danger-full-access` (a sandbox mode or the
    // `:danger-full-access` profile) down to `workspace-write` in a configured
    // session, so those choices never took effect and give way to this profile.
    let person_chose = config.get("default_permissions").is_some_and(|value| {
        ![Some(CODEX_PROFILE), Some(":danger-full-access")].contains(&value.as_str())
    }) || config
        .get("sandbox_mode")
        .is_some_and(|mode| mode.as_str() == Some("read-only"));
    if policy != ExecutionPolicy::ConfiguredApprovals || person_chose {
        if owned {
            remove_codex_profile(config);
        }
        return Ok(*config != before);
    }

    let mut grants = std::collections::BTreeMap::<String, &str>::new();
    let inherited = config
        .get("sandbox_workspace_write")
        .and_then(|table| table.get("writable_roots"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let targets = resolve_git_sandbox_targets(&sandbox.git_roots);
    let writable = sandbox.directories.iter().chain(&inherited).cloned().chain(
        targets
            .iter()
            .flat_map(|target| [target.common_dir.clone(), target.git_dir.clone()]),
    );
    for directory in writable.filter(|directory| directory.is_absolute()) {
        grants.insert(directory.to_string_lossy().into_owned(), "write");
    }
    for protected in targets
        .iter()
        .flat_map(GitSandboxTarget::codex_read_only_paths)
    {
        grants.insert(protected.to_string_lossy().into_owned(), "read");
    }
    // Codex's `workspace-write` sandbox has no network unless the person
    // asked for it; keeping their explicit `network_access = false` is theirs.
    let network = config
        .get("sandbox_workspace_write")
        .and_then(|table| table.get("network_access"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let filesystem = grants
        .into_iter()
        .map(|(path, access)| (path, toml::Value::String(access.into())))
        .collect::<toml::Table>();
    let profile = toml::Table::from_iter([
        (
            "extends".to_owned(),
            toml::Value::String(":workspace".into()),
        ),
        (
            "network".to_owned(),
            toml::Value::Table(toml::Table::from_iter([(
                "enabled".to_owned(),
                toml::Value::Boolean(network),
            )])),
        ),
        ("filesystem".to_owned(), toml::Value::Table(filesystem)),
    ]);
    config.insert(
        "default_permissions".into(),
        toml::Value::String(CODEX_PROFILE.into()),
    );
    config
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .with_context(|| format!("permissions in {} must be a table", path.display()))?
        .insert(CODEX_PROFILE.into(), toml::Value::Table(profile));
    Ok(*config != before)
}

fn remove_codex_profile(config: &mut toml::Table) {
    config.remove("default_permissions");
    if let Some(permissions) = config
        .get_mut("permissions")
        .and_then(toml::Value::as_table_mut)
    {
        permissions.remove(CODEX_PROFILE);
        if permissions.is_empty() {
            config.remove("permissions");
        }
    }
}

/// The `sandbox.filesystem` entries Mjolnir owns in Claude's staged settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ClaudeGrants {
    #[serde(default, rename = "allowWrite")]
    allow_write: BTreeSet<String>,
    #[serde(default, rename = "denyWrite")]
    deny_write: BTreeSet<String>,
}

impl ClaudeGrants {
    fn is_empty(&self) -> bool {
        self.allow_write.is_empty() && self.deny_write.is_empty()
    }

    fn union(&self, other: &Self) -> Self {
        Self {
            allow_write: self
                .allow_write
                .union(&other.allow_write)
                .cloned()
                .collect(),
            deny_write: self.deny_write.union(&other.deny_write).cloned().collect(),
        }
    }
}

/// Let Claude Code's own sandbox write a linked worktree's Git directories.
///
/// Claude's sandbox covers the working directory, so a linked worktree's
/// administrative directory and the primary clone's `.git` are read-only
/// whenever the person enables the sandbox. `sandbox.enabled` is the
/// person's choice and is never touched; this adds the common directory to
/// `sandbox.filesystem.allowWrite` and puts hooks and configuration back in
/// `denyWrite` (a directory granted from outside the working tree gets no
/// such protection). Entries are recorded in a sidecar file beside the
/// settings, written before them, so a reload removes what no longer applies
/// and never an entry the person wrote. Other execution policies remove
/// everything an earlier launch added.
pub(super) fn configure_claude_git_sandbox(
    root: &Path,
    session_id: &str,
    home: &Path,
    policy: ExecutionPolicy,
    sandbox: &HarnessSandbox,
) -> Result<()> {
    let settings_path = home.join("settings.json");
    let grants_path = home.join(CLAUDE_GRANTS_FILE);
    if !(super::subagents::private_staged_home(root, session_id, home)
        && super::subagents::regular_file_or_absent(&settings_path)
        && super::subagents::regular_file_or_absent(&grants_path))
    {
        return Ok(());
    }
    let previous = match std::fs::read(&grants_path) {
        Ok(body) => serde_json::from_slice::<ClaudeGrants>(&body).unwrap_or_else(|error| {
            tracing::warn!(path = %grants_path.display(), %error, "ignoring unreadable sandbox grant record");
            ClaudeGrants::default()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ClaudeGrants::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", grants_path.display()));
        }
    };
    let mut desired = ClaudeGrants::default();
    if policy == ExecutionPolicy::ConfiguredApprovals {
        for target in resolve_git_sandbox_targets(&sandbox.git_roots) {
            if target.is_worktree {
                desired
                    .allow_write
                    .insert(target.common_dir.to_string_lossy().into_owned());
            }
            desired.deny_write.extend(
                target
                    .protected_paths()
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
    }
    if previous.is_empty() && desired.is_empty() {
        return Ok(());
    }

    let mut settings: serde_json::Value = match std::fs::read(&settings_path) {
        Ok(body) => serde_json::from_slice(&body)
            .with_context(|| format!("parse staged Claude settings {}", settings_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", settings_path.display()));
        }
    };
    let original = settings.clone();
    let owned = plan_claude_grants(
        settings
            .as_object_mut()
            .with_context(|| format!("{} must be a JSON object", settings_path.display()))?,
        &previous,
        &desired,
    )
    .with_context(|| format!("edit staged Claude settings {}", settings_path.display()))?;

    write_claude_grants(&grants_path, &previous.union(&owned))?;
    if settings != original {
        let mut body = serde_json::to_vec_pretty(&settings)?;
        body.push(b'\n');
        mj_core::config::atomic_write(&settings_path, &body)
            .with_context(|| format!("write staged Claude settings {}", settings_path.display()))?;
    }
    write_claude_grants(&grants_path, &owned)
}

fn write_claude_grants(path: &Path, grants: &ClaudeGrants) -> Result<()> {
    if grants.is_empty() {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("remove {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let mut body = serde_json::to_vec_pretty(grants)?;
    body.push(b'\n');
    mj_core::config::atomic_write(path, &body)
        .with_context(|| format!("write sandbox grant record {}", path.display()))
}

/// Bring `sandbox.filesystem` to `desired`, removing `previous` entries that no
/// longer apply. Returns the entries Mjolnir now owns; an entry that was
/// already present and not in `previous` is the person's own and is neither
/// recorded nor ever removed.
fn plan_claude_grants(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    previous: &ClaudeGrants,
    desired: &ClaudeGrants,
) -> Result<ClaudeGrants> {
    let mut owned = ClaudeGrants::default();
    let had_sandbox = settings
        .get("sandbox")
        .and_then(serde_json::Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    let had_filesystem = settings
        .get("sandbox")
        .and_then(|sandbox| sandbox.get("filesystem"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    for (key, previous, desired, owned) in [
        (
            "allowWrite",
            &previous.allow_write,
            &desired.allow_write,
            &mut owned.allow_write,
        ),
        (
            "denyWrite",
            &previous.deny_write,
            &desired.deny_write,
            &mut owned.deny_write,
        ),
    ] {
        if previous.is_empty() && desired.is_empty() {
            continue;
        }
        let filesystem = settings
            .entry("sandbox")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("sandbox must be a JSON object")?
            .entry("filesystem")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("sandbox.filesystem must be a JSON object")?;
        let list = filesystem
            .entry(key)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .with_context(|| format!("sandbox.filesystem.{key} must be a JSON array"))?;
        list.retain(|entry| {
            entry
                .as_str()
                .is_none_or(|entry| !previous.contains(entry) || desired.contains(entry))
        });
        for entry in desired {
            let present = list.iter().any(|existing| existing.as_str() == Some(entry));
            if !present {
                list.push(entry.clone().into());
            }
            if !present || previous.contains(entry) {
                owned.insert(entry.clone());
            }
        }
        if list.is_empty() {
            filesystem.remove(key);
        }
    }
    prune_empty_sandbox(settings, had_sandbox, had_filesystem);
    Ok(owned)
}

/// Drop the `sandbox.filesystem` and `sandbox` objects this edit created or
/// emptied, leaving objects the person left empty as they were.
fn prune_empty_sandbox(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    kept_sandbox: bool,
    kept_filesystem: bool,
) {
    let Some(sandbox) = settings
        .get_mut("sandbox")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if sandbox
        .get("filesystem")
        .and_then(serde_json::Value::as_object)
        .is_some_and(serde_json::Map::is_empty)
        && !kept_filesystem
    {
        sandbox.remove("filesystem");
    }
    if sandbox.is_empty() && !kept_sandbox {
        settings.remove("sandbox");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::git;

    struct Repositories {
        _directory: tempfile::TempDir,
        base: PathBuf,
        clone: PathBuf,
        worktree: PathBuf,
    }

    /// A clone with a subdirectory and a linked worktree, built by real Git.
    fn repositories() -> Repositories {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let clone = base.join("clone");
        std::fs::create_dir_all(clone.join("crates/foo")).unwrap();
        git(&clone, &["init", "-q", "-b", "main"]);
        git(&clone, &["config", "user.name", "Sandbox Test"]);
        git(&clone, &["config", "user.email", "sandbox@example.invalid"]);
        std::fs::write(clone.join("crates/foo/lib.rs"), "\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "base"]);
        let worktree = base.join("linked");
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-qb",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        Repositories {
            _directory: directory,
            base,
            clone,
            worktree,
        }
    }

    fn sandbox(git_roots: Vec<PathBuf>, directories: Vec<PathBuf>) -> HarnessSandbox {
        HarnessSandbox {
            directories,
            git_roots,
        }
    }

    #[test]
    fn git_directories_resolve_from_a_clone_a_subdirectory_and_a_linked_worktree() {
        let repositories = repositories();
        let plain = repositories.base.join("plain");
        std::fs::create_dir(&plain).unwrap();
        let git_dir = repositories.clone.join(".git");
        let worktree_dir = git_dir.join("worktrees/linked");

        let mut targets = resolve_git_sandbox_targets(&[
            repositories.clone.join("crates/foo"),
            repositories.clone.clone(),
            repositories.worktree.clone(),
            repositories.worktree.join("crates"),
            plain,
            repositories.base.join("missing"),
            PathBuf::from("relative"),
        ]);
        // The scratch directory may itself sit inside a repository; the
        // plain directory resolves to that one, which is not under test.
        targets.retain(|target| target.git_dir.starts_with(&repositories.clone));

        assert_eq!(
            targets,
            [
                GitSandboxTarget {
                    git_dir: git_dir.clone(),
                    common_dir: git_dir.clone(),
                    is_worktree: false,
                },
                GitSandboxTarget {
                    git_dir: worktree_dir,
                    common_dir: git_dir,
                    is_worktree: true,
                },
            ]
        );
    }

    #[test]
    fn codex_profile_grants_git_directories_and_keeps_hooks_and_config_read_only() {
        let repositories = repositories();
        let git_dir = repositories.clone.join(".git");
        let worktree_dir = git_dir.join("worktrees/linked");
        let memory = repositories.base.join("memory");
        let inherited = repositories.base.join("inherited");
        let mut config: toml::Table = toml::from_str(&format!(
            "[sandbox_workspace_write]\nwritable_roots = [{:?}, \"relative\"]\n",
            inherited.to_string_lossy()
        ))
        .unwrap();
        let sandbox = sandbox(
            vec![repositories.worktree.clone()],
            vec![repositories.clone.clone(), memory.clone()],
        );

        assert!(
            configure_codex_permissions(
                &mut config,
                Path::new("config.toml"),
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox,
            )
            .unwrap()
        );
        assert!(
            !configure_codex_permissions(
                &mut config,
                Path::new("config.toml"),
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox,
            )
            .unwrap(),
            "staging the same session twice is a no-op"
        );

        assert_eq!(config["default_permissions"].as_str(), Some(CODEX_PROFILE));
        let profile = config["permissions"][CODEX_PROFILE].as_table().unwrap();
        assert_eq!(profile["extends"].as_str(), Some(":workspace"));
        assert_eq!(profile["network"]["enabled"].as_bool(), Some(true));
        let filesystem = profile["filesystem"]
            .as_table()
            .unwrap()
            .iter()
            .map(|(path, access)| (path.as_str(), access.as_str().unwrap()))
            .collect::<std::collections::BTreeMap<_, _>>();
        let path = |path: &Path| path.to_str().unwrap().to_owned();
        let mut expected = std::collections::BTreeMap::new();
        for writable in [
            &repositories.clone,
            &memory,
            &inherited,
            &git_dir,
            &worktree_dir,
        ] {
            expected.insert(path(writable), "write");
        }
        // A clone and a worktree both have hooks and configuration to protect,
        // and a worktree its (here absent) `config.worktree`. Other missing
        // paths (`config.lock`) are not listed: Codex mounts an empty file
        // there, and an empty `commondir` breaks Git.
        for protected in [
            git_dir.join("hooks"),
            git_dir.join("config"),
            worktree_dir.join("config.worktree"),
            worktree_dir.join("commondir"),
        ] {
            expected.insert(path(&protected), "read");
        }
        assert_eq!(
            filesystem,
            expected
                .iter()
                .map(|(path, access)| (path.as_str(), *access))
                .collect()
        );
        // A gitfile is not a directory Codex can mount over.
        assert!(!filesystem.contains_key(path(&repositories.worktree.join(".git")).as_str()));
    }

    #[test]
    fn codex_profile_protects_hooks_and_worktree_config_that_do_not_exist_yet() {
        let repositories = repositories();
        let git_dir = repositories.clone.join(".git");
        let worktree_dir = git_dir.join("worktrees/linked");
        std::fs::remove_dir_all(git_dir.join("hooks")).unwrap();
        let profile = |roots: Vec<PathBuf>| {
            let mut config = toml::Table::new();
            configure_codex_permissions(
                &mut config,
                Path::new("config.toml"),
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox(roots, Vec::new()),
            )
            .unwrap();
            config["permissions"][CODEX_PROFILE]["filesystem"]
                .as_table()
                .unwrap()
                .iter()
                .filter(|(_, access)| access.as_str() == Some("read"))
                .map(|(path, _)| path.clone())
                .collect::<BTreeSet<_>>()
        };
        let path = |path: PathBuf| path.to_str().unwrap().to_owned();

        // Without them the sandbox could create a hook or a worktree config
        // that Git later runs outside it.
        assert_eq!(
            profile(vec![repositories.worktree.clone()]),
            BTreeSet::from([
                path(git_dir.join("hooks")),
                path(git_dir.join("config")),
                path(worktree_dir.join("config.worktree")),
                path(worktree_dir.join("commondir")),
            ])
        );
        // A clone has no `commondir`, and an empty one breaks Git; its
        // optional files are listed only when they exist.
        assert_eq!(
            profile(vec![repositories.clone.clone()]),
            BTreeSet::from([path(git_dir.join("hooks")), path(git_dir.join("config"))])
        );
    }

    #[test]
    fn codex_profile_keeps_a_persons_network_refusal() {
        let network = |sandbox_workspace_write: &str| {
            let mut config: toml::Table = toml::from_str(sandbox_workspace_write).unwrap();
            configure_codex_permissions(
                &mut config,
                Path::new("config.toml"),
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox(Vec::new(), Vec::new()),
            )
            .unwrap();
            config["permissions"][CODEX_PROFILE]["network"]["enabled"]
                .as_bool()
                .unwrap()
        };
        assert!(network(""));
        assert!(network(
            "[sandbox_workspace_write]\nnetwork_access = true\n"
        ));
        assert!(!network(
            "[sandbox_workspace_write]\nnetwork_access = false\n"
        ));
    }

    #[test]
    fn a_workspace_cannot_point_the_sandbox_at_git_directories_that_are_not_its_own() {
        let repositories = repositories();
        let admin = repositories.clone.join(".git/worktrees/linked");
        let other = repositories.base.join("other");
        git(
            &repositories.clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "other",
                other.to_str().unwrap(),
            ],
        );
        let workspace = repositories.base.join("workspace");
        let plant = |name: &str, gitfile: String| {
            let root = workspace.join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(".git"), gitfile).unwrap();
            root
        };
        let resolve = |root: &Path| resolve_git_sandbox_targets(&[root.to_owned()]);
        let pointing_at = |directory: &Path| format!("gitdir: {}\n", directory.display());

        // A directory that is no repository (an `.ssh`).
        let ssh = repositories.base.join("victim/.ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        assert!(resolve(&plant("ssh", pointing_at(&ssh))).is_empty());

        // An unrelated real repository, named directly or by one of its
        // worktrees' administrative directories that points back elsewhere.
        assert!(
            resolve(&plant(
                "direct",
                pointing_at(&repositories.clone.join(".git"))
            ))
            .is_empty()
        );
        assert!(
            resolve(&plant(
                "stolen",
                pointing_at(&repositories.clone.join(".git/worktrees/other"))
            ))
            .is_empty()
        );

        // A fake Git directory inside the workspace whose `commondir` leads
        // to a real repository outside it.
        let forged = plant("forged", "gitdir: fake\n".into());
        std::fs::create_dir_all(forged.join("fake/objects")).unwrap();
        std::fs::create_dir_all(forged.join("fake/refs")).unwrap();
        std::fs::write(forged.join("fake/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            forged.join("fake/commondir"),
            repositories.clone.join(".git").to_str().unwrap(),
        )
        .unwrap();
        assert!(resolve(&forged).is_empty());

        // A `.git` symlink into a real repository outside the workspace.
        let linked = workspace.join("linked-dotgit");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(repositories.clone.join(".git"), linked.join(".git")).unwrap();
        assert!(resolve(&linked).is_empty());

        // A real external worktree proves ownership both ways and is
        // accepted; so is one whose directory moved when its repository is
        // in the workspace's approved roots.
        let accepted = resolve(&repositories.worktree);
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].git_dir, admin);
        let moved = repositories.base.join("moved");
        std::fs::rename(&other, &moved).unwrap();
        assert!(resolve(&moved).is_empty(), "the backlink no longer matches");
        let trusted = resolve_git_sandbox_targets(&[moved.clone(), repositories.clone.clone()]);
        assert!(trusted.iter().any(|target| target.is_worktree
            && target.git_dir == repositories.clone.join(".git/worktrees/other")));
    }

    #[test]
    fn codex_profile_yields_to_the_persons_permissions_and_is_removed_when_unconstrained() {
        let repositories = repositories();
        let sandbox = sandbox(vec![repositories.clone.clone()], Vec::new());
        let stage = |config: &mut toml::Table, policy| {
            configure_codex_permissions(config, Path::new("config.toml"), policy, &sandbox).unwrap()
        };

        for person in [
            "default_permissions = \":read-only\"\n",
            "default_permissions = \"team\"\n[permissions.team]\nextends = \":workspace\"\n",
            "default_permissions = \"custom\"\n",
            "sandbox_mode = \"read-only\"\n",
        ] {
            let mut config: toml::Table = toml::from_str(person).unwrap();
            let original = config.clone();
            assert!(!stage(&mut config, ExecutionPolicy::ConfiguredApprovals));
            assert_eq!(config, original, "{person}");
        }

        // codex-acp clamps these to workspace-write, so they give way.
        for clamped in [
            "sandbox_mode = \"danger-full-access\"\n",
            "default_permissions = \":danger-full-access\"\n",
        ] {
            let mut config: toml::Table = toml::from_str(clamped).unwrap();
            assert!(
                stage(&mut config, ExecutionPolicy::ConfiguredApprovals),
                "{clamped}"
            );
            assert_eq!(config["default_permissions"].as_str(), Some(CODEX_PROFILE));
        }

        let mut config: toml::Table =
            toml::from_str("sandbox_mode = \"workspace-write\"\n[tui]\nstatus = true\n").unwrap();
        assert!(stage(&mut config, ExecutionPolicy::ConfiguredApprovals));
        assert!(config.contains_key("permissions"));
        // Moving the session to unconstrained removes only what this staged.
        assert!(stage(&mut config, ExecutionPolicy::Unconstrained));
        assert_eq!(
            config,
            toml::from_str::<toml::Table>(
                "sandbox_mode = \"workspace-write\"\n[tui]\nstatus = true\n"
            )
            .unwrap()
        );
        let mut team: toml::Table = toml::from_str(
            "default_permissions = \"team\"\n[permissions.team]\nextends = \":workspace\"\n",
        )
        .unwrap();
        let original = team.clone();
        assert!(!stage(&mut team, ExecutionPolicy::Unconstrained));
        assert_eq!(team, original);
    }

    fn claude_home(root: &Path) -> PathBuf {
        let home = root.join("profile");
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn claude_settings(home: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap()
    }

    #[test]
    fn claude_worktree_grants_follow_the_workspace_and_never_touch_the_persons_entries() {
        let repositories = repositories();
        let root = repositories.base.join("worker");
        let home = claude_home(&root);
        let git_dir = repositories.clone.join(".git");
        let common = git_dir.to_str().unwrap().to_owned();
        let hooks = git_dir.join("hooks").to_str().unwrap().to_owned();
        let worktree_dir = git_dir.join("worktrees/linked");
        let own_deny = git_dir.join("config").to_str().unwrap().to_owned();
        std::fs::write(
            home.join("settings.json"),
            serde_json::json!({
                "sandbox": {
                    "enabled": true,
                    "filesystem": {
                        "allowWrite": ["/person/cache"],
                        // The person already denies this path themselves.
                        "denyWrite": [own_deny, "/person/secret"],
                    },
                },
            })
            .to_string(),
        )
        .unwrap();
        let stage = |policy, roots: Vec<PathBuf>| {
            configure_claude_git_sandbox(&root, "session", &home, policy, &sandbox(roots, vec![]))
                .unwrap();
        };

        stage(
            ExecutionPolicy::ConfiguredApprovals,
            vec![repositories.worktree.clone()],
        );
        let settings = claude_settings(&home);
        assert_eq!(settings["sandbox"]["enabled"], true);
        let filesystem = &settings["sandbox"]["filesystem"];
        assert_eq!(
            filesystem["allowWrite"],
            serde_json::json!(["/person/cache", common])
        );
        let deny = filesystem["denyWrite"].as_array().unwrap();
        for protected in [
            hooks.as_str(),
            worktree_dir.join("commondir").to_str().unwrap(),
            "/person/secret",
        ] {
            assert!(deny.iter().any(|entry| entry == protected), "{protected}");
        }
        assert_eq!(
            deny.iter().filter(|entry| **entry == own_deny).count(),
            1,
            "an entry the person already had is not duplicated"
        );
        // Staging again changes nothing.
        let before = std::fs::read(home.join("settings.json")).unwrap();
        stage(
            ExecutionPolicy::ConfiguredApprovals,
            vec![repositories.worktree.clone()],
        );
        assert_eq!(std::fs::read(home.join("settings.json")).unwrap(), before);

        // The session moves to a workspace with no repository: what was
        // staged goes, the person's entries (including the shared one) stay.
        stage(ExecutionPolicy::ConfiguredApprovals, Vec::new());
        let settings = claude_settings(&home);
        assert_eq!(
            settings["sandbox"],
            serde_json::json!({
                "enabled": true,
                "filesystem": {
                    "allowWrite": ["/person/cache"],
                    "denyWrite": [own_deny, "/person/secret"],
                },
            })
        );
        assert!(!home.join(CLAUDE_GRANTS_FILE).exists());

        // Unconstrained removes the grants of a launch that had them, and a
        // session that never had any leaves no sandbox keys behind.
        stage(
            ExecutionPolicy::ConfiguredApprovals,
            vec![repositories.worktree.clone()],
        );
        stage(
            ExecutionPolicy::Unconstrained,
            vec![repositories.worktree.clone()],
        );
        assert_eq!(
            claude_settings(&home)["sandbox"]["filesystem"]["allowWrite"],
            serde_json::json!(["/person/cache"])
        );
        assert!(!home.join(CLAUDE_GRANTS_FILE).exists());

        let bare = claude_home(&repositories.base.join("bare-worker"));
        configure_claude_git_sandbox(
            &repositories.base.join("bare-worker"),
            "session",
            &bare,
            ExecutionPolicy::ConfiguredApprovals,
            &sandbox(Vec::new(), vec![]),
        )
        .unwrap();
        assert!(!bare.join("settings.json").exists());
    }
}
