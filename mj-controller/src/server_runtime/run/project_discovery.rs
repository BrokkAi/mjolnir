use super::*;

/// The upgrade permit also lives on the blocking task: aborting its async
/// supervisor must not announce idle before cancelled subprocesses exit.
pub(super) fn spawn(
    jobs: &mut tokio::task::JoinSet<()>,
    request: crate::server::ProjectDiscoveryPreflight,
    termination: &tokio_util::sync::CancellationToken,
) {
    spawn_with(
        jobs,
        request,
        termination,
        crate::upgrade::gate(),
        |request, cancelled| {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(15));
            crate::project_picker::discover(&request, &executor)
                .map_err(|error| crate::project_picker::error_message(&error))
        },
    );
}

fn spawn_with(
    jobs: &mut tokio::task::JoinSet<()>,
    request: crate::server::ProjectDiscoveryPreflight,
    termination: &tokio_util::sync::CancellationToken,
    gate: &Arc<crate::upgrade::Gate>,
    discover: impl FnOnce(
        crate::project_picker::ProjectDiscoveryRequest,
        Arc<AtomicBool>,
    ) -> Result<crate::project_picker::ProjectDiscovery, &'static str>
    + Send
    + 'static,
) {
    let crate::server::ProjectDiscoveryPreflight { request, mut reply } = request;
    if reply.is_closed() {
        return;
    }
    let Ok(upgrade_task) = gate.enter("project discovery") else {
        if reply
            .send(Err("Mjolnir is completing an upgrade. Retry shortly."))
            .is_err()
        {
            tracing::debug!("project discovery reply dropped after client disconnect");
        }
        return;
    };
    let termination = termination.clone();
    jobs.spawn(async move {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_guard = ProcessCancellationGuard(cancelled.clone());
        let blocking_upgrade_task = upgrade_task.clone();
        let mut blocking = tokio::task::spawn_blocking(move || {
            let _upgrade_task = blocking_upgrade_task;
            discover(request, cancelled)
        });
        let answer = tokio::select! {
            biased;
            _ = termination.cancelled() => None,
            _ = reply.closed() => None,
            answer = &mut blocking => Some(answer),
        };
        let Some(answer) = answer else {
            drop(cancellation_guard);
            if blocking.await.is_err() {
                tracing::warn!("cancelled project discovery task failed");
            }
            return;
        };
        let answer = answer.unwrap_or_else(|_| {
            tracing::warn!("project discovery task failed");
            Err("Project discovery failed. Retry the request.")
        });
        if reply.send(answer).is_err() {
            tracing::debug!("project discovery reply dropped after client disconnect");
        }
        drop(upgrade_task);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_picker::ProjectDiscoveryRequest;
    use crate::server::ProjectDiscoveryPreflight;

    #[tokio::test]
    async fn project_discovery_cancels_work_and_keeps_upgrade_admission_until_it_exits() {
        for stop in ["disconnect", "shutdown", "abort"] {
            let gate = Arc::new(crate::upgrade::Gate::default());
            let termination = tokio_util::sync::CancellationToken::new();
            let mut jobs = tokio::task::JoinSet::new();
            let (reply, response) = tokio::sync::oneshot::channel();
            let (started, start) = tokio::sync::oneshot::channel();
            let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            spawn_with(
                &mut jobs,
                ProjectDiscoveryPreflight {
                    request: ProjectDiscoveryRequest::Github {
                        query: String::new(),
                    },
                    reply,
                },
                &termination,
                &gate,
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
            tokio::time::timeout(Duration::from_secs(3), start)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(gate.active_labels(), ["project discovery x2"]);
            assert!(!gate.try_close());
            match stop {
                "disconnect" => drop(response),
                "shutdown" => termination.cancel(),
                "abort" => jobs.abort_all(),
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
            release_tx.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), jobs.join_next())
                .await
                .unwrap()
                .unwrap();
            if stop == "abort" {
                assert!(result.unwrap_err().is_cancelled());
                // An aborted supervisor cannot join its blocking task, whose
                // own permit must still prevent handoff until it has exited.
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !gate.try_close() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            } else {
                result.unwrap();
                assert!(gate.try_close());
            }
        }
    }

    #[tokio::test]
    async fn project_discovery_does_not_start_abandoned_or_upgrade_rejected_requests() {
        for closed in [false, true] {
            let gate = Arc::new(crate::upgrade::Gate::default());
            let mut jobs = tokio::task::JoinSet::new();
            let (reply, mut response) = tokio::sync::oneshot::channel();
            if closed {
                response.close();
            } else {
                assert!(gate.try_close());
            }
            spawn_with(
                &mut jobs,
                ProjectDiscoveryPreflight {
                    request: ProjectDiscoveryRequest::Github {
                        query: String::new(),
                    },
                    reply,
                },
                &tokio_util::sync::CancellationToken::new(),
                &gate,
                |_, _| panic!("request must not start"),
            );
            assert!(jobs.is_empty());
            if !closed {
                assert!(response.await.unwrap().unwrap_err().contains("upgrade"));
            }
        }
    }
}
