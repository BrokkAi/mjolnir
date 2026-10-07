# Restore Codex async questions through ACP (#1265)

This ExecPlan follows `.agents/PLANS.md` and remains a living record of implementation and validation.

## Purpose / Big Picture

When Codex accepts `request_user_input_async`, Mjolnir must show the questions, keep them available after the turn ends, and deliver option or free-text answers to the same session. The Codex ACP bridge translates Codex app-server events into Agent Client Protocol (ACP), which Mjolnir workers consume. The fix must preserve guardian and yolo execution modes while updating the bridge from upstream.

## Progress

- [x] (2026-10-07) Claim #1265 and locate `/home/jonathan/Projects/codex-acp`.
- [x] (2026-10-07) Fetch upstream `origin/main` (4583cb3) and merge into the existing `main` branch in bridge commit 184224f.
- [x] (2026-10-07) Identify async questions in the `questions` field of agent-message items, currently omitted from ACP output.
- [x] (2026-10-07) Resolve and validate the upstream merge while retaining BrokkAi extensions (228 affected tests and typecheck/build).
- [x] (2026-10-07) Translate async agent-message questions into existing ACP forms; deliver replies through native atomic `turn/start` while the session lifetime owns the form.
- [x] (2026-10-07) Cover late answers, duplicates, free text, and visible delivery errors in bridge snapshots. Extend the worker protocol regression and TUI golden with recorded ACP forms; both pass. Existing durable projection and replay tests remain applicable because the wire/data shape is unchanged.
- [x] (2026-10-07) Bridge full validation: 1,226 tests passed, 33 skipped, plus typecheck and build. Prepare version 1.13.6, with Codex 0.160.1.
- [x] (2026-10-07) Real source-level Guardian validation: question visible after end_turn; late option answer starts the continuation; successive grant writes succeed; one escalation is automatically reviewed.
- [x] (2026-10-07) Exact-tarball real Codex checks in isolated homes passed in Guardian and yolo: late selected/custom answers, successive grant writes, and Guardian automatic review. The worker byte-stream integration separately proves Mjolnir accepts a late answer.
- [x] (2026-10-07) Publish bridge 1.13.6 (3789fde); npm release workflow 37685534371 succeeded and the registry serves the release. Update runtime/package/container pins to bridge 1.13.6 and Codex 0.160.1, regenerating the npm lockfile.
- [x] (2026-10-07) Full core/worker/chat suites passed: 470 core, 411 chat, and 662 worker module tests, plus all integration binaries.
- [x] (2026-10-07) Workspace `cargo clippy --all-targets -- -D warnings` passed on the dev profile; formatting and diff checks also passed.
- [ ] Commit and push Mjolnir, and confirm agent-dev image publication.
- [ ] Install the pushed checkout locally with `scripts/install.sh`, as requested on 2026-10-07, and verify the installed version and daemon readiness.

## Surprises & Discoveries

Upstream is now 2.1.1 and depends on Codex 0.160.1, while the BrokkAi adapter identifies itself as 1.13.5. The merge overlaps downstream session observation, goal control, tool filtering, workspace grants, and subagent cancellation. These behaviors must be preserved while adopting upstream pagination and reporting changes.

The worker can publish `PromptFinished` before its asynchronously spawned elicitation task publishes `ElicitationRequested`. The regression must accept both event orderings and ensure the question stays answerable after completion. The original test discarded the earlier completion and incorrectly timed out; the corrected test observes completion on either side of the question.

Codex's own `request_user_input_async` implementation emits only started/completed agent-message items with `delivery: "async"`, `questions`, and full text. The bridge previously ignored completion text and never requested a form. Native `turn/start` invokes `start_or_steer_turn` in Codex, so replies need no adapter-side turn-state check or retry.

## Decision Log

Decision: use the existing bridge checkout on `main` and push only to `brokkai`; keep Mjolnir on `master` and commit/push each issue separately. Rationale: the user requested the upstream bridge merge and separate pushes, and both repositories require committing on the existing branch. Date/Author: 2026-10-07, Codex.

Decision: use ordinary ACP `elicitation/create` and the existing worker-owned pending map, with no new persisted fields, migrations, or protocol revision. Rationale: Mjolnir already retains and answers forms after turn completion. Form waits run outside the notification queue and use the session cancellation signal, independently of a prompt's lifetime. Codex alone decides whether reply input joins or starts a turn. An explicit failure is shown if delivery fails; arbitrary mutations are never retried after uncertain acknowledgement. Date/Author: 2026-10-07, Codex.

## Outcomes & Retrospective

The #1266 doctor fix was committed as 5355fc2c and pushed after merging concurrent upstream documentation. Core/controller suites and workspace clippy passed. For #1265, upstream merge 184224f and bridge implementation 3789fde are pushed to brokkai/main, and version 1.13.6 is published. Exact-tarball Guardian/yolo checks passed. The Mjolnir protocol regression and TUI golden pass; matching pins are updated and full core/worker/chat validation passed. Workspace clippy, formatting and diff checks also passed. The final Mjolnir push/image publication and requested local installation remain. No ACP schema, stored data, or production UI code changes are required because the bridge now uses the existing form mechanism.

## Context and Orientation

The bridge's `src/CodexEventHandler.ts` processes `item/started`, `item/completed`, and `turn/completed`. Generated `src/app-server/v2/ThreadItem.ts` defines agent messages with `delivery` and `questions`. Synchronous questions already flow through `src/CodexElicitationHandler.ts` using ACP `elicitation/create`. In Mjolnir, `mj-core/src/elicitation.rs` models questions, `mj-core/src/acp.rs` receives bridge requests, and the worker's relay journal preserves them independently of daemon liveness. The controller (the background daemon managing sessions) projects pending questions into the terminal UI (TUI) and web viewer. Inspect the existing completion and response semantics before choosing how to represent asynchronous questions; async answer delivery must not block notification processing or clear a question merely because a turn ends.

## Plan of Work

Milestone 1: merge upstream and preserve downstream behavior.

First reconcile each bridge merge conflict, retaining downstream ownership and execution rules. Adopt upstream session configuration results with both tool restrictions and skipped MCP-server information. Preserve long-lived session event subscriptions and execution-state notifications. Validate with typecheck and affected tests, then commit the upstream merge as a coherent checkpoint.

Milestone 2: make questions visible and answerable across turns.

Next trace real async agent-message events and determine the native answer mechanism from the local Codex source and app-server README. Translate questions into the existing ACP forms if their response semantics fit; otherwise define the smallest explicit extension with one worker-owned state transition and backward-compatible decoding. Exercise option answers and free text, and ensure the pending record survives turn completion and daemon replay. Extend existing golden/protocol tests instead of adding ordinary computation examples.

Milestone 3: publish the bridge and adopt it in Mjolnir.

Finally validate the packaged bridge in guardian and yolo, publish through `docs/RELEASES.md`, and update Mjolnir's `mj-core/src/harness_runtime.rs`, `mj-worker/assets/harnesses/codex/package.json` and lockfile, and `containers/Containerfile.agent-dev` together. Check the image-publication workflow after pushing.

## Concrete Steps

In `/home/jonathan/Projects/codex-acp`, the merge is already committed. Run `npm ci`, `npm run typecheck`, affected Vitest files, then the full `npm test` and `npm run build` after any further logic changes. Use `.claude/skills/run-codex/SKILL.md` for live event evidence with a copied harness home. Package and install the tarball under `/mnt/optane/tmp/mj-fix-1265-live/package`; run packaged guardian and yolo validation before tag publication. The exact tarball checks returned `ANSWER_ACCEPTED Blue` in Guardian and `ANSWER_ACCEPTED Magenta` in yolo after an `end_turn` while the form stayed open. The isolated homes/workspaces live outside the checkout because downloaded plugin templates contain TypeScript and would otherwise enter the upstream tsconfig's default source scan.

In the Mjolnir clone, run filtered `cargo test -p <touched-crate> <filter>` outside the sandbox during iteration. After the last edits run `cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-chat` and `cargo clippy --all-targets -- -D warnings` on the dev profile, both outside the sandbox. Run `cargo fmt --all -- --check` and `git diff --check`. The named regressions are `form_elicitation_is_advertised_rendered_and_answered` and `golden_elicitation_dialog`. Every live binary uses `--instance fix-1265` and isolated `MJ_CONFIG_DIR`/`MJ_DATA_DIR`; never use the host default instance or real profile home.

## Validation and Acceptance

The bridge regression shows an ACP question request for a completed async agent message and an answer delivered to the corresponding Codex thread, including a turn that finishes before the user answers. Mjolnir regressions show the question still answerable after `EndTurn`, rendered with selectable options and free text. Existing resolution/replay semantics apply unchanged; a native answer-delivery failure must be visible. Real Codex validation must observe the answer on the running or next turn, and guardian/yolo behavior must remain correct. Required Rust suites, bridge tests, build, and clippy must pass before the final push.

## Idempotence and Recovery

Existing untracked bridge tarballs are user artifacts and remain untouched. Resolve the current merge in place; do not create or switch branches. Publish only a new unused version tag and never move an existing tag. If upstream advances before a push, fetch and merge it on the current branch, then validate the affected changes. Live test processes must be stopped before deleting their isolated files.

## Artifacts and Notes

The starting bridge merge reported conflicts in package identity/version files, `CodexAcpClient.ts`, `CodexAcpServer.ts`, `CodexEventHandler.ts`, session fork/metadata, goal snapshots, approval handling, and overlapping tests. This is evidence that accepting one side wholesale would lose required behavior.

## Interfaces and Dependencies

Use the existing ACP SDK, Codex app-server types, and Mjolnir question/projection machinery. Do not create a workspace crate, rewrite database migrations, or redirect Cargo target storage. Check the supported Codex version as part of bridge publication and update Mjolnir's exact Codex pin if the merged adapter requires it.

Initial plan created after the upstream merge revealed substantial conflicts and the async question event shape.

Updated after implementation to record the completed merge, native ownership of answer admission, event-ordering discovery, validation evidence, and unchanged ACP/store shapes.

Updated after exact-tarball Guardian/yolo validation and successful bridge publication to record the published version, matching pins, unchanged native hook format (0.159.1 and 0.160.1 discovery sources are identical), and remaining Mjolnir validation/publication.

Updated after all touched-crate suites passed to retain the validation counts and narrow the remaining work to clippy and publication.

Updated for the user's request to install locally after pushing. Use the repository installation script, which builds native and portable workers plus the controller and voice helper through the existing mbx setup and replaces installed binaries by rename.

Updated after workspace clippy passed to mark implementation validation complete and retain the remaining push, image publication, and local installation steps.
