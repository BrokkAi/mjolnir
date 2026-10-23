//! Worker-owned capture operations for child-session reviews.
use crate::relay::{DurableRelay, RelayResponseBody, RelayResponsePayload, ReviewerAdmission};
use anyhow::Result;
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
            mj_core::relay::ReviewerRequest::CaptureDelta { baselines } => {
                self.capture_delta(baselines).await
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

    #[test]
    fn a_review_capture_needs_the_protocol_that_introduced_it() {
        let request = RelayRequest::Reviewer {
            role: None,
            request: ReviewerRequest::CaptureDelta {
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
