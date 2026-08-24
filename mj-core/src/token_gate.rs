//! Vendor dispatch for the pre-spawn OAuth freshness gates.
//!
//! Every path that spawns an agent process calls [`ensure_fresh_before_spawn`]
//! first; it routes Claude Code invocations to [`crate::claude_token`] and
//! codex invocations to [`crate::codex_token`], and is a no-op for every
//! other adapter. Keeping the dispatch here means a new spawn path cannot
//! protect one vendor and forget the other.

use std::collections::HashMap;
use std::path::PathBuf;

/// Rotate a near-expiry OAuth token for whichever vendor `args` (or the
/// roster adapter id, when known) identifies, before the spawn.
pub async fn ensure_fresh_before_spawn(
    adapter_source_id: Option<&str>,
    args: &[String],
    cwd: PathBuf,
    env: &HashMap<String, String>,
) {
    if crate::claude_token::is_claude_invocation(adapter_source_id, args) {
        crate::claude_token::ensure_fresh_before_spawn(cwd, env).await;
    } else if crate::codex_token::is_codex_invocation(adapter_source_id, args) {
        crate::codex_token::ensure_fresh_before_spawn(cwd, env).await;
    }
}
