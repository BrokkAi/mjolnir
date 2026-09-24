//! Publication evidence for a verified clone checkpoint.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use mj_core::archive::ArchiveManifest;
use mj_core::state::{
    CheckpointMetadata, ManagedCheckoutKind, PublicationAssessment, PublicationState, SessionRecord,
};

use crate::targets::{CancellableProcessExecutor, CommandExecutor};

use super::worktree::managed_git_command;

/// A failed remote check is evidence of uncertainty, never permission to age
/// away the only checkpoint. The checkpoint itself has already been verified.
pub(super) fn assess_clone_checkpoint(
    session: &SessionRecord,
    checkpoint: &CheckpointMetadata,
) -> PublicationAssessment {
    let mut result = PublicationAssessment {
        checkpoint_sha256: checkpoint.sha256.clone(),
        state: PublicationState::Unknown,
        dirty: false,
        stashed: false,
        saved_commits: Vec::new(),
        destinations: Vec::new(),
        checked_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
    };
    if let Err(error) = assess_clone_checkpoint_inner(session, checkpoint, &mut result) {
        result.state = PublicationState::Unknown;
        result.reason = Some(format!("{error:#}"));
    }
    result
}

/// Assess every repository in a network-backed session while its target is
/// still present. A remote with a newer tip is Unknown until ancestry can be
/// proved; exact matching tips are enough for ordinary pushed branches.
pub(super) fn assess_network_checkpoint(
    session: &SessionRecord,
    checkpoint: &CheckpointMetadata,
    config: &mj_core::config::Config,
) -> PublicationAssessment {
    let mut result = PublicationAssessment {
        checkpoint_sha256: checkpoint.sha256.clone(),
        state: PublicationState::Unknown,
        dirty: false,
        stashed: false,
        saved_commits: Vec::new(),
        destinations: Vec::new(),
        checked_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
    };
    if let Err(error) = assess_network_checkpoint_inner(session, checkpoint, config, &mut result) {
        result.state = PublicationState::Unknown;
        result.reason = Some(format!("{error:#}"));
    }
    result
}

fn assess_network_checkpoint_inner(
    session: &SessionRecord,
    checkpoint: &CheckpointMetadata,
    config: &mj_core::config::Config,
    result: &mut PublicationAssessment,
) -> Result<()> {
    let manifest = mj_checkpoint::archive::read_checkpoint_manifest(&checkpoint.archive_path)
        .context("read verified network checkpoint")?;
    ensure!(
        !manifest.repositories.is_empty(),
        "checkpoint contains no repository"
    );
    let locator = session
        .target
        .as_ref()
        .context("session has no live target")?;
    let backend = super::backend::backend_locator(locator, session, config)?;
    let root = super::network_git::workspace_root(&backend, session.container_workspace.as_deref());
    let bounded = CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(30));
    let mut commits = BTreeSet::new();
    let mut any_unverified = false;
    for repository in &manifest.repositories {
        let meta = &repository.metadata;
        result.dirty |= checkpoint_has_file_changes(&manifest, repository);
        result.stashed |= !meta.stash_stack.is_empty();
        commits.extend(meta.saved_refs.values().cloned());
        commits.insert(meta.head_commit.clone());
        let destinations = if meta.push_urls.is_empty() {
            (!meta.origin.is_empty())
                .then_some(meta.origin.clone())
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            meta.push_urls.clone()
        };
        result.destinations.extend(destinations);
        if result.dirty || result.stashed {
            continue;
        }
        if meta.saved_refs.is_empty() || meta.origin.is_empty() {
            any_unverified = true;
            continue;
        }
        let path = std::path::Path::new(&root).join(&meta.relative_destination);
        let command = crate::targets::command_on_locator(
            &backend,
            &session.id,
            [
                "git".to_owned(),
                "-C".to_owned(),
                path.to_string_lossy().into_owned(),
                "config".to_owned(),
                "--local".to_owned(),
                "--get-all".to_owned(),
                "remote.origin.pushurl".to_owned(),
            ]
            .to_vec(),
            "read network workspace push destinations",
        )?;
        let configured = bounded.execute(&command)?;
        let urls = match configured.status {
            0 => String::from_utf8(configured.stdout)?
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            1 => {
                let fallback = crate::targets::command_on_locator(
                    &backend,
                    &session.id,
                    [
                        "git".to_owned(),
                        "-C".to_owned(),
                        path.to_string_lossy().into_owned(),
                        "remote".to_owned(),
                        "get-url".to_owned(),
                        "origin".to_owned(),
                    ]
                    .to_vec(),
                    "read network workspace fetch destination",
                )?;
                let output = bounded.execute(&fallback)?;
                ensure!(
                    output.status == 0,
                    "network workspace has no configured remote"
                );
                vec![String::from_utf8(output.stdout)?.trim().to_owned()]
            }
            _ => anyhow::bail!("could not read network workspace push destinations"),
        };
        let mut actual = urls
            .iter()
            .map(|url| mj_checkpoint::archive::redact_origin_credentials(url))
            .collect::<Result<Vec<_>>>()?;
        actual.sort();
        let mut expected = if meta.push_urls.is_empty() {
            vec![meta.origin.clone()]
        } else {
            meta.push_urls.clone()
        };
        expected.sort();
        ensure!(
            !urls.is_empty() && actual == expected,
            "network workspace push destinations changed since checkpoint"
        );
        for url in urls {
            let command = crate::targets::command_on_locator(
                &backend,
                &session.id,
                [
                    "git".to_owned(),
                    "-C".to_owned(),
                    path.to_string_lossy().into_owned(),
                    "ls-remote".to_owned(),
                    url,
                    "refs/heads/*".to_owned(),
                    "refs/tags/*".to_owned(),
                    "refs/notes/*".to_owned(),
                ]
                .to_vec(),
                "verify network workspace publication",
            )?;
            let output = bounded
                .execute(&command)
                .context("contact network workspace push remote")?;
            ensure!(
                output.status == 0,
                "verify network workspace publication failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let refs = String::from_utf8(output.stdout)?
                .lines()
                .filter_map(|line| line.split_once('\t'))
                .filter(|(_, name)| !name.ends_with("^{}"))
                .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
                .collect::<BTreeMap<_, _>>();
            if meta
                .saved_refs
                .iter()
                .any(|(name, oid)| refs.get(name) != Some(oid))
                || !refs
                    .iter()
                    .any(|(name, oid)| name.starts_with("refs/heads/") && oid == &meta.head_commit)
            {
                any_unverified = true;
            }
        }
    }
    result.saved_commits = commits.into_iter().collect();
    result.destinations.sort();
    result.destinations.dedup();
    if result.dirty || result.stashed {
        result.state = PublicationState::Unpublished;
        result.reason = Some("checkpoint contains uncommitted files or a stash".into());
    } else if any_unverified {
        result.reason = Some("one or more saved refs are not exact remote tips".into());
    } else {
        result.state = PublicationState::Published;
        result.reason = Some("all saved refs match push remotes".into());
    }
    Ok(())
}

/// Recheck an aged, stopped checkpoint without recreating its execution
/// checkout. Exact remote refs prove publication; a changed remote tip stays
/// Unknown because it might still contain the saved commit.
pub(crate) fn refresh_stopped_clone_publication(
    session: &SessionRecord,
) -> Option<PublicationAssessment> {
    let checkout = session.managed_worktree.as_ref()?;
    if checkout.kind != ManagedCheckoutKind::Clone
        || session.state != mj_core::state::SessionState::Stopped
    {
        return None;
    }
    let checkpoint = session.checkpoint.as_ref()?;
    if session.publication.as_ref().is_some_and(|evidence| {
        evidence.checkpoint_sha256 == checkpoint.sha256
            && (evidence.state == PublicationState::Published || evidence.dirty || evidence.stashed)
    }) {
        return None;
    }
    let mut result = PublicationAssessment {
        checkpoint_sha256: checkpoint.sha256.clone(),
        state: PublicationState::Unknown,
        dirty: false,
        stashed: false,
        saved_commits: Vec::new(),
        destinations: Vec::new(),
        checked_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
    };
    if let Err(error) = refresh_stopped_clone_publication_inner(session, checkpoint, &mut result) {
        result.state = PublicationState::Unknown;
        result.reason = Some(format!("{error:#}"));
    }
    Some(result)
}

fn refresh_stopped_clone_publication_inner(
    session: &SessionRecord,
    checkpoint: &CheckpointMetadata,
    result: &mut PublicationAssessment,
) -> Result<()> {
    let checkout = session
        .managed_worktree
        .as_ref()
        .context("session has no clone")?;
    let verified = mj_checkpoint::archive::verify_archive_streaming(&checkpoint.archive_path)
        .context("verify retained recovery archive")?;
    ensure!(
        verified.archive_sha256 == checkpoint.sha256,
        "recovery archive digest changed"
    );
    let manifest = verified.manifest;
    let [repository] = manifest.repositories.as_slice() else {
        anyhow::bail!("clone checkpoint does not contain exactly one repository");
    };
    let meta = &repository.metadata;
    result.dirty = checkpoint_has_file_changes(&manifest, repository);
    result.stashed = !meta.stash_stack.is_empty();
    result.saved_commits = meta
        .saved_refs
        .values()
        .chain(std::iter::once(&meta.head_commit))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    result.destinations = if meta.push_urls.is_empty() {
        (!meta.origin.is_empty())
            .then_some(meta.origin.clone())
            .into_iter()
            .collect()
    } else {
        meta.push_urls.clone()
    };
    if result.dirty || result.stashed {
        result.state = PublicationState::Unpublished;
        result.reason = Some("checkpoint contains uncommitted files or a stash".into());
        return Ok(());
    }
    if meta.origin.is_empty() || meta.push_urls.iter().any(|url| url != &meta.origin) {
        result.reason = Some("push destination cannot be verified from the source remote".into());
        return Ok(());
    }
    let bounded = CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(15));
    let source = &checkout.source_repository;
    let configured = bounded.execute(&managed_git_command(
        &checkout.target,
        source,
        ["config", "--get", "remote.origin.url"],
        "verify source remote identity",
    ))?;
    ensure!(configured.status == 0, "source origin is unavailable");
    let current_url = mj_checkpoint::archive::redact_origin_credentials(
        String::from_utf8(configured.stdout)?.trim(),
    )?;
    ensure!(
        current_url == meta.origin,
        "source origin changed since checkpoint"
    );
    let remote = bounded.execute(&managed_git_command(
        &checkout.target,
        source,
        [
            "ls-remote",
            "origin",
            "refs/heads/*",
            "refs/tags/*",
            "refs/notes/*",
        ],
        "refresh published Git refs",
    ))?;
    ensure!(
        remote.status == 0,
        "refresh published Git refs failed: {}",
        String::from_utf8_lossy(&remote.stderr).trim()
    );
    let remote_refs = String::from_utf8(remote.stdout)?
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, name)| !name.ends_with("^{}"))
        .map(|(oid, name)| (name.to_owned(), oid.to_owned()))
        .collect::<BTreeMap<_, _>>();
    for (name, oid) in &meta.saved_refs {
        if remote_refs.get(name) != Some(oid) {
            result.reason = Some(format!(
                "{name} is not an exact remote ref; ancestry is unverified"
            ));
            return Ok(());
        }
    }
    if !remote_refs
        .iter()
        .any(|(name, oid)| name.starts_with("refs/heads/") && oid == &meta.head_commit)
    {
        result.reason = Some("saved HEAD is not an exact remote branch tip".into());
        return Ok(());
    }
    result.state = PublicationState::Published;
    result.reason = Some("all saved refs match the push remote".into());
    Ok(())
}

fn assess_clone_checkpoint_inner(
    session: &SessionRecord,
    checkpoint: &CheckpointMetadata,
    result: &mut PublicationAssessment,
) -> Result<()> {
    let clone = session
        .managed_worktree
        .as_ref()
        .context("session has no owned checkout")?;
    ensure!(
        clone.kind == ManagedCheckoutKind::Clone,
        "session is not an independent clone"
    );
    let manifest = mj_checkpoint::archive::read_checkpoint_manifest(&checkpoint.archive_path)
        .context("read verified checkpoint manifest")?;
    let repository = manifest
        .repositories
        .first()
        .context("checkpoint has no repository")?;
    ensure!(
        manifest.repositories.len() == 1,
        "raw clone checkpoint has multiple repositories"
    );
    let meta = &repository.metadata;
    result.dirty = checkpoint_has_file_changes(&manifest, repository);
    result.stashed = !meta.stash_stack.is_empty();
    let commits = meta
        .saved_refs
        .values()
        .chain(std::iter::once(&meta.head_commit))
        .cloned()
        .collect::<BTreeSet<_>>();
    result.saved_commits = commits.into_iter().collect();
    result.destinations = if meta.push_urls.is_empty() {
        (!meta.origin.is_empty())
            .then_some(meta.origin.clone())
            .into_iter()
            .collect()
    } else {
        meta.push_urls.clone()
    };
    if result.dirty || result.stashed {
        result.state = PublicationState::Unpublished;
        result.reason = Some(if result.stashed {
            "checkpoint contains a stash".into()
        } else {
            "checkpoint contains uncommitted files".into()
        });
        return Ok(());
    }
    if meta.origin.is_empty() {
        // A seed-only checkout has no new session work. Every other case
        // remains unknown until a remote destination is configured.
        if meta.head_commit == meta.base_commit
            && meta
                .saved_refs
                .iter()
                .all(|(name, oid)| name.starts_with("refs/heads/") && oid == &meta.base_commit)
        {
            result.state = PublicationState::Published;
            result.reason = Some("no new Git work beyond the source commit".into());
        } else {
            result.reason = Some("repository has no configured push destination".into());
        }
        return Ok(());
    }
    let bounded = CancellableProcessExecutor::with_timeout(std::time::Duration::from_secs(30));
    let destinations = clone_push_destinations(&bounded, clone, meta)?;
    let check_id = mj_core::state::new_session_id()?;
    for (index, destination_url) in destinations.iter().enumerate() {
        let prefix = format!("refs/mj/publication/{check_id}/{index}");
        let fetch = managed_git_command(
            &clone.target,
            &clone.worktree_root,
            [
                "fetch".to_owned(),
                "--no-tags".to_owned(),
                destination_url.clone(),
                format!("+refs/heads/*:{prefix}/heads/*"),
                format!("+refs/tags/*:{prefix}/tags/*"),
                format!("+refs/notes/*:{prefix}/notes/*"),
            ],
            "verify published Git refs",
        );
        let output = bounded
            .execute(&fetch)
            .context("contact push destination")?;
        ensure!(
            output.status == 0,
            "Git publication fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let refs = git_text(
            &bounded,
            clone,
            [
                "for-each-ref",
                "--format=%(refname)%09%(objectname)",
                &prefix,
            ],
        )?;
        let remote_refs = refs
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
            .collect::<BTreeMap<_, _>>();
        let remote_tips = remote_refs
            .iter()
            .filter(|(name, _)| name.starts_with(&format!("{prefix}/heads/")))
            .map(|(_, oid)| oid.as_str())
            .collect::<Vec<_>>();
        for (name, oid) in &meta.saved_refs {
            let destination = name.replacen("refs/", &format!("{prefix}/"), 1);
            if name.starts_with("refs/tags/") || name.starts_with("refs/notes/") {
                if remote_refs.get(&destination) != Some(oid) {
                    result.state = PublicationState::Unpublished;
                    result.reason = Some(format!("{name} has no matching published ref"));
                    return Ok(());
                }
            } else if !any_published_ancestor(&bounded, clone, oid, &remote_tips)? {
                result.state = PublicationState::Unpublished;
                result.reason = Some(format!("{name} contains an unpublished commit"));
                return Ok(());
            }
        }
        if !any_published_ancestor(&bounded, clone, &meta.head_commit, &remote_tips)? {
            result.state = PublicationState::Unpublished;
            result.reason = Some("HEAD contains an unpublished commit".into());
            return Ok(());
        }
    }
    result.state = PublicationState::Published;
    result.reason = Some("all saved refs and HEAD are present on push destinations".into());
    Ok(())
}

fn clone_push_destinations(
    executor: &impl CommandExecutor,
    clone: &mj_core::state::ManagedWorktree,
    meta: &mj_core::archive::RepositoryMetadata,
) -> Result<Vec<String>> {
    let output = executor.execute(&managed_git_command(
        &clone.target,
        &clone.worktree_root,
        ["config", "--local", "--get-all", "remote.origin.pushurl"],
        "read configured push destinations",
    ))?;
    let destinations = match output.status {
        0 => String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        1 => vec![git_text(executor, clone, ["remote", "get-url", "origin"])?],
        _ => anyhow::bail!("could not read configured push destinations"),
    };
    ensure!(
        !destinations.is_empty(),
        "repository has no push destination"
    );
    let mut recorded = destinations
        .iter()
        .map(|url| mj_checkpoint::archive::redact_origin_credentials(url))
        .collect::<Result<Vec<_>>>()?;
    recorded.sort();
    let mut expected = if meta.push_urls.is_empty() {
        vec![meta.origin.clone()]
    } else {
        meta.push_urls.clone()
    };
    expected.sort();
    ensure!(
        recorded == expected,
        "push destinations changed since checkpoint"
    );
    Ok(destinations)
}

fn checkpoint_has_file_changes(
    manifest: &ArchiveManifest,
    repository: &mj_core::archive::RepositoryManifest,
) -> bool {
    manifest.payloads.iter().any(|payload| {
        if payload.path == repository.untracked_tar_path {
            // An empty tar contains two zero blocks.
            payload.size > 1024
        } else {
            (payload.path == repository.staged_patch_path
                || payload.path == repository.unstaged_patch_path)
                && payload.size > 0
        }
    })
}

fn git_text(
    executor: &impl CommandExecutor,
    clone: &mj_core::state::ManagedWorktree,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<String> {
    let output = executor.execute(&managed_git_command(
        &clone.target,
        &clone.worktree_root,
        args,
        "read publication refs",
    ))?;
    ensure!(
        output.status == 0,
        "read publication refs failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn published_ancestor(
    executor: &impl CommandExecutor,
    clone: &mj_core::state::ManagedWorktree,
    saved: &str,
    remote: &str,
) -> Result<bool> {
    let output = executor.execute(&managed_git_command(
        &clone.target,
        &clone.worktree_root,
        ["merge-base", "--is-ancestor", saved, remote],
        "compare published Git history",
    ))?;
    ensure!(
        output.status == 0 || output.status == 1,
        "compare published Git history failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.status == 0)
}

fn any_published_ancestor(
    executor: &impl CommandExecutor,
    clone: &mj_core::state::ManagedWorktree,
    saved: &str,
    tips: &[&str],
) -> Result<bool> {
    for tip in tips {
        if published_ancestor(executor, clone, saved, tip)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::{
        checkpoint_archive_input, committed_repository, raw_session_on, test_git,
        write_checkpoint_archive_input,
    };
    use crate::controller::worktree::{PrimaryCheckoutRequirement, create_managed_worktree};
    use crate::targets::ProcessExecutor;
    use mj_checkpoint::archive::{
        GitCollectionSpec, GitHistoryMode, SystemGit, collect_git_snapshot,
    };
    use mj_core::state::{ManagedCheckoutKind, ManagedWorktreeTarget};
    use std::process::Command;

    #[test]
    fn pushed_unmerged_clone_is_publishable_but_local_commits_and_files_are_not() {
        let source = committed_repository();
        let remote = tempfile::tempdir().unwrap();
        let archive_dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(remote.path())
            .args(["init", "--bare", "--initial-branch=master"])
            .output()
            .unwrap();
        assert!(init.status.success());
        test_git(
            source.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        test_git(source.path(), &["push", "-u", "origin", "master"]);
        let id = "0123456789abcdef0123456789abcdef";
        let base = test_git(source.path(), &["rev-parse", "HEAD"]);
        let checkout = mj_core::state::ManagedWorktree {
            kind: ManagedCheckoutKind::Clone,
            source_project_directory: source.path().to_path_buf(),
            source_repository: source.path().to_path_buf(),
            worktree_root: source.path().join(".mj/clones").join(id),
            branch: "master".into(),
            target: ManagedWorktreeTarget::Local,
            base_commit: Some(base.clone()),
        };
        create_managed_worktree(
            &ProcessExecutor,
            &checkout,
            None,
            PrimaryCheckoutRequirement::Any,
        )
        .unwrap();
        let mut session = raw_session_on("localhost", checkout.worktree_root.to_str().unwrap());
        session.managed_worktree = Some(checkout.clone());
        let assess = || {
            let snapshot = collect_git_snapshot(
                &SystemGit,
                &checkout.worktree_root,
                &GitCollectionSpec {
                    id: "project".into(),
                    relative_destination: "project".into(),
                    history: GitHistoryMode::CloneFrom(base.clone()),
                    origin_override: None,
                },
            )
            .unwrap();
            let input = checkpoint_archive_input(id, 0, vec![snapshot], Vec::new());
            let checkpoint = write_checkpoint_archive_input(archive_dir.path(), id, &input);
            assess_clone_checkpoint(&session, &checkpoint)
        };
        assert_eq!(assess().state, PublicationState::Published);
        std::fs::write(checkout.worktree_root.join("nested/file.txt"), "new work\n").unwrap();
        test_git(&checkout.worktree_root, &["commit", "-am", "new work"]);
        assert_eq!(assess().state, PublicationState::Unpublished);
        test_git(&checkout.worktree_root, &["push"]);
        assert_eq!(assess().state, PublicationState::Published);
        let separate_push = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(separate_push.path())
            .args(["init", "--bare", "--initial-branch=master"])
            .output()
            .unwrap();
        assert!(init.status.success());
        test_git(
            &checkout.worktree_root,
            &[
                "config",
                "remote.origin.pushurl",
                separate_push.path().to_str().unwrap(),
            ],
        );
        assert_eq!(assess().state, PublicationState::Unpublished);
        test_git(&checkout.worktree_root, &["push"]);
        assert_eq!(assess().state, PublicationState::Published);
        std::fs::write(checkout.worktree_root.join("new-untracked.txt"), "draft\n").unwrap();
        let dirty = assess();
        assert_eq!(dirty.state, PublicationState::Unpublished);
        assert!(dirty.dirty);
        test_git(
            &checkout.worktree_root,
            &["stash", "push", "--include-untracked", "-m", "draft"],
        );
        let stashed = assess();
        assert_eq!(stashed.state, PublicationState::Unpublished);
        assert!(stashed.stashed);
    }
}
