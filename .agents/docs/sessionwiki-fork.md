# The SessionWiki fork

Mjolnir links [SessionWiki](https://github.com/youdie006/sessionwiki) as a Rust
library so the daemon can index its own sessions into the user's ordinary
SessionWiki index. Upstream has no way for an embedding program to add its own
adapter, so Mjolnir depends on a fork. This note records where the fork is, what
it changes, and what has to happen before a Mjolnir release.

## Where it is

- Fork: `https://github.com/jbellis/sessionwiki`, remote `origin` in the local
  checkout at `../sessionwiki`. Upstream is remote `upstream`.
- Branch `mj-embed`: the library changes on top of upstream `main` at
  version 0.28.0. This is what Mjolnir depends on.
- Tag `v0.28.0-mj.1` (commit `529b8fe`): the first cut, with the three library
  changes.
- Tag `v0.28.0-mj.2` (commit `ef19d4c`): adds quiet progress output when stderr
  is not a terminal, and the tool-name fallback so `brief` on a Mjolnir row
  prints `Tool: mjolnir` instead of `Tool: unknown`. This is the tag the
  workspace `Cargo.toml` names.
- Branch `publish` (commit `33f67f6`), cut from `mj-embed`: renames the package
  to `brokk-sessionwiki` for crates.io. Nothing else differs.

A tag is created once and never moved after Mjolnir's `Cargo.lock` references
it in a commit. A further library change gets a new `-mj.N` tag.

## Session metadata needs no fork change

Mjolnir stores each indexed session's target, profile and harness in the index
as tags (`mj-target:`, `mj-profile:`, `mj-harness:`), written and read by
`mj-controller/src/sessionwiki/tags.rs`. That needs nothing from the fork.

The reason is that `tags` is one of SessionWiki's *durable* tables. `index::open`
drops only the derived cache -- `msgs`, `messages`, `touched`, `files`, `edits`
-- when the file's `user_version` differs from the version the binary was built
with, and never touches `tags`, `notes`, `summaries` or `archive`. So the
metadata survives a schema bump, which a new column on `files` would not: adding
one would need a fork release and a `SCHEMA_VERSION` bump, and a bump re-indexes
every user's whole corpus.

Mjolnir uses its own SQL rather than the crate's `add_tag` and `remove_tag`,
because `norm_tag` lowercases every tag and Mjolnir ids may be mixed case, and
because the crate cannot find a stale `mj-target:` tag without reading every tag
back. `tags.rs` has a round-trip test against a real index opened through
`sessionwiki::index::open`, so an upstream change to the table fails a test
rather than a user. If upstream ever accepts a `Session.metadata` field, that
module is the only place that has to change.

## The library changes

1. `src/index.rs`: `pub fn sync_with(conn, adapters, since)` holds what was the
   body of `sync_bounded`, and `sync_bounded` builds the standard adapter list
   and calls it. This is what lets Mjolnir pass `adapters::all()` plus its own
   `MjolnirAdapter`.
2. `src/adapters/mod.rs`: `fn reconcile_scope(&self) -> Option<String>` on the
   `Adapter` trait. When it returns `Some(prefix)`, deletion reconciliation only
   considers indexed rows whose key starts with that prefix. Without it, two
   Mjolnir instances sharing the tool name `mjolnir` would each archive the
   other's rows on every sync. The filtering is done in Rust, not with SQL
   `LIKE`, because keys are paths and contain `_`.
3. `src/commands.rs`: `pub fn brief_markdown(session, max_chars, include_tools)`
   exposes the existing private `brief_text` renderer so Mjolnir's preview pane
   and `/wiki/sessions/{id}/brief` produce the same briefing as the CLI.
4. `src/adapters/codex.rs` and `src/adapters/claude_code.rs`:
   `Codex::in_home(home)` and `ClaudeCode::in_home(home)` build an adapter for
   one install root instead of the stock `~/.codex` and `~/.claude`. Each keeps
   its tool name and reports a `reconcile_scope` covering only its own root, so
   Mjolnir can index every configured profile home without one install's sync
   archiving another's rows.

Upstream pull request for the embedder hooks:
<https://github.com/youdie006/sessionwiki/pull/26>. If it is merged, the fork
can be retired in favour of an upstream release and the dependency changed back
to the published `sessionwiki` crate.

## Release rule

A git dependency cannot be published to crates.io, and Mjolnir publishes every
workspace crate. So before a Mjolnir release that includes SessionWiki support:

1. In the fork, on branch `publish`, run `cargo publish`. The package is
   `brokk-sessionwiki`; the library target and the installed binary are both
   still called `sessionwiki`.
2. In Mjolnir's root `Cargo.toml`, replace the git dependency with
   `sessionwiki = { package = "brokk-sessionwiki", version = "0.28.0" }`.
3. Run `cargo update -p brokk-sessionwiki` and rebuild.

This needs crates.io credentials, so it is a manual step for the maintainer.

Keep the documented `cargo install` command in
`docs/src/content/docs/sessions.md` pointing at whatever the workspace links.
A `sessionwiki` binary at a different index schema version than the linked
library makes SessionWiki drop and rebuild the whole index every time the two
alternate. Once the crate is published, `cargo install brokk-sessionwiki` gives
a binary that matches the library by construction, and the documented install
command should change to it.

## Published

`brokk-sessionwiki` 0.28.0 was published to crates.io on 2026-09-17 from the fork's `publish` branch, commit 33f67f6, tagged `brokk-v0.28.0`. Mjolnir depends on it as `sessionwiki = { package = "brokk-sessionwiki", version = "0.28.0" }`. To ship a fork change: commit on `mj-embed`, merge into `publish`, bump the version there, `cargo publish`, then bump the version in Mjolnir's root `Cargo.toml` and the install command in `docs/src/content/docs/sessions.md` in the same commit.

`brokk-sessionwiki` 0.29.0 adds the `Codex::in_home` and `ClaudeCode::in_home`
constructors described above. Mjolnir uses them to index every enabled Codex and
Claude profile home instead of only the stock ones, so sessions started under a
profile home such as `~/.codex3` are searchable.

`brokk-sessionwiki` 0.30.0 adds the `grep` library module and the matching
`grep` command, which find the passages inside one session the way `search`
finds sessions, plus `show --jsonl` (one JSON object per message) and the
matched message index `i` on `search --json` hits. It was published from the
`publish` branch, tagged `brokk-v0.30.0`. Mjolnir's Resume-preview hit search
(`transcript_hits` in `mj-controller/src/sessionwiki.rs`) now calls
`grep_session` instead of scanning for itself, so the CLI and the TUI report
the same hits.

`brokk-sessionwiki` 0.30.1 (tag `brokk-v0.30.1`) restores the default SIGPIPE
disposition at startup so `sessionwiki grep -l ... | head` exits quietly
instead of panicking on the closed pipe. Mjolnir links 0.30.1.

The fork's `publish` branch now has commit `effa77d`, which prepares 0.30.2.
It scopes the stock Codex and Claude adapters to their actual roots, so a
standalone sync cannot mark other profile homes deleted. An unchanged source
file that was previously marked archived is made live again without reparsing
its transcript; a changed source is reparsed. Tests, clippy, and
`cargo publish --dry-run` pass. The crate has not been published yet, so
Mjolnir still links 0.30.1. After publication, update the root `Cargo.toml`,
`Cargo.lock`, and the install command in `docs/src/content/docs/sessions.md`
in one commit, then install the matching standalone binary. Run a Mjolnir full
sync to repair rows in additional profile homes; a standalone sync only scans
the stock homes.

The fork fix was pushed to `origin/publish` as `effa77d`. The same source fix
was ported to the open upstream [PR #29](https://github.com/youdie006/sessionwiki/pull/29)
as `6f84b8b`, with its description updated. That PR branch passed tests,
Clippy, and formatting. The first `cargo publish --locked` of 0.30.2 packaged
and verified successfully but crates.io rejected the upload with 403
authentication failed. Retry publication after restoring registry credentials
or package ownership; no 0.30.2 release tag has been created yet.
