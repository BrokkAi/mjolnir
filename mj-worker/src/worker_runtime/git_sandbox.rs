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
    /// directories enclosing them are the ones granted. A borrowed child's
    /// preserved owner roots are not here: a child in one of its owner's
    /// linked worktrees works in that worktree, not in the owner's clone.
    pub git_roots: Vec<PathBuf>,
    /// Every root the session may write, the owner roots included. Git
    /// directories inside them need no proof of ownership.
    pub approved_roots: Vec<PathBuf>,
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

/// The linked worktrees of `common_dir` other than the session's own.
fn sibling_worktrees(common_dir: &Path, own: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) else {
        return Vec::new();
    };
    let mut siblings = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| canonical(&entry.path()))
        .filter(|path| path.is_dir() && !own.contains(path))
        .collect::<Vec<_>>();
    siblings.sort();
    siblings
}

/// The Git directories a session's working trees share, split by what the
/// session may do to them.
struct GitAccess<'a> {
    targets: &'a [GitSandboxTarget],
    /// The session's own administrative directories.
    own: BTreeSet<PathBuf>,
    /// Common directories of clones, where the session may `git worktree add`.
    clones: BTreeSet<PathBuf>,
}

impl<'a> GitAccess<'a> {
    fn new(targets: &'a [GitSandboxTarget]) -> Self {
        Self {
            targets,
            own: targets
                .iter()
                .map(|target| target.git_dir.clone())
                .collect(),
            clones: targets
                .iter()
                .filter(|target| !target.is_worktree)
                .map(|target| target.common_dir.clone())
                .collect(),
        }
    }

    fn common_dirs(&self) -> BTreeSet<&Path> {
        self.targets
            .iter()
            .map(|target| target.common_dir.as_path())
            .collect()
    }

    /// Codex profile entries, path to `write` or `read`. Hooks, configuration
    /// and every `config.worktree` are listed even when absent, so the sandbox
    /// cannot create one that later runs outside it (`core.hooksPath`).
    /// `commondir` (read by Git, so an empty mount breaks it) and `config.lock`
    /// are listed only when present. A session that only has linked worktrees
    /// of a repository gets the `worktrees` directory read-only with its own
    /// administrative directory writable inside it; a clone's session may add
    /// worktrees, so each existing sibling's configuration and backlinks are
    /// protected instead.
    fn codex(&self) -> std::collections::BTreeMap<PathBuf, &'static str> {
        let mut access = std::collections::BTreeMap::new();
        for target in self.targets {
            access.insert(target.common_dir.clone(), "write");
            access.insert(target.git_dir.clone(), "write");
        }
        let present = |path: PathBuf| path.exists().then_some(path);
        let mut read = Vec::new();
        for common_dir in self.common_dirs() {
            // The common directory's `config.worktree` configures the
            // primary working tree.
            // `modules` holds initialized submodules' hooks and configuration;
            // a submodule the session works in is granted back inside it.
            read.extend([
                common_dir.join("hooks"),
                common_dir.join("config"),
                common_dir.join("config.worktree"),
                common_dir.join("modules"),
            ]);
            read.extend(present(common_dir.join("config.lock")));
            if self.clones.contains(common_dir) {
                for other in sibling_worktrees(common_dir, &self.own) {
                    read.push(other.join("config.worktree"));
                    read.extend(present(other.join("commondir")));
                    read.extend(present(other.join("gitdir")));
                }
            } else if common_dir.join("worktrees").is_dir() {
                read.push(common_dir.join("worktrees"));
            }
        }
        for target in self.targets {
            read.push(target.git_dir.join("config.worktree"));
            if target.is_worktree {
                read.extend(present(target.git_dir.join("commondir")));
            }
        }
        for path in read {
            access.insert(path, "read");
        }
        access
    }

    /// Deny `modules` whole, or, when the session works in a submodule inside
    /// it, everything around that submodule, so the grant of its own
    /// directory cannot reach another's hooks or configuration. A submodule
    /// named with a slash (`libs/foo`) lives in `modules/libs/foo`, so every
    /// directory on the way to the one in use is walked, siblings included.
    fn deny_modules(&self, modules: &Path, deny: &mut BTreeSet<PathBuf>) {
        if !self.own.iter().any(|own| own.starts_with(modules)) {
            deny.insert(modules.to_owned());
            return;
        }
        let Ok(entries) = std::fs::read_dir(modules) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            let is_symlink = entry.file_type().is_ok_and(|kind| kind.is_symlink());
            if is_symlink || !self.own.iter().any(|own| own.starts_with(&path)) {
                deny.insert(path);
            } else if !self.own.contains(&path) {
                // On the way to the submodule in use: deny what is around it,
                // and the configuration this directory may not have yet.
                for name in ["hooks", "config", "config.lock", "config.worktree"] {
                    deny.insert(path.join(name));
                }
                self.deny_modules(&path, deny);
            }
        }
    }

    /// Claude `denyWrite` entries: hooks and configuration everywhere, and
    /// the siblings of a worktree-only session whole (Claude grants the
    /// common directory, so its own children are carved back out).
    fn claude_denials(&self) -> BTreeSet<PathBuf> {
        let mut deny = BTreeSet::new();
        for common_dir in self.common_dirs() {
            for name in ["hooks", "config", "config.lock", "config.worktree"] {
                deny.insert(common_dir.join(name));
            }
            self.deny_modules(&common_dir.join("modules"), &mut deny);
            for other in sibling_worktrees(common_dir, &self.own) {
                if self.clones.contains(common_dir) {
                    for name in ["config.worktree", "commondir", "gitdir"] {
                        deny.insert(other.join(name));
                    }
                } else {
                    deny.insert(other);
                }
            }
        }
        for target in self.targets {
            deny.insert(target.git_dir.join("config.worktree"));
            if target.is_worktree {
                deny.insert(target.git_dir.join("commondir"));
            }
        }
        deny
    }
}

/// Resolve the Git directories enclosing `roots`; those named by a `.git` file
/// outside `approved_roots` need proof of ownership. A root that does not exist,
/// lies in no repository, or names Git directories the workspace has no claim
/// to contributes nothing.
///
/// A workspace controls its own `.git` file and the `commondir` file of a
/// Git directory inside it, so neither is trusted to point anywhere. Every
/// target must be a real Git directory; one outside `roots` must also be the
/// administrative directory its own repository made for this working tree
/// (`<common>/worktrees/<name>`, whose `gitdir` file points back at the
/// `.git` file that names it).
pub(super) fn resolve_git_sandbox_targets(
    roots: &[PathBuf],
    approved_roots: &[PathBuf],
) -> Vec<GitSandboxTarget> {
    let approved = approved_roots
        .iter()
        .map(|root| canonical(root))
        .collect::<Vec<_>>();
    let mut targets = BTreeSet::new();
    for root in roots {
        if !root.is_absolute() || !root.is_dir() {
            continue;
        }
        // A root that is a symlink works where it leads, so walk up from there.
        let physical = canonical(root);
        let Some((dotgit, base)) = physical.ancestors().find_map(|ancestor| {
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

/// Read a Git pointer file (`.git`, `commondir`, `gitdir`). A FIFO or device
/// would block the preparation thread on open, and a huge file is no pointer.
fn read_pointer(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024,
        "{} is not a regular pointer file",
        path.display()
    );
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
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
    let text = read_pointer(dotgit)?;
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
    let common_dir = if std::fs::symlink_metadata(git_dir.join("commondir")).is_ok() {
        let text = read_pointer(&git_dir.join("commondir"))?;
        let Ok(common) = std::fs::canonicalize(git_dir.join(text.trim())) else {
            bail!("commondir of {} names no directory", git_dir.display());
        };
        common
    } else {
        git_dir.clone()
    };
    ensure!(
        is_git_directory(&git_dir) && is_common_directory(&common_dir),
        "{} does not lead to a Git directory",
        dotgit.display()
    );
    let inside = |directory: &Path| approved.iter().any(|root| directory.starts_with(root));
    if !(inside(&git_dir) && inside(&common_dir)) {
        let administrative = git_dir.parent() == Some(common_dir.join("worktrees").as_path());
        let backlink = read_pointer(&git_dir.join("gitdir"))
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
    let targets = resolve_git_sandbox_targets(&sandbox.git_roots, &sandbox.approved_roots);
    for directory in sandbox
        .directories
        .iter()
        .chain(&inherited)
        .filter(|directory| directory.is_absolute())
    {
        grants.insert(directory.to_string_lossy().into_owned(), "write");
    }
    for (path, access) in GitAccess::new(&targets).codex() {
        grants.insert(path.to_string_lossy().into_owned(), access);
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
        let targets = resolve_git_sandbox_targets(&sandbox.git_roots, &sandbox.approved_roots);
        desired.allow_write.extend(
            targets
                .iter()
                .filter(|target| target.is_worktree)
                .map(|target| target.common_dir.to_string_lossy().into_owned()),
        );
        desired.deny_write.extend(
            GitAccess::new(&targets)
                .claude_denials()
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );
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
            approved_roots: git_roots.clone(),
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

        let roots = [
            repositories.clone.join("crates/foo"),
            repositories.clone.clone(),
            repositories.worktree.clone(),
            repositories.worktree.join("crates"),
            plain,
            repositories.base.join("missing"),
            PathBuf::from("relative"),
        ];
        let mut targets = resolve_git_sandbox_targets(&roots, &roots);
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
        // The worktrees directory is read-only around the session's own.
        for protected in [
            git_dir.join("hooks"),
            git_dir.join("config"),
            git_dir.join("config.worktree"),
            git_dir.join("modules"),
            git_dir.join("worktrees"),
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
                path(git_dir.join("config.worktree")),
                path(git_dir.join("modules")),
                path(git_dir.join("worktrees")),
                path(worktree_dir.join("config.worktree")),
                path(worktree_dir.join("commondir")),
            ])
        );
        // A clone has no `commondir` (an empty one breaks Git) but may name a
        // `config.worktree` into being; its existing worktree is protected.
        assert_eq!(
            profile(vec![repositories.clone.clone()]),
            BTreeSet::from([
                path(git_dir.join("hooks")),
                path(git_dir.join("config")),
                path(git_dir.join("config.worktree")),
                path(git_dir.join("modules")),
                path(worktree_dir.join("config.worktree")),
                path(worktree_dir.join("commondir")),
                path(worktree_dir.join("gitdir")),
            ])
        );
    }

    fn sibling_fixture() -> (Repositories, PathBuf) {
        let repositories = repositories();
        let sibling = repositories.base.join("sibling");
        git(
            &repositories.clone,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "sibling",
                sibling.to_str().unwrap(),
            ],
        );
        (repositories, sibling)
    }

    #[test]
    fn a_worktree_session_cannot_reach_its_siblings_in_codex_or_claude() {
        let (repositories, _sibling) = sibling_fixture();
        let git_dir = repositories.clone.join(".git");
        let mine = git_dir.join("worktrees/linked");
        let theirs = git_dir.join("worktrees/sibling");
        let path = |path: &Path| path.to_str().unwrap().to_owned();
        // The worktree alone, and a borrowed child whose owner clone is an
        // approved root but not a place it works in.
        for approved in [
            vec![repositories.worktree.clone()],
            vec![repositories.worktree.clone(), repositories.clone.clone()],
        ] {
            let sandbox = HarnessSandbox {
                directories: Vec::new(),
                git_roots: vec![repositories.worktree.clone()],
                approved_roots: approved,
            };
            let mut config = toml::Table::new();
            configure_codex_permissions(
                &mut config,
                Path::new("config.toml"),
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox,
            )
            .unwrap();
            let filesystem = config["permissions"][CODEX_PROFILE]["filesystem"]
                .as_table()
                .unwrap();
            let access = |p: &Path| filesystem.get(&path(p)).and_then(toml::Value::as_str);
            // Codex lets the child entry beat the read-only directory above.
            assert_eq!(access(&git_dir.join("worktrees")), Some("read"));
            assert_eq!(access(&mine), Some("write"));
            assert_eq!(access(&mine.join("config.worktree")), Some("read"));
            assert_eq!(access(&theirs), None);
            assert_eq!(access(&git_dir.join("config.worktree")), Some("read"));

            let root = repositories.base.join("worker");
            let home = claude_home(&root);
            configure_claude_git_sandbox(
                &root,
                "session",
                &home,
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox,
            )
            .unwrap();
            let settings = claude_settings(&home);
            let deny = settings["sandbox"]["filesystem"]["denyWrite"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry.as_str().unwrap().to_owned())
                .collect::<BTreeSet<_>>();
            for denied in [
                theirs.clone(),
                mine.join("config.worktree"),
                mine.join("commondir"),
                git_dir.join("hooks"),
                git_dir.join("config.worktree"),
            ] {
                assert!(deny.contains(&path(&denied)), "{}", denied.display());
            }
            assert!(!deny.contains(&path(&mine)));
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn a_clone_session_protects_the_configuration_and_backlinks_of_existing_worktrees() {
        let (repositories, _sibling) = sibling_fixture();
        let git_dir = repositories.clone.join(".git");
        let sandbox = sandbox(vec![repositories.clone.clone()], Vec::new());
        let path = |path: PathBuf| path.to_str().unwrap().to_owned();

        let mut config = toml::Table::new();
        configure_codex_permissions(
            &mut config,
            Path::new("config.toml"),
            ExecutionPolicy::ConfiguredApprovals,
            &sandbox,
        )
        .unwrap();
        let filesystem = config["permissions"][CODEX_PROFILE]["filesystem"]
            .as_table()
            .unwrap();
        for name in ["linked", "sibling"] {
            for file in ["config.worktree", "commondir", "gitdir"] {
                let protected = path(git_dir.join("worktrees").join(name).join(file));
                assert_eq!(filesystem[&protected].as_str(), Some("read"), "{protected}");
            }
        }
        // The clone may add worktrees, so their directory stays writable.
        assert!(!filesystem.contains_key(&path(git_dir.join("worktrees"))));

        let root = repositories.base.join("worker");
        let home = claude_home(&root);
        configure_claude_git_sandbox(
            &root,
            "session",
            &home,
            ExecutionPolicy::ConfiguredApprovals,
            &sandbox,
        )
        .unwrap();
        let settings = claude_settings(&home);
        let deny = settings["sandbox"]["filesystem"]["denyWrite"]
            .as_array()
            .unwrap();
        assert!(
            deny.iter()
                .any(|entry| *entry == path(git_dir.join("worktrees/sibling/gitdir")).as_str())
        );
    }

    #[test]
    fn a_pointer_file_that_is_not_a_regular_file_is_skipped_without_blocking() {
        let (repositories, _sibling) = sibling_fixture();
        let workspace = repositories.base.join("workspace");
        let fifo = |path: &Path| {
            let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            // SAFETY: `name` is a terminated path and mkfifo keeps no pointer.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        };
        let resolve = |roots: Vec<PathBuf>| {
            let (done, result) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = done.send(resolve_git_sandbox_targets(&roots, &roots));
            });
            result
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("a FIFO pointer file blocked the resolver")
        };

        // `.git` itself.
        std::fs::create_dir_all(workspace.join("a")).unwrap();
        fifo(&workspace.join("a/.git"));
        assert!(resolve(vec![workspace.join("a")]).is_empty());

        // `commondir` of a Git directory the workspace made.
        let forged = workspace.join("b");
        std::fs::create_dir_all(forged.join("fake/objects")).unwrap();
        std::fs::create_dir_all(forged.join("fake/refs")).unwrap();
        std::fs::write(forged.join("fake/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(forged.join(".git"), "gitdir: fake\n").unwrap();
        fifo(&forged.join("fake/commondir"));
        assert!(resolve(vec![forged]).is_empty());

        // `gitdir` of an external worktree's administrative directory.
        let backlink = repositories.clone.join(".git/worktrees/linked/gitdir");
        std::fs::remove_file(&backlink).unwrap();
        fifo(&backlink);
        assert!(resolve(vec![repositories.worktree.clone()]).is_empty());
    }

    #[test]
    fn initialized_submodules_stay_read_only_unless_the_session_works_in_one() {
        let repositories = repositories();
        let git_dir = repositories.clone.join(".git");
        for name in ["sub", "other", "libs/a", "libs/b"] {
            let source = repositories
                .base
                .join(format!("{}-source", name.replace('/', "-")));
            std::fs::create_dir(&source).unwrap();
            git(&source, &["init", "-q", "-b", "main"]);
            git(&source, &["config", "user.name", "Sandbox Test"]);
            git(
                &source,
                &["config", "user.email", "sandbox@example.invalid"],
            );
            git(&source, &["commit", "-q", "--allow-empty", "-m", "base"]);
            git(
                &repositories.clone,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    source.to_str().unwrap(),
                    name,
                ],
            );
        }
        let modules = git_dir.join("modules");
        let path = |path: &Path| path.to_str().unwrap().to_owned();
        let stage = |git_roots: Vec<PathBuf>| {
            let sandbox = HarnessSandbox {
                directories: Vec::new(),
                approved_roots: git_roots.clone(),
                git_roots,
            };
            let mut config = toml::Table::new();
            configure_codex_permissions(
                &mut config,
                Path::new("config.toml"),
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox,
            )
            .unwrap();
            let codex = config["permissions"][CODEX_PROFILE]["filesystem"]
                .as_table()
                .unwrap()
                .iter()
                .map(|(path, access)| (path.clone(), access.as_str().unwrap().to_owned()))
                .collect::<std::collections::BTreeMap<_, _>>();
            let root = repositories.base.join("worker");
            let home = claude_home(&root);
            configure_claude_git_sandbox(
                &root,
                "session",
                &home,
                ExecutionPolicy::ConfiguredApprovals,
                &sandbox,
            )
            .unwrap();
            let claude = claude_settings(&home)["sandbox"]["filesystem"]["denyWrite"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry.as_str().unwrap().to_owned())
                .collect::<BTreeSet<_>>();
            std::fs::remove_dir_all(&root).unwrap();
            (codex, claude)
        };

        // The clone's submodules (initialized ones, and any a later `git
        // submodule add` would create) cannot be reconfigured.
        let (codex, claude) = stage(vec![repositories.clone.clone()]);
        assert_eq!(codex[&path(&modules)], "read");
        assert!(claude.contains(&path(&modules)));

        // A session in one submodule works in its own directory only.
        let (codex, claude) = stage(vec![
            repositories.clone.clone(),
            repositories.clone.join("sub"),
        ]);
        assert_eq!(codex[&path(&modules)], "read");
        assert_eq!(codex[&path(&modules.join("sub"))], "write");
        assert_eq!(codex[&path(&modules.join("sub/hooks"))], "read");
        assert_eq!(codex[&path(&modules.join("sub/config"))], "read");
        assert!(!claude.contains(&path(&modules)));
        assert!(!claude.contains(&path(&modules.join("sub"))));
        assert!(claude.contains(&path(&modules.join("other"))));
        assert!(claude.contains(&path(&modules.join("sub/hooks"))));
        // A slash-named sibling (`libs/b`) is not hidden behind `libs`.
        assert!(claude.contains(&path(&modules.join("libs"))));

        let (codex, claude) = stage(vec![
            repositories.clone.clone(),
            repositories.clone.join("libs/a"),
        ]);
        assert_eq!(codex[&path(&modules.join("libs/a"))], "write");
        assert!(claude.contains(&path(&modules.join("libs/b"))));
        assert!(claude.contains(&path(&modules.join("sub"))));
        assert!(!claude.contains(&path(&modules.join("libs"))));
        assert!(!claude.contains(&path(&modules.join("libs/a"))));
        assert!(claude.contains(&path(&modules.join("libs/a/hooks"))));
    }

    #[test]
    fn a_root_that_is_a_symlink_is_resolved_where_it_leads() {
        let repositories = repositories();
        let alias = repositories.base.join("alias");
        std::os::unix::fs::symlink(repositories.worktree.join("crates/foo"), &alias).unwrap();

        let targets =
            resolve_git_sandbox_targets(std::slice::from_ref(&alias), std::slice::from_ref(&alias));

        assert_eq!(targets.len(), 1);
        assert!(targets[0].is_worktree);
        assert_eq!(
            targets[0].git_dir,
            repositories.clone.join(".git/worktrees/linked")
        );
        assert_eq!(targets[0].common_dir, repositories.clone.join(".git"));
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
        let resolve =
            |root: &Path| resolve_git_sandbox_targets(&[root.to_owned()], &[root.to_owned()]);
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
        let trusted = resolve_git_sandbox_targets(
            std::slice::from_ref(&moved),
            &[moved.clone(), repositories.clone.clone()],
        );
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
