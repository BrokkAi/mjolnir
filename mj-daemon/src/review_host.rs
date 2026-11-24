//! The daemon's side of turn review. The host, its state and the environment
//! it runs against are `mj_review_host::review_host`; what is here is the
//! environment the daemon gives it, which reads the daemon's runtime state,
//! and the constructors that start a host on that environment.

use std::sync::Arc;

use mj_controller::session_manager::SessionManagerControl;

pub use mj_review_host::review_host::*;

mod controller_environment;
pub(crate) mod selection;
pub use controller_environment::*;
pub(crate) use controller_environment::{SharedProfileCatalog, SharedReviewerRuntime};

/// Starts the host's task, reviewing through the real controller.
#[must_use]
pub fn spawn(control: SessionManagerControl, config: ReviewConfigSource) -> TurnReviewHost {
    spawn_notifying(control, config, Arc::new(|| {}), None, Default::default())
}

/// Starts the production host and calls `changed` whenever a surface view
/// is added, changed, or removed. A review waits behind the background
/// work `background` coordinates, such as a recovery copy, rather than
/// racing it for the session.
#[must_use]
pub(crate) fn spawn_notifying(
    control: SessionManagerControl,
    config: ReviewConfigSource,
    changed: Arc<dyn Fn() + Send + Sync>,
    background: Option<Arc<mj_controller::recovery_gate::RecoveryGate>>,
    runtime: SharedReviewerRuntime,
) -> TurnReviewHost {
    TurnReviewHost::spawn_in_notifying(
        control.clone(),
        config,
        Arc::new(ControllerEnvironment {
            manager: Some(control),
            runtime,
            background,
        }),
        changed,
    )
}
