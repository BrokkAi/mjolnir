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
    inspect_checkout(runner, &top_level, Path::new(""), &mut inspection, None)?;
    Ok(inspection)
}

/// Snapshot guard: any working changes and unpublished checked-out gitlink commits.
#[derive(Default)]
pub(crate) struct SnapshotSubmoduleInspection {
    pub dirty: Vec<PathBuf>,
    pub initialized: Vec<crate::snapshot::SnapshotSubmodule>,
    pub index_gitlinks: Vec<crate::snapshot::SnapshotGitlink>,
    pub untracked: Vec<PathBuf>,
    pub remote_refs_stale: bool,
}
pub(crate) fn inspect_snapshot_submodules(
    runner: &dyn GitCommandRunner,
    repository: &Path,
) -> Result<SnapshotSubmoduleInspection> {
    let mut inspection = SubmoduleInspection::default();
    let mut snapshot = SnapshotSubmoduleInspection::default();
    inspect_checkout(
        runner,
        repository,
        Path::new(""),
        &mut inspection,
        Some(&mut snapshot),
    )?;
    snapshot.dirty = inspection.dirty;
    Ok(snapshot)
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
    mut snapshot: Option<&mut SnapshotSubmoduleInspection>,
) -> Result<()> {
    let gitlinks = gitlinks(runner, checkout)?;
    if gitlinks.is_empty() {
        return Ok(());
    }
    let registered = registered_submodules(runner, checkout)?;
    if let Some(snapshot) = snapshot.as_deref_mut() {
        snapshot
            .index_gitlinks
            .extend(
                snapshot_index_gitlinks(runner, checkout)?
                    .into_iter()
                    .map(|mut entry| {
                        entry.path = prefix.join(entry.path);
                        entry
                    }),
            );
    }
    for gitlink in gitlinks {
        let named = prefix.join(&gitlink);
        let Some(config_key) = registered.get(&gitlink) else {
            inspection.unregistered.push(named);
            continue;
        };
        let submodule = checkout.join(&gitlink);
        if !submodule.join(".git").exists() {
            continue;
        }
        let mut matching_remotes = Vec::new();
        let mut witnesses: Vec<String> = Vec::new();
        if let Some(snapshot) = snapshot.as_deref_mut() {
            let name = std::str::from_utf8(config_key)?
                .strip_prefix("submodule.")
                .and_then(|name| name.strip_suffix(".path"))
                .context("invalid submodule configuration key")?;
            let configured = runner.run(
                checkout,
                &git_command(&["config", "--get", &format!("submodule.{name}.url")]),
            )?;
            if configured.status != 0 {
                inspection.dirty.push(named);
                continue;
            }
            let configured = std::str::from_utf8(&configured.stdout)?.trim();
            let effective =
                submodule_git(runner, checkout, &["ls-remote", "--get-url", configured])?;
            let effective = std::str::from_utf8(&effective)?.trim();
            let comparison_url = crate::snapshot::resolve_remote_path(checkout, effective)?;
            for remote in
                std::str::from_utf8(&submodule_git(runner, &submodule, &["remote"])?)?.lines()
            {
                let url = submodule_git(runner, &submodule, &["remote", "get-url", remote])?;
                if crate::snapshot::resolve_remote_path(
                    &submodule,
                    std::str::from_utf8(&url)?.trim(),
                )? == comparison_url
                {
                    matching_remotes.push(remote.to_owned());
                }
            }
            if !matching_remotes.is_empty() {
                let mut fallback = Vec::new();
                for remote in &matching_remotes {
                    let listed = submodule_git(
                        runner,
                        &submodule,
                        &[
                            "for-each-ref",
                            "--format=%(objectname)",
                            &format!("refs/remotes/{remote}/"),
                        ],
                    )?;
                    fallback.extend(std::str::from_utf8(&listed)?.lines().map(str::to_owned));
                }
                let remote_state =
                    crate::snapshot::remote_objects(runner, &submodule, effective, &fallback)?;
                snapshot.remote_refs_stale |= remote_state.stale;
                witnesses = remote_state.objects;
            }
            snapshot
                .initialized
                .push(crate::snapshot::SnapshotSubmodule {
                    path: named.clone(),
                    commit: std::str::from_utf8(&submodule_git(
                        runner,
                        &submodule,
                        &["rev-parse", "HEAD"],
                    )?)?
                    .trim()
                    .to_owned(),
                    name: name.to_owned(),
                    url: crate::snapshot::snapshot_remote(checkout, effective)?,
                });
            for path in submodule_git(
                runner,
                &submodule,
                &["ls-files", "--others", "--exclude-standard", "-z"],
            )?
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            {
                snapshot
                    .untracked
                    .push(named.join(mj_core::path_input::from_git_bytes(path)?));
            }
        }
        let status = if snapshot.is_some() {
            submodule_git(
                runner,
                &submodule,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=no",
                    "--ignore-submodules=untracked",
                ],
            )?
        } else {
            submodule_git(runner, &submodule, &["status", "--porcelain"])?
        };
        let mut dirty = !status.iter().all(u8::is_ascii_whitespace);
        if let Some(snapshot) = snapshot.as_ref()
            && !dirty
        {
            let mut commits = snapshot_gitlink_commits(runner, checkout, &gitlink)?;
            commits.push(
                snapshot
                    .initialized
                    .last()
                    .context("missing initialized submodule")?
                    .commit
                    .clone(),
            );
            commits.sort();
            commits.dedup();
            for commit in commits {
                let mut available = false;
                for witness in &witnesses {
                    let output = runner.run(
                        &submodule,
                        &git_command(&["merge-base", "--is-ancestor", &commit, witness]),
                    )?;
                    if output.status == 0 {
                        available = true;
                        break;
                    }
                }
                if !available {
                    dirty = true;
                    break;
                }
            }
            dirty |= matching_remotes.is_empty();
        }
        if dirty {
            inspection.dirty.push(named.clone());
        }
        inspect_checkout(
            runner,
            &submodule,
            &named,
            inspection,
            snapshot.as_deref_mut(),
        )?;
    }
    Ok(())
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

/// Objects needed from this submodule: committed and staged gitlinks. HEAD
/// may have no entry for a newly added submodule; conflicted stages all count.
fn snapshot_gitlink_commits(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    path: &Path,
) -> Result<Vec<String>> {
    let mut commits = Vec::new();
    for (arguments, oid_index) in [
        (vec!["ls-tree", "-z", "HEAD", "--"], 2),
        (vec!["ls-files", "--stage", "-z", "--"], 1),
    ] {
        let mut arguments: Vec<std::ffi::OsString> =
            arguments.into_iter().map(Into::into).collect();
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
fn registered_submodules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
    if !checkout.join(".gitmodules").exists() {
        return Ok(std::collections::BTreeMap::new());
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
        return Ok(std::collections::BTreeMap::new());
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
            Ok((
                mj_core::path_input::from_git_bytes(&entry[newline + 1..])?,
                entry[..newline].to_vec(),
            ))
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

#[cfg(test)]
mod tests;
