//! Ordered background saves of the workspace's requested pane sizes.

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use hel::hel_workspace::PaneSizes;
use mj_chat::hel_chat::Notices;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct PaneSizePersistence {
    requested: PaneSizes,
    sender: Option<watch::Sender<PaneSizes>>,
    task: Option<JoinHandle<Result<()>>>,
    failure: Option<String>,
    notices: Notices,
}

impl PaneSizePersistence {
    pub(super) fn start(workspace_id: String, initial: PaneSizes, notices: Notices) -> Self {
        Self::with_save(initial, notices, move |sizes| {
            let workspace_id = workspace_id.clone();
            async move {
                crate::daemon::connect_existing()
                    .await?
                    .save_workspace_pane_sizes(workspace_id, sizes)
                    .await
            }
        })
    }

    fn with_save<F, Fut>(initial: PaneSizes, notices: Notices, mut save: F) -> Self
    where
        F: FnMut(PaneSizes) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let (sender, mut receiver) = watch::channel(initial);
        let reports = notices.clone();
        let task = tokio::spawn(async move {
            let mut persisted = initial;
            let mut failure = None;
            // Watch retains only the latest pending layout while a save runs.
            // Closing the sender still drains its last unseen value.
            while receiver.changed().await.is_ok() {
                let sizes = *receiver.borrow_and_update();
                if sizes == persisted && failure.is_none() {
                    continue;
                }
                match save(sizes).await {
                    Ok(()) => {
                        persisted = sizes;
                        failure = None;
                    }
                    Err(error) => {
                        let message = format!("Could not save workspace pane sizes: {error:#}");
                        tracing::warn!(%message);
                        reports.set_failure(&message);
                        failure = Some(message);
                    }
                }
            }
            failure.map_or(Ok(()), |message| Err(anyhow!(message)))
        });
        Self {
            requested: initial,
            sender: Some(sender),
            task: Some(task),
            failure: None,
            notices,
        }
    }

    pub(super) fn update(&mut self, sizes: PaneSizes) {
        if sizes == self.requested {
            return;
        }
        self.requested = sizes;
        if let Some(sender) = &self.sender
            && sender.send(sizes).is_err()
        {
            self.notices
                .set_failure("Could not save workspace pane sizes: background saver stopped");
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.task.is_some()
    }

    /// Supervise panics as well as returned errors. Awaiting a JoinHandle by
    /// reference is cancellation-safe in the dashboard's select loop.
    pub(super) async fn wait(&mut self) {
        let Some(task) = self.task.as_mut() else {
            std::future::pending::<()>().await;
            return;
        };
        let result = match task.await {
            Ok(result) => result,
            Err(error) => Err(anyhow!("workspace pane-size save task failed: {error}")),
        };
        self.task = None;
        self.failure = result.err().map(|error| format!("{error:#}"));
        if let Some(message) = &self.failure {
            tracing::warn!(%message);
            self.notices.set_failure(message);
        }
    }

    /// Called after handing back the terminal. Retry an unsuccessful latest
    /// choice once, drain pending changes, and bound the entire final flush.
    pub(super) async fn finish(mut self) -> Result<()> {
        if let Some(sender) = self.sender.take() {
            sender.send_replace(self.requested);
            drop(sender);
        }
        if self.is_running()
            && tokio::time::timeout(FLUSH_TIMEOUT, self.wait())
                .await
                .is_err()
        {
            return Err(anyhow!(
                "Timed out saving workspace pane sizes after {} seconds; the latest layout may not have been saved",
                FLUSH_TIMEOUT.as_secs()
            ));
        }
        self.failure
            .take()
            .map_or(Ok(()), |error| Err(anyhow!(error)))
    }
}

impl Drop for PaneSizePersistence {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_workspace::PaneSize;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{mpsc, oneshot};

    fn layout(sessions: PaneSize) -> PaneSizes {
        PaneSizes {
            sessions,
            ..PaneSizes::default()
        }
    }

    #[tokio::test]
    async fn opening_and_closing_a_workspace_does_not_write_its_layout() {
        let persistence =
            PaneSizePersistence::with_save(PaneSizes::default(), Notices::default(), |_| async {
                panic!("opening a workspace must not save its layout")
            });
        persistence.finish().await.unwrap();
    }

    #[tokio::test]
    async fn rapid_changes_are_serialized_and_flush_the_latest_layout() {
        let (started, mut calls) = mpsc::unbounded_channel();
        let mut persistence = PaneSizePersistence::with_save(
            PaneSizes::default(),
            Notices::default(),
            move |sizes| {
                let (complete, completed) = oneshot::channel();
                started.send((sizes, complete)).unwrap();
                async move { completed.await.unwrap() }
            },
        );
        let first = layout(PaneSize::Minimized);
        let middle = layout(PaneSize::Maximized);
        let latest = PaneSizes::default();
        persistence.update(first);
        let (sizes, complete) = calls.recv().await.unwrap();
        assert_eq!(sizes, first);
        persistence.update(middle);
        persistence.update(latest);
        assert!(calls.try_recv().is_err(), "saves must not overlap");
        let finish = tokio::spawn(persistence.finish());
        complete.send(Ok(())).unwrap();
        let (sizes, complete) = calls.recv().await.unwrap();
        assert_eq!(sizes, latest);
        complete.send(Ok(())).unwrap();
        finish.await.unwrap().unwrap();
        assert!(calls.recv().await.is_none());
    }

    #[tokio::test]
    async fn failed_saves_are_reported_and_the_latest_choice_is_retried_on_exit() {
        let notices = Notices::default();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let saves = attempts.clone();
        let (failed, failure) = oneshot::channel();
        let mut failed = Some(failed);
        let mut persistence =
            PaneSizePersistence::with_save(PaneSizes::default(), notices.clone(), move |sizes| {
                saves.lock().unwrap().push(sizes);
                let first = failed.take();
                async move {
                    if let Some(failed) = first {
                        failed.send(()).unwrap();
                        anyhow::bail!("database unavailable");
                    }
                    Ok(())
                }
            });
        let desired = layout(PaneSize::Minimized);
        persistence.update(desired);
        failure.await.unwrap();
        assert!(notices.current().unwrap().contains("database unavailable"));
        persistence.finish().await.unwrap();
        assert_eq!(*attempts.lock().unwrap(), [desired, desired]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_save_bounds_exit_and_is_cancelled() {
        let (started, began) = oneshot::channel();
        let (cancelled, cancellation) = oneshot::channel::<()>();
        let mut started = Some(started);
        let mut cancelled = Some(cancelled);
        let mut persistence =
            PaneSizePersistence::with_save(PaneSizes::default(), Notices::default(), move |_| {
                let started = started.take().unwrap();
                let cancelled = cancelled.take().unwrap();
                async move {
                    let _cancelled = cancelled;
                    started.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                }
            });
        persistence.update(layout(PaneSize::Minimized));
        began.await.unwrap();
        let before = tokio::time::Instant::now();
        let error = persistence.finish().await.unwrap_err();
        assert_eq!(before.elapsed(), FLUSH_TIMEOUT);
        assert!(error.to_string().contains("Timed out"));
        assert!(
            cancellation.await.is_err(),
            "the save future must be dropped"
        );
    }

    #[tokio::test]
    async fn an_unsuccessful_final_save_is_returned_after_reporting_the_failure() {
        let notices = Notices::default();
        let mut persistence =
            PaneSizePersistence::with_save(PaneSizes::default(), notices.clone(), |_| async {
                anyhow::bail!("workspace was deleted")
            });
        persistence.update(layout(PaneSize::Minimized));
        let error = persistence.finish().await.unwrap_err();
        assert!(error.to_string().contains("workspace was deleted"));
        assert!(notices.current().unwrap().contains("workspace was deleted"));
    }

    #[tokio::test]
    async fn a_panicking_save_is_supervised_and_reported_on_exit() {
        let notices = Notices::default();
        let mut persistence =
            PaneSizePersistence::with_save(PaneSizes::default(), notices.clone(), |_| async {
                panic!("save panic")
            });
        persistence.update(layout(PaneSize::Minimized));
        persistence.wait().await;
        assert!(!persistence.is_running());
        assert!(notices.current().unwrap().contains("save task failed"));
        assert!(persistence.finish().await.is_err());
    }
}
