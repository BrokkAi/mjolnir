//! One cancellable attachment per selection; failed requests require a retry.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;

pub(super) const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct SessionAttachment {
    selection: Option<String>,
    generation: u64,
    task: Option<JoinHandle<()>>,
}

impl SessionAttachment {
    /// Observe selection even when its chat is already warm. Failed/cancelled
    /// opens stay observed so render/feed ticks cannot retry them indefinitely.
    pub(super) fn select(&mut self, session: &str) -> bool {
        if self.selection.as_deref() == Some(session) {
            return false;
        }
        self.selection = Some(session.to_owned());
        true
    }

    pub(super) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// A lifecycle owns the selected conversation temporarily. Unlike a
    /// failed/cancelled open, its completion should allow one fresh attempt.
    pub(super) fn defer(&mut self) {
        if self.selection.take().is_some() || self.task.is_some() {
            self.cancel();
        }
    }

    pub(super) fn accepts(&self, generation: u64, selection: Option<&str>) -> bool {
        // Input may have changed the row before the event loop starts its new
        // attachment. A queued completion must already respect that choice.
        self.generation == generation && self.selection.as_deref() == selection
    }

    pub(super) fn spawn<T: Send + 'static>(
        &mut self,
        session: &str,
        timeout: Duration,
        work: impl Future<Output = Result<T, String>> + Send + 'static,
        report: impl FnOnce(u64, Result<T, String>) + Send + 'static,
    ) {
        self.cancel();
        self.select(session);
        let generation = self.generation;
        self.task = Some(tokio::spawn(async move {
            let result = tokio::time::timeout(
                timeout,
                tokio_util::task::AbortOnDropHandle::new(tokio::spawn(work)),
            )
            .await;
            let result = match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(format!("session opening task failed: {error}")),
                Err(_) => Err(format!(
                    "Session opening did not respond within {} seconds",
                    timeout.as_secs()
                )),
            };
            report(generation, result);
        }));
    }
}

impl Drop for SessionAttachment {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    #[tokio::test]
    async fn a_transition_retires_an_attach_and_rearms_the_same_selection_once() {
        let mut attachment = SessionAttachment::default();
        let (tx, rx) = oneshot::channel();
        attachment.spawn(
            "moving",
            ATTACH_TIMEOUT,
            async { Ok(()) },
            move |generation, _| {
                tx.send(generation).unwrap();
            },
        );
        let generation = rx.await.unwrap();
        attachment.defer();
        attachment.defer();
        assert!(!attachment.accepts(generation, Some("moving")));
        assert!(attachment.select("moving"));
        assert!(!attachment.select("moving"));
    }

    #[tokio::test]
    async fn failed_open_does_not_retry_on_background_wakeups_but_can_be_retried_explicitly() {
        let mut attachment = SessionAttachment::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(attachment.select("muse"));
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async { Err::<(), _>("not managed".into()) },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (failed_generation, result) = rx.recv().await.unwrap();
        assert!(result.is_err());
        for _ in 0..1024 {
            assert!(!attachment.select("muse"));
        }
        let (tx, mut rx) = mpsc::unbounded_channel();
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async { Ok(()) },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (generation, result) = rx.recv().await.unwrap();
        assert!(result.is_ok());
        assert!(attachment.accepts(generation, Some("muse")));
        // A selection event can precede the next call to select/spawn.
        assert!(!attachment.accepts(generation, Some("codex")));
        assert!(!attachment.accepts(failed_generation, Some("muse")));
    }

    #[tokio::test]
    async fn switching_sessions_aborts_hung_open_and_opens_the_new_selection() {
        let mut attachment = SessionAttachment::default();
        let (started, started_rx) = oneshot::channel();
        let (held, mut released) = mpsc::channel::<()>(1);
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async move {
                let _held = held;
                started.send(()).unwrap();
                std::future::pending::<Result<(), String>>().await
            },
            |_, _| panic!("cancelled attempt must not report"),
        );
        started_rx.await.unwrap();
        let (tx, rx) = oneshot::channel();
        assert!(attachment.select("codex"));
        attachment.spawn(
            "codex",
            ATTACH_TIMEOUT,
            async { Ok(()) },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (generation, result) = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(attachment.accepts(generation, Some("codex")));
        assert!(result.is_ok());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), released.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn timeout_drops_hung_work_and_cancel_does_not_restart_the_same_selection() {
        let mut attachment = SessionAttachment::default();
        let (tx, rx) = oneshot::channel();
        let (held, mut released) = mpsc::channel::<()>(1);
        attachment.spawn(
            "muse",
            Duration::from_millis(20),
            async move {
                let _held = held;
                std::future::pending::<Result<(), String>>().await
            },
            move |generation, result| {
                tx.send((generation, result)).unwrap();
            },
        );
        let (generation, result) = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().contains("did not respond"));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), released.recv())
                .await
                .unwrap()
                .is_none()
        );
        attachment.cancel();
        assert!(!attachment.accepts(generation, Some("muse")));
        assert!(!attachment.select("muse"));
        assert!(attachment.select("codex"));
        assert!(attachment.select("muse"));
    }

    #[tokio::test]
    async fn panicked_open_reports_failure_instead_of_leaving_the_spinner_running() {
        let mut attachment = SessionAttachment::default();
        let (tx, rx) = oneshot::channel();
        attachment.spawn(
            "muse",
            ATTACH_TIMEOUT,
            async {
                panic!("fake preparation panic");
                #[allow(unreachable_code)]
                Ok(())
            },
            move |_, result| {
                tx.send(result).unwrap();
            },
        );
        assert!(rx.await.unwrap().unwrap_err().contains("task failed"));
    }
}
