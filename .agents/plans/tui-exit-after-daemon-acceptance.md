# Detach accepted lifecycle operations and stop retiring workers once

This ExecPlan is maintained according to `.agents/PLANS.md`. It records the final design, validation and the scope cut after review.

## Purpose / Big Picture

Suspending a session can require checkpointing and target cleanup that outlive the dashboard. Once the daemon has accepted responsibility, closing the TUI must not wait for execution to finish or cancel an accepted Move. Unsent requests, drafts and other client-owned writes must remain protected until their own handoff or save.

The reported suspend of session 5bc0d90883ed581d402cf1eee88b4a2f spent minutes stopping 93 historical worker roots serially and then stopping them again during removal. Stop independent roots concurrently once, and retain the managed lifecycle exclusion through removal. Keep the established bounded raw-worker shutdown and existing immediate container stop. Do not expand this latency fix into an unproven arbitrary-process containment mechanism.

## Progress

- [x] Confirmed the installed daemon was node-removal build 4732bd096; this report was unrelated to the old node-stream threshold.
- [x] Measured the delay: checkpoint export 6927ms; 93 serial stops 464557ms; another 19 repeated stop-and-remove commands consumed 106194ms before the sample ended.
- [x] Claimed #1506 and #1507 and mapped TUI request, quit and daemon ownership.
- [x] Implemented protocol87 lifecycle acceptance/observation, durable readiness, typed phase errors and TUI admission-only quit guards.
- [x] Implemented concurrent one-pass managed retirement with held worker permits and removal-only plans.
- [x] Completed integrated development tests and affected failure repairs, workspace Clippy and format/diff checks; proved actual graceful TUI exit while accepted completion remained pending.
- [x] Committed the initial implementation as b78af534d and requested review.
- [x] Accepted two real review findings in the newly added process-tree fence; selected complete removal of that mechanism after independent ownership investigation.
- [x] Completed the scoped backend cut, ten affected tests, final workspace Clippy with zero warnings and format/diff checks. Both worker-stop source files exactly match the pre-task baseline.
- [ ] Commit the correction, complete required review and push. Installation is explicitly excluded.

## Surprises & Discoveries

The original worker answered Idle in 22ms. Its checkpoint export completed in 6927ms (repositories 5986ms, native files 417ms, archive 214ms), followed by a 217ms copy and 28ms staging removal. The retiring environment contained 93 distinct roots: one parent Destroying and 92 logically Stopped children. Logical Stopped does not establish physical teardown. Serial retirement stops finished after 7m44.557s, and standalone cleanup plans then repeated stopping. There was no evidence of an unconditional five-second absent-worker sleep.

Suspend does not block the event loop; the TUI quit drain held a critical-operation guard until the synchronous completion reply. Restart, Resume, Discard and Destroy shared that wait. Quit also set lifecycle cancellation flags, causing Move and RetryMove to send CancelLifecycle. Creation and archive restoration already release their guards after registration. Resume repository preflight and archive-space preview were disposable reads unnecessarily treated as critical work.

The daemon already owns a serialized lifecycle map, exact operation IDs, supervised result watches and a completed-result FIFO. Reuse those mechanisms rather than adding another queue or result ledger. Existing durable writers provide the acceptance points. A generic refusal of a completion query is not failure of the previously accepted mutation.

Discard could not safely acknowledge after merely comparing a checkpoint. The existing Destroying state is the reusable durable intent: verify the selected archive, commit the existing cancellation control, persist Destroying with the checkpoint and target, then acknowledge. Existing interrupted-close recovery invokes child cleanup and finishes verified teardown without a new checkpoint. Stopped with a retained target was unsuitable because its existing deferred cleanup supports only retained Podman targets.

The first full suite found an obsolete workspace-close fixture. It seeded unkeyed operations and used close_requested as a join notification. Production now uses canonical selection keys, and the marker belongs to owner admission. The repaired test seeds canonical Fresh operations and waits for additional acceptance-watch subscriptions before cancelling; it retains its five-second deadline and committed/uncommitted stop assertions.

The attempted immediate raw-process fence was an unnecessary expansion. Session-wide environment markers cannot identify Move generations, and macOS ps text loses argument/environment boundaries. More fundamentally, a process-tree census followed by birth reads can attach old ancestry to a reused PID, while a child can fork and detach before freezing. Further snapshots cannot establish the claimed containment. Both official P1 findings are real; delete the new attribution path.

Existing raw owners explain the scope decision. A supervisor retains the actual bridge Child and process-group guard. Parent EOF gives it bounded stdin shutdown and group TERM/KILL cleanup. Killing the supervisor immediately can bypass cleanup of separately grouped harnesses. Terminal and user-shell registries own additional groups, but no current single immediate-teardown request or durable complete harness-group registry replaces them. The ordinary stop has existing snapshot/PID limitations; preserving it is not a claim of universal detached-writer containment.

## Decision Log

Decision: Separate operation latency from client exit latency. Rationale: moving a wait out of the TUI does not fix serial cleanup, and accepted daemon execution must not require an attached terminal.

Decision: Add typed StartSessionLifecycle and WaitSessionLifecycle, advancing daemon protocol86 to87. Return an exact session/operation receipt after real acceptance and observe its existing captured result separately. Reuse the current owner and FIFO, not a parallel task queue.

Decision: Register request identity, readiness and close_requested in one lifecycle-owner decision. Fresh requests join identical running work and replace completed attempts; legacy durable-key replay retains its previous result. Exact-ID observation never switches to a newer operation.

Decision: Readiness follows existing durable writers. Suspend follows Closing; Restart follows its original validated reservation and bound intent; Move follows save_move_operation or a validated unchanged result; Resume follows save_resumed_session with selected Provisioning data. Discard uses verified Destroying. Destroy retains its documented bounded, non-durable handoff exception.

Decision: Preserve phase honesty. Start transport/acknowledgement loss is LifecycleAdmissionUnconfirmed. Generic Wait refusal, protocol/transport loss or missing exact result is LifecycleObservationUnavailable. Only a captured failed result becomes LifecycleFailed and an ordinary operation failure. None of these paths arbitrarily replays a mutation.

Decision: Hold TUI quit guards through acceptance, then make completion observation disposable. Only explicit cancellation sends CancelLifecycle. Preserve draft, settings, read-status, import-byte and other client-owned write guards. Protect container-upgrade submission through its existing acceptance reply; make the two audited read-only previews disposable.

Decision: Keep owner-first/sorted worker permits and transactional Retiring admission. After existing container quiescence, perform independent remaining ordinary worker stops concurrently under their exact permits, join every result/panic and aggregate failures. Issue removal-only authority only after managed stops complete successfully, and retain its Reclamation borrow through command execution. Keep child removal serial and owner/enclosing removal last.

Decision: Remove the entire added raw-process fence and its identity refactor after review. Reuse the established bounded ordinary stop once per root in parallel. Rationale: the observed minutes came from serialized and repeated stops; immediate all-descendant containment needs ownership that the daemon currently lacks. Do not add a protocol, service, cgroup, generation marker, extra polling or another snapshot predicate to justify the discarded mechanism. Describe the retained authority as managed-stop completion plus lifecycle exclusion, never proof that all arbitrary forked writers are gone.

Decision: Validate, commit and push this task only. The user clarified that the earlier installation instruction did not cover this separate suspend/TUI work. Do not replace installed binaries or activate a new live daemon.

## Outcomes & Retrospective

The final implementation preserves the two latency fixes: concurrent one-pass retirement and TUI exit after daemon acceptance. The attempted immediate tree-kill mechanism was removed completely. This is a deliberate scope reduction, not a substitute claim that the old snapshot is containment-safe. The existing raw ownership limitation is recorded separately in https://github.com/BrokkAi/mjolnir/issues/1510.

The integrated five-package development suites passed 4,390 tests with 37 ignored, plus one ignored documentation test. One workspace-close fixture failed and was corrected; its affected rerun passed in0.39s. Client phase and TUI guard filters passed after boxing Move/Resume payloads without changing JSON. Workspace Clippy and format/diff checks passed after isolated enum/layout repairs. Passing full suites were not repeated for individual repairs.

The isolated real-TUI proof passed in instance tui-accepted-suspend-1507-92281: the pane exited in63.7ms, its requested-quit log recorded13ms total and0ms runtime shutdown, and the same accepted Suspend remained pending with its completion-publication hook held. Exact-ID Wait stayed pending, then returned Done after release with the session suspended. This is a process-exit/completion proof, not held checkpoint I/O; the separate actual daemon held-checkpoint wire regression supplies that evidence. Private cleanup completed.

Native Mac and Linux tests of the discarded fence remain historical experiment evidence, not validation claims for the final raw teardown design. The final scoped cut passed ten ordinary-stop/retirement/retained-Move tests in3.74s, followed by clean workspace Clippy, formatting and diff checks. The validator independently confirmed both process source files exactly match1f8ff4afc and both new reviewed paths are gone. No live suspend retry or new user-process cleanup was performed as a benchmark. No installation occurred.

## Context and Orientation

The TUI in mj-cli owns terminal input and unsaved client data. mj-controller's daemon owns lifecycle operations and persisted session/operation state. Workers own their harnesses and journals independently of attached clients. mj-client defines the daemon wire protocol.

Relevant source is mj-cli/src/dashboard/{actions,drains,io,io/spawn}.rs and pollers.rs; mj-client/src/daemon.rs; mj-controller/src/daemon/{session_lifecycle,lifecycle,owner,close,restart,resume,session_move}.rs; controller/{environment,lifecycle,move_session,resume}.rs; and targets/cleanup.rs. Existing targets/worker_daemon.rs and worker_binary/process.rs must retain their pre-task ordinary stop implementation after the cut.

Reclamation holds the retiring environment's inventory and worker permits. Its scoped stopped-worker value identifies a placement whose existing managed stop completed. An opaque cleanup plan retains that borrow until execution returns, preventing a caller from extracting an unguarded removal command.

## Plan of Work

Milestone one diagnoses the actual delay and the client/daemon boundary. It is complete with timestamped evidence, an operation matrix and the 93-root double-stop finding.

Milestone two implements acceptance and concurrent retirement through existing owners. It is complete except for removing the rejected process-tree fence. Restore that file and its helper to their pre-task implementation, call ordinary stop once per root, preserve the concurrency/failure/no-repeat tests and qualify the authority's managed-lifecycle contract.

Milestone three verifies the final scoped result and delivers reviewed commits to origin/master. Reuse accepted broad validation, run affected stop/retirement and retained-Move tests after the cut, run final workspace Clippy/format/diff checks, commit the correction and request review again. Merge upstream as already authorized. Do not install.

## Concrete Steps

Work on the current branch in this checkout. Do not create a branch, rebase or change build storage. Use normal Cargo/mbx targets. Every cargo test runs elevated with TZ=UTC. Keep existing private data/config fixtures; actual CLI, daemon and tmux invocations require named isolated instances. Do not set global MJ_INSTANCE for unit suites because it changes unrelated settings golden defaults.

The completed full validation used:

    TZ=UTC cargo test -p brokk-mjolnir -p brokk-mj-controller -p brokk-mj-client -p brokk-mj-core -p brokk-mj-worker --features brokk-mjolnir/test-hooks

The explicit feature enabled the controller completion-publication hook for the isolated real-TUI proof. Exact remaining executable targets ran once after Cargo's fail-fast, preserving the same build graph. Documentation tests ran separately. Reuse those results.

After the scope cut, use the same five packages and feature selection with --lib --bins and filters for environment::tests, worker_daemon::tests and retained-source Move cleanup. Repair only actual failures and rerun only affected filters. Then run:

    TZ=UTC cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check
    git diff --check

Stage only owned files. Commit the correction on the current branch, request the configured review and push after a clean verdict or a justified per-finding override. Both existing P1 findings are addressed by deleting their new code path. Do not describe baseline raw teardown limitations as fixed. Merge upstream changes rather than rebasing. No installation or live daemon handoff is authorized.

## Validation and Acceptance

Before acceptance, quit retains an unaccepted lifecycle request. After a receipt, quit neither waits for completion nor cancels accepted Move. A later observer sees the exact result or typed observation uncertainty; it must not restore Move input or encourage replay merely because a query was refused. Final client-owned draft/settings writes remain guarded.

Discard must verify its selected archive before committing intent, return its receipt before held teardown completes and recover from existing Destroying state with the original checkpoint and child reconciliation. Move and Resume readiness must follow their existing committed writers.

Retirement must acquire managed lifecycle authority, stop independent roots concurrently, join every failure, perform no removal after failed stop and avoid a second stop during removal. Its authority remains borrowed through exact-target deletion, with owner-last ordering. This scope preserves ordinary raw shutdown and container-engine behavior; it adds no arbitrary-process containment guarantee.

## Idempotence and Recovery

Investigation is read-only with respect to live sessions. Tests own isolated state and processes. A lost acknowledgement is not permission to replay a mutation. Existing durable writers and recovery paths own accepted work; no new schema or record is introduced. The completed-result FIFO and exact-ID result watches remain the single observation owner.

Retiring environment state supports retry after a failed managed stop. Keep all worker permits until every concurrent result has been joined and removal has returned. Do not delete working files as a substitute for stopping their owners.

## Artifacts and Notes

Issues: https://github.com/BrokkAi/mjolnir/issues/1506 and https://github.com/BrokkAi/mjolnir/issues/1507.

Live timings and backend work are in .mj/agents/1cbd92b1e462d4831611d7744363a0df/. Its 1506-authoritative-ownership-map.md describes actual group owners. The independent design review is .mj/agents/f2c6d8abae249bbaa96ea6880588d309/teardown-design-review.md; its final scope-cut addendum supersedes earlier immediate-kill proposals.

API evidence is .mj/agents/3015c9deeefcbc11932935c38d210b42/implementation.md, review-fixes-focused.log and authorized-fixture-boxing-tests.log. The initial incomplete TUI child was closed before parent takeover; the parent owns its final CLI changes.

Integrated evidence is .mj/agents/b0df254c0e1ea40035ec39f1841037fe/final-validation.md, test-summary.json and smoke-process-exit-proof.json. Final scope-cut lint is clippy-scope-cut.log with format/diff siblings. Backend final tests and report are .mj/agents/1cbd92b1e462d4831611d7744363a0df/1506-scope-cut-tests.log and1506-backend-cut-report.md. The accepted one-off tmux adapter is accepted_suspend_tmux.py in the validator report directory; full evidence is under /mnt/optane/tmp/1507-tmux-completion-evidence/tui-accepted-suspend-seed-1507-92281.

## Interfaces and Dependencies

SessionLifecycleRequest is the closed daemon input enum with boxed Move and Resume payloads. LifecycleReceipt has public session_id and operation_id. DaemonClient::start_session_lifecycle returns it; wait_session_lifecycle observes Done/MoveOutcome, captured operation failure or typed observation uncertainty. Wire replies include LifecycleAccepted, LifecycleObservationUnavailable and LifecycleFailed; daemon protocol87, relay unchanged.

No new crate, runtime dependency, concurrency cap, timeout, service, generation marker or containment framework remains in the final design. Existing ordinary raw stop bounds and container-engine stop remain unchanged.

Revision note: Initial diagnostics and implementation were followed by independent review. Two real races invalidated the added PS-based fencing proof. This revision records the final scope cut, the preserved latency/acceptance fixes, reusable validation and the user's explicit no-install instruction.

Revision note: The final scoped cut passed affected tests and workspace lint/format checks. Baseline raw process ownership is tracked separately as #1510; the current fixes are prepared for follow-up review and push.
