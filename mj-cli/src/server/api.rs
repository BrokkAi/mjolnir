//! The daemon half of the documented HTTP API.
//!
//! `mj-controller` serves the `/api/v1` routes but cannot reach the daemon's
//! live session actors or its SQLite store, so it declares the
//! [`SubagentBackend`](mj_controller::hel_server::api::SubagentBackend) trait
//! and this module implements it here, where both are available.
//!
//! Every database read runs on `spawn_blocking`: the web server's handlers run
//! on the async runtime, and a synchronous SQLite read on that thread would
//! stall unrelated requests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use anyhow::{Context, Result};

use hel::hel_worker::RelayCommand;
use mj_client::session::{BoxFuture, SessionControl, SessionHandle, new_command_id};
use mj_controller::hel_server::api::{
    BundleExport, ExportError, PushedBranch, StartFollowup, StartStatus, SubagentBackend,
    TranscriptPage, TurnState, TurnSummary,
};

/// The backend the `/api/v1` routes drive sessions through.
pub struct ApiBackend {
    sessions: SessionControl,
    /// How far each created session's follow-up configuration and first prompt
    /// have got. Empty until M2 populates it.
    starts: Mutex<BTreeMap<String, StartStatus>>,
}

impl ApiBackend {
    pub fn new(sessions: SessionControl) -> Self {
        Self {
            sessions,
            starts: Mutex::new(BTreeMap::new()),
        }
    }
}

/// Run one blocking database read off the async runtime, keeping its context.
async fn blocking<T, F>(label: &'static str, job: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .with_context(|| format!("{label} task panicked"))?
}

fn not_in_this_milestone(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} is not implemented in this milestone")
}

impl SubagentBackend for ApiBackend {
    fn session_handle(&self, session_id: String) -> BoxFuture<'_, Result<Option<SessionHandle>>> {
        Box::pin(async move { Ok(self.sessions.session(session_id).await.ok()) })
    }

    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let handle = self
                .sessions
                .session(session_id.clone())
                .await
                .with_context(|| format!("session {session_id} is not running"))?;
            handle
                .submit(
                    new_command_id("api")?,
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new(text))],
                    },
                )
                .await
        })
    }

    fn turn_state(&self, session_id: String) -> BoxFuture<'_, Result<Option<TurnState>>> {
        Box::pin(async move {
            blocking("load turn outcome", move || {
                Ok(
                    hel::hel_database::load_materialized_turn_outcome(&session_id)?.map(
                        |(execution, active_turn, last_turn_outcome)| TurnState {
                            execution,
                            active_turn,
                            last_turn_outcome,
                        },
                    ),
                )
            })
            .await
        })
    }

    fn turn_summary(
        &self,
        session_id: String,
        turn_start_position: u64,
    ) -> BoxFuture<'_, Result<TurnSummary>> {
        Box::pin(async move {
            blocking("load turn summary", move || {
                hel::hel_database::load_materialized_turn_summary(&session_id, turn_start_position)
            })
            .await
        })
    }

    fn start_followup(
        &self,
        _session_id: String,
        _followup: StartFollowup,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { Err(not_in_this_milestone("session creation follow-up")) })
    }

    fn start_status(&self, session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>> {
        Box::pin(async move {
            Ok(self
                .starts
                .lock()
                .expect("api start status mutex poisoned")
                .get(&session_id)
                .cloned())
        })
    }

    fn lookup_idempotency(&self, key: String) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            blocking("look up idempotency key", move || {
                hel::hel_database::lookup_api_idempotency(&key)
            })
            .await
        })
    }

    fn record_idempotency(&self, key: String, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            blocking("record idempotency key", move || {
                hel::hel_database::record_api_idempotency(&key, &session_id)
            })
            .await
        })
    }

    fn transcript(
        &self,
        _session_id: String,
        _after_seq: u64,
        _limit: usize,
    ) -> BoxFuture<'_, Result<Option<TranscriptPage>>> {
        Box::pin(async move { Err(not_in_this_milestone("transcript paging")) })
    }

    fn diff(&self, _session_id: String) -> BoxFuture<'_, std::result::Result<String, ExportError>> {
        Box::pin(async move { Err(ExportError::Failed(not_in_this_milestone("diff export"))) })
    }

    fn read_file(
        &self,
        _session_id: String,
        _path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, ExportError>> {
        Box::pin(async move { Err(ExportError::Failed(not_in_this_milestone("file export"))) })
    }

    fn push_branch(
        &self,
        _session_id: String,
        _branch: String,
    ) -> BoxFuture<'_, std::result::Result<PushedBranch, ExportError>> {
        Box::pin(async move { Err(ExportError::Failed(not_in_this_milestone("branch push"))) })
    }

    fn bundle(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, std::result::Result<BundleExport, ExportError>> {
        Box::pin(async move { Err(ExportError::Failed(not_in_this_milestone("bundle export"))) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::session::{
        ManagedSessionView, PendingRelaySubmit, PendingRelaySync, SessionControlBackend,
        SessionHandleBackend,
    };
    use tokio::sync::mpsc;

    /// A session actor that records what it was asked to submit and accepts it
    /// at a fixed ordinal. Hand-written rather than mocked so the test proves
    /// the exact relay command the API sends.
    #[derive(Clone)]
    struct FakeSession {
        session_id: String,
        accepted_ordinal: u64,
        submitted: mpsc::UnboundedSender<(String, RelayCommand)>,
    }

    impl SessionHandleBackend for FakeSession {
        fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
            Box::new(self.clone())
        }
        fn session_id(&self) -> &str {
            &self.session_id
        }
        fn view(&self) -> ManagedSessionView {
            ManagedSessionView::default()
        }
        fn is_stopped(&self) -> bool {
            false
        }
        fn has_changed(&self) -> Result<bool> {
            Ok(false)
        }
        fn changed(&mut self) -> BoxFuture<'_, Result<ManagedSessionView>> {
            Box::pin(std::future::pending())
        }
        fn enqueue_submit(
            &self,
            command_id: String,
            command: RelayCommand,
        ) -> BoxFuture<'_, Result<PendingRelaySubmit>> {
            let _ = self.submitted.send((command_id, command));
            let ordinal = self.accepted_ordinal;
            Box::pin(async move {
                Ok(PendingRelaySubmit::new(Box::pin(
                    async move { Ok(ordinal) },
                )))
            })
        }
        fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
            Box::pin(async move { Ok(PendingRelaySync::new(Box::pin(async { Ok(()) }))) })
        }
        fn respond_elicitation(
            &self,
            _elicitation_id: String,
            _response: hel::hel_elicitation::ElicitationResponse,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn stop_background_task(&self, _background_task_id: String) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn reviewer(
            &self,
            _role: Option<String>,
            _action: mj_client::session::ReviewerAction,
        ) -> BoxFuture<'_, Result<mj_client::session::ReviewerOutcome>> {
            Box::pin(async { anyhow::bail!("no reviewer in this fake") })
        }
    }

    struct FakeControl(FakeSession);

    impl SessionControlBackend for FakeControl {
        fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>> {
            let session = self.0.clone();
            Box::pin(async move {
                anyhow::ensure!(session_id == session.session_id, "unknown session");
                Ok(SessionHandle::new(session))
            })
        }
    }

    #[tokio::test]
    async fn prompt_submits_one_text_block_and_returns_its_acceptance_ordinal() {
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 42,
            submitted: submitted_tx,
        })));

        let turn_id = backend
            .prompt("session-1".into(), "add a README line".into())
            .await
            .unwrap();
        assert_eq!(turn_id, 42);

        let (command_id, command) = submitted.recv().await.unwrap();
        assert!(
            command_id.starts_with("api-"),
            "command id {command_id} should name the API as its source"
        );
        let RelayCommand::Prompt { prompt } = command else {
            panic!("the API must submit a prompt command");
        };
        assert_eq!(prompt.len(), 1);
        let ContentBlock::Text(text) = &prompt[0] else {
            panic!("the API must submit the prompt as one text block");
        };
        assert_eq!(text.text, "add a README line");
    }

    #[tokio::test]
    async fn prompt_reports_a_session_the_manager_does_not_hold() {
        let (submitted_tx, _submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(SessionControl::new(FakeControl(FakeSession {
            session_id: "session-1".into(),
            accepted_ordinal: 1,
            submitted: submitted_tx,
        })));
        let error = backend
            .prompt("session-2".into(), "hello".into())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("session-2 is not running"),
            "unexpected error: {error:#}"
        );
    }
}
