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
- [x] (2026-09-17 01:55Z) Milestone 4: fork tag `v0.28.0-mj.2` (commit `ef19d4c`) with quiet non-terminal progress and the tool-name fallback, plus the Mjolnir dependency bump; commits `90c91ca0`, `3b49cc4d` (API, client, restore), `857d10b3` (Archived tab), `fa082d58` (two fixes the live check found). Live check: the three routes answered, a hand-deleted checkpoint became an archived row, and a restored session's first reply quoted the earlier work.
- [x] (2026-09-17 02:20Z) Milestone 5: archive job driven by `archive_after_days`; commit `a9f0e473`. Live check: a two-day-old stopped session left `mj sessions`, its `.hel.zip` went, `mj/<id>` stayed in the repository, and `sessionwiki list --tool mjolnir` showed the row `[archived]`.
- [x] (2026-09-17 02:15Z) Milestone 6: web viewer parity, commit 77f7cc80.
- [x] (2026-09-17 03:30Z) Milestone 7: documentation, commits `b345b976` (docs site) and `fd16c8d9` (`.agents/docs/sessionwiki-fork.md`); follow-up ticket https://github.com/BrokkAi/mjolnir/issues/1066. The docs build the Docs workflow runs passes, including its internal link check.
- [x] (2026-09-17 04:30Z) Milestone 8: `brokk-sessionwiki` 0.28.0 published to crates.io from the fork's `publish` branch (commit 33f67f6, tag `brokk-v0.28.0`) at the user's request; Mjolnir's workspace dependency switched to the registry crate and re-locked; install command in `sessions.md` updated.
- [x] (2026-09-17 04:00Z) Milestone 9: SessionWiki always on, daemon side; commits `4ce2f9a7` (the milestone) and `94c06ef0` (re-index on rename, which the live check found). Live check: a running session found by a word from its reply, a renamed session found by a title in none of its messages, a fresh `MJ_DATA_DIR` creating its own index while the user's own index stayed absent, and a copy at `user_version = 7` answering `version_mismatch` with the file untouched.
- [x] (2026-09-17 05:40Z) Milestone 10: one search path in both UIs; commits
  `a496c318` (terminal), `235ff2b0` (web), `0a4c9e87` (docs), and `b2dbce5a`
  (a preview that promised a transcript nobody had asked for, which the live
  check found). Live checks: a word that appears only inside a transcript
  narrowed the Mjolnir tab to that session with its snippet and a phrase that
  appears in no message found it by its title; a background answer redrew the
  dialog six seconds after the last keystroke with no input at all; a fresh
  `MJ_DATA_DIR` held the search box closed at "Indexing…" for nine and a half
  minutes while tabs and rows kept working, then opened it in the same dialog;
  the browser showed both, and restored an archived session end to end; and the
  restore wizard reads " Restore · <step> " with the archived session's title.

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
- Observation: `acp::tests::bridge_exit_during_initialize_returns_an_actionable_error` belongs to the same timing-sensitive family. It failed once during milestone 10 in a whole-workspace run that the machine was also running two indexing daemons under, and passed alone and in a repeat of the whole suite.
  Evidence: `mj-worker/src/acp/tests.rs` line 4095 in the failing run; the crate's suite took 73s in that run.
- Observation: The fork's `main` already carried upstream through 0.28.0 when milestone 2 started, so the tag was cut as `v0.28.0-mj.1` rather than the 0.26.0 name in the first draft. The tag was created once and never moved after Mjolnir referenced it.
  Evidence: `git log --oneline mj-embed` shows the 0.27.0 and 0.28.0 release merges below commit 529b8fe.
- Observation: The adapter's view of controller state goes stale during a long sync, and nothing later corrects it. A sync pass walks every other tool's store before it reaches the Mjolnir adapter; on this machine the first pass took about twenty minutes (746 Claude Code and 1639 Codex sessions). Sessions closed during that walk were indexed with no record at all: empty project, no start or end time, and the title guessed from the first prompt. Because the checkpoint's modification time is the change token and the checkpoint never changes again, no later sync re-parses them.
  Evidence: the first live check produced rows with `project = ''` and `started = NULL` while `mj.sqlite3` held the correct `project_directory` and `updated_at`. Resolved by `MjolnirAdapter::reloading`, which re-reads controller state in `store()`, the moment the indexer reaches this adapter. `from_state` is kept as the fixed-snapshot form the unit test uses. Re-verified after touching the archives to change their tokens: project, title, start, and end are all populated.
- Observation: `sessionwiki brief` prints `Tool: unknown` for a Mjolnir session. `index::session_from_index` resolves the display tool through `adapters::by_name`, and the Mjolnir adapter is not in the standalone binary's registry.
  Evidence: `src/index.rs` lines 1957-1959 in the fork. Harmless for `list --tool mjolnir` and for search, but milestone 4's preview would show it too. If that matters, a fourth fork change should pass the row's own tool string through.
- Observation: SessionWiki's per-session progress output is very loud in the daemon log: one `[tool] indexing n/total` line per session, with no newline, for every tool on every full sync. The plan accepted stderr progress; in practice it fills `daemon.log` with a single multi-megabyte line.
  Evidence: `$MJ_DATA_DIR/daemon.log` after one full sync of this machine's corpus. Fixed in the fork at `v0.28.0-mj.2`: sync asks `std::io::stderr().is_terminal()` once and skips the progress writes when it is false. After milestone 4's live sync the isolated `daemon.log` held four lines, none of them progress.
- Observation: The restored session's title became the whole hand-off. The harness names a session from its first message, and that message carries the installed hidden context, so `acp_session_title` (which `display_title` prefers) was the summary plus the user's prompt.
  Evidence: `mj sessions` after the first successful restore. Fixed by pinning `session_title_override` to the archived session's title, which the harness cannot overwrite.
- Observation: A session on its way up passes through `Disconnected` while its worker reconnects, and the first hand-off wait treated that as a terminal state and gave up.
  Evidence: `could not install the restored archive's hand-off ... is Disconnected before its hand-off` in the isolated daemon log. The wait now accepts the same states `still_starting` accepts in `server_runtime/api.rs`.
- Observation: `index::session_from_index` resolving the tool name through the standalone registry (recorded above) also made `brief` useless for Mjolnir rows in the preview pane, so it was fixed in the fork rather than left as a cosmetic note.
  Evidence: the milestone 4 brief transcript below now reads `Tool: mjolnir`.
- Observation: The `zcode` profile named in the acceptance steps does not exist in the current `config.toml`; it survives only in a backup written by an older build, whose `kind = "zcode"` is not a `HarnessKind` this build accepts.
  Evidence: `~/.config/mjolnir/config.toml.bak-20260915T121000` line 52 against `HarnessKind` in `mj-core/src/config.rs`. The live check used `codex` and `deepseek` instead; `codex` was out of quota, so `deepseek` supplied the session with a tool call.
- Observation: No fourth fork change was needed for the archive job's index check. `index::resolve` returns `SessionRow`s that already carry `path`, `msg_count`, and `archived`, so the job asks for the session id and keeps the row whose `path` is this instance's own key. `indexed_message_count` was not added and no `v0.28.0-mj.3` tag exists.
  Evidence: `RESOLVE_COLS` and `map_resolve_row` in the fork's `src/index.rs` lines 1433-1453.
- Observation: Adding `LifecycleKind::ArchiveStopped` costs one enum variant and one arm. The client-facing `RuntimeLifecycleKind` in `mj-client/src/daemon.rs` is a serialized protocol type, so the new kind maps onto `DestroyStopped` there instead of growing a variant an older client could not read; surfaces show archiving as a destroy, which is what it looks like to a user.
  Evidence: `impl From<LifecycleKind> for RuntimeLifecycleKind` in `mj-controller/src/daemon.rs`.
- Observation: A `Stopped` session's managed worktree really is only a branch by then, so keeping the branch is exactly "do not call `cleanup_managed_worktree`". `retire_managed_worktree` removed the checkout, pruned the metadata, and removed the empty `.mj/worktrees` directories at close; `cleanup_managed_worktree` repeats that work (all no-ops) and adds the `git branch -D`.
  Evidence: `mj-controller/src/controller/worktree.rs` lines 1662-1745, and the live check: after `mj close` the project held no `.mj/worktrees` directory and still held `mj/79a8...`.
- Observation: The attachment store leaves a marker directory behind. After archiving, `$MJ_DATA_DIR/sessions/<id>/` still exists holding empty `attachments.deleted` and `attachments.lock` files. This is `AttachmentStore::remove_session_data`'s own tombstone and destroy behaves the same way; the checkpoint archive itself is gone, and the adapter keys off `*.hel.zip`, so the row still archives.
  Evidence: the milestone 5 live check below.
- Observation: SessionWiki drops and rebuilds its entire cache when the SQLite `user_version` differs from the library's `SCHEMA_VERSION` constant.
  Evidence: `src/index.rs` lines 375-395. A `sessionwiki` binary at a different schema version than the library Mjolnir links would force a full re-index on every alternation. Documented in milestone 7.
- Observation: The viewer's assets are compiled into the daemon binary, so editing `viewer.js` alone changes nothing at runtime.
  Evidence: `const VIEWER_JS: &str = include_str!("web/viewer.js");` at `mj-controller/src/server.rs` line 3597. Every viewer change in this milestone needed `cargo build` and a daemon restart before the browser saw it.
- Observation: The resume list's local filter tested only title, project, location, target, and profile, so a live session that a wiki search matched only by its transcript text was filtered out of the list it belonged in.
  Evidence: a search for a phrase that appears in a transcript returned the row from `/wiki/search` while the list showed nothing. The filter now also keeps a session whose id has a wiki snippet.
- Observation: The archived detail card overflowed a 420 px phone viewport. Long briefing lines and long titles have no spaces to break on.
  Evidence: horizontal page scroll at 420 px in the Playwright run. Fixed with `overflow-wrap` on the card body and a cap on the heading.
- Observation: A rename does not move any change token the index had. A checkpoint's modification time never changes again, and a running session's activity watermark only moves when something is said, so a renamed session kept its old title in the index indefinitely.
  Evidence: the milestone 9 live check renamed a running session and the next sync re-indexed nothing. Fixed in `MjolnirAdapter::store`: the token is the later of the conversation's own token and the record's `updated_at`, which a rename does move (`Controller::rename_session` writes both).
- Observation: A read-only open of the index creates an `index.db-shm` beside it. The schema-version guard opens the file read-only to read `PRAGMA user_version`, and SQLite creates the shared-memory file for a WAL database even for a reader.
  Evidence: the milestone 9 version-mismatch check: `index.db` kept its modification time and size exactly, and a zero-length `-wal` and a 32 KB `-shm` appeared beside it. This is ordinary SQLite behaviour and does not touch the index's contents.
- Observation: A first index build on this machine's corpus took about seven minutes with a fresh `MJ_DATA_DIR`, not the twenty the earlier note recorded, and searches answer from the part already built while it runs.
  Evidence: the milestone 9 daemon reached `status.state = "ready"` seven minutes after starting, and an empty query returned Claude Code rows two minutes in.
- Observation: The viewer selects the first workspace in the snapshot, which in the isolated environment was not the default workspace holding the sessions.
  Evidence: the archived section was empty until the workspace was switched by hand. Not changed; recorded because it makes a live check look like a failure.
- Observation: The preview pane promised a transcript nobody was loading. It is
  drawn whenever the selected row has an indexed session, but the briefing was
  only ever requested when the selection moved, and typing does not move it: the
  answer puts a different row under the same selection. The pane read "Loading
  the archived transcript…" indefinitely.
  Evidence: the first milestone 10 terminal run, where a search for `marmalade`
  left the pane loading for the whole capture. Fixed in `b2dbce5a`: the dialog
  hands back the briefing it still needs after an answer is folded in, through
  the background task the selection already used.
- Observation: The resume dialog opens with the focus on the tab strip when the
  Mjolnir tab has no rows, because an empty list is declared disabled. A bare
  Down then does nothing; Tab moves into the list and Down walks it from there.
  Evidence: the milestone 10 fresh-index check, where three Down keys left the
  footer on the first row until a Tab was sent first. This predates this
  milestone and is not changed here; it is recorded because it makes a live
  check look like a broken list.
- Observation: The first index build took nine and a half minutes with a fresh
  `MJ_DATA_DIR` on this machine, against the seven minutes milestone 9 recorded.
  The corpus keeps growing, so the documentation says "several minutes" rather
  than a number.
  Evidence: `Search: Indexing…` from t+36s to t+550s and an open box at t+570s
  in the milestone 10 capture.
- Observation: Deleting a session's checkpoint by hand is no longer enough to
  make its index row archived. `hel_session_id` is set whenever a record with
  that id exists, and the surfaces treat a row with one as a live session to
  resume rather than an archive to restore. The record has to go too.
  Evidence: the milestone 10 web check, where Destroy in the resume dialog was
  what moved the row to the Archived section about a minute later.

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

- Decision: `WikiRow` and `WikiRestoreRequest` live in `mj-client/src/daemon.rs`, not in `mj-controller/src/sessionwiki.rs` as the Interfaces section first said.
  Rationale: `mj-controller` depends on `mj-client`, not the other way round, and both the daemon protocol and the HTTP contract need the type. `mj-controller` builds the rows and the HTTP routes serialize them.
  Date/Author: 2026-09-17, Fable.
- Decision: `WikiRow` carries a `native_id` the daemon extracts with `sessionwiki::index::native_id_of`.
  Rationale: The terminal has to match a row against an import-scan row by native id, and it has no SessionWiki dependency. SessionWiki's own JSON contract publishes the same field for the same reason, and never the stored path.
  Date/Author: 2026-09-17, Fable.
- Decision: The terminal reaches search, brief, and restore over the daemon's own TCP protocol, not over the HTTP API. The HTTP routes exist for the documented API and, in milestone 6, the web viewer.
  Rationale: Every other thing the dashboard asks the daemon goes through `DaemonAction`. Restore answers with a `RegisteredSession`, so the dashboard follows a restored session through creation with the code it already has for a new one.
  Date/Author: 2026-09-17, Fable.
- Decision: A restored session opens the repository above the archived session's managed worktree when the caller names no project directory.
  Rationale: A Mjolnir session runs in `<repo>/.mj/worktrees/<id>`, and that worktree is gone by the time the session is archived. The repository above it is what the user still has. A caller that wants somewhere else passes `project_directory`.
  Date/Author: 2026-09-17, Fable.
- Decision: An archived session with no user message cannot be restored; the request fails with a plain message.
  Rationale: Compaction builds turns and attaches assistant and tool items to the open turn, so a transcript with no prompt has nothing to hand over. Leading assistant and tool messages are dropped for the same reason.
  Date/Author: 2026-09-17, Fable.
- Decision: The restored session's hand-off is the ordinary compaction output prefixed with `ARCHIVE_HANDOFF_PREAMBLE`, rather than a second rendering path inside `compaction.rs`.
  Rationale: `is_synthetic_handoff` matches on the opening sentence, so a prefix is enough for a later compaction to recognize it, and the compaction pipeline stays one path.
  Date/Author: 2026-09-17, Fable.
- Decision: The search debounce lives in the spawned task, not in a timer on the dashboard's event loop.
  Rationale: Each keystroke starts a task that sleeps 250 ms and gives up if a later keystroke has replaced it; the dialog drops answers naming an older request. No new tick had to be added to the select loop, and the event loop never waits.
  Date/Author: 2026-09-17, Fable.

- Decision: The archive pass skips a parent while any sub-agent child of its own is not being archived in the same pass, which is stricter than the plan's "no non-Stopped child".
  Rationale: Archiving a parent force-destroys its children first, branch and all. A child that is stopped but not yet old enough would be destroyed without ever being archived. Waiting a pass costs nothing; the parent goes as soon as its children qualify. Children are ordered before parents so the pass archives them itself.
  Date/Author: 2026-09-17, Fable with Opus.
- Decision: `LifecycleKind::ArchiveStopped` is internal to the daemon and reported to clients as `DestroyStopped`.
  Rationale: `RuntimeLifecycleKind` is serialized to every client, and a new variant would break older readers for no gain. The daemon still distinguishes the two internally, which is what the lifecycle log and the branch disposition need.
  Date/Author: 2026-09-17, Fable with Opus.
- Decision: When `archive_after_days` is set, the hourly tick runs the archive job in place of its plain full sync, in a background task that never overlaps with itself.
  Rationale: The job begins with `sync_now(true)`, so a separate full sync on the same tick would only duplicate it. A pass over a large corpus can outlast the tick, and a second concurrent pass would only meet a busy index, so a tick that finds one running logs at debug and waits.
  Date/Author: 2026-09-17, Fable with Opus.

- Decision: A 404 from the wiki routes means "not available" and the viewer shows nothing; only a 409 shows the line saying SessionWiki is disabled.
  Rationale: A daemon that predates these routes answers 404, and an older daemon is not a disabled feature. 409 is the only answer that means the user turned it off.
  Date/Author: 2026-09-17, Fable with Opus.
- Decision: The archived detail is its own route, `#workspace/{id}/resume/archive/{wikiId}`.
  Rationale: The viewer routes on the hash, so an archived briefing needs a hash of its own to be linkable and to survive Back. It sits under the resume route because that is where the row is.
  Date/Author: 2026-09-17, Fable with Opus.
- Decision: The web restore sends `workspace_id`, `profile_id`, and `target_id` only; there is no project-directory control in the browser yet.
  Rationale: The viewer already has profile and target selectors, and the daemon defaults the project directory to the repository above the archived worktree. A directory picker is a larger piece of UI; it is recorded as an open item.
  Date/Author: 2026-09-17, Fable with Opus.
- Decision: SessionWiki is always on. The `[sessionwiki] enabled` key is removed; `archive_after_days` stays opt-in because it deletes data.
  Rationale: The user sees no reason to make indexing optional, and a review found no downside that a switch fixes better than a direct guard. The three hazards are each guarded directly (milestone 9): test daemons writing to the real index, an index at another schema version, and first-build cost.
  Date/Author: 2026-09-17, Jonathan Ellis with Fable.
- Decision: There is one search path. The Resume search box queries the index through the daemon and nothing else; the local row filter is removed. The box is disabled and reads "Indexing…" until this index has completed one full sync. Routine top-up syncs do not disable it.
  Rationale: The user does not want two search paths to maintain. Disabling only for the first build avoids the box disabling itself on every open, since opening the dialog triggers a top-up when the last sync is over 60 seconds old.
  Date/Author: 2026-09-17, Jonathan Ellis with Fable.
- Decision: Live sessions are indexed from the daemon's stored transcript, not only checkpointed ones. The daemon's search also matches title and project, which SessionWiki's own search does not (it matches message text only, `src/index.rs` `search` and `search_like`).
  Rationale: With the local filter gone, a running session that has never been checkpointed, or a session known by a title that never appears in its messages, would otherwise be unfindable.
  Date/Author: 2026-09-17, Fable, accepted by Jonathan Ellis.

- Decision: A process may only touch the index once startup has chosen where the index belongs, which `mj_core::config::apply_instance_flag` records. Everything else indexes nothing unless it names an index with `SESSIONWIKI_DATA`.
  Rationale: The plan's rule covers every real binary, because they all pass through that startup step and inherit the variable into the daemon child. It does not cover a unit test that builds a daemon runtime directly against the real environment, and several do. A flag set by that one startup step is a test for "this is a real Mjolnir process that settled its own data directory", which is exactly the condition, rather than a `cfg!(test)` guess.
  Date/Author: 2026-09-17, Fable.
- Decision: Title and project matching scans the 2000 most recent indexed sessions through `index::recent` rather than adding SQL of its own.
  Rationale: Neither column is indexed, so any form of this is a scan. Going through the public function keeps one reader of SessionWiki's row shape, and the cap keeps a keystroke's cost bounded. Full-text hits still come first and a name match never displaces one.
  Date/Author: 2026-09-17, Fable.
- Decision: A session's change token is the later of its conversation's token and its record's `updated_at`.
  Rationale: Renaming a session changes nothing the conversation token can see, so without this the index keeps the old title. `updated_at` moves on a rename and on every other record change, and taking the later of the two never loses a conversation change.
  Date/Author: 2026-09-17, Fable.
- Decision: `WikiRow` and the search rows are unchanged; the status travels beside them as `{ rows, status }` on both the HTTP route and the daemon protocol, and `DaemonReply::WikiRows` now carries that pair.
  Rationale: Milestone 10 needs the state and the rows together from one request, and a client that only wants rows reads one field. The protocol version already gates older clients.
  Date/Author: 2026-09-17, Fable.

- Decision: A tab's rows under a query are ordered by the index's ranking, not
  by recency. Every row carries the position of the hit it came from, and a
  non-empty query sorts by it.
  Rationale: The index ranks by relevance and the merge sorts by activity. Two
  orders cannot both be right, and the person typed a query to see the best
  match, not the newest session.
  Date/Author: 2026-09-17, Fable.
- Decision: A 404 from the wiki routes now also closes the search box, with the
  placeholder "Search is unavailable". The rest of the 404 rule is unchanged:
  no archived section, no line of explanation, and no further asking.
  Rationale: 404 means a daemon with no wiki routes. With the local filter gone
  there is nothing for the box to do, and a box that silently matches nothing is
  worse than one that says it cannot search. The rows themselves are Mjolnir's
  own and are still listed.
  Date/Author: 2026-09-17, Fable.
- Decision: Both clients start in the `indexing` state rather than assuming the
  index is ready, and the web page's first ask has no typing debounce.
  Rationale: Nothing has answered yet, so offering a search that cannot run is a
  claim neither client can back. Sending the first ask immediately keeps the
  closed box to one round trip when the index is in fact ready.
  Date/Author: 2026-09-17, Fable.
- Decision: The repeat that a still-building or still-syncing index asks for is
  issued by the same background task the first ask uses, driven from the answer
  rather than from a timer on the event loop.
  Rationale: It reuses the request counter that already drops stale answers, and
  it keeps the rule that nothing waits on the event or render loop. A dialog
  that is closed has no answers to act on, so the polling stops by itself.
  Date/Author: 2026-09-17, Fable.

## Outcomes & Retrospective

The Purpose asked for three things, and all three work.

Every checkpointed Mjolnir session is in the user's SessionWiki index under the
tool name `mjolnir`. `sessionwiki list --tool mjolnir` and `sessionwiki brief
<id> --tools` show them with their real project, title, times, and user, tool,
and assistant lines, which the milestone 3 and 4 transcripts record. One index
holds them beside Claude Code, Codex, and the rest, and several Mjolnir
instances share the one tool name without archiving each other's rows.

Indexing is always on, and Resume searches that index and nothing else. There
is one search path in both surfaces: an empty box lists what each tab always
listed, and a query lists only what the index returned, in the order the index
ranked it, with the text it matched on. A running session is indexed from the
daemon's own transcript and a stopped one from its checkpoint, and the daemon's
search matches title and project as well as message text, so a session is
findable by something said inside it, by a name that appears in none of its
messages, and before it has ever been closed. The live checks found the same
session all three ways.

Because there is nothing else to search with, the box is closed while the index
cannot answer: it reads "Indexing…" through the first build, and names a
version mismatch when the index file belongs to another SessionWiki schema
version. The tabs and the rows keep working throughout, and the box opens by
itself when the build ends, in the dialog or page that is already open. While a
build or a top-up sync is running, both clients ask again on their own — every
five seconds while building, and up to ten times every two seconds while
topping up — from the same background task the first ask used, so nothing waits
on an event or render loop.

Resume still previews the selected session and still lists archived ones on a
third tab, and Enter or **Restore** starts a new session carrying a compacted
summary of the old conversation; both the terminal and the web check ended with
a new session answering a question about work it never did, from the hand-off
alone. The wizard that does it says " Restore · <step> " and names the archived
session it is restoring.

`archive_after_days` deletes Mjolnir's copy of an old stopped session once
SessionWiki has it, and keeps the branch. The milestone 5 check saw the record,
the checkpoint, and the attachments go, the `mj/<id>` branch stay, and the
index row flip to `[archived]`.

The success test in the Purpose — close a session, find it from a terminal,
search for a word from it, watch the row move to Archived, restore it — was run
end to end and is in Artifacts.

What it cost: one dependency bump (rusqlite 0.37 to 0.40, mechanical once
`fallible_uint` was enabled), a fork of SessionWiki with three small library
changes and a pull request offering them upstream, and about 800 lines of new
Mjolnir code in an adapter, an indexer, three routes, a restore path, an
archive job, and two user interfaces. The local row filter both surfaces used
to have is gone, which is the one thing this work removed.

Release note, for whoever writes the next release's notes. This repository has
no unreleased-notes file — `.agents/docs/release-v*-notes.md` exists only for
versions that have shipped, and there is no CHANGELOG — so the line lives here:

> Session search is now always on and comes from the SessionWiki index, so the
> first run after upgrading builds that index before Resume can be searched.
> The search box reads "Indexing…" until it finishes, which takes several
> minutes on a large corpus of other tools' sessions; the session list and the
> tabs work throughout, and the box opens by itself when the build ends.

Open items:

- A prompt sent in the instant between a restored session becoming ready and its
  hand-off being installed goes out without the context. The window is short and
  the user has to be quick, but nothing closes it; the hand-off is installed
  after provisioning, not before the session accepts input.
- `Lost` and `Error` sessions that have a checkpoint are indexed but never
  archived, because the archive job only considers `Stopped`. Whether they
  should be archivable is still open.
- The web restore has no project-directory control, so the browser always takes
  the default (the repository above the archived worktree). The terminal has the
  same gap; only the API accepts `project_directory`.
- The viewer's node unit tests and its Playwright suite are run by
  `.github/workflows/reliability.yml`, not by `ci.yml`. A viewer regression is
  therefore not caught by an ordinary pull-request run.
- Milestone 8 is done: the dependency is the registry crate `brokk-sessionwiki`
  0.28.0, so this branch can be released.
- A search with the index unavailable — a 404, which means a daemon older than
  these routes — leaves the browser with no search at all. The box says so and
  the rows are still listed, but there is no fallback, by design: two search
  paths were the thing this milestone removed.
- The resume dialog opens with the focus on the tab strip when its first tab is
  empty, so Down does nothing until Tab moves into the list. It predates this
  work and is not obviously wrong, but it surprised the live check.
- The follow-up for a SessionWiki skill inside client sessions is
  https://github.com/BrokkAi/mjolnir/issues/1066. The hard part there is
  reaching the binary and the index from container and SSH targets, and keeping
  the binary's schema version in step with the linked library.

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

A git dependency cannot be published to crates.io, and Mjolnir publishes every workspace crate, so the dependency has to come from the registry before the next release. The fork's `publish` branch is ready: commit `33f67f6`, pushed to `origin`, renaming the package to `brokk-sessionwiki` at version 0.28.0 with explicit `[lib] name = "sessionwiki"` and `[[bin]] name = "sessionwiki"` targets, so the library still compiles as `sessionwiki` and the installed command is still `sessionwiki`. `cargo build`, `cargo test`, and `cargo package --allow-dirty --list` pass on that branch. Nothing has been published; that needs crates.io credentials this work does not have.

The exact commands, for the user to run:

    # in the fork, /home/jonathan/Projects/sessionwiki
    git checkout publish
    cargo publish

    # then in Mjolnir, in the root Cargo.toml [workspace.dependencies],
    # replace
    #   sessionwiki = { git = "https://github.com/jbellis/sessionwiki.git", tag = "v0.28.0-mj.2" }
    # with
    #   sessionwiki = { package = "brokk-sessionwiki", version = "0.28.0" }
    cargo update -p brokk-sessionwiki
    cargo build

Schema-version caveat, and the reason this is worth doing beyond the publishing rule: once the crate is on crates.io, a binary from `cargo install brokk-sessionwiki` is built from the same source as the library Mjolnir links, so it matches the index schema version by construction. The `cargo install --git ... --tag v0.28.0-mj.2` command in `docs/src/content/docs/sessions.md` should change to `cargo install brokk-sessionwiki` in the same change that moves the dependency, so the documented tool and the linked library cannot drift.

### Milestone 9: always on, daemon side

Config. Remove `enabled` from `SessionWikiConfig` as a setting. Version 10 configs containing `enabled` exist on master builds, and the struct uses `deny_unknown_fields`, so keep the key readable and ignored exactly the way `Config.show_stopped_sessions` is kept (`#[serde(default, skip_serializing)]`, documented as deprecated), and follow the existing version pattern if a bump is required. Remove it from the setup schema, labels, and help. Remove every `config.sessionwiki.enabled` check in `mj-controller` and `mj-tui`, the 409 responses, the "Enable SessionWiki in Setup" messages, and the tests that assert them.

Index isolation. SessionWiki chooses its index directory from the `SESSIONWIKI_DATA` environment variable, else the platform data directory. With indexing always on, any daemon started with an overridden data directory (every test and e2e daemon) would otherwise walk the user's real stores and write the user's real index. Rule: at process start, where `apply_instance_flag` in `mj-core/src/config.rs` already records `MJ_INSTANCE` with `std::env::set_var`, if the `MJ_DATA_DIR` override is set and `SESSIONWIKI_DATA` is not, set `SESSIONWIKI_DATA` to `<MJ_DATA_DIR>/sessionwiki`. Named instances without a data-dir override keep sharing the real index. The daemon child process must inherit it. Audit every test and script that starts a daemon (`tests/e2e/`, `mj-cli/tests/`, `scripts/test-*.sh`, and unit tests that reach `sync_blocking` or `query_rows`) and prove none can reach the real index; a test that cannot be isolated this way must set `SESSIONWIKI_DATA` itself.

Schema guard. Before `sessionwiki::index::open()` or `open_readonly()`, if the index file exists, open it read-only with `rusqlite` directly and read `PRAGMA user_version`. If it is non-zero and differs from `sessionwiki::index::SCHEMA_VERSION`, do not open it through SessionWiki at all (its `open` would drop the cache): skip syncs and answer searches with no rows and status `version_mismatch`. Log once at warn level.

First-build marker. `<data_dir>/sessionwiki-built` holds the schema version and is written after the first successful full sync. The index counts as built when the marker's version matches and the index file exists. Status is `indexing` until then.

Status. The search response becomes `{ rows, status }` with `status = { state: "ready" | "indexing" | "version_mismatch", topping_up: bool }`. `topping_up` is true while any sync is running. The daemon-protocol reply carries the same. Make the startup sync an explicit `request_sync(true)` when the runtime starts, with a comment, instead of relying on the interval's immediate first tick.

Title and project matching. For a non-empty query, union the full-text hits with index rows whose title or project contains the query case-insensitively (from `recent` with a generous limit, or a direct read-only query over `files`), full-text hits first, de-duplicated by session id, capped at `limit`.

Live sessions. `MjolnirAdapter::store()` also lists every record that is not `Stopped` and has a stored transcript, under the same key `<sessions_dir>/<id>`, with token = `last_activity_at_ms / 1000`. `parse_key` for such a session reads the materialized transcript from the daemon's database (the reader the reviewer uses for `MaterializedSession`; see `mj-controller/src/database.rs` and `seed_from_session` in `review_host.rs`) and maps it exactly as the checkpoint path does. A `Stopped` session keeps using its checkpoint. Because a live key is listed, reconciliation never archives a running session; destroying it removes the key and the row becomes archived at the next sync. Unit-test both sources and the transition.

Acceptance, isolated environment: start a session, send a prompt, do not close it; `GET /wiki/search?q=<word from the reply>` returns it with `hel_session_id` set within one top-up. Rename a session to a title that appears nowhere in its messages and find it by that title. Start a daemon with a fresh `MJ_DATA_DIR` and no `SESSIONWIKI_DATA` and confirm the index is created under it and the real index's modification time does not change. Set `PRAGMA user_version = 7` on a copy of an index and confirm `version_mismatch` and that the file is not modified.

### Milestone 10: one search path in both UIs

Terminal (`mj-tui/src/resume.rs`, drivers in `mj-cli/src/dashboard.rs`) and web (`mj-controller/src/web/viewer.js`): remove the local text filter. With an empty query the tabs list what they list today. With a query, each tab shows only rows the index returned: the Mjolnir tab rows whose session id is a hit's `hel_session_id`, the Import tab rows whose harness and native id match a hit, the Archived tab the archived hits; each with its snippet. Opening the dialog issues the empty query, which also fetches `status`. While `status.state` is `indexing` the search box is disabled with the placeholder "Indexing…" and the client re-queries every five seconds until it is `ready`; tabs and row navigation keep working. `version_mismatch` disables the box with "SessionWiki index is at a different version". While `topping_up` is true the client re-issues the current query every two seconds, at most ten times, so results refresh when the top-up ends. The archive-restore wizard title reads " Restore · <step> " and shows the archived session's title. Verify in a live terminal against the isolated daemon that search and preview results arriving in the background redraw the dialog under master's redraw-once-per-wakeup model. Update the unit, node, and Playwright tests; remove tests of the local filter. Update `docs/src/content/docs/sessions.md`, `configuration.md`, and `web-viewer.md`, and add a release note line about the first index build on upgrade.

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

Milestone 5, run on 2026-09-17. A second isolated environment, because
another agent was working in the milestone 3/4 one at the same time. Its
SessionWiki index starts as a copy of the first, so the cold walk of every
tool's store did not have to run again, and its web viewer binds a different
port:

    S=/tmp/claude-1000/-home-jonathan-Projects-hel3/68c2d9bc-4218-4756-9eaa-7d0de76bb3b9/scratchpad
    M=$S/m5
    mkdir -p $M/mj-config $M/mj-data $M/wiki $M/project
    cp $S/mj-config/config.toml $M/mj-config/config.toml
    cp -a $S/wiki/. $M/wiki/
    sed -i 's/127.0.0.1:37650/127.0.0.1:37651/' $M/mj-config/config.toml
    # and under [sessionwiki]: archive_after_days = 1
    (cd $M/project && git init -q && echo hello > README.md && git add README.md &&
     git -c user.email=a@b -c user.name=t commit -qm init)
    export MJ_CONFIG_DIR=$M/mj-config MJ_DATA_DIR=$M/mj-data SESSIONWIKI_DATA=$M/wiki
    ./target/debug/mj new --profile deepseek --target localhost \
      --project-directory $M/project --workspace-id default \
      "Read README.md and tell me the single word it contains."
    ./target/debug/mj wait --session 79a818190728012e01a28d4c1ccea905
    ./target/debug/mj close --session 79a818190728012e01a28d4c1ccea905

Ageing the record, with the daemon stopped so it does not write over the row:

    ./target/debug/mj daemon stop
    sqlite3 $M/mj-data/mj.sqlite3 \
      "UPDATE sessions SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-2 days')
       WHERE session_id='79a818190728012e01a28d4c1ccea905';"

Triggering the tick. `MJ_PRUNE_TICK_SECONDS` shortens the hourly interval and
is read once per daemon; in practice the first tick fires as the daemon comes
up, so restarting it is enough:

    MJ_PRUNE_TICK_SECONDS=30 ./target/debug/mj sessions   # restarts the daemon
    ./target/debug/mj daemon stop                          # afterwards

Milestone 9, run on 2026-09-17. A third isolated environment, with no
`SESSIONWIKI_DATA` at all, which is what proves the startup rule:

    S=/tmp/claude-1000/-home-jonathan-Projects-hel3/68c2d9bc-4218-4756-9eaa-7d0de76bb3b9/scratchpad
    M=$S/m9
    mkdir -p $M/mj-config $M/project
    cp $S/mj-config/config.toml $M/mj-config/config.toml
    sed -i 's/127.0.0.1:37650/127.0.0.1:37652/' $M/mj-config/config.toml
    # the copied config still says `[sessionwiki] enabled = true`, which this
    # build reads and ignores
    (cd $M/project && git init -q && echo hello > README.md && git add README.md &&
     git -c user.email=a@b -c user.name=t commit -qm init)
    export MJ_CONFIG_DIR=$M/mj-config MJ_DATA_DIR=$M/mj-data
    unset SESSIONWIKI_DATA
    ./target/debug/mj sessions            # starts the isolated daemon
    ./target/debug/mj new --profile deepseek --target localhost \
      --project-directory $M/project --workspace-id default \
      "Reply with exactly this sentence and nothing else: the marmalade telescope hums."
    ./target/debug/mj wait --session 323a5869f20026c5ccd77a5d2ba1f7a1
    # the session is NOT closed: it is indexed from the stored transcript

Renaming it, with the daemon stopped so it does not write over the row:

    ./target/debug/mj daemon stop
    sqlite3 $M/mj-data/mj.sqlite3 \
      "UPDATE sessions SET session_title_override='zephyr custard vault',
       updated_at=strftime('%Y-%m-%dT%H:%M:%SZ','now')
       WHERE session_id='323a5869f20026c5ccd77a5d2ba1f7a1';"
    ./target/debug/mj sessions            # restarts the daemon

The schema-version check, on a copy of that environment's own index:

    ./target/debug/mj daemon stop
    sqlite3 $M/mj-data/sessionwiki/index.db "PRAGMA wal_checkpoint(TRUNCATE);"
    mkdir -p $M/wiki-v7 && cp $M/mj-data/sessionwiki/index.db $M/wiki-v7/index.db
    sqlite3 $M/wiki-v7/index.db "PRAGMA user_version = 7;"
    export SESSIONWIKI_DATA=$M/wiki-v7
    ./target/debug/mj sessions            # restarts the daemon against the copy

Milestone 10, run on 2026-09-17. Three environments, all under the scratchpad
`S`, and all started with `MJ_CONFIG_DIR` and `MJ_DATA_DIR` only, so each
daemon indexes into its own `$MJ_DATA_DIR/sessionwiki`.

- `$S/m9`, milestone 9's environment reused: its index was already built
  (`sessionwiki-built` = 8, 2.8 GB), and its one session was closed and then
  destroyed so its row would archive.
- `$S/m10`, a fresh `MJ_DATA_DIR` on port 37653, for the first-build state in
  the terminal.
- `$S/m10b`, another fresh `MJ_DATA_DIR` on port 37654, for the same state in
  the browser.

Each environment's config is a copy of milestone 9's with its own `[phone]`
port. The terminal is driven through a PTY, the way `mj-cli/tests/
termination_pty.rs` drives it, with a small VT100 screen to read the output
back; the browser is driven with the repository's own Playwright install
against the live viewer.

    S=/tmp/claude-1000/-home-jonathan-Projects-hel3/68c2d9bc-4218-4756-9eaa-7d0de76bb3b9/scratchpad

    # the built index, in a terminal
    python3 $S/m10_terminal.py          # search, snippet, title-only phrase
    python3 $S/m10_terminal2.py         # the background redraw, with no keypress

    # the first build, in a terminal, in one dialog that is never reopened
    mkdir -p $S/m10/mj-config && cp $S/m9/mj-config/config.toml $S/m10/mj-config/
    sed -i 's/127.0.0.1:37652/127.0.0.1:37653/' $S/m10/mj-config/config.toml
    python3 $S/m10_indexing.py
    python3 $S/m10_rows2.py m10         # tabs and rows while the box is closed

    # the wizard, and the same two behaviours in a browser
    python3 $S/m10_wizard.py
    MJ_CONFIG_DIR=$S/m9/mj-config MJ_DATA_DIR=$S/m9/mj-data \
      ./target/debug/mj daemon status   # prints the viewer code
    node $S/m10_web.mjs https://<host>:37652 <code> ready "marmalade,xyzzyplugh"
    node $S/m10_web.mjs https://<host>:37652 <code> restore "" restore deepseek localhost
    node $S/m10_web.mjs https://<host>:37654 <code> indexing indexing

The browser specs were also run against the live m9 viewer, which is what the
three viewport tests in `layout.spec.js` need and a fixture cannot give them:

    cd tests/e2e/web
    MJ_BROWSER_SPEC=layout.spec.js \
      MJ_BROWSER_BASE_URL=https://<host>:37652 MJ_BROWSER_CODE=<code> npx playwright test

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

Milestone 4, 2026-09-17. The same isolated environment as milestone 3, reusing
its index so the cold sync did not have to run again:

    S=/tmp/claude-1000/-home-jonathan-Projects-hel3/68c2d9bc-4218-4756-9eaa-7d0de76bb3b9/scratchpad
    export MJ_CONFIG_DIR=$S/mj-config MJ_DATA_DIR=$S/mj-data SESSIONWIKI_DATA=$S/wiki
    ./target/debug/mj sessions          # starts the isolated daemon
    ./target/debug/mj api-info          # base url and token file
    TOKEN=$(cat $S/mj-data/api-token)
    B=https://minasmorgul-wsl.tail5caf7.ts.net:37650/api/v1

The API authenticates with the bearer token in `$MJ_DATA_DIR/api-token`;
`mj api-info` prints both the base URL and the file. The bind address is the
`[phone]` one, so the isolated daemon answers on port 37650.

`GET /wiki/search?limit=3` with no query, abridged to the fields that matter:

    {"rows": [
      {"id": "1e0d61a75458ca85a9f040878e6aee8e", "tool": "mjolnir",
       "title": "project via deepseek", "msgs": 3, "archived": false,
       "preview": "The single word in README.md is: **hello**",
       "native_id": null, "snippet": null,
       "hel_session_id": "1e0d61a75458ca85a9f040878e6aee8e"},
      {"id": "dc393ff29ce9", "tool": "codex",
       "title": "Reply with exactly: pomegranate sentinel", "msgs": 3,
       "archived": false, "native_id": "01a0acbe-d358-7862-ab70-3848daa57bee",
       "hel_session_id": null},
      {"id": "04852244ba07430fabca26288502b642", "tool": "mjolnir",
       "title": "project via codex", "msgs": 2, "archived": false,
       "hel_session_id": "04852244ba07430fabca26288502b642"}]}

`GET /wiki/search?q=pomegranate&limit=3` returns the same rows with the
matching text, for example `"snippet": "…ly: \u0002pomegranate\u0003 sent…"`,
where U+0002 and U+0003 are SessionWiki's own match markers.

`GET /wiki/sessions/1e0d61a75458ca85a9f040878e6aee8e/brief?max_chars=600`:

    # Previous session: project via deepseek

    - Tool: mjolnir | Project: /tmp/.../worktrees/1e0d61a75458ca85a9f040878e6aee8e/ | Date: 2026-09-17 00:23

    **User:**
    Read README.md and tell me the single word it contains.

    > [tool] Read file '/tmp/.../README.md'

    **Assistant:**
    The single word in README.md is: **hello**

`Tool: mjolnir` rather than the `Tool: unknown` of milestone 3, which is the
fork's tool-name fallback working. An unknown id answers 404:

    $ curl -s -w "\n%{http_code}\n" -H "Authorization: Bearer $TOKEN" \
        "$B/wiki/sessions/nosuchid/brief"
    {"error":"no indexed session nosuchid"}
    404

Archiving by hand. The milestone 5 job does not exist yet, so its end state was
produced manually: stop the daemon, delete the checkpoint, and delete the
record the way that job will.

    ./target/debug/mj daemon stop
    rm $S/mj-data/sessions/1e0d61a75458ca85a9f040878e6aee8e-*.hel.zip
    sqlite3 $S/mj-data/mj.sqlite3 \
      "DELETE FROM session_checkpoints WHERE session_id='1e0d61a75458ca85a9f040878e6aee8e';
       DELETE FROM sessions WHERE session_id='1e0d61a75458ca85a9f040878e6aee8e';"
    ./target/debug/mj sessions          # restarts the daemon
    # a search triggers the bounded sync; the row flipped within two minutes
    curl -s -H "Authorization: Bearer $TOKEN" "$B/wiki/search?q=single%20word&limit=5"
    # -> id 1e0d61a7…: "archived": true, "hel_session_id": null

The whole of `daemon.log` for that run was four lines, none of them progress:

    Mjolnir viewer code: 540958
    [aider] the store could not be read in full; skipping deletion reconciliation this run
    archived 1 session(s) the tool removed (1 kept that your tools have deleted)
    Mjolnir: Synced skills for profile deepseek to 1 session(s).

Restore:

    $ curl -s -w "\n%{http_code}\n" -X POST -H "Authorization: Bearer $TOKEN" \
        -H "Content-Type: application/json" \
        -d '{"workspace_id":"default","profile_id":"deepseek","target_id":"localhost"}' \
        "$B/wiki/sessions/1e0d61a75458ca85a9f040878e6aee8e/restore"
    {"session_id":"fdc9fa7a29575b7e52356483467f55ed"}
    201

The daemon log a few seconds later:

    INFO mj_controller::daemon: installed the restored archive's hand-off
      session_id="fdc9fa7a29575b7e52356483467f55ed" bytes=523

The restored session carries the old conversation without any file in its
workspace naming it, and `mj sessions` shows it under the archived session's
own title, `project via deepseek`:

    $ ./target/debug/mj prompt --session fdc9fa7a29575b7e52356483467f55ed --wait \
        "In one sentence: what did we conclude earlier?"
    finished (EndTurn) turn 1 in 9.6s

    We concluded earlier that README.md contains the single word "hello".

An earlier attempt against the same row answered, before the title fix, with
`The single word was **hello**, found in README.md at
scratchpad/project/.mj/worktrees/1e0d61a75458ca85a9f040878e6aee8e/README.md` —
the old worktree path, which only the hand-off could have supplied.

Terminal UI. Driven over a PTY (a small `pty.fork` script; the repository's own
PTY harness is in `mj-cli/tests/termination_pty.rs`): Alt-S opens the dialog,
Right twice reaches the Archived tab. The rendered screen, abridged:

    ╭ × Resume a session ──────────────────────────────────────────────────────╮
    │ Mjolnir   Import   Archived                                              │
    │Search:                                                                   │
    │╭ Archived sessions · newest first ──────────────────────────────────────╮│
    ││  PROFILE      TARGET                  LAST ACTIVE   SESSION            ││
    ││mjolnir      local/1e0d61a75458ca85a…  1 hour ago    project via deepseek│
    │╰────────────────────────────────────────────────────────────────────────╯│
    │╭ Archived transcript ───────────────────────────────────────────────────╮│
    ││# Previous session: project via deepseek                                ││
    ││- Tool: mjolnir | Project: /tmp/.../worktrees/1e0d61a75458ca85a9f040878e…││
    │╰────────────────────────────────────────────────────────────────────────╯│
    │      archived · 3 messages · The single word in README.md is: **hello**  │
    │            Enter restores · ←/→ tabs · / searches · Tab moves            │
    │  Cancel     Restore                                                      │
    ╰──────────────────────────────────────────────────────────────────────────╯

The tab lists the archived session, the preview pane under the list holds the
briefing fetched in the background, the row's details line carries the message
count and the tail of the conversation, and the action button reads Restore.

Workspace validation for these commits: `cargo build`, `cargo test` (3490
passed, 0 failed), `cargo clippy --all-targets -- -D warnings`, `cargo fmt
--check`. The fork's own `cargo test` (127 passing in the library alone) and
`cargo clippy --all-targets -- -D warnings` pass at `v0.28.0-mj.2`.

Milestone 5, 2026-09-17. Before the tick, one stopped session two days old,
its checkpoint on disk and its branch in the repository:

    $ ./target/debug/mj sessions
    ID                                STATE    TITLE
    79a818190728012e01a28d4c1ccea905  stopped  project via deepseek
    $ ls $M/mj-data/sessions
    79a818190728012e01a28d4c1ccea905-51-archive-55287db7464233f8a6f900dc95b76da4.hel.zip
    $ git -C $M/project branch --list
    * master
      mj/79a818190728012e01a28d4c1ccea905

Twenty seconds after the daemon restarted, all four things the milestone asks
for:

    $ ./target/debug/mj sessions
    ID  STATE  TITLE
    $ sqlite3 $M/mj-data/mj.sqlite3 "select count(*) from sessions;"
    0
    $ ls $M/mj-data/sessions
    79a818190728012e01a28d4c1ccea905          # the attachment tombstone only
    $ ls $M/mj-data/sessions/79a818190728012e01a28d4c1ccea905
    attachments.deleted  attachments.lock
    $ git -C $M/project branch --list
    * master
      mj/79a818190728012e01a28d4c1ccea905
    $ SESSIONWIKI_DATA=$M/wiki .../sessionwiki list --tool mjolnir
    ID            TOOL     WHEN     MSGS  PROJECT                  TITLE
    79a818190728… mjolnir  1m ago      3  …/79a818190728012e01a28… project via deepseek  [archived]

The one info line per archived session, from the daemon's own log in
`$MJ_DATA_DIR/logs/` (`daemon.log` carries stderr, not tracing):

    2026-09-17T02:14:45.507854Z  INFO mj_controller::daemon: archived a stopped
      session: SessionWiki keeps the conversation and the repository keeps the
      branch session_id=79a818190728012e01a28d4c1ccea905 older_than_days=1

Workspace validation for this commit: `cargo build`, `cargo test` (3494
passed, 0 failed), `cargo clippy --all-targets -- -D warnings`, `cargo fmt
--check`.

Milestone 6, 2026-09-17. The web viewer, driven in a browser against the
isolated daemon's `[phone]` address. The resume page's **Archived** section
listed the one archived deepseek session. Typing `single word` into the resume
search box called `GET /wiki/search` on the 250 ms debounce and attached the
returned snippets to two live rows as well as listing the archived one.
Selecting the archived row opened its detail card, which held the briefing from
`GET /wiki/sessions/{id}/brief` and a **Restore** button beside the profile and
target selectors. Restore posted to the restore route and the viewer navigated
straight to the new conversation, `1c6028f7c856fd8a4d6e9b4f66d3fb5f`. Asked in
that conversation:

    In one sentence: what did we conclude earlier?

    We concluded that the single word contained in the README.md file is "hello".

The reply can only have come from the installed hand-off; nothing in the new
session's workspace names the earlier conversation.

Validation for this commit: 31 node unit tests in
`tests/e2e/web/viewer.unit.test.mjs` and 20 Playwright tests in
`tests/e2e/web/resume.spec.js` pass; `cargo build` and
`cargo clippy --all-targets -- -D warnings` are clean.

Milestone 9, 2026-09-17. All four acceptance checks, against the isolated
daemon on port 37652.

Its own index, and the user's: the client's environment named `MJ_DATA_DIR`
and no `SESSIONWIKI_DATA`, and the daemon it started had both:

    $ tr '\0' '\n' < /proc/<daemon pid>/environ | grep -E "SESSIONWIKI|MJ_DATA"
    MJ_DATA_DIR=.../scratchpad/m9/mj-data
    SESSIONWIKI_DATA=.../scratchpad/m9/mj-data/sessionwiki
    $ ls .../m9/mj-data/sessionwiki
    index.db  index.db-shm  index.db-wal
    $ ls ~/.local/share/sessionwiki
    ls: cannot access '/home/jonathan/.local/share/sessionwiki': No such file or directory

The user's own index does not exist on this machine at all, before or after
the whole workspace test suite and every check below, which is a stronger
result than an unchanged modification time.

The first build, and the status beside the rows. Seven minutes after the
daemon started:

    {"rows":[],"status":{"state":"indexing","topping_up":true}}     # while building
    {"rows":[...],"status":{"state":"ready","topping_up":false}}    # after
    $ cat $M/mj-data/sessionwiki-built
    8

A running session, never closed, found by a word from its reply:

    $ curl -s -H "Authorization: Bearer $TOKEN" "$B/wiki/search?q=marmalade%20telescope&limit=5"
    {"rows":[{"id":"323a5869f20026c5ccd77a5d2ba1f7a1","tool":"mjolnir",
      "project":".../m9/project/.mj/worktrees/323a5869f20026c5ccd77a5d2ba1f7a1/",
      "title":"project via deepseek","started":"2026-09-17T03:32:50+00:00","msgs":2,
      "archived":false,"native_id":null,
      "snippet":"the \u0002marmalade telesc\u0003…",
      "hel_session_id":"323a5869f20026c5ccd77a5d2ba1f7a1"}],
     "status":{"state":"ready","topping_up":false}}

`hel_session_id` is set, so a surface offers to resume it rather than restore
it. This session has no checkpoint at all; the conversation came from the
daemon's own projection.

The same session renamed to a phrase that appears in none of its messages, and
found by it, with no snippet because it is a name match and not a full-text
one:

    $ curl -s -H "Authorization: Bearer $TOKEN" "$B/wiki/search?q=zephyr%20custard&limit=5"
    {"rows":[{"id":"323a5869f20026c5ccd77a5d2ba1f7a1","tool":"mjolnir",
      "title":"zephyr custard vault","msgs":2,"archived":false,
      "preview":"the marmalade telescope hums.","snippet":null,
      "hel_session_id":"323a5869f20026c5ccd77a5d2ba1f7a1"}],
     "status":{"state":"ready","topping_up":false}}

An index at another schema version, answered without opening it:

    $ curl -s -H "Authorization: Bearer $TOKEN" "$B/wiki/search?q=zephyr%20custard&limit=3"
    {"rows":[],"status":{"state":"version_mismatch","topping_up":false}}
    $ stat -c '%Y %s' $M/wiki-v7/index.db     # before and after the daemon ran
    1789616550 2922754048
    1789616550 2922754048

and one line in the daemon's log:

    WARN mj_controller::sessionwiki: the SessionWiki index was written by
      another version; Mjolnir will not open it, because opening it would
      rebuild it. Install the matching sessionwiki command found=7 expected=8
      path=.../m9/wiki-v7/index.db

Isolation audit for this milestone. Everything that starts a daemon or reaches
the sync or query code, and how each is kept away from the user's index:

- `mj-cli/src/main.rs` is the only caller of `apply_instance_flag`, and every
  `mj` subcommand including `daemon-run` passes through it. With `MJ_DATA_DIR`
  set and `SESSIONWIKI_DATA` unset it points the index at
  `$MJ_DATA_DIR/sessionwiki`; the daemon child inherits the variable (the spawn
  in `mj-cli/src/daemon.rs` clears one unrelated variable and nothing else) and
  resolves it again for itself.
- `tests/e2e/reliability_lab.py`, `tests/e2e/prepare-luna-lab.py` and
  `tests/e2e/web_viewer_recovery.py` set `MJ_CONFIG_DIR` and `MJ_DATA_DIR` and
  run the real binary, so the rule above covers them.
- `mj-cli/tests/common/mod.rs` sets both variables for every `mj-cli`
  integration test, which covers `daemon_startup.rs`, `import_e2e.rs`,
  `instance.rs`, `logging.rs`, `store_divergence.rs`; `termination_pty.rs` sets
  `MJ_DATA_DIR` itself.
- `scripts/test-codex-import-e2e.sh`, `test-kimi-import-e2e.sh`,
  `test-grok-import-e2e.sh` and `test-linux-cli-runtime.sh` export
  `MJ_DATA_DIR` before running the binary.
- `mj-controller`'s own unit tests build a `RuntimeState` directly
  (`test_runtime_state`, `test_runtime_state_with_manager` in `daemon.rs`),
  which spawns a `WikiIndexer`. They never call `apply_instance_flag`, so
  `session_index_is_resolved()` is false and `sync_blocking`, `query_rows`,
  `brief`, `archived_session` and `indexed_with_messages` all refuse before
  opening anything. The re-executed child processes in `checkpoint.rs`,
  `session_manager.rs`, `recovery_scan.rs`, `provisioning.rs`, `worktree.rs`
  and `move_session/tests.rs` set `MJ_DATA_DIR` but are the test binary rather
  than `mj`, so the same refusal covers them.
- The adapter's own unit tests never open an index: they build an
  `MjolnirAdapter` over a `tempfile` directory and call `store` and
  `parse_key` directly.

Proof: `~/.local/share/sessionwiki/index.db` does not exist. It did not exist
before `cargo test`, and it did not exist after two full runs of the workspace
suite and all four live checks.

Workspace validation for these commits: `cargo build`, `cargo test` (3508
passed, 0 failed), `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --check`, and the viewer's 31 node unit tests in
`tests/e2e/web/viewer.unit.test.mjs`.

Milestone 10, 2026-09-17. The four checks.

1. The built index, in a real terminal (the `m9` daemon on port 37652). Typing
a word that appears only inside a transcript narrows the Mjolnir tab to that
session and shows the matching text; the row's own title, project, and profile
carry none of it:

    ╭ × Resume a session ────────────────────────────────────────────────╮
    │ Mjolnir   Import   Archived                                        │
    │Search: marmalade                                                   │
    │╭ Mjolnir sessions · newest first ─────────────────────────────────╮│
    ││  PROFILE     TARGET              LAST ACTIVE     SESSION         ││
    ││deepseek    localhost/project   4 minutes ago   zephyr custard vault
    │╰──────────────────────────────────────────────────────────────────╯│
    │╭ Archived transcript ─────────────────────────────────────────────╮│
    ││# Previous session: zephyr custard vault                          ││
    │╰──────────────────────────────────────────────────────────────────╯│
    │                    project · the marmalade telesc… · 9.8K          │

A phrase that appears in none of its messages, only in the title the session
was renamed to, finds the same row:

    │Search: zephyr custard                                              │
    ││deepseek    localhost/project   4 minutes ago   zephyr custard vault

A query the index does not match empties the tab, which is the whole point of
removing the local filter: the row's text no longer matters.

    │Search: xyzzyplugh                                                  │
    ││No matching sessions                                              ││

The background redraw, under master's redraw-once-per-wakeup model. The last
keystroke is drawn, and then no key is pressed again:

    (0.1s after the last keystroke)  project · 9.8K
    bytes the dialog wrote with no keypress: 116
    (6.0s later, no input)           project · the marmalade telesc… · 9.8K

2. A fresh `MJ_DATA_DIR` (the `m10` daemon on port 37653), in one dialog that
is opened once and never reopened:

    --- t+36s ---                       --- t+570s ---
    │ Mjolnir   Import   Archived        │ Mjolnir   Import   Archived
    │Search: Indexing…                   │Search:
    ...                                  ...
    t+36s … t+550s  Search: Indexing…
    t+570s          Search:
    (then) Search: mjolnir

While the box was closed, typing did nothing, the tab strip still moved
between Mjolnir, Import and Archived, and the Import tab's rows still walked
under the arrow keys:

    tab=Import  ('Search: Index', 'master · 28.5MB · ~/Projects/bifrost')
    down 1:     ('Search: Index', '2771 · 758.6KB · ~/Projects/bifrost2')
    down 2:     ('Search: Index', 'master · 10.0MB · ~/Projects/hel')
    down 3:     ('Search: Index', 'hel2 · 2.1MB · ~/Projects/hel2')
    down 4:     ('Search: Index', 'hel3 · 2.2MB · ~/Projects/hel3')

The first build took nine and a half minutes on this machine's corpus.

3. The browser, headless Chromium through the repository's own Playwright
install. Against the built index:

    --- on arrival
       search box disabled: false
       placeholder: Title, project, or anything said
       rows: [ '323a5869f20026c5ccd77a5d2ba1f7a1' ]
    --- searching "marmalade"
       rows: [ '323a5869f20026c5ccd77a5d2ba1f7a1' ]
       snippets: [ 'the marmalade telesc…' ]
    --- searching "xyzzyplugh"
       rows: []

Against a fresh `MJ_DATA_DIR` (the `m10b` daemon on port 37654), with the page
left open throughout:

    --- on arrival
       search box disabled: true
       placeholder: Indexing…
       box opened after 480s with no reload
    --- after the build
       search box disabled: false
       placeholder: Title, project, or anything said

And the restore flow, end to end. The session was destroyed from the resume
dialog first, which is what moves its index row to archived; about a minute
later the browser listed it:

    --- archived row: zephyr custard vault
    mjolnir · …/m9/project/.mj/worktrees/323a5869f20026c5ccd77a5d2ba1f7a1/ · 2 messages
    the marmalade telescope hums.
       detail hash: #workspace/default/resume/archive/323a5869f20026c5ccd77a5d2ba1f7a1
       brief: Previous session: zephyr custard vault …
       wiki-profile: deepseek
       wiki-target: localhost
       hash after Restore: #conversation/3626d5e2ffc1c99158f84a4a27f506dc

The restored session had the old conversation and nothing else:

    $ mj prompt --session 3626d5e2… --wait \
        "In one short sentence, what did the earlier conversation say the telescope did?"
    finished (EndTurn) turn 1 in 2.4s

    It hummed.

4. The archive-restore wizard, from Enter on that archived row:

    ╭ × Restore · 1/3 profile (cross-harness supported) ──────────────────╮
    │   PROFILE   HARNESS      QUOTA                                      │
    │ … │Restoring: zephyr custard vault                                  │

    ╭ × Restore · 2/3 new target ────────────────────────────────────────╮
    │ … │Restoring: zephyr custard vault                                  │

    ╭ × Restore · 3/3 review ────────────────────────────────────────────╮
    │Profile: claude                                                      │
    │Archived session: zephyr custard vault                               │
    │Target: aws-runson (AWS EC2)                                         │
    │  Cancel     Back     Add directory…     Restore                     │

Workspace validation for these commits: `cargo build`, `cargo test` (3512
passed, 0 failed, on the dev profile outside the sandbox), `cargo clippy
--all-targets -- -D warnings`, `cargo fmt --check`, the viewer's 34 node unit
tests, and Playwright: `resume.spec.js` 21 passed, `new-session.spec.js` 15
passed, and `layout.spec.js` 5 passed against the live isolated viewer, which
its three viewport tests need.

## Interfaces and Dependencies

SessionWiki fork (`../sessionwiki`, branch `mj-embed`, tag `v0.28.0-mj.2`):

    // src/index.rs
    pub fn sync_with(conn: &mut Connection, adapters: &[Box<dyn Adapter>], since: Option<i64>) -> Result<()>;
    // src/adapters/mod.rs, on trait Adapter
    fn reconcile_scope(&self) -> Option<String> { None }
    // src/commands.rs
    pub fn brief_markdown(session: &crate::model::Session, max_chars: usize, include_tools: bool) -> String;

Mjolnir:

    // mj-core/src/config.rs
    pub struct SessionWikiConfig { pub enabled: bool /* deprecated, read and ignored */, pub archive_after_days: Option<u32> }
    pub const SESSION_INDEX_ENV: &str = "SESSIONWIKI_DATA";
    pub fn session_index_is_resolved() -> bool;   // set by apply_instance_flag
    // mj-controller/src/database.rs
    pub fn load_transcribed_session_activity() -> Result<BTreeMap<String, Option<i64>>>;
    // mj-controller/src/sessionwiki.rs
    pub struct MjolnirAdapter;            // implements sessionwiki::adapters::Adapter
    impl MjolnirAdapter { pub fn from_state(state: &mj_core::state::State) -> Self; }
    pub struct WikiIndexer;               // request_sync(full: bool), sync_now(full: bool) -> Result<()>
    pub fn query_rows(query: &str, limit: usize, live: &BTreeSet<String>) -> Result<Vec<WikiRow>>;
    pub fn index_state() -> WikiIndexState;
    impl WikiIndexer { pub fn status(&self) -> WikiStatus; }
    pub fn brief(id: &str, max_chars: usize) -> Result<Option<String>>;
    pub fn archived_session(id: &str) -> Result<Option<ArchivedSession>>;
    // mj-client/src/daemon.rs (the protocol needs it, and mj-controller depends on mj-client)
    pub struct WikiRow { id, tool, project, title, started, msgs, preview, archived, native_id, snippet, hel_session_id }
    pub enum WikiIndexState { Ready, Indexing, VersionMismatch }   // serialized snake_case
    pub struct WikiStatus { state: WikiIndexState, topping_up: bool }
    pub struct WikiSearchPage { rows: Vec<WikiRow>, status: WikiStatus }
    // GET /wiki/search answers a WikiSearchPage; DaemonReply::WikiRows carries one
    pub struct WikiRestoreRequest { wiki_id, workspace_id, profile_id, target_template_id, project_directory, additional_mounts, resource_allocation }
    // mj-controller/src/controller/lifecycle.rs
    pub enum BranchDisposition { Delete, Keep }
    impl Controller { pub fn destroy_session_controlled_with(&mut self, session_id: &str, executor: &impl CommandExecutor, branch: BranchDisposition) -> Result<()>; }
    // mj-controller/src/sessionwiki.rs
    pub fn sessions_ready_to_archive(sessions: &BTreeMap<String, SessionRecord>, subagents: &BTreeMap<String, SubagentRecord>, now: DateTime<Utc>, older_than_days: u32) -> Vec<String>;
    pub fn indexed_with_messages(session_ids: &[String]) -> Result<BTreeSet<String>>;
    // mj-client/src/daemon.rs
    pub async fn wiki_search(&mut self, query: String, limit: usize) -> Result<WikiSearchPage>;
    pub async fn wiki_brief(&mut self, wiki_id: String, max_chars: usize) -> Result<String>;
    pub async fn wiki_restore(&mut self, request: WikiRestoreRequest) -> Result<RegisteredSession>;
    // mj-tui/src/resume.rs
    enum ResumeTab { Hel, Import, Archive }
    enum ResumeRowKey { Hel(String), Native(HarnessKind, String), Archive(String) }
    // mj-tui/src/wizards.rs
    enum ResumeSource { Session, Archive }   // what ResumeWizard is starting

Dependency: `sessionwiki` via git tag during development (milestone 2), via crates.io as `brokk-sessionwiki` before release (milestone 8). `rusqlite` 0.40 with `bundled` everywhere.

Revision note (2026-09-16): first version, written after a review of an earlier draft that found the crates.io publishing conflict, the cross-instance reconciliation flip-flop, and the schema-skew hazard. Those findings are recorded in Surprises & Discoveries and their resolutions in the Decision Log.

Revision note (2026-09-17): milestone 8 completed. The first draft said the publish step needed credentials the automated work lacked; that was an unchecked assumption, and a crates.io credentials file was present. The user authorized the publish explicitly. The version number matches upstream's 0.28.0 deliberately: crate versions are scoped to the crate name, so there is no clash.

Revision note (2026-09-17): added milestones 9 and 10 after the user decided SessionWiki should be always on with one search path. The three hazards of always-on and their direct guards, and the finding that SessionWiki's search does not match titles, are recorded in the Decision Log and in milestone 9.

Revision note (2026-09-17): milestone 10 completed, and with it the plan. The
local row filter is gone from both surfaces; the Resume search box is the
index's answer and closes itself while the index cannot answer. Two things the
plan did not have: the preview pane was promising a transcript nobody had asked
for, because only a moved selection ever requested one, and a 404 from the wiki
routes now closes the search box as well, since with the local filter gone
there is nothing else for it to do.

Revision note (2026-09-17): milestone 9 completed. Two things the plan did not
have: a rename moves no conversation token, so the change token had to take in
the record's own `updated_at`; and the plan's `MJ_DATA_DIR` rule does not reach
a unit test that builds a daemon runtime directly, so the daemon also refuses
an index until process startup has chosen one.
