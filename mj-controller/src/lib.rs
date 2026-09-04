//! Hel's daemon-side controller: provisioning, session management, the web
//! server, and the surrounding host-side services.

mod claude_usage;
mod codex_usage;
mod grok_usage;

pub mod hel_compaction;
pub mod hel_controller;
pub mod hel_desktop;
pub mod hel_doctor;
pub mod hel_import;
pub use hel::hel_git_proxy;
pub mod hel_quota;
pub mod hel_readline;
pub mod hel_recovery;
pub mod hel_review_host;
pub mod hel_server;
pub mod hel_session_manager;
pub mod hel_setup;
pub mod hel_tailscale;
pub mod hel_utility_llm;
pub mod hel_worker_client;
pub mod hel_worker_upgrade;
