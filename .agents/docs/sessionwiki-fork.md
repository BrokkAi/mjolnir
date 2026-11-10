# SessionWiki library integration

Mjolnir links the `brokk-sessionwiki` fork of
[SessionWiki](https://github.com/youdie006/sessionwiki) as a Rust library so
the daemon can index its own sessions into the user's ordinary SessionWiki
index. The root `Cargo.toml` and `Cargo.lock` name the linked version.

## Current state

The dependency is published as `brokk-sessionwiki` and imported as
`sessionwiki` in Rust. The matching standalone installation command is in
`docs/src/content/docs/sessions.md`; keep it aligned with the lockfile.

The user contributes library changes upstream through
`https://github.com/jbellis/sessionwiki`. Publish generally useful library
changes in the fork for coordinated validation, submit upstream, and switch
Mjolnir to the published version after joint tests. Keep adapter behavior
generally useful rather than Mjolnir-specific.

`sessionwiki::adapters::{KimiCode, GrokBuild, Muse, Dsh}` own native history
indexing, with one `in_home` adapter per enabled Mjolnir profile home. Kimi
and Grok keep directory keys; Muse keeps its `session.jsonl` key. DSH searches
both bridge JSON and native histories under the supplied home. The tool
names remain `kimi-code`, `grok-build`, `muse` and `dsh`. Mjolnir retains its
native import readers for resume fidelity; DSH search does not imply DSH
import support. `MjolnirAdapter` still indexes Mjolnir's stored transcripts.

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
  `IndexVersion` in `mj-controller/src/sessionwiki.rs` classifies the file:
  Mjolnir leaves a newer index alone and rebuilds an older one on its next
  write, with a full pass. Standing aside from an older index too stranded it
  for good, since nothing else would ever bring it forward. Check
  `sessionwiki::index::SCHEMA_VERSION` on every bump. 0.30.1 to 0.33.1 kept
  version 8; 0.34.0 to 0.34.24 keep version 9. SessionWiki itself refuses to
  rebuild a newer index from 0.34.1 on, so only a `sessionwiki` command older
  than that rebuilds Mjolnir's index back down.
- **Full-pass marker.** `<data_dir>/sessionwiki-built` records the cache
  generation (`user_version` and SQLite's `schema_version` cookie) the last
  complete full pass filled. An incremental pass is only sound on that
  generation; any rebuild moves the cookie, so the next pass is full and the
  index reads as indexing until it finishes.
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
