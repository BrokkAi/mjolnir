# The SessionWiki fork

Mjolnir links [SessionWiki](https://github.com/youdie006/sessionwiki) as a Rust
library so the daemon can index its own sessions into the user's ordinary
SessionWiki index. Upstream has no way for an embedding program to add its own
adapter, so Mjolnir depends on a fork. This note records where the fork is, what
it changes, and what has to happen before a Mjolnir release.

## Where it is

- Fork: `https://github.com/jbellis/sessionwiki`, remote `origin` in the local
  checkout at `../sessionwiki`. Upstream is remote `upstream`.
- Branch `mj-embed`: the three library changes on top of upstream `main` at
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

## The three library changes

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
