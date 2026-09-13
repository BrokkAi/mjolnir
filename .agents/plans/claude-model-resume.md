# Preserve Claude models across resume

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

An Opus 1M session must restart with its saved model without asking for a replacement. Discovery must use the same setup-token authentication as worker launch. Recovery messages must describe failure to restore a selection, not assert that a model was withdrawn or that a default is running.

## Progress

- [x] (2026-09-13 UTC) Reproduced the cause using installed Claude ACP 0.73.0 and SDK 0.3.257 with private synthetic transcripts and no prompts.
- [x] Implement startup pin, shared authentication preparation, and cache invalidation.
- [x] Add protocol regressions and truthful recovery messages.
- [x] Run focused tests, full Cargo tests, and strict Clippy.
- [x] Prepare the validated changes for the required current-branch commit.

## Surprises & Discoveries

Setup-token authentication suppresses subscription metadata used by Claude's model menu. Fresh sessions add the configured opus[1m] to that menu; resumed sessions can omit it even while the live SDK still runs claude-opus-5[1m]. The ACP bridge then resolves settings against the incomplete menu and changes the model to opus. Mjolnir's later restoration encounters the missing option and raises recovery. Discovery previously omitted the automatically injected setup token.

The full installed bridge reproduction returned current=opus and live=claude-opus-5 without a startup pin; passing SDK options.model=opus[1m] returned current=opus[1m] and live=claude-opus-5[1m]. Both reported context=1000000. This proves a model identity change, not a loss of context capacity. Temporary credential copies were removed after their processes exited.

## Decision Log

Keep setup tokens to avoid rotating credential races. Pass the durable accepted model through existing Claude ACP metadata before session creation. Keep post-start selector restoration and genuine error reporting; do not add fuzzy matching or strip suffixes. Share the existing setup-token helper with discovery, resolve the probe environment once, and hash it into a versioned cache key. Never persist a raw token in cache metadata. Do not change existing sessions that already accepted opus. These decisions implement the user's approved plan (2026-09-13).

## Outcomes & Retrospective

Startup metadata, shared authentication resolution and versioned fingerprints are implemented. Claude worker observations no longer write probe cache entries because they cannot establish authentication provenance. Protocol regressions cover three consecutive bridge starts, queued work, changed accepted models, and startup error causes. All nine focused configuration tests pass (0.44 seconds). The full installed bridge probe was repeated and confirmed the same before/after model identity, with context=1000000 in both cases. Its private profile was removed after the child exited. Full cargo test passed, including the authentication-cache regression and PTY tests. Strict cargo clippy --all-targets -- -D warnings passed in 56.49 seconds. Formatting and git diff --check passed. Implementation and validation are complete. The changes are ready for the required current-branch commit. No active user sessions will be restarted or reconfigured for validation.

## Context and Orientation

`mj-worker/src/acp.rs` builds new/load/resume requests through session_request_meta, then restores AcceptedSessionConfig after the adapter starts. ACP is the JSON protocol between the worker and the Claude adapter. `mj-controller/src/controller/worker_binary.rs` injects a stored setup token into a worker's environment. `mj-controller/src/controller/profile_config.rs` runs isolated discovery and caches its model menu. Existing colocated tests and ACP subprocess fakes provide the validation harness.

## Plan of Work

First add the accepted Claude model to claudeCode.options.model in the shared request builder, preserving the other options. Expose the existing controller setup-token helper to sibling modules and use it to resolve discovery's environment before cache lookup. Include that environment and a version marker in fingerprint; use the same snapshot for the probe and its result. Avoid assigning authentication provenance to observed worker catalogues unless it can be established.

Next update recovery text and its form description to use the currently reported selection, without changing accepted values until an explicit replacement succeeds. Add protocol tests covering pinned restart, queued prompts after restored effort, changed accepted selections, missing selections and startup errors. Add cache/authentication regression coverage without real credentials.

Finally run the required validation, update this document with outcomes, and commit only task files on the current branch.

## Concrete Steps

From /home/jonathan/Projects/hel, run cargo test -p brokk-mj-worker and cargo test -p brokk-mj-controller for focused development as appropriate. Every cargo test runs elevated. Finish with cargo test and cargo clippy --all-targets -- -D warnings, using normal build storage, plus formatting and git diff --check. Expected outcome is no failed tests or lint warnings.

## Validation and Acceptance

A fake adapter must omit opus[1m] on resume unless metadata pins it. The fixed runtime must restore effort, dispatch a queued prompt, and emit no replacement request. Request coverage includes new/load/resume and selection changes between starts. Recovery still reports genuinely unavailable selectors; unrelated startup errors retain their cause. Authentication tests prove explicit overrides and token changes affect discovery consistently and invalidate cache. The installed bridge control above establishes the real provider behavior; repeat the isolated probe if changes invalidate that evidence.

## Idempotence and Recovery

No database migration or dependency change is needed. Cache versioning invalidates old entries naturally. Stop probe children before removing their temporary files. Leave unrelated files and active sessions alone. Do not publish or restart deployments as part of this implementation.

## Artifacts and Notes

Evidence from the installed bridge, with identical setup token and synthetic resumed history:

    without options.model: current=opus, live=claude-opus-5, context=1000000
    with options.model=opus[1m]: current=opus[1m], live=claude-opus-5[1m], context=1000000

## Interfaces and Dependencies

Use existing LaunchSpec.accepted_config and the existing claudeCode.options metadata. No new public API or crate. Keep the setup-token helper in its current controller module with sibling visibility. Fingerprint accepts the resolved BTreeMap environment. Existing workers remain protocol-compatible.

Revision: created from the approved plan with the empirical root-cause evidence and implementation checkpoints.

Revision: implemented the approved changes and excluded Claude observations from probe caching because worker events do not include the authentication snapshot.

Validation artifacts: target/claude-resume-focused.log and target/claude-resume-provider.log. The fake regression originally waited for the wire spelling end_turn rather than the runtime event EndTurn; correcting that test expectation made the three-start scenario pass.

Final validation artifacts: target/claude-resume-workspace.log and target/claude-resume-clippy.log. No dependencies or database schemas changed; the versioned cache fingerprint invalidates old menus.
