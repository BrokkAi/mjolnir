//! Where turn review runs: the host that feeds the review driver, its state,
//! and the environment trait it reviews through (`review_host`).
//!
//! It builds on the session runtime and the store. The daemon supplies the
//! production environment, which reads the daemon's own state; the host does
//! not know the daemon.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

pub mod review_host;
