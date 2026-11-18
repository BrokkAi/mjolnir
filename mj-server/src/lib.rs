//! The Mjolnir daemon's HTTP API and web viewer server: routes, request and
//! response types, authentication and the embedded viewer assets. The daemon
//! (`mj-daemon`) implements the API's backend traits over its live state and
//! re-exports this module as `mj_daemon::server`.
//!
//! This is a separate crate so that an edit to the daemon does not recompile
//! the server, and the two test suites build as separate units.

pub mod server;
