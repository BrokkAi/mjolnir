# Resolve adaptive child effort before launch

This ExecPlan is a living document. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

After this change, an all-model sub-agent spawn can request `effort: "adaptive"`. The daemon sends the child task name, assignment, and selected model to Jev, maps Jev's medium/high/xhigh/max recommendation onto that model's advertised effort choices, then persists and launches only the concrete choice. If Jev is disabled or unavailable, the daemon starts at the offered effort corresponding to `high`; if the model has no effort choices, it leaves effort unset. A person or agent can see the resolved effort in the spawn result and `list_agents`, and can inspect the decision in Jev diagnostics. A single-model session continues to reject effort arguments supplied by its parent agent.

The complete feature also needs a hosted Jev proxy contract and user-facing labels/help. The proxy slice is in commit `a323daf16`; the UI, CLI, and documentation slices are in commits `23b717887` and `bbcbd66f4`. This daemon slice implements their matching contract without editing those owners' files.

## Progress

- [x] (2026-10-08 05:06Z) Read repository and ExecPlan rules; traced policy selection, prepared-spawn persistence, Jev transport/logging, and MCP output paths.
- [x] (2026-10-08 05:06Z) Created this cross-slice plan before implementation.
- [x] (2026-10-08 06:02Z) Add the core effort-verdict contract, bounded brief, parser, question, and offered-choice rung mapping.
- [x] (2026-10-08 06:02Z) Resolve adaptive requests from both daemon spawn entry points, record every outcome, and fall back without failing spawn.
- [x] (2026-10-08 06:02Z) Validate adaptive effort in core and expose resolved effort through MCP spawn and `list_agents` results.
- [x] (2026-10-08 06:02Z) Confirm the proxy and user-facing slices are committed with the matching endpoint, question, label, and documentation; do not edit their owned files.
- [x] (2026-10-08 06:02Z) Run focused core/controller/worker checks, final core and controller suites, and workspace clippy; record outcomes below.
- [ ] Stage and commit only this slice's changed files on the current branch; do not push.

## Surprises & Discoveries

- Observation: the HTTP API and worker MCP requests both call `resolve_subagent_policy_selection`; the MCP path also persists a `PreparedSpawn` before child registration.
  Evidence: `mj-controller/src/server/api/subagents.rs` and `mj-controller/src/server_runtime/api.rs`.
- Observation: durable `SubagentRecord` already stores `effort`, while the MCP spawn response and `list_agents` JSON omit it.
  Evidence: `mj-core/src/subagent.rs`, `mj-controller/src/server_runtime/api.rs`.
- Observation: controller Jev HTTP requests already use a 10-second bounded transport; `[jev].enabled` is in `Config.jev.enabled`.
  Evidence: `mj-controller/src/jev_transport.rs`, `mj-core/src/config.rs`.
- Observation: Jev may also be disabled through `MJ_JEV_DISABLED=1`; the daemon must check that process environment as well as `[jev].enabled`.
  Evidence: `mj-core/src/jev.rs`; the adaptive resolver checks both before classifying.
- Observation: the proxy's question prompt considers the named model, so the direct TypeSafe question must use identical wording.
  Evidence: `services/jev-proxy/src/effort.ts` and `mj-core/src/effort_verdict/questions.json`.
- Observation: the high-rung fallback follows the same positional mapping as a successful Jev choice; with three choices (`low`, `medium`, `high`), it maps to the middle item (`medium`).
  Evidence: `mj-core/src/effort_verdict.rs::map_rung_to_offered` and the controller selection test.

## Decision Log

- Decision: use the canonical Jev ladder `medium < high < xhigh < max`; map onto advertised efforts only after removing `default`, conditionally sorting recognized names, and keeping at most the top four choices.
  Rationale: provider effort lists differ in both names and length, while a single stable four-rung judgment must work across models.
  Date/Author: 2026-10-08 / Codex.
- Decision: preserve fixed-parent rejection of `profile_id`, `model`, and `effort`; adaptive selection is resolved only for policy or spawn values that explicitly request `adaptive`.
  Rationale: a fixed-model session's selectors belong to the user, not the child agent.
  Date/Author: 2026-10-08 / Codex.
- Decision: treat Jev/config/response failures as a logged `high` fallback, and treat an empty advertised effort list as no effort without a Jev request.
  Rationale: advisory effort selection must never prevent a child from starting.
  Date/Author: 2026-10-08 / Codex.
- Decision: treat both `[jev].enabled = false` and `MJ_JEV_DISABLED=1` as disabled; pass a mapped concrete value through `child_effort` before returning the selection.
  Rationale: the worker's Jev switch also applies to daemon-side choices, and the existing child validator remains the single owner of model-offered effort validation.
  Date/Author: 2026-10-08 / Codex.
- Decision: keep the direct TypeSafe question identical to the hosted proxy's question, including its instruction to consider the named model.
  Rationale: both endpoints must make the same judgment for the same state.
  Date/Author: 2026-10-08 / Codex.

## Outcomes & Retrospective

The daemon resolves adaptive effort before child registration and prepared-spawn persistence, records the outcome, and returns only an advertised concrete effort or `None`. An empty effort list skips Jev; disabled, unavailable, invalid, or failed Jev requests warn and map the `high` rung. The direct and hosted questions agree, and proxy/UI commits are present on the current branch. Full core tests and workspace clippy pass. The full controller suite has two repeatable unrelated cache-release assertion failures; adaptive selection, options, and MCP focused checks pass. The remaining work is to commit only this daemon/core/worker slice.

## Context and Orientation

`mj-core/src/subagent.rs` defines `SubagentPolicy`, validates model/effort options, and owns the `ADAPTIVE_EFFORT` marker. `mj-core/src/effort_verdict.rs` defines the shared Jev v1 request/response contract and pure mapping from Jev rungs to a model's `SessionConfigChoice` list. The controller's `server/api/subagents.rs` resolves the effective profile/model/effort for HTTP and MCP spawns. Its caller in `server_runtime/api.rs` stores `PreparedSpawn`, registers the child, configures its first turn, and formats MCP results. `mj-controller/src/github_item_verdict.rs` and `jev_transport.rs` provide the bounded Jev client and diagnostic-log patterns. `mj-worker/src/subagent_mcp.rs` defines the worker-facing `spawn` schema and preserves the fixed-parent argument refusal.

The proxy accepts a bounded hosted state containing `task_name`, `instructions`, `model`, and `instructions_truncated`, then forwards `{model: "jev-latest", state, questions}` to the TypeSafe endpoint. Its response exposes `answers.effort.choice`, confidence, and probabilities. The daemon must accept a valid choice only from `medium`, `high`, `xhigh`, and `max`. The user-facing slice labels the option `Adaptive (Jev chooses per task)` and documents it only for all-model sessions.

## Plan of Work

First add `mj-core/src/effort_verdict.rs` and export it from `mj-core/src/lib.rs`. Keep the v1 question JSON with this module and semantically identical to the hosted proxy question. Define a UTF-8-safe bounded evidence type: task name and model are at most 256 UTF-8 bytes, instructions retain at most the first 24 KiB and last 8 KiB with a truncation marker/flag, and hosted and direct serialized bodies never exceed 64 KiB. Parse the one `effort` answer and validate its choice, confidence, and four probabilities. Implement the pure mapping rule: remove case-insensitive `default`; sort by `minimal, low, medium, high, xhigh, max` only if all remaining names are recognized, otherwise preserve advertised order; discard the bottom choices if more than four remain; map rung `r` to `round(r * (n - 1) / 3)`, with no result for an empty list. Add the single ordering/mapping test required by the repository test rules.

Next update `SubagentOptions::validate` so `adaptive` is valid exactly when the selected model offers at least one configurable effort. In controller selection, detect `adaptive`, resolve it with the already-fetched offered list and task brief, then pass the concrete result through `child_effort`. Cover all-model spawn arguments and a stored single-model adaptive policy. Read `[jev].enabled` off the async path and also honor `mj_core::jev::disabled_by_environment()`. If the list is empty, do not call Jev and leave effort unset. Otherwise choose direct TypeSafe when a key is available, or the hosted `/v1/effort-verdict` endpoint when it is not. On disabled Jev, transport/config failure, timeout, invalid body, or invalid answer, warn and map `high`; never return a Jev failure to the spawn caller. Persist and pass only the mapped concrete effort.

For every adaptive request, append a `DecisionLog` record of kind `effort-verdict` scoped to the child task name. Attach the actual bounded request and response when available, and identify the source/contract. The answer records the selected rung, mapped effort, and fallback status; the action says `child starts at <effort>` (use `default` when no effort is offered). Logging failures warn and do not block spawn. Extend MCP schema text to say that all-model callers may pass `adaptive`, include the resolved effort in the spawn response and each `list_agents` entry, and leave the fixed-parent guard unchanged.

The proxy-owned slice must serve `POST /v1/effort-verdict`, enforce the exact bounded hosted request, forward the fixed question through the existing TypeSafe URL, and return only the validated effort answer. The surface-owned slice must add the adaptive label and documentation while keeping the option unavailable for fixed-parent spawn arguments. Coordinate against these interfaces without editing those owners' files.

## Concrete Steps

Run commands from the repository root `/home/jonathan/Projects/mjolnir/.mj/clones/96f321a675e77a6fcd31abbcc11344d7`. Do not redirect Cargo output or alter Cargo/mbx configuration. Rust tests must run outside the restricted sandbox with elevated permissions and use the development profile. Focused checks used `cargo test --manifest-path mj-core/Cargo.toml effort_rungs_map_after_default_removal_ordering_and_ladder_capping`, `cargo test --manifest-path mj-controller/Cargo.toml a_spawn_checks_effort_against_the_model_it_names`, `cargo test --manifest-path mj-controller/Cargo.toml an_unavailable_model_or_effort_is_refused_with_its_message`, and `cargo test --manifest-path mj-worker/Cargo.toml initialize_and_tool_list_deliver_concise_parent_and_child_contracts`. After the last code change, run the full core and controller manifests and workspace `cargo clippy --all-targets -- -D warnings`. Keep each command on the normal shared Cargo/mbx cache.

## Validation and Acceptance

The pure mapping test exercises default removal, recognized-name sorting, dropping the lowest entries from lists longer than four, and rung mapping for two- and three-choice lists. Existing selection tests establish that adaptive is converted before `child_effort` can reject it, disabled Jev maps the high rung, and a model without effort choices starts with no configured effort. The MCP schema/result path exposes `effort`, while fixed-parent tool definitions and rejection remain unchanged. Proxy route tests and user-facing changes are owned by their respective slices; their test evidence is outside this daemon-slice report.

Acceptance is that an adaptive spawn starts with a concrete advertised effort, or with the harness default when no effort is offered; Jev failures start the child at the mapped `high` rung. No child launch or persisted prepared request contains the literal `adaptive`. `list_agents`, the spawn result, and Jev diagnostics reveal the resolved outcome. Final core tests pass (477 unit tests and 2 Jev scenario tests, with 1 ignored doctest), and workspace clippy passes. The final controller suite has 1,941 passing and 10 ignored tests plus two repeatable cache-release assertion failures in existing lifecycle tests. The readiness and mbx deadline failures from an earlier suite run passed individually on retry.

## Idempotence and Recovery

The work adds no database columns or migrations; prepared spawn and child records already serialize an optional concrete effort. Re-running tests uses their existing isolated configurations. If an implementation step fails, keep the last compiling checkpoint, revise this plan's progress and decision log, and rerun only the failed test before final validation. Do not touch another agent's files or stage concurrent changes.

## Artifacts and Notes

The shared question key is `effort`. Choices are `medium`, `high`, `xhigh`, `max` in that order. Hosted requests are at most 64 KiB; `task_name` and `model` are each at most 256 bytes; instructions retain a 24 KiB head and an 8 KiB tail at most. Direct requests use the existing TypeSafe URL and `{model: "jev-latest", state, questions}` envelope. The v1 hosted endpoint is `https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/effort-verdict`.

Final evidence is in `.mj/agents/7c091ccc399af27d2b5a874bb911b8c2/mj-core-full-final2.log`, `.mj/agents/7c091ccc399af27d2b5a874bb911b8c2/mj-controller-full-final2.log`, `.mj/agents/7c091ccc399af27d2b5a874bb911b8c2/cargo-clippy-final.log`, and focused logs in that same report directory. The repeatable controller failures are `controller::lifecycle::tests::destroying_a_managed_clone_releases_its_mbx_build_state_once_the_checkout_is_gone` and `controller::lifecycle::tests::removing_a_cached_container_releases_its_workspaces_from_the_shared_cache`.

## Interfaces and Dependencies

`mj_core::effort_verdict` exposes the shared rung names, v1 questions, bounded evidence/upstream serialization, typed response parsing, and pure `map_rung_to_offered` helper. The controller resolver receives the task name and instructions alongside the already-discovered offered list and returns `Option<String>` containing only a concrete offered effort. It uses `crate::jev_transport::post_bounded_json`, `mj_core::activity::verdict::api_key`, and `mj_core::jev::DecisionLog`. The worker schema sends the string `adaptive` only in all-model spawn requests; the daemon's response carries the concrete result.

### Current revision note

This revision records the implementation and its final validation, including environment-level Jev disable handling, shared direct/hosted question wording, concrete-effort validation after mapping, the two repeatable unrelated controller failures, and the completed proxy and user-facing commits. The daemon implementation commit remains to be made.
