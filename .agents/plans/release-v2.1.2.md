# Release Mjolnir v2.1.2


This living ExecPlan follows `.agents/PLANS.md` and `RELEASING.md`.

## Purpose / Big Picture


Ship the workstation web layout and the session-transition fixes on master as patch release 2.1.2. The user requested a release after the desktop fix was committed as `1cc72989`. A completed release has downloadable GitHub archives, all eight Rust crates, and all four npm packages at 2.1.2.

## Progress


- [x] (2026-09-07) Confirm clean master, unchanged upstream, and latest release v2.1.1.
- [x] Identify the existing Windows CI failure: a Unix-only test imports `ProvisionStage` unconditionally. Apply the matching conditional import.
- [x] Set the workspace version to 2.1.2 and synchronize internal dependency constraints.
- [x] Refresh all eight workspace lock entries and regenerate the license report with cargo-about 0.9.1; the report changes only the eight workspace versions.
- [x] Pass formatting, version checking, 11 npm packaging tests, documentation diagnostics, and root/prefixed builds with 1691 internal links each. Supplemental notices and the launch-template helper also pass.
- [ ] Commit the release candidate and pass formatting, tests, Clippy, release build, license, packaging, and exact-commit master CI.
- [ ] Verify publisher authorization under the before-tagging requirement in RELEASING.md.
- [ ] Tag the validated candidate, publish through the existing workflows, and verify shipped artifacts and registries.

## Surprises & Discoveries


Master CI run 34141678837 failed only its Windows job because `mj-controller/src/hel_controller/checkpoint.rs` imported `ProvisionStage` into the test module even though its explicit test uses are Unix-only. The container has the pinned Rust toolchain and musl target but no build cache or installed license tools. Build and validation logs are stored under `target/release-verification/v2.1.2/`.

The preceding release plan records successful publication with the existing publishers on the same day. Its npm inspection override was explicitly scoped to that release; inspect current authorization before tagging this release. Do not infer that local inspection limitations mean the publishing pipeline is broken or that settings need changing.

Current inspection attempts returned HTTP 401 from `npm trust list @brokkai/mjolnir` and HTTP 403 from crates.io publisher configuration requests. This container has no registry inspection credentials. The GitHub `crates-io` and `npm-publish` environments exist and have no protection rules, but that alone does not verify registry authorization.

## Decision Log


Use 2.1.2 because the changes repair existing behavior. The release request authorizes pushing master and the release tag and using normal automatic publication. Stay on master, do not create a branch, and never move a published tag. The Windows correction changes only test compilation.

## Outcomes & Retrospective


Release preparation is in progress. No release tag has been created.

## Context and Orientation


`Cargo.toml` owns the shared package version. `scripts/release-version.mjs sync` updates published internal requirements. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` also embed workspace versions. `.github/workflows/ci.yml` validates master across supported build targets. `.github/workflows/release.yml` builds archives and publishes a GitHub Release after a version tag; `publish.yml` and `publish-npm.yml` publish registries afterward. All work is in `/workspace/mjolnir`.

## Plan of Work


First synchronize release files and fix the existing Windows compilation failure. Commit these files after formatting and version review. This produces a clean, reviewable candidate.

Next run release validations and push that exact candidate to master for CI, including platform checks unavailable locally. Confirm each job passes for the candidate SHA. Resolve any failure and validate the resulting candidate before tagging. Verify publisher configurations name repository `BrokkAi/mjolnir`, workflow `publish.yml` and environment `crates-io` for Rust, or `publish-npm.yml` and `npm-publish` for npm. If inspection remains unavailable, complete preparation before requesting the specific instruction needed to proceed.

Finally create an annotated v2.1.2 tag at the clean validated candidate and push it. Follow release and registry workflows through completion. Download the Linux x64 archive, check its SHA-256 sidecar, and run `mj --version`; verify the version exists for every published package.

## Concrete Steps


From the repository root, run `node scripts/release-version.mjs sync`, `cargo update --workspace`, and `cargo about generate --workspace --offline --config licenses/about.toml --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html`. Install pinned cargo-about 0.9.1 and cargo-deny 0.20.2 into `target/release-tools` if necessary.

Run `cargo fmt --check`, `cargo test --locked`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo build --release --locked`, `cargo deny --workspace --config licenses/deny.toml --locked check licenses`, `cargo package --locked --workspace --no-verify`, and `node scripts/release-version.mjs check v2.1.2` on the clean release candidate. Use explicit musl worker builds and exact-commit CI for cross-platform verification. Run npm packaging tests from `npm/`. The viewer change already passed 15 unit and 27 browser tests, including desktop sizes through 2560px.

Use `git add` with explicit changed paths, commit on master, and push upstream. Inspect `gh run list` and `gh run view` for the exact candidate. After all prerequisites pass, use `git tag -a v2.1.2 <candidate> -m 'Mjolnir v2.1.2'` and push only that tag. Inspect the GitHub Release and each registry directly after the workflows finish.

## Validation and Acceptance


The exact release commit must be clean, match v2.1.2 throughout all eight workspace packages, and pass required validations before tagging. GitHub must expose all three archives and checksum sidecars. All eight crates must exist at 2.1.2 and not be yanked; all four npm packages must expose 2.1.2 as latest. The downloaded Linux binary must report `mj 2.1.2` after checksum verification.

## Idempotence and Recovery


Version synchronization and notice generation are repeatable. Preserve concurrent upstream work; do not force-push or retag a published version. Inspect failures before retrying workflows. Registry publishing skips versions already present, allowing partial publication to resume safely. Keep credentials out of output and tracked documents.

## Artifacts and Notes


The preceding release's successful publication runs are 34137744246 (GitHub), 34140415827 (Rust), and 34140423735 (npm). These are historical evidence, not a substitute for this candidate's validation.

## Interfaces and Dependencies


No new product dependencies or publisher configuration changes are planned. Use pinned Rust 1.96.0, cargo-about 0.9.1, and cargo-deny 0.20.2. Retain the existing release pipeline.

Revision 2026-09-07: record release scope, Windows CI repair, validation sequence, and publisher verification requirement.

Revision 2026-09-07: record synchronized notices, completed packaging/docs checks, and the current publisher inspection limitation before committing the candidate.
