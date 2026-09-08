# Prevent stale terminal drafts from returning

This ExecPlan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Clearing or submitting a composer must remain effective after switching sessions, refreshing session state, and reopening the terminal. A saved legacy draft must not repeatedly seed the composer after the user has replaced it. Preserve genuine unsent input and independent clients' recovery copies.

## Progress

- [x] (2026-09-08) Traced the stale shared draft and removed the explicitly requested `/model g` session value and 68 exact-match recovery copies from the user's database.
- [x] (2026-09-08) Implemented independent terminal composer state and conditional retirement of inherited shared drafts; both focused database regressions pass.
- [x] (2026-09-08) Ran behavioral regressions, full Rust tests and final affected-suite validation, Clippy, formatting, and the local CLI build; completed final review for the current-branch commit.

## Surprises & Discoveries

`mj-cli/src/dashboard.rs` seeds chat from `SessionRecord.draft_input` and uses that same projected record as a temporary draft cache. Background loads replace that record. `PersistDetachedSessionState` in `mj-cli/src/daemon.rs` only creates recovery records; it never clears the old shared field, even after the user clears the composer. The live database contained one exact `/model g` session draft and 68 detached copies.

## Decision Log

The terminal will own a per-session cache, including explicit empty strings, outside the shared controller snapshot. It captures the outgoing composer before an attachment starts and again before replacement, because the old chat stays editable while preparation runs. Keep the existing same-session handoff behavior.

On detach, save the current nonempty draft for explicit recovery and conditionally clear the inherited shared value in the same SQLite transaction. Compare against the value originally inherited by this terminal and require current workspace membership so a later explicit recovery or workspace move is preserved. Archive the caller's unsent text independently of stale membership, matching the existing independent read-receipt/recovery contract. Never write the current client-local draft into shared session state. This fixes the persistence mismatch without introducing per-keystroke database work or changing the schema. The detach protocol gains the inherited value; bump the CLI daemon protocol so an old daemon cannot silently ignore retirement. Root owns persistence and integration; Luna owns the terminal cache implementation.

## Outcomes & Retrospective

Implementation and validation are complete. Terminal drafts remain independent of shared session refreshes, unchanged inherited values are retired on detach, and all shutdown paths preserve the warm composer. Regression tests verify empty and edited composers, explicit recovery preservation, workspace isolation, and transactional rollback. The requested live data cleanup is complete; repeated queries confirm zero exact-match ghost values in all three draft stores. No other draft text was removed. The updated CLI is available at `target/debug/mj`; already-running terminal processes need reopening to use it.

## Context and Orientation

The daemon is the persistent background CLI process. `mj-cli/src/dashboard/io.rs` sends detach writes to it in supervised tasks. `src/hel_database.rs` serializes writes and stores detached recovery copies separately from the legacy `sessions.draft_input` column. `mj-chat/src/hel_chat/active.rs` builds chats asynchronously and preserves the latest same-session draft during a handoff. Composer text may encode attachments, so the cache must retain its existing encoded string without interpreting or trimming it.

## Plan of Work

First implement the independent terminal cache in `mj-cli/src/dashboard.rs` and optionally a colocated module under `mj-cli/src/dashboard/`. Ensure successful attachment completion reads the latest cache, while failed or cancelled attachment leaves the old chat intact. Test empty and edited values against stale incoming session snapshots.

Then introduce `hel::hel_database::DetachedSessionDraft` containing `text: String` and `inherited_input: Option<String>`. Extend the background detach message to carry it. Add a database operation that inserts nonempty recovery text and compare-and-clears the shared inherited field atomically. Keep ordinary detached-draft saving unchanged for callers that have no inherited composer. Test clear, edited draft, new explicit recovery, workspace membership changes, and transaction rollback on invalid archive source.

Finally validate the integrated code, build the local CLI, review the actual diff, and commit on the current branch. Do not push or stop user sessions.

## Concrete Steps

Run commands in `/home/jonathan/Projects/hel2`. Run every Cargo test outside the restricted sandbox. Use normal repository build storage.

    cargo test -q -- --test-threads=1
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    cargo build -p brokk-mjolnir
    git diff --check

## Validation and Acceptance

A terminal that inherits a draft, clears or consumes it, switches away, receives an older shared snapshot, and switches back must display an empty composer. An edited unsent draft must survive the same sequence. Another terminal must not replace that local state. Detaching with an empty composer must clear the unchanged legacy seed; detaching with edited input must preserve that input as a recovery record and clear the old seed. If another recovery replaced the shared value, the old client must leave the new value intact. A failed archive insertion must roll back retirement. All required Rust suites and Clippy must pass.

## Idempotence and Recovery

Source edits and tests are repeatable. Exact-match live cleanup has already completed in one database transaction. Do not delete other recovery records or unrelated configuration. Conditional shared-draft retirement is safe to repeat; existing recovery saving may create another independent record as before.

## Artifacts and Notes

Live cleanup reported: `Cleared 1 stale session draft, 68 exact-match recovery copies, 0 client drafts.`

## Interfaces and Dependencies

Use existing SQLite transactions, the serialized database writer, supervised dashboard I/O, and encoded chat drafts. No new dependency or schema version is needed. Keep protocol coordination in `mj-cli/src/daemon.rs`. Tests stay in module-level test blocks or the existing `src/hel_database/tests.rs` module.

Initial revision records the observed failure and a cache plus conditional-retirement fix.

Implementation revision: completed the client cache, completion-time capture, lifecycle retirement capture, protocol 13 coordination, and atomic shared-seed retirement. Initial database fixtures had to use the dedicated draft setter because ordinary session persistence deliberately ignores draft fields; correcting the fixtures made the regressions exercise actual nonempty legacy state.

Final-review revision: centralized warm-chat persistence in `begin_shutdown`, covering global quit while another chat is opening, workspace switching, and termination signals. Added workspace membership to shared-seed retirement without discarding a stale client's own recovery text. The first full run passed every library suite (434 chat, 684 controller, 843 core, 353 TUI, 108 worker, 206 CLI; existing ignored tests excluded), but one PTY preview assertion did not observe its contiguous raw-output label. Final affected core/CLI validation is running after these review fixes and includes the PTY suite again.

Final validation revision: `cargo test -q -p brokk-mj-core -p brokk-mjolnir -- --test-threads=1` passed after the review fixes (844 core, 206 CLI, and every CLI integration suite, including all five PTY termination tests). The initial full run covers unchanged chat/controller/TUI/worker code; the terminal preview test passed on rerun without changing its assertion. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check`, and `cargo build -p brokk-mjolnir` passed. No user session or daemon was stopped during this repair.
