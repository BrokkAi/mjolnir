use super::*;
use mj_controller::project_picker::{ProjectDiscovery, ProjectDiscoveryRequest};
use tokio::sync::OwnedSemaphorePermit;

pub(super) async fn discover(
    slot: OwnedSemaphorePermit,
    request: ProjectDiscoveryRequest,
) -> Result<ProjectDiscovery, &'static str> {
    discover_with(
        slot,
        request,
        mj_controller::upgrade::gate(),
        |request, cancelled| {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(15));
            mj_controller::project_picker::discover(&request, &executor)
                .map_err(|error| mj_controller::project_picker::error_message(&error))
        },
    )
    .await
}

async fn discover_with(
    slot: OwnedSemaphorePermit,
    request: ProjectDiscoveryRequest,
    gate: &Arc<mj_controller::upgrade::Gate>,
    discover: impl FnOnce(
        ProjectDiscoveryRequest,
        Arc<AtomicBool>,
    ) -> Result<ProjectDiscovery, &'static str>
    + Send
    + 'static,
) -> Result<ProjectDiscovery, &'static str> {
    let admission = gate
        .enter("project discovery")
        .map_err(|_| "Mjolnir is completing an upgrade. Retry shortly.")?;
    preflight::cancellable_blocking(slot, move |cancelled| {
        let _admission = admission;
        discover(request, cancelled)
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "project discovery task failed");
        Err("Project discovery failed. Retry the request.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn project_discovery_cancels_work_and_keeps_upgrade_admission_until_it_exits() {
        for stop in ["disconnect", "shutdown", "abort"] {
            let gate = Arc::new(mj_controller::upgrade::Gate::default());
            let termination = tokio_util::sync::CancellationToken::new();
            let slots = Arc::new(Semaphore::new(1));
            let slot = slots.clone().try_acquire_owned().unwrap();
            let (disconnect, disconnected) = tokio::sync::oneshot::channel::<()>();
            let (started, start) = tokio::sync::oneshot::channel();
            let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let request_gate = gate.clone();
            let shutdown = termination.clone();
            let job = mj_core::runtime::spawn(async move {
                let discovery = discover_with(
                    slot,
                    ProjectDiscoveryRequest::Github {
                        query: String::new(),
                    },
                    &request_gate,
                    move |_, cancelled| {
                        started.send(()).unwrap();
                        let deadline = std::time::Instant::now() + Duration::from_secs(3);
                        while !cancelled.load(Ordering::Acquire) {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "work was not cancelled"
                            );
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        cancelled_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                        Err("cancelled")
                    },
                );
                tokio::select! {
                    _ = discovery => panic!("discovery completed before cancellation"),
                    _ = disconnected => {},
                    _ = shutdown.cancelled() => {},
                }
            });
            tokio::time::timeout(Duration::from_secs(3), start)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(gate.active_labels(), ["project discovery"]);
            assert!(!gate.try_close());
            match stop {
                "disconnect" => drop(disconnect),
                "shutdown" => termination.cancel(),
                "abort" => job.abort(),
                _ => unreachable!(),
            }
            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .unwrap()
                .unwrap();
            assert!(
                !gate.try_close(),
                "{stop} released upgrade ownership before blocking work exited"
            );
            assert_eq!(slots.available_permits(), 0);
            release_tx.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), job)
                .await
                .unwrap();
            if stop == "abort" {
                assert!(result.unwrap_err().is_cancelled());
            } else {
                result.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                while !gate.try_close() || slots.available_permits() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn project_discovery_does_not_start_abandoned_or_upgrade_rejected_requests() {
        for closed in [false, true] {
            let gate = Arc::new(mj_controller::upgrade::Gate::default());
            let slot = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
            if !closed {
                assert!(gate.try_close());
            }
            let discovery = discover_with(
                slot,
                ProjectDiscoveryRequest::Github {
                    query: String::new(),
                },
                &gate,
                |_, _| panic!("request must not start"),
            );
            if closed {
                drop(discovery);
                assert!(gate.active_labels().is_empty());
            } else {
                assert!(discovery.await.unwrap_err().contains("upgrade"));
            }
        }
    }
}
