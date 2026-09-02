//! Hel's reusable controller, worker, and session-management core.

pub mod clock;
pub mod termination;

pub mod hel_acp;
pub mod hel_archive;
pub mod hel_checkpoint;
pub mod hel_config;
pub mod hel_credentials;
pub mod hel_database;
pub mod hel_diff;
pub mod hel_elicitation;
pub mod hel_local_git;
pub mod hel_project_memory;
pub mod hel_projection;
pub mod hel_resources;
pub mod hel_review;
pub mod hel_second_opinion;
pub mod hel_skills;
pub mod hel_state;
pub mod hel_subprocess;
pub mod hel_targets;
pub mod hel_terminal;
pub mod hel_test_hooks;
pub mod hel_transcript;
pub mod hel_worker;
pub mod hel_worker_launch;
pub mod hel_worker_protocol;
pub mod hel_workspace;
