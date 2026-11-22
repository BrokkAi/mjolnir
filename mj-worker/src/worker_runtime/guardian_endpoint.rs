//! Pending permission reviews belong to the worker, across relay connections.

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::{Context, Result, ensure};
use mj_core::guardian::{
    GuardianReviewRequest, GuardianReviewResult, MAX_PENDING_GUARDIAN_REVIEWS,
};
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::time::Instant;

#[cfg_attr(
    not(any(unix, test)),
    allow(
        dead_code,
        reason = "only a Unix worker serves the live guardian relay"
    )
)]
struct Pending {
    request: GuardianReviewRequest,
    expires: Instant,
    reply: oneshot::Sender<GuardianReviewResult>,
}

pub(crate) struct GuardianEndpoint {
    pending: Mutex<BTreeMap<String, Pending>>,
    permits: Semaphore,
    changes: watch::Sender<u64>,
}

impl Default for GuardianEndpoint {
    fn default() -> Self {
        Self {
            pending: Mutex::default(),
            permits: Semaphore::new(MAX_PENDING_GUARDIAN_REVIEWS),
            changes: watch::Sender::new(0),
        }
    }
}

impl GuardianEndpoint {
    #[cfg(unix)]
    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    #[cfg(any(unix, test))]
    pub(crate) fn requests(&self) -> Vec<GuardianReviewRequest> {
        let now = Instant::now();
        self.pending
            .lock()
            .expect("guardian queue poisoned")
            .values()
            .filter(|pending| pending.expires > now && !pending.reply.is_closed())
            .map(|pending| pending.request.clone())
            .collect()
    }

    #[cfg(any(unix, test))]
    pub(crate) fn complete(&self, result: GuardianReviewResult) {
        let pending = self
            .pending
            .lock()
            .expect("guardian queue poisoned")
            .remove(&result.request_id);
        if let Some(pending) = pending
            && pending.expires > Instant::now()
            && pending.reply.send(result).is_err()
        {
            tracing::debug!("guardian permission request was withdrawn before its response");
        }
    }

    pub(crate) async fn review(
        &self,
        mut request: GuardianReviewRequest,
        deadline: Instant,
    ) -> Result<GuardianReviewResult> {
        let timeout = "the daemon guardian did not return a verdict before the relay deadline";
        ensure!(Instant::now() < deadline, "{timeout}");
        let _permit = self
            .permits
            .try_acquire()
            .context("too many pending guardian reviews")?;
        let id = mj_core::state::new_session_id()?;
        tracing::debug!(
            target: "mj_guardian",
            request_id = %id,
            tool_call_id = %request.tool_call_id,
            "queued a guardian request for the daemon"
        );
        request.request_id = id.clone();
        let (reply, received) = oneshot::channel();
        self.pending
            .lock()
            .expect("guardian queue poisoned")
            .insert(
                id.clone(),
                Pending {
                    request,
                    expires: deadline,
                    reply,
                },
            );
        let _cleanup = RemovePending(self, id);
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        tokio::time::timeout_at(deadline, received)
            .await
            .context(timeout)?
            .context("the guardian response channel closed")
    }
}

struct RemovePending<'a>(&'a GuardianEndpoint, String);

impl Drop for RemovePending<'_> {
    fn drop(&mut self) {
        self.0
            .pending
            .lock()
            .expect("guardian queue poisoned")
            .remove(&self.1);
    }
}
