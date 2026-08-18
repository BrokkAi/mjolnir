//! Frontend-neutral ACP session state.
//!
//! This module owns state shared by the terminal, print, and remote frontends.

use agent_client_protocol::schema::v1::SessionConfigOption;

use crate::app::Entry;
use crate::event::SessionConfigTarget;

#[derive(Debug, Default)]
pub struct SessionState {
    pub session_id: Option<String>,
    pub session_title: Option<String>,
    pub current_mode: Option<String>,
    pub session_config_options: Vec<SessionConfigOption>,
    pub session_config_targets: Vec<SessionConfigTarget>,
    pub prompt_images_supported: bool,
    pub session_fork_supported: bool,
    pub transcript: Vec<Entry>,
}
