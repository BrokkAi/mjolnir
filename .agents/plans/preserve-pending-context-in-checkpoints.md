# Preserve pending session context through checkpoint restore

This ExecPlan is maintained according to `.agents/PLANS.md`. Its living sections record implementation and validation as work proceeds.

## Purpose / Big Picture


A switch between agent harnesses (the programs running models, such as Antigravity and Claude) creates a new native conversation and gives its worker hidden context for the first prompt. Suspending before that prompt currently omits the context from the checkpoint, so another resume can lose it. Preserve the existing pending text in the saved session and restore it through the existing worker delivery mechanism. The visible result is that switching harness, suspending before prompting, and resuming still supplies the exact handoff to the first prompt once.

## Progress


- [x] (2026-11-25 22:37Z) Trace live session and code; verify the observed session retains its pending handoff and identify the distinct checkpoint omission.
- [x] (2026-11-25 22:37Z) Agree with the user on preserving existing worker state without another recovery mechanism.
- [x] (2026-11-25 22:48Z) Extend shared formats and archive capture/restore support.
- [x] (2026-11-25 22:48Z) Extend worker cut reads and restored relay delivery.
- [x] (2026-11-25 22:48Z) Integrate controller capture, restore, and compatibility handling.
- [x] (2026-11-25 22:58Z) Finish mechanical snapshot constructor updates in dependent crates after the first compiler failure.
- [x] (2026-11-25 22:58Z) Run focused regression tests and affected-crate clippy under mbx: 10 tests passed; ten-package all-target clippy passed.
- [x] Review, commit, and push the checkpoint fix and version preparation. Fourth review recorded the transitional queued legacy-close finding as follow-up #1703.
- [ ] Prepare and review the next workspace release, push its version commit and tag, and follow the normal release workflow through publication.
- [x] Push v2.76.0 at ae3157fad and cancel its failed first CI run before publication.
- [x] Repair inherited Rust formatting and viewer VM fixtures; affected viewer files pass 4 and 41 tests.
- [x] (2026-11-25 23:47Z) Move existing CLI logging into mj-client; CLI is 49,626 lines, client 20,362, workspace format check passes, 11 focused logging tests and affected-crate clippy pass.
- [x] (2026-11-26 00:33Z) Repair third-run fixture/Windows failures: all seven controller and ten daemon failures pass individually, saved quota continuation passes, and ten-package clippy passes.
- [ ] Review and push CI repairs, replace the unpublished tag, and monitor publication.
- [x] Commit the initial validated fix and prepare version 2.76.0; request review of the combined deliverable.
- [x] (2026-11-25 23:15Z) Address first review: multi-frame pending-context transfer, old checkpoint-only worker replacement, and avoiding combined context installation frames; seven focused tests and ten-package clippy pass.
- [x] (2026-11-25 23:23:42 UTC) Address second review: unsupported-schema fixture selects 17, and checkpoint-only preparation/readiness release daemon admission using the existing resumable-work boundary; three affected tests and ten-package clippy pass.
- [x] (2026-11-25 23:28:49 UTC) Address third review by deleting the redundant origin-delta pre-stop upgrade; protocol-94 automatic and explicit close regression and ten-package clippy pass.

## Surprises & Discoveries


The worker stores pending text outside the visible transcript. Installing it commits a relay snapshot without advancing the journal event frontier. Therefore archive reuse must compare pending context as content, rather than assuming a matching event frontier proves identical state. Pending context also carries arbitrary hidden notes and clear-context memory, so reconstructing it from visible history would be incomplete.

The initial targeted build stopped before tests because `mj-transcript/src/projection/materialize.rs` needed the new optional field. The same constructor update is needed in importer, wiki, daemon-test, and binary-test fixtures. Those are mechanical format-consumer fixes. The workspace format check also reports pre-existing differences in `mj-core/src/suspend_consent.rs`, `mj-daemon/src/daemon.rs`, and `mj-daemon/src/daemon/tests.rs`; keep unrelated formatting out of this change.

Validation also caught an existing checkpoint test launch fixture missing `parked_move: None`, a new archive assertion using a method as a field, and a new controller fixture installing hidden context after mailbox delivery had already started a prompt. Those were corrected narrowly. The controller regression now installs context through the normal request before delivery; its isolated subprocesses check protocols 95, 94, and pre-mail behavior.

The first complete review found three accepted gaps. Multiple valid installations can accumulate text larger than the relay's documented frame limit, so a single-frame read can prevent every subsequent checkpoint. Combining saved text and a generated handoff can similarly exceed a request frame. Finally, rejecting an older checkpoint-only worker leaves recovery stuck even though the worker can be restarted without running its failed harness. All findings will be fixed rather than overridden.

## Decision Log


- Decision: Save optional text, and restore into the existing pending-context field without an old command attachment.
  Rationale: Command attachment belongs to a live dispatch; the checkpoint boundary must settle or recover that dispatch before detachment. The text is the durable session information. Preserve it even when queued commands are discarded.
  Date/Author: 2026-11-25, parent agent and user.
- Decision: Extend the existing barrier-checked native-use read response with optional pending text; negotiate relay capability 95 and archive schema 16 when text is present.
  Rationale: Both values come from the same worker snapshot at the established cut. The version distinguishes an older worker that cannot report context from a current worker reporting absence, and older archive readers must fail explicitly rather than silently omit new state.
  Date/Author: 2026-11-25, parent agent.
- Decision: Keep one owner, the worker relay. Use existing upgrade/admission paths for older workers, existing archive compatibility handling, and existing prompt installation/delivery. Preserve arbitrary pending text alongside newly generated context on a further harness switch.
  Rationale: This repairs state preservation without introducing a second generator, daemon cache, recovery loop, or runtime flag.
  Date/Author: 2026-11-25, parent agent.
- Decision: Commit the validated fix, prepare the release from the clean checkout, and review both commits together before pushing and tagging.
  Rationale: The user requested both outcomes. The release preparation is a separate normal version commit, and one review can assess the final deliverable without repeating unchanged code review or local CI.
  Date/Author: 2026-11-25, parent agent.
- Decision: Transfer pending context in pages through the existing cut-checked read, and invalidate that cut using its existing owner when eventless context installation changes the text.
  Rationale: The transport has a real frame limit, but accepted context can legitimately exceed it in aggregate. Optional byte offsets and the existing barrier preserve completeness without a second RPC, snapshot cache, or recovery state machine.
  Date/Author: 2026-11-25, parent agent, first-review revision.
- Decision: Restore saved pending text into the seed regardless of native continuity; the existing cross-harness handoff install then appends only the new handoff. Replace older checkpoint-only workers through their existing current-binary launch path while retaining relay state.
  Rationale: Seeding eliminates the oversized combined request and an unnecessary cross-harness special case. Replacing the worker removes the recovery dead end without starting a failed harness.
  Date/Author: 2026-11-25, parent agent, first-review revision.

## Outcomes & Retrospective


The initial implementation passed ten focused tests and all-target clippy across ten affected crates. It is committed as 356497937, followed by a conflict-free upstream merge and release preparation 49236a8fa for 2.76.0. First-review fixes are committed as 42335cb49: paged transfers cover more than 8 MiB of escaped Unicode, saved text restores directly from a real archive even across a further harness switch, and old checkpoint-only sources use existing replacement. Seven affected tests and ten-package clippy passed. Second-review corrections now pass the unsupported-schema, queued-capture/admission, and durable-launch/readiness admission tests plus ten-package clippy. In total 14 distinct focused tests have passed across the implementation, with only affected tests repeated after each change. The checkpoint-only regression proves replacement is attempted and injected failure preserves launch/suspend capture/relay state; the unchanged helper's successful restart is not newly simulated. The reviewed code and version were pushed at ae3157fad and tagged v2.76.0; the first release run was cancelled after CI failures, with no GitHub Release or package publication. No live session was mutated. The third review found a redundant pre-stop upgrade guard: existing origin-delta stop already retains the relay and launches the current checkpoint-only worker. Deleting the guard preserves automatic moves on protocol 94; its existing isolated regression now covers automatic and explicit close without replacing the harness. That regression and ten-package clippy pass. The post-stop current-worker path is verified by source inspection; the test stops at an injected quiescence refusal. Fourth review explicitly deferred the queued pre-95 non-OriginDelta close compatibility issue to https://github.com/BrokkAi/mjolnir/issues/1703 to avoid an unsafe additional rollback mechanism. The code is pushed; release CI repair and publication remain.

## Context and Orientation


The daemon controls session lifecycle; each worker owns the agent and its relay journal. A checkpoint barrier stops dispatch at a consistent event cursor (the cut) so state can be exported. `mj-core/src/relay/snapshot.rs` defines `PendingPromptContext { text, attached_command_id }`. `mj-worker/src/relay/commands.rs` attaches the text to the first eligible prompt, and `mj-core/src/relay/snapshot/apply.rs` clears it only on completion or detaches it on interruption. Those delivery semantics already work.

`mj-core/src/archive.rs` defines `CanonicalSessionSnapshot`, the stable saved session format, and `content_matches`, used for archive reuse. `mj-core/src/relay.rs` defines `RestoredRelaySeed`, the compact state installed into a newly restored worker. Both currently omit pending text and reject unknown fields. `mj-checkpoint/src/archive.rs` selects and validates archive schema versions; the current highest is 15. `mj-checkpoint/src/checkpoint/restore.rs::relay_seed_for_restore` builds the restored seed.

`mj-core/src/relay/protocol.rs` defines `ReadCheckpointNativeUse` and `CheckpointNativeUse`. The current capability is 80 and overall relay version is 94. `mj-worker/src/relay/checkpoint_hold.rs` checks the barrier ID and cursor before reading native-use evidence. Extend this read to carry pending text from the same snapshot. `mj-session-runtime/src/worker_client/relay.rs` and `session_manager/standalone.rs` wrap the response. `mj-controller/src/controller/checkpoint/latched.rs` assembles the canonical snapshot, while `controller/checkpoint/staging.rs` handles archive export and reuse. `mj-controller/src/controller/resume.rs` performs restore and installs generated cross-harness handoffs.

## Plan of Work


The format owner edits mj-core and mj-checkpoint. Add an optional string named `pending_prompt_context` to the canonical snapshot and restored seed, with serde default and omission when absent. Include it in content comparison. Add optional text to the existing native-use response and a documented protocol-95 capability constant. Retain old wire request names and old-request compatibility. Introduce schema 16 only for archives containing this field and update schema checks and existing format fixtures. Restore pending text independently of queue policy and native continuity. Extend the same request with optional `pending_context_offset: Option<u64>` (absent means byte zero) and response with `next_context_offset: Option<u64>` (absent means complete). Offsets name raw UTF-8 bytes; each page must end at a character boundary.

The worker owner edits mj-worker. Read text at the same established cut as native-use evidence, fit each page within the actual serialized relay frame budget without truncation, seed the existing pending field, and preserve the current live relay over a stale seed during repair. Validate requested byte offsets and ensure returned next offsets advance. Eventless context installation must spoil the existing cut, so a reader cannot combine pages from different snapshots. Verify whether the cut can carry an attached in-flight dispatch and uphold the existing completion/interruption invariant. Extend existing relay/protocol tests to prove multi-frame complete transfer, escaped Unicode boundaries, cut rejection after context changes, and exact first-prompt delivery after seed restoration and restart.

The controller owner edits mj-controller and mj-session-runtime. The existing client wrapper requests successive pages under the same barrier ID/cursor, assembles the complete optional text, and keeps its existing outward evidence/text result. Capture it into the canonical snapshot and ensure old workers cannot be mistaken for a proof that context is absent. Use existing idle replacement for live workers and current-binary checkpoint-only replacement for failed-harness recovery, retaining the relay files; do not introduce a parallel upgrade state machine or start the failed harness. Seed saved text directly on every restore; the original cross-harness code installs only the new generated handoff before work is admitted. Existing stopped-subagent notes retain their ordering. Extend controller integration tests using actual archives, including old checkpoint-only recovery and further harness switches, and ensure context changes prevent stale reuse.

Writers own disjoint crate trees and fix constructor literals in their owned crates. No writer runs a build while another writer is editing. After integration, one validation worker owns formatting, compilation, focused tests, and clippy. The parent owns design decisions, plan updates, integrated review, commits, and acceptance. No work in go/ is authorized.

## Concrete Steps


All commands run from this repository root. Use mbx with the existing build storage; never redirect target directories or use plain Cargo. For validation, run elevated because tests use sockets:

    mbx test -p brokk-mj-core -p brokk-mj-checkpoint -p brokk-mj-worker-runtime -p brokk-mj-session-runtime -p brokk-mj-controller checkpoint_round_trips_the_latched_materialized_session
    mbx test -p brokk-mj-core -p brokk-mj-checkpoint -p brokk-mj-worker-runtime -p brokk-mj-session-runtime -p brokk-mj-controller checkpoint_native_use_returns_worker_evidence_at_cut_and_refuses_off_cut
    mbx test -p brokk-mj-core -p brokk-mj-checkpoint -p brokk-mj-worker-runtime -p brokk-mj-session-runtime -p brokk-mj-controller a_stopped_sub_agents_note_reaches_only_the_first_prompt_after_a_resume
    mbx test -p brokk-mj-core -p brokk-mj-checkpoint -p brokk-mj-worker-runtime -p brokk-mj-session-runtime -p brokk-mj-controller a_resume_tells_the_agent_and_the_person_which_sub_agents_the_suspend_stopped
    mbx clippy -p brokk-mj-core -p brokk-mj-checkpoint -p brokk-mj-worker-runtime -p brokk-mj-session-runtime -p brokk-mj-controller --all-targets -- -D warnings

The validator must replace broad filters with the narrowest meaningful existing test names after inspecting the final changes, record nonzero test counts, and rerun only failures after repairs. Use dev profile. Any real CLI/daemon scenario must use a short-lived named `--instance pending-context-checkpoint` with isolated state and stop its daemon afterward. Automated tests retain their existing isolation; never point test binaries at the host default instance.

## Validation and Acceptance


Acceptance requires a real archive export/read/seed round trip retaining the exact pending text when queued commands are kept or discarded, a first eligible worker prompt receiving it once, interruption preserving it for retry, and successful completion removing it for later prompts. Verify a subsequent harness switch keeps arbitrary hidden notes alongside the new handoff. Extend existing boundary and tricky-condition tests rather than adding getter tests. Add a hard-won comment for the post-use checkpoint omission. Schema checks must accept older archives, select version 16 only when needed, and reject inconsistent manifests. At a matching journal frontier, changing only pending context must invalidate archive reuse. A worker at an older protocol must be handled explicitly through the existing upgrade mechanism.

Passing validation must report exact commands, test counts, and decisive log paths. The parent will read the integrated diff, commit only this task's files on the current branch, call request_review, then push origin/master after a clean review or justified overrides. If upstream moved, merge origin/master rather than rebase, and review any actual conflict resolutions.

After committing the fix, follow `RELEASING.md` as requested by the user. From a clean checkout prepare the next minor release using the installed cargo-release command through mbx, preserving existing build storage. It updates workspace versions, internal dependency requirements, and Cargo.lock and commits the preparation. Request review covering the fix and release preparation before pushing either commit, tag the prepared version commit, and push the new immutable version tag. Release CI provides release-wide validation, so do not repeat it locally. Monitor the release workflow and required package-channel updates, remedy actual failures using the runbook, and report the published release link.

## Idempotence and Recovery


No live session or database migration is needed. Old archives cannot recover text they never saved; the new reader must continue accepting them. New fields are absent when empty, and new archives carrying them declare their schema. Preserve runtime state through the existing worker upgrade path. Tests may be repeated in their isolated directories; do not stop or replace the live daemon. Keep evidence in child report directories during this run and summarize decisive results in this plan before commit.

## Artifacts and Notes


The design source was a read-only investigation. Its essential conclusions and file pointers are embedded here so the implementation does not depend on untracked reports surviving a suspend. Expected proof is a previously failing suspend-before-first-prompt archive round trip that now delivers the handoff once, with zero focused-test and lint failures.

## Interfaces and Dependencies


Use only the existing Rust workspace crates and serde format support. The new canonical and seed field is `pub pending_prompt_context: Option<String>`. The existing `RelayResponsePayload::CheckpointNativeUse` gains that optional page field and `next_context_offset: Option<u64>`; `ReadCheckpointNativeUse` gains `pending_context_offset: Option<u64>`. The runtime wrapper assembles pages while preserving its existing evidence/text return value. Restore initializes `PendingPromptContext` using the saved text and no historical command attachment after the barrier has settled dispatch, including on a further harness switch. The wire capability and archive schema are version declarations, not new runtime state machines.

Plan created 2026-11-25 to implement the user's approved state-preservation design.

Plan amended 2026-11-25: the user requested a new mj release after the fix; release preparation and publication are now part of acceptance.

Plan corrected 2026-11-25 after validator preparation: use actual package names (the worker library package is brokk-mj-worker-runtime) and hold the package selection stable across filtered test commands. Earlier directory-name package selectors were invalid; no builds used them.

Plan updated 2026-11-25 22:48Z: all implementation owners handed back, and initial compilation identified dependent snapshot constructors. Existing barrier invariants settle or recover attached prompts before restoring text without a dispatch ID. Compatibility uses normal worker replacement for explicit suspend, defers background captures requiring old-worker replacement, and visibly rejects an old checkpoint-only source before exporting incomplete state.

Plan updated 2026-11-25 22:58Z: all 10 targeted regressions and affected-consumer clippy pass after narrow compiler and fixture fixes. Prepare the clean-checkout release commit before requesting one review of the complete deliverable. Detailed execution logs remain in validator and controller-child report directories for the current run; the decisive results are summarized above.

Plan revised after the first complete review on 2026-11-25: all three distinct findings are accepted. Add paging within the same checkpoint read, seed pending text on all restores to simplify cross-harness installation, and replace older checkpoint-only workers without invoking the failed harness. The prepared version remains 2.76.0; tag only the final reviewed commit containing these fixes.

Plan updated 2026-11-25 23:15Z: first-review fixes pass all seven affected tests, changed-file formatting, and ten-package all-target clippy. Earlier unaffected passing schema, reuse, and legacy-idle tests were not repeated. Commit these fixes and request the second review before any push.

Plan updated 2026-11-25 23:23:42 UTC: second review found one existing fixture selecting newly-supported schema 16 as invalid, and preparation/readiness holding daemon admission during checkpoint-only replacement. Select schema 17 and use the existing resumable boundary with a labeled swap guard. The three affected regressions, changed-file formatting, and ten-package clippy pass. Request the third review before push and publication.

Plan updated 2026-11-25 23:28:49 UTC: third-review correction removes the unnecessary pre-stop upgrade instead of adding another path. The protocol-94 regression and clippy pass; commit and request the fourth review before pushing.


## Release CI repair milestone


Release run 45722471421 failed Rust formatting, viewer fixtures, and the existing crate-size check. It was cancelled before publication; release/registry checks confirm v2.76.0 remains unpublished. Follow RELEASING.md to fix the commit and replace this unpublished tag only.

Formatting repairs touch wrapping in mj-core/src/suspend_consent.rs, mj-daemon/src/daemon.rs, and mj-daemon/src/daemon/tests.rs. Viewer tests were missing existing isSubagentSession, sessionById, and moveDelegationApplies helpers in isolated JavaScript contexts. Load those real helpers and session rows in tests/e2e/web/viewer.targets.unit.test.mjs and viewer.unit.test.mjs. These files pass 4 and 41 tests. Other viewer unit files passed; the projects test passed after using existing lockfile-matching Playwright and elevated process permissions.

The checker counts physical Rust lines under src. The CLI grew from 49,473 at v2.75.0 to 50,375 against its existing 50,000 budget, without CLI source changes in the pending-context fix. Move its existing 749-line process logging module and colocated tests from mj-cli/src/logging.rs into mj-client/src/logging.rs. Client already owns daemon metadata/lifecycle lookup used by logging. Export only the four caller entrypoints, change internal paths to crate::daemon, add the existing workspace tracing-subscriber dependency, and adapt the two CLI imports. Preserve behavior and platform conditions, create no crate, and do not raise the budget. Measured CLI size is 49,626 and client size 20,362 after the move.

The logging owner is the only Rust builder. Validate the demonstrated failures and changed boundary with the existing crate-size checker, mbx fmt --all -- --check, elevated dev mbx test -p brokk-mj-client -p brokk-mjolnir logging, and mbx clippy -p brokk-mj-client -p brokk-mjolnir --all-targets -- -D warnings. Independent viewer tests use Node. Commit/review/push the repairs, then replace the unpublished v2.76.0 tag at the final reviewed commit. Release-wide validation stays remote.

Plan amended after the first release attempt to handle only observed publication blockers. The legacy queued-close issue remains explicitly tracked as #1703 and is not described as fixed.


Plan updated 2026-11-25 23:47Z: all three observed release blockers are repaired. The logger move preserves implementation byte-for-byte except cross-crate visibility and two internal paths; tracing-subscriber moved with its only user and Cargo.lock changed only dependency edges. Full workspace formatting and crate-size checks pass. Focused logging tests pass 10 client and 1 CLI test; final all-target client/CLI clippy passes. Viewer fixture files pass 4 and 41 tests, and the isolated projects test passes with existing matching dependencies. Review the integrated repair, push it, and retry the unpublished 2.76.0 tag per RELEASING.md.

CI-repair review identified implicit tracing metadata changed by the logger module move. Preserve the original mj::logging target explicitly on both emitted events, so existing RUST_LOG filters retain the startup record and invalid-filter warning. Changed-file formatting and final client/CLI all-target clippy pass; unchanged logging tests were not repeated.

The second release run, 45724647758, was cancelled before publication after two demonstrated failures. The server active-stop test successfully fetched preflight but its FakeNode lacked native replaceChildren, so rendering threw. Add that API using the fixture's existing remove/append linkage; the exact server regression now passes 1/1 with 319 others filtered, and active/idle diagnostic renders complete. No timeout or production behavior changed. The notice generator pinned libsqlite3-sys 0.39.1 although the locked graph resolves 0.40.0; select the unique resolved wrapper while retaining the audited SQLite 3.54.1 content/version and inventory checks. A full generator run on the locked offline graph succeeds and cites 0.40.0/3.54.1. These two small repairs require commit/review/push and another unpublished-tag retry; earlier successful checks were not repeated.


## Third release attempt repair


Run 45725719000 was cancelled before publication after controller tests (7 failures), daemon tests (10), and Windows test compilation (4 errors). Its server tests, notices, formatting, viewer checks, dependency budget and other completed gates passed. The tag remains unpublished.

The lifecycle failures came from current DurableRelay stand-ins advertising their default protocol 58. Configure only affected current-worker cases using RELAY_PROTOCOL_VERSION; retain deliberately older-worker cases and the global stand-in default. Current archive fixtures must include the stand-in's open-request checkpoint, notification stack and native-use evidence, centralized in controller/checkpoint/stand_in.rs::current_latch_archive_metadata. Reused copies now match actual same-cut metadata. A cross-target export and resumed clone supply the real native rollout claimed by Used evidence. Stand-ins without the project-memory transport explicitly opt out where memory transfer is not under test. Assertions preserving session/checkout state, child lifecycle, cancellation, mail and archive reuse remain.

The listing fixture now pairs its session with a same-profile spawn relation, preserving its existing model assertion while respecting current metadata ownership. The quota provenance test now expects Obsolete for stale profiles for every session kind. This matches unified Move ownership: accepted Moves persist their continuation and recover it; an assessment from profile A must not replan using profile B. The previous exemption belonged to the removed nondurable ChildSwitch. The existing recovered_parked_move_submits_saved_quota_continuation_before_completion regression passes; this does not prove recovery of an interrupted legacy ChildSwitch across upgrade.

Windows fixture compilation is repaired with portable read-only permissions and correctly scoped imports, preserving the cross-platform rollback test and existing POSIX-only ignore. Actual Windows compilation remains the next release run's responsibility.

Validation: all seven controller failure filters plus saved continuation passed individually. All ten daemon failure filters passed individually with nonzero counts; only failed attempts were rerun after fixture repair. Ten-package dev all-target clippy passes after the final source edits. No product behavior changed in these repairs. Parent reviewed the fixture changes; next commit/review/push them and retry the still-unpublished v2.76.0 tag. Status recorded 2026-11-26 00:33Z.
