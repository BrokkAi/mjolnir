# Keep a raw session's bundle when it resumes onto a workspace target

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

In Mjolnir a session works on a bundle (the repositories it covers, and their identity) inside a checkout (the directory where the code lives, and who owns it). A raw session is a bundle of one repository with an attached checkout: a directory the user owns. Resuming or moving a session to another target should change only its checkout.

Today one path breaks that. When a raw session resumes onto a workspace target (a container or remote clone), Mjolnir builds a new bundle from the live checkout's path and folder name, often named `import-<name>-N`, and moves the session's history onto it. A session imported with `mj import ... --bundle bifrost` therefore stops being a `bifrost` session the first time it moves to a container.

After this change the session keeps its bundle ID and bundle snapshot. Mjolnir adds the network source the workspace needs (the checkout's network remote) to the session's own snapshot, and restores the checkout's local commits and uncommitted changes onto a clone of that same bundle. To see it: import a raw session with `--bundle <name>`, resume it onto a container target in an isolated instance, and observe that `mj sessions --session <id>` still names `<name>` as its bundle, no new bundle appears in the configuration, and the session's prompt history is unchanged.

## Progress

- [x] (2026-10-07) Research: both conversions mapped; findings in Context below.
- [x] (2026-10-07) Milestone 1: raw-to-workspace planning now uses the accepted session bundle; resume and move keep its identity and repository definition.
- [x] (2026-10-07) Milestone 2: controller suite, workspace clippy, formatting check, and isolated session-move e2e pass. This task explicitly excludes commit and push.

## Surprises & Discoveries

- Observation: `WorkspaceToRaw` already keeps `bundle_id` and the project snapshot and only creates a managed local worktree or clone; it is already a checkout-only transition.
  Evidence: `mj-controller/src/controller/worktree.rs:204-273, 793-801`.
- Observation: the bundle synthesis is deliberate, not a missing lookup: `plan_raw_to_workspace_with_checkout` builds its plan from the live checkout only, and `planned_bundle.save_to` reuses a configured bundle only if its single repository has exactly the checkout's local path and folder-name destination.
  Evidence: `mj-controller/src/controller/worktree.rs:825-877, 899-967`; `mj-controller/src/controller/resume.rs:191-217`.
- Observation: accepted snapshot identities can predate the checkout's network remote and therefore be local-path identities. The planner accepts that legacy identity only when its path matches the checkout's canonical source repository; it still stores and validates the resolved network identity for the new workspace source.
  Evidence: `mj-controller/src/controller/worktree.rs:930-967`.
- Observation: carrying the accepted snapshot made the `ResumeConversion` raw variant much larger than the local-worktree variant. Boxing that payload keeps the enum compact without changing its behavior.
  Evidence: workspace `cargo clippy --all-targets -- -D warnings` passed after boxing; see the validation log in Artifacts.

## Decision Log

- Decision: keep the session's `bundle_id` and accepted `project` snapshot across raw-to-workspace resume and move. Add the checkout's resolved network remote to the session's `project.network_sources`, and map the conversion archive onto the bundle's own repository ID and destination.
  Rationale: the only thing the raw bundle lacks for a workspace is a network source. Provisioning already accepts per-session network-source overrides (`mj-controller/src/controller/backend.rs:763-795`), and adding them does not change the snapshot's catalog key, which uses repository identities and destinations (`mj-core/src/repository.rs:21-60`). Catalog, memory, publication and review do not depend on the bundle ID.
  Date/Author: 2026-10-07, Claude (approved by jbellis).
- Decision: never edit a configured bundle in the shared configuration during resume.
  Rationale: that would redefine the bundle for every future session.
  Date/Author: 2026-10-07, Claude.
- Decision: sessions already converted keep their synthesized `import-*` bundles; no data migration.
  Rationale: they are valid bundles, and rewriting history bindings is risk without user value.
  Date/Author: 2026-10-07, Claude.
- Decision: removing the duplicated `project_directory` copies is out of scope; tracked as #1258.
  Rationale: it needs a breaking schema revision and changes nothing observable, because `State::checkout` already ignores the copies.
  Date/Author: 2026-10-07, Claude (approved by jbellis).

## Outcomes & Retrospective

A raw session that resumes or moves onto a workspace target now keeps its bundle ID and bundle snapshot. The checkout's network remote is added to the session's `project.network_sources`, and the conversion archive maps onto the bundle's own repository ID and destination. Bundle synthesis, the configuration write and the history rebind are gone (net 41 fewer lines). Covered by behaviour tests for the kept bundle, a configured GitHub source, the missing-remote refusal, move, archive mapping, rollback, and the unchanged inverse direction. The isolated `tests/e2e/session_move.py` run passed, but it moves a bundle session between local bare targets, so it shows no regression in move rather than exercising a raw-to-container conversion; a real container walkthrough has not been run. Removing the duplicated `project_directory` copies remains in #1258.

Raw-to-workspace resume and move now retain `bundle_id` and the accepted bundle definition, write the chosen source into the session snapshot, and map conversion archives to the accepted repository ID and destination. The full controller suite passes (1,892 passed, 10 ignored), workspace clippy and formatting pass, and `tests/e2e/session_move.py` passes with real local-bare workers and a deterministic ACP. No database or protocol change was needed. No commit or push was made, as requested.

## Context and Orientation

`State::checkout` in `mj-core/src/state.rs` derives a session's checkout as `Attached` (user-owned directory), `ManagedWorktree` (raw worktree or clone Mjolnir created), `ManagedWorkspace` (bundle workspace cloned on the target) or `Borrowed` (sub-agent in its parent's checkout). It is the only reader of `project_directory` and `managed_worktree` for ownership questions.

`resume_compatibility_with_checkout` in `mj-client/src/target.rs:70-165` chooses `ResumePlan::InPlace`, `RawToWorkspace` (an attached local-bare checkout, or a whole local managed worktree, moving to a workspace target) or `WorkspaceToRaw` (a managed workspace with one local-source repository moving to local bare), and refuses attached SSH to workspace, relocating an SSH managed worktree, and managed checkouts opened at a subdirectory.

`RawToWorkspace` today: `plan_raw_to_workspace_with_checkout` (`worktree.rs:899-967`) requires the whole Git root, resolves a network remote ("no network Git remote" otherwise), derives repository and destination from the checkout's folder name, and builds a read-only preview (`worktree.rs:1082-1159`: branch, unpushed commits, dirty counts, remote URLs, managed-source retention; dirty submodules refuse). Resume (`resume.rs:1761-1876`) saves the planned bundle under the config lock, clears `project_directory` and `managed_worktree`, replaces `bundle_id` and `project`, rebinds `session_contexts` to the new bundle and inserts the old ID into `project_session_aliases`. The conversion archive (`worktree.rs:973-1025`; `resume.rs:1940-2000, 2305-2365`) captures local commits outside origin plus staged, unstaged and untracked changes, records the remotes as provenance, and is restored over a fresh clone (`resume.rs:2027-2067`). A managed source checkout is retired only after the destination is ready; failure removes the conversion archive and rolls back record and context (`resume.rs:1889-1902, 2070-2211`). Move uses the same plan and preview (`move_session.rs:812-845, 1199-1244, 2312-2331`). Preflight returns the preview or a clear error (`server_runtime/preflight.rs:143-190`; `resume.rs:167-217`).

Import with `--bundle` (`mj-cli/src/import.rs:73-76, 290-350`; `import/bundles.rs:21-48`; `import/native_import.rs:50-53, 132-166`) validates the configured bundle against the detected root, keeps its ID and snapshot, and builds the snapshot without `network_sources`.

## Plan of Work

Milestone 1. Replace the raw-to-workspace planning with a plan built from the derived checkout plus the session's accepted bundle snapshot (`SessionRecord::project_bundle`, which prefers `project.bundle` over configuration). The plan carries the session's bundle ID unchanged, the bundle repository that the checkout corresponds to (match by repository identity; the raw bundle has exactly one repository), the checkout's resolved network remote as a network-source override for that repository, and the archive mapping from the checkout to that repository's ID and destination. Resume and move write the network source into the session's `project.network_sources`, clear the checkout fields as today, and do not change `bundle_id`, `project.bundle`, `session_contexts` or `project_session_aliases`. Delete `planned_bundle`, `save_to`, and their use of `unique_bundle_id` in resume (keep the helper if import still uses it). Keep the preview, confirmation, refusals, archive contents, delayed retirement and rollback; rephrase the missing-remote refusal as the bundle repository having no network source. If the accepted bundle's repository has a configured GitHub source, use it and still archive local commits and changes. Leave `WorkspaceToRaw` as is, routed through the same planner entry point if that is a small change.

Milestone 2. Behaviour tests (colocated) for: import with `--bundle <name>` then resume onto a workspace target keeps `bundle_id`, history context, and adds no config bundle or alias row; a raw session with only a snapshot bundle keeps its ID; a configured GitHub-source bundle is used directly; refusals unchanged; a failed conversion leaves record and context unchanged; move follows the same path; `WorkspaceToRaw` unchanged. Then try `tests/e2e/session_move.py` in an isolated instance; record why if it cannot run here.

## Concrete Steps

From `/workspace/1872507e4d7eeafd43b66594471ab0a3/hel`: focused tests per round with `cargo test -p brokk-mj-controller <filter>` and `cargo test -p brokk-mj-client <filter>`; at the end once, the full suites of touched crates, workspace `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`, all outside any sandbox.

## Validation and Acceptance

Acceptance is the behaviour in Purpose: after resume onto a workspace target, the session's bundle is the one it had before, no configuration bundle was added, its history is still listed under that bundle, and its local commits and uncommitted changes are present on the target.

## Idempotence and Recovery

No schema, protocol or stored-format change. Rollback on a failed conversion is preserved. Re-running tests is safe.

## Artifacts and Notes

Validation evidence is in `/workspace/1872507e4d7eeafd43b66594471ab0a3/.mj-agents/17a09f543d495c0a740f5b098e5a1abf/`: `controller-suite-final.log`, `workspace-clippy-final.log`, `fmt-check-final.log`, and `session-move-e2e.log`; the full e2e trace and runtime artifacts are in `e2e/session-move-seed-1-564128/`.

## Interfaces and Dependencies

No new dependencies, schema revision, or protocol change. `RawToWorkspaceConversion` carries the derived checkout, accepted repository ID and destination, accepted `ProjectBundleSnapshot`, resolved `NetworkGitSource`, and optional managed source to retire. `plan_raw_to_workspace_for_session(session, checkout, config, executor)` derives that plan without writing; `apply_raw_to_workspace` preserves `bundle_id` and `project.bundle`, adds the per-session network source, and clears raw checkout fields. Resume and move share this planner. `WorkspaceToRaw` remains behaviorally unchanged.
