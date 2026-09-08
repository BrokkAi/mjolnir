//! Ordered background saves of the workspaces' requested pane sizes.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use hel::hel_workspace::PaneSizes;
use mj_chat::hel_chat::Notices;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
struct LayoutSnapshot {
    requested: BTreeMap<String, PaneSizes>,
    baseline: BTreeMap<String, PaneSizes>,
}

pub(super) struct PaneSizePersistence {
    requested: BTreeMap<String, PaneSizes>,
    baseline: BTreeMap<String, PaneSizes>,
    sender: Option<watch::Sender<LayoutSnapshot>>,
    task: Option<JoinHandle<Result<()>>>,
    failure: Option<String>,
    notices: Notices,
}

impl PaneSizePersistence {
    pub(super) fn start(initial: BTreeMap<String, PaneSizes>, notices: Notices) -> Self {
        Self::with_save(initial, notices, |workspace_id, sizes| async move {
            crate::daemon::connect_existing()
                .await?
                .save_workspace_pane_sizes(workspace_id, sizes)
                .await
        })
    }

    fn with_save<F, Fut>(
        initial: BTreeMap<String, PaneSizes>,
        notices: Notices,
        mut save: F,
    ) -> Self
    where
        F: FnMut(String, PaneSizes) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send,
    {
        let initial_snapshot = LayoutSnapshot {
            requested: initial.clone(),
            baseline: initial,
        };
        let (sender, mut receiver) = watch::channel(initial_snapshot.clone());
        let reports = notices.clone();
        let task = tokio::spawn(async move {
            let mut persisted = BTreeMap::new();
            let mut failures = BTreeMap::<String, String>::new();
            // The initial snapshot is the database baseline. Newly remembered
            // workspaces add their own baseline before their first update.
            while receiver.changed().await.is_ok() {
                let snapshot = receiver.borrow_and_update().clone();
                // A deleted workspace must not keep an old failed save alive
                // until shutdown. The latest snapshot is authoritative for
                // both retry and final-error reporting.
                persisted.retain(|workspace_id, _| snapshot.requested.contains_key(workspace_id));
                failures.retain(|workspace_id, _| snapshot.requested.contains_key(workspace_id));
                for (workspace_id, sizes) in &snapshot.requested {
                    let baseline = snapshot
                        .baseline
                        .get(workspace_id)
                        .copied()
                        .unwrap_or_default();
                    let previous = persisted.entry(workspace_id.clone()).or_insert(baseline);
                    let retrying = failures.contains_key(workspace_id);
                    if *previous == *sizes && !retrying {
                        continue;
                    }
                    match save(workspace_id.clone(), *sizes).await {
                        Ok(()) => {
                            *previous = *sizes;
                            failures.remove(workspace_id);
                        }
                        Err(error) => {
                            // The workspace may have been deleted while the
                            // save was in flight. Do not report that expected
                            // failure or make shutdown fail in that case.
                            if receiver.borrow().requested.contains_key(workspace_id) {
                                let message = format!(
                                    "Could not save workspace pane sizes for {workspace_id}: {error:#}"
                                );
                                tracing::warn!(%message);
                                reports.set_failure(&message);
                                failures.insert(workspace_id.clone(), message);
                            } else {
                                failures.remove(workspace_id);
                            }
                        }
                    }
                }
            }
            failures
                .into_values()
                .next()
                .map_or(Ok(()), |message| Err(anyhow!(message)))
        });
        Self {
            requested: initial_snapshot.requested,
            baseline: initial_snapshot.baseline,
            sender: Some(sender),
            task: Some(task),
            failure: None,
            notices,
        }
    }

    /// Add a layout loaded for a workspace without treating it as a user
    /// mutation. A duplicate remember is deliberately ignored so a late load
    /// cannot reset a layout the user already changed.
    pub(super) fn remember(&mut self, workspace_id: String, sizes: PaneSizes) {
        if self.requested.contains_key(&workspace_id) {
            return;
        }
        self.requested.insert(workspace_id.clone(), sizes);
        self.baseline.insert(workspace_id, sizes);
        self.publish();
    }

    pub(super) fn forget(&mut self, workspace_id: &str) {
        let requested_removed = self.requested.remove(workspace_id).is_some();
        let baseline_removed = self.baseline.remove(workspace_id).is_some();
        if requested_removed || baseline_removed {
            self.publish();
        }
    }

    pub(super) fn update(&mut self, workspace_id: &str, sizes: PaneSizes) {
        if self.requested.get(workspace_id) == Some(&sizes) {
            return;
        }
        let workspace_id = workspace_id.to_owned();
        // Callers normally remember a lazy-loaded workspace first. A default
        // baseline keeps an unexpected early update deterministic and lets the
        // first non-default choice be persisted rather than discarded.
        self.baseline.entry(workspace_id.clone()).or_default();
        self.requested.insert(workspace_id, sizes);
        self.publish();
    }

    fn publish(&self) {
        let Some(sender) = &self.sender else {
            return;
        };
        let snapshot = LayoutSnapshot {
            requested: self.requested.clone(),
            baseline: self.baseline.clone(),
        };
        if sender.send(snapshot).is_err() {
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

    /// Called after handing back the terminal. Retry unsuccessful latest
    /// choices, drain every workspace's pending change, and bound the flush.
    pub(super) async fn finish(mut self) -> Result<()> {
        if let Some(sender) = self.sender.take() {
            sender.send_replace(LayoutSnapshot {
                requested: std::mem::take(&mut self.requested),
                baseline: std::mem::take(&mut self.baseline),
            });
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

    fn initial(workspace_id: &str) -> BTreeMap<String, PaneSizes> {
        BTreeMap::from([(workspace_id.to_owned(), PaneSizes::default())])
    }

    #[tokio::test]
    async fn opening_and_closing_a_workspace_does_not_write_its_layout() {
        let mut persistence = PaneSizePersistence::with_save(
            initial("workspace-a"),
            Notices::default(),
            |_, _| async { panic!("opening a workspace must not save its layout") },
        );
        persistence.remember("workspace-b".into(), PaneSizes::default());
        persistence.finish().await.unwrap();
    }

    #[tokio::test]
    async fn rapid_changes_are_serialized_and_flush_the_latest_layout() {
        let (started, mut calls) = mpsc::unbounded_channel();
        let mut persistence = PaneSizePersistence::with_save(
            initial("workspace-a"),
            Notices::default(),
            move |workspace_id, sizes| {
                let (complete, completed) = oneshot::channel();
                started.send((workspace_id, sizes, complete)).unwrap();
                async move { completed.await.unwrap() }
            },
        );
        let first = layout(PaneSize::Minimized);
        let middle = layout(PaneSize::Maximized);
        let latest = PaneSizes::default();
        persistence.update("workspace-a", first);
        let (workspace_id, sizes, complete) = calls.recv().await.unwrap();
        assert_eq!(workspace_id, "workspace-a");
        assert_eq!(sizes, first);
        persistence.update("workspace-a", middle);
        persistence.update("workspace-a", latest);
        assert!(calls.try_recv().is_err(), "saves must not overlap");
        let finish = tokio::spawn(persistence.finish());
        complete.send(Ok(())).unwrap();
        let (workspace_id, sizes, complete) = calls.recv().await.unwrap();
        assert_eq!(workspace_id, "workspace-a");
        assert_eq!(sizes, latest);
        complete.send(Ok(())).unwrap();
        finish.await.unwrap().unwrap();
        assert!(calls.recv().await.is_none());
    }

    #[tokio::test]
    async fn rapid_workspace_switches_retain_each_latest_layout() {
        let (started, mut calls) = mpsc::unbounded_channel();
        let mut persistence = PaneSizePersistence::with_save(
            initial("workspace-a"),
            Notices::default(),
            move |workspace_id, sizes| {
                let (complete, completed) = oneshot::channel();
                started.send((workspace_id, sizes, complete)).unwrap();
                async move { completed.await.unwrap() }
            },
        );
        persistence.remember("workspace-b".into(), PaneSizes::default());
        let a_first = layout(PaneSize::Minimized);
        let a_latest = layout(PaneSize::Maximized);
        let b_latest = layout(PaneSize::Minimized);
        persistence.update("workspace-a", a_first);
        let (workspace_id, sizes, complete) = calls.recv().await.unwrap();
        assert_eq!((workspace_id.as_str(), sizes), ("workspace-a", a_first));

        // Both changes arrive while A is still being written. A's newer
        // value and B's value must survive in the one watch snapshot.
        persistence.update("workspace-b", b_latest);
        persistence.update("workspace-a", a_latest);
        assert!(calls.try_recv().is_err(), "saves must not overlap");
        complete.send(Ok(())).unwrap();

        let (workspace_id, sizes, complete) = calls.recv().await.unwrap();
        assert_eq!((workspace_id.as_str(), sizes), ("workspace-a", a_latest));
        complete.send(Ok(())).unwrap();
        let (workspace_id, sizes, complete) = calls.recv().await.unwrap();
        assert_eq!((workspace_id.as_str(), sizes), ("workspace-b", b_latest));
        complete.send(Ok(())).unwrap();
        persistence.finish().await.unwrap();
        assert!(calls.recv().await.is_none());
    }

    #[tokio::test]
    async fn failed_saves_are_reported_and_the_latest_choice_is_retried_on_exit() {
        let notices = Notices::default();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let saves = attempts.clone();
        let (failed, failure) = oneshot::channel();
        let mut failed = Some(failed);
        let mut persistence = PaneSizePersistence::with_save(
            initial("workspace-a"),
            notices.clone(),
            move |workspace_id, sizes| {
                saves.lock().unwrap().push((workspace_id, sizes));
                let first = failed.take();
                async move {
                    if let Some(failed) = first {
                        failed.send(()).unwrap();
                        anyhow::bail!("database unavailable");
                    }
                    Ok(())
                }
            },
        );
        let desired = layout(PaneSize::Minimized);
        persistence.update("workspace-a", desired);
        failure.await.unwrap();
        assert!(notices.current().unwrap().contains("database unavailable"));
        persistence.finish().await.unwrap();
        assert_eq!(
            *attempts.lock().unwrap(),
            [
                ("workspace-a".to_owned(), desired),
                ("workspace-a".to_owned(), desired)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_save_bounds_exit_and_is_cancelled() {
        let (started, began) = oneshot::channel();
        let (cancelled, cancellation) = oneshot::channel::<()>();
        let mut started = Some(started);
        let mut cancelled = Some(cancelled);
        let mut persistence = PaneSizePersistence::with_save(
            initial("workspace-a"),
            Notices::default(),
            move |_, _| {
                let started = started.take().unwrap();
                let cancelled = cancelled.take().unwrap();
                async move {
                    let _cancelled = cancelled;
                    started.send(()).unwrap();
                    std::future::pending::<Result<()>>().await
                }
            },
        );
        persistence.update("workspace-a", layout(PaneSize::Minimized));
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
            PaneSizePersistence::with_save(initial("workspace-a"), notices.clone(), |_, _| async {
                anyhow::bail!("workspace was deleted")
            });
        persistence.update("workspace-a", layout(PaneSize::Minimized));
        let error = persistence.finish().await.unwrap_err();
        assert!(error.to_string().contains("workspace was deleted"));
        assert!(notices.current().unwrap().contains("workspace was deleted"));
    }

    #[tokio::test]
    async fn a_panicking_save_is_supervised_and_reported_on_exit() {
        let notices = Notices::default();
        let mut persistence =
            PaneSizePersistence::with_save(initial("workspace-a"), notices.clone(), |_, _| async {
                panic!("save panic")
            });
        persistence.update("workspace-a", layout(PaneSize::Minimized));
        persistence.wait().await;
        assert!(!persistence.is_running());
        assert!(notices.current().unwrap().contains("save task failed"));
        assert!(persistence.finish().await.is_err());
    }

    #[tokio::test]
    async fn forgetting_workspace_while_save_fails_does_not_fail_finish() {
        let notices = Notices::default();
        let (started, began) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let mut started = Some(started);
        let mut released = Some(released);
        let mut persistence =
            PaneSizePersistence::with_save(initial("workspace-a"), notices.clone(), move |_, _| {
                let started = started.take().unwrap();
                let released = released.take().unwrap();
                async move {
                    started.send(()).unwrap();
                    released.await.unwrap();
                    anyhow::bail!("workspace was deleted")
                }
            });
        persistence.update("workspace-a", layout(PaneSize::Minimized));
        began.await.unwrap();
        persistence.forget("workspace-a");
        release.send(()).unwrap();
        persistence.finish().await.unwrap();
        assert!(notices.current().is_none());
    }
}
