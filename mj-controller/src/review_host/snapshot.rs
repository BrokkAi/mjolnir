//! A push review's own checkout of the commit it reviews.
//!
//! Reviewers read context from the checkout they start in. The requester's
//! worktree keeps changing while the review runs, so a single-repository group
//! gets a detached worktree at exactly the reviewed commit instead. The group
//! owns it: [`HostState::release_group`] is the only place a review slot is
//! removed, and it removes the checkout too. The worker runs git on the
//! session's target; the host only decides which snapshots should exist.
//! Design and limits: `.agents/plans/review-before-push.md`, 2026-11-06.

use super::*;
use mj_core::relay::{RELAY_REVIEW_SNAPSHOT_PROTOCOL, SnapshotOperation};
use std::path::PathBuf;

/// The repository a group reads and the commit it reviews, when the group has
/// one repository to point its reviewers at.
fn source(slot: &ReviewSlot) -> Option<(PathBuf, String)> {
    match slot.driver.deltas() {
        [delta] => Some((delta.root.clone(), delta.head_commit.clone()?)),
        _ => None,
    }
}

async fn snapshot_action(
    control: &SessionManagerControl,
    session_id: &str,
    repository: PathBuf,
    operation: SnapshotOperation,
) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let outcome = reviewer_action(
        control,
        session_id,
        None,
        ReviewerAction::Snapshot {
            repository,
            operation,
        },
    )
    .await;
    match outcome {
        Ok(ReviewerOutcome::Snapshots { path, present }) => Ok((path, present)),
        other => Err(unexpected(other)),
    }
}

/// Why a group has no checkout of its own. Both leave its reviewers reading
/// the requester's checkout; they are logged apart because only the second is
/// unexpected.
#[derive(Debug)]
pub(super) enum SnapshotFailure {
    /// The session's worker predates review snapshots.
    OldWorker,
    Failed(String),
}

async fn create_snapshot(
    control: &SessionManagerControl,
    session_id: &str,
    repository: PathBuf,
    id: String,
    commit: String,
) -> Result<(PathBuf, Vec<String>), SnapshotFailure> {
    let handle = control
        .session(session_id.to_owned())
        .await
        .map_err(|error| SnapshotFailure::Failed(format!("{error:#}")))?;
    let protocol = handle
        .view()
        .snapshot
        .and_then(|snapshot| snapshot.operational.relay_protocol_version)
        .unwrap_or(0);
    if protocol < RELAY_REVIEW_SNAPSHOT_PROTOCOL {
        return Err(SnapshotFailure::OldWorker);
    }
    let (path, present) = snapshot_action(
        control,
        session_id,
        repository,
        SnapshotOperation::Create { id, commit },
    )
    .await
    .map_err(SnapshotFailure::Failed)?;
    path.map(|path| (path, present))
        .ok_or_else(|| SnapshotFailure::Failed("the worker returned no snapshot path".to_owned()))
}

impl HostState {
    /// Starts a group's reviewers. A group with one repository first asks the
    /// worker for its own checkout, and its reviewers start once that settles.
    /// A restored group asks again: the worker keeps a checkout still at the
    /// commit and re-creates a missing one.
    pub(super) fn start_group(&mut self, session_id: &str, requests: Vec<ReviewRequest>) {
        let Some(slot) = self.reviews.get_mut(session_id) else {
            return;
        };
        let wanted = source(slot).filter(|_| !slot.driver.awaited_commands().is_empty());
        let Some((repository, commit)) = wanted else {
            self.run(session_id, requests);
            return;
        };
        let id = slot.driver.snapshot().review_id;
        let epoch = slot.epoch;
        slot.awaiting_snapshot = Some(requests);
        let control = self.control.clone();
        let events = self.events.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            let result = create_snapshot(
                &control,
                &session_id,
                repository.clone(),
                id.clone(),
                commit,
            )
            .await;
            let _ = events.send(HostEvent::SnapshotCreated {
                session_id,
                epoch,
                repository,
                id,
                result,
            });
        });
    }

    /// The worker answered. The host, not the worker, decides which of the
    /// repository's snapshots are orphans, here in its single event loop where
    /// every live group is known; a worker deciding from a list sent earlier
    /// could remove a group another session opened since.
    pub(super) fn snapshot_created(
        &mut self,
        session_id: String,
        epoch: u64,
        repository: PathBuf,
        id: String,
        result: Result<(PathBuf, Vec<String>), SnapshotFailure>,
    ) {
        let result = result.map(|(path, present)| {
            let live: BTreeSet<_> = self
                .reviews
                .values()
                .map(|slot| slot.driver.snapshot().review_id)
                .collect();
            // A group that ended while its snapshot was being made is not
            // live, so this also removes the checkout it left behind.
            for orphan in present.iter().filter(|name| !live.contains(*name)) {
                self.remove_snapshot(&session_id, repository.clone(), orphan.clone());
            }
            path
        });
        let Some(slot) = self
            .reviews
            .get_mut(&session_id)
            .filter(|slot| slot.epoch == epoch)
        else {
            return;
        };
        match result {
            Ok(path) => slot.snapshot = Some(path),
            Err(SnapshotFailure::OldWorker) => tracing::warn!(
                session_id,
                review_id = id,
                "the session's worker predates review snapshots; its reviewers read the requester's checkout until the session restarts"
            ),
            Err(SnapshotFailure::Failed(error)) => tracing::warn!(
                session_id,
                review_id = id,
                repository = %repository.display(),
                %error,
                "could not create the review's own checkout; its reviewers read the requester's checkout"
            ),
        }
        // A review cancelled while the checkout was being made has no reviewer
        // left to start.
        let requests = slot
            .awaiting_snapshot
            .take()
            .filter(|_| !slot.driver.awaited_commands().is_empty());
        if let Some(requests) = requests {
            self.run(&session_id, requests);
        }
    }

    /// Removes a review slot and its checkout. Every way a review ends comes
    /// through here; a daemon shutdown does not, because the group and its
    /// reviewers survive it.
    pub(super) fn release_group(&mut self, session_id: &str) {
        let Some(slot) = self.reviews.remove(session_id) else {
            return;
        };
        if slot.snapshot.is_some()
            && let Some((repository, _)) = source(&slot)
        {
            self.remove_snapshot(session_id, repository, slot.driver.snapshot().review_id);
        }
    }

    /// Removal failing leaves a checkout that the next sweep of the repository
    /// removes; it never affects a verdict.
    fn remove_snapshot(&self, session_id: &str, repository: PathBuf, id: String) {
        let control = self.control.clone();
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = snapshot_action(
                &control,
                &session_id,
                repository.clone(),
                SnapshotOperation::Remove { id: id.clone() },
            )
            .await
            {
                tracing::warn!(
                    session_id,
                    snapshot = id,
                    repository = %repository.display(),
                    %error,
                    "could not remove a review's checkout; the next sweep of the repository retries"
                );
            }
        });
    }
}
