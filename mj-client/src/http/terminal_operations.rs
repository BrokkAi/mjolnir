use super::{HttpApiError, HttpDaemonClient, encode_segment, sessions::session_path};
use crate::daemon::{
    CreateSessionRequest, ImportedBundle, InterruptAllResult, LifecycleReceipt, MAX_FRAME_BYTES,
    RegisteredSession, ResumeSessionRequest, SessionLifecycleRequest,
    StartupPromptIdentityConflict, StartupPromptOutcome,
};
use agent_client_protocol::v1::schema::ContentBlock;
use anyhow::{Result, ensure};
use mj_core::elicitation::ElicitationResponse;
use mj_core::move_workspace::RetainedMoveSource;
use mj_core::state::{MoveOutcome, MoveSessionRequest};
use mj_core::suspend_consent::{SuspendConsent, SuspendPreflight};
use mj_core::{relay::RelayCommand, state::SessionRecord};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCommandRequest {
    pub command_id: String,
    pub command: RelayCommand,
    pub inherited_draft: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupPromptRequest {
    pub command_id: String,
    pub text: String,
    pub prompt: Vec<ContentBlock>,
    pub encoded_draft: String,
    pub move_operation_id: Option<String>,
    pub inherited_draft: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartupPromptOutcomesRequest {
    pub command_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbandonedPromptRequest {
    pub bundle_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuspendPreflightRequest {
    pub dry_run: bool,
    pub without_checkpoint: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartSessionRequest {
    pub acknowledge_container_recreation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveSourcesRequest {
    pub cleanup_operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSessionMetadata {
    pub session: Box<SessionRecord>,
    pub bundle: Option<ImportedBundle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum LifecycleCompletion {
    Done,
    MoveOutcome(MoveOutcome),
    LifecycleFailed(String),
    LifecycleObservationUnavailable(LifecycleReceipt),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionProfile {
    pub preferred_profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptionResponse {
    pub text: String,
}

pub const STARTUP_PROMPT_IDENTITY_CONFLICT: &str = "startup_prompt_identity_conflict";

impl HttpDaemonClient {
    pub async fn interrupt_all(&self, session_id: String) -> Result<InterruptAllResult> {
        self.post_empty(&session_path(&session_id, "/interrupt-all")?)
            .await
    }

    pub async fn submit_session_command(
        &self,
        session_id: String,
        request: SessionCommandRequest,
    ) -> Result<u64> {
        self.post_json(&session_path(&session_id, "/commands")?, &request)
            .await
    }

    pub async fn queue_startup_prompt(
        &self,
        session_id: String,
        request: StartupPromptRequest,
    ) -> Result<()> {
        self.post_json_no_content(&session_path(&session_id, "/startup-prompts")?, &request)
            .await
            .map_err(|error| {
                if error.downcast_ref::<HttpApiError>().is_some_and(|failure| {
                    failure.status == reqwest::StatusCode::CONFLICT
                        && failure.code.as_deref() == Some(STARTUP_PROMPT_IDENTITY_CONFLICT)
                }) {
                    StartupPromptIdentityConflict.into()
                } else {
                    error
                }
            })
    }

    pub async fn withdraw_startup_prompt(
        &self,
        session_id: String,
        command_id: String,
    ) -> Result<bool> {
        self.json(
            Method::DELETE,
            &session_path(
                &session_id,
                &format!("/startup-prompts/{}", encode_segment(&command_id)?),
            )?,
            |request| request,
            true,
        )
        .await
    }

    pub async fn startup_prompt_outcomes(
        &self,
        session_id: String,
        request: StartupPromptOutcomesRequest,
    ) -> Result<BTreeMap<String, StartupPromptOutcome>> {
        self.post_json(
            &session_path(&session_id, "/startup-prompts/outcomes")?,
            &request,
        )
        .await
    }

    pub async fn sync_session(&self, session_id: String) -> Result<()> {
        self.post_empty_no_content(&session_path(&session_id, "/sync")?)
            .await
    }

    pub async fn respond_elicitation(
        &self,
        session_id: String,
        elicitation_id: String,
        request: ElicitationResponse,
    ) -> Result<()> {
        self.post_json_no_content(
            &session_path(
                &session_id,
                &format!("/elicitations/{}/respond", encode_segment(&elicitation_id)?),
            )?,
            &request,
        )
        .await
    }

    pub async fn record_abandoned_prompt(
        &self,
        session_id: String,
        request: AbandonedPromptRequest,
    ) -> Result<()> {
        self.post_json_no_content(&session_path(&session_id, "/abandoned-prompts")?, &request)
            .await
    }

    pub async fn start_session_lifecycle(
        &self,
        request: SessionLifecycleRequest,
    ) -> Result<LifecycleReceipt> {
        self.post_json("/api/v1/sessions/lifecycle/start", &request)
            .await
    }

    pub async fn wait_session_lifecycle(
        &self,
        request: LifecycleReceipt,
    ) -> Result<LifecycleCompletion> {
        // The accepting daemon owns the receipt and its result.
        self.json(
            Method::POST,
            "/api/v1/sessions/lifecycle/wait",
            |request_builder| request_builder.json(&request),
            false,
        )
        .await
    }

    pub async fn suspend_preflight(
        &self,
        session_id: String,
        request: SuspendPreflightRequest,
    ) -> Result<SuspendPreflight> {
        self.post_json(
            &session_path(&session_id, "/lifecycle/suspend-preflight")?,
            &request,
        )
        .await
    }

    pub async fn suspend_session_with_consent(
        &self,
        session_id: String,
        request: SuspendConsent,
    ) -> Result<()> {
        self.post_json_no_content(&session_path(&session_id, "/lifecycle/suspend")?, &request)
            .await
    }

    pub async fn restart_session_with_ack(
        &self,
        session_id: String,
        request: RestartSessionRequest,
    ) -> Result<()> {
        self.post_json_no_content(&session_path(&session_id, "/lifecycle/restart")?, &request)
            .await
    }

    pub async fn start_create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        self.post_json("/api/v1/sessions/create", &request).await
    }

    pub async fn wait_create_session(&self, session_id: String) -> Result<()> {
        self.send(
            Method::POST,
            &session_path(&session_id, "/create/wait")?,
            |request| request,
            false,
        )
        .await
        .map_err(terminal_wait_error)?;
        Ok(())
    }

    pub async fn resume_session(&self, request: ResumeSessionRequest) -> Result<()> {
        self.post_json_no_content(
            &session_path(&request.session_id, "/lifecycle/resume")?,
            &request,
        )
        .await
    }

    pub async fn move_session(&self, request: MoveSessionRequest) -> Result<MoveOutcome> {
        self.post_json(
            &session_path(&request.preparation.selection.session_id, "/lifecycle/move")?,
            &request,
        )
        .await
    }

    pub async fn move_sources(
        &self,
        session_id: String,
        request: MoveSourcesRequest,
    ) -> Result<Vec<RetainedMoveSource>> {
        self.post_json(&session_path(&session_id, "/move-sources")?, &request)
            .await
    }

    pub async fn destroy_stopped_session(&self, session_id: String) -> Result<()> {
        self.post_empty_no_content(&session_path(&session_id, "/lifecycle/destroy-stopped")?)
            .await
    }

    pub async fn force_destroy_session(&self, session_id: String) -> Result<()> {
        self.post_empty_no_content(&session_path(&session_id, "/lifecycle/force-destroy")?)
            .await
    }

    pub async fn cancel_lifecycle(&self, session_id: String) -> Result<()> {
        self.post_empty_no_content(&session_path(&session_id, "/lifecycle/cancel")?)
            .await
    }

    pub async fn transcription_available(&self, preferred_profile: String) -> Result<bool> {
        self.get_json_query(
            "/api/v1/audio/transcription-available",
            &TranscriptionProfile { preferred_profile },
        )
        .await
    }

    pub async fn transcribe_audio(
        &self,
        preferred_profile: String,
        audio: Vec<u8>,
    ) -> Result<String> {
        let response: TranscriptionResponse = self
            .json(
                Method::POST,
                "/api/v1/audio/transcribe",
                |request| {
                    request
                        .query(&TranscriptionProfile { preferred_profile })
                        .header(reqwest::header::CONTENT_TYPE, "audio/wav")
                        .body(audio)
                },
                false,
            )
            .await?;
        Ok(response.text)
    }

    pub async fn import_session_observed<P, PF, C>(
        &self,
        metadata: ImportSessionMetadata,
        file: &mut tokio::fs::File,
        mut on_phase: P,
        cancel: C,
    ) -> Result<()>
    where
        P: FnMut(bool) -> PF,
        PF: Future<Output = Result<()>>,
        C: Future<Output = ()>,
    {
        use futures::{StreamExt, stream};
        use tokio::io::AsyncSeekExt;
        use tokio_util::io::ReaderStream;

        let metadata = serde_json::to_vec(&metadata)?;
        ensure!(
            metadata.len() <= MAX_FRAME_BYTES,
            "import header exceeds the daemon frame limit"
        );
        let mut header = Vec::with_capacity(4 + metadata.len());
        header.extend_from_slice(&u32::try_from(metadata.len())?.to_be_bytes());
        header.extend_from_slice(&metadata);
        let header = bytes::Bytes::from(header);
        let upload = async {
            // Keep one file position owner even if an early refusal arrives
            // while the HTTP transport is still dropping its request body.
            let file = std::sync::Arc::new(tokio::sync::Mutex::new(file.try_clone().await?));
            let mut endpoint = self.endpoint.read().await.clone();
            loop {
                on_phase(true).await?;
                let mut reader = file.clone().lock_owned().await;
                reader.seek(std::io::SeekFrom::Start(0)).await?;
                let (eof, mut reached_eof) = tokio::sync::oneshot::channel();
                let prefix = header.clone();
                let mut eof = Some(eof);
                let body = stream::once(async move { Ok(prefix) })
                    .chain(ReaderStream::new(ImportFile(reader)))
                    .chain(stream::poll_fn(move |_| {
                        if let Some(eof) = eof.take() {
                            let _ = eof.send(());
                        }
                        std::task::Poll::Ready(None::<std::io::Result<bytes::Bytes>>)
                    }));
                let mut request = self
                    .http
                    .post(endpoint.url("/api/v1/sessions/import"))
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(reqwest::Body::wrap_stream(body))
                    .build()?;
                endpoint.authorize(&mut request)?;
                let response = self.send_once(request);
                tokio::pin!(response);
                let result = tokio::select! {
                    biased;
                    signal = &mut reached_eof => {
                        if signal.is_ok() {
                            on_phase(false).await?;
                        }
                        response.await
                    }
                    result = &mut response => result,
                };
                match result {
                    Err(error)
                        if self.allow_reconnect
                            && error
                                .downcast_ref::<HttpApiError>()
                                .is_some_and(HttpApiError::is_handoff) =>
                    {
                        endpoint = self.refresh_endpoint(&endpoint).await?;
                    }
                    response => return response.map(|_| ()),
                }
            }
        };
        tokio::select! {
            biased;
            _ = cancel => anyhow::bail!("upload cancelled"),
            result = upload => result,
        }
    }
}

fn terminal_wait_error(error: anyhow::Error) -> anyhow::Error {
    if let Some(failure) = error.downcast_ref::<HttpApiError>() {
        if failure.is_handoff() {
            return error.context(crate::daemon::DaemonTransportLost);
        }
        return crate::daemon::DaemonRefusal(failure.message.clone()).into();
    }
    if error.is::<reqwest::Error>() {
        return error.context(crate::daemon::DaemonTransportLost);
    }
    error
}

struct ImportFile(tokio::sync::OwnedMutexGuard<tokio::fs::File>);

impl tokio::io::AsyncRead for ImportFile {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_read(context, buffer)
    }
}
