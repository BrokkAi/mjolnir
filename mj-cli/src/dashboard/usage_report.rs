//! One cancellable report reader, replaced when the modal or filters change.
use super::io::{DashboardIoUpdate, report};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::task::AbortOnDropHandle;

#[derive(Default)]
pub(super) struct UsageReportLoader {
    generation: Option<u64>,
    task: Option<AbortOnDropHandle<()>>,
}

impl UsageReportLoader {
    pub(super) fn sync(
        &mut self,
        dashboard: &mj_tui::DashboardState,
        updates: &UnboundedSender<DashboardIoUpdate>,
    ) {
        let request = dashboard.session_usage_request();
        if self.generation == request.as_ref().map(|r| r.0) {
            return;
        }
        self.cancel();
        let Some((generation, query)) = request else {
            return;
        };
        self.generation = Some(generation);
        let updates = updates.clone();
        self.task = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                let query = query.clone();
                let child = AbortOnDropHandle::new(tokio::spawn(async move {
                    crate::api_client::ApiClient::connect()
                        .await?
                        .usage_report(&query)
                        .await
                }));
                let result =
                    match tokio::time::timeout(std::time::Duration::from_secs(20), child).await {
                        Ok(Ok(result)) => result.map_err(|error| format!("{error:#}")),
                        Ok(Err(error)) => Err(format!("usage reader failed: {error}")),
                        Err(_) => Err("usage request timed out".into()),
                    };
                let failed = result.is_err();
                if let Err(error) = &result {
                    tracing::warn!(%error, "usage report unavailable");
                }
                report(
                    "session usage",
                    &updates,
                    DashboardIoUpdate::SessionUsageFinished { generation, result },
                );
                // Errors wait for an explicit Refresh; successful reads are serial.
                if failed {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        })));
    }

    pub(super) fn cancel(&mut self) {
        self.task = None;
        self.generation = None;
    }
}
