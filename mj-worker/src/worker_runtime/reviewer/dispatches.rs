//! Worker-owned lane delivery. Reads replay; only durable acceptance acknowledges.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use mj_core::relay::ReviewerLaneDispatch;
use mj_core::review::lanes::{LaneDispatch, LaneDispatchReply};
use serde::{Deserialize, Serialize};

const DISPATCH_FILE: &str = "lane-dispatches.json";
const DISPATCH_BYTE_LIMIT: usize = 1024 * 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchState {
    sequence: u64,
    pending: Vec<ReviewerLaneDispatch>,
}

pub(super) struct LaneDispatches {
    path: PathBuf,
    // Disk is authoritative. A failed write never publishes an optimistic
    // mutation, and every reader waits for the serialized commit.
    commit: Mutex<()>,
}

impl LaneDispatches {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            path: root.join(DISPATCH_FILE),
            commit: Mutex::new(()),
        }
    }

    fn load(&self) -> Result<DispatchState> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("decode reviewer lane dispatches"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(DispatchState::default())
            }
            Err(error) => Err(error).context("read reviewer lane dispatches"),
        }
    }

    fn save(&self, state: &DispatchState) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        anyhow::ensure!(
            bytes.len() <= DISPATCH_BYTE_LIMIT,
            "reviewer lane dispatch queue is full"
        );
        mj_core::config::atomic_write(&self.path, &bytes)
            .context("persist reviewer lane dispatches")
    }

    pub(super) fn record(&self, generation: u64, dispatch: LaneDispatch) -> LaneDispatchReply {
        let result = (|| -> Result<Vec<String>> {
            if let Err(message) = mj_review::lanes::validate_dispatch(&dispatch.reviewers) {
                bail!("{message}");
            }
            let _commit = self.commit.lock().expect("review dispatch lock poisoned");
            let mut state = self.load()?;
            let mut started = Vec::new();
            for request in dispatch.reviewers {
                if state.pending.iter().any(|queued| {
                    queued.generation == generation
                        && queued.request.agent_type == request.agent_type
                }) {
                    continue;
                }
                state.sequence = state
                    .sequence
                    .checked_add(1)
                    .context("review dispatch IDs exhausted")?;
                started.push(request.agent_type.clone());
                state.pending.push(ReviewerLaneDispatch {
                    id: format!("review-lane-{}", state.sequence),
                    generation,
                    request,
                });
            }
            self.save(&state)?;
            Ok(started)
        })();
        match result {
            Ok(started) => LaneDispatchReply {
                started,
                error: None,
            },
            Err(error) => LaneDispatchReply {
                started: Vec::new(),
                error: Some(format!("{error:#}")),
            },
        }
    }

    pub(super) fn read(&self) -> Result<Vec<ReviewerLaneDispatch>> {
        let _commit = self.commit.lock().expect("review dispatch lock poisoned");
        Ok(self.load()?.pending)
    }

    pub(super) fn acknowledge(&self, ids: &[String]) -> Result<()> {
        let _commit = self.commit.lock().expect("review dispatch lock poisoned");
        let mut state = self.load()?;
        state.pending.retain(|dispatch| !ids.contains(&dispatch.id));
        self.save(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::review::lanes::ReviewSubagentRequest;

    fn dispatch() -> LaneDispatch {
        LaneDispatch {
            reviewers: vec![ReviewSubagentRequest {
                agent_type: "tests".into(),
                hypothesis: "check the regression coverage".into(),
            }],
        }
    }

    #[test]
    fn lost_read_response_replays_after_reopening_until_explicit_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        let queue = LaneDispatches::new(root.path().to_path_buf());
        assert_eq!(queue.record(7, dispatch()).started, vec!["tests"]);
        let first = queue.read().unwrap();
        drop(queue);
        let queue = LaneDispatches::new(root.path().to_path_buf());
        assert_eq!(queue.read().unwrap(), first);
        assert!(queue.record(7, dispatch()).started.is_empty());
        queue.acknowledge(&[first[0].id.clone()]).unwrap();
        queue.acknowledge(&[first[0].id.clone()]).unwrap();
        assert!(queue.read().unwrap().is_empty());
        assert_eq!(queue.record(7, dispatch()).started, vec!["tests"]);
        assert_ne!(queue.read().unwrap()[0].id, first[0].id);
    }

    #[test]
    fn supervisor_generations_keep_distinct_dispatch_identity_until_acknowledged() {
        let root = tempfile::tempdir().unwrap();
        let queue = LaneDispatches::new(root.path().to_path_buf());
        assert_eq!(queue.record(7, dispatch()).started, vec!["tests"]);
        assert_eq!(queue.record(8, dispatch()).started, vec!["tests"]);
        let pending = queue.read().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].generation, 7);
        assert_eq!(pending[1].generation, 8);
        assert_ne!(pending[0].id, pending[1].id);
        queue.acknowledge(&[pending[0].id.clone()]).unwrap();
        assert_eq!(queue.read().unwrap(), vec![pending[1].clone()]);
    }

    #[test]
    fn failed_commit_never_reports_a_dispatch_as_started() {
        let root = tempfile::tempdir().unwrap();
        let queue = LaneDispatches::new(root.path().join("blocked"));
        std::fs::write(root.path().join("blocked"), b"not a directory").unwrap();
        let reply = queue.record(7, dispatch());
        assert!(reply.started.is_empty());
        assert!(reply.error.is_some());
        std::fs::remove_file(root.path().join("blocked")).unwrap();
        assert!(queue.read().unwrap().is_empty());
        assert_eq!(queue.record(7, dispatch()).started, vec!["tests"]);
    }
}
