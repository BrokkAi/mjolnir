# Keep native sessions live in SessionWiki

This ExecPlan is a living document. Maintain it according to `.agents/PLANS.md`.

## Purpose / Big Picture

Native Codex and Claude sessions stored in additional profile homes must remain live in the shared SessionWiki index while their source files exist. A standalone SessionWiki sync must not mistake those files for deletions, and a later Mjolnir sync must repair an already mistaken archive flag. A user can verify this by searching for a distinctive word and seeing the native session as live.

## Progress

- [x] (2026-09-24 00:30Z) Reproduced the false archive and identified both reconciliation defects.
- [x] (2026-09-24 00:51Z) Changed the SessionWiki fork, added behavior tests, and committed 0.30.2 as `effa77d` on its current `publish` branch. Tests, clippy, and publication dry run pass.
- [x] (2026-09-24 00:51Z) Removed the slow fallback scan from filled search results and made name-only scans metadata-only. Focused search test passes.
- [x] (2026-09-24 00:51Z) Kept cross-tab hit counts fresh while Live is selected and searched the index while typing there. Focused resume tests pass.
- [x] (2026-09-24 00:51Z) Shared scrollbar pointer handling between the chat transcript and resume preview; added click-and-drag and hit arrows. Focused chat and resume tests pass.
- [x] (2026-09-24 00:58Z) Completed full Mjolnir tests and clippy; the 4,000-native-session dialog benchmark measured 5.9 ms per rebuild.
- [x] (2026-09-24) Pushed Mjolnir's validated UI/search commit `180c4733` to `origin/master`, the fork fix `effa77d` to `origin/publish`, and source commit `6f84b8b` to upstream SessionWiki PR #29. The PR port passed tests, clippy, and formatting.
- [ ] Publish the approved fork package. The first `cargo publish --locked` attempt packaged and verified 0.30.2 but crates.io rejected upload with 403 authentication failed. Retry after credentials or ownership are repaired, then update Mjolnir's dependency and install the corrected binary.
- [ ] Repair the affected live index only after the corrected code is installed.

## Surprises & Discoveries

- The standalone `Codex::default()` adapter returns no reconciliation scope and scans only `~/.codex`. It therefore archives rows under `~/.codex2`, `~/.codex3`, and `~/.codex4` that Mjolnir added. The live index has 1,237, 826, and 158 such false archived rows, respectively, while the source directories still exist.
- `sync_with` compares only size and modification time to decide whether to index a discovered file. An archived row with unchanged source file is never made live again. The same defect affects shared stores.
- Mjolnir's `archive_after_days` selects only `Controller.state.sessions`, so it cannot directly delete native Codex files. The shared index's archived flag comes from SessionWiki reconciliation.
- A read-only `sessionwiki search --no-sync restic` returns the main session as result 2. The earlier phrase explanation was wrong; the false archived flag is a separate defect that misclassifies this result in the resume UI.
- The dialog rebuild path zeroed all index hit counts whenever Live was selected. Clicking Import rebuilt them from the already received answer, explaining the sudden `0` to `7` change. Live typing also deferred the index search until tab switch.
- A `restic` search ranked 48 main sessions and two subagents in its first 50 candidates. Mjolnir then ran its fallback 2,000-row `recent` scan even though nearly all useful results were already present. On the live 13 GB index, that query took 2.846 seconds warm; selecting the same 2,000 metadata rows took 0.045 seconds. Cache misses and concurrent indexing amplify the difference.
- The first fork repair would have reparsed more than 2,000 falsely archived Codex sessions. The final 0.30.2 implementation instead clears the archive flag and durable copy when the present source's change token is unchanged; it reparses changed sources. Behavior tests assert that the unchanged path preserves message IDs and the changed path captures new text.

## Decision Log

- Decision: Scope stock Codex and Claude reconciliation to the exact root each scans, and always re-index a discovered row that is marked archived. Rationale: this fixes both the false deletion and its persistence without special cases in Mjolnir. Date/Author: 2026-09-24, Codex.
- Decision: Search up to the existing 200-candidate cap before filtering subagents, and skip title matching after filling the caller's fixed result limit. Use a metadata-only scan when title matching is still needed. Rationale: preserve title discovery while avoiding the expensive 2,000-row preview, summary, and tag fetch on common terms. Date/Author: 2026-09-24, Codex.
- Decision: Use one scrollbar pointer component for chat and resume, with view-specific translation from an offset to content position. Rationale: both controls need the same track click, thumb hold, clamping, and release behavior; chat uses estimated row positions while resume has exact wrapped rows. Date/Author: 2026-09-24, Codex.

## Outcomes & Retrospective

The prepared fork package fixes false native archival and makes the repair of unchanged sources cheap. Mjolnir now returns search results without the unnecessary full recent-session scan on common terms, searches history while the Live tab is selected, and updates cross-tab counts immediately when the answer arrives. The preview supports shared scrollbar dragging and clickable hit arrows. The fork and Mjolnir UI/search commits have been pushed, and upstream PR #29 includes the source fix. The crate has not been published or installed because crates.io rejected authentication. Existing binaries can still mark alternate-profile rows archived. The live index has not been mutated by this work.

Validation: fork `cargo test`, clippy, and `cargo publish --dry-run` passed. Mjolnir full `cargo test`, clippy, focused controller and resume tests, and the dialog benchmark passed. A 2,000-row title fallback took 2.846 seconds on the live index when it fetched previews, summaries, and tags, while the metadata-only selection took 0.045 seconds; the filled `restic` result path now skips it entirely. This establishes the avoidable query cost but does not claim a measured end-to-end 30-second reproduction because the old live daemon was no longer running when inspected.

## Context and Orientation

Mjolnir links the published `brokk-sessionwiki` crate and passes one native adapter per enabled harness profile in `mj-controller/src/sessionwiki.rs`. The sibling checkout `../sessionwiki` is the fork source. Its `src/adapters/codex.rs` and `src/adapters/claude_code.rs` choose the file roots; `src/index.rs` reconciles missing rows and parses discovered ones. An archived row is retained in `files` and `archive`, but appears as deleted in the UI. The standalone binary's stock adapters currently claim all rows of the tool instead of only those in their root.

## Plan of Work

In the fork, make the default Codex and Claude adapters return a scope built from `root()`, including the trailing path separator. Extend the indexed-file metadata fetched by `sync_with` to include whether the row is archived; treat an archived row as pending when its source file or shared-store key exists even if its size and modification token are unchanged. Existing `index_one` already clears both the flag and durable archive copy. Add tests proving that a stock adapter leaves another profile root untouched and that a subsequent sync restores an erroneously archived row. Update outdated comments and tests.

Keep Mjolnir's own age-based archive job scoped to managed sessions. Add a focused regression test only if existing tests do not already demonstrate that boundary. Validate the fork with `cargo test` and clippy, then Mjolnir with dev-profile `cargo test` and `cargo clippy --all-targets -- -D warnings`, running tests outside the restricted sandbox as required. A portable Mjolnir dependency requires a published fork version; prepare the release and update Mjolnir only when the version exists. Do not repoint Cargo output or the live index during validation.

In `mj-controller/src/sessionwiki.rs`, avoid the title fallback when ranked results fill the fixed result limit; request enough candidates to replace excluded subagents. Make the fallback read only `files` metadata. In `mj-tui/src/resume.rs`, update hit counts even on Live and request index search during Live typing; render two hit buttons on the transcript panel border. Put pointer gesture state and mapping in `mj-chat/src/components/scrollbar.rs`, and use it in both `mj-chat/src/chat/transcript.rs` and the resume preview. Keep selection routing from treating a held thumb as a text selection.

## Concrete Steps

From `../sessionwiki`, edit the two adapters and `src/index.rs`, run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. From this repository, run the normal required checks on the dev profile. The behavior tests should pass with source files still on disk and the index row's `archived_at` cleared.

## Validation and Acceptance

The fork tests must show that a stock `~/.codex` scan leaves a `.codex2` row live, and that rescanning `.codex2` repairs a previously archived row with unchanged file metadata. The analogous Claude scope must be tested. On the user's index, corrected code should turn the native session IDs `01a044c6-4a2e-78a2-9b87-18863a9785d0` and `01a044f3-0aca-7842-87e5-0003ac49b2b5` back to live without removing their source files.

The Mjolnir search test must keep subagents out of the result while filling its result limit with main sessions. A Live-tab search must update the other tabs' counts before a tab switch. The shared scrollbar tests must prove a track click, thumb drag past both ends, and release. A resume preview test must prove the visible thumb moves the wrapped transcript and that clicking the previous/next buttons changes `N/M` and preview position.

## Idempotence and Recovery

The corrected sync can run repeatedly. An interrupted sync leaves archived copies in the durable archive table; a later run retries restoration. Do not mutate the live index until the corrected binary is installed, because the old binary would recreate the false archive state.

## Artifacts and Notes

The affected sessions are under `/home/jonathan/.codex2/sessions/2026/08/27/`. The shared index is `/home/jonathan/.local/share/sessionwiki/index.db` and should be queried with normal SQLite locking while a writer runs; an immutable read of an active database can report malformed data.

## Interfaces and Dependencies

Keep the `Adapter::reconcile_scope() -> Option<String>` interface. `src/adapters/mod.rs::root_scope` already builds the required prefix. `src/index.rs::sync_with` owns both file and shared-store incremental decisions; `index_one` owns restoration. Mjolnir's root `Cargo.toml` currently names `brokk-sessionwiki` 0.30.1.

Plan created on 2026-09-24 to fix false archival of native sessions and the failure to recover them from the shared index.
Plan expanded on 2026-09-24 after the user reproduced stale Live-tab counts and asked for search latency and transcript controls; it now covers those directly observed failures.
