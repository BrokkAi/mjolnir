# Make session search truthful and top-level-only

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

The Sessions dialog must distinguish a pending search from a background index sync. Answers remain visible while fresh results are fetched. Mjolnir supplies only top-level sessions to SessionWiki, including supported native/importable harness sessions. Existing identified child index copies, including archived transcripts, are deleted. The user explicitly chose that deletion and prohibited SessionWiki source or dependency changes.

## Progress

- [x] Inspected the search UI, background tasks, adapters, native classification, and destroy/archive paths.
- [x] File the two deferred issues in the Mjolnir tracker.
- [x] Implement truthful search state and one task per query; focused dialog tests passed. Final task and overlay regressions run with the full suite.
- [x] Implement top-level discovery, transactional index cleanup, and root-only destroy indexing; 47 focused SessionWiki tests passed. The new isolated archive regression runs with the full suite.
- [x] Complete dev-profile Cargo validation and isolated instance validation: reuse the passing full suite, corrected controller tests, and affected merge-site checks as directed by the user.
- [x] Commit only task-owned files on master: implementation commit `f8069058`. Publish the completed commits to origin/master as the final delivery action authorized by the user.

## Surprises & Discoveries

The dialog defaults to an indexing status before receiving a daemon answer. Automatic refreshes immediately set the pending flag, including their sleep, which can leave an empty tab saying Searching indefinitely. Success and error replies currently take different identity-handling paths.

Codex's SessionWiki parser labels every session as main, whereas Mjolnir's import metadata already rejects structured sub-agent sources. Claude's upstream discovery walks child directories. Omitting previously indexed children from discovery is insufficient: SessionWiki archives vanished inputs instead of removing them. Destroy indexing currently enumerates a whole child tree.

## Decision Log

The implementation remains entirely in Mjolnir. Filter adapter inputs rather than changing SessionWiki or adding a dependency release. Use the canonical `State::is_subagent_session` classification for owned sessions and child ownership reported by the existing native metadata parsers. Import eligibility is broader than child ownership: noninteractive parents still belong in the index. Delete positively identified child copies, including durable archive and curation rows; never treat unreadable metadata as evidence for deletion. Standalone SessionWiki and conversational/tool-hit optimization remain deferred issues.

One background task owns the sequential searches for one query. Dialog state owns displayed results and accepts only the current monotonically allocated request ID, including errors. Repeated answers never create new request IDs or reset the answered state.

## Outcomes & Retrospective

Both implementation milestones and validation are complete, and implementation commit `f8069058` is ready for publication to origin/master. An earlier full suite passed; after the final ownership correction, all 2111 controller tests and 703 worker unit tests passed. After integrating upstream UI changes, the combined chat, TUI, CLI, daemon startup, and PTY tests passed, as did all-target Clippy and formatting. The user explicitly requested reusing that full-suite result and validating affected merge sites rather than repeating the full suite. The redundant rerun was stopped. Search now retains answered results during sync, while adapter discovery and cleanup exclude positively identified children and preserve parents. SessionWiki, dependency pins, and the default daemon/store remain untouched. Deferred transcript filtering is tracked in issue #1210.

## Context and Orientation

`mj-tui/src/resume.rs` owns the Sessions dialog and its render state. `mj-cli/src/dashboard/io/spawn.rs` runs asynchronous I/O and sends updates handled in `mj-cli/src/dashboard/io.rs`. `mj-controller/src/sessionwiki.rs` supplies adapters to the linked library, coordinates syncs, and preserves transcripts before destroy. `mj-controller/src/import/` already classifies native session eligibility. A top-level session is a session started directly rather than a Mjolnir-managed or harness-owned child.

## Plan of Work

First replace assumed status and independent pending state with pending/answered/failed states. Move refreshing into the supervised query task and remove refresh counters/schedules. Give requests IDs allocated outside dialog lifetime. A new query or closed dialog cancels its UI task; accepted daemon work may finish normally. The task repeats after five seconds only while its answer says indexing or syncing and stops on errors or version mismatch.

Next filter Mjolnir adapter store keys and metadata using one ownership snapshot. Replace before-destroy tree indexing with root-only indexing and let eligible child cleanup proceed without index copies. Wrap native Codex/Claude discovery, retaining library parsers and reconciliation scopes, and use explicit child ownership from the metadata parsers shared with import discovery. Prepare explicit exclusions and delete their index copies before normal reconciliation. Keep other native harnesses using their established top-level source enumeration. Search callers always exclude child sessions, while preserving their existing tool-text matching distinction.

Finally run focused behavior regressions, required full validation, and an isolated daemon instance. Review, commit on the current branch, then push to origin/master. Do not modify SessionWiki, Cargo dependencies, build storage, or live instance data.

## Milestones

The first milestone replaces refresh-driven pending flags with one query task and an explicit pending, answered, or failed state. The dialog's focused tests verify that repeated identical answers on an empty tab show index syncing rather than searching, stale errors do not change the current query, and replies from a closed dialog cannot enter a reopened one. Task tests drive build and sync answers to a settled index and check cancellation during debounce.

The second milestone selects parents at the Mjolnir and supported native adapter boundaries. Focused SessionWiki tests run real library sync against an isolated index, retaining native parents while never offering Codex structured child sources or Claude child transcripts. Transactional pruning deletes old child cache rows, full-text entries, archive copies, and curation together. The daemon regression uses a real checkpoint and isolated controller store to prove child cleanup proceeds without an index copy while parent history survives.

The final milestone validates the integrated change on current master, including the full dev-profile Cargo suite and Clippy. The remote advanced twelve commits during implementation; the current branch was fast-forwarded from 89543c3a to bd0c941d and the task's changes reapplied. One nearby conflict was resolved by preserving the upstream daemon-creation helper and the new query task. The initial full-suite compilation caught missing arguments in the new overlay test fixture; these were corrected before the integrated run.

## Concrete Steps

From `/home/jonathan/Projects/mjolnir`, use normal mbx-managed Cargo. Run `cargo fmt --all -- --check`, focused `cargo test -p brokk-mj-tui resume::tests` and `cargo test -p brokk-mj-controller sessionwiki`, then `cargo test` and `cargo clippy --all-targets -- -D warnings`. All tests execute with elevated sandbox permissions; existing tests retain their isolated directories. Every manual binary invocation uses `--instance search-top-level`. Stage explicitly named task files, commit, and run `git push origin HEAD:master`.

## Validation and Acceptance

Regression tests exercise initial unknown status, genuine index building, empty-tab syncing, unchanged results across refreshes, finished refreshes, matching and stale failures, and close/reopen identities. Adapter tests cover live and checkpointed child exclusion; Codex/Claude child discovery; legacy native parents; cleanup of live, archived, and misclassified children; and successful parent preservation through destroy/archive. No source or top-level index entry is deleted by child cleanup. Required Cargo checks and isolated daemon tests must pass before delivery.

## Idempotence and Recovery

Index cleanup uses transactions and positive classification; repeating it is harmless. Failed discovery is reported and marks listings incomplete so reconciliation does not archive missing parents. Tests never use the live database. Keep unrelated concurrent working-tree changes out of commits. Do not switch branches or rebase if the remote advances; report any non-fast-forward push honestly.

## Artifacts and Notes

The deferred issues are https://github.com/BrokkAi/mjolnir/issues/1210 (whole-transcript conversational filtering) and https://github.com/BrokkAi/mjolnir/issues/1211 (standalone SessionWiki top-level sync option). Focused TUI validation passed: 72 passed, 0 failed, 1 ignored timing measurement. Controller and task regressions are being validated.

Plan revision: implementation uses a file metadata cache for native eligibility, so refreshes parse only changed sources. Discovery and explicit cleanup share one classification snapshot. Existing unrecognized native adapters retain library behavior, with identified sub rows removed after sync as well as before it; search excludes those rows throughout.

## Interfaces and Dependencies

Daemon wire types, store schema, and the SessionWiki pin remain unchanged. Internal TUI request state gains explicit pending/answered/failed variants. Native adapter wrappers implement the existing `sessionwiki::adapters::Adapter` contract and delegate transcript parsing. Index cleanup operates on existing SessionWiki tables, including the external-content FTS delete protocol, rather than changing schema.

Plan revision: existing-row classification and cleanup now occur in the same SQLite transaction, so a concurrent writer cannot change a row between selection and deletion. Mjolnir sources sync before preparing native listings, preserving prompt indexing of newly closed sessions even during a large native scan. The archive acceptance test now uses a real checkpoint instead of a synthetic index row, which exercises the normal indexing boundary.

Validation revision: the first integrated full run finished with 2106 controller tests passed, 9 ignored, and two expected corrections remaining. The archive fixture incorrectly marked a non-streamed user message with a latest-content ordinal; it now uses None and the focused isolated archive regression passes. The pre-existing parent-suspend regression expected the deleted child's SessionWiki archive; it now asserts the child is absent, matching the requested behavior. Clippy passed again after both corrections. A long-running historical-schema upgrade test was inspected while validation was quiet and subsequently completed successfully; no test-process or live-daemon changes were needed. The corrected full suite is in progress.

Final lifetime revision: the same nested-dialog lookup now covers preview selection, requests, briefing replies, and hit replies. This prevents Help or a destroy confirmation from accepting a search answer while discarding the preview that answer needs. The overlay regression asserts that the briefing is requested and cached while Help covers the dialog, then remains available when the dialog returns. All-target Clippy and rustfmt checks pass on this final Rust source. An integrated full suite passed; final-source validation is running after the ownership-snapshot adjustment.

Integration revision (2026-10-01): master was fast-forwarded again from bd0c941d to 2e34ef4e as four concurrent upstream commits landed. The search cancellation guard remains after dashboard actions in the new paced loop, so it still cancels queries immediately on close or replacement. Two superseded full-suite jobs and their test-only process groups were stopped before the final integrated run; no other agents or live sessions were stopped. One integrated full suite subsequently passed. Cleanup and owned-adapter enumeration now use MjolnirAdapter::from_state for one fixed snapshot, because owned indexing runs before the native scan; both decisions therefore consult the same child classification. All-target Clippy passed in 3m08s, and the full-suite rebuild finished in 52.10s.

Parent preservation revision: review found that import eligibility rejects Codex exec and Claude SDK parents as well as children. The shared native summaries now report positive child ownership separately; index discovery uses that ownership while the import picker keeps its existing policy. The real-adapter regression verifies those noninteractive parents are indexed and survive cleanup. The corrected full suite has passed all 2111 controller tests, and all-target Clippy passed in 58.55s.

Final integration revision: two concurrent UI commits advanced master to 61030348. The task-owned files were temporarily stashed, master fast-forwarded, and the changes reapplied without conflicts; only the two new DashboardState fields overlap the upstream edits. The corrected full-suite job continues for unchanged controller/worker code. Chat, TUI, CLI, and their isolated integrations are being revalidated together on the updated branch, along with all-target Clippy. The superseded full-suite job was stopped by its verified test-only process group; no live processes were touched.

Validation orchestration correction: the older full-suite job passed 2111 controller and 703 worker unit tests, but its daemon startup test compared the old compiled build ID with the executable replaced by the final UI rebuild, causing one assertion failure. The final integrated daemon startup suite passed all 12 tests. A full suite is rerunning against the final consistent build; no source change was needed. All-target Clippy passed on the integrated branch in 1m17s.

Final validation decision (2026-10-01): the user requested reusing the already passing full suite and checking affected merge sites instead of another full run. The combined `cargo test -p brokk-mj-chat -p brokk-mj-tui -p brokk-mjolnir` passed, including all 12 daemon startup and 12 PTY tests, and integrated all-target Clippy and rustfmt passed. The redundant full-suite rerun was stopped by its verified process group. No required affected-site checks remain.

Completion: implementation was committed on master as `f8069058`, staging only the 17 task-owned files. This plan records the finished implementation and validation; publishing these commits to origin/master is the authorized final delivery step. No further implementation work remains.

Publication integration: the first push was rejected because four more commits reached origin/master. Merge `a71e66d4` preserves them without rebasing or replacing either history. The merge was clean. Its overlapping files add an independent profile-capabilities feed and hydration actions, retain the search task's cancellation and unified reply handling, and change a daemon-test helper's visibility. Affected-site checks passed: 47 controller indexing tests, 4 CLI wiki tests, 73 Sessions dialog tests (one existing benchmark ignored), rustfmt, and all-target Clippy. The earlier full-suite result is reused as requested. No code fixes were required after the merge; this documentation update records the passing results before publication.
