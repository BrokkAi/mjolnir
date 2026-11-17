# Let turn reviews run while the conversation continues

## Purpose

The primary conversation should accept prompts while independent reviewers work. Findings still enter the primary worker's ordinary prompt queue. A new completed turn during an open review requests one follow-up review after the current review closes.

## Progress

- [x] Trace prompt admission, review persistence, and reviewer delivery.
- [x] Remove daemon prompt holds and their internal admission capability.
- [x] Persist and drain one coalesced follow-up review, including restart recovery.
- [x] Retry corrective delivery with a durable attempt count and stable command ID.
- [x] Remove manual resolution surfaces (separate UI/API owner).
- [x] Validate focused behavior, crate suites, clippy, formatting, and web specs.

## Surprises & Discoveries

The corrective delivery capability was issued through the prompt hold registry. Removing the registry required changing delivery to the ordinary command path while preserving its durable command ID. Before-push verdict completion writes from an asynchronous task, so the pending follow-up marker needs a dedicated atomic database update and stale state writes must preserve it.

## Decision Log

The review host remains the single owner of review admission and follow-up state. Schema revision 109 is breaking: older daemons cannot preserve the new follow-up marker or retry count and could lose coverage or repeat delivery indefinitely. The schema compatibility floor rises in the migration transaction. The host coalesces completed turns in one in-memory set and persists the marker through an atomic column update; it drains the set after the open review closes. A new review opening clears the marker. Corrective delivery retries at most three times with the same command ID; exhaustion closes without advancing coverage and records a notice. Failed reviewer verdicts do the same.

## Milestones

First, remove prompt refusal in the actor, HTTP layer, and both composers. A prompt sent during a running review should be admitted by the worker and the review should continue.

Second, change the host to enqueue findings through the worker's ordinary prompt queue and to record a durable pending follow-up when a turn ends during an open review. Closing the open review starts one follow-up using the then-current baseline.

Third, remove the manual resolution actions from public surfaces while preserving old persisted phase and resolution variants for reading. Run focused concurrency and restore tests, then the affected crate suites, workspace clippy, rustfmt, and the web review spec.

## Outcomes & Retrospective

The daemon now accepts primary prompts throughout review, queues corrective findings as ordinary prompts, resolves reviewer failures and exhausted delivery retries on its own, and starts one durable follow-up after an overlapping turn. Revision 109 protects the marker and retry count from older writers. The controller, core, review, and CLI daemon-startup suites passed; workspace clippy, formatting, and diff checks passed. The UI/API owner separately removed the controls and validated TUI and web renderings.
