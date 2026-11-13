# Refresh managed harness pins

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Mjolnir-managed sessions have fallen behind current stable harness releases. After this change, a new bare, SSH, EC2, or agent-dev container session installs a coherent current ACP bridge or native harness binary, verifies its checked-in artifact checksum, and reports the updated version consistently in Mjolnir's managed-install identity and diagnostics. A person can see the result by running each installed command's version check, by running `mj doctor`/a small session in an isolated instance, and by observing that the agent-dev image pin test and managed-install tests agree.

## Progress

- [x] (2026-11-13 08:20Z) Inventoried every harness pin, install asset, image check, checksum manifest, and affected compatibility documentation.
- [x] (2026-11-13 08:35Z) Compared published stable releases and release notes for Codex, Claude, Kimi, Grok, Muse, OpenCode, DSH, and Antigravity.
- [x] (2026-11-13 08:45Z) Confirmed Codex ACP 1.15.0 and Muse ACP 0.21.0 are the latest published adapters; Muse 1.4.15 adapter support is not yet released.
- [x] (2026-11-13 08:31Z) Downloaded the official Kimi, Grok, OpenCode, and Antigravity artifacts for all supported platforms and verified their checked-in SHA-256 values; Antigravity SHA-512 values also match its official manifests.
- [x] (2026-11-13 08:39Z) Updated Rust pins, install identities, npm package manifests and locks, runtime JSON manifests, container pins/checks, and version-specific test fixtures/documentation.
- [x] (2026-11-13 08:33Z) Regenerated the OpenCode provider environment from tag `v2.2.7`; only its generation header changed.
- [x] (2026-11-13 09:00Z) Ran focused pin, installer, configuration, generated-file, Python, Node, core, worker, and controller tests.
- [x] (2026-11-13 08:56Z) Exercised real ACP initialize/version handshakes for all seven ACP harnesses; Codex launched app-server 0.168.0, and OpenCode completed a real two-step tool turn with whole-turn usage.
- [x] (2026-11-13 09:11Z) Ran workspace Clippy, reviewed the complete diff, and confirmed the Grok fixture changes only its filename and recorded version.
- [x] (2026-11-13 09:31Z) Committed and pushed the validated change as `dca598e54`; the agent-dev image workflow `43398690145` succeeded for both architectures and published the multi-arch manifest.

## Surprises & Discoveries

- Observation: The current system date is 2026-11-13, but several releases relevant to this repository landed during the preceding week, so most pins are behind by one to several releases.
  Evidence: npm reports Claude ACP 0.102.0 to 0.106.0, Kimi 2.4.0 to 2.6.1, DSH 0.5.0 to 0.6.0, OpenCode 2.2.6 to 2.2.7, and Antigravity 1.5.7 to 1.5.8.
- Observation: Grok's npm version list is ahead of its stable `latest` dist-tag.
  Evidence: registry `latest` is 1.0.77 while 1.0.78 through 1.0.84 are present only behind the `alpha` tag, so the plan uses 1.0.77.
- Observation: Muse has newer stable host binaries than the latest published adapter supports completely.
  Evidence: `@brokkai/muse-acp@0.21.0` is the latest npm version and its changelog stops at Muse 1.4.14; `muse-acp` master added tested 1.4.15 support on 2026-11-12 but has not published a release. The Muse stable channel already advertises 1.4.18.
- Observation: Claude 0.103.0 contains a packaging-breaking change, but it targets deep imports of `dist/` rather than the ACP command Mjolnir launches.
  Evidence: the release notes say ACP v1 remains served and only the package's library exports changed; Mjolnir executes `claude-agent-acp` as a process.
- Observation: The updated native and bridge versions all accept Mjolnir's ACP v1 initialization.
  Evidence: initialize responses named Codex ACP 1.15.0, Claude ACP 0.106.0, Kimi 2.6.1, Grok 1.0.77, OpenCode 2.2.7, DSH 0.6.0, and Antigravity 1.5.8 with `protocolVersion: 1`.
- Observation: OpenCode 2.2.7 still reports whole-turn usage in the same shape as 2.2.6.
  Evidence: a local fake provider drove two model steps through the pinned binary; `session/prompt` returned `{inputTokens: 106, outputTokens: 15, cachedReadTokens: 14, totalTokens: 135}`.
- Observation: Codex ACP 1.15.0 successfully starts and initializes the separately pinned Codex 0.168.0 app-server.
  Evidence: with a synthetic API key, `session/new` returned a session and the 0.168.0 model/mode catalog; an invalid-key refresh error occurred afterward without preventing session creation.
- Observation: The Claude stripped-credential acceptance test cannot enter its required private Linux network namespace in this container.
  Evidence: it stopped immediately with `unshare: unshare failed: Operation not permitted`; direct Claude 2.1.323 ACP initialization and `session/new` succeeded outside that isolated harness.
- Observation: One untouched worker process-inspection test was flaky under the full suite.
  Evidence: `a_real_setsid_orphan_is_found_by_its_session_marker_and_identified_like_the_park_script` saw `[sleep]` instead of `sleep 61` in one full run, then passed when rerun alone.

## Decision Log

- Decision: Update only code and binary pins that can be backed by a published stable release and an ACP v1 stdio path.
  Rationale: Mjolnir invokes harnesses as external ACP processes. Committing to an unreleased adapter or an adapter-unverified native host would make managed installs less reproducible and could silently degrade sessions.
  Date/Author: 2026-11-13, Codex.
- Decision: Keep `CODEX_ACP_VERSION` at 1.15.0 while updating the separately selected Codex CLI to 0.168.0.
  Rationale: 1.15.0 is the newest npm and GitHub release. Mjolnir intentionally selects the separately pinned CLI through `CODEX_PATH`, and the bridge already supported a version skew from its nested SDK.
  Date/Author: 2026-11-13, Codex.
- Decision: Keep Muse ACP 0.21.0 and native Muse 1.4.14-R7309.1 until a published adapter includes 1.4.15 support.
  Rationale: 0.21.0 classifies 1.4.15 as an unknown/degraded host and the unreleased adapter changes explicitly fix behaviors that would otherwise degrade Mjolnir sessions. The release boundary is the safe compatibility contract.
  Date/Author: 2026-11-13, Codex.
- Decision: Use Grok 1.0.77, the npm `latest` dist-tag, rather than the newer alpha-tagged builds.
  Rationale: Managed installs should use a stable release; alpha builds have a different support expectation and rolling version stream.
  Date/Author: 2026-11-13, Codex.

## Outcomes & Retrospective

Implementation is complete, reviewed, committed, pushed, and published. Core passed 648 unit tests plus 3 Jev scenario tests, controller passed 2,611 tests, and worker passed 837 tests with one unrelated process-title flake that then passed alone. All 15 agent-dev/release Node tests, formatting, workspace Clippy, three live native-install checks, all seven ACP initializations, Codex app-server session creation, and an OpenCode whole-turn usage probe passed. The Claude stripped-credential test is blocked by this container's lack of permission to create its private network namespace.

The agent-dev image workflow built amd64 and arm64 and published the multi-arch manifest successfully. Hosted CI runs for the feature commit were cancelled because other agents pushed newer master commits while they ran; the local full suites and workspace Clippy cover the committed feature set. Muse remains intentionally on its last fully supported published adapter/host pair until a new adapter release includes the 1.4.15 surface.

## Context and Orientation

Mjolnir is a Rust workspace. `mj-core/src/harness_runtime.rs` is the single Rust source of harness version constants, install IDs, display names, executable entrypoints, and the built-in fallback launcher. `mj-core/src/harness_install.rs` consumes those constants plus checked-in JSON manifests to download and verify managed installations. The npm-based Codex and Claude installs use the package files under `mj-core/assets/harnesses/`; native Kimi, Grok, OpenCode, Muse, and Antigravity installs use the JSON files under `mj-core/assets/<harness>/runtime.json`. `containers/Containerfile.agent-dev` installs the same supported tool stack into the multi-architecture image, and `mj-controller/src/controller/worker_binary/tests.rs` cross-checks that image against the Rust pins.

ACP means Agent Client Protocol, the JSON-RPC protocol carried over a child process's standard input and standard output. A bridge or native harness that "speaks ACP v1" must accept Mjolnir's initialize request with `protocolVersion: 1`. A managed install is a checksum-verified cache entry that carries `mj-harness.json`; its directory identity must change whenever the installed bytes change.

## Plan of Work

First, update the canonical constants in `mj-core/src/harness_runtime.rs` and every corresponding `HarnessPin` install ID and display version. Update the two npm `package.json` files and regenerate their `package-lock.json` files so the adapter and CLI versions are exact and reproducible.

Second, refresh each checked-in runtime manifest. Replace version strings and all platform checksums from the official release artifacts. For npm-packaged OpenCode, use the published per-platform tarballs. For Kimi and Grok, use their official binary release URLs. For Antigravity, use the official CLI manifest. Keep Muse unchanged under the release-boundary decision above.

Third, update `containers/Containerfile.agent-dev` package installs and runtime version assertions. Update agent-facing and user-facing version-specific text, especially DSH's 0.5.0 floor in `docs/src/content/docs/profiles.md`. Regenerate `mj-core/assets/opencode-provider-environment.txt` with `scripts/generate-opencode-provider-environment.py` from an OpenCode 2.2.7 checkout.

Fourth, validate checksums and package locks, run the focused harness/container/config tests, run real ACP initialize handshakes for the npm bridges and native ACP commands, and perform a managed-install smoke test in a disposable cache. Then run `cargo fmt --check`, workspace `cargo clippy --all-targets -- -D warnings`, and full tests for touched crates.

Finally, review the complete diff, commit only the files changed for this update on the current `master` branch, push to `origin/master`, and verify the agent-dev image workflow triggered by the push.

## Concrete Steps

Work from the repository root:

    cd /workspace/b651177ccc93bd830e05738a76f44471/mjolnir

Download each native artifact into a disposable directory under `/tmp`, compute `sha256sum`, and compare it with the release manifest or npm registry artifact before putting the digest into JSON. Generate npm locks with:

    npm install --package-lock-only --ignore-scripts

inside `mj-core/assets/harnesses/codex` and `mj-core/assets/harnesses/claude`.

Regenerate the OpenCode provider environment from a checkout of tag `v2.2.7`:

    python3 scripts/generate-opencode-provider-environment.py <opencode-checkout> 2.2.7 mj-core/assets/opencode-provider-environment.txt

Focused validation while iterating:

    cargo test -p mj-core harness
    cargo test -p mj-controller bridge_fallback_pins_match_the_agent_dev_containerfile
    node --test scripts/agent-dev-image.test.mjs

Final validation:

    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test -p mj-core
    cargo test -p mj-worker
    cargo test -p mj-controller

Use a named isolated instance for every CLI/session smoke test and unset inherited `MJ_CONFIG_DIR` and `MJ_DATA_DIR` first.

## Validation and Acceptance

Acceptance requires all version assertions to report the target releases, all artifact checksums to match downloaded bytes, and the npm locks to resolve exact adapter and CLI versions. A real `initialize` request through each updated ACP command must return protocol version 1 (or a compatible selected version) rather than reject Mjolnir's request. The managed-install tests must prove that a stale install identity is not reused, the OpenCode provider-environment test must prove the file came from 2.2.7, and the container/controller pin test must fail if the Rust pins and image drift.

The final diff must not update Muse to an adapter-unverified host. The final report will name the remaining Muse adapter/host lag and the upstream release needed to remove it.

## Idempotence and Recovery

All source and manifest edits are ordinary checked-in changes and can be reapplied from this plan. Downloads go only to a disposable `/tmp` directory and can be repeated. Regenerating npm locks and the OpenCode environment is idempotent when given the same versions. If a download produces a checksum mismatch, do not update the pin; re-fetch from the official URL and compare with the upstream manifest or registry identity. If a focused compatibility test fails, stop before committing and either remove that harness from the update or record the reason and leave its previous pin intact.

## Artifacts and Notes

Current pins at plan start:

    Codex: ACP 1.15.0, CLI 0.165.0
    Claude: ACP 0.102.0, CLI 2.1.321
    Kimi: 2.4.0
    Grok: 1.0.70
    Muse: ACP 0.21.0, host 1.4.14-R7309.1
    OpenCode: 2.2.6
    DSH: 0.5.0
    Antigravity: 1.5.7

Target pins:

    Codex: ACP 1.15.0, CLI 0.168.0
    Claude: ACP 0.106.0, CLI 2.1.323
    Kimi: 2.6.1
    Grok: 1.0.77
    Muse: unchanged
    OpenCode: 2.2.7
    DSH: 0.6.0
    Antigravity: 1.5.8

## Interfaces and Dependencies

The public Rust interface remains `mj_core::harness_runtime::{pin, npm_bridge, fallback_launch}` and `HarnessPin`. No shape changes are expected. The installed content identity is still `HarnessPin::install_id`; it must change for every new binary or npm dependency set so cache reuse cannot select old bytes. Native runtime JSON keeps its existing schema (`version`, `platforms`, and per-platform filename/target/checksum fields). The agent-dev image remains the shared `ghcr.io/brokkai/mjolnir/agent-dev:latest` consumer path and must stay internally consistent with the worker-side pins.

## Change Note

2026-11-13: Created after inventorying the current pins and upstream stable releases. The plan records the decision to stay within published adapter compatibility, especially for Muse, rather than updating a native binary ahead of its adapter.

2026-11-13 09:02Z: Updated progress with checksum, handshake, generated-file, and full crate-test evidence. The only validation failure observed was the unrelated isolated process-title flake, which passed on its required single-test rerun.

2026-11-13 09:11Z: Recorded the completed Clippy run and final diff review, including exact passing suite counts and the one environment-bound Claude test limitation.

2026-11-13 09:32Z: Finalized the plan after confirming the pushed agent-dev image workflow succeeded for both architectures and the multi-arch manifest. Recorded the concurrent-push CI cancellations and remaining Muse adapter-release dependency.
