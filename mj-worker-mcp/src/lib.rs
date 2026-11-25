//! Stdio MCP servers exposed by the Mjolnir worker.

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

pub mod claude_mcp;
mod mcp_stdio;
pub mod memory_mcp;
pub mod subagent_mcp;
