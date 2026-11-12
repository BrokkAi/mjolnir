# Install pinned Antigravity on session targets

This ExecPlan follows `.agents/PLANS.md` and fixes #1520.

## Purpose / Big Picture

Starting a session with profile gemini must prepare Antigravity's `agy` executable on the machine or container running that session. The person should not copy binaries to each target. The worker (the process running the harness) already has an installer for pinned harnesses; integrate Antigravity there, keeping authentication copied from the configured ~/.gemini source.

## Progress

- [x] 2026-11-12: Read #1520, claim the issue, and merge current origin/master before editing.
- [x] Verify Google's versioned 1.5.7 artifact manifests and documented AGY_CLI_DISABLE_AUTO_UPDATE=true setting.
- [x] Verified all six published archives against Google's SHA-512, recorded SHA-256 pins, and added the managed resolver, quota/login integration and matching container installer.
- [x] Both Linux musl architectures have no ELF program interpreter; the x86_64 artifact launches on this glibc host. The container installer passed locally, and the macOS arm64 artifact verified and launched on the Mac.
- [ ] Extend installer and ACP integration regressions; run focused tests during changes, then touched-crate suites and workspace clippy once.
- [ ] Complete an isolated remote Podman launch without an ambient agy, verify both quota pools, and clean up the instance.
- [ ] Commit, obtain clean review, merge any newer upstream changes, and push; verify image publication and dispatch platform CI.

## Surprises & Discoveries

The original implementation bypasses every HarnessRuntimePolicy for Antigravity and reports an ambient-binary error even under Managed. Quota collection separately calls the ambient executable finder. Container targets use Ambient, remote bare targets use Managed, and a daemon running inside its own image uses Image. A portable musl worker can run in a glibc container, so its compilation target does not identify that container's libc.

Google publishes archive URLs and SHA-512 hashes for Linux x86_64/aarch64 with glibc and musl and macOS x86_64/aarch64. Verify these before recording immutable metadata. The existing downloader verifies SHA-256; recording an independently computed SHA-256 after upstream SHA-512 verification lets it retain that implementation.

The Linux musl artifacts are static: both ELF program-header tables contain no PT_INTERP, and the x86_64 binary runs on this glibc host. Use those artifacts on Linux by architecture instead of guessing libc from the portable worker or adding runtime libc detection. This concerns Google's prebuilt CLI only; Rust build settings and the ordinary host/dev test target are unchanged.

## Decision Log

Use the existing resolver, staging, installer lock, atomic publication, receipt, and shared installation lease. Do not add a controller-to-target binary copy mechanism. Keep custom launch commands and the established Managed/Ambient/Image policies. Set AGY_CLI_DISABLE_AUTO_UPDATE=true in all mj-owned launch environments so sessions and quota probes cannot replace the pinned binary.

Use checked-in per-platform artifact URLs and checksums rather than resolving Google's moving latest manifest during a session launch. Prepare downloads before worker-upgrade idle reservation through the existing prepare-harness path. No database or wire format change is needed.

## Outcomes & Retrospective

Implementation and acceptance are pending. Preserve the user's gemini profile, disabled GPU scoring, and accepted additional-directory shell-cwd limitation. Install authorization from the earlier Antigravity work persists, but never replace a newer build without first merging it.

## Context and Orientation

`mj-core/src/harness_runtime.rs` owns pins and fallback launch descriptions. `mj-core/src/harness_install.rs` resolves runtime policies and atomically publishes managed installs under ~/.cache/mjolnir/harnesses. `mj-controller/src/antigravity_usage.rs` runs the CLI's built-in zero-turn /usage command. `mj-controller/src/harness_login/acp_login.rs` already resolves ACP/login executables through the shared installer. `containers/Containerfile.agent-dev` must install the same artifact version using checked-in metadata.

## Milestones and Plan of Work

First verify immutable downloads and their archive entry, and choose a Linux artifact that works in both supported libc environments. Put platform metadata under `mj-core/assets/antigravity/runtime.json`. Replace the ambient-only pin and exception with normal policy resolution, and retain the configured --gemini-dir argument for ACP. Reuse the existing download and archive-extraction helpers. Bake the pin into agent-dev with a small installer using that same metadata.

Next route /usage through locate and hold its managed lease for the whole command. Ensure ACP, login, discovery, version probes, and quota commands all disable self-updates. Extend existing cross-harness/image integration tests and installer race/failure tests for the new runtime path rather than adding ordinary unit examples.

Finally build the CLI and portable worker normally through mbx. Run a real isolated instance with copied configuration and credentials on a dedicated remote Podman container without agy installed. The first launch must install the pin inside the container, return a requested marker, and preserve correct quota collection. Tear down only test-owned resources. Review, push, check image publication, and dispatch nightly platform tests.

## Concrete Steps and Validation

From the repository root run targeted `cargo test -p brokk-mj-core harness_install` and affected controller tests outside the sandbox. After fixes run `cargo test -p brokk-mj-core -p brokk-mj-controller --no-fail-fast` and `cargo clippy --all-targets -- -D warnings` in dev profile. Keep package selection stable. Build portable workers with the explicit x86_64-unknown-linux-musl target; never redirect Cargo output.

Every new-build daemon, CLI, TUI, and E2E invocation uses --instance agy-managed with isolated MJ_CONFIG_DIR/MJ_DATA_DIR. Evidence belongs under `.mj/agents/35cd6ee67e57560bb12ce5b621a1387a/1520`. The decisive result is a real remote-container Gemini response from an initially missing runtime, plus successful Ge/Cl quota windows. Check reuse/concurrent publication and failed downloads through existing installer integration fixtures. Source OAuth files are only read; probes use copies.

## Idempotence and Recovery

Existing install locks serialize publication and leases protect running versions. A failed download leaves no complete receipt and the next attempt can retry. Never replace or delete files owned by a running process; terminate test processes before cleanup. Preserve active live sessions and do not downgrade the installed daemon.

## Interfaces and Dependencies

Extend existing HarnessPin and Located/ManagedHarness behavior without new persisted state. Artifact verification uses the existing SHA-256 downloader after verifying metadata acquisition against Google's SHA-512. Keep one resolver for mj-owned ACP and CLI execution. The container installer reads the same checked-in runtime metadata.

2026-11-12: Created this plan after the user approved managed-runtime support for #1520; it supersedes the initial ambient-only cut in antigravity-acp.md.
