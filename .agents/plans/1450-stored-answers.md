# Store projection-owned prompt answers

This plan follows `.agents/PLANS.md`.

## Purpose / Big Picture
Persist which accepted prompts finished outcomes answer, eliminating inference from changing queues in readers. Keep older v1 clients decoding outcomes and refuse older daemons access to the changed store.

## Progress
- [x] Inspected #1450 commits, steering, removal, and reminder admission.
- [x] Parent settled owed-set cap, own-prompt answers, and CLI mismatch notice.
- [x] Implemented stored watermark and centralized readers; added the breaking migration (113 after merging upstream, which took 111 and 112).
- [x] Finished outbound compatibility and regression fixtures.
- [x] Ran formatting, full touched library suites, the mj binary tests, and workspace clippy.

## Surprises & Discoveries
Queue removal only changes relay ledger state and emits QueueChanged. A queued rejection may record an outcome while another prompt runs.

## Decision Log
Parent approved capping by queued prompts except ending command and active turn unless belongs_to ending command. Each outcome answers its own acceptance as well as watermark. Numeric mismatches use neutral CLI wording.

## Outcomes & Retrospective
Implemented and validated. Readers ask `answers(ordinal)` of one stored outcome; no reader infers answers from the queue or the active turn. The migration became 113 when upstream took 111 (Oracle) and 112 (session titles).

## Context and Orientation
Core state defines outcome and reader; transcript observation projection owns the watermark. Controller wait and child progress consume the outcome. Database schema controls compatibility.

## Plan of Work
Update projection, eliminate queue inference and cached scalar answers, add migration 113 without backfill, strip outbound metadata from v1 outcomes and runtime headers, extend existing regressions.

## Concrete Steps
Run plain cargo with normal mbx storage; tests outside sandbox with isolated existing fixtures. No live daemon access.

## Validation and Acceptance
Run touched crate --lib suites with TZ=UTC, CLI api_commands, cargo fmt --check, and cargo clippy --all-targets -- -D warnings. Queued B rejection answers B but leaves running A owed. A later completion answers removed B and CLI names the actual outcome.

## Idempotence and Recovery
No backfill or live store migration. Changes remain uncommitted for parent review.

## Artifacts and Notes
Evidence and validation logs in `.mj/agents/f1748e5d6581ce6829a09182bd56c6b9/`.

## Interfaces and Dependencies
MaterializedTurnOutcome.answered_through, answered_ordinal(), and answers(ordinal) own answer interpretation.
