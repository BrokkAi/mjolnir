//! Session-scoped model tools for Mjolnir Computer.app.
//!
//! This layer owns the policy absent from the platform backend: every input
//! request names a fresh observation, and the display must still match that
//! observation before the separate macOS host receives any event.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agent_client_protocol::schema::v1::McpServer;
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use anyhow::{Context as _, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::{NotificationContext, Peer, RequestContext},
    tool, tool_router,
};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    computer::{
        BackendAction, ClickArgs, ComputerBackend, ComputerError, ComputerPermission,
        CurrentDisplay, DisplayId, DoubleClickArgs, DragArgs, KeyArgs, MoveArgs, Observation,
        ObservationId, ObservationMetadata, ObserveArgs, PermissionReadiness, ScrollArgs,
        TargetedPointArgs, TypeTextArgs, WaitArgs,
    },
    computer_host::HostSessionId,
    computer_host_macos::MacosComputerHost,
    event::{ComputerControlStatus, PermissionDecision, PermissionPrompt, UiEvent},
};

pub const MCP_SERVER_NAME: &str = "mj-computer";

const MAX_RETAINED_OBSERVATIONS: usize = 32;
const MAX_WAIT_MILLISECONDS: u64 = 10_000;
const CONFIG_REVOCATION_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const COMPUTER_TOOL_NAMES: [&str; 9] = [
    "computer_click",
    "computer_double_click",
    "computer_drag",
    "computer_key",
    "computer_move",
    "computer_observe",
    "computer_scroll",
    "computer_type",
    "computer_wait",
];

const SERVER_GUIDANCE: &str = concat!(
    "LOCAL MAC COMPUTER CONTROL: the user enables, disables, and verifies it from Mjolnir's ",
    "built-in `/mjconfig` Computer tab. The separate Mjolnir Computer.app owns macOS permissions, ",
    "not the terminal. `computer_observe` returns an image of the visible desktop. Before ",
    "every click, move, drag, key, text, or scroll action, first observe and target that ",
    "fresh image; the service rejects stale observations and changed displays. Mjolnir asks ",
    "the user to approve each input action before the app receives it. ",
    "Control only the local desktop interaction the user asked for; never use it to expand ",
    "scope, reveal sensitive information, or approve access on the user's behalf."
);

#[derive(Default)]
struct ObservationStore {
    values: HashMap<ObservationId, ObservationMetadata>,
    order: VecDeque<ObservationId>,
}

struct ComputerControl<B> {
    backend: Arc<B>,
    observations: Mutex<ObservationStore>,
}

impl<B: ComputerBackend> ComputerControl<B> {
    fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            observations: Mutex::new(ObservationStore::default()),
        }
    }

    async fn observe(&self, args: ObserveArgs) -> Result<Observation, ComputerError> {
        let observation = self.backend.observe(args, CancellationToken::new()).await?;
        observation.validate(crate::computer::ImageLimits::DEFAULT)?;
        let now = unix_millis()?;
        let mut observations = self.observations.lock().await;
        observations
            .values
            .retain(|_, value| value.expires_at_unix_ms > now);
        let retained_ids: HashSet<_> = observations.values.keys().cloned().collect();
        observations.order.retain(|id| retained_ids.contains(id));
        let id = observation.metadata.observation_id.clone();
        if observations
            .values
            .insert(id.clone(), observation.metadata.clone())
            .is_none()
        {
            observations.order.push_back(id);
        }
        while observations.order.len() > MAX_RETAINED_OBSERVATIONS {
            if let Some(id) = observations.order.pop_front() {
                observations.values.remove(&id);
            }
        }
        Ok(observation)
    }

    async fn execute_targeted(
        &self,
        target: &TargetedPointArgs,
        make_action: impl FnOnce(f64, f64) -> BackendAction,
    ) -> Result<(), ComputerError> {
        let (x, y) = self.resolve_target(target).await?;
        self.backend
            .execute(make_action(x, y), CancellationToken::new())
            .await
    }

    async fn execute_drag(&self, args: &DragArgs) -> Result<(), ComputerError> {
        let from = TargetedPointArgs {
            observation_id: args.observation_id.clone(),
            point: args.from,
        };
        let to = TargetedPointArgs {
            observation_id: args.observation_id.clone(),
            point: args.to,
        };
        let (from_x, from_y) = self.resolve_target(&from).await?;
        let (to_x, to_y) = self.resolve_target(&to).await?;
        self.backend
            .execute(
                BackendAction::Drag {
                    from: (from_x, from_y),
                    to: (to_x, to_y),
                    button: args.button,
                },
                CancellationToken::new(),
            )
            .await
    }

    async fn execute_text(&self, args: &TypeTextArgs) -> Result<(), ComputerError> {
        self.validate_observation(&args.observation_id).await?;
        self.backend
            .execute(
                BackendAction::TypeText {
                    text: args.text.clone(),
                },
                CancellationToken::new(),
            )
            .await
    }

    async fn execute_key(&self, args: &KeyArgs) -> Result<(), ComputerError> {
        self.validate_observation(&args.observation_id).await?;
        self.backend
            .execute(
                BackendAction::Key {
                    key: args.key,
                    modifiers: args.modifiers.clone(),
                },
                CancellationToken::new(),
            )
            .await
    }

    async fn wait(&self, args: &WaitArgs) -> Result<(), ComputerError> {
        if args.milliseconds > MAX_WAIT_MILLISECONDS {
            return Err(ComputerError::WaitDurationExceeded);
        }
        self.validate_observation(&args.observation_id).await?;
        tokio::time::sleep(std::time::Duration::from_millis(args.milliseconds)).await;
        Ok(())
    }

    async fn resolve_target(
        &self,
        target: &TargetedPointArgs,
    ) -> Result<(f64, f64), ComputerError> {
        let metadata = self.metadata(&target.observation_id).await?;
        let current = self.current_display(&metadata.display_id).await?;
        metadata.resolve_target(target.point, unix_millis()?, &current)
    }

    async fn validate_observation(
        &self,
        observation_id: &ObservationId,
    ) -> Result<(), ComputerError> {
        let metadata = self.metadata(observation_id).await?;
        let current = self.current_display(&metadata.display_id).await?;
        metadata.validate_current(unix_millis()?, &current)
    }

    async fn metadata(
        &self,
        observation_id: &ObservationId,
    ) -> Result<ObservationMetadata, ComputerError> {
        self.observations
            .lock()
            .await
            .values
            .get(observation_id)
            .cloned()
            .ok_or(ComputerError::ObservationNotFound)
    }

    async fn current_display(
        &self,
        display_id: &DisplayId,
    ) -> Result<CurrentDisplay, ComputerError> {
        self.backend
            .current_display(display_id.clone(), CancellationToken::new())
            .await
    }
}

#[derive(Default)]
struct SessionHostState {
    enabled: bool,
    host: Option<Arc<MacosComputerHost>>,
}

#[derive(Debug, Clone, Copy)]
struct SetupOutcome {
    readiness: PermissionReadiness,
    requested_permission: Option<ComputerPermission>,
}

/// Dynamic backend behind the always-advertised primary-session MCP endpoint.
/// It has no host and no macOS authority until the user enables Computer
/// Control from Mjolnir's own setup panel.
struct SessionBackend {
    config_path: PathBuf,
    state: Mutex<SessionHostState>,
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigFileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

async fn config_file_stamp(path: &Path) -> Option<ConfigFileStamp> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    Some(ConfigFileStamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

impl SessionBackend {
    fn new(config_path: PathBuf) -> Self {
        Self {
            config_path,
            state: Mutex::new(SessionHostState::default()),
        }
    }

    fn persisted_enabled(&self) -> bool {
        crate::config::Config::load(&self.config_path)
            .map(|config| config.computer.enabled)
            .unwrap_or(false)
    }

    async fn has_active_host(&self) -> bool {
        let state = self.state.lock().await;
        state.enabled && state.host.is_some()
    }

    async fn activate(&self) -> Result<PermissionReadiness, ComputerError> {
        let host = self.ensure_host().await?;
        self.state.lock().await.enabled = true;
        host.permission_readiness(CancellationToken::new()).await
    }

    /// Restart the native app before checking macOS privacy state. TCC can
    /// retain the old process's view after the user changes a setting.
    async fn refresh(&self) -> Result<PermissionReadiness, ComputerError> {
        let host = {
            let mut state = self.state.lock().await;
            state.enabled = false;
            state.host.take()
        };
        if let Some(host) = host {
            // A broken control connection means the process is already gone
            // or unusable; replacing it is the recovery path.
            let _ = host.shutdown().await;
        }
        self.activate().await
    }

    async fn set_up(&self) -> Result<SetupOutcome, ComputerError> {
        let host = self.ensure_host().await?;
        self.state.lock().await.enabled = true;
        let mut readiness = host.permission_readiness(CancellationToken::new()).await?;
        let requested_permission = next_missing_permission(readiness);
        if let Some(permission) = requested_permission {
            readiness = host
                .request_permission(permission, CancellationToken::new())
                .await?;
        }
        Ok(SetupOutcome {
            readiness,
            requested_permission,
        })
    }

    async fn status(&self) -> ComputerControlStatus {
        if !self.persisted_enabled() {
            let _ = self.disable().await;
            return ComputerControlStatus::disabled();
        }
        let (enabled, host) = {
            let state = self.state.lock().await;
            (state.enabled, state.host.clone())
        };
        if !enabled {
            return ComputerControlStatus::disabled();
        }
        let Some(host) = host else {
            return ComputerControlStatus {
                enabled: true,
                readiness: None,
                detail: Some("Mjolnir Computer is not running for this session".to_string()),
            };
        };
        match host.permission_readiness(CancellationToken::new()).await {
            Ok(readiness) => ComputerControlStatus {
                enabled: true,
                readiness: Some(readiness),
                detail: None,
            },
            Err(error) => ComputerControlStatus {
                enabled: true,
                readiness: None,
                detail: Some(error.to_string()),
            },
        }
    }

    async fn disable(&self) -> Result<(), ComputerError> {
        let host = {
            let mut state = self.state.lock().await;
            state.enabled = false;
            state.host.take()
        };
        if let Some(host) = host {
            host.shutdown().await?;
        }
        Ok(())
    }

    async fn ensure_enabled(&self) -> Result<(), ComputerError> {
        if !self.persisted_enabled() {
            let _ = self.disable().await;
            return Err(ComputerError::ControlDisabled);
        }
        let state = self.state.lock().await;
        if state.enabled && state.host.is_some() {
            Ok(())
        } else {
            Err(ComputerError::ControlDisabled)
        }
    }

    async fn require_host(&self) -> Result<Arc<MacosComputerHost>, ComputerError> {
        self.ensure_enabled().await?;
        let state = self.state.lock().await;
        if !state.enabled {
            return Err(ComputerError::ControlDisabled);
        }
        state
            .host
            .clone()
            .ok_or_else(|| ComputerError::Backend("Mjolnir Computer is not running".to_string()))
    }

    async fn ensure_host(&self) -> Result<Arc<MacosComputerHost>, ComputerError> {
        if let Some(host) = self.state.lock().await.host.clone() {
            return Ok(host);
        }
        let bundle =
            computer_bundle_path().map_err(|error| ComputerError::Backend(error.to_string()))?;
        let launched = Arc::new(
            MacosComputerHost::launch(
                &bundle,
                HostSessionId::generate()
                    .map_err(|error| ComputerError::Backend(error.to_string()))?,
            )
            .await?,
        );
        let mut state = self.state.lock().await;
        if let Some(host) = state.host.clone() {
            drop(state);
            let _ = launched.shutdown().await;
            return Ok(host);
        }
        state.host = Some(launched.clone());
        Ok(launched)
    }
}

fn next_missing_permission(readiness: PermissionReadiness) -> Option<ComputerPermission> {
    (readiness.screen_recording != crate::computer::PermissionState::Granted)
        .then_some(ComputerPermission::ScreenRecording)
        .or_else(|| {
            (readiness.accessibility != crate::computer::PermissionState::Granted)
                .then_some(ComputerPermission::Accessibility)
        })
}

fn permission_request_detail(permission: ComputerPermission) -> String {
    let name = match permission {
        ComputerPermission::ScreenRecording => "Screen Recording",
        ComputerPermission::Accessibility => "Accessibility",
    };
    format!(
        "{name} request sent. Complete it in System Settings, return here, then press r to restart Mjolnir Computer and recheck."
    )
}

#[async_trait::async_trait]
impl ComputerBackend for SessionBackend {
    async fn observe(
        &self,
        request: ObserveArgs,
        cancellation: CancellationToken,
    ) -> Result<Observation, ComputerError> {
        self.require_host()
            .await?
            .observe(request, cancellation)
            .await
    }

    async fn permission_readiness(
        &self,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        self.require_host()
            .await?
            .permission_readiness(cancellation)
            .await
    }

    async fn request_permission(
        &self,
        permission: ComputerPermission,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        self.require_host()
            .await?
            .request_permission(permission, cancellation)
            .await
    }

    async fn current_display(
        &self,
        display_id: DisplayId,
        cancellation: CancellationToken,
    ) -> Result<CurrentDisplay, ComputerError> {
        self.require_host()
            .await?
            .current_display(display_id, cancellation)
            .await
    }

    async fn host_lock_state(
        &self,
        cancellation: CancellationToken,
    ) -> Result<crate::computer::HostLockState, ComputerError> {
        self.require_host()
            .await?
            .host_lock_state(cancellation)
            .await
    }

    async fn execute(
        &self,
        action: BackendAction,
        cancellation: CancellationToken,
    ) -> Result<(), ComputerError> {
        self.require_host()
            .await?
            .execute(action, cancellation)
            .await
    }
}

struct UserApproval {
    event_tx: mpsc::UnboundedSender<UiEvent>,
    next_id: AtomicU64,
}

impl UserApproval {
    fn new(event_tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            event_tx,
            next_id: AtomicU64::new(1),
        }
    }

    async fn request(&self, action: &str) -> Result<(), ComputerError> {
        let (responder, response) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(
                format!("mj-computer-{id}"),
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Other)
                    .title(format!("Mjolnir Computer wants to {action}")),
            ),
            options: vec![
                PermissionOption::new(
                    "allow",
                    "Allow this action",
                    PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
            ],
            responder,
        };
        self.event_tx
            .send(UiEvent::PermissionRequest(prompt))
            .map_err(|_| ComputerError::Cancelled)?;
        match response.await {
            Ok(PermissionDecision::Selected(option)) if option == "allow" => Ok(()),
            Ok(PermissionDecision::Selected(_)) | Ok(PermissionDecision::Cancelled) | Err(_) => {
                Err(ComputerError::ActionNotApproved)
            }
        }
    }
}

#[derive(Default)]
struct ToolListNotificationState {
    peer: Option<Peer<RoleServer>>,
    batching: bool,
    changed_during_batch: bool,
}

/// Bridges router mutations to the one active MCP connection. A control-state
/// transition changes nine routes, so batch them into one standard
/// `tools/list_changed` notification rather than making the client refresh
/// the same list nine times.
#[derive(Default)]
struct ToolListNotifier {
    state: std::sync::Mutex<ToolListNotificationState>,
}

impl ToolListNotifier {
    fn set_peer(&self, peer: Peer<RoleServer>) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .peer = Some(peer);
    }

    fn begin_batch(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        debug_assert!(!state.batching);
        state.batching = true;
        state.changed_during_batch = false;
    }

    fn route_changed(&self) {
        let peer = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.batching {
                state.changed_during_batch = true;
                None
            } else {
                state.peer.clone()
            }
        };
        if let Some(peer) = peer {
            Self::send(peer);
        }
    }

    fn finish_batch(&self) {
        let peer = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            debug_assert!(state.batching);
            state.batching = false;
            std::mem::take(&mut state.changed_during_batch).then(|| state.peer.clone())
        }
        .flatten();
        if let Some(peer) = peer {
            Self::send(peer);
        }
    }

    fn send(peer: Peer<RoleServer>) {
        tokio::spawn(async move {
            if let Err(error) = peer.notify_tool_list_changed().await {
                tracing::debug!("could not send computer tools/list_changed: {error}");
            }
        });
    }
}

#[derive(Clone)]
struct McpHandler {
    control: Arc<ComputerControl<SessionBackend>>,
    approval: Arc<UserApproval>,
    tool_router: Arc<RwLock<ToolRouter<Self>>>,
    tool_notifier: Arc<ToolListNotifier>,
}

#[tool_router(router = tool_router)]
impl McpHandler {
    fn new(control: Arc<ComputerControl<SessionBackend>>, approval: Arc<UserApproval>) -> Self {
        let mut tool_router = Self::tool_router();
        for name in COMPUTER_TOOL_NAMES {
            tool_router.disable_route(name);
        }
        let tool_notifier = Arc::new(ToolListNotifier::default());
        let notifier = tool_notifier.clone();
        tool_router.set_notifier(move || notifier.route_changed());
        Self {
            control,
            approval,
            tool_router: Arc::new(RwLock::new(tool_router)),
            tool_notifier,
        }
    }

    async fn set_tools_enabled(&self, enabled: bool) {
        let mut tool_router = self.tool_router.write().await;
        self.tool_notifier.begin_batch();
        for name in COMPUTER_TOOL_NAMES {
            if enabled {
                tool_router.enable_route(name);
            } else {
                tool_router.disable_route(name);
            }
        }
        drop(tool_router);
        self.tool_notifier.finish_batch();
    }

    #[cfg(test)]
    async fn visible_tool_names(&self) -> Vec<String> {
        self.tool_router
            .read()
            .await
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    async fn approve_action(&self, action: String) -> Result<(), ComputerError> {
        self.control.backend.ensure_enabled().await?;
        self.approval.request(&action).await
    }

    #[tool(
        name = "computer_observe",
        description = "Capture a current PNG of one local macOS display or desktop-point region. Use this immediately before any visual computer action. The response includes the image and geometry metadata."
    )]
    async fn computer_observe(
        &self,
        Parameters(args): Parameters<ObserveArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        match self.control.observe(args).await {
            Ok(observation) => Ok(render_observation(observation)),
            Err(error) => Ok(tool_error("computer_observe", error)),
        }
    }

    #[tool(
        name = "computer_move",
        description = "Move the local pointer to a point in a fresh computer_observe image."
    )]
    async fn computer_move(
        &self,
        Parameters(args): Parameters<MoveArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!(
                "move the pointer to image point {:.0}, {:.0}",
                args.target.point.x, args.target.point.y
            ))
            .await
        {
            return Ok(tool_error("computer_move", error));
        }
        Ok(result_unit(
            "computer_move",
            self.control
                .execute_targeted(&args.target, |x, y| BackendAction::Move { x, y })
                .await,
        ))
    }

    #[tool(
        name = "computer_click",
        description = "Click a point in a fresh computer_observe image. The observation must not be expired and its display geometry must still match."
    )]
    async fn computer_click(
        &self,
        Parameters(args): Parameters<ClickArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!(
                "{:?}-click image point {:.0}, {:.0}",
                args.button, args.target.point.x, args.target.point.y
            ))
            .await
        {
            return Ok(tool_error("computer_click", error));
        }
        Ok(result_unit(
            "computer_click",
            self.control
                .execute_targeted(&args.target, |x, y| BackendAction::Click {
                    x,
                    y,
                    button: args.button,
                })
                .await,
        ))
    }

    #[tool(
        name = "computer_double_click",
        description = "Double-click a point in a fresh computer_observe image."
    )]
    async fn computer_double_click(
        &self,
        Parameters(args): Parameters<DoubleClickArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!(
                "double-{:?}-click image point {:.0}, {:.0}",
                args.button, args.target.point.x, args.target.point.y
            ))
            .await
        {
            return Ok(tool_error("computer_double_click", error));
        }
        Ok(result_unit(
            "computer_double_click",
            self.control
                .execute_targeted(&args.target, |x, y| BackendAction::DoubleClick {
                    x,
                    y,
                    button: args.button,
                })
                .await,
        ))
    }

    #[tool(
        name = "computer_drag",
        description = "Drag between two points in the same fresh computer_observe image."
    )]
    async fn computer_drag(
        &self,
        Parameters(args): Parameters<DragArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!(
                "drag from image point {:.0}, {:.0} to {:.0}, {:.0}",
                args.from.x, args.from.y, args.to.x, args.to.y
            ))
            .await
        {
            return Ok(tool_error("computer_drag", error));
        }
        Ok(result_unit(
            "computer_drag",
            self.control.execute_drag(&args).await,
        ))
    }

    #[tool(
        name = "computer_type",
        description = "Type literal text after a fresh computer_observe. Use computer_key for named keys and shortcuts."
    )]
    async fn computer_type(
        &self,
        Parameters(args): Parameters<TypeTextArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!("type {}", quote_for_approval(&args.text)))
            .await
        {
            return Ok(tool_error("computer_type", error));
        }
        Ok(result_unit(
            "computer_type",
            self.control.execute_text(&args).await,
        ))
    }

    #[tool(
        name = "computer_key",
        description = "Press one named key, optionally with modifiers, after a fresh computer_observe. Printable text belongs in computer_type."
    )]
    async fn computer_key(
        &self,
        Parameters(args): Parameters<KeyArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!("press {:?} with {:?}", args.key, args.modifiers))
            .await
        {
            return Ok(tool_error("computer_key", error));
        }
        Ok(result_unit(
            "computer_key",
            self.control.execute_key(&args).await,
        ))
    }

    #[tool(
        name = "computer_scroll",
        description = "Scroll at a point in a fresh computer_observe image. Positive and negative deltas represent opposite directions."
    )]
    async fn computer_scroll(
        &self,
        Parameters(args): Parameters<ScrollArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(error) = self
            .approve_action(format!(
                "scroll image point {:.0}, {:.0} by {:.0}, {:.0}",
                args.point.x, args.point.y, args.delta_x, args.delta_y
            ))
            .await
        {
            return Ok(tool_error("computer_scroll", error));
        }
        let target = TargetedPointArgs {
            observation_id: args.observation_id,
            point: args.point,
        };
        Ok(result_unit(
            "computer_scroll",
            self.control
                .execute_targeted(&target, |x, y| BackendAction::Scroll {
                    x,
                    y,
                    delta_x: args.delta_x,
                    delta_y: args.delta_y,
                })
                .await,
        ))
    }

    #[tool(
        name = "computer_wait",
        description = "Wait up to ten seconds after a fresh computer_observe, then observe again before another visual action."
    )]
    async fn computer_wait(
        &self,
        Parameters(args): Parameters<WaitArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Ok(result_unit("computer_wait", self.control.wait(&args).await))
    }
}

fn render_observation(observation: Observation) -> CallToolResult {
    let metadata = serde_json::to_string(&observation.metadata)
        .unwrap_or_else(|error| format!("could not serialize observation metadata: {error}"));
    CallToolResult::success(vec![
        Content::image(
            observation.image.data_base64,
            observation.metadata.mime_type,
        ),
        Content::text(metadata),
    ])
}

fn result_unit(operation: &str, result: Result<(), ComputerError>) -> CallToolResult {
    match result {
        Ok(()) => CallToolResult::success(vec![Content::text(format!("{operation} completed"))]),
        Err(error) => tool_error(operation, error),
    }
}

fn tool_error(operation: &str, error: ComputerError) -> CallToolResult {
    CallToolResult::error(vec![Content::text(format!("{operation} failed: {error}"))])
}

fn quote_for_approval(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut quoted = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().nth(MAX_CHARS).is_some() {
        quoted.push('…');
    }
    format!("{quoted:?}")
}

fn unix_millis() -> Result<u64, ComputerError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ComputerError::Backend(format!("read system clock: {error}")))
        .map(|duration| duration.as_millis() as u64)
}

/// In-process MCP endpoint for a primary ACP session. It stays connected on
/// macOS so enabling Computer Control in the built-in settings tab takes
/// effect in this session. While control is off it exposes no tools, holds no
/// app host, and has no macOS automation capability.
pub struct ToolServer {
    bridge: crate::mcp_bridge::BridgeServer,
    control: Arc<ComputerControl<SessionBackend>>,
    handler: McpHandler,
    revocation_watch: CancellationToken,
}

impl ToolServer {
    pub async fn start(
        event_tx: mpsc::UnboundedSender<UiEvent>,
        config_path: PathBuf,
    ) -> Result<Self> {
        let backend = Arc::new(SessionBackend::new(config_path));
        let control = Arc::new(ComputerControl::new(backend));
        let handler = McpHandler::new(control.clone(), Arc::new(UserApproval::new(event_tx)));
        let bridge = crate::mcp_bridge::BridgeServer::start(MCP_SERVER_NAME, handler.clone())
            .await
            .context("start computer MCP bridge")?;
        let revocation_watch = CancellationToken::new();
        let watch_cancel = revocation_watch.clone();
        let backend_for_watch = control.backend.clone();
        let handler_for_watch = handler.clone();
        let config_path = backend_for_watch.config_path.clone();
        tokio::spawn(async move {
            let mut config_stamp = config_file_stamp(&config_path).await;
            loop {
                tokio::select! {
                    _ = watch_cancel.cancelled() => break,
                    _ = tokio::time::sleep(CONFIG_REVOCATION_POLL_INTERVAL) => {
                        // Only a session with a live host needs cross-session
                        // revocation. Avoid parsing the full config while
                        // control is off, and parse it only after a save.
                        if !backend_for_watch.has_active_host().await {
                            continue;
                        }
                        let current_stamp = config_file_stamp(&config_path).await;
                        if current_stamp == config_stamp {
                            continue;
                        }
                        config_stamp = current_stamp;
                        if !backend_for_watch.persisted_enabled() {
                            let _ = backend_for_watch.disable().await;
                            handler_for_watch.set_tools_enabled(false).await;
                        }
                    }
                }
            }
        });
        Ok(Self {
            bridge,
            control,
            handler,
            revocation_watch,
        })
    }

    pub fn advertised(&self) -> &McpServer {
        self.bridge.advertised()
    }

    /// Start the private app host for a saved Computer Control preference
    /// without opening any OS prompt. The panel's `setup` action is the only
    /// path that requests permission.
    pub async fn activate(&self) -> ComputerControlStatus {
        match self.control.backend.activate().await {
            Ok(readiness) => {
                self.handler.set_tools_enabled(readiness.is_ready()).await;
                ComputerControlStatus {
                    enabled: true,
                    readiness: Some(readiness),
                    detail: None,
                }
            }
            Err(error) => {
                let _ = self.control.backend.disable().await;
                self.handler.set_tools_enabled(false).await;
                ComputerControlStatus {
                    enabled: true,
                    readiness: None,
                    detail: Some(error.to_string()),
                }
            }
        }
    }

    /// Start the host and ask Mjolnir Computer.app for every permission that
    /// is still missing.
    pub async fn set_up(&self) -> ComputerControlStatus {
        match self.control.backend.set_up().await {
            Ok(outcome) => {
                self.handler
                    .set_tools_enabled(outcome.readiness.is_ready())
                    .await;
                ComputerControlStatus {
                    enabled: true,
                    readiness: Some(outcome.readiness),
                    detail: outcome.requested_permission.map(permission_request_detail),
                }
            }
            Err(error) => {
                let _ = self.control.backend.disable().await;
                self.handler.set_tools_enabled(false).await;
                setup_failure_status(error)
            }
        }
    }

    /// Recreate Mjolnir Computer.app and read its fresh TCC state after the
    /// user changes Screen Recording or Accessibility in System Settings.
    pub async fn refresh(&self) -> ComputerControlStatus {
        if !self.control.backend.persisted_enabled() {
            let _ = self.control.backend.disable().await;
            self.handler.set_tools_enabled(false).await;
            return ComputerControlStatus::disabled();
        }
        match self.control.backend.refresh().await {
            Ok(readiness) => {
                self.handler.set_tools_enabled(readiness.is_ready()).await;
                ComputerControlStatus {
                    enabled: true,
                    readiness: Some(readiness),
                    detail: None,
                }
            }
            Err(error) => {
                // `activate` stores the new host before reading readiness.
                // A failed read therefore needs the same teardown as any
                // other failed host start, not just a hidden tool surface.
                let _ = self.control.backend.disable().await;
                self.handler.set_tools_enabled(false).await;
                ComputerControlStatus {
                    enabled: true,
                    readiness: None,
                    detail: Some(error.to_string()),
                }
            }
        }
    }

    pub async fn disable(&self) -> ComputerControlStatus {
        self.handler.set_tools_enabled(false).await;
        match self.control.backend.disable().await {
            Ok(()) => ComputerControlStatus::disabled(),
            Err(error) => ComputerControlStatus {
                enabled: false,
                readiness: None,
                detail: Some(format!(
                    "automation was disabled, but host shutdown failed: {error}"
                )),
            },
        }
    }

    pub async fn status(&self) -> ComputerControlStatus {
        let status = self.control.backend.status().await;
        self.handler
            .set_tools_enabled(status.readiness.is_some_and(PermissionReadiness::is_ready))
            .await;
        status
    }

    /// Exercise the complete native route without moving the pointer: capture
    /// once, post a mouse-move event at the current pointer location, then
    /// capture again.
    pub async fn verify(&self) -> ComputerControlStatus {
        let result = async {
            let initial = self
                .control
                .observe(ObserveArgs {
                    display_id: None,
                    region: None,
                    max_image_width: None,
                    max_image_height: None,
                })
                .await?;
            self.control
                .backend
                .execute(BackendAction::Verify, CancellationToken::new())
                .await?;
            self.control
                .observe(ObserveArgs {
                    display_id: Some(initial.metadata.display_id),
                    region: None,
                    max_image_width: None,
                    max_image_height: None,
                })
                .await
        }
        .await;
        let mut status = self.status().await;
        status.detail = Some(match result {
            Ok(_) => "Verified: Mjolnir Computer captured the display, posted a no-op pointer event, and captured again.".to_string(),
            Err(error) => format!("Verification failed: {error}"),
        });
        status
    }
}

fn setup_failure_status(error: ComputerError) -> ComputerControlStatus {
    // `main` persists the preference before setup so a concurrent session can
    // observe the user's choice. Returning disabled here makes that caller
    // clear the preference when setup itself failed; a nonexistent bundle or
    // failed launch must not make every future session attempt activation.
    ComputerControlStatus {
        enabled: false,
        readiness: None,
        detail: Some(error.to_string()),
    }
}

impl Drop for ToolServer {
    fn drop(&mut self) {
        self.revocation_watch.cancel();
        let backend = self.control.backend.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = backend.disable().await;
            });
        }
    }
}

fn computer_bundle_path() -> Result<PathBuf> {
    if let Some(bundle) = std::env::var_os("MJOLNIR_COMPUTER_BUNDLE") {
        return Ok(PathBuf::from(bundle));
    }
    let executable = std::env::current_exe().context("locate mj executable for computer host")?;
    let executable = executable
        .canonicalize()
        .with_context(|| format!("resolve mj executable {}", executable.display()))?;
    computer_bundle_path_for_executable(&executable)
}

fn computer_bundle_path_for_executable(executable: &Path) -> Result<PathBuf> {
    let parent = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("mj executable has no parent: {}", executable.display()))?;
    Ok(parent.join("Mjolnir Computer.app"))
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new(
            MCP_SERVER_NAME,
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(SERVER_GUIDANCE)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(
            self.tool_router.read().await.list_all(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        // ToolRouter awaits the entire tool body. Clone its immutable route
        // table before invoking so a long `computer_wait` never blocks the
        // disable or cancellation path from changing the visible tool set.
        let tool_router = self.tool_router.read().await.clone();
        tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.tool_notifier.set_peer(context.peer);
        std::future::ready(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::computer::{
        DesktopPoint, EncodedImage, HostLockState, ImagePoint, PermissionState, PixelSize,
        PointerButton, SourceRegion,
    };

    #[derive(Default)]
    struct MockBackend {
        actions: StdMutex<Vec<BackendAction>>,
    }

    #[async_trait::async_trait]
    impl ComputerBackend for MockBackend {
        async fn observe(
            &self,
            _request: ObserveArgs,
            _cancellation: CancellationToken,
        ) -> Result<Observation, ComputerError> {
            Ok(observation())
        }

        async fn permission_readiness(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<PermissionReadiness, ComputerError> {
            Ok(PermissionReadiness {
                screen_recording: PermissionState::Granted,
                accessibility: PermissionState::Granted,
            })
        }

        async fn current_display(
            &self,
            display_id: DisplayId,
            _cancellation: CancellationToken,
        ) -> Result<CurrentDisplay, ComputerError> {
            Ok(CurrentDisplay {
                display_id,
                origin: DesktopPoint { x: 0, y: 0 },
                pixel_size: PixelSize {
                    width: 100,
                    height: 100,
                },
                scale_x: 1.0,
                scale_y: 1.0,
            })
        }

        async fn host_lock_state(
            &self,
            _cancellation: CancellationToken,
        ) -> Result<HostLockState, ComputerError> {
            Ok(HostLockState::Unlocked)
        }

        async fn execute(
            &self,
            action: BackendAction,
            _cancellation: CancellationToken,
        ) -> Result<(), ComputerError> {
            self.actions.lock().unwrap().push(action);
            Ok(())
        }
    }

    fn observation() -> Observation {
        Observation {
            metadata: ObservationMetadata {
                observation_id: ObservationId("observation-1".to_string()),
                display_id: DisplayId("display-1".to_string()),
                display_origin: DesktopPoint { x: 0, y: 0 },
                display_pixel_size: PixelSize {
                    width: 100,
                    height: 100,
                },
                display_scale_x: 1.0,
                display_scale_y: 1.0,
                source_region: SourceRegion {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                returned_image_size: PixelSize {
                    width: 100,
                    height: 100,
                },
                mime_type: "image/png".to_string(),
                created_at_unix_ms: 0,
                expires_at_unix_ms: u64::MAX,
            },
            image: EncodedImage {
                data_base64: "AA==".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn pointer_actions_are_resolved_from_the_retained_observation() {
        let backend = Arc::new(MockBackend::default());
        let control = ComputerControl::new(backend.clone());
        let observed = control
            .observe(ObserveArgs {
                display_id: None,
                region: None,
                max_image_width: None,
                max_image_height: None,
            })
            .await
            .unwrap();
        control
            .execute_targeted(
                &TargetedPointArgs {
                    observation_id: observed.metadata.observation_id,
                    point: ImagePoint { x: 20.0, y: 30.0 },
                },
                |x, y| BackendAction::Click {
                    x,
                    y,
                    button: PointerButton::Left,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            backend.actions.lock().unwrap().as_slice(),
            &[BackendAction::Click {
                x: 20.0,
                y: 30.0,
                button: PointerButton::Left,
            }]
        );
    }

    #[test]
    fn tool_router_exposes_the_complete_control_surface() {
        let names = McpHandler::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "computer_click",
                "computer_double_click",
                "computer_drag",
                "computer_key",
                "computer_move",
                "computer_observe",
                "computer_scroll",
                "computer_type",
                "computer_wait",
            ]
        );
    }

    #[tokio::test]
    async fn disabled_control_is_absent_from_the_primary_tool_list() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        crate::config::Config::default().save(&config_path).unwrap();
        let backend = Arc::new(SessionBackend::new(config_path));
        let control = Arc::new(ComputerControl::new(backend));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let handler = McpHandler::new(control, Arc::new(UserApproval::new(event_tx)));

        assert!(handler.visible_tool_names().await.is_empty());
        handler.set_tools_enabled(true).await;
        assert_eq!(
            handler.visible_tool_names().await,
            COMPUTER_TOOL_NAMES
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        handler.set_tools_enabled(false).await;
        assert!(handler.visible_tool_names().await.is_empty());
    }

    #[tokio::test]
    async fn concurrent_tool_list_transitions_are_serialized() {
        use tokio::sync::Barrier;

        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        crate::config::Config::default().save(&config_path).unwrap();
        let backend = Arc::new(SessionBackend::new(config_path));
        let control = Arc::new(ComputerControl::new(backend));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let handler = McpHandler::new(control, Arc::new(UserApproval::new(event_tx)));
        handler.set_tools_enabled(true).await;

        // Keep both tasks queued at the router. The transition batch must
        // begin only after this write guard is acquired; otherwise the second
        // task hits the notifier's nested-batch assertion before either can
        // make progress.
        let router_guard = handler.tool_router.write().await;
        let barrier = Arc::new(Barrier::new(3));
        let first_handler = handler.clone();
        let first_barrier = barrier.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_handler.set_tools_enabled(false).await;
        });
        let second_handler = handler.clone();
        let second_barrier = barrier.clone();
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_handler.set_tools_enabled(false).await;
        });
        barrier.wait().await;
        tokio::task::yield_now().await;
        drop(router_guard);

        first.await.expect("first transition");
        second.await.expect("second transition");
        assert!(handler.visible_tool_names().await.is_empty());
    }

    #[tokio::test]
    async fn enabling_control_notifies_the_existing_mcp_connection() {
        use tokio::{
            io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
            net::TcpStream,
            time::{Duration, timeout},
        };

        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        crate::config::Config::default().save(&config_path).unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let server = ToolServer::start(event_tx, config_path).await.unwrap();
        let McpServer::Stdio(stdio) = server.advertised() else {
            panic!("computer bridge must advertise stdio");
        };
        let addr = stdio
            .args
            .iter()
            .skip_while(|arg| arg.as_str() != "--addr")
            .nth(1)
            .expect("bridge address");
        let token = &stdio
            .env
            .iter()
            .find(|variable| variable.name == crate::mcp_bridge::TOKEN_ENV)
            .expect("bridge token")
            .value;
        let stream = TcpStream::connect(addr).await.expect("connect bridge");
        let (read, mut write) = stream.into_split();
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}},
                "clientInfo": {"name": "computer-test", "version": "1"}
            }
        });
        write
            .write_all(format!("{token}\n{initialize}\n").as_bytes())
            .await
            .expect("initialize bridge");
        let mut lines = BufReader::new(read).lines();
        let response = lines
            .next_line()
            .await
            .expect("read initialize response")
            .expect("bridge remains open");
        let response: serde_json::Value = serde_json::from_str(&response).expect("JSON response");
        assert_eq!(response["id"], 1);
        write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
            )
            .await
            .expect("mark bridge initialized");

        // A request after the notification establishes that the server has
        // processed initialization and bound the router's peer notifier.
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .expect("request disabled tools");
        let response = lines
            .next_line()
            .await
            .expect("read disabled tools response")
            .expect("bridge remains open");
        let response: serde_json::Value = serde_json::from_str(&response).expect("JSON response");
        assert_eq!(response["id"], 2);
        assert_eq!(response["result"]["tools"], serde_json::json!([]));

        server.handler.set_tools_enabled(true).await;

        let notification = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .expect("tools/list_changed notification")
            .expect("read notification")
            .expect("bridge remains open");
        let notification: serde_json::Value =
            serde_json::from_str(&notification).expect("notification is JSON");
        assert_eq!(notification["method"], "notifications/tools/list_changed");

        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .expect("request tools");
        let response = lines
            .next_line()
            .await
            .expect("read tools response")
            .expect("bridge remains open");
        let response: serde_json::Value = serde_json::from_str(&response).expect("JSON response");
        let names = response["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>();
        assert_eq!(names, COMPUTER_TOOL_NAMES);
    }

    #[tokio::test]
    async fn a_long_running_tool_does_not_block_disablement() {
        use rmcp::{handler::server::router::tool::ToolRoute, model::Tool};
        use tokio::{
            io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
            net::TcpStream,
            sync::Notify,
            time::{Duration, timeout},
        };

        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        crate::config::Config::default().save(&config_path).unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let server = ToolServer::start(event_tx, config_path).await.unwrap();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        {
            let mut router = server.handler.tool_router.write().await;
            let route_entered = entered.clone();
            let route_release = release.clone();
            router.add_route(ToolRoute::new_dyn(
                Tool::new(
                    "test_blocking_tool",
                    "test-only blocking tool",
                    Arc::default(),
                ),
                move |_| {
                    let entered = route_entered.clone();
                    let release = route_release.clone();
                    Box::pin(async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(CallToolResult::success(vec![Content::text("finished")]))
                    })
                },
            ));
        }
        let McpServer::Stdio(stdio) = server.advertised() else {
            panic!("computer bridge must advertise stdio");
        };
        let addr = stdio
            .args
            .iter()
            .skip_while(|arg| arg.as_str() != "--addr")
            .nth(1)
            .expect("bridge address");
        let token = &stdio
            .env
            .iter()
            .find(|variable| variable.name == crate::mcp_bridge::TOKEN_ENV)
            .expect("bridge token")
            .value;
        let stream = TcpStream::connect(addr).await.expect("connect bridge");
        let (read, mut write) = stream.into_split();
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "computer-test", "version": "1"}
            }
        });
        write
            .write_all(format!("{token}\n{initialize}\n").as_bytes())
            .await
            .expect("initialize bridge");
        let mut lines = BufReader::new(read).lines();
        let _ = lines
            .next_line()
            .await
            .expect("read initialize response")
            .expect("bridge remains open");
        write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
            )
            .await
            .expect("mark bridge initialized");
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"test_blocking_tool\",\"arguments\":{}}}\n")
            .await
            .expect("call blocking tool");
        timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("tool body started");

        let handler = server.handler.clone();
        timeout(
            Duration::from_millis(100),
            tokio::spawn(async move { handler.set_tools_enabled(false).await }),
        )
        .await
        .expect("disablement must not wait for the tool body")
        .expect("disablement task");

        release.notify_one();
        let response = lines
            .next_line()
            .await
            .expect("read tool response")
            .expect("bridge remains open");
        let response: serde_json::Value = serde_json::from_str(&response).expect("JSON response");
        assert_eq!(response["id"], 2);
    }

    #[tokio::test]
    async fn a_new_session_has_no_computer_host_until_the_user_enables_it() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        crate::config::Config::default().save(&config_path).unwrap();
        let backend = SessionBackend::new(config_path);
        assert!(!backend.has_active_host().await);
        assert_eq!(backend.status().await, ComputerControlStatus::disabled());
        assert!(matches!(
            backend
                .observe(
                    ObserveArgs {
                        display_id: None,
                        region: None,
                        max_image_width: None,
                        max_image_height: None,
                    },
                    CancellationToken::new(),
                )
                .await,
            Err(ComputerError::ControlDisabled)
        ));
    }

    #[test]
    fn setup_requests_one_missing_permission_at_a_time() {
        let missing = PermissionReadiness {
            screen_recording: PermissionState::NotGranted,
            accessibility: PermissionState::NotGranted,
        };
        assert_eq!(
            next_missing_permission(missing),
            Some(ComputerPermission::ScreenRecording)
        );
        assert_eq!(
            next_missing_permission(PermissionReadiness {
                screen_recording: PermissionState::Granted,
                accessibility: PermissionState::NotGranted,
            }),
            Some(ComputerPermission::Accessibility)
        );
        assert_eq!(
            next_missing_permission(PermissionReadiness {
                screen_recording: PermissionState::Granted,
                accessibility: PermissionState::Granted,
            }),
            None
        );
    }

    #[test]
    fn setup_explains_that_refresh_restarts_the_host() {
        assert_eq!(
            permission_request_detail(ComputerPermission::ScreenRecording),
            "Screen Recording request sent. Complete it in System Settings, return here, then press r to restart Mjolnir Computer and recheck."
        );
    }

    #[test]
    fn failed_setup_does_not_leave_control_enabled() {
        let status = setup_failure_status(ComputerError::Backend("bundle unavailable".to_string()));

        assert!(!status.enabled);
        assert_eq!(status.readiness, None);
        assert_eq!(
            status.detail.as_deref(),
            Some("computer backend error: bundle unavailable")
        );
    }

    #[tokio::test]
    async fn config_stamp_changes_when_a_config_save_replaces_the_file() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.save(&config_path).unwrap();
        let first = config_file_stamp(&config_path).await;

        config.computer.enabled = true;
        config.save(&config_path).unwrap();
        let second = config_file_stamp(&config_path).await;

        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn a_saved_disable_stops_a_live_session_before_its_next_request() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.computer.enabled = true;
        config.save(&config_path).unwrap();

        let backend = SessionBackend::new(config_path.clone());
        backend.state.lock().await.enabled = true;
        assert!(backend.status().await.enabled);

        config.computer.enabled = false;
        config.save(&config_path).unwrap();

        assert_eq!(backend.status().await, ComputerControlStatus::disabled());
        assert!(!backend.state.lock().await.enabled);
    }

    #[test]
    fn approval_text_keeps_short_input_exact_and_bounds_long_input() {
        assert_eq!(quote_for_approval("hello"), "\"hello\"");
        let long = "x".repeat(400);
        let quoted = quote_for_approval(&long);
        assert!(quoted.ends_with("…\""));
        assert!(quoted.len() < long.len());
    }

    #[test]
    fn release_bundle_is_resolved_next_to_the_mj_executable() {
        assert_eq!(
            computer_bundle_path_for_executable(Path::new("/opt/mj/mj")).unwrap(),
            PathBuf::from("/opt/mj/Mjolnir Computer.app")
        );
    }
}
