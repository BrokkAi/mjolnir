# Let Jev classify transient server failures before retrying

This ExecPlan is a living document and follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current through implementation.

## Purpose / Big Picture

Mjolnir currently retries one Codex capacity error detected with a provider code or an exact final message. A user should instead see automatic recovery from any transient provider failure that Jev confidently recognizes, across harnesses, while errors needing user action remain visible. The worker must retain ownership of the retry and continue it when the daemon is absent.

## Progress

- [x] (2026-09-23 14:33Z) Inspected existing capacity retry, Jev v3 classifier, proxy, wait API, and upgrade constraints.
- [x] (2026-09-23) Added frozen v3 questions, a v4 retryability question, and bounded completion evidence.
- [x] (2026-09-23) Replaced capacity detection with durable, generation-checked retry assessment and generalized retry presentation.
- [x] (2026-09-23) Rust workspace tests, clippy, proxy tests/check/dry-run, and web unit tests passed; deployed v4 as `bd69b6cf-199b-4e41-96bb-cde8e3d05cbd` and smoke-tested v4 plus all six older routes.
- [x] (2026-09-23) Recorded the outcome and staged only files changed for this work for a commit on `hel2`.

## Surprises & Discoveries

- The hosted Jev proxy validates but does not deploy in GitHub Actions. User authorization in this conversation includes deploying the new route.
- The current worker records `capacity_retry` in relay state and exposes it to clients; preserving the old field while adding generic presentation prevents older readers from losing an armed retry.
- The existing retry timer had a Codex-only gate even though the assessment path was generic. Removed the gate and added a test for all five harness policies.
- The proxy bounds strings by UTF-8 bytes. Diagnostic evidence now uses byte boundaries in Rust so non-ASCII provider messages cannot invalidate otherwise valid requests.
- The first unconstrained workspace test run overloaded several coordinator timing tests; the worker package passed with four test threads. The new paused-clock timer test also needed a setup wait that does not advance Tokio's mock clock.
- `ModelCapacity` was still mapped to a quota outcome and used to arm a retry during journal projection. New completions now treat it as an ordinary error for Jev; historical stored retry state remains readable.
- The new `server-retry-*` command ID initially bypassed the generated-prompt helper, which would have made an automatic retry appear to renew user authorization. The retry test now asserts the original user command stays attached.
- An isolated daemon upgrade test used a `2.15.0` fake daemon while expecting the atomic handoff introduced in `2.18.0`. The fixture now advertises `2.18.0` against the same older database schema so it exercises its intended contract.
- Initial synthetic text-only capacity examples scored below the 0.90 threshold. The v4 question was calibrated with the provider while retaining low scores for quoted examples, quota, authentication, and local errors; four final repeats scored 0.91–0.94.

## Decision Log

- Decision: Jev classifies transient provider outages, overload, and short throttling for all harnesses, including failures expressed only in final reply text. Rationale: This is the user's selected scope. Date/Author: 2026-09-23, user and Codex.
- Decision: A Jev failure or score below 0.90 leaves the error visible; retries send `Continue` using existing 1/2/4/8/16-minute backoff. Rationale: These are the selected behavior and safe replay semantics. Date/Author: 2026-09-23, user and Codex.
- Decision: Add `/v4/turn-verdict` while leaving v1–v3 unchanged, deploy proxy before enabling the new hosted worker. Rationale: Existing installed workers require their frozen contract. Date/Author: 2026-09-23, Codex.

## Outcomes & Retrospective

The worker now records a completed-turn assessment, asks Jev v4 about transient provider failure, and arms a durable retry only at score ≥0.90. It accepts structured errors and final reply text from all five harnesses, preserves the user's original authorization across automatic retries, and survives worker reopening. New completions no longer trigger a retry through `ModelCapacity` or `server_overloaded` comparisons. Clients see generic retry and checking status, while legacy wire fields remain readable.

`cargo test -q -- --test-threads=1` passed across the workspace, including the isolated automatic-upgrade fixture after its fake daemon version was corrected. `cargo clippy --all-targets -- -D warnings`, the proxy's TypeScript check and tests, Wrangler's dry run, and all 43 web unit tests passed. The final question-only edit passed focused core tests and a fresh clippy run. The deployed proxy is version `bd69b6cf-199b-4e41-96bb-cde8e3d05cbd`; synthetic live requests verified v4 retryability and every earlier route. Jev probabilities can vary across inputs, so the worker still abstains whenever the current answer is below 0.90 or unavailable.

## Context and Orientation

`mj-worker/src/acp/session.rs` converts harness prompt results to runtime events. `mj-worker/src/relay/commands.rs` records those events in the durable relay journal, and `mj-core/src/relay/snapshot/apply.rs` projects journal records into worker state. `mj-worker/src/worker_runtime/unix/dispatch.rs` owns background Jev requests and retry timers. `mj-worker/src/acp/verdict_client.rs` calls Jev; `services/jev-proxy/src/index.ts` supplies the hosted endpoint. `mj-controller/src/server/api/wait_policy.rs` decides whether a caller should keep waiting. The daemon is the control plane; the worker survives daemon handoff.

## Plan of Work

Freeze v3 questions, add a v4 retryability question and a bounded completed-turn error payload to Jev evidence. The answer is a probability, not a command. The worker accepts it only at 0.90 or above, after checking that the same completed command remains current. Remove the exact capacity-message and `server_overloaded` comparisons; preserve the provider's diagnostic code and message as classifier evidence.

Record pending classification durably with the completed turn, then resolve it through a journal event. The wait API must not report a final result during that bounded assessment. A worker restart resumes a pending assessment from durable evidence; a stale verdict cannot arm a retry after newer user or control work. Failed or uncertain classification resolves to no retry. A confident verdict arms the existing durable backoff and submits `Continue` when due and quiet. Older capacity state and stop reasons remain readable but new completions no longer produce them.

Add `/v4/turn-verdict` to the hosted proxy and its fixed request/answer validators. Keep v1–v3 behavior frozen. Generalize client status text and transcript labels, while retaining the old wire field as a compatibility alias. Update `.agents/docs/jev-proxy.md` with the route and deployment result.

## Concrete Steps

Work in `/home/jonathan/Projects/hel2`. Implement small, testable edits in the core verdict schema, worker relay/coordinator, proxy, and display surfaces. Use `cargo fmt --check` or `cargo fmt` only after code edits. Run `cargo test` outside the restricted sandbox with elevated permissions and run `cargo clippy --all-targets -- -D warnings` on the dev profile. Run `npm ci`, `npm run check`, `npm test`, and `npm run deploy:dry-run` under `services/jev-proxy/`. After all local checks pass, run `npm run deploy` in that directory, then synthetic HTTP smoke checks for v4 and legacy routes. Commit the changed files on branch `hel2`; do not push.

## Validation and Acceptance

Focused tests must show high-confidence Jev verdicts schedule one retry from structured and text-only server failures in multiple harnesses; quoted examples, quota, auth, cancellation, local errors, stale decisions, Jev outage, and low confidence do not. A restarted worker must recover a pending decision or armed retry without duplicate submission. The wait API must remain pending during classification and backoff. Proxy tests must prove v4 validation and that v1–v3 retain their current exact contracts. Synthetic production smoke checks must return a typed v4 answer and successful legacy answers without sending real session content.

## Idempotence and Recovery

The retry command ID is derived from the durable completion ordinal so duplicate timer wakes do not resubmit. Jev assessment resolution checks command identity and completion ordinal. Proxy deployment is additive: rollback to the previous Cloudflare version restores legacy routes while new workers fail closed on v4 errors. Keep the deployed version ID in `.agents/docs/jev-proxy.md`.

## Interfaces and Dependencies

Use the existing TypeSafe `jev-latest` model and the existing 10-second Rust and 8-second proxy deadlines. Add a v4 answer named `retryable_server_error` as a Noul probability in [0,1]. Keep the worker's retry state durable and its HTTP call in supervised background work; never await Jev in the TUI, web render loop, or daemon request handler. No new database table or SQL migration is planned; any relay schema change must remain forward-readable from shipped snapshots and be covered by isolated upgrade regressions.
