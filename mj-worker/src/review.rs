//! Target-owned reviewer services.
#[cfg(unix)]
mod attribution;
#[cfg(unix)]
pub(crate) mod capture;

#[cfg(unix)]
#[allow(dead_code)] // Standalone git helpers; runtime wiring follows separately.
pub(crate) mod hook;
