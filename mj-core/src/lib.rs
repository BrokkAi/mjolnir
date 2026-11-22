//! Mjolnir shared contracts, formats, pure projections and reusable utilities.

pub mod clock;

pub mod acp;
pub mod activity;
pub mod archive;
pub mod assessment;
pub mod attachment;
pub mod aws_credentials;
pub mod backoff;
pub mod bounded_frame;
pub mod codex_catalog;
pub mod codex_provider;
pub mod config;
pub mod confined_path;
pub mod credentials;
pub mod diagnostic;
pub mod diff;
pub mod draft;
pub mod elicitation;
pub mod event_outcome;
pub mod file_lock;
pub mod github_item;
pub mod github_rate_limit;
pub mod github_reference;
pub mod go;
pub mod goal;
#[doc(hidden)]
pub mod golden;
pub mod guardian;
pub mod harness_runtime;
pub mod hex;
pub mod history;
pub mod jev;
pub mod local_git;
pub mod local_sockets;
pub mod lock_wait;
pub mod logging;
pub mod mailbox;
pub mod model_catalog;
pub mod move_workspace;
pub mod native_agent;
pub mod path_completion;
pub mod path_input;
pub mod placement;
pub mod profile_capabilities;
pub mod project_catalog;
pub mod project_memory;
pub mod project_picker;
pub mod redact;
pub mod refusal;
pub mod relay;
pub mod remote_git;
pub mod repository;
pub mod retry_after;
pub mod review;
pub mod runtime;
pub mod session_instructions;
pub mod session_keep;
pub mod skills;
pub mod slopcop;
pub mod snapshot_map;
pub mod state;
pub mod storage;
pub mod subagent;
pub mod subprocess;
pub mod suspend_consent;
pub mod target_path;
pub mod targets;
pub mod test_hooks;
pub mod text;
pub mod transcript;
pub mod usage;
pub mod work_facts;
pub mod worker_build;
pub mod worker_launch;
pub mod worker_protocol;
pub mod worker_suspend;
pub mod workspace;

pub mod continuation;

pub mod cpu_usage;

pub mod attention;

pub mod notification_message;

pub mod packed_text;
