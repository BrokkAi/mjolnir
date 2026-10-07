# SessionWiki: from fork to upstream

Mjolnir links [SessionWiki](https://github.com/youdie006/sessionwiki) as a Rust
library so the daemon can index its own sessions into the user's ordinary
SessionWiki index.

## Current state

Since 139726d84 (2026-10-07), Mjolnir depends on the upstream crate:
`sessionwiki = "0.33.1"` in the root `Cargo.toml`. It no longer depends on the
fork's `brokk-sessionwiki` package, which stopped at 0.30.2.

The user contributes to SessionWiki through the fork
`https://github.com/jbellis/sessionwiki`, which exists only to send pull
requests upstream. Do not publish Mjolnir-specific changes from the fork. A
library change Mjolnir needs goes upstream as a pull request, and Mjolnir
bumps the dependency once a release includes it. For example, role-filtered
search (`index::search_with_roles`) came from youdie006/sessionwiki#40 and was
released in 0.32.0.

What the fork used to add, and where it is now:

- The embedder hooks (`index::sync_with`, `Adapter::reconcile_scope`,
  `commands::brief_markdown`, `Codex::in_home`, `ClaudeCode::in_home`) are all
  in upstream.
- The `grep` passage matcher existed only in the fork, never upstream. It now
  lives in Mjolnir as `mj-controller/src/sessionwiki/transcript_grep.rs` and
  serves the Resume preview and the history tools.

## Rules that still apply

- **Schema version.** SessionWiki drops and rebuilds its derived cache when the
  index file's `user_version` differs from the version it was built with.
  `index_version_mismatch` in `mj-controller/src/sessionwiki.rs` makes Mjolnir
  stand aside rather than rebuild an index written at another version. Check
  `sessionwiki::index::SCHEMA_VERSION` on every bump. 0.30.1 to 0.33.1 kept
  version 8.
- **Documented install command.** Keep the install command in
  `docs/src/content/docs/sessions.md` pointing at the version Mjolnir links,
  and change it in the same commit as the dependency.
- **Metadata.** Mjolnir stores each session's target, profile and harness as
  tags (`mj-target:`, `mj-profile:`, `mj-harness:`) through its own SQL in
  `mj-controller/src/sessionwiki/tags.rs`. `tags` is a durable table that a
  schema rebuild keeps. A round-trip test against a real index catches an
  upstream change to it.
- **Behaviour changes on bump.** Read the upstream CHANGELOG for every version
  crossed. 0.31.0 made multi-word queries match only when every word is in the
  same message.
