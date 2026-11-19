//! Submodule rules for a repository snapshot.
//!
//! A snapshot records each gitlink as the commit the superproject points to,
//! never the files in the submodule's own working tree.

use super::*;
use crate::archive::GitOutput;

/// What a checkout's gitlinks hold that its snapshot does not carry. Paths
/// are relative to the top level of the inspected checkout.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SubmoduleInspection {
    /// Checked-out submodules, registered in `.gitmodules`, whose working
    /// tree has changes. A snapshot would lose those changes.
    pub dirty: Vec<PathBuf>,
    /// Gitlinks with no `.gitmodules` entry, which is what a nested
    /// repository committed without `git submodule add` leaves. A snapshot
    /// carries only the commit each one points to.
    pub unregistered: Vec<PathBuf>,
}

impl SubmoduleInspection {
    /// "submodule <path> has uncommitted changes", or `None` when every
    /// checked-out submodule is clean.
    pub fn dirty_summary(&self) -> Option<String> {
        let paths = self
            .dirty
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        match paths.as_slice() {
            [] => None,
            [path] => Some(format!("submodule {path} has uncommitted changes")),
            paths => Some(format!(
                "submodules {} have uncommitted changes",
                paths.join(", ")
            )),
        }
    }
}

/// Inspect every gitlink in the checkout that contains `repository`.
///
/// The gitlinks come from the index, not from `git submodule foreach`, which
/// fails outright when a gitlink has no URL in `.gitmodules`. A session's
/// project directory can be a subdirectory of its checkout, so the inspection
/// starts at the checkout's top level to see every gitlink.
pub fn inspect_submodules(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<SubmoduleInspection> {
    let top_level = submodule_git(runner, repository, &["rev-parse", "--show-toplevel"])?;
    let top_level = top_level.strip_suffix(b"\n").unwrap_or(&top_level);
    let top_level = mj_core::path_input::from_git_bytes(top_level)?;
    let mut inspection = SubmoduleInspection::default();
    inspect_checkout(runner, &top_level, Path::new(""), &mut inspection)?;
    Ok(inspection)
}

/// Refuse a repository with uncommitted submodule changes, which the snapshot
/// would lose. A gitlink with no `.gitmodules` entry never blocks the
/// checkpoint, but its files are not in the snapshot, so it is named.
pub(super) fn reject_dirty_submodules(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<()> {
    let inspection = inspect_submodules(runner, repository)?;
    for gitlink in &inspection.unregistered {
        // This runs on the target, where standard error is the only log.
        eprintln!(
            "gitlink {} has no entry in .gitmodules; the checkpoint records the commit it points to, not its files",
            gitlink.display()
        );
    }
    if let Some(dirty) = inspection.dirty_summary() {
        bail!("{dirty}; commit or stash them, then try again");
    }
    Ok(())
}

/// Inspect the gitlinks of one checkout, then of each checked-out submodule
/// in turn. `prefix` is the checkout's path from the outermost top level.
fn inspect_checkout(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    prefix: &Path,
    inspection: &mut SubmoduleInspection,
) -> Result<()> {
    let gitlinks = gitlinks(runner, checkout)?;
    if gitlinks.is_empty() {
        return Ok(());
    }
    let registered = registered_submodule_paths(runner, checkout)?;
    for gitlink in gitlinks {
        let named = prefix.join(&gitlink);
        if !registered.contains(&gitlink) {
            inspection.unregistered.push(named);
            continue;
        }
        let submodule = checkout.join(&gitlink);
        // A submodule that was never checked out has no work to lose.
        if !submodule.join(".git").exists() {
            continue;
        }
        let status = submodule_git(runner, &submodule, &["status", "--porcelain"])?;
        if !status.iter().all(u8::is_ascii_whitespace) {
            inspection.dirty.push(named.clone());
        }
        inspect_checkout(runner, &submodule, &named, inspection)?;
    }
    Ok(())
}

/// Gitlink paths in the checkout's index. A conflicted gitlink has one entry
/// per stage, so the paths are collected as a set.
fn gitlinks(runner: &dyn GitCommandRunner, checkout: &Path) -> Result<BTreeSet<PathBuf>> {
    let listed = submodule_git(runner, checkout, &["ls-files", "--stage", "-z"])?;
    let mut gitlinks = BTreeSet::new();
    for entry in listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        // Each entry is `<mode> <object> <stage>\t<path>`.
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .context("failed to inspect submodules: git ls-files printed an entry with no path")?;
        if entry.starts_with(b"160000 ") {
            gitlinks.insert(mj_core::path_input::from_git_bytes(&entry[tab + 1..])?);
        }
    }
    Ok(gitlinks)
}

/// Submodule paths that `.gitmodules` registers. Git's own submodule commands
/// read the file from the working tree, and so does this.
fn registered_submodule_paths(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<BTreeSet<PathBuf>> {
    if !checkout.join(".gitmodules").exists() {
        return Ok(BTreeSet::new());
    }
    let output = runner.run(
        checkout,
        &git_command(&[
            "config",
            "--file",
            ".gitmodules",
            "--null",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ]),
    )?;
    // `git config` exits 1 when no key matches.
    if output.status == 1 {
        return Ok(BTreeSet::new());
    }
    ensure_inspected(&output)?;
    // With `--null`, each entry is the key, a newline, the value, and a NUL.
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let newline = entry
                .iter()
                .position(|byte| *byte == b'\n')
                .context("failed to inspect submodules: git config printed a key with no value")?;
            mj_core::path_input::from_git_bytes(&entry[newline + 1..])
        })
        .collect()
}

fn submodule_git(
    runner: &dyn GitCommandRunner,
    directory: &Path,
    arguments: &[&str],
) -> Result<Vec<u8>> {
    let output = runner.run(directory, &git_command(arguments))?;
    ensure_inspected(&output)?;
    Ok(output.stdout)
}

fn git_command(arguments: &[&str]) -> GitCommand {
    GitCommand {
        arguments: arguments.iter().copied().map(Into::into).collect(),
        stdin: Vec::new(),
        env: Vec::new(),
    }
}

fn ensure_inspected(output: &GitOutput) -> Result<()> {
    ensure!(
        output.status == 0,
        "failed to inspect submodules: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[derive(Default)]
pub(crate) struct SnapshotSubmoduleInspection {
    pub dirty: Vec<PathBuf>,
    pub unregistered: Vec<PathBuf>,
    pub initialized: Vec<crate::snapshot::SnapshotSubmodule>,
    pub index_gitlinks: Vec<crate::snapshot::SnapshotGitlink>,
    pub untracked: Vec<PathBuf>,
    pub hidden_tracked_edits: Vec<PathBuf>,
    pub intent_to_add: Vec<(PathBuf, PathBuf)>,
    pub remote_refs_stale: bool,
    pub uninitialized_origins: Vec<crate::snapshot::UninitializedOrigin>,
    pub untracked_registration: Vec<PathBuf>,
}
pub(crate) fn inspect_snapshot_submodules(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    origin_override: Option<&str>,
    managed: bool,
) -> Result<SnapshotSubmoduleInspection> {
    let mut inspection = SnapshotSubmoduleInspection::default();
    inspect_snapshot_checkout(
        runner,
        repository,
        Path::new(""),
        origin_override,
        managed,
        &mut inspection,
    )?;
    Ok(inspection)
}
fn inspect_snapshot_checkout(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    prefix: &Path,
    origin_override: Option<&str>,
    managed: bool,
    inspection: &mut SnapshotSubmoduleInspection,
) -> Result<()> {
    let indexed = if prefix.as_os_str().is_empty() {
        snapshot_index_gitlinks(runner, checkout)?
    } else {
        Vec::new()
    };
    let mut paths = gitlinks(runner, checkout)?;
    let changed = submodule_git(
        runner,
        checkout,
        &[
            "diff",
            "--cached",
            "--ignore-submodules=none",
            "--raw",
            "--no-renames",
            "--no-ext-diff",
            "-z",
        ],
    )?;
    let fields = changed
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    for pair in fields.as_chunks::<2>().0 {
        if pair[0].starts_with(b":160000 ") {
            paths.insert(mj_core::path_input::from_git_bytes(pair[1])?);
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let config = snapshot_registered_submodules(runner, checkout)?;
    if managed
        && config.untracked_registration
        && config.paths.keys().any(|path| paths.contains(path))
    {
        inspection
            .untracked_registration
            .push(prefix.join(".gitmodules"));
    }
    inspection
        .index_gitlinks
        .extend(indexed.into_iter().map(|mut link| {
            link.path = prefix.join(link.path);
            link
        }));
    inventory_uninitialized_files(
        runner,
        checkout,
        prefix,
        paths.iter().filter(|path| {
            config.paths.contains_key(*path) && !checkout.join(path).join(".git").exists()
        }),
        &mut inspection.untracked,
    )?;
    for path in paths {
        let named = prefix.join(&path);
        let Some(key) = config.paths.get(&path) else {
            inspection.unregistered.push(named);
            continue;
        };
        let directory = checkout.join(&path);
        if !directory.join(".git").exists() {
            // Only survival of the configured local origin is audited. Native
            // submodule update later decides whether an uninitialized pin exists.
            if managed {
                let url_key = format!(
                    "{}.url",
                    std::str::from_utf8(key)?
                        .strip_suffix(".path")
                        .context("invalid submodule key")?
                );
                if let Some(url) = config.urls.get(&url_key) {
                    let resolved = crate::snapshot::resolve_submodule_url(
                        runner,
                        checkout,
                        url,
                        origin_override,
                    )?;
                    let neutral = tempfile::tempdir()?;
                    let output = runner.run(
                        neutral.path(),
                        &git_command(&["ls-remote", "--get-url", &resolved]),
                    )?;
                    ensure_inspected(&output)?;
                    let effective = std::str::from_utf8(
                        output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout),
                    )?;
                    if let Some(route) =
                        crate::snapshot::local_origin_route(neutral.path(), effective)?
                    {
                        inspection.uninitialized_origins.push(
                            crate::snapshot::UninitializedOrigin {
                                owner: prefix.to_path_buf(),
                                config_key: url_key,
                                route,
                            },
                        );
                    }
                }
            }
            continue;
        }
        let head =
            std::str::from_utf8(&submodule_git(runner, &directory, &["rev-parse", "HEAD"])?)?
                .trim()
                .to_owned();
        inspection
            .initialized
            .push(crate::snapshot::SnapshotSubmodule {
                path: named.clone(),
                commit: head.clone(),
            });
        for path in submodule_git(
            runner,
            &directory,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        {
            inspection
                .untracked
                .push(named.join(mj_core::path_input::from_git_bytes(path)?));
        }
        if managed {
            inspection.hidden_tracked_edits.extend(
                crate::archive::hidden_tracked_paths(runner, &directory)?
                    .into_iter()
                    .map(|path| named.join(path)),
            );
            inspection.intent_to_add.extend(
                crate::snapshot::intent_to_add_paths(runner, &directory)?
                    .into_iter()
                    .map(|path| (named.clone(), path)),
            );
            let status = submodule_git(
                runner,
                &directory,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=no",
                    "--ignore-submodules=none",
                ],
            )?;
            let mut tips = snapshot_gitlink_commits(runner, checkout, &path)?;
            tips.push(head);
            let (delta, stale) =
                crate::snapshot::submodule_selection_has_delta(runner, &directory, tips)?;
            inspection.remote_refs_stale |= stale;
            if !status.is_empty() || delta {
                inspection.dirty.push(named.clone());
            }
        }
        inspect_snapshot_checkout(runner, &directory, &named, None, managed, inspection)?;
    }
    Ok(())
}

/// Objects needed from this submodule: committed and staged gitlinks. HEAD
/// may have no entry for a newly added submodule; conflicted stages all count.
pub(crate) fn snapshot_gitlink_commits(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    path: &Path,
) -> Result<Vec<String>> {
    let mut commits = Vec::new();
    for (arguments, oid_index) in [
        (vec!["ls-tree", "-z", "HEAD", "--"], 2),
        (vec!["ls-files", "--stage", "-z", "--"], 1),
    ] {
        let mut arguments: Vec<std::ffi::OsString> = std::iter::once("--literal-pathspecs")
            .chain(arguments)
            .map(Into::into)
            .collect();
        arguments.push(path.as_os_str().to_owned());
        let output = runner.run(
            checkout,
            &GitCommand {
                arguments,
                ..Default::default()
            },
        )?;
        ensure_inspected(&output)?;
        for entry in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| entry.starts_with(b"160000 "))
        {
            let header = entry
                .split(|byte| *byte == b'\t')
                .next()
                .context("missing gitlink header")?;
            let oid = header
                .split(|byte| *byte == b' ')
                .nth(oid_index)
                .context("missing gitlink object")?;
            commits.push(std::str::from_utf8(oid)?.to_owned());
        }
    }
    Ok(commits)
}

#[derive(Default)]
struct SnapshotModulesConfig {
    paths: std::collections::BTreeMap<PathBuf, Vec<u8>>,
    urls: std::collections::BTreeMap<String, String>,
    untracked_registration: bool,
}
fn snapshot_registered_submodules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<SnapshotModulesConfig> {
    let unmerged = submodule_git(
        runner,
        checkout,
        &["ls-files", "--unmerged", "--", ".gitmodules"],
    )?;
    let source = if !unmerged.is_empty() {
        // Conflict markers cannot be parsed; the operation policy needs the
        // committed registration before managed abort/raw committed-only cuts.
        if submodule_git(runner, checkout, &["ls-tree", "HEAD", "--", ".gitmodules"])?.is_empty() {
            return Ok(Default::default());
        }
        ["--blob", "HEAD:.gitmodules"]
    } else if checkout.join(".gitmodules").exists() {
        ["--file", ".gitmodules"]
    } else {
        // Match Git's registration fallback when working-tree deletion leaves
        // the index or HEAD version available, including staged deletions.
        let indexed = submodule_git(
            runner,
            checkout,
            &["ls-files", "--stage", "-z", "--", ".gitmodules"],
        )?;
        if !indexed.is_empty() {
            ["--blob", ":.gitmodules"]
        } else if !submodule_git(runner, checkout, &["ls-tree", "HEAD", "--", ".gitmodules"])?
            .is_empty()
        {
            ["--blob", "HEAD:.gitmodules"]
        } else {
            return Ok(Default::default());
        }
    };
    let output = runner.run(
        checkout,
        &git_command(&[
            "config",
            source[0],
            source[1],
            "--null",
            "--get-regexp",
            r"^submodule\..*\.(path|url)$",
        ]),
    )?;
    if output.status == 1 {
        return Ok(Default::default());
    }
    ensure_inspected(&output)?;
    let mut paths = std::collections::BTreeMap::new();
    let mut urls = std::collections::BTreeMap::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let newline = entry
            .iter()
            .position(|byte| *byte == b'\n')
            .context("invalid submodule config")?;
        let (key, value) = (&entry[..newline], &entry[newline + 1..]);
        if key.ends_with(b".path") {
            paths.insert(mj_core::path_input::from_git_bytes(value)?, key.to_vec());
        } else {
            urls.insert(
                std::str::from_utf8(key)?.to_owned(),
                std::str::from_utf8(value)?.to_owned(),
            );
        }
    }
    let untracked_registration = if source[0] == "--file" {
        let tracked = runner.run(
            checkout,
            &git_command(&["ls-files", "--error-unmatch", "--", ".gitmodules"]),
        )?;
        ensure!(
            matches!(tracked.status, 0 | 1),
            "failed to inspect submodule registration: {}",
            String::from_utf8_lossy(&tracked.stderr)
        );
        tracked.status == 1
    } else {
        false
    };
    Ok(SnapshotModulesConfig {
        paths,
        urls,
        untracked_registration,
    })
}

/// Uninitialized gitlinks hide ordinary files from Git. Inspect all nonempty
/// directories through one private index, without copying an index for absent
/// or empty paths. NUL stdin avoids argument limits and preserves native paths.
fn inventory_uninitialized_files<'a>(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    prefix: &Path,
    paths: impl Iterator<Item = &'a PathBuf>,
    untracked: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut directories = BTreeSet::new();
    for path in paths {
        let location = checkout.join(path);
        match fs::symlink_metadata(&location) {
            Ok(metadata) => {
                if !metadata.is_dir() || fs::read_dir(location)?.next().transpose()?.is_some() {
                    directories.insert(path.clone());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect uninitialized module directory"),
        }
    }
    if directories.is_empty() {
        return Ok(());
    }
    let index = crate::snapshot::private_index(runner, checkout)?;
    let env = vec![("GIT_INDEX_FILE".into(), index.path().as_os_str().to_owned())];
    let mut stdin = Vec::new();
    for path in &directories {
        stdin.extend(crate::archive::slash_path(path)?.as_bytes());
        stdin.push(0);
    }
    let removed = runner.run(
        checkout,
        &GitCommand {
            arguments: ["update-index", "--force-remove", "-z", "--stdin"]
                .map(Into::into)
                .to_vec(),
            env: env.clone(),
            stdin,
        },
    )?;
    ensure_inspected(&removed)?;
    let files = runner.run(
        checkout,
        &GitCommand {
            arguments: ["ls-files", "--others", "--exclude-standard", "-z"]
                .map(Into::into)
                .to_vec(),
            env,
            ..Default::default()
        },
    )?;
    ensure_inspected(&files)?;
    for path in files
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = mj_core::path_input::from_git_bytes(path)?;
        if path
            .ancestors()
            .any(|ancestor| directories.contains(ancestor))
        {
            untracked.push(prefix.join(path));
        }
    }
    Ok(())
}

pub(crate) struct SnapshotGitStructure {
    pub directories: Vec<PathBuf>,
    pub worktrees: Vec<crate::snapshot::UnrepresentedWorktree>,
}

struct StructuralInspection<'a> {
    runner: &'a dyn GitCommandRunner,
    handled: BTreeSet<PathBuf>,
    root_worktrees: BTreeSet<PathBuf>,
    seen_admins: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
    worktrees: Vec<crate::snapshot::UnrepresentedWorktree>,
    checkout: PathBuf,
}
impl StructuralInspection<'_> {
    fn entries(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("list structural Git state"),
        };
        let mut entries = entries.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        Ok(entries)
    }
    fn modules(&mut self, path: &Path) -> Result<()> {
        for entry in Self::entries(path)? {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = std::fs::canonicalize(entry.path())?;
            if self.handled.contains(&path) {
                self.admin(&path, false)?;
            } else if self
                .handled
                .iter()
                .any(|handled| handled.starts_with(&path))
            {
                self.modules(&path)?;
            } else {
                // Presence alone blocks deletion; never inspect refs/index/operations.
                self.directories.insert(path.clone());
                self.admin(&path, false)?;
            }
        }
        Ok(())
    }
    fn admin(&mut self, path: &Path, root: bool) -> Result<()> {
        if !self.seen_admins.insert(path.to_path_buf()) {
            return Ok(());
        }
        if self.handled.contains(path) {
            self.directories
                .extend(crate::snapshot::git_presence_paths(self.runner, path)?);
        }
        self.modules(&path.join("modules"))?;
        for entry in Self::entries(&path.join("worktrees"))? {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let admin = std::fs::canonicalize(entry.path())?;
            if root && self.root_worktrees.contains(&admin) {
                self.admin(&admin, true)?;
                continue;
            }
            match std::fs::read(admin.join("gitdir")) {
                Ok(bytes) => {
                    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
                    let link = mj_core::path_input::from_git_bytes(bytes)?;
                    let link = if link.is_absolute() {
                        link
                    } else {
                        admin.join(link)
                    };
                    let checkout = link
                        .parent()
                        .context("worktree registration has no checkout")?
                        .to_path_buf();
                    let can_move_inside = root
                        && checkout.is_dir()
                        && !checkout.starts_with(&self.checkout)
                        && !checkout.join(".gitmodules").exists()
                        && !admin.join("modules").exists();
                    self.worktrees.push(crate::snapshot::UnrepresentedWorktree {
                        git_dir: path.to_path_buf(),
                        locked: admin.join("locked").exists(),
                        path: checkout,
                        can_move_inside,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.directories.insert(admin.clone());
                }
                Err(error) => return Err(error).context("read worktree registration path"),
            }
            self.modules(&admin.join("modules"))?;
        }
        Ok(())
    }
}

pub(crate) fn inspect_snapshot_git_structure(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    roots: &[PathBuf],
    initialized_modules: &[PathBuf],
) -> Result<SnapshotGitStructure> {
    let checkout = std::fs::canonicalize(checkout)?;
    let (git_dir, common) = crate::archive::git::git_directories(runner, &checkout)?;
    let git_dir = std::fs::canonicalize(git_dir)?;
    let common = std::fs::canonicalize(common)?;
    let mut inspection = StructuralInspection {
        runner,
        handled: BTreeSet::new(),
        root_worktrees: BTreeSet::new(),
        seen_admins: BTreeSet::new(),
        directories: BTreeSet::new(),
        worktrees: Vec::new(),
        checkout: checkout.clone(),
    };
    for path in roots {
        let admin = std::fs::canonicalize(crate::archive::git::git_directories(runner, path)?.0)?;
        inspection.handled.insert(admin.clone());
        inspection.root_worktrees.insert(admin);
    }
    for path in initialized_modules {
        inspection.handled.insert(std::fs::canonicalize(
            crate::archive::git::git_directories(runner, path)?.0,
        )?);
    }
    // The private root admin is deleted even for a legacy linked checkout;
    // its shared common directory outside these physical roots is not scanned.
    let admins = inspection
        .handled
        .iter()
        .filter(|path| path.starts_with(&checkout) || path.starts_with(&git_dir))
        .cloned()
        .collect::<Vec<_>>();
    for admin in admins {
        inspection.admin(&admin, admin == common)?;
    }
    inspection
        .worktrees
        .sort_by(|a, b| a.git_dir.cmp(&b.git_dir).then(a.path.cmp(&b.path)));
    inspection.worktrees.dedup();
    Ok(SnapshotGitStructure {
        directories: inspection.directories.into_iter().collect(),
        worktrees: inspection.worktrees,
    })
}

pub(crate) fn snapshot_index_gitlinks(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<Vec<crate::snapshot::SnapshotGitlink>> {
    let output = runner.run(
        checkout,
        &GitCommand {
            arguments: vec!["ls-files".into(), "--stage".into(), "-z".into()],
            ..Default::default()
        },
    )?;
    ensure_inspected(&output)?;
    let mut gitlinks = Vec::new();
    for entry in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| entry.starts_with(b"160000 "))
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .context("missing gitlink path")?;
        let fields = std::str::from_utf8(&entry[..tab])?
            .split_whitespace()
            .collect::<Vec<_>>();
        if fields.get(2) == Some(&"0") {
            gitlinks.push(crate::snapshot::SnapshotGitlink {
                path: mj_core::path_input::from_git_bytes(&entry[tab + 1..])?,
                commit: fields.get(1).context("missing gitlink object")?.to_string(),
            });
        }
    }
    Ok(gitlinks)
}

#[cfg(test)]
mod tests;
