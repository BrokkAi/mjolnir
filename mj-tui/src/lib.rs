//! Ratatui terminal frontend and interactive application state for Mjolnir.

pub mod app;
pub mod auth;
pub mod clipboard;
pub mod ink;
pub mod labels;
pub mod menu;
pub mod notifications;
pub mod onboarding;
pub mod palette;
pub mod qr;
pub mod session;
pub mod session_state;
pub mod settings;
pub mod speech;
pub mod spinner;
pub mod term;
pub mod terminal_palette;
pub mod termination;
pub mod text;
pub mod ui;

pub mod acp {
    pub use mj_core::acp::*;
}
pub mod agent_usage {
    pub use mj_core::agent_usage::*;
}
pub mod claude_usage {
    pub use mj_core::claude_usage::*;
}
pub mod codex_usage {
    pub use mj_core::codex_usage::*;
}
pub mod config {
    pub use mj_core::config::*;
}
pub mod deepswe {
    pub use mj_agents::deepswe::*;
}
pub mod event {
    pub use mj_core::event::*;
}
pub mod keep_awake {
    pub use mj_core::keep_awake::*;
}
pub mod memory {
    pub use mj_core::memory::*;
}
pub mod pull_request {
    pub use mj_agents::pull_request::*;
}
pub mod roster {
    pub use mj_core::roster::*;
}
pub mod terminal_output {
    pub use mj_core::terminal_output::*;
}
pub mod usage_format {
    pub use mj_core::usage_format::*;
}
pub mod workflow {
    pub use mj_core::workflow::*;
}

pub mod version {
    pub const MJOLNIR_VERSION: &str = env!("CARGO_PKG_VERSION");

    pub fn mjolnir_version_label() -> String {
        format!("mjolnir v{MJOLNIR_VERSION}")
    }
}

pub type Terminal = ratatui::Terminal<term::TrackedBackend<std::io::Stdout>>;
