//! The session operations used by interactive control surfaces.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::SessionConfigOption;
use anyhow::{Context, Result, ensure};
use hel::hel_config::HelConfig;
use hel::hel_elicitation::ElicitationResponse;
use hel::hel_state::{ManagedSessionSnapshot, SessionRecord};
use hel::hel_worker::{
    AnalyzeDeltaRepository, RelayCommand, RelayCursor, RelayEvent, RelayOperationalState, RepoDelta,
};
use hel::hel_worker_launch::ReviewerLaunchConfig;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum ViewError {
    Unreachable(String),
    TargetMissing(String),
    ProjectionIntegrity(String),
}

impl ViewError {
    pub fn detail(&self) -> &str {
        match self {
            Self::Unreachable(detail)
            | Self::TargetMissing(detail)
            | Self::ProjectionIntegrity(detail) => detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ManagedSessionView {
    pub snapshot: Option<ManagedSessionSnapshot>,
    pub connected: bool,
    pub error: Option<ViewError>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelayAttachment {
    pub state: RelayOperationalState,
    pub events: Vec<RelayEvent>,
    pub through_ordinal: u64,
    pub through_digest: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StartedReviewer {
    pub native_session_id: Option<String>,
    pub config_options: Vec<SessionConfigOption>,
    pub reused: bool,
    pub state: RelayOperationalState,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerAction {
    Start {
        config: Box<ReviewerLaunchConfig>,
    },
    Submit {
        command_id: String,
        command: RelayCommand,
    },
    Attach {
        after_ordinal: u64,
        after_digest: String,
    },
    Acknowledge {
        through_ordinal: u64,
        through_digest: String,
    },
    Status,
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
    },
    Pause,
    CaptureDelta {
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    AdvanceBaseline {
        trees: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    AnalyzeDelta {
        repositories: Vec<AnalyzeDeltaRepository>,
    },
    TakeLaneDispatches,
}

impl ReviewerAction {
    pub const fn operation_name(&self) -> &'static str {
        match self {
            Self::Start { .. } => "reviewer_start",
            Self::Submit { .. } => "reviewer_submit",
            Self::Attach { .. } => "reviewer_attach",
            Self::Acknowledge { .. } => "reviewer_acknowledge",
            Self::Status => "reviewer_status",
            Self::RespondElicitation { .. } => "reviewer_respond_elicitation",
            Self::Pause => "reviewer_pause",
            Self::CaptureDelta { .. } => "reviewer_capture_delta",
            Self::AdvanceBaseline { .. } => "reviewer_advance_baseline",
            Self::AnalyzeDelta { .. } => "reviewer_analyze_delta",
            Self::TakeLaneDispatches => "reviewer_take_lane_dispatches",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerOutcome {
    Started(Box<StartedReviewer>),
    Accepted {
        ordinal: u64,
    },
    Attached(Box<RelayAttachment>),
    Acknowledged(RelayCursor),
    Status(Box<RelayOperationalState>),
    ElicitationResolved,
    Paused,
    Delta {
        repositories: Vec<RepoDelta>,
    },
    BaselineAdvanced,
    ChangedFunctions {
        packet: String,
    },
    LaneDispatches {
        requests: Vec<hel::hel_review::lanes::ReviewSubagentRequest>,
    },
}

pub struct PendingRelaySubmit {
    completion: BoxFuture<'static, Result<u64>>,
}

impl PendingRelaySubmit {
    pub fn new(completion: BoxFuture<'static, Result<u64>>) -> Self {
        Self { completion }
    }

    pub async fn wait(self) -> Result<u64> {
        self.completion.await
    }
}

pub struct PendingRelaySync {
    completion: BoxFuture<'static, Result<()>>,
}

impl PendingRelaySync {
    pub fn new(completion: BoxFuture<'static, Result<()>>) -> Self {
        Self { completion }
    }

    pub async fn wait(self) -> Result<()> {
        self.completion.await
    }
}

pub trait SessionHandleBackend: Send + Sync {
    fn config_result(&self, command_id: String) -> BoxFuture<'_, Result<Option<Option<String>>>> {
        let session_id = self.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                hel::hel_database::load_config_result(&session_id, &command_id)
            })
            .await
            .context("read configuration completion task")?
        })
    }

    fn clone_box(&self) -> Box<dyn SessionHandleBackend>;
    fn session_id(&self) -> &str;
    fn view(&self) -> ManagedSessionView;
    fn is_stopped(&self) -> bool;
    fn has_changed(&self) -> Result<bool>;
    fn changed(&mut self) -> BoxFuture<'_, Result<ManagedSessionView>>;
    fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> BoxFuture<'_, Result<PendingRelaySubmit>>;
    fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>>;
    fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> BoxFuture<'_, Result<()>>;
    fn stop_background_task(&self, background_task_id: String) -> BoxFuture<'_, Result<()>>;
    fn reviewer(
        &self,
        role: Option<String>,
        action: ReviewerAction,
    ) -> BoxFuture<'_, Result<ReviewerOutcome>>;
}

pub struct SessionHandle {
    backend: Box<dyn SessionHandleBackend>,
}

impl SessionHandle {
    pub fn new(backend: impl SessionHandleBackend + 'static) -> Self {
        Self {
            backend: Box::new(backend),
        }
    }

    pub fn session_id(&self) -> &str {
        self.backend.session_id()
    }

    pub fn view(&self) -> ManagedSessionView {
        self.backend.view()
    }

    pub fn is_stopped(&self) -> bool {
        self.backend.is_stopped()
    }

    pub fn has_changed(&self) -> Result<bool> {
        self.backend.has_changed()
    }

    pub async fn changed(&mut self) -> Result<ManagedSessionView> {
        self.backend.changed().await
    }

    pub async fn submit(&self, command_id: String, command: RelayCommand) -> Result<u64> {
        self.enqueue_submit(command_id, command).await?.wait().await
    }

    /// Apply a setting and wait for its durable success or rejection.
    pub async fn set_config(&self, key: String, value: String) -> Result<()> {
        let command_id = new_command_id("set-config")?;
        self.submit(command_id.clone(), RelayCommand::SetConfig { key, value })
            .await?;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(error) = self.backend.config_result(command_id.clone()).await? {
                    if let Some(error) = error {
                        anyhow::bail!("{error}");
                    }
                    self.sync_now().await?;
                    return Ok(());
                }
                ensure!(
                    !self.is_stopped(),
                    "session stopped while applying configuration"
                );
                if let Some(error) = self.view().error {
                    anyhow::bail!("configuration connection failed: {}", error.detail());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("configuration command did not complete within 60 seconds")?
    }

    pub async fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<PendingRelaySubmit> {
        self.backend.enqueue_submit(command_id, command).await
    }

    pub async fn sync_now(&self) -> Result<()> {
        self.enqueue_sync().await?.wait().await
    }

    pub async fn enqueue_sync(&self) -> Result<PendingRelaySync> {
        self.backend.enqueue_sync().await
    }

    pub async fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        self.backend
            .respond_elicitation(elicitation_id, response)
            .await
    }

    pub async fn stop_background_task(&self, background_task_id: String) -> Result<()> {
        self.backend.stop_background_task(background_task_id).await
    }

    pub async fn reviewer(&self, action: ReviewerAction) -> Result<ReviewerOutcome> {
        self.reviewer_as(None, action).await
    }

    pub async fn reviewer_as(
        &self,
        role: Option<String>,
        action: ReviewerAction,
    ) -> Result<ReviewerOutcome> {
        self.backend.reviewer(role, action).await
    }
}

impl Clone for SessionHandle {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone_box(),
        }
    }
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionHandle")
            .field("session_id", &self.session_id())
            .finish_non_exhaustive()
    }
}

pub trait SessionControlBackend: Send + Sync {
    fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>>;
}

#[derive(Clone)]
pub struct SessionControl {
    backend: Arc<dyn SessionControlBackend>,
}

impl SessionControl {
    pub fn new(backend: impl SessionControlBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    pub async fn session(&self, session_id: impl Into<String>) -> Result<SessionHandle> {
        self.backend.session(session_id.into()).await
    }

    pub async fn wait_for_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<SessionHandle> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.session(session_id.to_owned()).await {
                    Ok(handle) => return Ok(handle),
                    Err(error) => {
                        tracing::trace!(session_id, "waiting for session actor: {error:#}");
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "session {session_id} did not become available within {} seconds",
                timeout.as_secs()
            )
        })?
    }
}

impl fmt::Debug for SessionControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionControl(..)")
    }
}

pub trait ReviewerStagerBackend: Send + Sync {
    fn stage(
        &self,
        config: HelConfig,
        session: SessionRecord,
        profile_id: String,
        generation: u64,
    ) -> Result<ReviewerLaunchConfig>;
}

#[derive(Clone)]
pub struct ReviewerStager {
    backend: Arc<dyn ReviewerStagerBackend>,
}

impl ReviewerStager {
    pub fn new(backend: impl ReviewerStagerBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    pub fn stage(
        &self,
        config: HelConfig,
        session: SessionRecord,
        profile_id: String,
        generation: u64,
    ) -> Result<ReviewerLaunchConfig> {
        self.backend.stage(config, session, profile_id, generation)
    }

    #[doc(hidden)]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(UnavailableReviewerStager(message.into()))
    }
}

impl fmt::Debug for ReviewerStager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReviewerStager(..)")
    }
}

struct UnavailableReviewerStager(String);

impl ReviewerStagerBackend for UnavailableReviewerStager {
    fn stage(
        &self,
        _config: HelConfig,
        _session: SessionRecord,
        _profile_id: String,
        _generation: u64,
    ) -> Result<ReviewerLaunchConfig> {
        anyhow::bail!(self.0.clone())
    }
}

pub fn new_command_id(prefix: &str) -> Result<String> {
    ensure!(!prefix.trim().is_empty(), "command ID prefix is required");
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate command ID: {error}"))?;
    Ok(format!("{prefix}-{}", hex(&random)))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// A stopped session and a manager that resolves its live replacement.
///
/// Chat's cross-crate tests use this hand-written client fake to verify actor
/// replacement without depending on the controller implementation crate.
#[doc(hidden)]
pub struct ReplacementSessionTestFixture {
    pub stopped: SessionHandle,
    pub control: SessionControl,
    pub submitted: tokio::sync::mpsc::UnboundedReceiver<RelayCommand>,
}

#[derive(Clone)]
struct ReplacementTestSession {
    session_id: String,
    stopped: bool,
    accepted_ordinal: u64,
    submitted: Option<tokio::sync::mpsc::UnboundedSender<RelayCommand>>,
    view: tokio::sync::watch::Receiver<ManagedSessionView>,
    _view_guard: Option<Arc<tokio::sync::watch::Sender<ManagedSessionView>>>,
}

impl SessionHandleBackend for ReplacementTestSession {
    fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
        Box::new(self.clone())
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn view(&self) -> ManagedSessionView {
        self.view.borrow().clone()
    }

    fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn has_changed(&self) -> Result<bool> {
        self.view.has_changed().context("session manager stopped")
    }

    fn changed(&mut self) -> BoxFuture<'_, Result<ManagedSessionView>> {
        Box::pin(async move {
            self.view
                .changed()
                .await
                .context("session manager stopped")?;
            Ok(self.view())
        })
    }

    fn enqueue_submit(
        &self,
        _command_id: String,
        command: RelayCommand,
    ) -> BoxFuture<'_, Result<PendingRelaySubmit>> {
        let submitted = self.submitted.clone();
        let stopped = self.stopped;
        let accepted_ordinal = self.accepted_ordinal;
        Box::pin(async move {
            ensure!(!stopped, "session manager stopped");
            let submitted = submitted.context("unsupported test operation")?;
            submitted
                .send(command)
                .context("test submit observer stopped")?;
            Ok(PendingRelaySubmit::new(Box::pin(async move {
                Ok(accepted_ordinal)
            })))
        })
    }

    fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
        let stopped = self.stopped;
        Box::pin(async move {
            ensure!(!stopped, "session manager stopped");
            Ok(PendingRelaySync::new(Box::pin(async { Ok(()) })))
        })
    }

    fn respond_elicitation(
        &self,
        _elicitation_id: String,
        _response: ElicitationResponse,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { anyhow::bail!("unsupported test operation") })
    }

    fn stop_background_task(&self, _background_task_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { anyhow::bail!("unsupported test operation") })
    }

    fn reviewer(
        &self,
        _role: Option<String>,
        _action: ReviewerAction,
    ) -> BoxFuture<'_, Result<ReviewerOutcome>> {
        Box::pin(async { anyhow::bail!("unsupported test operation") })
    }
}

struct ReplacementTestControl {
    session_id: String,
    replacement: SessionHandle,
}

impl SessionControlBackend for ReplacementTestControl {
    fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>> {
        Box::pin(async move {
            ensure!(
                session_id == self.session_id,
                "session {session_id} is not managed"
            );
            Ok(self.replacement.clone())
        })
    }
}

#[doc(hidden)]
pub fn replacement_session_test_fixture(
    session_id: &str,
    accepted_ordinal: u64,
) -> ReplacementSessionTestFixture {
    let (stopped_view_tx, stopped_view) =
        tokio::sync::watch::channel(ManagedSessionView::default());
    drop(stopped_view_tx);
    let stopped = SessionHandle::new(ReplacementTestSession {
        session_id: session_id.to_owned(),
        stopped: true,
        accepted_ordinal,
        submitted: None,
        view: stopped_view,
        _view_guard: None,
    });

    let (view_tx, view) = tokio::sync::watch::channel(ManagedSessionView::default());
    let (submitted_tx, submitted) = tokio::sync::mpsc::unbounded_channel();
    let replacement = SessionHandle::new(ReplacementTestSession {
        session_id: session_id.to_owned(),
        stopped: false,
        accepted_ordinal,
        submitted: Some(submitted_tx),
        view,
        _view_guard: Some(Arc::new(view_tx)),
    });
    let control = SessionControl::new(ReplacementTestControl {
        session_id: session_id.to_owned(),
        replacement,
    });
    ReplacementSessionTestFixture {
        stopped,
        control,
        submitted,
    }
}
