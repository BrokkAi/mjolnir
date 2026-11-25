//! Managed harness installation and target environment discovery for Mjolnir.

#[cfg(unix)]
pub mod harness_install;
pub mod login_environment;
pub mod program_path;
pub mod runtime_manifests;
pub mod session_image;

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();
