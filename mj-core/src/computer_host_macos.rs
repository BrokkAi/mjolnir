//! macOS launcher and proxy for the Mjolnir Computer.app host.
//!
//! This process never calls capture or event-injection APIs. It creates a
//! private launch descriptor, starts the separate app bundle, then speaks the
//! authenticated session IPC defined in [`crate::computer_host`].

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use async_trait::async_trait;
use tokio::{net::UnixStream, sync::Mutex, time::sleep};
use tokio_util::sync::CancellationToken;

use crate::{
    computer::{
        BackendAction, ComputerBackend, ComputerError, ComputerPermission, HostLockState,
        Observation, ObserveArgs, PermissionReadiness,
    },
    computer_host::{
        HostCallError, HostCapability, HostClient, HostLaunchDescriptor, HostRequest, HostResponse,
        HostSessionId,
    },
};

const HOST_START_TIMEOUT: Duration = Duration::from_secs(5);
const HOST_RETRY_DELAY: Duration = Duration::from_millis(50);
const BUNDLE_INFO_PATH: &str = "Contents/Info.plist";
const BUNDLE_EXECUTABLE_PATH: &str = "Contents/MacOS/mj-computer-host";

/// A live, session-scoped proxy to Mjolnir Computer.app. Keeping this value
/// alive keeps a control connection open; dropping it causes the host to
/// revoke the session immediately when it observes EOF.
pub struct MacosComputerHost {
    control: Mutex<HostClient<UnixStream>>,
    socket_path: PathBuf,
    session_id: HostSessionId,
    capability: HostCapability,
    _private_dir: tempfile::TempDir,
}

impl MacosComputerHost {
    /// Starts one app-host session. `bundle` must be the Mjolnir Computer.app
    /// that shipped next to the `mj` release artifact, not an arbitrary helper
    /// executable launched by the terminal.
    pub async fn launch(bundle: &Path, session_id: HostSessionId) -> Result<Self, ComputerError> {
        validate_bundle(bundle)?;
        let private_dir = tempfile::Builder::new()
            .prefix("mjolnir-computer-")
            .tempdir()
            .map_err(|error| {
                ComputerError::Backend(format!("create computer host directory: {error}"))
            })?;
        fs::set_permissions(private_dir.path(), fs::Permissions::from_mode(0o700)).map_err(
            |error| ComputerError::Backend(format!("protect computer host directory: {error}")),
        )?;
        let capability = HostCapability::generate().map_err(|error| {
            ComputerError::Backend(format!("create computer capability: {error}"))
        })?;
        let socket_path = private_dir.path().join("host.sock");
        let descriptor_path = private_dir.path().join("launch.json");
        write_private_descriptor(
            &descriptor_path,
            &HostLaunchDescriptor {
                socket_path: socket_path.clone(),
                session_id: session_id.clone(),
                capability: capability.clone(),
            },
        )?;
        launch_bundle(bundle, &descriptor_path)?;
        let stream = connect_until_ready(&socket_path).await?;
        let control = HostClient::authenticate(stream, session_id.clone(), capability.clone())
            .await
            .map_err(|error| {
                ComputerError::Backend(format!("authenticate computer host: {error}"))
            })?;
        Ok(Self {
            control: Mutex::new(control),
            socket_path,
            session_id,
            capability,
            _private_dir: private_dir,
        })
    }

    /// A second authenticated connection is used only for prompt revocation
    /// while the control connection is busy in capture or compound input.
    pub async fn shutdown(&self) -> Result<(), ComputerError> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|error| {
                ComputerError::Backend(format!("connect to computer host for shutdown: {error}"))
            })?;
        let mut cancellation =
            HostClient::authenticate(stream, self.session_id.clone(), self.capability.clone())
                .await
                .map_err(|error| {
                    ComputerError::Backend(format!("authenticate computer host shutdown: {error}"))
                })?;
        match cancellation.call(HostRequest::Shutdown).await {
            Ok(HostResponse::Completed)
            | Err(crate::computer_host::HostClientError::Host(HostCallError::Revoked)) => Ok(()),
            Ok(response) => Err(ComputerError::Backend(format!(
                "unexpected computer host shutdown response: {}",
                response_name(&response)
            ))),
            Err(error) => Err(ComputerError::Backend(format!(
                "shutdown computer host: {error}"
            ))),
        }
    }

    /// Ask Mjolnir Computer.app to show the native authorization prompt for a
    /// single missing capability. The CLI itself does not call TCC APIs.
    pub async fn request_permission(
        &self,
        permission: ComputerPermission,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        match self
            .call(HostRequest::RequestPermission(permission), cancellation)
            .await?
        {
            HostResponse::PermissionRequested(readiness) => Ok(readiness),
            response => Err(unexpected_response("permission request", &response)),
        }
    }

    async fn call(
        &self,
        request: HostRequest,
        cancellation: CancellationToken,
    ) -> Result<HostResponse, ComputerError> {
        tokio::select! {
            response = async {
                self.control
                    .lock()
                    .await
                    .call(request)
                    .await
            } => response.map_err(|error| ComputerError::Backend(format!("computer host request: {error}"))),
            _ = cancellation.cancelled() => {
                let _ = self.shutdown().await;
                Err(ComputerError::Cancelled)
            }
        }
    }
}

#[async_trait]
impl ComputerBackend for MacosComputerHost {
    async fn observe(
        &self,
        request: ObserveArgs,
        cancellation: CancellationToken,
    ) -> Result<Observation, ComputerError> {
        match self
            .call(HostRequest::Observe(request), cancellation)
            .await?
        {
            HostResponse::Observation(observation) => Ok(observation),
            response => Err(unexpected_response("observe", &response)),
        }
    }

    async fn permission_readiness(
        &self,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        match self
            .call(HostRequest::PermissionReadiness, cancellation)
            .await?
        {
            HostResponse::PermissionReadiness(readiness) => Ok(readiness),
            response => Err(unexpected_response("permission readiness", &response)),
        }
    }

    async fn request_permission(
        &self,
        permission: ComputerPermission,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        MacosComputerHost::request_permission(self, permission, cancellation).await
    }

    async fn host_lock_state(
        &self,
        cancellation: CancellationToken,
    ) -> Result<HostLockState, ComputerError> {
        match self.call(HostRequest::HostLockState, cancellation).await? {
            HostResponse::HostLockState(lock_state) => Ok(lock_state),
            response => Err(unexpected_response("lock state", &response)),
        }
    }

    async fn execute(
        &self,
        action: BackendAction,
        cancellation: CancellationToken,
    ) -> Result<(), ComputerError> {
        match self
            .call(HostRequest::Execute(action), cancellation)
            .await?
        {
            HostResponse::Completed => Ok(()),
            response => Err(unexpected_response("input action", &response)),
        }
    }
}

fn validate_bundle(bundle: &Path) -> Result<(), ComputerError> {
    if bundle
        .extension()
        .is_none_or(|extension| extension != "app")
        || !bundle.join(BUNDLE_INFO_PATH).is_file()
        || !bundle.join(BUNDLE_EXECUTABLE_PATH).is_file()
    {
        return Err(ComputerError::Backend(format!(
            "Mjolnir Computer.app is missing or incomplete at {}",
            bundle.display()
        )));
    }
    Ok(())
}

fn write_private_descriptor(
    path: &Path,
    descriptor: &HostLaunchDescriptor,
) -> Result<(), ComputerError> {
    let bytes = serde_json::to_vec(descriptor).map_err(|error| {
        ComputerError::Backend(format!("serialize computer host descriptor: {error}"))
    })?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            ComputerError::Backend(format!("create computer host descriptor: {error}"))
        })?;
    file.write_all(&bytes).map_err(|error| {
        ComputerError::Backend(format!("write computer host descriptor: {error}"))
    })?;
    file.sync_all()
        .map_err(|error| ComputerError::Backend(format!("sync computer host descriptor: {error}")))
}

fn launch_bundle(bundle: &Path, descriptor_path: &Path) -> Result<(), ComputerError> {
    let status = Command::new("/usr/bin/open")
        .arg("-n")
        .arg(bundle)
        .arg("--args")
        .arg("--launch-descriptor")
        .arg(descriptor_path)
        .status()
        .map_err(|error| ComputerError::Backend(format!("launch Mjolnir Computer.app: {error}")))?;
    if !status.success() {
        return Err(ComputerError::Backend(format!(
            "launch Mjolnir Computer.app exited with {status}"
        )));
    }
    Ok(())
}

async fn connect_until_ready(path: &Path) -> Result<UnixStream, ComputerError> {
    let deadline = tokio::time::Instant::now() + HOST_START_TIMEOUT;
    let mut last_error = None;
    while tokio::time::Instant::now() < deadline {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
        sleep(HOST_RETRY_DELAY).await;
    }
    Err(ComputerError::Backend(format!(
        "Mjolnir Computer.app did not become ready within {} seconds{}",
        HOST_START_TIMEOUT.as_secs(),
        last_error
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    )))
}

fn response_name(response: &HostResponse) -> &'static str {
    match response {
        HostResponse::Hello { .. } => "hello",
        HostResponse::PermissionReadiness(_) => "permission readiness",
        HostResponse::PermissionRequested(_) => "permission requested",
        HostResponse::HostLockState(_) => "lock state",
        HostResponse::Observation(_) => "observation",
        HostResponse::Completed => "completed",
        HostResponse::Error(_) => "error",
    }
}

fn unexpected_response(operation: &str, response: &HostResponse) -> ComputerError {
    ComputerError::Backend(format!(
        "unexpected Mjolnir Computer response to {operation}: {}",
        response_name(response)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_app_bundle_is_never_accepted_as_the_automation_host() {
        let temporary = tempfile::tempdir().unwrap();
        let bundle = temporary.path().join("Mjolnir Computer.app");
        fs::create_dir(&bundle).unwrap();
        assert!(matches!(
            validate_bundle(&bundle),
            Err(ComputerError::Backend(_))
        ));
    }

    #[test]
    fn descriptor_is_created_private_and_cannot_be_replaced() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("launch.json");
        let descriptor = HostLaunchDescriptor {
            socket_path: temporary.path().join("host.sock"),
            session_id: HostSessionId("session".to_string()),
            capability: HostCapability::generate().unwrap(),
        };
        write_private_descriptor(&path, &descriptor).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        assert!(write_private_descriptor(&path, &descriptor).is_err());
    }
}
