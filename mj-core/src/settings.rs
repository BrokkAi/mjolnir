//! Frontend-neutral `/mjconfig` catalog rules.
//!
//! The terminal and web clients render their controls differently, but they
//! must agree on the panels and on which ACP options are safe to expose for a
//! seat. Keep those product rules here instead of copying them into a frontend.

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
};

use crate::roster::AdapterKind;

/// Built-in ACP servers whose enablement belongs in `/mjconfig`.
///
/// A platform adapter is deliberately absent: it can be the only launchable
/// route on that build, so treating it as disableable would be misleading.
pub const CONFIGURABLE_ACP_SERVERS: [&str; 2] = ["codex-acp", "claude-acp"];

pub fn is_configurable_acp_server(id: &str) -> bool {
    CONFIGURABLE_ACP_SERVERS.contains(&id)
}

/// Top-level `/mjconfig` panels shared by every interactive frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Team,
    Agents,
    Reviewer,
    Subagents,
    AcpServers,
    Input,
    Appearance,
}

impl SettingsTab {
    pub const ALL: [Self; 7] = [
        Self::Team,
        Self::Agents,
        Self::Reviewer,
        Self::Subagents,
        Self::AcpServers,
        Self::Input,
        Self::Appearance,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Agents => "agents",
            Self::Reviewer => "reviewer",
            Self::Subagents => "subagents",
            Self::AcpServers => "servers",
            Self::Input => "input",
            Self::Appearance => "appearance",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Team => "Team",
            Self::Agents => "Agent",
            Self::Reviewer => "Reviewer",
            Self::Subagents => "Subagents",
            Self::AcpServers => "ACP Servers",
            Self::Input => "Input",
            Self::Appearance => "Appearance",
        }
    }
}

/// A role whose saved session defaults are being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDefaultsSeat {
    Primary,
    Review,
    Subagents,
}

/// Whether a discovered ACP option belongs in this seat's `/mjconfig` panel.
///
/// The delegated Codex and Claude `mode` control is the provider's permission
/// mode. It is owned by the explicit reviewer/subagent Permissions setting,
/// rather than a low-level session-default override. The primary agent retains
/// the option because it has no separate permission preset.
pub fn session_option_is_editable(
    seat: SessionDefaultsSeat,
    adapter_kind: AdapterKind,
    option: &SessionConfigOption,
) -> bool {
    if !matches!(option.kind, SessionConfigKind::Select(_))
        || matches!(option.category, Some(SessionConfigOptionCategory::Model))
    {
        return false;
    }

    let permissions_own_mode =
        matches!(
            seat,
            SessionDefaultsSeat::Review | SessionDefaultsSeat::Subagents
        ) && matches!(adapter_kind, AdapterKind::Codex | AdapterKind::Claude);
    !(permissions_own_mode && option.id.to_string() == "mode")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionConfigSelectOption;

    #[test]
    fn catalog_includes_the_input_panel() {
        assert_eq!(SettingsTab::ALL[5], SettingsTab::Input);
        assert_eq!(SettingsTab::Input.id(), "input");
        assert_eq!(SettingsTab::Input.label(), "Input");
    }

    #[test]
    fn only_builtin_servers_are_configurable() {
        assert!(is_configurable_acp_server("codex-acp"));
        assert!(is_configurable_acp_server("claude-acp"));
        assert!(!is_configurable_acp_server("anvil"));
    }

    #[test]
    fn delegated_permissions_own_codex_and_claude_mode() {
        let mode = SessionConfigOption::select(
            "mode",
            "Mode",
            "agent",
            vec![SessionConfigSelectOption::new("agent", "Agent")],
        );
        let effort = SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        );

        assert!(session_option_is_editable(
            SessionDefaultsSeat::Primary,
            AdapterKind::Codex,
            &mode,
        ));
        for seat in [SessionDefaultsSeat::Review, SessionDefaultsSeat::Subagents] {
            assert!(!session_option_is_editable(seat, AdapterKind::Codex, &mode));
            assert!(!session_option_is_editable(
                seat,
                AdapterKind::Claude,
                &mode
            ));
            assert!(session_option_is_editable(
                seat,
                AdapterKind::Codex,
                &effort
            ));
        }
    }
}
