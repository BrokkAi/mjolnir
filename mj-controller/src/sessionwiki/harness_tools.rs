//! Map native SessionWiki tool names to Mjolnir harnesses.

use mj_core::config::HarnessKind;

/// The SessionWiki tool name a harness's sessions are indexed under.
///
/// This is the only table of tool names; [`harness_for_tool`] reads it
/// backwards.
pub(super) const fn tool_name(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Claude => "claude-code",
        HarnessKind::Codex => "codex",
        HarnessKind::Kimi => "kimi-code",
        HarnessKind::Grok => "grok-build",
        HarnessKind::Muse => "muse",
        HarnessKind::OpenCode => "opencode",
        HarnessKind::Dsh => "dsh",
    }
}

/// The harness a SessionWiki tool name names, or `None` for an unsupported
/// harness. A known harness need not have a native import reader yet.
pub(super) fn harness_for_tool(name: &str) -> Option<HarnessKind> {
    HarnessKind::ALL
        .into_iter()
        .find(|kind| tool_name(*kind) == name)
}
