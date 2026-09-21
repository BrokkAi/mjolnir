//! Read-only diagnostics use a separate connection, never a projection attachment.
use super::*;

impl RuntimeState {
    pub(crate) async fn jev_decisions(
        self: Arc<Self>,
        session: String,
        decision_id: Option<String>,
    ) -> Result<mj_core::jev::DecisionPage> {
        ensure!(self.session_record(&session).is_some(), "unknown session");
        let local_session = session.clone();
        let local_id = decision_id.clone();
        let local = tokio::task::spawn_blocking(move || {
            mj_core::jev::read(
                &mj_core::jev::controller_log_dir(),
                &local_session,
                local_id.as_deref(),
            )
        });
        let remote = async {
            let state = self.clone();
            let target_session = session.clone();
            let spec = tokio::task::spawn_blocking(move || {
                state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .reconnect_command(&target_session)
            })
            .await
            .context("resolve worker diagnostic target")??;
            let mut client = crate::worker_client::RelayClient::connect(&spec, &session).await?;
            client.jev_decisions(decision_id).await
        };
        let (local, remote) =
            tokio::join!(local, tokio::time::timeout(Duration::from_secs(15), remote));
        let mut page = match local.context("read daemon Jev diagnostics").and_then(|r| r) {
            Ok(page) => page,
            Err(error) => mj_core::jev::DecisionPage {
                decisions: Vec::new(),
                warnings: vec![format!("Daemon details unavailable: {error:#}")],
            },
        };
        match remote
            .context("worker diagnostic read timed out")
            .and_then(|r| r)
        {
            Ok(remote) => page.merge(remote),
            Err(error) => page
                .warnings
                .push(format!("Worker details unavailable: {error:#}")),
        }
        Ok(page)
    }
}
