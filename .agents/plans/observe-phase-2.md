# Enforce Brokk Observe policies through Mjolnir

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan follows [`.agents/PLANS.md`](../PLANS.md). It implements GitHub issue [#1527](https://github.com/BrokkAi/mjolnir/issues/1527).

## Purpose / Big Picture

After this change, an organization can turn on Brokk Observe guardrails for Mjolnir sessions and have policy decisions made before a DSH tool runs. A matching deny rejects that one call without cancelling the turn; ask opens Mjolnir's normal permission form; warn is shown before the existing approval behavior continues; allow preserves Mjolnir's current behavior. The worker keeps tool approval off the ACP event loop while it waits for the remote decision.

Only DSH is claimed initially. dsh-acp exposes an `ask` permission mode that sends commands, writes, and MCP calls to Mjolnir before execution. Other Mjolnir harnesses may approve tools internally, so requesting enforcement for them fails at session launch with an actionable error instead of silently running without protection.

## Progress

- [x] (2026-11-13) Assigned issue #1527 and marked it `agent-in-progress`.
- [x] (2026-11-13) Added the authenticated Brokk Observe runtime policy endpoint.
- [x] (2026-11-13) Added Mjolnir's bearer-authenticated policy client with a bounded deadline and a redacted configuration representation.
- [x] (2026-11-13) Added the DSH-only capability check and forced-configured-approval execution policy for enforced sessions.
- [x] (2026-11-13) Moved permission handling into a spawned ACP task so the remote policy request cannot block protocol dispatch.
- [x] (2026-11-13) Added policy-action, local HTTP, fail-open/fail-closed, and end-to-end ACP deny coverage.
- [x] (2026-11-13) Added DSH capability and effective-execution-policy tests.
- [x] (2026-11-13) Validated all affected Mjolnir crates, the focused suites, Clippy, formatting, and the complete Brokk Observe suite against isolated PostgreSQL. One unrelated controller digest test fails locally because this host has neither `shasum` nor the expected fallback.
- [x] (2026-11-13) Documented environment variables, DSH-only rollout, failure semantics, and token isolation.
- [x] (2026-11-13) Kept the Mjolnir logical session in the policy client and included the harness-native session in event identity, covering native children with reused tool-call ids.
- [x] (2026-11-13) Split evaluation from enforcement acknowledgement. Observe stores runtime decisions provisionally and marks them enforced only after Mjolnir has delivered the permission response.
- [x] (2026-11-13) Record the resulting permission choice (`approved`, `rejected`, `warned`, or `cancelled`) with the acknowledgement.
- [x] (2026-11-13) Added end-to-end ask approval/rejection, warn, timeout, fail-open, fail-closed, and logical/native identity coverage.
- [x] (2026-11-13) Final verification: Observe passed all 66 unit and 65 API tests; Mjolnir Clippy and formatting passed; SlopCop returned `clean` in both repositories. Full affected Mjolnir suites passed except host-dependent cases documented below.

## Surprises & Discoveries

- Observation: ACP request callbacks run on the connection event loop.
  Evidence: the `agent-client-protocol` `ConnectionTo::spawn` documentation states that awaiting expensive work in a handler blocks every later message. Permission handling is now spawned, so the Observe request does not stop cancellation or session traffic.

- Observation: DSH's fake ACP bridge must acknowledge `session/set_config_option` with its `configOptions`, not an empty response, before the worker can prove permission mode `ask` is active.
  Evidence: the first end-to-end test failed with `select required ACP permission_mode ask: missing field configOptions`; the corrected fixture passes the gate.

- Observation: Returning a policy decision is not evidence that the caller delivered it. The permission responder can fail after the HTTP response, so Observe needs a second, idempotent acknowledgement.
  Evidence: the runtime policy test stores a deny with `enforced=false`, acknowledges it, and only then observes `enforced=true` and one denied call.

## Decision Log

- Decision: Support DSH first and fail visibly for every other harness.
  Rationale: DSH asks Mjolnir before every command, write, and MCP call in its `ask` permission mode. Other current harnesses can auto-approve some tools internally, so no common ACP veto exists for them at this checkpoint.
  Date/Author: 2026-11-13 / Codex

- Decision: Reuse `OBSERVE_URL`, `OBSERVE_TOKEN`, `OBSERVE_ENFORCE`, `OBSERVE_FAIL_CLOSED`, and `OBSERVE_TIMEOUT`.
  Rationale: These are the existing Brokk Observe plugin knobs, which makes rollout familiar and avoids a second configuration vocabulary.
  Date/Author: 2026-11-13 / Codex

- Decision: Keep the ingest token in `WorkerLaunchConfig`, never in the harness environment.
  Rationale: the worker needs the credential for the synchronous request, but the coding harness does not and should not inherit it.
  Date/Author: 2026-11-13 / Codex

- Decision: Execute the policy check in an ACP connection task before Mjolnir's YOLO, owned-MCP, trusted-`gh`, and human approval paths.
  Rationale: all existing automatic approvals must remain behind the policy gate, while the protocol can continue handling cancellation and other session messages.
  Date/Author: 2026-11-13 / Codex

- Decision: Keep the Mjolnir logical session id inside `PolicyClient`, and carry only the native ACP session id from the permission handler.
  Rationale: one owner prevents every call site from accidentally filing policy audit events under a harness-native identity. The native id still participates in event identity so native children do not collide.
  Date/Author: 2026-11-13 / Codex

- Decision: Record runtime decisions as unenforced, then acknowledge delivery separately.
  Rationale: an HTTP response can be lost before Mjolnir returns the selected option to the harness. A provisional event plus an acknowledgement cannot falsely claim a block or warning was applied.
  Date/Author: 2026-11-13 / Codex

## Outcomes & Retrospective

The server endpoint returns a generic action and stores the same event asynchronously with idempotent identity. The event starts unenforced; a second acknowledgement marks it applied after Mjolnir has delivered the permission response and records the resulting `approved`, `rejected`, `warned`, or `cancelled` choice. Pending acknowledgements are bound to the ingest key and can arrive before event storage. Mjolnir sends the logical session and native harness session separately, maps a deny to a one-time rejection, displays an ask reason in the existing permission form, and surfaces warn notices. End-to-end fake DSH sessions cover allow-preserving approve/reject forms, warn, timeout, fail-open, fail-closed, and a deny before an automatic approval path.

The final full Mjolnir run encountered unrelated host-dependent failures: the controller digest test lacks `shasum`; the worker credential-isolation test finds the host `gh` keyring; the core Node install tests intermittently hit `ETXTBSY` under parallel extraction and pass in isolation; and two daemon startup integration tests depend on the invoking shell's `HOME`. None touches the Observe enforcement path.

## Context and Orientation

Brokk Observe stores central policies in PostgreSQL and exposes admin management routes. `server/src/api.rs` now exposes `POST /api/v1/policy/evaluate` to an ingest key. The request names a session, stable event id, tool name, tool input, call id, working directory, and harness. The endpoint answers from the policy database before waiting for the normal ingest transaction, then stores the `PreToolUse` event in a background retry loop.

Mjolnir's controller builds one `WorkerLaunchConfig` for each session. `mj-core/src/worker_launch.rs` carries an optional `ObserveEnforcementConfig`. `mj-controller/src/controller/worker_binary/launch.rs` reads the environment, verifies the harness capability, and changes the worker execution policy to configured approvals. `mj-worker/src/observe.rs` owns the HTTP client. `mj-worker/src/acp/drive.rs` receives ACP permission requests and applies the decision before its existing automatic approvals.

An ACP permission request is the harness asking the client whether a tool may run. Reject-once denies only that call and lets the harness continue. Ask means Mjolnir asks the person. A tool-call notification alone is not a veto.

## Plan of Work

The first milestone adds the runtime endpoint and its agent identity. The endpoint validates ids, constructs the same canonical `PreToolUse` envelope as the Phase 1 collector, answers through the policy database, and stores the event with the answer.

The second milestone adds configuration and the capability gate. Environment values become a serializable `ObserveEnforcementConfig` with a redacted `Debug` implementation. DSH is accepted; every other harness receives a launch failure that explains why its approval behavior cannot guarantee a pre-execution decision.

The third milestone adds the worker client and ACP gate. Permission handling moves into an ACP connection task. The decision is evaluated before automatic approval. Deny selects `reject_once`, ask skips automatic approval and annotates the normal permission form, warn emits a runtime warning and continues, allow preserves existing behavior, and errors follow the configured open/closed mode.

The fourth milestone validates both fake ACP traffic and the real HTTP contract. The fake client verifies that DSH's permission mode is selected before the prompt, the policy endpoint receives the normalized command, and a deny returns `reject_once`.

## Concrete Steps

In `/home/ryansvihla/code/brokk-observe`:

    cargo test mjolnir_runtime_policy_returns_and_records_guardrails
    cargo test --no-run
    cargo clippy --all-targets -- -D warnings

The integration test needs PostgreSQL. Use an isolated container:

    docker run -d --rm --name observe-phase2-test \
      -e POSTGRES_PASSWORD=observe -e POSTGRES_DB=observe \
      -p 127.0.0.1:55432:5432 postgres:18
    OBSERVE_TEST_DATABASE_URL=postgres://postgres:observe@127.0.0.1:55432/observe \
      cargo test mjolnir_runtime_policy_returns_and_records_guardrails
    docker stop observe-phase2-test

In `/home/ryansvihla/code/mjolnir`:

    cargo test -p brokk-mj-worker observe_denial_precedes_automatic_approval_and_rejects_once
    cargo test -p brokk-mj-worker observe::tests
    cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker -p brokk-mjolnir observe
    cargo clippy -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker -p brokk-mjolnir --all-targets -- -D warnings
    cargo fmt --all -- --check

## Validation and Acceptance

The Observe integration test asserts deny, ask, and allow responses, stores the event under agent `mjolnir`, marks the deny enforced, and leaves a benign call without a policy decision.

The Mjolnir ACP test runs a fake dsh-acp session, sends a permission request for `git push --force origin main`, observes the HTTP request to a local policy server, and asserts that the ACP response selects `reject_once` instead of the one-time allow option.

The final acceptance is a real isolated DSH session with `OBSERVE_ENFORCE=true`: a default protected-branch policy rejects the force-push, the turn continues after the rejected call, and the Observe session shows the decision as enforced. A failure of the policy endpoint follows `OBSERVE_FAIL_CLOSED`.

## Idempotence and Recovery

The policy event id is derived from the Mjolnir session and tool-call id, so retrying the HTTP request does not store two decisions. The final permission response is still needed; Open/closed failure behavior is deterministic and reported in Mjolnir's notices.

The PostgreSQL test container is isolated by name and port and removed with `docker stop`. No user database is modified.

## Artifacts and Notes

The policy token is redacted from `Debug`, stored only in the worker launch configuration, and never inserted into `WorkerLaunchConfig::environment`.

The default execution policy for isolated targets is overridden to configured approvals only when enforcement is active, so the container isolation remains a second boundary rather than a reason to skip policy.

## Interfaces and Dependencies

`mj_core::observe::ObserveEnforcementConfig` carries the URL, bearer token, deadline, and failure mode. `PolicyTool` carries one normalized ACP tool call. `mj_worker::observe::PolicyClient::evaluate` returns `PolicyAction::{Allow,Deny,Ask,Warn}`.

Brokk Observe route:

    POST /api/v1/policy/evaluate
    Authorization: Bearer <ingest key>
    Content-Type: application/json

    {
      "session_id": "…",
      "event_id": "…",
      "tool_name": "Bash",
      "tool_input": {"command": "…"},
      "tool_use_id": "…",
      "cwd": "/workspace/repo",
      "harness": "dsh"
    }

---

Revision note, 2026-11-13: Created after implementing the runtime endpoint, DSH capability gate, policy client, spawned permission gate, and the first end-to-end denial test.

Revision note, 2026-11-13: Recorded completed disposition tests, isolated PostgreSQL validation, the unrelated local digest-script failure, and the documented DSH-only rollout.

Revision note, 2026-11-13: Recorded logical/native identity separation, provisional enforcement acknowledgements with permission outcomes, isolated reruns, final Observe validation, Clippy, formatting, and clean SlopCop checks.
