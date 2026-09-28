//! Reviewer work participates in the primary relay's atomic replacement gate.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use super::{DurableRelay, RelayExecutionState};

/// The primary relay owns admission. Holding this lease, rather than sampling
/// a role's status later, excludes worker replacement throughout preparation,
/// execution, and cleanup. No lease survives its worker process.
pub(crate) struct ReviewerAdmission {
    relay: Arc<Mutex<DurableRelay>>,
    id: u64,
}

impl ReviewerAdmission {
    pub(crate) fn acquire(relay: Arc<Mutex<DurableRelay>>, label: String) -> Result<Self> {
        let id = {
            let mut owner = relay.lock().expect("relay admission lock poisoned");
            anyhow::ensure!(
                !owner.checkpoint_only
                    && owner.snapshot.execution != RelayExecutionState::Closed
                    && owner.snapshot.checkpoint_barrier.is_none()
                    && !owner.pending_checkpoint_barrier(),
                "worker is reserved for checkpoint or replacement; reviewer work was not admitted"
            );
            let id = owner
                .next_reviewer_admission
                .checked_add(1)
                .context("reviewer admission IDs exhausted")?;
            owner.next_reviewer_admission = id;
            owner.reviewer_admissions.insert(id, label);
            id
        };
        Ok(Self { relay, id })
    }
}

impl Drop for ReviewerAdmission {
    fn drop(&mut self) {
        match self.relay.lock() {
            Ok(mut owner) => {
                owner.reviewer_admissions.remove(&self.id);
            }
            Err(error) => {
                tracing::error!(%error, "could not release reviewer admission after relay failure")
            }
        }
    }
}
