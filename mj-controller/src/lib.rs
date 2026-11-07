//! Hel's daemon-side controller: provisioning, session management, the web
//! server, and the surrounding host-side services.

mod aws_assume_role;
mod aws_credentials;
mod aws_selection;
mod claude_usage;
mod codex_usage;
mod grok_usage;
mod kimi_auth;
mod muse_usage;
mod oauth_lock;
mod quota_runtime;
mod zai_usage;

pub mod compaction;
pub mod controller;
pub mod desktop;
pub mod doctor;
pub(crate) mod handoff;
mod host_fs;
pub mod image;
pub mod import;
pub mod opencode_logins;
pub mod profile_health;
pub mod profile_login;
pub(crate) mod profile_scheduler;
pub(crate) mod project_catalog;
pub mod project_picker;
pub mod quota;
pub mod readline;
pub mod recovery;
pub mod review_host;
pub(crate) mod review_selection;
pub mod server;
pub mod session_manager;
pub mod sessionwiki;
pub mod setup;
mod streamed_child;
pub mod tailscale;
pub mod utility_llm;
pub mod worker_client;
pub(crate) mod worker_lifecycle;
pub mod worker_upgrade;

pub mod database;

pub mod target_storage;
pub mod targets;

pub mod termination;

pub mod daemon;
pub mod node_registry;
pub mod pollers;
pub(crate) mod project_memory_merge;
pub mod server_runtime;
pub mod web_viewer;

pub mod checkpoint_transfer;

pub mod image_pull_gate;
pub mod recovery_gate;

mod continuation;
pub mod github_item_verdict;
mod github_watch;
mod jev_transport;
mod mailbox_outbox;
#[cfg(test)]
mod test_log;
mod upgrade;
