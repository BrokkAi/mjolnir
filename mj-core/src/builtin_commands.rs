//! Single source of truth for Mjolnir-owned slash commands.
//!
//! Membership lives in three lists: [`SHARED_COMMANDS`] is advertised by both
//! the TUI and the web viewer, [`TUI_ONLY_COMMANDS`] only by the TUI, and
//! [`WEB_ONLY_COMMANDS`] only by the web viewer. Moving a command between
//! surfaces means moving its entry between lists (and giving the new surface
//! a handler for it). Each surface renders the specs into its own command
//! type and keeps ownership of dispatch, of where conditional commands
//! (fork/load/side) sit in its list, and of side-conversation composition.

pub const NEW_COMMAND: &str = "new";
pub const CLEAR_COMMAND: &str = "clear";
pub const COMPACT_COMMAND: &str = "compact";
pub const NUDGE_COMMAND: &str = "nudge";
pub const LOAD_COMMAND: &str = "load";
pub const FORK_COMMAND: &str = "fork";
pub const SIDE_COMMAND: &str = "side";
pub const EXPORT_COMMAND: &str = "export";
pub const DIFF_COMMAND: &str = "diff";
pub const MJCONFIG_COMMAND: &str = "mjconfig";
pub const MODEL_COMMAND: &str = "model";
pub const EFFORT_COMMAND: &str = "effort";
pub const AGENTS_COMMAND: &str = "agents";
pub const SUBAGENTS_COMMAND: &str = "subagents";
pub const DISCRETE_REVIEW_COMMAND: &str = "discrete-review";
pub const ADVERSARIAL_REVIEW_COMMAND: &str = "adversarial-review";
pub const TERMINALS_COMMAND: &str = "terminals";
pub const MEMORY_COMMAND: &str = "memory";
pub const EXIT_COMMAND: &str = "exit";
/// Retired command name kept reserved so an agent command cannot shadow the
/// "this was renamed" notice.
pub const RETIRED_REVIEW_COMMAND: &str = "review";

/// A command advertised on both surfaces. Descriptions differ where the
/// surface behavior differs (e.g. `/export` downloads in the browser).
pub struct SharedCommand {
    pub name: &'static str,
    pub tui_description: &'static str,
    pub web_description: &'static str,
    /// Argument hint the web composer renders after the command name.
    pub web_input_hint: Option<&'static str>,
}

/// A command advertised on a single surface.
pub struct SurfaceCommand {
    pub name: &'static str,
    pub description: &'static str,
    /// Argument hint the web composer renders; unused by the TUI.
    pub input_hint: Option<&'static str>,
}

/// Commands both surfaces advertise, in display order.
pub const SHARED_COMMANDS: &[SharedCommand] = &[
    SharedCommand {
        name: NEW_COMMAND,
        tui_description: "start a new session",
        web_description: "start a new web session",
        web_input_hint: None,
    },
    SharedCommand {
        name: CLEAR_COMMAND,
        tui_description: "start a fresh session with the current agent",
        web_description: "start a fresh session with the same agent",
        web_input_hint: None,
    },
    SharedCommand {
        name: COMPACT_COMMAND,
        tui_description: "compact the primary agent's session where supported",
        web_description: "compact the primary agent's session where supported",
        web_input_hint: None,
    },
    SharedCommand {
        name: LOAD_COMMAND,
        tui_description: "load a previous session into the current primary",
        web_description: "load a previous session",
        web_input_hint: None,
    },
    SharedCommand {
        name: EXPORT_COMMAND,
        tui_description: "export primary transcript; add full for nested agents",
        web_description: "download this transcript as markdown",
        web_input_hint: None,
    },
    SharedCommand {
        name: MJCONFIG_COMMAND,
        tui_description: "configure the team, reviewer, subagents, ACP servers, and appearance",
        web_description: "open the configuration editor",
        web_input_hint: None,
    },
    SharedCommand {
        name: MODEL_COMMAND,
        tui_description: "change the active session model without starting a new session",
        web_description: "change the active session model without starting a new session",
        web_input_hint: None,
    },
    SharedCommand {
        name: EFFORT_COMMAND,
        tui_description: "change the active session reasoning effort without starting a new session",
        web_description: "change the active session reasoning effort without starting a new session",
        web_input_hint: None,
    },
    SharedCommand {
        name: DISCRETE_REVIEW_COMMAND,
        tui_description: "run the configured discrete review; add quick or extended to override its tier",
        web_description: "run the configured discrete review",
        web_input_hint: Some("recent|uncommitted|head [quick|extended]"),
    },
    SharedCommand {
        name: ADVERSARIAL_REVIEW_COMMAND,
        tui_description: "alias for discrete-review",
        web_description: "alias for discrete-review",
        web_input_hint: Some("recent|uncommitted|head [quick|extended]"),
    },
    SharedCommand {
        name: SIDE_COMMAND,
        tui_description: "open an isolated ephemeral conversation",
        web_description: "open an isolated ephemeral conversation",
        web_input_hint: Some("optional question"),
    },
    SharedCommand {
        name: FORK_COMMAND,
        tui_description: "fork the current session (unstable ACP extension)",
        web_description: "fork the current session",
        web_input_hint: None,
    },
];

/// Commands only the TUI advertises, in display order.
pub const TUI_ONLY_COMMANDS: &[SurfaceCommand] = &[
    SurfaceCommand {
        name: NUDGE_COMMAND,
        description: "ask a quiet active runtime to report status and continue",
        input_hint: None,
    },
    SurfaceCommand {
        name: AGENTS_COMMAND,
        description: "show active model selections and usage",
        input_hint: None,
    },
    SurfaceCommand {
        name: SUBAGENTS_COMMAND,
        description: "inspect implementation and review agent transcripts",
        input_hint: None,
    },
    SurfaceCommand {
        name: TERMINALS_COMMAND,
        description: "view terminals the agent started, including ones still running",
        input_hint: None,
    },
    SurfaceCommand {
        name: DIFF_COMMAND,
        description: "show workspace changes against HEAD",
        input_hint: None,
    },
    SurfaceCommand {
        name: MEMORY_COMMAND,
        description: "list and manage persistent memories (usage: /memory [add|forget|on|off|use|generate|clear])",
        input_hint: None,
    },
    SurfaceCommand {
        name: EXIT_COMMAND,
        description: "quit Mjolnir",
        input_hint: None,
    },
];

/// Commands only the web viewer advertises, in display order.
pub const WEB_ONLY_COMMANDS: &[SurfaceCommand] = &[];

/// Look up a shared command spec by name.
pub fn shared_command(name: &str) -> Option<&'static SharedCommand> {
    SHARED_COMMANDS.iter().find(|command| command.name == name)
}

/// Look up a TUI-only command spec by name.
pub fn tui_only_command(name: &str) -> Option<&'static SurfaceCommand> {
    TUI_ONLY_COMMANDS
        .iter()
        .find(|command| command.name == name)
}

/// Names the TUI owns: shared and TUI-only commands plus the retired
/// `review` name. Agent commands with these names are filtered out.
pub fn is_tui_builtin(name: &str) -> bool {
    name == RETIRED_REVIEW_COMMAND
        || shared_command(name).is_some()
        || tui_only_command(name).is_some()
}

/// Names the web viewer owns: shared and web-only commands, the retired
/// `review` name, and `exit`, which the viewer advertises only inside a side
/// conversation. Expects a trimmed, lowercased name.
pub fn is_web_builtin(name: &str) -> bool {
    name == RETIRED_REVIEW_COMMAND
        || name == EXIT_COMMAND
        || shared_command(name).is_some()
        || WEB_ONLY_COMMANDS.iter().any(|command| command.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_are_unique_across_lists() {
        let mut seen = std::collections::HashSet::new();
        for name in SHARED_COMMANDS
            .iter()
            .map(|command| command.name)
            .chain(TUI_ONLY_COMMANDS.iter().map(|command| command.name))
            .chain(WEB_ONLY_COMMANDS.iter().map(|command| command.name))
        {
            assert!(seen.insert(name), "duplicate builtin command name {name}");
            assert_ne!(name, RETIRED_REVIEW_COMMAND);
        }
    }
}
