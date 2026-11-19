//! Tests of mj-controller behaviour that check the result through the daemon
//! layer, such as the viewer snapshot or the daemon's delegation delivery.

mod image_pull_gate;
mod lifecycle;
// Every test and helper in these three starts a Unix stand-in relay or shell fixture.
#[cfg(unix)]
mod move_session;
mod profile_catalog;
mod resume;
#[cfg(unix)]
mod session_manager;
#[cfg(unix)]
mod worker_client;
