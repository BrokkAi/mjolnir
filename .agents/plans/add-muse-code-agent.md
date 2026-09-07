# Add Muse Code as an ACP agent

This ExecPlan follows `.agents/PLANS.md`. Maintain Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective during implementation.

## Purpose / Big Picture

Users will select Muse Code alongside existing agents in Mjolnir, authenticate a Muse profile, and use streaming chat, tool approvals, images, model/effort selection, cancellation, and durable session continuation through BrokkAi's muse-acp adapter. The existing controller owns session lifecycle and the existing worker supervises the adapter and its Muse process.

## Progress

- [x] (2026-09-07) Inspect adapter v0.2.3, Muse 1.0.3-R2198.1 help and embedded MSP schema, and existing harness integration.
- [x] (2026-09-07) Add harness metadata, isolated environment mapping, credentials, setup/UI integration, and schema v27 migration preserving existing data and indexes.
- [x] (2026-09-07) Add verified managed binary installation and container parity; test corrupt downloads, incomplete caches, concurrent actual downloads and container installer output.
- [x] (2026-09-07) Add selected-session and child-stream checkpoint capture, resume destination restrictions, and explicit MCP/reviewer/import/quota capability limits.
- [x] (2026-09-07) Fix and release upstream muse-acp v0.2.4, with regression tests and successful release workflow on all five platforms.
- [x] (2026-09-07) Final full parallel cargo test, Clippy, web unit tests, documentation build and 1688 internal-link checks, and the expanded published-adapter protocol test pass.
- [x] (2026-09-07) Standalone worker build passes; implementation is complete and recorded in the current-branch integration commit containing this plan.

## Surprises & Discoveries

Muse uses XDG config and data roots rather than a single harness-specific home variable. Its configuration lives in XDG_CONFIG_HOME/muse and its session trees in XDG_DATA_HOME/muse/sessions/YYYY/MM/DD/ID, including child streams. The default source config is ~/.config/muse and credentials are auth.json. The adapter ignores client MCP servers. Muse's native session/resume schema has no workspace relocation parameter. Muse serve fixes sandbox posture at process startup, while approval mode is selected over the protocol.

Actual integration testing with published adapter v0.2.3 showed that ACP v1 questions were silently cancelled. Upstream read only v2's capabilities key and gated the question bridge on protocol version 2 twice. The v0.2.4 fix reads v1 clientCapabilities and gates questions on advertised form support rather than version. A new upstream test failed before the complete fix and passes afterward; clients without form support still cancel safely. Upstream commit 2a29c9b and release workflow 34097444996 record the fix and successful release.

Parallel full-suite runs exposed two existing lease tests that assumed a close immediately releases a file lock even while another thread forks a process. The supervisor test passed alone, while full runs alternated failures between that test and obsolete-install cleanup. Forked children can retain close-on-exec descriptors until exec. These tests now assert bounded eventual release, preserving the checks that live leases prevent exclusive access and deletion. Production lock/cleanup behavior is unchanged; the full parallel suite passes with these assertions.

## Decision Log

Use serialized harness ID muse and label Muse Code. Pin adapter 0.2.4 and Muse 1.0.3-R2198.1; download verified release artifacts directly, not an auto-updating launcher. Add shared environment mapping instead of scattered XDG special cases. Preserve guardian security settings; only explicitly unconstrained targets disable inner sandbox and force auto approvals. Reject unsupported workspace relocation before teardown. Do not promise injected MCP features or Muse reviewer roles when their required servers cannot be delivered. External native-session import is outside this implementation.

On 2026-09-07 the user explicitly authorized fixing upstream and creating a release. Fix the source defect in muse-acp instead of maintaining a Mjolnir workaround. Its independent repository release instructions require package/protocol version updates, fmt, tests, Clippy and self-test before an annotated release tag. Those checks passed: 18 unit tests and 42 integration tests. The upstream master commit and v0.2.4 tag were pushed; Mjolnir remains local. Ordinary Muse tool approvals use the existing elicitation UI and validate responses against the options supplied by the adapter, cancelling safely when the user does not select an offered choice.

## Context and Orientation

src/hel_config.rs defines HarnessKind and profiles. src/hel_harness_runtime.rs owns version pins. mj-worker/src/hel_worker_runtime/harness.rs installs and leases managed runtimes. mj-controller/src/hel_controller/worker_binary.rs stages profiles and constructs launches. src/hel_credentials.rs handles login and credential synchronization. src/hel_checkpoint.rs selects native artifacts; controller resume and move modules enforce restoration compatibility. The ACP session surface normalizes model, effort, modes and available commands for both user interfaces.

## Plan of Work

First add Muse to the existing harness enum and exhaustive dispatch, and introduce a shared helper applying a private config/data layout to launched process environments. Keep existing harness behavior unchanged. Discover Muse config from XDG, stage its allowlisted settings/authentication and configure login under that profile. Model and effort remain adapter-advertised values; ask/auto/deny are approval modes rather than planning modes.

Next extend the managed runtime installer with checksum-verified native downloads for Linux and macOS x86_64/aarch64, using the existing atomic cache publication and leases. Set MUSE_CLI to the pinned adjacent native binary. Container builds must use the same pins and verification metadata. Installation work remains supervised off UI loops.

Finally collect the chosen session's complete native tree without other sessions or credentials, restore it under the private data root, and enforce original-workspace compatibility before teardown. Reuse ACP resume and existing queue admission, cancellation and recovery. Add explicit capabilities for unsupported MCP-dependent roles and user-facing limitations. Update human documentation and license notices.

## Concrete Steps

Work from /home/ryan/code/mjolnir. Use apply_patch for edits. Compile early to locate exhaustive matches. Run cargo fmt --all -- --check, cargo test (with elevated permissions), cargo clippy --all-targets -- -D warnings, and a standalone brokk-mj-worker build. Run relevant web checks from tests/e2e/web. Use target/muse-integration-inspect for ignored runtime inspection artifacts; the downloaded Muse x86 Linux binary has SHA256 75a68f98c437dfd17d264730c5bc72d57e5f1e18d10472a9f53261ffcc091352.

To reproduce the protocol test, download the pinned Linux adapter archive from BrokkAi/muse-acp release v0.2.4, verify the hash in mj-worker/assets/muse/runtime.json, and extract it. Run MJ_MUSE_ACP_TEST_BINARY=<absolute-adapter-path> cargo test -p brokk-mj-core real_muse_adapter -- --ignored with elevated permissions. This launches the actual ACP client, published adapter and tests/e2e/muse_host.py, a deterministic Muse protocol host. It tests images above 64 KiB, streamed replies, effort selection, permissions, form answers/cancellation, turn cancellation and resume without replay. Run cargo test -p brokk-mj-worker muse_real_install -- --ignored with elevated permissions and network access to exercise concurrent managed installation. Run python3 scripts/install-muse.py mj-worker/assets/muse/runtime.json target/muse-integration-inspect/image/bin to exercise the container installer without building the full image.

## Validation and Acceptance

Exercise the real adapter with a deterministic fake MSP host for streamed text/tools, permission approval and denial, elicitation, changing selectors, image payloads greater than 64 KiB, cancellation, and resume without duplicated events. Tests must prove environment isolation, selected-session checkpoint capture including child artifacts, corruption errors, relocation rejection before source teardown, queued-work recovery, and concurrent/failed/checksum-invalid installs. Run a real-provider smoke only when authenticated credentials are available; record unavailable live coverage explicitly.

## Idempotence and Recovery

Install into temporary cache directories and atomically publish only verified artifacts. Use existing lifecycle cleanup; never delete a live process's files. Preserve existing user configuration and unrelated changes. Stage only task files and commit on the current branch after validation. Only the explicitly authorized upstream adapter release is pushed/published; do not push or release Mjolnir without further instruction.

## Interfaces and Dependencies

Add HarnessKind::Muse (muse), native binary pin metadata and a shared profile-to-environment mapping. ACP remains v1, with muse-acp providing session/resume, configOptions, permissions and images. No new crate or service is required. Original native workspace paths are mandatory on restoration because MSP cannot override them.

## Outcomes & Retrospective

The upstream defect is fixed and published, and Mjolnir implementation and required validation are complete. No authenticated provider turn has been run: native download/startup are verified, while protocol behavior uses the real adapter with a deterministic host. Checkpoint coverage verifies selected native trees and child artifacts, not a paid provider's running child continuation. Full container image build and execution on remote/macOS targets are not claimed. The image installer itself was executed successfully on Linux. Human-readable integration and limitations are documented in docs/src/content/docs/profiles.md and related reference pages.

Revision 2026-09-07: record implemented integration, actual upstream regression/release, runtime installation evidence, reproduction commands, and remaining validation limits. The change from adapter 0.2.3 to 0.2.4 follows the observed defect and user's release authorization.

Final revision 2026-09-07: record successful final tests, Clippy, standalone build, docs/web checks, and the test-only lease timing correction. All implementation steps are complete; authenticated provider and full-image smoke coverage remain explicitly unclaimed.
