//! Worker-owned capture operations for child-session reviews.
use crate::relay::{DurableRelay, RelayResponseBody, RelayResponsePayload, ReviewerAdmission};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(super) struct ReviewCapture {
    pub session_id: String,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub primary_relay: Arc<Mutex<DurableRelay>>,
    pub operation: tokio::sync::Mutex<()>,
}

impl ReviewCapture {
    pub async fn handle(
        self: &Arc<Self>,
        request: mj_core::relay::ReviewerRequest,
    ) -> Result<RelayResponseBody> {
        let capture = self.clone();
        tokio::spawn(async move {
            let result = capture.run(request).await;
            if let Err(error) = &result {
                tracing::warn!(%error, "review capture operation failed");
            }
            result
        })
        .await
        .map_err(|error| anyhow::anyhow!("review capture task stopped: {error}"))?
    }

    async fn run(&self, request: mj_core::relay::ReviewerRequest) -> Result<RelayResponseBody> {
        let _operation = self.operation.lock().await;
        let _admission =
            ReviewerAdmission::acquire(self.primary_relay.clone(), "review capture".to_owned())?;
        match request {
            mj_core::relay::ReviewerRequest::CaptureDelta {
                baselines: _,
                scope: _,
                repositories,
            } => self.capture_delta(repositories).await,
            mj_core::relay::ReviewerRequest::RecordReviewed { trees } => {
                tokio::task::spawn_blocking(move || -> Result<()> {
                    for (root, (tree, commit)) in trees {
                        mj_checkpoint::archive::record_reviewed_tree(
                            &mj_checkpoint::archive::SystemGit,
                            &root,
                            &tree,
                            &commit,
                        )?;
                    }
                    Ok(())
                })
                .await
                .context("reviewed-tree recording task stopped")??;
                Ok(RelayResponseBody::Ok {
                    payload: RelayResponsePayload::ReviewBaselineAdvanced,
                })
            }
            mj_core::relay::ReviewerRequest::RecordOverridden { trees } => {
                tokio::task::spawn_blocking(move || -> Result<()> {
                    for (root, (tree, commit)) in trees {
                        mj_checkpoint::archive::record_overridden_tree(
                            &mj_checkpoint::archive::SystemGit,
                            &root,
                            &tree,
                            &commit,
                        )?;
                    }
                    Ok(())
                })
                .await
                .context("overridden-tree recording task stopped")??;
                Ok(RelayResponseBody::Ok {
                    payload: RelayResponsePayload::ReviewBaselineAdvanced,
                })
            }
            mj_core::relay::ReviewerRequest::SetReviewBase { path, commit } => {
                self.set_review_base(path, commit).await
            }
            mj_core::relay::ReviewerRequest::Snapshot {
                repository,
                operation,
            } => self.snapshot(repository, operation).await,
        }
    }

    /// Creates or removes a review's checkout of one repository.
    async fn snapshot(
        &self,
        repository: PathBuf,
        operation: mj_core::relay::SnapshotOperation,
    ) -> Result<RelayResponseBody> {
        let (path, present) = tokio::task::spawn_blocking(move || {
            let git = &mj_checkpoint::archive::SystemGit;
            match operation {
                mj_core::relay::SnapshotOperation::Create { id, commit } => {
                    let snapshot = crate::review::snapshot::create(git, &repository, &id, &commit)?;
                    Ok::<_, anyhow::Error>((Some(snapshot.path), snapshot.present))
                }
                mj_core::relay::SnapshotOperation::Remove { id } => Ok((
                    None,
                    crate::review::snapshot::remove(git, &repository, &id)?,
                )),
            }
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review snapshot operation stopped: {error}"))??;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewSnapshots { path, present },
        })
    }
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.cwd.clone()];
        roots.extend(self.additional_directories.iter().cloned());
        roots
    }

    /// Captures committed work since the push review base in each repository.
    ///
    /// Git work is blocking and can take a moment on a large tree, so it runs
    /// on the blocking pool rather than on the runtime that also serves the
    /// primary session's relay.
    async fn capture_delta(&self, requested: Option<Vec<PathBuf>>) -> Result<RelayResponseBody> {
        let roots = self.roots();
        let cwd = self.cwd.clone();
        let session_id = self.session_id.clone();
        let repositories = tokio::task::spawn_blocking(move || {
            let discovered = crate::review::capture::discover_repositories(
                &mj_checkpoint::archive::SystemGit,
                &roots,
            );
            let (repositories, origins) = match requested {
                Some(paths) => scoped_repositories(&cwd, &discovered, &paths)?,
                None => (discovered, Default::default()),
            };
            repositories
                .iter()
                .map(|root| {
                    let git = &mj_checkpoint::archive::SystemGit;
                    let start = match origins.get(root) {
                        Some(origin) => crate::review::capture::linked_worktree_start(
                            git,
                            root,
                            origin,
                            &session_id,
                        )?,
                        None => crate::review::capture::review_start_head(git, root, &session_id)?,
                    }
                    .ok_or_else(|| anyhow::anyhow!("the repository has no HEAD commit"))?;
                    let capture =
                        crate::review::capture::capture_pushable_commits(git, root, &start)?;
                    let mut delta = capture.delta;
                    delta.base_commit = capture.base.commit().map(str::to_owned);
                    delta.head_commit = Some(capture.head_commit);
                    Ok(delta)
                })
                .collect::<Result<Vec<_>>>()
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review capture stopped: {error}"))??;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewDelta { repositories },
        })
    }

    /// Makes a reviewed or overridden commit the base of the next push review.
    async fn set_review_base(
        &self,
        path: Option<PathBuf>,
        commit: String,
    ) -> Result<RelayResponseBody> {
        let roots = self.roots();
        let cwd = self.cwd.clone();
        let (root, commit, tree, changed) = tokio::task::spawn_blocking(move || {
            let git = &mj_checkpoint::archive::SystemGit;
            let discovered = crate::review::capture::discover_repositories(git, &roots);
            let (repositories, _) =
                scoped_repositories(&cwd, &discovered, &[path.unwrap_or_else(|| cwd.clone())])?;
            let root = repositories
                .into_iter()
                .next()
                .context("the path names no repository")?;
            if let Some((commit, tree)) =
                crate::review::capture::later_review_base(git, &root, &commit)?
            {
                return Ok((root, commit, tree, false));
            }
            let (commit, tree) = crate::review::capture::set_review_base(git, &root, &commit)?;
            Ok::<_, anyhow::Error>((root, commit, tree, true))
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review base update stopped: {error}"))??;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewBaseSet {
                root,
                commit,
                tree,
                changed,
            },
        })
    }
}

/// The requested repositories, each with the workspace checkout of the same
/// repository it belongs to when it is not itself a workspace root.
fn scoped_repositories(
    cwd: &std::path::Path,
    discovered: &[PathBuf],
    paths: &[PathBuf],
) -> Result<(Vec<PathBuf>, std::collections::BTreeMap<PathBuf, PathBuf>)> {
    let git = &mj_checkpoint::archive::SystemGit;
    let common = |root: &std::path::Path| -> Result<PathBuf> {
        let output = crate::review::capture::checked_git(
            git,
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let path = PathBuf::from(String::from_utf8(output)?.trim());
        std::fs::canonicalize(&path)
            .with_context(|| format!("resolve Git common directory for {}", root.display()))
    };
    let allowed = discovered
        .iter()
        .map(|root| Ok((common(root)?, root.clone())))
        .collect::<Result<Vec<_>>>()?;
    let mut selected = Vec::new();
    let mut origins = std::collections::BTreeMap::new();
    for path in paths {
        let directory = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        // Resolved as discovery resolves a root, so an override's recapture
        // reports the same repository keys the verdict recorded.
        let root = crate::review::capture::repository_root(git, &directory)
            .ok_or_else(|| anyhow::anyhow!("{} is not in a Git repository", path.display()))?;
        let repository = common(&root)?;
        let Some((_, origin)) = allowed.iter().find(|(common, _)| common == &repository) else {
            anyhow::bail!(
                "{} is not a worktree of a workspace repository",
                path.display()
            );
        };
        if !discovered.contains(&root) {
            origins.insert(root.clone(), origin.clone());
        }
        if !selected.contains(&root) {
            selected.push(root);
        }
    }
    Ok((selected, origins))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::relay::{
        RELAY_PROTOCOL_VERSION, RelayRequest, RelayRequestEnvelope, ReviewerRequest,
    };

    // Hard-won: 10e9fd277: explicit commit reviews were refused on child workers without turn baselines.
    #[tokio::test]
    async fn scoped_push_capture_ignores_dirty_root_and_accepts_linked_worktree() {
        use crate::test_support::git;
        use mj_checkpoint::archive::GitCommandRunner;
        use mj_core::relay::CaptureScope;
        let temp = tempfile::tempdir().unwrap();
        // Git reports resolved paths; macOS temporary directories sit behind a symlink.
        let temp_root = temp.path().canonicalize().unwrap();
        let root = temp_root.join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Review Test"]);
        git(&root, &["config", "user.email", "review@example.invalid"]);
        std::fs::write(root.join("tracked.rs"), "base\n").unwrap();
        git(&root, &["add", "tracked.rs"]);
        git(&root, &["commit", "-qm", "base"]);
        let base = git(&root, &["rev-parse", "HEAD"]);
        let worktree = temp_root.join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-qb",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree.join("tracked.rs"), "feature\n").unwrap();
        git(&worktree, &["commit", "-qam", "feature"]);
        std::fs::write(root.join("tracked.rs"), "dirty\n").unwrap();
        let relay_dir = temp_root.join("relay");
        std::fs::create_dir(&relay_dir).unwrap();
        let capture = Arc::new(ReviewCapture {
            session_id: "scoped-test".into(),
            cwd: root.clone(),
            additional_directories: Vec::new(),
            primary_relay: Arc::new(Mutex::new(
                DurableRelay::open(&relay_dir, "scoped-test", "1.0.0").unwrap(),
            )),
            operation: tokio::sync::Mutex::new(()),
        });
        let mut reviewed_tree = String::new();
        for path in [worktree.clone(), PathBuf::from("../linked")] {
            let result = capture
                .handle(ReviewerRequest::CaptureDelta {
                    repositories: Some(vec![path]),
                    baselines: Default::default(),
                    scope: CaptureScope::PushableCommits,
                })
                .await
                .unwrap();
            let RelayResponseBody::Ok {
                payload: RelayResponsePayload::ReviewDelta { repositories },
            } = result
            else {
                panic!("missing delta")
            };
            assert_eq!(repositories.len(), 1);
            assert_eq!(repositories[0].root, worktree);
            reviewed_tree = repositories[0].current_tree.clone();
            assert_eq!(repositories[0].base_commit.as_deref(), Some(base.trim()));
            assert_eq!(
                repositories[0].head_commit.as_deref(),
                Some(git(&worktree, &["rev-parse", "HEAD"]).trim())
            );
        }
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        capture
            .handle(ReviewerRequest::RecordReviewed {
                trees: std::collections::BTreeMap::from([(
                    worktree.clone(),
                    (reviewed_tree, head.trim().to_owned()),
                )]),
            })
            .await
            .unwrap();
        crate::review::hook::install_pre_push_hook(&mj_checkpoint::archive::SystemGit, &worktree)
            .unwrap();
        let remote = temp_root.join("remote.git");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "-q", "--bare"]);
        git(
            &worktree,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let output = mj_checkpoint::archive::SystemGit
            .run(
                &worktree,
                &mj_checkpoint::archive::GitCommand {
                    arguments: vec![
                        "push".into(),
                        "origin".into(),
                        "HEAD:refs/heads/feature".into(),
                    ],
                    stdin: Vec::new(),
                    env: vec![("MJ_REVIEW_GATE".into(), "1".into())],
                },
            )
            .unwrap();
        assert_eq!(
            output.status,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let unrelated = temp_root.join("unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        git(&unrelated, &["init", "-q"]);
        assert!(
            capture
                .handle(ReviewerRequest::CaptureDelta {
                    repositories: Some(vec![unrelated]),
                    baselines: Default::default(),
                    scope: CaptureScope::PushableCommits,
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("not a worktree of a workspace repository")
        );
    }

    /// A worktree made after the session started has no start of its own, and
    /// a repository without main, master or origin/HEAD has no default branch
    /// to bound it. Its first review still covers the commits it already holds.
    #[tokio::test]
    async fn scoped_push_capture_starts_a_new_worktree_at_its_fork_point() {
        use crate::test_support::git;
        use mj_core::relay::CaptureScope;
        let temp = tempfile::tempdir().unwrap();
        let temp_root = temp.path().canonicalize().unwrap();
        let root = temp_root.join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "trunk"]);
        git(&root, &["config", "user.name", "Review Test"]);
        git(&root, &["config", "user.email", "review@example.invalid"]);
        std::fs::write(root.join("tracked.rs"), "base\n").unwrap();
        git(&root, &["add", "tracked.rs"]);
        git(&root, &["commit", "-qm", "base"]);
        let base = git(&root, &["rev-parse", "HEAD"]);
        crate::review::capture::ensure_session_start_head(
            &mj_checkpoint::archive::SystemGit,
            &root,
            "fork-test",
        )
        .unwrap();
        let worktree = temp_root.join("lane");
        git(
            &root,
            &["worktree", "add", "-qb", "lane", worktree.to_str().unwrap()],
        );
        std::fs::write(worktree.join("tracked.rs"), "lane\n").unwrap();
        git(&worktree, &["commit", "-qam", "lane work"]);
        std::fs::write(root.join("other.rs"), "root\n").unwrap();
        git(&root, &["add", "other.rs"]);
        git(&root, &["commit", "-qm", "root moves on"]);
        let relay_dir = temp_root.join("relay");
        std::fs::create_dir(&relay_dir).unwrap();
        let capture = Arc::new(ReviewCapture {
            session_id: "fork-test".into(),
            cwd: root.clone(),
            additional_directories: Vec::new(),
            primary_relay: Arc::new(Mutex::new(
                DurableRelay::open(&relay_dir, "fork-test", "1.0.0").unwrap(),
            )),
            operation: tokio::sync::Mutex::new(()),
        });
        let result = capture
            .handle(ReviewerRequest::CaptureDelta {
                repositories: Some(vec![PathBuf::from("../lane")]),
                baselines: Default::default(),
                scope: CaptureScope::PushableCommits,
            })
            .await
            .unwrap();
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewDelta { repositories },
        } = result
        else {
            panic!("missing delta")
        };
        assert_eq!(repositories.len(), 1);
        let delta = &repositories[0];
        assert_eq!(delta.root, worktree);
        assert_eq!(delta.base_commit.as_deref(), Some(base.trim()));
        assert_eq!(
            delta.head_commit.as_deref(),
            Some(git(&worktree, &["rev-parse", "HEAD"]).trim())
        );
        assert_eq!(
            delta
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["tracked.rs"]
        );
    }

    /// An orphan branch shares no history with the session start or the
    /// default branch, so every commit it holds is reviewed.
    #[tokio::test]
    async fn scoped_push_capture_reviews_an_orphan_worktree_from_the_empty_tree() {
        use crate::test_support::git;
        use mj_core::relay::CaptureScope;
        let temp = tempfile::tempdir().unwrap();
        let temp_root = temp.path().canonicalize().unwrap();
        let root = temp_root.join("root");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Review Test"]);
        git(&root, &["config", "user.email", "review@example.invalid"]);
        std::fs::write(root.join("tracked.rs"), "base\n").unwrap();
        git(&root, &["add", "tracked.rs"]);
        git(&root, &["commit", "-qm", "base"]);
        crate::review::capture::ensure_session_start_head(
            &mj_checkpoint::archive::SystemGit,
            &root,
            "orphan-test",
        )
        .unwrap();
        let worktree = temp_root.join("solo");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                worktree.to_str().unwrap(),
            ],
        );
        git(&worktree, &["checkout", "-q", "--orphan", "solo"]);
        git(&worktree, &["rm", "-rqf", "."]);
        std::fs::write(worktree.join("solo.rs"), "solo\n").unwrap();
        git(&worktree, &["add", "solo.rs"]);
        git(&worktree, &["commit", "-qm", "orphan work"]);
        let relay_dir = temp_root.join("relay");
        std::fs::create_dir(&relay_dir).unwrap();
        let capture = Arc::new(ReviewCapture {
            session_id: "orphan-test".into(),
            cwd: root.clone(),
            additional_directories: Vec::new(),
            primary_relay: Arc::new(Mutex::new(
                DurableRelay::open(&relay_dir, "orphan-test", "1.0.0").unwrap(),
            )),
            operation: tokio::sync::Mutex::new(()),
        });
        let result = capture
            .handle(ReviewerRequest::CaptureDelta {
                repositories: Some(vec![worktree.clone()]),
                baselines: Default::default(),
                scope: CaptureScope::PushableCommits,
            })
            .await
            .unwrap();
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewDelta { repositories },
        } = result
        else {
            panic!("missing delta")
        };
        assert_eq!(repositories.len(), 1);
        let delta = &repositories[0];
        assert_eq!(delta.root, worktree);
        assert_eq!(delta.base_commit, None);
        assert_eq!(delta.base_kind.as_deref(), Some("no shared history"));
        assert_eq!(
            delta.head_commit.as_deref(),
            Some(git(&worktree, &["rev-parse", "HEAD"]).trim())
        );
        assert_eq!(
            delta
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["solo.rs"]
        );
        assert!(delta.patch.contains("+solo"), "{}", delta.patch);
    }

    #[tokio::test]
    async fn an_override_pins_only_the_overridden_ref_and_set_review_base_promotes_it() {
        use crate::test_support::git;
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().canonicalize().unwrap().join("repository");
        std::fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Review Test"]);
        git(
            &repository,
            &["config", "user.email", "review@example.invalid"],
        );
        std::fs::write(repository.join("tracked.rs"), "before\n").unwrap();
        git(&repository, &["add", "tracked.rs"]);
        git(&repository, &["commit", "-qm", "base"]);
        std::fs::write(repository.join("tracked.rs"), "after\n").unwrap();
        git(&repository, &["commit", "-qam", "change"]);
        let head = git(&repository, &["rev-parse", "HEAD"]);
        let tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
        let worker = temp.path().join("worker");
        std::fs::create_dir(&worker).unwrap();
        let capture = Arc::new(ReviewCapture {
            session_id: "override-test".into(),
            cwd: repository.clone(),
            additional_directories: Vec::new(),
            primary_relay: Arc::new(Mutex::new(
                DurableRelay::open(&worker, "override-test", "1.0.0").unwrap(),
            )),
            operation: tokio::sync::Mutex::new(()),
        });
        let refused = capture
            .handle(ReviewerRequest::SetReviewBase {
                path: None,
                commit: head.trim().to_owned(),
            })
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("call request_review first"));
        capture
            .handle(ReviewerRequest::RecordOverridden {
                trees: std::collections::BTreeMap::from([(
                    repository.clone(),
                    (tree.clone(), head.trim().to_owned()),
                )]),
            })
            .await
            .unwrap();
        assert!(git(&repository, &["for-each-ref", "refs/mj/reviewed/"]).is_empty());
        assert_eq!(
            git(
                &repository,
                &["rev-parse", &format!("refs/mj/overridden/{tree}")]
            ),
            head.trim()
        );
        let result = capture
            .handle(ReviewerRequest::SetReviewBase {
                path: None,
                commit: "HEAD".into(),
            })
            .await
            .unwrap();
        let RelayResponseBody::Ok {
            payload:
                RelayResponsePayload::ReviewBaseSet {
                    root,
                    commit,
                    tree: set,
                    changed: true,
                },
        } = result
        else {
            panic!("set_review_base did not answer with the base");
        };
        assert_eq!(
            (root, commit.as_str(), set),
            (repository.clone(), head.trim(), tree.clone())
        );
        assert_eq!(
            git(
                &repository,
                &["rev-parse", &format!("refs/mj/reviewed/{tree}")]
            ),
            head.trim()
        );
        // Hard-won: 9ebd246b2: setting an older reviewed base must be a no-op.
        std::fs::write(repository.join("tracked.rs"), "later\n").unwrap();
        git(&repository, &["commit", "-qam", "later reviewed commit"]);
        let later = git(&repository, &["rev-parse", "HEAD"]);
        let later_tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
        capture
            .handle(ReviewerRequest::RecordReviewed {
                trees: std::collections::BTreeMap::from([(
                    repository.clone(),
                    (later_tree, later.clone()),
                )]),
            })
            .await
            .unwrap();
        let unchanged = capture
            .handle(ReviewerRequest::SetReviewBase {
                path: None,
                commit: head.trim().into(),
            })
            .await
            .unwrap();
        assert!(matches!(unchanged, RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewBaseSet { commit, changed: false, .. }
        } if commit == later));
        let outside = capture
            .handle(ReviewerRequest::SetReviewBase {
                path: Some(temp.path().to_path_buf()),
                commit: "HEAD".into(),
            })
            .await
            .unwrap_err();
        assert!(
            outside.to_string().contains("not in a Git repository"),
            "{outside}"
        );
    }

    #[tokio::test]
    async fn push_capture_and_recording_use_the_committed_git_tree() {
        use crate::test_support::git;
        use mj_checkpoint::archive::SystemGit;
        use mj_core::relay::CaptureScope;
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Review Test"]);
        git(
            &repository,
            &["config", "user.email", "review@example.invalid"],
        );
        std::fs::write(repository.join("tracked.rs"), "before\n").unwrap();
        git(&repository, &["add", "tracked.rs"]);
        git(&repository, &["commit", "-qm", "base"]);
        let start = git(&repository, &["rev-parse", "HEAD"]);
        crate::review::capture::ensure_session_start_head(&SystemGit, &repository, "push-test")
            .unwrap();
        git(&repository, &["checkout", "-qb", "feature"]);
        std::fs::write(repository.join("tracked.rs"), "after\n").unwrap();
        git(&repository, &["commit", "-qam", "change"]);
        std::fs::write(repository.join("untracked.txt"), "not committed").unwrap();
        let worker = temp.path().join("worker");
        std::fs::create_dir(&worker).unwrap();
        let capture = Arc::new(ReviewCapture {
            session_id: "push-test".into(),
            cwd: repository.clone(),
            additional_directories: Vec::new(),
            primary_relay: Arc::new(Mutex::new(
                DurableRelay::open(&worker, "push-test", "1.0.0").unwrap(),
            )),
            operation: tokio::sync::Mutex::new(()),
        });
        let result = capture
            .handle(ReviewerRequest::CaptureDelta {
                repositories: None,
                baselines: Default::default(),
                scope: CaptureScope::PushableCommits,
            })
            .await
            .unwrap();
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewDelta { repositories },
        } = result
        else {
            panic!("capture did not return repository deltas");
        };
        assert_eq!(repositories.len(), 1);
        let delta = &repositories[0];
        assert_eq!(delta.base_commit.as_deref(), Some(start.trim()));
        assert_eq!(
            delta.base_kind.as_deref(),
            Some("merge base with refs/heads/main")
        );
        assert_eq!(
            delta
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["tracked.rs"]
        );
        let head = git(&repository, &["rev-parse", "HEAD"]);
        assert_eq!(delta.head_commit.as_deref(), Some(head.trim()));
        let trees = std::collections::BTreeMap::from([(
            repository.clone(),
            (delta.current_tree.clone(), head.trim().to_owned()),
        )]);
        capture
            .handle(ReviewerRequest::RecordReviewed { trees })
            .await
            .unwrap();
        assert_eq!(
            git(
                &repository,
                &[
                    "rev-parse",
                    &format!("refs/mj/reviewed/{}", delta.current_tree)
                ]
            )
            .trim(),
            head.trim()
        );
        std::fs::write(repository.join("tracked.rs"), "dirty\n").unwrap();
        let error = capture
            .handle(ReviewerRequest::CaptureDelta {
                repositories: None,
                baselines: Default::default(),
                scope: CaptureScope::PushableCommits,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("commit or stash these tracked changes first"));
        assert!(error.contains("tracked.rs"));
    }

    #[test]
    fn a_review_capture_needs_the_protocol_that_introduced_it() {
        let request = RelayRequest::Reviewer {
            role: None,
            request: ReviewerRequest::CaptureDelta {
                repositories: None,
                scope: Default::default(),
                baselines: Default::default(),
            },
        };
        assert_eq!(request.minimum_protocol(), 43);
        assert!(!request.supported_at(5));
        assert!(request.supported_at(RELAY_PROTOCOL_VERSION));
        assert_eq!(request.method_name(), "reviewer_capture_delta");
    }

    #[test]
    fn the_plain_relay_refuses_reviewer_requests() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), "capture-test", "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "req-1".to_owned(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request: RelayRequest::Reviewer {
                role: None,
                request: ReviewerRequest::CaptureDelta {
                    repositories: None,
                    scope: Default::default(),
                    baselines: Default::default(),
                },
            },
        });
        let RelayResponseBody::Error { error } = &response.body else {
            panic!("unexpected response: {response:?}");
        };
        assert!(
            error.message.contains("live relay transport"),
            "{response:?}"
        );
    }
}
