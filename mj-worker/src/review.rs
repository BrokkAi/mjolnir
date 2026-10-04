//! Target-owned reviewer services.
#[cfg(unix)]
pub mod bifrost;
#[cfg(unix)]
pub(crate) mod capture;
pub mod mcp;
