# Publish and enforce target harness runtime identity

This living ExecPlan follows `.agents/PLANS.md` and addresses #1163. It builds on the exact-checkout PR's session plumbing and migration 54, without changing that contract.

## Purpose / Big Picture

A scheduler can create a session without a task prompt, read the runtime receipt from the public API, save its comparison identity, and require that identity on a later session. The worker that accepts work checks the requirement against its own resolved runtime. Changed or unknown selections fail before prompting. A daemon restart, resumed session, or worker replacement must retain earlier receipts as history.

## Progress

- [x] (2026-09-26) Claimed the issue and traced managed installation, ACP startup, relay admission, durable projection, and public events.
- [x] (2026-09-26) Define runtime identity and collect component provenance on the worker.
- [x] (2026-09-26) Persist and forward the optional expected identity through HTTP, ACP, session records, and worker launch.
- [x] (2026-09-26) Record receipts in the worker journal and public event history; expose the latest receipt on session lookup.
- [x] (2026-09-26) Enforce startup and prompt admission constraints and add isolated regressions.
- [x] (2026-09-26) Documented the HTTP/ACP contract and passed 10 focused tests: 3 controller, 6 worker, and 1 ACP consumer.
- [ ] Open and review the PR, and merge after CI validation.

## Surprises & Discoveries

An ACP session load can resume a native goal before the coordinator consumes the queued initialization event. Therefore the launch specification also carries the identity constraint, and ACP checks it synchronously immediately after initialization and before session load/new/resume. The relay still checks before readiness and under admission/dispatch. A real fake-bridge test records all methods and requires only `initialize` on mismatch.

npm can hoist the provider binary beside its package. Package fingerprints therefore include the containing node_modules tree; this may conservatively change identity when another package in that tree changes. The comparison excludes system libraries and out-of-band mutations of installations; it is not image attestation.

The cross-layer history test caught that API events are derived after observation projection; receipt events must be added in `mj-transcript/src/projection/api_events.rs`, not the earlier observation mutation which is replaced.

The worker journal already records every ACP initialization, but the controller projection discards it. Extending that observation with an optional runtime receipt preserves old journal digests when the field is absent. Public API events are durable and already retain session history, so they can hold each new runtime receipt without overwriting older runs. The default container supplies CODEX_PATH and Muse installation metadata on the target; custom images may lack enough provenance to establish a comparable identity.

## Decision Log

- Decision: Use an opaque versioned identity over explicitly described target runtime components, with readable versions and explicit unknown provenance. Resolve the actual selected target command and provider metadata; never infer a container runtime from the controller's release version.
  Rationale: Bridge version alone is insufficient, and custom installations need an honest unknown state.
  Date/Author: 2026-09-26, Codex.
- Decision: Store `expected_runtime_identity` on the session and worker launch, check it at initialization and under the relay admission lock, and retain it across recovery/resume.
  Rationale: Discovery and dispatch cannot race through worker replacement. Existing unconstrained sessions keep normal upgrade behavior, and busy accepted work is never replaced to satisfy a request.
  Date/Author: 2026-09-26, Codex.
- Decision: Project each receipt as a public `runtime_resolved` event and expose the latest retained receipt on single-session lookup.
  Rationale: The existing append-only event path preserves every run across restart without maintaining daemon-only state or rewriting history.
  Date/Author: 2026-09-26, Codex.
- Decision: Migration 55 is breaking, with an atomic compatibility-floor update; bump the daemon protocol for the creation message change.
  Rationale: Older daemons cannot enforce the constraint, and older readers cannot interpret new stored runtime events.
  Date/Author: 2026-09-26, Codex.
- Decision: CI performs broad validation, as the user explicitly requested. Run focused local behavior tests and formatting; rely on the PR's full dev-profile test, Clippy, upgrade, and platform jobs before merging.
  Date/Author: 2026-09-26, Codex.

## Outcomes & Retrospective

The target inspection, durable receipt, expected selection, migration, HTTP lookup/events, and ACP flag are implemented. All 10 focused migration/API/ACP/installation/admission/pre-load tests passed; receipt persistence is verified after reopening the database. PR #1164 is independently running CI for exact checkout. Broad tests and Clippy will run in CI.

## Context and Orientation

A harness is the agent program executed by one worker. `mj-worker/src/worker_runtime/harness.rs` resolves managed installations, while `worker_runtime/unix.rs` prepares the actual launch environment and starts the ACP bridge (the program translating the provider interface to ACP). `worker_runtime/unix/dispatch.rs` receives its initialization event. `mj-worker/src/relay.rs` and `relay/commands.rs` own journal writes and command admission under one lock. `mj-core/src/relay/snapshot.rs` defines durable events and snapshots. `mj-transcript/src/projection/api_events.rs` turns worker events into controller mutations, including public API events stored by `mj-controller/src/database/events.rs`.

Creation follows `StartSessionRequest`, `ControllerAction::New`, `CreateSessionRequest`, and `SessionLaunchOptions`, ending in `SessionRecord`. `controller/worker_binary/launch.rs` constructs every worker launch, including resume and replacement. `mj-cli/src/acp.rs` creates owned sessions, waits for readiness, and forwards prompts. `server/api/subagent_backend.rs` exposes asynchronous database and worker operations to public routes; fake services exercise the same interfaces in tests.

## Plan of Work

First define shared serializable runtime component, resolved identity, and receipt types in `mj-core/src/harness_runtime.rs`. An opaque comparison ID covers the documented component/version/digest selection and platform, excluding paths, homes, credentials, model, effort, and unrelated environment values. Unknown identities have no comparison ID and a public reason. Add worker-side discovery beside managed harness resolution, using bounded background filesystem work and existing digest helpers. Managed provenance comes from the leased installation and its metadata. Ambient/container provenance comes from the actual selected command and provider installation metadata; insufficient metadata remains explicitly unknown.

Second propagate the optional expected identity from HTTP and `mj acp --expected-runtime-identity` through the durable session and every worker launch. Add nullable storage in migration 55 and preserve historical forward migrations. Reject empty constraints. An unavailable saved identity is an actionable refusal; arbitrary retired runtimes are not installed.

Third attach a receipt to each initialized runtime in the relay journal. Keep old serialized observations unchanged when no receipt exists. Check the expected identity before readiness and again when admitting a prompt under the same relay lock. A historical receipt is never proof that a newly started process has initialized. Persist each observation as an API event with its relay ordinal and receipt time; latest-session lookup reads the latest persisted event, while the event API exposes earlier receipts. All database reads stay off HTTP event loops.

Finally add fake target and disposable-store regressions: complete managed and container identities, unknown custom images, matching and mismatched constraints, stale discovery followed by a new runtime, receipt history after replay/resume, and ACP propagation without task prompts. Document discovery, comparison scope, unknown states, upgrade behavior, and receipt retention.

## Concrete Steps

Work at repository root on `codex/runtime-identity`. Run focused Cargo tests with elevated permissions and `TMPDIR=/home/ryan/mj-tmp-9767` because the shared tmpfs is full. Keep output under `target/`. Any manual application invocation must use `--instance runtime-identity-1163`, with disposable configuration/data directories. Never point this build at the live/default store.

    cargo fmt --all -- --check
    cargo test -p brokk-mj-worker runtime_identity
    cargo test -p brokk-mj-controller runtime_identity
    cargo test -p brokk-mjolnir acp::tests

CI runs `cargo test` and `cargo clippy --all-targets -- -D warnings` plus isolated upgrade/platform checks. Open a separate PR, initially based on the exact-checkout branch if it has not merged, then retarget master. Review the final diff and wait for green checks before merging. After both PRs merge, follow `RELEASING.md`; the user also authorized fixing master blockers and publishing a new release.

## Validation and Acceptance

Create a session without `prompt`, await readiness, and GET its session receipt. Save the non-null identity and supply it as `expected_runtime_identity` on another session or through the ACP flag. Matching identities permit work; a changed or unknown runtime refuses before a task prompt. A saved receipt from before an upgrade cannot authorize the replacement runtime. Omitting the constraint preserves existing behavior. Session lookup and runtime-resolved events remain readable after daemon restart and session resume, with a separate receipt for each initialized runtime.

## Idempotence and Recovery

Journal replay reprojects the same event ordinal once and does not duplicate public receipts. Expected identity is durable across daemon replacement. Worker replacement keeps the existing atomic idle admission rules and cannot cancel busy accepted work. A mismatch preserves session history and explains that the operator must discover and explicitly select the new identity. No probe reads credentials or starts a task prompt. Unknown installations remain usable for unconstrained callers.

## Artifacts and Notes

The shared tmpfs was full during #1162 validation. `/home/ryan/mj-tmp-9767` is task-specific and has sufficient disk space; no shared temporary files were deleted. Local broad validation is intentionally delegated to CI by the user's latest instruction.

## Interfaces and Dependencies

Use existing serde, SHA-256, `CommandExecutor`/subprocess helpers, worker installation leases, relay journal, and public event persistence. Add no workspace crate. Public fields are `expected_runtime_identity`, latest `runtime` receipt, and `runtime_resolved` events. Unknown runtime identity is explicit and cannot match any expected ID.

Initial plan recorded 2026-09-26 before implementation.

Revision 2026-09-26: implemented the cross-layer contract, added pre-load enforcement after identifying native goal recovery timing, and defined npm dependency fingerprint scope. User narrowed the remaining task to issues #1162/#1163 and the release; do not work the other issues.

Validation 2026-09-26: `cargo test -p brokk-mj-controller -p brokk-mj-worker -p brokk-mjolnir runtime_identity --lib --bins` passed 10 tests in isolated temporary stores, with no live harness or default instance. Formatting and diff whitespace checks passed.
