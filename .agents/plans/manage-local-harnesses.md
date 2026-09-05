# Manage local bare harnesses and advance Codex

This living ExecPlan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Local bare sessions should use the same versioned harness installation as remote bare sessions. Native shell use of Codex remains independent. Advance mj's Codex CLI from 0.151.0 to 0.153.4 to expose Astra, and route local workers through the existing managed installer.

## Progress

- [x] (2026-09-05) Confirmed that the same codex3 account returns Astra with native 0.153.4 but not ACP's bundled 0.152.1.
- [x] (2026-09-05) Updated shared Codex metadata, container pin, npm manifest and generated lockfile.
- [x] (2026-09-05) Enabled managed local launches and safe upgrade preparation with behavior tests.
- [x] (2026-09-05) Reviewed integrated changes; formatting, cargo test, and cargo clippy --all-targets -- -D warnings passed.
- [x] (2026-09-05) Completed the validated implementation checkpoint; this plan is included in its commit.

## Surprises & Discoveries

Validation encountered the documented morannon NFS recovery storm (4,907 TEST_STATEID operations/second). After the user authorized recovery, the targeted client reset restored zero counter growth and 30 fingerprint reads in 11.33 ms; validation then resumed.

Ambient local ACP uses its own bundled CLI, independent of native Codex. The managed installer already runs off the event loop, stages installations atomically, and retains active versions with file leases.

## Decision Log

- Decision: Reuse managed installation for local bare targets; retain image-provided container runtimes and profile homes and approval policy.
  Rationale: This gives bare targets one consistent runtime without coupling native CLI use to mj.
  Date/Author: 2026-09-05 / user and Codex.
- Decision: Retain ACP 1.8.0, advance CLI to 0.153.4, and omit a live ACP compatibility probe.
  Rationale: The user explicitly accepted that compatibility assumption. Normal repository checks remain required.
  Date/Author: 2026-09-05 / user and Codex.

## Outcomes & Retrospective

Local bare initial provisioning and upgrade preparation now use the existing managed installer, preserving profile homes and approvals. Codex CLI is pinned to 0.153.4 across shared metadata, npm recipes, and the container. Legacy launch policy JSON remains compatible. Formatting, the full Cargo test suite, and clippy with warnings denied passed after targeted NFS recovery. A stale command-count assertion in the new test was corrected during review; the final full run passes. Live ACP compatibility testing was intentionally omitted as requested.

## Context and Orientation

`src/hel_harness_runtime.rs` defines version metadata; `mj-worker/assets/harnesses/codex/` contains npm dependencies and integrity hashes; `containers/Containerfile.agent-dev` installs the matching CLI. CODEX_PATH selects that CLI instead of the ACP bridge's bundled dependency. ACP translates mj's session protocol to Codex requests.

`mj-controller/src/hel_controller/worker_binary.rs` selects runtime policy and prepares worker upgrades. `src/hel_worker_launch.rs` serializes this policy. `mj-worker/src/hel_worker_runtime/harness.rs` owns the isolated installation cache; `unix.rs` resolves it before bridge launch. Local bare means running directly on the local host rather than in a container.

## Plan of Work

Update version metadata, npm manifests, and container recipe. Extend managed selection to LocalBare. Rename remote-specific policy terminology while retaining serialized compatibility. Prepare the new local harness before stopping an existing worker during upgrade. Add behavior coverage with the existing command-executor fakes, then inspect all policy uses and validate the integrated changes.

## Milestones

The version milestone yields consistent CLI 0.153.4 metadata and lockfile registry integrity records. Existing version-parity tests verify the recipes agree.

The runtime milestone routes local bare starts through the installer and prepares upgrades before replacing workers. Tests prove target selection and preparation failure behavior while retaining container behavior.

The integration milestone passes formatting, tests, and clippy and commits task files on the current branch.

## Concrete Steps

From `/home/jonathan/Projects/hel`, regenerate the lockfile with `npm install --package-lock-only --ignore-scripts --no-audit --no-fund --prefix mj-worker/assets/harnesses/codex`. Run `cargo fmt --all -- --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Cargo tests require elevated permissions. Review `git diff`, stage only task files, and commit without pushing.

## Validation and Acceptance

Local bare launch configurations must select the managed installer with the configured home and approvals. Containers keep their existing runtime selection. Preparation failures must not stop running workers. Existing saved launch policy values remain readable. All Rust checks must exit successfully. Live ACP compatibility testing is excluded by user instruction. New starts or explicit worker upgrades after installing the updated mj build obtain the new runtime; existing processes do not change in place.

## Idempotence and Recovery

Reuse the installer's atomic staging, installation locks, and active version leases. Retry failed installations through normal launch; never modify native Codex installations, credentials, or stop active user workers during development. Lockfile regeneration is repeatable.

## Artifacts and Notes

Read-only probes of model/list returned six visible models from CLI 0.152.1 and the same six plus gpt-6-astra from CLI 0.153.4 under codex3. No inference was performed.

## Interfaces and Dependencies

Reuse HarnessRuntimePolicy, harness_runtime_policy, prepare_managed_harness_for_upgrade, and harness::resolve. Do not add crates or a separate installer. Preserve the old serialized managed_remote value when generalizing the policy name.

Revision note: Created during implementation to record user scope, compatibility assumption, and upgrade safety requirements.

Revision note: Updated after successful full validation and authorized host recovery; implementation is ready to commit.
