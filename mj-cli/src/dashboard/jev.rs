//! One cancellable inspector read, owned by the dashboard.
use super::io::{DashboardIoUpdate, report};
use tokio_util::task::AbortOnDropHandle;
#[derive(Default)]
pub(super) struct JevInspector {
    generation: Option<u64>,
    task: Option<AbortOnDropHandle<()>>,
}
impl JevInspector {
    pub(super) fn sync(
        &mut self,
        dashboard: &mj_tui::DashboardState,
        updates: &tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    ) {
        let request = dashboard.jev_request();
        let generation = request.as_ref().map(|r| r.0);
        if generation == self.generation {
            return;
        }
        self.task = None;
        self.generation = generation;
        if let Some((generation, session, id)) = request {
            let updates = updates.clone();
            self.task = Some(AbortOnDropHandle::new(tokio::spawn(async move {
                let child = AbortOnDropHandle::new(tokio::spawn(async move {
                    let mut client = crate::daemon::connect_existing().await?;
                    client.jev_decisions(session, id).await
                }));
                let result =
                    match tokio::time::timeout(std::time::Duration::from_secs(20), child).await {
                        Ok(Ok(result)) => result.map_err(|e| format!("{e:#}")),
                        Ok(Err(error)) => Err(format!("Jev diagnostic task failed: {error}")),
                        Err(_) => Err("Jev diagnostic read timed out.".into()),
                    };
                report(
                    "Jev inspector",
                    &updates,
                    DashboardIoUpdate::JevDecisions { generation, result },
                );
            })));
        }
    }
}
