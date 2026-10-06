# Four-choice Subagents control

This ExecPlan follows `.agents/PLANS.md` and is maintained throughout implementation.

## Purpose / Big Picture

Claude and Codex new-session wizards offer Native, Mjolnir all models, Mjolnir single model, and None. Single-model parents select model and corresponding effort from eligible profiles, receive a reduced MCP interface with fixed spawn selectors, and receive the user's delegation guidance. Claude/Codex Mjolnir children cannot delegate natively. The last accepted top-level choice is shared per instance across TUI and web.

## Progress

- [x] Inspected existing creation, profile discovery, MCP, staging, and persistence paths; agreed scope and defaults with user.
- [x] (2026-09-27) Implement shared policy, persisted-data compatibility, migration, and remembered default.
- [x] (2026-09-27) Implement shared discovery, enforcement, worker behavior, and staged guidance.
- [x] (2026-09-27) Implement TUI, web, and CLI controls.
- [x] (2026-09-27) Review changes; formatting, Clippy, browser checks, and final runtime/CLI tests pass.
- [x] (2026-09-27) Finish full-workspace verification: all unit suites passed; one late integration startup timeout passed in an isolated rerun. The final runtime/CLI run also passed that complete integration suite.
- [x] (2026-09-27) Commit as `4c03a8d5`, merge to master as `389a1666`, and push as explicitly requested while the slow tests run.

## Surprises & Discoveries

The old native suppression predicate depends on the MCP socket, so None requires an explicit launch policy. The profile catalog's initial efforts describe a profile's default model; single-model selection must discover capabilities for the selected model. Children can use other harness profiles, but this change is restricted to existing Claude/Codex delegation integration by the user's explicit scope correction.

## Decision Log

- Native is the initial default; subsequent accepted top-level launches persist their policy per instance. Canceling a wizard does not save it. Child creation and resume do not save it.
- Single-model MCP retains interrupt, but omits list_profiles and spawn's profile/model/effort fields. Both daemon spawn paths enforce the stored selection.
- All Claude/Codex Mjolnir children disable native delegation, including all-model children. No new harness integrations.
- The migration is breaking because an older binary cannot honor None or fixed selectors. Retain all shipped forward migrations and raise the compatibility floor atomically. Schema revision and minimum compatible revision are 56; daemon protocol is 39.
- Runtime snapshots and the terminal runtime feed carry the durable remembered policy, so an already-open terminal observes choices accepted from the web. The creation transaction owns this preference; clients do not infer it from session order.

## Outcomes & Retrospective

Implementation and diff review are complete. Clippy and formatting pass. Browser unit tests passed; the full browser run passed 112 cases with one obsolete checkbox assertion, then all 41 session-creation tests passed after correcting that assertion. Initial Rust failures exposed old implicit-native fixtures and migration equality expectations; corrected these without changing native behavior. A later full run hit an unrelated relay-fixture timing assertion (broken pipe instead of disconnect). The final runtime/CLI run passes, including all 1,852 controller tests and isolated daemon startup/upgrade tests. The full-workspace run subsequently passed all unit suites but hit a late timeout in `concurrent_starts_wait_for_controller_ownership_before_launching`; that exact test passed alone in 1.98 seconds. Its complete integration suite had also passed in the final runtime/CLI run. The user explicitly authorized the optimistic merge and push. The pushed merge also passes formatting and `cargo clippy --all-targets -- -D warnings`. Browser coverage is 51 unit tests plus 112 tests in the initial full run and all 41 session-creation cases on the corrected test.

## Context and Orientation

`mj-core/src/subagent.rs` owns shared contracts. Session creation crosses `mj-tui`, `mj-cli`, `mj-client`, and `mj-controller`; durable session records live in the controller's SQLite database. `mj-controller/src/server_runtime/profile_catalog.rs` owns eligible profile discovery and `mj-controller/src/server/api/subagents.rs` resolves spawn selectors. `mj-worker/src/subagent_mcp.rs` exposes tools; `mj-worker/src/acp/launch.rs` configures harness tool suppression. Controller worker staging copies profile instructions into each session's private home. The TUI control is in `mj-tui/src/wizards/`, and the web control is in `mj-controller/src/web/viewer.js`.

## Plan of Work

Milestone 1 replaces internal booleans with a shared tagged policy and accepts old booleans only at serialization boundaries. Persist JSON policy in a new session column and a durable singleton remembered preference. Map true to AllModels and false/missing to Native. Explicit recipes retain their own choice. Add CLI flags and reject legacy flags and public API parameters entirely, per the user’s follow-up. Keep compatibility decoding only for persisted data and internal handoff.

Milestone 2 reuses eligible profile discovery for pre-session choices and model-specific efforts, with background loading and stale-response rejection. Validate the exact model/effort pair at creation and spawn. Choose the existing quota-ranked eligible profile supporting that pair; do not substitute unavailable selections. Expose the fixed-parent MCP role without selector arguments, reject hidden tools and override arguments, and enforce policy in the daemon. Derive native suppression from launch policy independently of handback sockets. Append the exact guidance below only to single-model parent staged instructions, interpolating configured max_concurrent. Repeated staging must not duplicate it or change source files.

Milestone 3 replaces both checkboxes with four-choice inputs. Single-model mode reveals model and effort. Model-less or invalid selections block submission with a useful error; models with no effort selector use harness default, never parent effort. Explain profile setup with Settings → Profiles and Settings → Sub-agents. Persist accepted choices, share them through snapshots, and preserve them across restart and recovery.

## Concrete Steps

Work in `/home/jonathan/Projects/mjolnir2`, on the current branch. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Cargo tests must run elevated outside the restricted sandbox with existing build storage unchanged. Run `npm test` in `tests/e2e/web`. Use isolated test data and `--instance subagent-selection` for any manual new-build CLI/daemon/TUI invocations. Never migrate the live instance.

## Validation and Acceptance

Behavior tests cover all four choices, model-dependent efforts, unavailable profiles, stale discovery, remembered defaults, cancellation, recipe and legacy compatibility, tool discovery and rejected overrides, quota selection, native suppression with handback preserved, exact instruction text and repeat staging, and forward migration with old-reader rejection. Existing isolated startup/worker handoff regressions must remain green. Review the diff, stage only changed files, commit validated changes on the current branch, and do not push.

## Idempotence and Recovery

Database changes are forward-only and transactional. Existing parents preserve their choice. Active workers are not interrupted to change native tools: apply child suppression through ordinary idle worker replacement. Instruction append happens only in a fresh private staged profile. Capability failures remain visible and never select another model silently.

## Artifacts and Notes

Exact user-provided instruction text (replace only `$N`):

Delegation policy: this session has Mjolnir sub-agent tools (spawn, wait, list_agents, send_input, close); use them. Treat the subagent as a strong senior engineer, between Sonnet and Opus. You can have up to $N subagents at a time.

Here are some suggestions on using subagents to best effect:
    For research activities, spawn subagents in parallel, one question each, with relevant files excerpts if you have them. Do not read the repository yourself for a question you have assigned; read the report, then the files it names.
    A sub-agent's report is short (Mjolnir caps it) and points to files in the sub-agent's report directory. This is deliberate, to keep irrelevant details out of your context. Read those files as needed.
    Delegate builds, tests, and lint runs to a sub-agent: "run these; fix obvious problems; report only failures, with test names, a one-line reason and the log path each". Rerun nothing yourself that a sub-agent ran green; rerun only what it reports failed, if you take ownership of fixes yourself.
    Plan the next step, then call wait without arguments. It watches all children that are not stopped and returns the reports that became ready since the last wait. A wait may end before work is done, and another wait is normal. Call it again when more reports are needed. Do not investigate in parallel what a child is investigating; every request carries your whole context.
    Keep for yourself: the design, the review of the integrated diff, the commit, the final report, and any decision a sub-agent hands back. When a sub-agent hands back a question, answer it with send_input.
    Dispatch several sub-agents in parallel whenever the work splits (separate investigations, disjoint files, separate suites). Sub-agents share your container and checkout: give concurrent sub-agents disjoint files and say so. You can also create separate worktrees if that's a better fit.
    After you have read a sub-agent's report, close it unless you will re-task it: idle sub-agents hold process slots in the container. Re-using a subagent for a followup with related work will save effort and time over starting fresh. Conversely, if you have a separate task, start a fresh subagent.

## Interfaces and Dependencies

Use `SubagentPolicy::{Native, AllModels, SingleModel { model, effort }, None}` in shared core contracts, with tagged JSON and boundary-only legacy boolean decoding. Add pre-session subagent options keyed by parent profile and optional selected model. Reuse ProfileConfig discovery, quota ranking, the existing daemon admission machinery, and background UI tasks; add no workspace crates or dependencies.

Revision: user requested removal of legacy CLI and public API parameters. The pre-session discovery service reuses cached, supervised model-specific profile probes for both UIs and daemon creation; spawn retains the daemon’s quota-ranked catalog.

Revision (2026-09-27): completed runtime preference propagation, advanced the daemon protocol for the new wire fields, and added explicit migration coverage for all legacy boolean states and rejection of older writers.

Revision (2026-09-27): user authorized merging and pushing before the remaining slow tests finish. Preserve the active test run and report any late failure.

Final verification (2026-09-27): merge conflict was limited to independent tests appended to `mj-controller/src/controller/worker_binary/tests.rs`; both additions were preserved. No live instance was migrated. Existing untracked files in the master worktree were left untouched. No implementation work remains.
