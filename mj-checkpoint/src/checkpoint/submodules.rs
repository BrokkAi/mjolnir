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
    inspect_checkout(
        runner,
        &top_level,
        Path::new(""),
        &mut inspection,
        None,
        None,
    )?;
    Ok(inspection)
}

/// Snapshot guard: any working changes and unpublished checked-out gitlink commits.
#[derive(Default)]
pub(crate) struct SnapshotSubmoduleInspection {
    pub notices: Vec<String>,
    pub dirty: Vec<PathBuf>,
    pub unregistered: Vec<PathBuf>,
    pub initialized: Vec<crate::snapshot::SnapshotSubmodule>,
    pub index_gitlinks: Vec<crate::snapshot::SnapshotGitlink>,
    pub untracked: Vec<PathBuf>,
    pub hidden_tracked_edits: Vec<PathBuf>,
    pub intent_to_add: Vec<(PathBuf, PathBuf)>,
    pub remote_refs_stale: bool,
    pub local_work: Vec<crate::snapshot::ModuleLocalWork>,
    pub operations: Vec<(PathBuf, crate::snapshot::GitOperationKind)>,
}
pub(crate) fn inspect_snapshot_submodules(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    origin_override: Option<&str>,
) -> Result<SnapshotSubmoduleInspection> {
    let mut inspection = SubmoduleInspection::default();
    let mut snapshot = SnapshotSubmoduleInspection::default();
    inspect_checkout(
        runner,
        repository,
        Path::new(""),
        &mut inspection,
        Some(&mut snapshot),
        origin_override,
    )?;
    snapshot.dirty = inspection.dirty;
    snapshot.unregistered = inspection.unregistered;
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
    origin_override: Option<&str>,
) -> Result<()> {
    let mut gitlinks = gitlinks(runner, checkout)?;
    if snapshot.is_some() {
        // Current index entries already cover unchanged gitlinks. The raw
        // index diff adds HEAD gitlinks staged for deletion/type replacement.
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
        let fields: Vec<_> = changed
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect();
        ensure!(fields.len().is_multiple_of(2), "malformed Git index diff");
        for pair in fields.as_chunks::<2>().0 {
            if pair[0].starts_with(b":160000 ") {
                gitlinks.insert(mj_core::path_input::from_git_bytes(pair[1])?);
            }
        }
    }
    if gitlinks.is_empty() {
        return Ok(());
    }
    let registered = if snapshot.is_some() {
        snapshot_registered_submodules(runner, checkout)?
    } else {
        registered_submodules(runner, checkout)?
    };
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
            if let Some(snapshot) = snapshot.as_deref_mut() {
                // Git normally stops at an indexed gitlink. Remove it only
                // from a private index so its ordinary files enter consent,
                // with the parent's and directory's ignore rules intact.
                let index = crate::snapshot::private_index(runner, checkout)?;
                let env = vec![("GIT_INDEX_FILE".into(), index.path().as_os_str().to_owned())];
                let removed = runner.run(
                    checkout,
                    &GitCommand {
                        arguments: [
                            std::ffi::OsString::from("--literal-pathspecs"),
                            "update-index".into(),
                            "--force-remove".into(),
                            "--".into(),
                            gitlink.as_os_str().to_owned(),
                        ]
                        .to_vec(),
                        env: env.clone(),
                        ..Default::default()
                    },
                )?;
                ensure!(
                    removed.status == 0,
                    "inspect uninitialized module files: {}",
                    String::from_utf8_lossy(&removed.stderr)
                );
                let files = runner.run(
                    checkout,
                    &GitCommand {
                        arguments: [
                            std::ffi::OsString::from("--literal-pathspecs"),
                            "ls-files".into(),
                            "--others".into(),
                            "--exclude-standard".into(),
                            "-z".into(),
                            "--".into(),
                            gitlink.as_os_str().to_owned(),
                        ]
                        .to_vec(),
                        env,
                        ..Default::default()
                    },
                )?;
                ensure!(
                    files.status == 0,
                    "list uninitialized module files: {}",
                    String::from_utf8_lossy(&files.stderr)
                );
                for path in files
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                {
                    snapshot
                        .untracked
                        .push(prefix.join(mj_core::path_input::from_git_bytes(path)?));
                }
            }
            if snapshot.is_some()
                && !uninitialized_gitlink_available(
                    runner,
                    checkout,
                    &gitlink,
                    config_key,
                    origin_override,
                )?
            {
                inspection.dirty.push(named);
            }
            continue;
        }
        let mut origin_evidence = None;
        if let Some(snapshot) = snapshot.as_deref_mut() {
            if let Some(operation) = crate::snapshot::operation(runner, &submodule)? {
                snapshot.operations.push((named.clone(), operation));
            }
            snapshot.intent_to_add.extend(
                crate::snapshot::intent_to_add_paths(runner, &submodule)?
                    .into_iter()
                    .map(|path| (named.clone(), path)),
            );
            snapshot.hidden_tracked_edits.extend(
                crate::archive::hidden_tracked_paths(runner, &submodule)?
                    .into_iter()
                    .map(|path| named.join(path)),
            );
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
            let url = crate::snapshot::stored_remote(runner, checkout, configured)?;
            let origins = crate::snapshot::config_values(runner, &submodule, "remote.origin.url")?;
            if origins.is_empty() {
                inspection.dirty.push(named);
                continue;
            }
            let origin_urls = origins
                .iter()
                .map(|url| crate::snapshot::stored_remote(runner, &submodule, url))
                .collect::<Result<Vec<_>>>()?;
            let push_urls =
                crate::snapshot::config_values(runner, &submodule, "remote.origin.pushurl")?
                    .iter()
                    .map(|url| crate::snapshot::stored_remote(runner, &submodule, url))
                    .collect::<Result<Vec<_>>>()?;
            let tracking = crate::snapshot::origin_tracking_objects(runner, &submodule)?;
            let evidence =
                crate::snapshot::remote_objects(runner, &submodule, "origin", &tracking.objects)?;
            ensure!(
                !(evidence.stale && tracking.ambiguous && evidence.objects.is_empty()),
                "submodule {} has ambiguous offline origin refs; restore connectivity or use a unique fetch namespace",
                named.display()
            );
            snapshot.remote_refs_stale |= evidence.stale && !evidence.objects.is_empty();
            origin_evidence = Some(evidence);
            let captured_rules = crate::snapshot::local_url_rules(runner, &submodule)?;
            snapshot.notices.extend(
                captured_rules
                    .notices
                    .into_iter()
                    .map(|notice| format!("{}: {notice}", named.display())),
            );
            let url_rules = captured_rules.rules;
            let captured_worktree_rules = crate::snapshot::worktree_url_rules(runner, &submodule)?;
            snapshot.notices.extend(
                captured_worktree_rules
                    .notices
                    .into_iter()
                    .map(|notice| format!("{}: {notice}", named.display())),
            );
            let worktree_url_rules = captured_worktree_rules.rules;
            if let Some(notice) = crate::snapshot::worktree_remote_notice(runner, &submodule)? {
                snapshot
                    .notices
                    .push(format!("{}: {notice}", named.display()));
            }

            snapshot
                .initialized
                .push(crate::snapshot::SnapshotSubmodule {
                    origin_urls,
                    push_urls,
                    worktree_config_enabled: crate::snapshot::worktree_config_enabled(
                        runner, &submodule,
                    )?,
                    object_format: std::str::from_utf8(&submodule_git(
                        runner,
                        &submodule,
                        &["rev-parse", "--show-object-format"],
                    )?)?
                    .trim()
                    .to_owned(),
                    url_rules,
                    worktree_url_rules,
                    path: named.clone(),
                    commit: std::str::from_utf8(&submodule_git(
                        runner,
                        &submodule,
                        &["rev-parse", "HEAD"],
                    )?)?
                    .trim()
                    .to_owned(),
                    name: name.to_owned(),
                    url,
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
        if let Some(snapshot) = snapshot.as_deref_mut() {
            let evidence = origin_evidence
                .as_ref()
                .context("missing submodule origin evidence")?;
            snapshot.local_work.extend(unpublished_local_work(
                runner,
                &submodule,
                &named,
                &evidence.objects,
            )?);
        }
        let status = if snapshot.is_some() {
            submodule_git(
                runner,
                &submodule,
                &[
                    "status",
                    "--porcelain",
                    "--untracked-files=no",
                    "--ignore-submodules=all",
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
                let evidence = origin_evidence
                    .as_ref()
                    .context("missing submodule origin query")?;
                let available = commit_published(runner, &submodule, &commit, &evidence.objects)?;
                if !available {
                    dirty = true;
                    break;
                }
            }
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
            None,
        )?;
    }
    Ok(())
}

fn commit_published(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    commit: &str,
    witnesses: &[String],
) -> Result<bool> {
    for witness in witnesses {
        if commit == witness
            || runner
                .run(
                    repository,
                    &git_command(&["merge-base", "--is-ancestor", commit, witness]),
                )?
                .status
                == 0
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) struct SnapshotGitStructure {
    pub directories: Vec<PathBuf>,
    pub worktrees: Vec<crate::snapshot::UnrepresentedWorktree>,
}

struct StructuralInspection {
    handled: BTreeSet<PathBuf>,
    root_worktrees: BTreeSet<PathBuf>,
    seen_admins: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
    worktrees: Vec<crate::snapshot::UnrepresentedWorktree>,
    checkout: PathBuf,
}
impl StructuralInspection {
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

fn unpublished_local_work(
    runner: &dyn GitCommandRunner,
    repository: &Path,
    named: &Path,
    witnesses: &[String],
) -> Result<Vec<crate::snapshot::ModuleLocalWork>> {
    let listed = submodule_git(
        runner,
        repository,
        &[
            "for-each-ref",
            "--format=%(objectname)%00%(refname)%00%(objecttype)",
            "refs/heads",
            "refs/tags",
            "refs/notes",
        ],
    )?;
    let active = runner.run(
        repository,
        &git_command(&["symbolic-ref", "--quiet", "HEAD"]),
    )?;
    let active = std::str::from_utf8(&active.stdout)?.trim();
    let mut refs = Vec::new();
    for line in std::str::from_utf8(&listed)?.lines() {
        let mut fields = line.split('\0');
        let oid = fields.next().context("missing submodule ref object")?;
        let name = fields.next().context("missing submodule ref name")?;
        let kind = fields.next().context("missing submodule ref type")?;
        // The active branch's position is already checked by the gitlink guard.
        if name != active {
            refs.push((oid.to_owned(), name.to_owned(), kind.to_owned()));
        }
    }
    let stash_ref = runner.run(
        repository,
        &git_command(&["rev-parse", "--verify", "--quiet", "refs/stash"]),
    )?;
    ensure!(
        matches!(stash_ref.status, 0 | 1),
        "could not inspect submodule stash: {}",
        String::from_utf8_lossy(&stash_ref.stderr)
    );
    if stash_ref.status == 0 {
        // Preserve the current stash ref even after its reflog expires.
        let stashes = submodule_git(
            runner,
            repository,
            &["reflog", "show", "--format=%H%x00%gd", "refs/stash"],
        )?;
        let current = std::str::from_utf8(&stash_ref.stdout)?
            .split_whitespace()
            .next()
            .context("missing submodule stash object")?;
        let mut listed_current = false;
        for line in std::str::from_utf8(&stashes)?.lines() {
            let (oid, name) = line.split_once('\0').context("malformed submodule stash")?;
            listed_current |= oid == current;
            refs.push((oid.to_owned(), name.to_owned(), "commit".to_owned()));
        }
        if !listed_current {
            // Reflog expiration can empty the list while refs/stash still keeps work.
            refs.push((
                current.to_owned(),
                "refs/stash".to_owned(),
                "commit".to_owned(),
            ));
        }
    }
    let mut lost = Vec::new();
    for (oid, name, kind) in refs {
        let mut published = witnesses.contains(&oid);
        if kind == "commit" {
            published = commit_published(runner, repository, &oid, witnesses)?;
        } else if !published && !witnesses.is_empty() {
            let mut arguments = vec![
                "rev-list".into(),
                "--objects".into(),
                "--no-object-names".into(),
            ];
            if kind == "tag" {
                arguments.push("--filter=tree:0".into());
            }
            arguments.extend(witnesses.iter().map(Into::into));
            let graph = runner.run(
                repository,
                &GitCommand {
                    arguments,
                    ..Default::default()
                },
            )?;
            ensure_inspected(&graph)?;
            published = std::str::from_utf8(&graph.stdout)?
                .lines()
                .any(|object| object == oid);
        }
        if !published {
            lost.push(crate::snapshot::ModuleLocalWork {
                path: named.to_path_buf(),
                description: format!("submodule {} {name} ({oid})", named.display()),
            });
        }
    }
    Ok(lost)
}

fn uninitialized_gitlink_available(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    path: &Path,
    config_key: &[u8],
    origin_override: Option<&str>,
) -> Result<bool> {
    let indexed = snapshot_index_gitlinks(runner, checkout)?
        .into_iter()
        .find(|entry| entry.path == path);
    let Some(indexed) = indexed else {
        return Ok(true);
    };
    let mut arguments: Vec<std::ffi::OsString> =
        ["--literal-pathspecs", "ls-tree", "-z", "HEAD", "--"]
            .into_iter()
            .map(Into::into)
            .collect();
    arguments.push(path.as_os_str().to_owned());
    let head = runner.run(
        checkout,
        &GitCommand {
            arguments,
            ..Default::default()
        },
    )?;
    let old = head
        .stdout
        .split(|byte| *byte == 0)
        .find(|entry| entry.starts_with(b"160000 "))
        .and_then(|entry| entry.split(|byte| *byte == b'\t').next())
        .and_then(|header| header.split(|byte| *byte == b' ').nth(2));
    if old.is_some_and(|old| old == indexed.commit.as_bytes()) {
        return Ok(true);
    }
    // Cached admin repositories may point at a fork. Only .gitmodules tips
    // authorize an uninitialized pointer change; initialize to check ancestry.
    let key = std::str::from_utf8(config_key)?
        .strip_suffix(".path")
        .context("invalid submodule path key")?;
    let url = runner.run(
        checkout,
        &git_command(&[
            "config",
            "--file",
            ".gitmodules",
            "--get",
            &format!("{key}.url"),
        ]),
    )?;
    if url.status != 0 {
        return Ok(false);
    }
    let mut url =
        std::str::from_utf8(url.stdout.strip_suffix(b"\n").unwrap_or(&url.stdout))?.to_owned();
    if origin_override.is_some()
        && (url.starts_with("./") || url.starts_with("../"))
        && runner
            .run(checkout, &git_command(&["remote", "get-url", "origin"]))?
            .status
            != 0
    {
        // A provisioned no-origin restore resolves this against its own cwd,
        // which capture cannot know. Use the existing staged-pin refusal.
        return Ok(false);
    }
    url = crate::snapshot::resolve_submodule_url(runner, checkout, &url, origin_override)?;
    // Match the clone used by submodule update: host config applies, but the
    // parent's repository-local rewrite rules must not reroute this probe.
    let neutral = tempfile::tempdir().context("create neutral submodule probe directory")?;
    let mut probe = git_command(&["ls-remote", &url]);
    // TMPDIR itself may be inside a checkout; do not discover any parent repo.
    probe.env.push((
        "GIT_CEILING_DIRECTORIES".into(),
        neutral.path().as_os_str().to_owned(),
    ));
    let advertised = runner.run(neutral.path(), &probe)?;
    let exact_tip = advertised.status == 0
        && std::str::from_utf8(&advertised.stdout)?
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .any(|(oid, name)| {
                oid == indexed.commit
                    && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            });
    Ok(exact_tip)
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
fn snapshot_registered_submodules(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
) -> Result<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
    let unmerged = submodule_git(
        runner,
        checkout,
        &["ls-files", "--unmerged", "--", ".gitmodules"],
    )?;
    if unmerged.is_empty() {
        return registered_submodules(runner, checkout);
    }
    // Use committed registration to inventory an unresolved operation. Managed
    // capture aborts before repinning; raw capture skips the working overlay.
    let committed = submodule_git(runner, checkout, &["ls-tree", "HEAD", "--", ".gitmodules"])?;
    if committed.is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }
    let output = runner.run(
        checkout,
        &git_command(&[
            "config",
            "--blob",
            "HEAD:.gitmodules",
            "--null",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ]),
    )?;
    parse_registered_submodules(output)
}

pub(crate) fn registered_gitlinks_at(
    runner: &dyn GitCommandRunner,
    checkout: &Path,
    commit: &str,
    config_commit: &str,
) -> Result<BTreeSet<PathBuf>> {
    let file = format!("{config_commit}:.gitmodules");
    let exists = runner.run(checkout, &git_command(&["cat-file", "-e", &file]))?;
    if exists.status != 0 {
        return Ok(BTreeSet::new());
    }
    let registered = parse_registered_submodules(runner.run(
        checkout,
        &git_command(&[
            "config",
            "--blob",
            &file,
            "--null",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ]),
    )?)?;
    let tree = submodule_git(runner, checkout, &["ls-tree", "-r", "-z", commit])?;
    let mut paths = BTreeSet::new();
    for entry in tree
        .split(|byte| *byte == 0)
        .filter(|entry| entry.starts_with(b"160000 "))
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .context("invalid predicted gitlink")?;
        let path = mj_core::path_input::from_git_bytes(&entry[tab + 1..])?;
        if registered.contains_key(&path) {
            paths.insert(path);
        }
    }
    Ok(paths)
}

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
    parse_registered_submodules(output)
}
fn parse_registered_submodules(
    output: crate::archive::GitOutput,
) -> Result<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
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
