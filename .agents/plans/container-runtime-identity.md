# Resolve container bridges before identifying their runtime

This ExecPlan follows `.agents/PLANS.md` and addresses issue #1166.

## Purpose / Big Picture

Container sessions must report the identity of the coding bridge and provider they actually execute. A scheduler can create a session without a prompt, save its runtime identity, and require that identity on later dispatch. The current controller generates a shell bootstrap command; the worker fingerprints the shell instead of the selected bridge and reports an unknown identity. Fix both Codex and Claude while preserving installation when an image lacks a suitable bridge.

## Progress

- [x] (2026-09-26) Claimed #1166 and confirmed the defect by tracing controller-generated shell launchers into worker runtime inspection.
- [x] (2026-09-26) Created `codex/container-runtime-identity` from current master; no other issue or PR owns the work.
- [x] (2026-09-26) Added and ran the real controller-to-worker regression: the old selection failed with the exact shell digest reported in #1166; the fix passes for both Codex and Claude.
- [x] (2026-09-26) Added shared preparation for primary/reviewer launches, absolute bridge/provider selection, recognition of persisted built-in shell launchers, and pinned managed installation fallback.
- [x] (2026-09-26) Five container regressions and six runtime/ACP guard tests pass. Covered provider changes, fixed PATH/provider selection, old launchers, pinned fallback/cache reuse/leases, custom wrappers, stale discovery, and refusal before native session load or goal recovery. Reviewer startup and excluded-credential regressions also pass (13 focused tests total); rustfmt and diff checks pass.
- [ ] Review, fix findings, open a PR, and let CI perform broad validation before merging.
- [ ] Prepare and publish a patch release from an exactly validated commit, including crates.io, npm, and Homebrew.

## Surprises & Discoveries

`mj-controller/src/controller/worker_binary/harness.rs::bridge_launch` currently returns `sh -c ...` for both Codex and Claude. `mj-worker/src/worker_runtime/harness.rs::resolve` returns no selection for ambient/container launches. `worker_runtime/unix.rs` therefore passes `sh` to `runtime_identity::inspect`, which cannot find bridge package metadata above the shell executable. The same selection must feed the fingerprint and the ACP supervisor; inspecting another executable while retaining the shell is insufficient.

The existing managed installer already provides pinned npm dependency graphs, explicit Codex provider selection, installation locking, and process-lifetime leases. Reuse it when a container lacks a suitable preinstalled bridge rather than launching a deferred `npx` selector after inspection. Existing persisted shell launch descriptions must remain recognizable on upgrade, and arbitrary custom wrappers must not be mistaken for built-in launchers.

## Decision Log

Decision (2026-09-26, Codex): Keep bridge selection and installation on the worker, then inspect and execute the same concrete selection. Use background filesystem/install work and existing subprocess helpers. Do not start ACP sessions or prompts during selection. Preserve the image's preinstalled bridge policy and expose unknown metadata honestly.

Decision (2026-09-26, Codex): Use a patch release because this repairs the runtime receipt feature shipped in 2.23.0. Broad Cargo tests, Clippy, platform checks, and release validation remain in CI as the user requested. Local checks are focused behavior regressions and formatting only.

## Outcomes & Retrospective

Investigation confirmed the issue. Implementation, PR validation, and publication remain pending.

## Context and Orientation

The controller writes `WorkerLaunchConfig` through `mj-controller/src/controller/worker_binary/launch.rs`; container targets use `HarnessRuntimePolicy::Ambient`. The worker resolves its effective environment, prepares its harness, fingerprints it, and writes `AcpSupervisorSpec` in `mj-worker/src/worker_runtime/unix.rs`. The supervisor executes that specification. `mj-worker/src/worker_runtime/harness.rs` owns managed installations, while `runtime_identity.rs` computes public component identities. The reviewer also starts harnesses through these preparation helpers. Shared inert bridge metadata belongs in `mj-core/src/harness_runtime.rs` or `worker_launch.rs`, not a new crate.

A lease is an advisory file lock held while a managed installation is in use; garbage collection must not remove leased files. An ambient selection uses the target's installed tools. A constrained launch requires the recorded identity and must fail before native session creation/resume or any task prompt when it differs.

## Plan of Work

First add a cross-runtime regression in controller worker-binary tests, using the controller's real container configuration and a disposable npm-shaped Codex/Claude installation. Show that the worker selects the actual bridge and obtains package/provider identity instead of a shell digest. Keep fixtures isolated, without using real credentials or sending work to a provider.

Next make the built-in npm bridge selection explicit before fingerprinting. Preserve old generated shell configurations during worker upgrades, recognize only known built-in launcher forms, and leave custom commands untouched. Resolve the target executable, preserve Codex's pinned bridge version check and Claude's preinstalled-bridge preference, and use the existing pinned managed installation when necessary. Hold the installation lease through inspection and execution. Persist the selected absolute command and environment in the supervisor specification. Apply the same preparation to reviewer launches so removing bootstrap selection does not lose their installation behavior.

Finally add regressions for installed bridges, missing/incompatible bridge installation, deterministic selection after PATH changes, constrained mismatch before native work, and unknown custom wrappers. Update public container/runtime documentation to describe actual selection and provenance. Review the PR, fix findings, wait for green CI, merge, and follow `RELEASING.md` for the release and Homebrew formula update.

## Concrete Steps

Work in `/home/ryan/.codex/worktrees/9767/mjolnir`. Use `TMPDIR=/home/ryan/mj-tmp-9767` for local focused Cargo tests because the shared tmpfs was full during prior work. Run every Cargo test with elevated permissions; keep output in `target/`. Any manual CLI/daemon/TUI invocation uses `--instance container-runtime-1166` and disposable configuration/data directories.

    cargo fmt --all -- --check
    cargo test -p brokk-mj-controller container_runtime
    cargo test -p brokk-mj-worker container_runtime

CI supplies the full dev-profile `cargo test` and `cargo clippy --all-targets -- -D warnings` runs. After the fix is committed, open a PR referencing #1166, review the final head, and merge only after green checks. Prepare the patch version using cargo-release 1.1.5 in a clean checkout; validate the exact versioned commit in CI, tag that commit, and publish through the existing workflows without repeating broad local checks.

## Validation and Acceptance

The regression must consume the actual controller-generated container launch description. A preinstalled Codex or Claude bridge with valid provider metadata produces a non-null identity and executes that same resolved bridge. Missing or incompatible built-in bridges install through a deterministic pinned location before inspection. A required identity accepts a matching selection and rejects a changed/unknown selection before native ACP session work. A custom wrapper remains explicitly unknown instead of being silently replaced with some other installed bridge. Existing isolated upgrade tests remain green, with no schema migration or live-store intervention.

## Idempotence and Recovery

Use disposable directories and named instances only. Installation retries use existing locking and staging; never remove a busy installation or stop an active worker to satisfy a runtime constraint. Preserve earlier receipts and existing expected identities. If CI fails, fix the actual cause and revalidate the changed head. Release retries use the same published tag; never move a published tag.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1166. The previous release is v2.23.0; the proposed bug-fix release is 2.23.1, subject to checking for concurrent releases when preparing the version.

## Interfaces and Dependencies

Reuse `WorkerLaunchConfig`, `HarnessRuntimePolicy`, `ManagedHarness`, `RuntimeIdentity`, `AcpSupervisorSpec`, shared subprocess helpers, and the existing npm package locks. Add no workspace crate or database migration. Preserve the serialized worker launch format where possible; keep compatibility for already persisted bootstrap commands.

Initial plan recorded 2026-09-26 after confirming the reported launch/inspection mismatch.

Reproduction: `cargo test -p brokk-mj-controller container_runtime_identifies_the_controller_selected_bridge_and_provider` failed before the fix with `id: None` and shell digest `a6f559e00b69a4aa4d8cb607be18d9386c5aee55c509e2c075549dcf00e00fc7`. The same test passed after the fix for Codex and Claude. Additional upgrade/fallback/custom-wrapper regressions are being validated.

Review decision (2026-09-26): Keep the controller’s bootstrap description backward compatible for new reviewer requests sent to older busy workers. Shared `NpmBridge` metadata emits the old script and recognizes that complete form on upgraded workers; the worker replaces it with the concrete executable before inspection. Do not introduce a new wire-format field or require a busy worker restart.

Release decision (2026-09-26): Prepare 2.23.1 on the PR branch after the implementation checkpoint. Push an additional `ci/release-2.23.1` ref to validate the exact versioned commit concurrently with PR checks. Merge only when the PR is green, and tag the exact release candidate after its own CI is green; a later master advance need not be included.
