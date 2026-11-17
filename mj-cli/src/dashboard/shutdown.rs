use std::future::Future;
use std::time::Instant;

pub(crate) struct Timed<T> {
    pub(crate) started_at: Instant,
    pub(crate) finished_at: Instant,
    pub(crate) output: T,
}

async fn timed<F: Future>(future: F) -> Timed<F::Output> {
    let started_at = Instant::now();
    let output = future.await;
    Timed {
        started_at,
        finished_at: Instant::now(),
        output,
    }
}

/// Run independent final persistence and manager shutdown together, awaiting
/// both even when either one returns an error.
pub(crate) async fn overlap_final_cleanup<S, M, T>(
    saves: S,
    manager: M,
) -> (Timed<T>, Timed<anyhow::Result<()>>)
where
    S: Future<Output = T>,
    M: Future<Output = anyhow::Result<()>>,
{
    tokio::join!(timed(saves), timed(manager))
}

#[cfg(test)]
mod tests {
    use super::overlap_final_cleanup;
    use anyhow::bail;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn final_cleanup_overlaps_and_manager_error_does_not_cancel_saves() {
        let (saves_started_tx, saves_started_rx) = oneshot::channel();
        let (manager_started_tx, manager_started_rx) = oneshot::channel();
        let (release_saves_tx, release_saves_rx) = oneshot::channel();
        let (release_manager_tx, release_manager_rx) = oneshot::channel();

        let cleanup = tokio::spawn(overlap_final_cleanup(
            async move {
                saves_started_tx.send(()).unwrap();
                release_saves_rx.await.unwrap();
                Ok::<(), anyhow::Error>(())
            },
            async move {
                manager_started_tx.send(()).unwrap();
                release_manager_rx.await.unwrap();
                bail!("manager shutdown failed")
            },
        ));

        saves_started_rx.await.unwrap();
        manager_started_rx.await.unwrap();
        release_manager_tx.send(()).unwrap();
        assert!(!cleanup.is_finished());
        release_saves_tx.send(()).unwrap();

        let (saves, manager) = cleanup.await.unwrap();
        saves.output.unwrap();
        assert!(
            manager
                .output
                .unwrap_err()
                .to_string()
                .contains("manager shutdown failed")
        );
        assert!(saves.started_at <= manager.finished_at);
        assert!(manager.started_at <= saves.finished_at);
    }
}
