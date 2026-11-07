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

/// What the worker reports of a snapshot operation.
pub(super) struct SnapshotAnswer {
    path: Option<PathBuf>,
    /// Every snapshot id this instance holds in the repository afterwards.
    present: Vec<String>,
    /// What a creation left out.
    notes: Vec<String>,
}

async fn snapshot_action(
    control: &SessionManagerControl,
    session_id: &str,
    repository: PathBuf,
    operation: SnapshotOperation,
) -> Result<SnapshotAnswer, String> {
    let outcome = reviewer_action(
        control,
        session_id,
        None,
        ReviewerAction::Snapshot {
            repository,
            // Checkouts are namespaced by instance, so this daemon only ever
            // lists, sweeps and removes its own.
            instance: mj_core::config::instance_identity(),
            operation,
        },
    )
    .await;
    match outcome {
        Ok(ReviewerOutcome::Snapshots {
            path,
            present,
            notes,
        }) => Ok(SnapshotAnswer {
            path,
            present,
            notes,
        }),
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

impl SnapshotFailure {
    /// What the requester is told: the review still runs, on a checkout they
    /// can change under it.
    fn notice(&self) -> String {
        let why = match self {
            Self::OldWorker => "the session's worker predates review snapshots".to_owned(),
            Self::Failed(error) => format!("a snapshot could not be created ({error})"),
        };
        format!(
            "Reviewers are reading your live checkout, not a snapshot of the reviewed commit, because {why}. Files you change while the review runs can change what they read."
        )
    }
}

/// A checkout the worker made.
pub(super) struct Created {
    path: PathBuf,
    present: Vec<String>,
    notes: Vec<String>,
}

async fn create_snapshot(
    control: &SessionManagerControl,
    session_id: &str,
    repository: PathBuf,
    id: String,
    commit: String,
) -> Result<Created, SnapshotFailure> {
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
    let answer = snapshot_action(
        control,
        session_id,
        repository,
        SnapshotOperation::Create { id, commit },
    )
    .await
    .map_err(SnapshotFailure::Failed)?;
    let path = answer.path.ok_or_else(|| {
        SnapshotFailure::Failed("the worker returned no snapshot path".to_owned())
    })?;
    Ok(Created {
        path,
        present: answer.present,
        notes: answer.notes,
    })
}

/// A group's checkout, left in place because reviewers of the group are still
/// being closed. Removing the files first would pull them out from under a
/// reviewer that has not stopped yet.
pub(super) struct DeferredRemoval {
    session_id: String,
    repository: PathBuf,
    id: String,
    /// Reviewer children whose close has not completed.
    waiting: BTreeSet<String>,
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
    /// instance's snapshots in the repository are orphans, here in its single
    /// event loop where every live group is known; a worker deciding from a
    /// list sent earlier could remove a group another session opened since.
    pub(super) fn snapshot_created(
        &mut self,
        session_id: String,
        epoch: u64,
        repository: PathBuf,
        id: String,
        result: Result<Created, SnapshotFailure>,
    ) {
        let result = result.inspect(|created| {
            let live: BTreeSet<_> = self
                .reviews
                .values()
                .map(|slot| slot.driver.snapshot().review_id)
                .chain(
                    self.snapshot_removals
                        .iter()
                        .map(|removal| removal.id.clone()),
                )
                .collect();
            // A group that ended while its snapshot was being made is not
            // live, so this also removes the checkout it left behind.
            for orphan in created.present.iter().filter(|name| !live.contains(*name)) {
                self.remove_snapshot(&session_id, repository.clone(), orphan.clone());
            }
        });
        let Some(slot) = self
            .reviews
            .get_mut(&session_id)
            .filter(|slot| slot.epoch == epoch)
        else {
            return;
        };
        match result {
            Ok(created) => {
                if !created.notes.is_empty() {
                    tracing::warn!(
                        session_id,
                        review_id = id,
                        repository = %repository.display(),
                        notes = ?created.notes,
                        "the review's own checkout is missing some submodule contents"
                    );
                }
                // The reviewer prompt names the checkout, not the requester's
                // worktree, which keeps changing while the review runs.
                if let Some(requests) = slot.awaiting_snapshot.as_mut() {
                    let prompt = slot.driver.review_prompt_in(&created.path);
                    for request in requests {
                        if let ReviewRequest::StartReviewer { prompt: text, .. } = request {
                            text.clone_from(&prompt);
                        }
                    }
                }
                slot.snapshot = Some(created.path);
            }
            Err(failure) => {
                match &failure {
                    SnapshotFailure::OldWorker => tracing::warn!(
                        session_id,
                        review_id = id,
                        "the session's worker predates review snapshots; its reviewers read the requester's checkout until the session restarts"
                    ),
                    SnapshotFailure::Failed(error) => tracing::warn!(
                        session_id,
                        review_id = id,
                        repository = %repository.display(),
                        %error,
                        "could not create the review's own checkout; its reviewers read the requester's checkout"
                    ),
                }
                // `accept` hands this to the requester with the start answer.
                slot.checkout_note = Some(failure.notice());
            }
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

    /// Removes a review slot. Every way a review ends comes through here; a
    /// daemon shutdown does not, because the group and its reviewers survive
    /// it. The group's checkout goes with it, but only after the closes of the
    /// group's reviewers have completed: closing a reviewer is what stops its
    /// process, and files must not be deleted under a process that still runs.
    pub(super) fn release_group(&mut self, session_id: &str) {
        let Some(slot) = self.reviews.remove(session_id) else {
            return;
        };
        if slot.snapshot.is_none() {
            return;
        }
        let Some((repository, _)) = source(&slot) else {
            return;
        };
        let waiting: BTreeSet<String> = slot
            .closing_children
            .into_iter()
            .filter(|child| {
                self.child_closure_owners
                    .values()
                    .any(|(closing, _)| closing == child)
            })
            .collect();
        let id = slot.driver.snapshot().review_id;
        if waiting.is_empty() {
            self.remove_snapshot(session_id, repository, id);
        } else {
            self.snapshot_removals.push(DeferredRemoval {
                session_id: session_id.to_owned(),
                repository,
                id,
                waiting,
            });
        }
    }

    /// A reviewer child's close completed. Checkouts that were waiting only
    /// for it can go now.
    pub(super) fn child_close_completed(&mut self, child: &str) {
        for removal in &mut self.snapshot_removals {
            removal.waiting.remove(child);
        }
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.snapshot_removals)
            .into_iter()
            .partition(|removal| removal.waiting.is_empty());
        self.snapshot_removals = waiting;
        for removal in ready {
            self.remove_snapshot(&removal.session_id, removal.repository, removal.id);
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
