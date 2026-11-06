# Search prompt history through a trigram index, eight matches at a time

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

In the terminal chat, Ctrl-R opens a reverse search over the prompts the user has already sent. The web viewer has the same search. Both used to load prompt rows from SQLite and test each one in Rust with a lower-cased substring check. That cost grew with the size of the history, not with what the user sees. The terminal read the whole `prompt_history` table on every keystroke. The web route stopped after 4,096 rows, so it could miss matches.

After this change, each search asks a full-text index for the next few matches and stops as soon as it has them. The terminal keeps a queue of eight matches ahead of the one shown and refills it in the background as the user steps through. Matches still come from the smallest scope first: this session, then the rest of the project, then every other project, each newest first. A prompt that appears in several scopes is shown once, at the smallest.

To see it working, open a session in the terminal and press Ctrl-R. Type three or more characters of an earlier prompt from another session in the same project. The prompt appears. Pressing Ctrl-R again steps to older matches without a pause, even across thousands of stored prompts. Typing one or two characters shows "type 3+ characters", because the index matches substrings of three characters or more.


## Progress

- [x] (2026-11-06) Research and prototype: confirmed trigram FTS5 behaviour on SQLite 3.46 and the bundled 3.53.2, measured index build time and size on a copy of the live store.
- [x] (2026-11-06) Milestone 1: schema revision 114 adds the `prompt_history_text` index and its triggers; `prompt_text_index_covers_existing_rows_and_follows_previous_writers` passes.
- [x] (2026-11-06) Milestone 2: `search_prompt_page_from` serves the terminal (`client_backend.rs`) and the web route (`run.rs`); the session tier's filter is written `+p.session_id` so the full-text index drives it; store tests cover ordering, paging with `limit = 2`, short queries and quotes.
- [x] (2026-11-06) Milestone 3: `HistorySearch` keeps eight matches queued and refills from `next`; `history_search_keeps_eight_matches_queued_and_refills_from_the_cursor` passes; the web palette explains queries shorter than 3 characters.
- [x] (2026-11-06) Revision 114 also limits `prompt_history` to top-level sessions that still exist (see the Decision Log); `prompt_history_keeps_top_level_prompts_and_its_index_follows_previous_writers` and the `database::`, prompt and history filters (326 tests) pass.
- [x] (2026-11-06) Milestone 4: docs, workspace clippy, and the Ctrl-R step in `tests/e2e/tui_components_chat.py` passed against a real daemon and TUI (seed 61113, captures `008-history-search` and `009-history-search-scope`). The rest of that scenario is stale on master: it waited for a composer titled "Prompt" (fixed to the `╭  mic` border) and for a "Choose a model" dialog that is now a "model" menu (not fixed). `Lab.environment()` no longer passes the host session's `CLAUDE*`, `CODEX_*` and `MJ_*` variables to lab daemons. The full suites ran once with 11 worker-startup timeouts in resume, move and proxy tests, which the user chose not to rerun.
- [ ] Commit, review, push.


## Surprises & Discoveries

- Observation: the FTS5 trigram tokenizer folds case for non-ASCII letters, so the index keeps the old case-insensitive behaviour for text such as "Ärger".
  Evidence: a prototype query for `"ärger"` returned the row `Ärger mit Übersetzung`.

- Observation: a query shorter than three characters matches nothing and raises no error.
  Evidence: `SELECT count(*) FROM t WHERE t MATCH '"fi"'` returned 0.

- Observation: a newest-first page with a cursor is answered by the index itself, with no sort step. SQLite passes both the `MATCH` and the `rowid < ?` constraint to FTS5 and reads rows in descending rowid order.
  Evidence: `EXPLAIN QUERY PLAN` printed `SCAN f VIRTUAL TABLE INDEX 192:M1<` followed by `SEARCH p USING INTEGER PRIMARY KEY (rowid=?)`.

- Observation: a page for a session that has no matches, under a query that matches 160,000 rows elsewhere, reads every match and takes about 85 ms. The live store is far smaller, so this is not a practical concern.
  Evidence: the live store held 13,891 prompts in 8,719 sessions, at most 291 in one session.

- Observation: on the real schema, SQLite planned the session tier from the `prompt_history_session_recent` index and probed the full-text index once per row. That took 75 ms for a 291-prompt session. Writing the filter as `+p.session_id = ?` hides it from index selection, so the full-text index drives the query, and the same page took 6 ms. The project and other-project tiers already let the full-text index drive, because their filters are correlated `EXISTS` subqueries.
  Evidence: `EXPLAIN QUERY PLAN` showed `SEARCH p USING INDEX prompt_history_session_recent ... SCAN f VIRTUAL TABLE INDEX 192:=M1 ... USE TEMP B-TREE FOR ORDER BY` before the change and `SCAN f VIRTUAL TABLE INDEX 192:M1< ; SEARCH p USING INTEGER PRIMARY KEY` after it, on a copy of the live store.

- Observation: SQLite refuses `ALTER TABLE ... RENAME` while a trigger on another table names a table that has just been dropped. Revision 77 recreates `prompt_history` this way, so test fixtures that rewind a current store below revision 114 and rerun older migrations failed with "error in trigger prompt_history_child_registered: no such table: main.prompt_history". Real stores below revision 114 have no such triggers. The fix is the fixture helper `restore_pre114_prompt_history` in `mj-controller/src/database/schema.rs`, called from `restore_review_state_schema`. A future migration that recreates `prompt_history`, `sessions` or `subagent_sessions` must drop and recreate these triggers itself.
  Evidence: 13 schema and store tests failed with that message before the helper and passed after it.

- Observation: building the index on a copy of the live store's prompt text (61 MB) took 10.9 s and grew the file from 64 MB to 206 MB. Queries on that copy took under 5 ms.
  Evidence: prototype output `rebuild s 10.9 base MB 64 with index MB 206`; `"the"` matched 11,330 rows in 2.8 ms.


## Decision Log

- Decision: index `prompt_history` with an FTS5 table that uses the trigram tokenizer, instead of querying the SessionWiki index.
  Rationale: SessionWiki indexes transcript messages. It lacks prompts the user abandoned with Ctrl-C, includes sub-agent instructions as user messages, lags by up to a minute, scopes projects by filesystem path, and orders by index insertion. An index on `prompt_history` keeps today's content and project scoping exactly.
  Date/Author: 2026-11-06, user and agent.

- Decision: every tier, including this session, queries the index. There is no substring scan in Rust.
  Rationale: triggers keep the index current within the transaction that records a prompt, so it is never behind the session. Two matching modes behaved differently for short queries and were confusing.
  Date/Author: 2026-11-06, user.

- Decision: queries shorter than three characters return no matches, and the terminal footer says "type 3+ characters".
  Rationale: the trigram index cannot answer them, and a separate scan for short queries would bring back the second mode.
  Date/Author: 2026-11-06, agent, following from the decision above.

- Decision: the terminal keeps eight unvisited matches ahead of the selected one and requests the next page from a cursor when fewer remain.
  Rationale: the work then follows what the user steps through, not the size of the history.
  Date/Author: 2026-11-06, user.

- Decision: the web viewer keeps its single list of up to 40 matches and its "More matches than N" row. It calls the same paged search with a limit of 40 and reports `truncated` when a next cursor exists.
  Rationale: its palette shows a list rather than stepping through matches, so a queue gives it nothing. One search function still serves both surfaces.
  Date/Author: 2026-11-06, agent.

- Decision: the schema change is compatible and leaves the compatibility floor at its current value.
  Rationale: older readers ignore the new table. Older writers insert into `prompt_history`, and the triggers live in the database, so they keep the index current. Every shipped build links SQLite through `rusqlite` with the `bundled` feature, and the bundled `libsqlite3-sys` builds 0.26, 0.30 and 0.38 all compile with `SQLITE_ENABLE_FTS5`, so older builds can run the triggers.
  Date/Author: 2026-11-06, agent.

- Decision: `prompt_history` holds only prompts of top-level sessions that still exist. Revision 114 deletes the rows of sub-agent children and of destroyed sessions, and triggers keep the rule for every writer: `prompt_history_top_level_only` ignores inserts for sessions in `subagent_sessions`, `prompt_history_child_registered` deletes a child's rows when it is registered, and `prompt_history_session_destroyed` deletes a session's rows when its `sessions` row is deleted. The index then covers the whole table.
  Rationale: on the live store, 5,333 child sessions held one row each (their spawn instructions, 45.5 MB of 62.3 MB), and destroyed sessions held 3,908 rows (12.9 MB). Neither is text a person typed into a conversation they can still open. Only Ctrl-R, Up/Ctrl-P and the web palette read the table, so nothing else loses data. This reverses the earlier rule in `delete_session` that kept a destroyed session's prompts. With about 4 MB of text left, building the index inside the migration takes about a second, so the build stays synchronous. The user confirmed the deletion.
  Date/Author: 2026-11-06, user.

- Decision: a full page reads at most 16 further rows (`PEEK_ROWS`) for a match that repeats no listed text; past that it returns a cursor meaning "may continue", and the web palette says "Stopped after N matches" instead of "More matches than N".
  Rationale: reading past every repeat made a page's cost follow the number of repeats, which review found could be thousands of rows per keystroke. A bounded peek keeps the answer exact in the common case and the cost bounded in all cases.
  Date/Author: 2026-11-06, agent, after review.

- Decision: each tier query groups its matches by text in SQL (`GROUP BY p.text`, ordered by each text's newest row), leaves out texts an earlier tier holds, and pages with `HAVING newest < cursor`. This replaces the Rust `seen` set, the 16-row peek past a full page, and the composer's removal of repeats.
  Rationale: three review rounds found defects in handling repeated rows after the fact (stalled refills, unbounded scans before and after a full page, refills walking repeat-only pages). The kept live rows hold 4,828 prompts but 4,266 distinct texts, with `/clear` 173 times and `continue` 99 times. Grouping makes each text appear once across all pages by construction. A request costs one query per tier and follows the tier's matches; on a copy of the live store the session and project tiers took under 5 ms and the other-projects tier 12 to 14 ms for common queries.
  Date/Author: 2026-11-06, agent, after review.

- Decision: an open chat lists its session's stored prompts again after each `TurnCompleted`, and Up/Ctrl-P show each text once at its newest place.
  Rationale: prompts sent to the session from the web viewer or the API are recorded by the daemon, not by this chat. `PromptAccepted` events also carry agent `send_message` turns, so the event text cannot feed Up directly; the stored listing holds only recorded user prompts, and its record is written before the turn ends.
  Date/Author: 2026-11-06, agent, after review.

- Decision: web and API prompt records are written by a task that holds an upgrade admission (`crate::upgrade::activity("prompt history record")`), spawned with no await after acceptance.
  Rationale: AGENTS.md requires accepted daemon work to survive client disconnects and to take part in handoff.
  Date/Author: 2026-11-06, agent, after review.

- Decision: remove the in-memory substring fallback `ChatState::local_history_search_results`.
  Rationale: it ran only when a chat had no project ID, which an open chat always has (`ActiveChat` calls `set_history_context` on open), so it was a second matching mode reachable only from tests.
  Date/Author: 2026-11-06, agent.


## Outcomes & Retrospective

Ctrl-R and the web palette now search a trigram index over prompts of top-level sessions that still exist, smallest scope first, eight matches ahead at a time. Work per keystroke follows the matches shown. The live store will keep about 4,750 of its 13,984 rows. Two gaps remain. Prompts sent through the HTTP API's `prompt` action, which the web viewer uses, are not recorded in `prompt_history` at all (`submit_prompt_with_id` in `mj-controller/src/server_runtime/api.rs` submits without `record_prompt`), so they never appear in history; this predates the plan. The tmux component scenario is stale after the history step. Lesson: measure what a table holds before indexing it; 73% of its text was sub-agent instructions.


## Context and Orientation

The project is a Rust workspace. The parts involved are these.

`prompt_history` is a table in the daemon's SQLite store (the daemon is the background process `mj daemon-run`; its store code is in `mj-controller/src/database/`). Each row has `history_id INTEGER PRIMARY KEY`, `session_id`, a nullable `event_ordinal`, `submitted_at` and `text`. Rows are written by `record_prompt_to`, when a prompt is delivered to a session, and by `record_abandoned_prompt_to`, when the user clears a typed draft with Ctrl-C. Both functions are in `mj-controller/src/database/prompts.rs`. The table `session_contexts(session_id, bundle_id, ...)` maps each session to its project. A "bundle" is mj's word for a project. `super::projects::session_project_id` turns a bundle ID into its canonical ID after projects are merged.

Schema changes are numbered revisions in `migrate_schema` in `mj-controller/src/database/schema.rs`, guarded by `if version < N`. The current number is the constant `SCHEMA_VERSION` in `mj-controller/src/database.rs` (113 when this plan merged; it was 110 when the plan began, and revisions 111 to 113 landed upstream meanwhile). AGENTS.md requires every migration to be classified as compatible or breaking, with a comment beside it saying why. A compatible migration does not update `schema_compatibility.minimum_compatible_version`.

FTS5 is SQLite's full-text index. An "external content" FTS5 table stores only the index and reads the text from another table by rowid. Here that is `prompt_history`, with `content_rowid='history_id'`. The trigram tokenizer indexes every three-character sequence, so a quoted query such as `"the parser"` matches any row containing that substring, ignoring case. Triggers on `prompt_history` must insert and delete index entries, because an external-content table does not follow its source automatically.

The scope type is `mj_core::storage::HistoryScope` (`Session`, `Project`, `All`) in `mj-core/src/storage.rs`. A "tier" is one of the disjoint row sets a scope reads, in order: this session; other sessions in this project; sessions in other projects. `Session` reads the first tier, `Project` the first two, and `All` all three.

The terminal chat is the `mj-chat` crate. `mj-chat/src/chat/history.rs` holds the search state `HistorySearch`, the key handling inside an open search (`handle_history_search_key`), and Up/Ctrl-P history walking (`move_history`). A search request is queued in `pending_history_search`. `dispatch_history_search_request` in `mj-chat/src/chat/active.rs` runs it on a Tokio task and sends the result back as `ComposerIoUpdate::HistorySearchResults`, defined in `mj-chat/src/chat/composer.rs`. A `generation` counter drops results for an older query. The chat reaches the store through the trait `mj_client::session::SessionHandleBackend` (`mj-client/src/session.rs`), method `search_prompts`. The real implementation is in `mj-controller/src/session_manager/client_backend.rs` and reads the database in the same process. Test fakes implement the trait in `mj-client/src/session.rs`, `mj-chat/src/chat/active/tests.rs`, `mj-controller/src/server_runtime/api/tests.rs` and `mj-controller/src/server/api/tests.rs`. When a chat opens, `ActiveChat` also prefetches this session's prompts for Up/Ctrl-P walking (`ChatIoUpdate::SessionHistoryPrefetched`, applied by `set_session_history`).

The web viewer calls `GET /api/sessions/{id}/history?q=...&scope=project` (`prompt_history` in `mj-controller/src/server/handlers.rs`). The daemon answers in `mj-controller/src/server_runtime/run.rs` (`ClientStateRequest::History`) with `ViewerPromptHistory { entries, truncated }`, limited to `crate::server::MAX_HISTORY_MATCHES` (40). The viewer's palette code is `searchHistory` in `mj-controller/src/web/viewer.js`.


## Plan of Work

Milestone 1 adds the index. In `mj-controller/src/database/schema.rs`, after the last existing block, add a `version < 114` block in one `BEGIN IMMEDIATE` transaction:

    CREATE VIRTUAL TABLE IF NOT EXISTS prompt_history_text USING fts5(
        text, content='prompt_history', content_rowid='history_id', tokenize='trigram');
    CREATE TRIGGER IF NOT EXISTS prompt_history_text_insert AFTER INSERT ON prompt_history
        BEGIN INSERT INTO prompt_history_text(rowid, text) VALUES (new.history_id, new.text); END;
    CREATE TRIGGER IF NOT EXISTS prompt_history_text_delete AFTER DELETE ON prompt_history
        BEGIN INSERT INTO prompt_history_text(prompt_history_text, rowid, text)
              VALUES ('delete', old.history_id, old.text); END;
    CREATE TRIGGER IF NOT EXISTS prompt_history_text_update AFTER UPDATE OF text ON prompt_history
        BEGIN INSERT INTO prompt_history_text(prompt_history_text, rowid, text)
              VALUES ('delete', old.history_id, old.text);
              INSERT INTO prompt_history_text(rowid, text) VALUES (new.history_id, new.text); END;
    INSERT INTO prompt_history_text(prompt_history_text) VALUES ('rebuild');

It then records revision 114 in `schema_migrations` and sets `PRAGMA user_version = 114`, without touching the compatibility floor. Place a "Compatible:" comment above it giving the reasoning in the Decision Log. Raise `SCHEMA_VERSION` to 114. Also check whether a fresh database is created from `baseline.sql` and then migrated; if so, the new block covers fresh stores too. Note in the comment that a future migration that recreates `prompt_history` (as revision 77 did) must recreate the triggers.

Milestone 2 replaces the two search functions in `mj-controller/src/database/prompts.rs` (`search_prompts_from`, the unbounded one, and `search_prompts_bounded_from`, the one capped at 4,096 rows) with one function:

    pub(super) fn search_prompt_page_from(path, session_id, bundle_id, scope, query,
        after: Option<HistoryCursor>, limit: usize) -> Result<PromptHistoryPage>

It reads tiers in order, starting at the cursor's tier and rowid. Each tier is one SQL query: `prompt_history_text MATCH ?` joined to `prompt_history` by rowid, with the tier's filter, `rowid < ?before`, `ORDER BY rowid DESC LIMIT ?`. The tier filters are `p.session_id = ?` for this session; `p.session_id <> ? AND c.bundle_id = ?` through `session_contexts` for the rest of the project; and `NOT EXISTS (SELECT 1 FROM session_contexts c WHERE c.session_id = p.session_id AND c.bundle_id = ?)` for other projects. The query string is passed as one FTS5 phrase: wrap it in double quotes and double any embedded quote. A query of fewer than three characters returns an empty page with no cursor, without touching the database. Repeated text within a page is dropped. The function returns once it holds `limit` entries, setting `next` to the tier and the last rowid read, or when the last tier is exhausted, with `next = None`. To know whether more exist, it asks each tier for one row more than it still needs.

Define the cursor and the page in `mj-core/src/storage.rs`, beside `HistoryScope`:

    pub struct HistoryCursor { pub tier: u8, pub before: i64 }
    pub struct PromptHistoryPage { pub entries: Vec<PromptHistoryEntry>, pub next: Option<HistoryCursor> }

Replace `BoundedPromptHistory` with `PromptHistoryPage`. The public wrappers become `search_prompt_page` (used by `client_backend.rs`) and `session_prompts(session_id)`, which returns this session's prompts newest first with repeated text removed, for Up/Ctrl-P walking. That listing needs no query, so it reads `prompt_history` directly through its `prompt_history_session_recent` index. The web route in `run.rs` calls `search_prompt_page` with `after = None` and `limit = MAX_HISTORY_MATCHES`, and sets `truncated = page.next.is_some()`.

Milestone 3 changes the terminal. In `mj-client/src/session.rs`, `SessionHandleBackend::search_prompts` takes `after: Option<HistoryCursor>` and `limit: usize` and returns `PromptHistoryPage`. A new method `session_prompts()` returns `Vec<PromptHistoryEntry>`. Update `client_backend.rs` and every test fake. In `mj-chat/src/chat/history.rs`, `HistorySearch` gains `next: Option<HistoryCursor>` and `fetching: bool`. `matches` keeps every match fetched so far, with no repeated text, so Ctrl-S can step back. After any change to the query or scope, the search resets `matches`, sets `next` to the start cursor (tier 0, `i64::MAX`) and requests a page. After a page arrives, or after the user steps forward, the search requests another page when fewer than eight matches lie ahead of the selection, `next` is set, and no request is in flight. A page is applied only if its generation matches the current one. Its entries are appended after dropping any text already in `matches`. If that leaves fewer than eight ahead, the next request follows immediately. Rename `HistorySearchRequest` fields to carry `after` and `limit`. Remove `local_history_search_results` and the `local_history` field. Tests that used it now feed pages through `apply_history_search_results`. The footer `history_search_footer` shows `n/m` when `next` is `None` and `n/m+` while more may exist, and "type 3+ characters" when the query is shorter than three characters. The prefetch in `active.rs` calls `session_prompts()` instead of `search_prompts(..., Session, "")`.

Milestone 4 updates `docs/src/content/docs/terminal-surface.mdx` (the 3-character minimum), runs the full validation, commits, requests review and pushes.


## Concrete Steps

Run all commands from the repository root. Run every `cargo test` outside the restricted sandbox, as AGENTS.md requires, because the suite uses loopback sockets.

    cargo test -p brokk-mj-controller --lib -- prompt history schema
    cargo test -p brokk-mj-chat --lib history
    TZ=UTC cargo test -p brokk-mj-chat -p brokk-mj-client -p brokk-mj-controller -p brokk-mj-core
    cargo clippy --all-targets -- -D warnings

`golden_transcript_navigation` in `mj-chat` renders local times, so it fails unless `TZ=UTC` is set. That failure is unrelated to this plan.


## Validation and Acceptance

Milestone 1 is accepted when a migration test shows two things. Rows already in `prompt_history` before revision 114 are found through `prompt_history_text` afterwards. A row inserted, updated or deleted with plain SQL afterwards, as an older writer would do, is reflected in the index.

Milestone 2 is accepted when a store test with prompts in three sessions across two projects shows four things. `All` returns this session's matches, then the project's, then other projects', each newest first, with repeated text shown once at the smallest scope. Paging with `limit = 2` through the whole result, following `next`, returns the same sequence as one large page, with no gaps and no repeats. A two-character query returns an empty page. A query containing a double quote does not fail.

Milestone 3 is accepted when chat tests show three things. Pressing Ctrl-R and typing a query queues a request with the start cursor and a limit. Applying a page with fewer than eight matches ahead queues the next request from the returned cursor. Stepping with Ctrl-R through the queue triggers refills, while a page from an older generation is ignored. Up and Ctrl-P walk only the session's prompts, which the existing test `move_history_walks_only_this_sessions_prompts` already covers.

End to end: build, start a test instance (`--instance <name>`, never the default instance), send three prompts in two sessions of one project, then open a session and press Ctrl-R and type "pro". The footer shows the matches, smallest scope first.


## Idempotence and Recovery

The migration runs in one transaction and uses `IF NOT EXISTS`, so a failed run leaves revision 113 intact and can be retried. Rebuilding the index is safe to repeat: `INSERT INTO prompt_history_text(prompt_history_text) VALUES ('rebuild')` regenerates it from `prompt_history`. Never run migration tests against the default instance's store.


## Artifacts and Notes

Prototype query plan for one page of this session's matches:

    SELECT p.history_id, p.text FROM prompt_history_text f
    JOIN prompt_history p ON p.history_id = f.rowid
    WHERE prompt_history_text MATCH ?1 AND f.rowid < ?2 AND p.session_id = ?3
    ORDER BY f.rowid DESC LIMIT 8
    plan: SCAN f VIRTUAL TABLE INDEX 192:M1< ; SEARCH p USING INTEGER PRIMARY KEY (rowid=?)


## Interfaces and Dependencies

No new dependencies. SQLite comes from `rusqlite` 0.40 with the `bundled` feature, which builds SQLite 3.53.2 with FTS5.

In `mj-core/src/storage.rs`:

    pub struct HistoryCursor { pub tier: u8, pub before: i64 }
    pub struct PromptHistoryPage { pub entries: Vec<PromptHistoryEntry>, pub next: Option<HistoryCursor> }

In `mj-controller/src/database/prompts.rs`:

    pub fn search_prompt_page(session_id: &str, bundle_id: &str, scope: HistoryScope, query: &str,
        after: Option<HistoryCursor>, limit: usize) -> Result<PromptHistoryPage>
    pub fn session_prompts(session_id: &str) -> Result<Vec<PromptHistoryEntry>>

In `mj-client/src/session.rs`, on `SessionHandleBackend` and `SessionHandle`:

    fn search_prompts(&self, bundle_id: String, scope: HistoryScope, query: String,
        after: Option<HistoryCursor>, limit: usize) -> BoxFuture<'_, Result<PromptHistoryPage>>;
    fn session_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptHistoryEntry>>>;


Revision note (2026-11-06): the user decided that prompt history keeps only top-level sessions that still exist, so revision 114 now deletes child and destroyed-session rows and adds three triggers that keep that rule. The build stays synchronous because the remaining text is small. Progress, Decision Log and Surprises were updated.

Revision note (2026-11-06): upstream took revisions 111 to 113 while this plan was in review, so the merge renumbered this plan's migration to 114, its fixture helper to `restore_pre114_prompt_history`, and its migration test to rewind to 113.
