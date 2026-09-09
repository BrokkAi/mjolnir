# Release Mjolnir v2.5.0

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture

Publish the completed work that entered master after v2.4.0. The release gives users independent network Git clones for isolated sessions, daemon-lifetime worker-binary pinning, the redesigned workspace and Sessions terminal interface, change-aware TUI rendering, and native Meta Muse Spark as the second utility model. GitHub release archives, all eight Rust workspace crates, and all four npm packages should expose version 2.5.0.

## Progress

- [x] (2026-09-09) Read `RELEASING.md` and `.agents/PLANS.md`, fetched `origin`, and confirmed local `master` exactly matches `origin/master` at `c03b2bbac0eefe3e46e9d47441e8253393eea6a5`.
- [x] Select minor version 2.5.0 because master adds substantial user-facing behavior rather than only defect repairs.
- [x] Synchronize the workspace version, five internal dependency constraints, all eight `Cargo.lock` workspace entries, and generated third-party license report. Supplemental notices remain byte-for-byte current.
- [x] Pass pre-commit formatting, default-member Clippy with warnings denied, host release build, x86-64 musl worker release build, the full default-member Cargo test suite, dependency-license policy, fresh notice-report comparisons, and whole-workspace source packaging.
- [x] Verify all eight crates.io trusted publishers name repository `BrokkAi/mjolnir`, workflow `publish.yml`, and environment `crates-io`.
- [x] (2026-09-09) Commit clean release candidate `ccfbe0b461ea690128428aea29ffa0e28c5e47be`, validate it locally, and push it to `origin/master`.
- [x] (2026-09-09) Confirm exact-commit CI run `34324527404` passed all seven jobs, including Linux, macOS, Windows, licenses, desktop, reliability, and voice checks.
- [x] (2026-09-09) Create and push annotated tag `v2.5.0`; tag object `55c9d95ca322ed8312bd8018b8c6d6695322fb94` resolves to candidate `ccfbe0b461ea690128428aea29ffa0e28c5e47be`.
- [x] (2026-09-09) Confirm GitHub Release workflow `34326858337`, crates.io workflow `34329773630`, and npm workflow `34329784335` all completed successfully.
- [x] (2026-09-09) Verify all three release archives and checksum sidecars, run the released Linux controller and portable worker, confirm all eight Rust crates are 2.5.0 and not yanked, and confirm all four npm packages are 2.5.0 and `latest`.
- [x] (2026-09-09) Publish concise user-facing release notes and record completion.

## Surprises & Discoveries

- Observation: The first sandboxed `cargo update --workspace` could not resolve `index.crates.io`, and `cargo package` later required registry access even with a complete local fetch.
  Evidence: Both commands succeeded when rerun with network access outside the restricted sandbox; only workspace versions changed in `Cargo.lock`.
- Observation: The supplemental notice generator invokes `cargo metadata`, which the restricted sandbox blocks with `EPERM`.
  Evidence: Regeneration succeeded outside the sandbox and produced no diff in `licenses/SUPPLEMENTAL_THIRD_PARTY_NOTICES.txt`.
- Observation: The only `cargo deny` diagnostic is the known unmatched `libbz2-rs-sys@0.2.5` exception warning also present in the validated v2.4.0 release pipeline.
  Evidence: `cargo deny --workspace --config licenses/deny.toml --locked check licenses` exits successfully and reports `licenses ok`.
- Observation: Exact-commit CI and the macOS release build were correct but slow.
  Evidence: Windows CI completed after cache housekeeping in 26m15s, and the universal macOS release build completed in 28m25s; every job ultimately passed.
- Observation: crates.io needed time to propagate same-release sibling versions while publishing the dependency chain.
  Evidence: Six crates were visible before `brokk-mj-desktop`; the publisher retried as designed and all eight were published when workflow `34329773630` completed successfully.

## Decision Log

- Decision: Release version 2.5.0.
  Rationale: Isolated network clones, worker pinning, the TUI redesign, and utility-model selection are additive feature work.
  Date/Author: 2026-09-09, Ryan SVIHLA.
- Decision: Keep the existing release and registry workflows unchanged.
  Rationale: The user asked for a new release, not a publishing redesign; all workflow files are unchanged since the successfully published v2.3.0 pipeline.
  Date/Author: 2026-09-09, Ryan SVIHLA.
- Decision: Use read-only authenticated crates.io API reads to verify every Rust trusted publisher.
  Rationale: `RELEASING.md` requires exact repository, workflow, and environment verification rather than inferring authorization from package ownership or prior success.
  Date/Author: 2026-09-09, Ryan SVIHLA.
- Decision: Treat the unchanged npm trusted-publisher pipeline as authorized only from the previously recorded explicit user direction and only while the workflow and package identities remain unchanged.
  Rationale: The v2.3.0 release record says the user explicitly instructed proceeding through the normal pipeline with existing publisher configuration instead of repeating an npm settings-inspection detour. The npm workflow and release package identities are unchanged since that instruction. Do not change publishers or package names under this decision.
  Date/Author: 2026-09-09, Ryan SVIHLA.

## Outcomes & Retrospective

Mjolnir v2.5.0 is published at https://github.com/BrokkAi/mjolnir/releases/tag/v2.5.0. The immutable tag identifies validated commit `ccfbe0b461ea690128428aea29ffa0e28c5e47be`; this completion record is committed afterward and does not alter the tagged release. All local checks, exact-commit CI, release builds, registry workflows, asset checksums, binary versions, and public package checks passed.

## Context and Orientation

The root `Cargo.toml` is the single source of the workspace version. Eight crates inherit it: `brokk-mj-core`, `brokk-mj-voice-worker`, `brokk-mj-worker`, `brokk-mj-controller`, `brokk-mj-chat`, `brokk-mj-tui`, `brokk-mj-desktop`, and `brokk-mjolnir`. Five same-release path dependencies repeat the version in `[workspace.dependencies]`. `Cargo.lock` records all eight local package versions. `licenses/THIRD_PARTY_LICENSES.html` embeds them and must be regenerated after a bump.

`RELEASING.md` is the authoritative release runbook. `.github/workflows/ci.yml` validates master across Linux, macOS, and Windows. `.github/workflows/release.yml` builds Linux x86-64, Linux ARM64, and universal macOS archives, creates a complete GitHub Release, dispatches Rust publication, and announces it. `.github/workflows/publish.yml` publishes the eight Rust crates through the `crates-io` GitHub environment. `.github/workflows/publish-npm.yml` packages and publishes `@brokkai/mjolnir` plus the Darwin, Linux x64, and Linux ARM64 platform packages through `npm-publish`.

The feature baseline is release commit `14b99cb3679de7b18121ce022169e1052cac1a54` (`v2.4.0`). The release candidate starts from master commit `c03b2bbac0eefe3e46e9d47441e8253393eea6a5`.

## Plan of Work

First create this plan and commit it with the version manifest, lockfile, and license report. Then validate the exact clean commit. Push only that commit to `origin/master`, inspect the CI run for its exact SHA, and repair an actual failure on master if one appears. After CI and publisher checks are satisfied, create an annotated `v2.5.0` tag at the validated commit and push the tag. Monitor the GitHub, crates.io, and npm workflows to completion. Finally verify public artifacts and registry versions before recording the release as complete.

## Concrete Steps

Run from `/home/ryan/code/mjolnir`. Cargo tests must run outside the restricted sandbox because they exercise loopback sockets and PTYs.

    node scripts/release-version.mjs check v2.5.0
    cargo fmt --check
    cargo clippy --locked --workspace --all-targets -- -D warnings
    cargo test --locked
    cargo build --locked --release
    cargo build --locked --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    cargo clippy --locked --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker -- -D warnings
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    npm --prefix docs run check
    npm --prefix docs run build
    node scripts/release-version.mjs check v2.5.0

Generate a second license report and supplemental notice file under `target/`, then compare them byte-for-byte with the committed files. Confirm the working tree is clean and record the candidate SHA. Push the candidate with `git push origin master`. Monitor CI with `gh run watch <run-id> --repo BrokkAi/mjolnir --exit-status`. After all jobs pass, create the immutable release with:

    git tag -a v2.5.0 <validated-commit> -m 'Mjolnir v2.5.0'
    git push origin v2.5.0

Monitor the release, crates.io, and npm workflows. Query the GitHub Release assets, verify checksums and the Linux binaries, and query every registry package for 2.5.0.

## Validation and Acceptance

All local commands above must exit successfully against the clean release commit, and the CI run for that exact SHA must pass every job. The annotated tag must resolve to that commit and must never be moved after publication.

GitHub must expose the Linux x86-64, Linux ARM64, and universal macOS archives with matching SHA-256 sidecars. A checksum-verified Linux `mj` binary must report `2.5.0`. All eight Rust crates must expose 2.5.0 and not be yanked. All four npm packages must expose 2.5.0, with `latest` pointing to 2.5.0 for the root wrapper and platform packages.

## Idempotence and Recovery

Version synchronization, notice generation, builds, tests, and packaging are safe to rerun. Never force-push master, rewrite history, or move a published tag. If publication partially completes, rerun the affected workflow: Rust and npm steps skip versions already published. If any validation fails, fix the source, commit a replacement candidate, and repeat exact-commit validation before tagging.

## Artifacts and Notes

The tagged release changes only `Cargo.toml`, `Cargo.lock`, `licenses/THIRD_PARTY_LICENSES.html`, and the initial revision of this plan. Pre-commit packaging produced all eight `target/package/brokk-*-2.5.0.crate` files. The known cargo-deny unmatched libbz2 exception warning is not a failure.

Published asset SHA-256 values are:

    brokk-mjolnir-v2.5.0-aarch64-unknown-linux-gnu.tar.gz  f3086954cc8a26d32b9a4570d94d70dcb5c71b374bd05bc523bbb20fa26c652e
    brokk-mjolnir-v2.5.0-universal-apple-darwin.tar.gz     5555f2e573bae11db35ca713be29dbdc4cd28a7ad138725bf30329f296d5f8f0
    brokk-mjolnir-v2.5.0-x86_64-unknown-linux-gnu.tar.gz   3888aac3b0419c74ae08316485f9e3a86f578fea1f1101e1a07316fcb4a177f9

The checksum-verified Linux controller reports `mj 2.5.0`, and its bundled x86-64 musl worker reports `mj-worker 2.5.0`. Downloaded archives and verification outputs remain under `target/release-v2.5.0-assets/`; local notice comparisons remain under `target/release-v2.5.0-checks/`.

## Interfaces and Dependencies

Use Rust 1.96.0, cargo-about 0.9.1, cargo-deny 0.20.2, and Node 24. Do not add dependencies, crates, workflows, package identities, publisher settings, or registry triggers in this release commit.

Revision 2026-09-09: created the self-contained release plan after synchronizing 2.5.0 metadata and recording initial validation and publisher status.

Revision 2026-09-09: recorded the clean candidate, successful exact-commit CI and publication workflows, immutable tag identity, verified assets and registry versions, and public release notes.
