# Carry Mjolnir target and profile through the SessionWiki search

This ExecPlan is a living document. The sections `Progress`,
`Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must
be kept up to date as work proceeds. It is maintained in accordance with
`.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir's Resume dialog (opened with `prefix+g` in the terminal interface) has
three tabs. The third, **Archived**, lists sessions whose live Mjolnir record is
gone but whose conversation the SessionWiki index still holds. SessionWiki is a
separate tool that keeps one searchable index of AI coding sessions across every
harness a person runs; Mjolnir links it as a Rust library and writes its own
sessions into that index.

Before this change an Archived row could not say anything true about where the
session ran. Its PROFILE column showed the literal word `mjolnir` (the
SessionWiki tool name, not a Mjolnir profile) and its TARGET column showed
`local/<project folder>` whether or not the session ever ran locally. Pressing
Enter on the row opened the restore wizard on the first profile and the first
target in the configuration, ignoring where the session actually ran.

After this change, a Mjolnir session that the index holds carries its own
target template id, profile id, and harness kind. The Archived row shows the
real profile id and the real target, and the restore wizard opens already
pointed at them. The same three values appear in the web viewer's archived row
and restore card, and the restore form's Profile and Target selects default to
them. A person can see it working by closing a session started on a named
target and profile, letting the index sync, deleting its checkpoint so the row
becomes archived, and searching for a word from the conversation in the Resume
dialog: the row now reads the real ids.

## Progress

- [x] (2026-09-18) Read the approved plan, the SessionWiki 0.29.0 crate source,
      and every file named below.
- [x] (2026-09-18) Wrote this ExecPlan copy.
- [x] (2026-09-18) `mj-core`: extracted `target_label` and pointed
      `SessionRecord::project_target` at it, with a unit test.
- [x] (2026-09-18) `mj-client`: added `target`, `profile`, `harness` to
      `WikiRow`.
- [x] (2026-09-18) `mj-controller`: added `sessionwiki/tags.rs`, wrote tags in
      `sync_blocking`, read them in `query_rows`, with tests.
- [x] (2026-09-18) `mj-tui`: archived rows show the indexed profile and target;
      the restore wizard opens on them; tests added.
- [x] (2026-09-18) `mj-controller/src/web/viewer.js`: row meta line and restore
      card show the values, and the selects default to them.
- [x] (2026-09-18) Docs paragraphs in `docs/src/content/docs/sessions.md` and
      `.agents/docs/sessionwiki-fork.md`.
- [x] (2026-09-18) `cargo test` and `cargo clippy --all-targets -- -D warnings`
      pass on the dev profile outside the sandbox. The first `cargo test` run
      failed on one doc test, described under `Surprises & Discoveries`, and
      passed after the module comment was rewritten.
- [ ] Live manual check (start a session on a named target and profile, archive
      it, confirm the row and wizard) — not performed; it needs a running
      daemon and a real index, which this change did not have available.

## Surprises & Discoveries

- Observation: the approved plan referred to "the existing isolated-index
  pattern in the `sessionwiki.rs` tests module at line 1447". There is no such
  pattern: no test in that module opens a SessionWiki index at all, because
  `index_is_writable` refuses any process that has not resolved an index
  location.
  Evidence: `grep -rn "SESSIONWIKI_DATA" --include=*.rs` matches only
  `mj-core/src/config/loading.rs` and `mj-controller/src/sessionwiki.rs`
  (no test).
  Consequence: the new `tags.rs` tests open a `rusqlite::Connection` on a
  temporary file through `sessionwiki::index::open()` with `SESSIONWIKI_DATA`
  pointed at a `tempfile::tempdir()`, which is what proves the raw SQL matches
  the crate's real schema. Because that is a process-wide environment variable,
  those tests are serialized behind one mutex inside the module.

- Observation: an indented block inside a `//!` module comment is a Rust doc
  test, and `cargo test` tried to compile the three tag shapes as code.
  Evidence: `test mj-controller/src/sessionwiki/tags.rs - sessionwiki::tags
  (line 12) ... FAILED`, `error: expected one of `!` or `::`, found `-``.
  The module comment now names the three shapes inline in backticks instead.

- Observation: the approved plan wrote the harness tag as
  `mj-harness:<harness_kind kind_name>`. `HarnessKind` has no `kind_name`; the
  stable string is `HarnessKind::id()` (`mj-core/src/config/harness.rs:316`),
  which yields `codex`, `claude`, `kimi`, `grok`, `muse`. `kind_name` exists on
  `TargetTemplate` and on `MachineTemplate`, which are different types.
  The implementation uses `id()`.

## Decision Log

- Decision: store the metadata as rows in SessionWiki's durable
  `tags(session_id, tag)` table rather than as new columns on the index's
  `files` table.
  Rationale: new columns would need a fork release and a `SCHEMA_VERSION` bump,
  and a bump re-indexes the user's entire corpus (tens of minutes). The `tags`
  table survives a schema bump untouched: `index::open` drops only `msgs`,
  `messages`, `touched`, `files` and `edits`
  (`brokk-sessionwiki-0.29.0/src/index.rs:375-395`).
  Date/Author: 2026-09-18, implementation of the approved plan.

- Decision: write and read the `tags` table with Mjolnir's own SQL in one
  module, `mj-controller/src/sessionwiki/tags.rs`, instead of the crate's
  `add_tag` / `remove_tag`.
  Rationale: the crate's `norm_tag` lowercases every tag, and Mjolnir ids may
  be mixed case (`validate_id`, `mj-core/src/config/loading.rs:236`), so a
  round trip through `add_tag` would corrupt an id. The crate also offers no
  way to find a stale `mj-target:` tag without reading tags back. Keeping the
  SQL in one module with a round-trip test against the real crate schema means
  an upstream table change fails a test instead of failing a user.
  Date/Author: 2026-09-18.

- Decision: take the records to tag from the adapter's own final `Sessions`
  snapshot rather than from the `Controller` loaded before the sync walk.
  Rationale: `MjolnirAdapter::reloading` re-reads controller state when the
  indexer reaches it, which can be minutes after `sync_blocking` loaded the
  controller. A session closed during that walk is indexed from the reloaded
  state, so its tags must come from the same state that produced its row.
  Date/Author: 2026-09-18.

- Decision: write the tags for every session the adapter knows about, on every
  sync, inside a single transaction, as a delete-then-insert per session.
  Rationale: a Move or a profile switch changes the record, and the stale tag
  must not survive. The cost is one delete and three inserts per session per
  sync, which is negligible next to the transcript indexing in the same pass.
  A single transaction keeps the write off the disk sync path per row.
  Date/Author: 2026-09-18.

- Decision: `HarnessKind::id()` supplies the `mj-harness:` value.
  Rationale: see `Surprises & Discoveries`; `kind_name` does not exist on that
  type.
  Date/Author: 2026-09-18.

- Decision: carry the ids to the restore wizard as two new `Option<String>`
  fields on `ResumeRow` (`wiki_profile`, `wiki_target`) rather than widening
  `ResumeRowKey::Archive`.
  Rationale: `ResumeRowKey` is compared and sorted to identify a row across
  incremental scan updates; putting mutable metadata in it would change row
  identity whenever the index answer changed.
  Date/Author: 2026-09-18.

## Outcomes & Retrospective

The change is complete in code and covered by tests. A Mjolnir session indexed
into SessionWiki now carries `mj-target:`, `mj-profile:` and `mj-harness:` tags;
the daemon returns them on `WikiRow`; the terminal Archived tab shows the real
profile and target and pre-seeds the restore wizard with them; the web viewer
shows them and defaults its restore selects to them.

What remains is the live manual check described under `Validation and
Acceptance`. It needs a running daemon, a real index and a real session, none
of which a test run provides. Existing indexed sessions gain their tags on the
next sync, with no migration and no re-index, because the sync writes the tags
for every session the adapter knows about rather than only for changed ones.

A lesson worth recording: the useful property of the `tags` table is not that
it is a tag table but that it is *durable* in the crate's own sense — the crate
promises never to drop it on a schema bump. That promise is what lets Mjolnir
store structured metadata in a shared index it does not own the schema of.

## Context and Orientation

Read this section as if you have never seen the repository.

Mjolnir is a Rust workspace. The crates that matter here are:

- `mj-core` — shared types and configuration. `mj-core/src/state.rs` holds
  `SessionRecord`, the daemon's record of one session, with the fields
  `target_template_id` (which target template the session ran on),
  `last_profile` (which harness profile id it last used) and `harness_kind`
  (which harness product: Codex, Claude, Kimi, Grok or Muse).
  `mj-core/src/config/targets.rs` holds `TargetTemplate`, the enum of target
  kinds (`LocalBare`, `SshBare`, container kinds, `AwsEc2`).
- `mj-client` — the wire types the daemon and its control surfaces share.
  `mj-client/src/daemon.rs` holds `WikiRow`, one indexed session as a control
  surface sees it. `WikiRow` is `#[serde(deny_unknown_fields)]`, so any new
  field must also be `#[serde(default)]` for an older peer's payload to
  deserialize.
- `mj-controller` — the daemon. `mj-controller/src/sessionwiki.rs` is the whole
  of Mjolnir's SessionWiki integration: the adapter that publishes Mjolnir's
  sessions into the index, the background sync job, and the queries that answer
  a search. `mj-controller/src/web/viewer.js` is the browser control surface,
  compiled into the daemon binary with `include_str!`.
- `mj-tui` — the terminal control surface. `mj-tui/src/resume.rs` builds the
  Resume dialog's rows; `mj-tui/src/wizards/dashboard/begin.rs` opens the
  wizards.

"SessionWiki" is a separate program, published on crates.io as
`brokk-sessionwiki` (this build pins 0.29.0). Its source, once `cargo fetch`
has run, is under `~/.cargo/registry/src/*/brokk-sessionwiki-0.29.0/`. It keeps
one SQLite file, by default under the user's data directory, or wherever the
`SESSIONWIKI_DATA` environment variable points. Inside that file:

- A *cache* of tables — `files`, `msgs`, `messages`, `touched`, `edits` — built
  by walking each tool's session store. These are dropped and rebuilt whenever
  the crate's `SCHEMA_VERSION` changes (`src/index.rs:375-395`).
- *Durable* tables — `tags`, `notes`, `summaries`, `archive` — which the crate
  promises never to drop on a schema bump, because they hold things that cannot
  be rebuilt.

The `tags` table is `tags(session_id TEXT NOT NULL, tag TEXT NOT NULL, PRIMARY
KEY (session_id, tag))` with an index on `tag` (`src/index.rs:244-249`). Row
queries expose it as `TAGS_SQL`, which is
`(SELECT group_concat(t.tag, ',') FROM tags t WHERE t.session_id = f.session_id)`
(`src/index.rs:1121`) — that is, the tags of a row arrive comma-joined in
`SessionRow::tags`. Mjolnir's ids never contain a comma (`validate_id` in
`mj-core/src/config/loading.rs:236` allows only `[A-Za-z0-9._-]`), so that
contract holds.

Mjolnir's adapter, `MjolnirAdapter` in `mj-controller/src/sessionwiki.rs`, sets
each indexed `Session.id` to the Mjolnir session id verbatim
(`parse_key`, around line 390). So a tag keyed by the Mjolnir session id is
keyed by exactly the `session_id` the `tags` table wants.

Why the metadata cannot simply be looked up when a row is shown: the daemon's
archive job destroys the Mjolnir record
(`archive_stopped_session`, `mj-controller/src/daemon/resume.rs:239-250`). The
rows that need the metadata are precisely the ones with no record left to join
against. The index must carry the data itself.

## Plan of Work

In this order, because each step compiles on its own:

1. `mj-core/src/state.rs`: add a free function

       pub fn target_label(config: &Config, target_id: &str, project: Option<&Path>) -> String

   holding the rule `SessionRecord::project_target` (around line 1155) already
   implements: when the configuration has no template under `target_id`, or the
   template is not `LocalBare` or `SshBare`, return `target_id` verbatim;
   otherwise return `"<target_id>/<final component of project>"`, falling back
   to `target_id` alone when there is no project path. Rewrite
   `project_target` to resolve its own project path (the managed worktree's
   source project directory, else the record's project directory) and call the
   new function, so the two can never drift.

2. `mj-client/src/daemon.rs`: add three fields to `WikiRow`, each
   `#[serde(default)] pub …: Option<String>`: `target`, `profile`, `harness`.
   Document that only Mjolnir's own rows ever carry them.

3. `mj-controller/src/sessionwiki/tags.rs` (new module, declared beside the
   existing `mod harness_adapters;` at the top of
   `mj-controller/src/sessionwiki.rs`):

       pub struct MjTags { pub target: Option<String>, pub profile: Option<String>, pub harness: Option<String> }
       pub fn write(connection: &rusqlite::Connection, session_id: &str, tags: &MjTags) -> anyhow::Result<()>
       pub fn read(connection: &rusqlite::Connection, session_ids: &[&str]) -> anyhow::Result<BTreeMap<String, MjTags>>

   `write` deletes this session's `mj-target:`, `mj-profile:` and `mj-harness:`
   rows, then `INSERT OR IGNORE`s the current three. It runs inside the
   caller's transaction. `read` runs one `SELECT session_id, tag FROM tags
   WHERE tag LIKE 'mj-%' AND session_id IN (…)` per batch and parses each tag
   back into the struct.

4. `mj-controller/src/sessionwiki.rs`:
   - `MjolnirAdapter` gains `pub fn indexed_tags(&self) -> BTreeMap<String, tags::MjTags>`,
     reading its own final `Sessions` snapshot under the mutex.
   - `sync_blocking` (around line 574), after `sync_with` returns, opens one
     transaction on the same connection and calls `tags::write` for every entry
     of that map. The adapter must therefore be kept reachable after being put
     in the boxed adapter list; keep an `Arc<MjolnirAdapter>` and give the list
     a `Box` that delegates to it, or build the list from a clone of the `Arc`.
   - `query_rows` (around line 799): after the rows are built, collect the ids
     of rows whose `tool` is `mjolnir`, call `tags::read` once, and fill
     `target`, `profile` and `harness` on those rows.
   - `wiki_row` fills the three fields with `None`; the caller overwrites them.

5. `mj-tui/src/resume.rs`:
   - `ResumeRow` gains `wiki_profile: Option<String>` and
     `wiki_target: Option<String>`.
   - In `merged_resume_rows`, the archived row's `profile_id` becomes
     `hit.profile` when present and `hit.tool` otherwise; its `origin` becomes
     `mj_core::state::target_label(config, target, project)` when `hit.target`
     is present, and `archive_origin(&hit.project)` otherwise.
   - `harness_of_tool` also accepts `hit.harness` by parsing it as a
     `HarnessKind`.
   - `activate_selected_resume_row` passes the two ids into
     `begin_archive_restore`.

6. `mj-tui/src/wizards/dashboard/begin.rs`: `begin_archive_restore` takes the
   two ids and resolves each to an index: the profile against the enabled
   profiles in `config.profiles` order (the same list
   `resume_wizard_profiles` builds for `ResumeSource::Archive`, in
   `mj-tui/src/wizards/dashboard/targets.rs:44`), the target against
   `config.targets` keys (the list `nth_key` indexes, `mj-tui/src/lib.rs:953`).
   An id the configuration no longer has leaves the index at 0.

7. `mj-controller/src/web/viewer.js`: `wikiRowNode` puts `row.profile` and
   `row.target` in the meta line before `row.tool`, when present.
   `renderWikiDetail` does the same in the card's dim line, and `wikiDraft`
   seeds `profileId` and `targetId` from the row's values when the page's
   configuration snapshot still has them.

8. Docs: a paragraph in `docs/src/content/docs/sessions.md` naming the three
   tags and what reads them, and a note in `.agents/docs/sessionwiki-fork.md`
   saying this needed no fork change and why.

## Concrete Steps

From the repository root, `/home/jonathan/Projects/hel`:

    cargo test
    cargo clippy --all-targets -- -D warnings

Both must be run outside the restricted sandbox with elevated permissions: the
suite exercises loopback TCP and Unix sockets, and a sandboxed run fails with
`EPERM` or hangs. Both must be run on the dev profile, which is where
`debug_assert!` and integer-overflow checks are live.

The new tests to expect passing, by name:

- `mj_core::state::tests::target_label_names_the_project_for_bare_targets`
- `mj_controller::sessionwiki::tags::tests::tags_round_trip_through_the_index`
- `mj_controller::sessionwiki::tags::tests::a_changed_target_replaces_the_stale_tag`
- `mj_controller::sessionwiki::tags::tests::the_crate_reports_the_same_tags_on_its_own_row`
- `mj_controller::sessionwiki::tests::query_rows_returns_the_indexed_target_profile_and_harness`
- `mj_tui::resume::tests::archived_row_shows_indexed_target_and_profile`
- `mj_tui::resume::tests::enter_on_archived_row_preseeds_wizard_profile_and_target`
- `mj_tui::resume::tests::an_unknown_indexed_profile_leaves_the_restore_wizard_on_its_first_choice`

## Validation and Acceptance

Automated: `cargo test` passes with the six tests above included, and
`cargo clippy --all-targets -- -D warnings` is clean.

Manual, with an isolated data directory so the person's real index is not
touched — set `MJ_CONFIG_DIR`, `MJ_DATA_DIR` and `SESSIONWIKI_DATA` to fresh
directories before starting the daemon:

1. Start a session on a named target and a named profile, say something in it,
   and close it. Wait for the sync (it runs at most a minute after a change).
2. Run `sessionwiki tags <session id>` and expect three lines,
   `mj-target:<target id>`, `mj-profile:<profile id>` and
   `mj-harness:<codex|claude|kimi|grok|muse>`.
3. Delete that session's checkpoint file under the sessions directory so the
   row becomes archived, and trigger another sync.
4. Open Resume with `prefix+g`, search for a word from the conversation, and go
   to the Archived tab. The row's PROFILE column reads the real profile id, not
   `mjolnir`. Its TARGET column reads the real target id, followed by `/` and
   the project folder when the target is a bare (non-container) one.
5. Press Enter. The restore wizard opens with that profile selected on its
   first step and that target selected on its second.
6. Rename the target in `config.toml`, sync again, and repeat: the row now
   shows the old target id verbatim, with no project suffix, because the
   configuration no longer says what kind of target it was, and the wizard
   falls back to the first target.
7. For the browser: run `cargo build`, restart the daemon (the JavaScript is
   compiled in with `include_str!`, so an old daemon serves the old page), open
   the viewer's Resume page, and confirm the archived row's meta line and the
   restore card show the same ids and that the Profile and Target selects start
   on them.

## Idempotence and Recovery

Every step is repeatable. The tag write is a delete followed by an insert, so
running a sync twice leaves the same three rows. `INSERT OR IGNORE` makes a
concurrent duplicate harmless. Nothing is migrated and nothing is re-indexed:
sessions already in the index gain their tags on the next ordinary sync.

To undo the data side by hand, with the daemon stopped:

    sqlite3 "$SESSIONWIKI_DATA/sessionwiki.db" \
      "DELETE FROM tags WHERE tag LIKE 'mj-target:%' OR tag LIKE 'mj-profile:%' OR tag LIKE 'mj-harness:%';"

The next sync writes them again.

## Interfaces and Dependencies

In `mj-core/src/state.rs`:

    pub fn target_label(config: &Config, target_id: &str, project: Option<&std::path::Path>) -> String;

In `mj-client/src/daemon.rs`, on `WikiRow`:

    #[serde(default)] pub target: Option<String>,
    #[serde(default)] pub profile: Option<String>,
    #[serde(default)] pub harness: Option<String>,

In `mj-controller/src/sessionwiki/tags.rs`:

    pub struct MjTags {
        pub target: Option<String>,
        pub profile: Option<String>,
        pub harness: Option<String>,
    }

    pub fn write(connection: &rusqlite::Connection, session_id: &str, tags: &MjTags) -> anyhow::Result<()>;
    pub fn read(connection: &rusqlite::Connection, session_ids: &[&str]) -> anyhow::Result<std::collections::BTreeMap<String, MjTags>>;

In `mj-controller/src/sessionwiki.rs`, on `MjolnirAdapter`:

    pub fn indexed_tags(&self) -> std::collections::BTreeMap<String, tags::MjTags>;

In `mj-tui/src/wizards/dashboard/begin.rs`:

    pub(crate) fn begin_archive_restore(
        &mut self,
        wiki_id: String,
        title: String,
        profile_id: Option<String>,
        target_id: Option<String>,
    ) -> DashboardAction;

No new dependency is added. `rusqlite` is already a direct dependency of
`mj-controller`, and `tempfile` is already a dev-dependency.

## Revision note

2026-09-18: first version, written from the approved plan before implementation
and updated during it. Two deviations from the approved plan are recorded in
`Surprises & Discoveries` and `Decision Log`: the harness tag uses
`HarnessKind::id()` because `kind_name` does not exist on that type, and the
`tags.rs` tests create their own isolated index because the pattern the
approved plan pointed at does not exist in the repository.
