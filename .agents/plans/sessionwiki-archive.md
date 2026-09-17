# Integrate SessionWiki: search with preview, Archived tab, archive-after-N-days

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It must be maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir (the `mj` binary in this repository) keeps a copy of every closed coding session as a checkpoint archive on disk. Those copies grow forever and there is no way to search them. SessionWiki is a separate open-source tool that indexes AI coding sessions from many tools into one SQLite full-text index per user. After this change:

- Every Mjolnir session that has been checkpointed appears in the user's SessionWiki index under the tool name `mjolnir`, so `sessionwiki search "some phrase"` and the SessionWiki MCP server find Mjolnir sessions next to Claude Code, Codex, and other tools' sessions.
- The Resume dialog in the terminal UI and in the web viewer searches that index as you type, shows a text preview of the selected session, and has a third tab, "Archived", listing sessions whose live Mjolnir copy is gone. Pressing Enter on an archived row starts a new session whose first prompt carries a compacted summary of the old transcript.
- An optional setting, `archive_after_days`, makes the daemon delete Mjolnir's own copy of stopped sessions older than that many days once SessionWiki has indexed them. The session's git branch is kept.

Success is visible from a terminal: close a session, run `sessionwiki list --tool mjolnir` and see it; open Resume, type a word from that session, see the hit and its preview; delete the checkpoint and see the row move to the Archived tab; press Enter and get a new session whose first agent turn shows it read the summary.

## Progress

- [x] (2026-09-16 18:00Z) Plan written and committed (milestone 0).
- [x] (2026-09-16 20:05Z) Milestone 1: rusqlite 0.40 bump in mj-worker, mj-cli, mj-controller; full `cargo test`; commit `4994dc15`.
- [x] (2026-09-16 20:40Z) Milestone 2: fork branch `mj-embed` commit 529b8fe, tag `v0.28.0-mj.1` pushed to `jbellis/sessionwiki`, upstream PR https://github.com/youdie006/sessionwiki/pull/26; Mjolnir git dependency added, `cargo build` and `cargo about generate` pass.
- [x] (2026-09-17 00:50Z) Milestone 3: `[sessionwiki]` config section (CONFIG_VERSION 10), `MjolnirAdapter`, `WikiIndexer` with close and hourly triggers; commit `60ed7bed`. Live check: two closed sessions listed by `sessionwiki list --tool mjolnir` and shown by `sessionwiki brief <id> --tools`.
- [ ] Milestone 4: HTTP API and client wrappers; terminal Resume dialog search, preview, Archived tab, restore.
- [ ] Milestone 5: archive job driven by `archive_after_days`.
- [ ] Milestone 6: web viewer parity.
- [ ] Milestone 7: documentation; follow-up ticket for the in-session SessionWiki skill.
- [ ] Milestone 8 (release gate, needs crates.io credentials): publish the fork as `brokk-sessionwiki` and switch the dependency from git to the registry.

## Surprises & Discoveries

- Observation: SessionWiki reconciles deletions per tool name, not per store root. `archive_or_prune` in `src/index.rs` archives every live row whose `tool` matches and whose key was not listed in the current sync.
  Evidence: `src/index.rs` lines 878-919 in the fork's `main` at commit f3e1d8c. With two Mjolnir instances each listing only its own sessions, each sync would archive the other's rows and the next sync would un-archive them. This drove the `reconcile_scope` fork change below.
- Observation: Mjolnir publishes every crate to crates.io, and users install with `cargo install --locked brokk-mjolnir`.
  Evidence: `.github/workflows/publish.yml` line 143 and `docs/src/content/docs/install.md` line 73. A git dependency cannot be published, so milestone 8 exists.
- Observation: rusqlite 0.40 moved the `u64` and `usize` `ToSql`/`FromSql` implementations behind a new `fallible_uint` feature, because those conversions can fail.
  Evidence: `rusqlite-0.40.2/src/types/from_sql.rs` lines 141-145 and `src/types/to_sql.rs` lines 272-280. Mjolnir reads and writes many `u64` columns, so the bump produced 57 trait-bound errors in `mj-controller/src/database.rs` and its submodules. Enabling `fallible_uint` in the three manifests restores the 0.37 behaviour exactly; no call site changed.
- Observation: `mj-controller/src/controller/recovery_scan.rs` did not compile under `--all-targets` before this milestone: its `candidate` test helper was missing the `borrowed_from` field that an earlier merge added to `TargetLocator::LocalDocker`.
  Evidence: `mj-core/src/state.rs` line 649 against `recovery_scan.rs` line 1306. Fixed in the same commit because it blocked `cargo test`.
- Observation: `mj-worker`'s `acp::tests::ten_large_photos_reach_acp_for_both_prompt_and_steering` failed once under whole-workspace load and passed alone and in its own crate suite. It is timing-sensitive, not a rusqlite regression.
  Evidence: the workspace run took 207s for that suite against 86s for the crate alone.
- Observation: The fork's `main` already carried upstream through 0.28.0 when milestone 2 started, so the tag was cut as `v0.28.0-mj.1` rather than the 0.26.0 name in the first draft. The tag was created once and never moved after Mjolnir referenced it.
  Evidence: `git log --oneline mj-embed` shows the 0.27.0 and 0.28.0 release merges below commit 529b8fe.
- Observation: The adapter's view of controller state goes stale during a long sync, and nothing later corrects it. A sync pass walks every other tool's store before it reaches the Mjolnir adapter; on this machine the first pass took about twenty minutes (746 Claude Code and 1639 Codex sessions). Sessions closed during that walk were indexed with no record at all: empty project, no start or end time, and the title guessed from the first prompt. Because the checkpoint's modification time is the change token and the checkpoint never changes again, no later sync re-parses them.
  Evidence: the first live check produced rows with `project = ''` and `started = NULL` while `mj.sqlite3` held the correct `project_directory` and `updated_at`. Resolved by `MjolnirAdapter::reloading`, which re-reads controller state in `store()`, the moment the indexer reaches this adapter. `from_state` is kept as the fixed-snapshot form the unit test uses. Re-verified after touching the archives to change their tokens: project, title, start, and end are all populated.
- Observation: `sessionwiki brief` prints `Tool: unknown` for a Mjolnir session. `index::session_from_index` resolves the display tool through `adapters::by_name`, and the Mjolnir adapter is not in the standalone binary's registry.
  Evidence: `src/index.rs` lines 1957-1959 in the fork. Harmless for `list --tool mjolnir` and for search, but milestone 4's preview would show it too. If that matters, a fourth fork change should pass the row's own tool string through.
- Observation: SessionWiki's per-session progress output is very loud in the daemon log: one `[tool] indexing n/total` line per session, with no newline, for every tool on every full sync. The plan accepted stderr progress; in practice it fills `daemon.log` with a single multi-megabyte line.
  Evidence: `$MJ_DATA_DIR/daemon.log` after one full sync of this machine's corpus. Worth a fork change that silences progress when the caller is not a terminal.
- Observation: The `zcode` profile named in the acceptance steps does not exist in the current `config.toml`; it survives only in a backup written by an older build, whose `kind = "zcode"` is not a `HarnessKind` this build accepts.
  Evidence: `~/.config/mjolnir/config.toml.bak-20260915T121000` line 52 against `HarnessKind` in `mj-core/src/config.rs`. The live check used `codex` and `deepseek` instead; `codex` was out of quota, so `deepseek` supplied the session with a tool call.
- Observation: SessionWiki drops and rebuilds its entire cache when the SQLite `user_version` differs from the library's `SCHEMA_VERSION` constant.
  Evidence: `src/index.rs` lines 375-395. A `sessionwiki` binary at a different schema version than the library Mjolnir links would force a full re-index on every alternation. Documented in milestone 7.

## Decision Log

- Decision: Mjolnir links SessionWiki as a Rust library and writes into the user's default SessionWiki index. No index path option.
  Rationale: The user's choice. One index means one search across every tool. Minimum surface.
  Date/Author: 2026-09-16, Jonathan Ellis with Fable.
- Decision: The adapter that reads Mjolnir sessions lives in Mjolnir, not in SessionWiki. The fork adds a generic hook for embedders to register adapters.
  Rationale: The adapter reads checkpoints through Mjolnir's typed `mj_checkpoint` reader, so a format change is a compile error here instead of a runtime parse failure in another repository. It also needs Mjolnir state (project directory, sub-agent flag). A generic hook is more likely to be accepted upstream than a Mjolnir-specific adapter with a zip dependency.
  Date/Author: 2026-09-16, Jonathan Ellis with Fable.
- Decision: All Mjolnir instances share one tool name, `mjolnir`. Each daemon indexes only its own instance's sessions. Reconciliation is scoped to the instance by a key prefix.
  Rationale: The user wants one search partition. Scoping reconciliation to the store's own keys fixes the cross-instance flip-flop without changing the search unit.
  Date/Author: 2026-09-16, Jonathan Ellis.
- Decision: Archiving a session removes its record, its checkpoint archive, and its image attachments, and keeps its git branch.
  Rationale: The user's choice. The branch is cheap and may hold work; the rest is recoverable from the index by compaction.
  Date/Author: 2026-09-16, Jonathan Ellis.
- Decision: The archive job only considers sessions in the `Stopped` state.
  Rationale: `Stopped` is defined as "checkpointed and torn down" in `mj-core/src/state.rs`, so a `Stopped` session always has a checkpoint. `Lost` and `Error` sessions are left alone for now; this is recorded as an open question in the retrospective.
  Date/Author: 2026-09-16, Fable.
- Decision: Restore builds a compacted hand-off with Mjolnir's existing cross-harness compaction and delivers it as hidden first-prompt context in a new session.
  Rationale: Mjolnir already does exactly this when a session moves between harnesses. Reuse beats a second summarizer.
  Date/Author: 2026-09-16, Jonathan Ellis with Fable.
- Decision: The daemon's frequent syncs are bounded with `since`; only the hourly tick runs a full sync. All daemon syncs are single-flight. A `database is locked` error from SessionWiki is retried on the next trigger, not reported as a failure.
  Rationale: SessionWiki's full sync walks every tool's store; the bounded form still reconciles deletions. SessionWiki holds a write transaction per adapter batch with a five-second busy timeout, so overlapping writers fail with a busy error.
  Date/Author: 2026-09-16, Fable.
- Decision: A session the user destroys by hand stays in the index and becomes archived at the next sync. No delete key on the Archived tab in this plan.
  Rationale: Matches SessionWiki's own "archive, never delete" model. Minimum surface.
  Date/Author: 2026-09-16, Fable.

## Outcomes & Retrospective

To be written at completion. Open question carried forward: whether `Lost` and `Error` sessions with a checkpoint should be archivable.

## Context and Orientation

Two repositories are involved. This one, Mjolnir, is a Cargo workspace. The other, SessionWiki, is checked out beside it at `../sessionwiki` with remote `origin` = `git@github.com:jbellis/sessionwiki.git` (the user's fork) and remote `upstream` = the original project by youdie006. The fork's `main` is identical to upstream `main` at the start of this work, at version 0.28.0.

Terms used below. A "session" is one conversation between the user and a coding agent, run by Mjolnir on a "target" (the local machine, a container, or an SSH host) under a "profile" (which agent harness to run, for example Codex or Claude Code). The "daemon" is the long-running background process, implemented in the `mj-controller` crate, that owns every session; the terminal UI (`mj-tui`, driven by `mj-cli`) and the web viewer (`mj-controller/src/web/`) are clients that talk to it over an HTTP API in `mj-controller/src/server/api.rs`, wrapped for Rust callers in `mj-client/src/daemon.rs`. An "instance" is an isolated copy of Mjolnir selected with `mj -i <name>` or the `MJ_INSTANCE` environment variable; it has its own config, database, daemon, and data directory nested under `instances/<name>` (see `with_instance_dir` in `mj-core/src/config.rs`). A "checkpoint" is a zip archive of a session's transcript and workspace state written when a session closes; a "record" is the row describing a session in Mjolnir's own SQLite database, loaded into memory as `mj_core::state::SessionRecord`.

Where things are in Mjolnir:

- Checkpoint files live in `mj_core::config::sessions_dir()`, which is `<data_dir>/sessions`. Each is named `<session_id>-<frontier>-archive-<32 hex>.hel.zip`; the frontier is a counter that increases with each new checkpoint of the same session, and `is_managed_checkpoint_archive_name` in `mj-controller/src/controller/checkpoint.rs` (around line 136) parses that name. Imported archives may be named `<session_id>.hel.zip`. The record's `checkpoint.archive_path` names the current archive for the session. `reconcile_managed_checkpoint_archives_in` in the same file deletes archives no record references.
- Inside a checkpoint, the transcript is the zip entry `canonical/session.json`, a `mj_core::archive::CanonicalSessionSnapshot`. Read it with `mj_checkpoint::archive::read_archive_verified(path)?.canonical_session()?` (`mj-checkpoint/src/archive.rs` lines 109 and 385). The snapshot's `transcript` is a list of `CanonicalTranscriptItem`, each with a `body` of kind `User { content }`, `Agent { chunks, .. }`, `Thought { .. }`, or `Tool { call, .. }`. `session.session_title` is the title. Text extraction helpers: `mj_core::transcript::materialized_content_text` for user content (it also strips Mjolnir's hidden prompt context) and `materialized_chunks_text` for agent chunks. A tool call's title is `call["title"]`. The reviewer in `mj-controller/src/review_host.rs` (`seed_from_session`, around line 2400) renders a transcript this way and is the model for the adapter.
- Session state: `mj_core::state::SessionState` has `Stopped` meaning checkpointed and torn down. `SessionRecord` has `project_directory`, `native_session_id`, `harness_kind`, `acp_session_title`, `session_title_override`, `updated_at` (RFC 3339 string), `checkpoint`, and a legacy `archived: bool` that current code ignores. Do not reuse the word `archived` for new record fields.
- Destroying a stopped session: `Controller::destroy_session_controlled` in `mj-controller/src/controller/lifecycle.rs` (around line 550) removes the managed worktree's branch with `cleanup_managed_worktree`, deletes the checkpoint file, removes image attachments, deletes the database row, and drops the in-memory record. By the time a session is `Stopped`, `retire_managed_worktree` has already removed the worktree checkout and kept the branch. The daemon wrapper `destroy_stopped_session` in `mj-controller/src/daemon.rs` (around line 1664) destroys sub-agent child sessions first, waits for deferred cleanup, then runs the controller call under `run_lifecycle(LifecycleKind::DestroyStopped, ..)`.
- Sub-agents: a session can spawn child sessions; the parent-child relation is in `crate::database::list_subagents` and the controller's `state.subagents` map.
- The daemon's periodic work loop is in `mj-controller/src/server_runtime.rs`: `prune_tick` (an hourly `tokio::time::interval`, around line 870) with its handler around line 1339. This is the only time-based job today.
- Compaction: `mj-controller/src/compaction.rs` has `compact_snapshot(snapshot, budget, backend)` which turns a `CanonicalSessionSnapshot` into a hand-off string; it does not validate the snapshot. `mj-controller/src/handoff.rs` `build_handoff_context(session_id, config, snapshot, context_bytes, cancel)` wraps it with utility-model discovery and a verbatim-tail fallback when no summarizer is available. The cross-harness resume path in `mj-controller/src/controller/resume.rs` runs it through `utility_handoff_while_cancellable` (around line 1967) and installs the result with `install_prompt_context` (around line 1391), so the first prompt the agent sees carries it invisibly to the user. `HANDOFF_PREAMBLE` in compaction.rs marks such text and `is_synthetic_handoff` recognizes it.
- Configuration: `mj_core::config::Config` in `mj-core/src/config.rs` (struct around line 1615) with sections like `ReviewConfig` (around line 93). `CONFIG_VERSION` (line 347) is bumped for every change and the upgrade test `every_previous_config_version_upgrades_with_compatible_defaults` must keep passing. The setup screen's editable defaults are in `mj-tui/src/setup/schema.rs` (`defaults`, plus label and help functions later in the same file) and section visibility in `mj-tui/src/setup.rs` `visible_keys`.
- Resume dialog: `mj-tui/src/resume.rs` holds `ResumeTab` (`Hel`, `Import`), `ResumeRowKey` (`Hel(session_id)`, `Native(harness, native_id)`), `merged_resume_rows` (around line 328, which merges live records with native sessions found by a background import scan and marks native ones already adopted), and the dialog state with `selected`, a text filter, and `apply_resume_profile` (around line 555) which receives scan results. The wizard that picks profile and target after a row is chosen is `ResumeWizard` in `mj-tui/src/wizards.rs` (around line 412) and its dashboard driver in `mj-tui/src/wizards/dashboard.rs`. `mj-cli/src/dashboard.rs` runs the background scans (`start_resume_discovery`, around line 2547) and dispatches actions to the daemon.
- Web viewer: `mj-controller/src/web/viewer.html` and `viewer.js`. The resume page already exists with a search box (`#resume-search`, viewer.js line 35), a list view, and a detail view.
- Instances: `mj_core::config::data_dir()` nests under `instances/<name>` when an instance is selected. `sessions_dir()` is therefore per instance.

Where things are in SessionWiki (`../sessionwiki`):

- `src/lib.rs` exposes `adapters`, `index`, `commands`, `model`, `redact`, and more.
- `src/adapters/mod.rs`: `pub trait Adapter { fn name(&self) -> &'static str; fn root(&self) -> Option<PathBuf>; fn discover(&self) -> Discovered; fn parse(&self, path: &Path) -> Result<Session>; fn store(&self) -> Option<Store> { None } fn parse_key(&self, key: &str) -> Result<Session> }`. A "shared store" adapter returns `Some(Store { keys: Vec<(String, i64)>, files, had_error })`: `keys` are `(stable key, change token)` pairs, the key doubles as the stored `path`, and `parse_key` parses one session by key. `adapters::all()` is the fixed registry.
- `src/model.rs`: `Session { id, tool: &'static str, path, project, started, ended, title, subagent, messages: Vec<Message { role: Role, text, ts }>, touched, edits, .. }` with `Role::{User, Assistant, Tool}`.
- `src/index.rs`: `open()` (default path `~/.local/share/sessionwiki/index.db`, overridable with the `SESSIONWIKI_DATA` environment variable naming a directory), `sync` and `sync_bounded(conn, only_tool, since)`, `archive_or_prune`, `recent(conn, limit, tool, project, tag, include_subagents)`, `search(conn, query, limit, tool, project) -> Vec<Hit { row: SessionRow, role, snippet }>` (whole query is one phrase; needs three or more characters), `search_like` (substring scan for shorter queries), `session_from_index(conn, &row) -> Session`, `forget`. `SessionRow` has `session_id, tool, path, project, title, started, msg_count, kind, preview, summary, tags, archived`. `archived` is true when the source went away; archived sessions keep their messages and a durable copy in the `archive` table.
- `src/commands.rs`: `brief_text(session, max_chars, include_tools, include_source) -> String` (private) renders a head-and-tail markdown briefing; `brief` is the CLI wrapper.
- Diagnostics go to stderr with `eprint!`; parse failures are counted and printed, not returned.

## Plan of Work

### Milestone 1: rusqlite 0.40

Mjolnir pins `rusqlite = "0.37"` in `mj-worker/Cargo.toml`, `mj-cli/Cargo.toml`, and `mj-controller/Cargo.toml`, all with the `bundled` feature. SessionWiki uses 0.40. Cargo refuses two crates that both link the native `sqlite3` library, so Mjolnir must move to 0.40 first. Change the three manifests, run `cargo update -p rusqlite` (and `libsqlite3-sys`), fix any API changes the compiler reports, run the full test suite outside the sandbox, and commit.

### Milestone 2: the SessionWiki fork

In `../sessionwiki`, create branch `mj-embed` from `main`. Make three library changes, each with a unit test in the existing test modules:

1. In `src/index.rs`, add `pub fn sync_with(conn: &mut Connection, adapters: &[Box<dyn Adapter>], since: Option<i64>) -> Result<()>` holding the current body of `sync_bounded`; `sync_bounded` builds its adapter list as today and calls `sync_with`. This lets an embedding program pass its own adapters.
2. In `src/adapters/mod.rs`, add to the trait `fn reconcile_scope(&self) -> Option<String> { None }` documented as: when `Some(prefix)`, deletion reconciliation for this adapter only considers indexed rows whose key starts with `prefix`, for stores that hold only part of a tool's sessions. In `archive_or_prune`, take `scope: Option<&str>` and, in Rust, drop live rows not starting with the prefix before computing `gone` (do not use SQL `LIKE`; paths contain `_`). Both call sites in `sync_with` pass `adapter.reconcile_scope().as_deref()`. The test: index two rows for one tool with different prefixes, sync an adapter scoped to one prefix that lists nothing, and assert only the in-scope row is archived.
3. In `src/commands.rs`, add `pub fn brief_markdown(session: &crate::model::Session, max_chars: usize, include_tools: bool) -> String { brief_text(session, max_chars, include_tools, false) }`.

Run `cargo test` in `../sessionwiki`, commit with a plain-language message, tag `v0.28.0-mj.1`, push branch and tag to `origin`, and open a pull request against `upstream` `main` titled for the embedder hooks. In Mjolnir, add to `[workspace.dependencies]` in the root `Cargo.toml`: `sessionwiki = { git = "https://github.com/jbellis/sessionwiki.git", tag = "v0.28.0-mj.1" }` and to `mj-controller/Cargo.toml`: `sessionwiki.workspace = true`. Confirm `cargo build` and that `cargo about generate` (see `.github/workflows/release.yml` line 39) still runs locally if `cargo-about` is installed; if it is not installed, note that in Progress and rely on CI. Commit.

### Milestone 3: config, adapter, indexer

Config. In `mj-core/src/config.rs`, add:

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SessionWikiConfig {
        #[serde(default, skip_serializing_if = "is_false")]
        pub enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub archive_after_days: Option<u32>,
    }

with `is_default`, a `pub sessionwiki: SessionWikiConfig` field on `Config` between `review` and `subagents`, a `CONFIG_VERSION` bump to 10 following the existing upgrade pattern, and validation that `archive_after_days` is `Some(0)` rejected with a clear message. In `mj-tui/src/setup/schema.rs`, add defaults `{"enabled":false,"archive_after_days":null}`, labels "SessionWiki" / "Enabled" / "Archive after (days)", and help text. The help for `archive_after_days`: "Stopped sessions older than this many days are removed from Mjolnir once SessionWiki has indexed them. The session's branch in the repository is kept; the checkpoint and any image attachments are deleted. Leave empty to keep every session." The section appears automatically through `visible_keys`; confirm it sorts sensibly and that the existing setup tests pass.

Adapter. New file `mj-controller/src/sessionwiki.rs` with `pub struct MjolnirAdapter { sessions_dir: PathBuf, records: BTreeMap<String, SessionRecord>, subagent_ids: BTreeSet<String> }` built by `MjolnirAdapter::from_state(&State)` (records cloned, sub-agent child ids collected from `state.subagents`). Implement `sessionwiki::adapters::Adapter`:

- `name()` returns `"mjolnir"`.
- `root()` returns `Some(self.sessions_dir.clone())`.
- `discover()` returns an empty `Discovered`; `parse()` bails. Both are unused for a shared-store adapter.
- `store()` lists `*.hel.zip` in `sessions_dir`, groups by session id using a `pub(crate)` version of `is_managed_checkpoint_archive_name` extended to return the parsed id and frontier (or a sibling function beside it; do not duplicate the parsing), keeps the highest frontier per id, and returns keys `format!("{}/{}", sessions_dir.display(), session_id)` with token = that archive's modification time in epoch seconds. A read error on the directory sets `had_error`. `files` lists the chosen archives.
- `reconcile_scope()` returns `Some(format!("{}/", sessions_dir.display()))`.
- `parse_key(key)` splits the trailing path component as the session id, picks the highest-frontier archive for that id again, reads it with `read_archive_verified` and `canonical_session`, and builds a `Session`: `id` = session id, `tool` = `"mjolnir"`, `path` = the key, `project` = the record's `project_directory` rendered as a string or empty, `title` = `session_title_override`, else `acp_session_title`, else the snapshot's `session_title`, else the first user prompt truncated to 80 characters, `started` / `ended` from the record's `created_at` / `updated_at`, `subagent` = whether the id is in `subagent_ids`, `messages` from the transcript: `User` bodies with non-empty `materialized_content_text` become `Role::User`; `Agent` bodies with non-empty `materialized_chunks_text` become `Role::Assistant`; `Tool` bodies with a non-empty `call["title"]` become `Role::Tool` holding just the title; `Thought` bodies are skipped. Message `ts` from `created_at_ms`. `touched` and `edits` empty.

Add a unit test that writes a small checkpoint with the existing test helpers in `mj-checkpoint` (look at `mj-controller/src/controller/checkpoint.rs` tests for how archives are written in tests), runs `store()` and `parse_key()`, and asserts the roles and texts.

Indexer. In `mj-controller/src/sessionwiki.rs`, add `pub struct WikiIndexer` owned by the daemon runtime with an async `request_sync(&self, full: bool)`. Implementation: a single background task with a `tokio::sync::Notify` and an `AtomicBool` "rerun requested" flag so triggers coalesce. Each run does `spawn_blocking`: load `Controller::load()?.state`, build `MjolnirAdapter::from_state`, `let mut adapters = sessionwiki::adapters::all(); adapters.push(Box::new(adapter));`, open with `sessionwiki::index::open()`, call `sync_with(&mut conn, &adapters, since)` where `since` is `None` for a full run and otherwise the epoch second of the last successful run minus 60. Record the completion time. Log errors with `tracing::warn!` and context; a `rusqlite` busy or locked error is logged at debug level and the flag is set so the next trigger retries. When `config.sessionwiki.enabled` is false the indexer does nothing. Triggers: a session entering `Stopped` (find where the daemon observes that transition; the lifecycle supervisor in `daemon.rs` is the likely place), the hourly `prune_tick` (full), and the search API when the last run is older than 60 seconds (bounded). Note that SessionWiki prints progress to the daemon's stderr; accept that.

Acceptance: with isolated `MJ_CONFIG_DIR`, `MJ_DATA_DIR`, and `SESSIONWIKI_DATA`, and `[sessionwiki] enabled = true`, start a session on the `zcode` profile and the `local-bare` target, send one prompt, close it, then run `SESSIONWIKI_DATA=<same dir> ../sessionwiki/target/debug/sessionwiki list --tool mjolnir` and see the session, and `sessionwiki brief <id> --tools` showing user, assistant, and tool lines.

### Milestone 4: API, client, terminal UI

API in `mj-controller/src/server/api.rs`, added to the same router as the session routes so the existing authentication applies:

- `GET /wiki/search?q=<text>&limit=<n>` returns `{ "rows": [WikiRow] }`. Empty or missing `q` uses `index::recent(conn, limit, None, None, None, false)`. A query shorter than three characters uses `search_like`; otherwise `search`. Each `WikiRow` is `{ id, tool, project, title, started, msgs, preview, archived, snippet, hel_session_id }`. `hel_session_id` is set when `tool == "mjolnir"` and the trailing component of the row's path names a record in this daemon's state. Default `limit` 50, maximum 200. Before querying, call `request_sync(false)` when the last run is older than 60 seconds; do not wait for it.
- `GET /wiki/sessions/{id}/brief?max_chars=<n>` returns `{ "markdown": .. }` from `session_from_index` and `brief_markdown(&session, max_chars, true)`. Default 24000.
- `POST /wiki/sessions/{id}/restore` with body `{ profile_id, target_id, project_directory?, model?, effort? }` (mirror whatever the existing start-session body accepts for those fields) returns `{ "session_id": .. }`.
- Every route returns 409 with a plain message when `config.sessionwiki.enabled` is false, and 404 when the id is unknown.

Restore, in `mj-controller/src/controller/resume.rs` or a new `restore.rs` beside it: build a `CanonicalSessionSnapshot` with `event_frontier: 0`, `event_frontier_digest: EVENT_FRONTIER_GENESIS_DIGEST`, `session.last_activity_at_ms: None`, `session.session_title` from the row, and transcript items in order: user messages as `User { content: [{"type":"text","text": ..}] }`, assistant messages as `Agent` with a single text chunk in the shape `materialized_chunks_text` reads (inspect that helper and `turns_from_snapshot` in compaction.rs to match), tool messages as `Tool` items whose `call` is `{"title": ..}` with an empty result, or skipped if `turns_from_snapshot` cannot take them. Positions increase by one; `stable_id` is `wiki-<n>`. Then create the session exactly as the normal start path does, and once it is provisioned run `build_handoff_context` with the profile's context budget and install it with `install_prompt_context`, the same way the cross-harness path does around resume.rs line 1391. Add `ARCHIVE_HANDOFF_PREAMBLE` beside `HANDOFF_PREAMBLE` in compaction.rs reading "Archived session restored from SessionWiki." and extend `is_synthetic_handoff` to recognize it. The restored session's title is the row's title.

Client: add `wiki_search`, `wiki_brief`, `wiki_restore` wrappers in `mj-client/src/daemon.rs` following the neighbouring wrappers.

Terminal UI in `mj-tui/src/resume.rs` and the drivers named in Context:

- `ResumeTab::Archive` after `Import`, `ResumeRowKey::Archive(wiki_id)`. Tab title " Archived sessions · newest first ". When `config.sessionwiki.enabled` is false the tab body reads "Enable SessionWiki in Setup to search and archive sessions".
- Rows: `merged_resume_rows` gains a `wiki: &[WikiRow]` input. An Archive row is a wiki row with `archived == true`, no `hel_session_id`, and, for tools other than `mjolnir`, no import-scan row with the same harness and native id (map SessionWiki tool names to `HarnessKind` where one exists; use `native_id_of` on the row's path for the native id). On the Hel and Import tabs, a wiki hit whose `hel_session_id` or native id matches a row attaches its snippet to that row's details.
- Search: the existing text filter keeps filtering locally. When SessionWiki is enabled, typing also schedules `wiki_search` on a 250 ms debounce in a background task in `mj-cli/src/dashboard.rs`, delivered to the dialog the same way import-scan results are, keyed by a request counter so stale results are dropped.
- Preview: a pane under the list showing the brief for the selected row when it has a wiki id, fetched in the background on selection change, cached per id for the dialog's lifetime, rendered as wrapped plain text with the markdown left as is.
- Enter on an Archive row opens `ResumeWizard` with a new source variant carrying the wiki id (add an enum for the wizard's source if none exists; today it carries a session id), and on confirmation dispatches a new dashboard action that calls `wiki_restore` and then selects the new session.

Acceptance: with the same isolated setup as milestone 3, after closing a session and deleting its `.hel.zip` by hand and waiting for or forcing a sync, the row appears on the Archived tab; typing a word from the transcript filters to it and the preview shows the brief; Enter, choose profile and target, and a new session starts whose first agent reply references the earlier work. Check the daemon log for the hand-off installation.

### Milestone 5: archive job

In `mj-controller/src/controller/lifecycle.rs`, split `destroy_session_controlled` so the branch cleanup is a parameter: add `pub enum BranchDisposition { Delete, Keep }` and `destroy_session_controlled_with(session_id, executor, disposition)`; the existing function calls it with `Delete`. In `daemon.rs`, add `archive_stopped_session(session_id)` mirroring `destroy_stopped_session` (children first, deferred cleanup wait, existence check, `run_lifecycle` with a new `LifecycleKind::ArchiveStopped` or the existing kind if adding one is invasive) that calls the `Keep` variant.

In `server_runtime.rs` on the hourly tick, when `config.sessionwiki.enabled` and `archive_after_days` is `Some(n)`: run `request_sync(true)` and await its completion (add a `sync_now` that returns when the run finishes), then in `spawn_blocking` load state and select records with `state == Stopped`, `updated_at` older than `n` days, and not a parent of any non-Stopped child. For each, open the index read-only and confirm a `files` row exists for key `<sessions_dir>/<id>` with `msg_count > 0` and `archived_at IS NULL` (use `index::resolve` or a small query through the public API; if none fits, add `pub fn indexed_message_count(conn, key) -> Result<Option<i64>>` to the fork as a fourth change and re-tag). Sessions that fail that check are skipped with a warn log naming the id. Then call `archive_stopped_session` for each, and finally `request_sync(false)` so the rows flip to archived. Log one info line per archived session.

Acceptance: set `archive_after_days = 1` in the isolated config, close a session, set its record's `updated_at` two days back directly in the isolated database (state the exact SQL in Progress when known), trigger the tick (add a test-only environment variable that shortens the interval, or restart the daemon and wait), and observe: the record is gone from `mj` listings, the `.hel.zip` is gone, `git branch --list 'mj/*'` (check the actual branch prefix used by `ManagedWorktree`) still shows the branch, and `sessionwiki list --tool mjolnir` shows the row as archived.

### Milestone 6: web viewer

In `viewer.js` and `viewer.html`: the resume page's search box calls `GET /wiki/search` with the same 250 ms debounce; an "Archived" section lists rows with `archived` and no `hel_session_id`; the detail view for a wiki row shows the brief from `GET /wiki/sessions/{id}/brief`; a "Restore" button uses the existing profile and target selectors (viewer.js around lines 1760-1787) and posts to the restore route, then navigates to the new session. When the search route returns 409, hide the section and show one line saying SessionWiki is disabled. Add a Node test beside the existing web tests if the repository has them for viewer.js; otherwise validate by hand in a browser against the `[phone]` bind address and record the transcript in Artifacts.

### Milestone 7: documentation and follow-up

Add a `sessionwiki` row to the configuration table in `docs/src/content/docs/configuration.md` and a short section to `docs/src/content/docs/sessions.md` describing the Archived tab, restore by compaction, what archiving deletes and keeps, and this rule: the `sessionwiki` command-line tool you install must be the version Mjolnir links (state it in the docs as the exact version), because a different index schema version forces a full re-index every time the two alternate. Record the fork's location and tag in `.agents/docs/sessionwiki-fork.md`. Open a follow-up ticket in the Mjolnir issue tracker for a SessionWiki skill available inside client sessions, noting the binary and index must be reachable from container and SSH targets.

### Milestone 8: release gate

A git dependency cannot be published to crates.io. Before the next release, publish the fork under the package name `brokk-sessionwiki` (on a `publish` branch of the fork that renames the package and keeps `[[bin]] name = "sessionwiki"`), then change the workspace dependency to `sessionwiki = { package = "brokk-sessionwiki", version = "0.28.0" }` and re-lock. This needs crates.io credentials the automated work does not have; the plan stops at preparing the branch and stating the command.

## Concrete Steps

All Mjolnir commands run in `/home/jonathan/Projects/hel3`; SessionWiki commands run in `/home/jonathan/Projects/sessionwiki`.

Build and test (outside the sandbox, on the dev profile):

    cargo build
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

Isolated live environment for acceptance checks, using the scratchpad directory `S=/tmp/claude-1000/-home-jonathan-Projects-hel3/68c2d9bc-4218-4756-9eaa-7d0de76bb3b9/scratchpad`:

    export MJ_CONFIG_DIR=$S/mj-config MJ_DATA_DIR=$S/mj-data SESSIONWIKI_DATA=$S/wiki
    mkdir -p $MJ_CONFIG_DIR $MJ_DATA_DIR $SESSIONWIKI_DATA
    # copy the user's config.toml into $MJ_CONFIG_DIR, add [sessionwiki] enabled = true
    ./target/debug/mj daemon ...   # see `mj --help` for the daemon start command
    ./target/debug/mj new --profile zcode --target local-bare ...

Fork CLI for verification:

    (cd ../sessionwiki && cargo build)
    SESSIONWIKI_DATA=$S/wiki ../sessionwiki/target/debug/sessionwiki list --tool mjolnir
    SESSIONWIKI_DATA=$S/wiki ../sessionwiki/target/debug/sessionwiki brief <id> --tools

Milestone 3, run on 2026-09-17. The isolated environment, from the repository root:

    S=/tmp/claude-1000/-home-jonathan-Projects-hel3/68c2d9bc-4218-4756-9eaa-7d0de76bb3b9/scratchpad
    export MJ_CONFIG_DIR=$S/mj-config MJ_DATA_DIR=$S/mj-data SESSIONWIKI_DATA=$S/wiki
    mkdir -p $MJ_CONFIG_DIR $MJ_DATA_DIR $SESSIONWIKI_DATA $S/project
    cp ~/.config/mjolnir/config.toml $MJ_CONFIG_DIR/config.toml
    # appended to that copy:
    #   [phone]
    #   enabled = true
    #   bind = "127.0.0.1:37650"
    #
    #   [sessionwiki]
    #   enabled = true
    (cd $S/project && git init -q && echo hello > README.md && git add README.md &&
     git -c user.email=a@b -c user.name=t commit -qm init)
    ./target/debug/mj new --profile deepseek --target localhost \
      --project-directory $S/project --workspace-id default \
      "Read README.md and tell me the single word it contains."
    ./target/debug/mj wait --session <id>
    ./target/debug/mj close --session <id>

The daemon starts by itself on the first client command. Two details the plan
did not have: the copied config must bind the web viewer somewhere other than
port 3765, or the isolated daemon collides with the real one and refuses every
request; and `mj new` needs `--workspace-id default`, or the API answers 500
with "create a workspace before starting a phone session" in the daemon log.

Verification, after the sync had run:

    (cd /home/jonathan/Projects/sessionwiki && cargo build)
    SESSIONWIKI_DATA=$S/wiki /home/jonathan/Projects/sessionwiki/target/debug/sessionwiki list --tool mjolnir
    SESSIONWIKI_DATA=$S/wiki /home/jonathan/Projects/sessionwiki/target/debug/sessionwiki brief 1e0d61a75458ca85a9f040878e6aee8e --tools

Update this section with the exact commands and observed output as each milestone lands.

## Validation and Acceptance

Each milestone above ends with an acceptance paragraph phrased as behavior. In addition, every milestone that touches Rust must pass `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` on the dev profile outside the sandbox before its commit. The fork must pass its own `cargo test`. Tests to add are named in each milestone; they must fail before the change and pass after.

## Idempotence and Recovery

Every step is additive and can be rerun. The rusqlite bump is a manifest edit plus `cargo update -p`, which can be repeated. The fork branch and tag can be recreated; if the tag must move before anyone depends on it, delete and recreate it, and never move it after Mjolnir's `Cargo.lock` references it in a commit. All live checks use isolated directories under the scratchpad; never point `SESSIONWIKI_DATA` at the user's real index during testing, and never run the archive job against the user's real data directory. If the archive job misbehaves, set `archive_after_days` back to empty; archived sessions remain restorable from the index.

## Artifacts and Notes

Add transcripts proving each acceptance here as milestones complete.

Milestone 3, 2026-09-17. `sessionwiki list --tool mjolnir`:

    ID            TOOL         WHEN        MSGS  PROJECT                  TITLE
    1e0d61a75458… mjolnir      16m ago        3  …/1e0d61a75458ca85a9f04… project via deepseek
    04852244ba07… mjolnir      17m ago        2  …/04852244ba07430fabca2… project via codex

`sessionwiki brief 1e0d61a75458ca85a9f040878e6aee8e --tools`:

    # Previous session: project via deepseek

    - Tool: unknown | Project: /tmp/.../scratchpad/project/.mj/worktrees/1e0d61a75458ca85a9f040878e6aee8e/ | Date: 2026-09-17 00:23
    - Source: /tmp/.../scratchpad/mj-data/sessions/1e0d61a75458ca85a9f040878e6aee8e

    **User:**
    Read README.md and tell me the single word it contains.

    > [tool] Read file '/tmp/.../README.md'

    **Assistant:**
    The single word in README.md is: **hello**

User, tool, and assistant lines are all present, which is what the milestone
asked for. `Tool: unknown` is the standalone binary's registry lookup, not a
defect in the stored row; see Surprises.

Workspace validation for the commit: `cargo build`, `cargo test` (3485 passed,
0 failed), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`.

## Interfaces and Dependencies

SessionWiki fork (`../sessionwiki`, branch `mj-embed`, tag `v0.28.0-mj.1`):

    // src/index.rs
    pub fn sync_with(conn: &mut Connection, adapters: &[Box<dyn Adapter>], since: Option<i64>) -> Result<()>;
    // src/adapters/mod.rs, on trait Adapter
    fn reconcile_scope(&self) -> Option<String> { None }
    // src/commands.rs
    pub fn brief_markdown(session: &crate::model::Session, max_chars: usize, include_tools: bool) -> String;

Mjolnir:

    // mj-core/src/config.rs
    pub struct SessionWikiConfig { pub enabled: bool, pub archive_after_days: Option<u32> }
    // mj-controller/src/sessionwiki.rs
    pub struct MjolnirAdapter;            // implements sessionwiki::adapters::Adapter
    impl MjolnirAdapter { pub fn from_state(state: &mj_core::state::State) -> Self; }
    pub struct WikiIndexer;               // request_sync(full: bool), sync_now(full: bool) -> Result<()>
    pub struct WikiRow { id, tool, project, title, started, msgs, preview, archived, snippet, hel_session_id }
    // mj-controller/src/controller/lifecycle.rs
    pub enum BranchDisposition { Delete, Keep }
    // mj-client/src/daemon.rs
    pub async fn wiki_search(&self, q: &str, limit: usize) -> Result<Vec<WikiRow>>;
    pub async fn wiki_brief(&self, id: &str, max_chars: usize) -> Result<String>;
    pub async fn wiki_restore(&self, id: &str, request: WikiRestoreRequest) -> Result<String>;
    // mj-tui/src/resume.rs
    enum ResumeTab { Hel, Import, Archive }
    enum ResumeRowKey { Hel(String), Native(HarnessKind, String), Archive(String) }

Dependency: `sessionwiki` via git tag during development (milestone 2), via crates.io as `brokk-sessionwiki` before release (milestone 8). `rusqlite` 0.40 with `bundled` everywhere.

Revision note (2026-09-16): first version, written after a review of an earlier draft that found the crates.io publishing conflict, the cross-instance reconciliation flip-flop, and the schema-skew hazard. Those findings are recorded in Surprises & Discoveries and their resolutions in the Decision Log.
