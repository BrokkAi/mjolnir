//! Target-owned reviewer services.
pub mod bifrost;
#[cfg(unix)]
pub(crate) mod capture;
pub mod mcp;
