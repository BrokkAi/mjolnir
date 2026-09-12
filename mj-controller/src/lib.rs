//! Hel's daemon-side controller: provisioning, session management, the web
//! server, and the surrounding host-side services.

mod claude_usage;
mod codex_usage;
mod grok_usage;
mod muse_usage;

pub mod compaction;
pub mod controller;
pub mod desktop;
pub mod dictation;
pub mod doctor;
pub mod image;
pub mod import;
pub mod quota;
pub mod readline;
pub mod recovery;
pub mod review_host;
pub mod review_settings;
pub mod server;
pub mod session_manager;
pub mod setup;
pub mod tailscale;
pub mod utility_llm;
pub mod worker_client;
pub mod worker_upgrade;

pub mod database;

pub mod targets;

pub mod termination;

pub mod daemon;
pub mod pollers;
pub mod server_runtime;
pub mod web_viewer;

pub mod checkpoint_transfer;

pub mod recovery_gate;
