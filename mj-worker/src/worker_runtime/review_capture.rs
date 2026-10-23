//! Worker-owned capture operations for child-session reviews.
use crate::relay::{DurableRelay, RelayResponseBody, RelayResponsePayload, ReviewerAdmission};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(super) struct ReviewCapture {
    pub session_id: String,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub review_capture: bool,
    pub untracked_at_start: super::unix::UntrackedReviewEntries,
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
            mj_core::relay::ReviewerRequest::CaptureDelta { baselines, scope } => {
                self.capture_delta(baselines, scope).await
            }
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
            mj_core::relay::ReviewerRequest::AdvanceBaseline { trees } => {
                self.advance_baseline(trees).await
            }
        }
    }
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.cwd.clone()];
        roots.extend(self.additional_directories.iter().cloned());
        roots
    }

    /// Reports what every workspace repository changed since `baselines`.
    ///
    /// Git work is blocking and can take a moment on a large tree, so it runs
    /// on the blocking pool rather than on the runtime that also serves the
    /// primary session's relay.
    async fn capture_delta(
        &self,
        baselines: std::collections::BTreeMap<PathBuf, String>,
        scope: mj_core::relay::CaptureScope,
    ) -> Result<RelayResponseBody> {
        anyhow::ensure!(
            self.review_capture,
            "this session was started without a review baseline, so a review \
             cannot tell its work from what the working tree already held; \
             sub-agent children are never reviewed, and an ordinary session \
             needs an eligible reviewer in Settings before it starts; resume or restart the session after configuring one"
        );
        let roots = self.roots();
        let untracked_at_start = self
            .untracked_at_start
            .lock()
            .expect("untracked-at-start lock poisoned")
            .clone();
        let session_id = self.session_id.clone();
        let repositories = tokio::task::spawn_blocking(move || {
            let repositories = crate::review::capture::discover_repositories(
                &mj_checkpoint::archive::SystemGit,
                &roots,
            );
            if scope == mj_core::relay::CaptureScope::PushableCommits {
                return repositories
                    .iter()
                    .map(|root| {
                        let git = &mj_checkpoint::archive::SystemGit;
                        let start = crate::review::capture::ensure_session_start_head(
                            git,
                            root,
                            &session_id,
                        )?
                        .ok_or_else(|| anyhow::anyhow!("the repository has no HEAD commit"))?;
                        let capture =
                            crate::review::capture::capture_pushable_commits(git, root, &start)?;
                        let mut delta = capture.delta;
                        delta.base_commit = Some(capture.base.commit().to_owned());
                        delta.head_commit = Some(capture.head_commit);
                        Ok(delta)
                    })
                    .collect::<Result<Vec<_>>>();
            }
            crate::review::capture::capture_repository_deltas(
                &mj_checkpoint::archive::SystemGit,
                &repositories,
                &baselines,
                &untracked_at_start,
                &session_id,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review capture stopped: {error}"))??;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewDelta { repositories },
        })
    }

    /// Records the trees a completed review reviewed through.
    async fn advance_baseline(
        &self,
        trees: std::collections::BTreeMap<PathBuf, String>,
    ) -> Result<RelayResponseBody> {
        // The untracked list has to move with the tree it belongs to, or the
        // next review would report every file this turn created as new work
        // all over again. Re-reading the state is one stat-walk per completed
        // review, which is what the new capture costs anyway.
        let roots = self.roots();
        let refreshed = tokio::task::spawn_blocking(move || {
            let repositories = crate::review::capture::discover_repositories(
                &mj_checkpoint::archive::SystemGit,
                &roots,
            );
            crate::review::capture::advance_baselines(&mj_checkpoint::archive::SystemGit, &trees)?;
            crate::review::capture::read_untracked_at_start(
                &mj_checkpoint::archive::SystemGit,
                &repositories,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("the review baseline update stopped: {error}"))??;
        *self
            .untracked_at_start
            .lock()
            .expect("untracked-at-start lock poisoned") = refreshed;
        Ok(RelayResponseBody::Ok {
            payload: RelayResponsePayload::ReviewBaselineAdvanced,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::relay::{
        RELAY_PROTOCOL_VERSION, RelayRequest, RelayRequestEnvelope, ReviewerRequest,
    };

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
        crate::review::capture::initialize_review_baselines(
            &SystemGit,
            std::slice::from_ref(&repository),
            "push-test",
        )
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
            review_capture: true,
            untracked_at_start: Default::default(),
            primary_relay: Arc::new(Mutex::new(
                DurableRelay::open(&worker, "push-test", "1.0.0").unwrap(),
            )),
            operation: tokio::sync::Mutex::new(()),
        });
        let result = capture
            .handle(ReviewerRequest::CaptureDelta {
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
        capture
            .handle(ReviewerRequest::AdvanceBaseline {
                trees: std::collections::BTreeMap::from([(
                    repository.clone(),
                    delta.current_tree.clone(),
                )]),
            })
            .await
            .unwrap();
        let baseline =
            mj_checkpoint::archive::review_baseline_ref(&SystemGit, &repository).unwrap();
        assert_eq!(
            git(&repository, &["rev-parse", &format!("{baseline}^{{tree}}")]).trim(),
            delta.current_tree
        );
        std::fs::write(repository.join("tracked.rs"), "dirty\n").unwrap();
        let error = capture
            .handle(ReviewerRequest::CaptureDelta {
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
                scope: Default::default(),
                baselines: Default::default(),
            },
        };
        assert_eq!(request.minimum_protocol(), 6);
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
