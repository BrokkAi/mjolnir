//! Hang guards shared by unit and real-worker integration fixtures.

/// Interpreter startup and temporary-file I/O can exceed five seconds when
/// worker suites run concurrently. This is a hang guard, not an application SLA.
pub(crate) const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
