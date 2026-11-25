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
- [ ] Review integrated diff, commit, request review, resolve accepted findings, and push to origin/master.
- [ ] Prepare and review the next workspace release, push its version commit and tag, and follow the normal release workflow through publication.

## Surprises & Discoveries


The worker stores pending text outside the visible transcript. Installing it commits a relay snapshot without advancing the journal event frontier. Therefore archive reuse must compare pending context as content, rather than assuming a matching event frontier proves identical state. Pending context also carries arbitrary hidden notes and clear-context memory, so reconstructing it from visible history would be incomplete.

The initial targeted build stopped before tests because `mj-transcript/src/projection/materialize.rs` needed the new optional field. The same constructor update is needed in importer, wiki, daemon-test, and binary-test fixtures. Those are mechanical format-consumer fixes. The workspace format check also reports pre-existing differences in `mj-core/src/suspend_consent.rs`, `mj-daemon/src/daemon.rs`, and `mj-daemon/src/daemon/tests.rs`; keep unrelated formatting out of this change.

Validation also caught an existing checkpoint test launch fixture missing `parked_move: None`, a new archive assertion using a method as a field, and a new controller fixture installing hidden context after mailbox delivery had already started a prompt. Those were corrected narrowly. The controller regression now installs context through the normal request before delivery; its isolated subprocesses check protocols 95, 94, and pre-mail behavior.

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

## Outcomes & Retrospective


Implementation slices are integrated and parent-reviewed. Ten focused tests passed: archive round trip (1), schema/version behavior (1), content equality (1), cut reads and real socket transfer (2), restored delivery/interruption (1), controller capture and legacy source behavior (1), archive reuse (1), further cross-harness resume with stopped-subagent notes (1), and legacy idle suspend admission (1). All-target clippy passed across the five behavioral crates and five format consumers on dev profile. No live session was mutated. Commit, external review, push, and release publication remain.

## Context and Orientation


The daemon controls session lifecycle; each worker owns the agent and its relay journal. A checkpoint barrier stops dispatch at a consistent event cursor (the cut) so state can be exported. `mj-core/src/relay/snapshot.rs` defines `PendingPromptContext { text, attached_command_id }`. `mj-worker/src/relay/commands.rs` attaches the text to the first eligible prompt, and `mj-core/src/relay/snapshot/apply.rs` clears it only on completion or detaches it on interruption. Those delivery semantics already work.

`mj-core/src/archive.rs` defines `CanonicalSessionSnapshot`, the stable saved session format, and `content_matches`, used for archive reuse. `mj-core/src/relay.rs` defines `RestoredRelaySeed`, the compact state installed into a newly restored worker. Both currently omit pending text and reject unknown fields. `mj-checkpoint/src/archive.rs` selects and validates archive schema versions; the current highest is 15. `mj-checkpoint/src/checkpoint/restore.rs::relay_seed_for_restore` builds the restored seed.

`mj-core/src/relay/protocol.rs` defines `ReadCheckpointNativeUse` and `CheckpointNativeUse`. The current capability is 80 and overall relay version is 94. `mj-worker/src/relay/checkpoint_hold.rs` checks the barrier ID and cursor before reading native-use evidence. Extend this read to carry pending text from the same snapshot. `mj-session-runtime/src/worker_client/relay.rs` and `session_manager/standalone.rs` wrap the response. `mj-controller/src/controller/checkpoint/latched.rs` assembles the canonical snapshot, while `controller/checkpoint/staging.rs` handles archive export and reuse. `mj-controller/src/controller/resume.rs` performs restore and installs generated cross-harness handoffs.

## Plan of Work


The format owner edits mj-core and mj-checkpoint. Add an optional string named `pending_prompt_context` to the canonical snapshot and restored seed, with serde default and omission when absent. Include it in content comparison. Add optional text to the existing native-use response and a documented protocol-95 capability constant. Retain old wire request names and old-request compatibility. Introduce schema 16 only for archives containing this field and update schema checks and existing format fixtures. Restore pending text independently of queue policy and without installing it twice on a cross-harness path.

The worker owner edits mj-worker. Read text at the same established cut as native-use evidence, enforce the actual relay frame budget without truncation, seed the existing pending field, and preserve the current live relay over a stale seed during repair. Verify whether the cut can carry an attached in-flight dispatch and uphold the existing completion/interruption invariant. Extend existing relay/protocol tests to prove cut rejection and exact first-prompt delivery after seed restoration and restart.

The controller owner edits mj-controller and mj-session-runtime. Carry both reply fields through the existing client wrappers, capture text into the canonical snapshot at the cut, and ensure old workers cannot be mistaken for a proof that context is absent. Prefer existing idle worker replacement before capture; do not add a parallel upgrade path. Preserve pending notes on a second harness switch in the existing handoff installation before work is admitted. Extend existing controller checkpoint and resume tests and ensure content changes prevent stale archive reuse.

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


Use only the existing Rust workspace crates and serde format support. The new canonical and seed field is `pub pending_prompt_context: Option<String>`. The existing `RelayResponsePayload::CheckpointNativeUse` gains the same optional field. The runtime wrappers may return the existing evidence and text together without introducing another authority or persistence format. Restore initializes `PendingPromptContext` using the saved text and no historical command attachment after the barrier has settled dispatch. The wire capability and archive schema are version declarations, not new runtime state machines.

Plan created 2026-11-25 to implement the user's approved state-preservation design.

Plan amended 2026-11-25: the user requested a new mj release after the fix; release preparation and publication are now part of acceptance.

Plan corrected 2026-11-25 after validator preparation: use actual package names (the worker library package is brokk-mj-worker-runtime) and hold the package selection stable across filtered test commands. Earlier directory-name package selectors were invalid; no builds used them.

Plan updated 2026-11-25 22:48Z: all implementation owners handed back, and initial compilation identified dependent snapshot constructors. Existing barrier invariants settle or recover attached prompts before restoring text without a dispatch ID. Compatibility uses normal worker replacement for explicit suspend, defers background captures requiring old-worker replacement, and visibly rejects an old checkpoint-only source before exporting incomplete state.

Plan updated 2026-11-25 22:58Z: all 10 targeted regressions and affected-consumer clippy pass after narrow compiler and fixture fixes. Prepare the clean-checkout release commit before requesting one review of the complete deliverable. Detailed execution logs remain in validator and controller-child report directories for the current run; the decisive results are summarized above.
