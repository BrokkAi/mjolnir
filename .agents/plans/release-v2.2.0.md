# Release Mjolnir v2.2.0

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


Publish the session-management improvements as a minor release. Users gain a global Sessions sidebar, quick task-first session creation, immediate stop/restart keys, yes/no deletion, and the F4 Setup modal. The existing full session wizard remains on Shift-N or Alt-W. Release downloads, all eight Rust crates, and all four npm packages should expose version 2.2.0.

## Progress


- [x] (2026-09-08) Read the release rules, fetch upstream, and confirm master contains the completed feature commit `6e0d0454` and is one commit ahead of origin/master.
- [x] Choose minor version 2.2.0 for the added user-facing features.
- [x] Synchronize the manifest, internal dependencies, lockfile, and generated license report; only workspace versions changed.
- [x] Pass formatting, all 2,681 default-member tests (16 intentionally ignored), host and portable-worker Clippy with warnings denied, host and portable-worker release builds, all-workspace/all-target compile checks including desktop and voice, license policy and notice checks, all eight source packages, and all 11 npm packaging tests.
- [ ] Validate, commit on master, push upstream, and verify CI on the exact release commit.
- [ ] Verify every existing registry publisher, create and push the version tag, and monitor publication.
- [ ] Verify downloaded artifacts and registry versions and record the result.

## Surprises & Discoveries


The installed license tools match CI: cargo-about 0.9.1 and cargo-deny 0.20.2. Existing local registry credentials are present. Authenticated crates.io read-back verified all eight configurations name BrokkAi/mjolnir, publish.yml, and crates-io. The npm trusted-publisher read currently requests one-time authentication; this is a settings-inspection limitation and does not establish a publishing failure. Browser discovery found no active browser sessions, and selecting an npm browser returned "No browser is available".

## Decision Log


Use 2.2.0 because this release adds substantial user-visible features beyond the repairs appropriate for a patch. Keep the existing release pipeline and publisher settings. The user explicitly authorized pushing the release. Preserve master and never rewrite a published tag.

## Outcomes & Retrospective


Local release validation passed. The version and notice changes are ready to commit and push for exact-commit CI. All Rust publishers are verified; npm settings inspection still requires one-time authentication. No tag or registry publication has been created yet.

## Context and Orientation


`Cargo.toml` owns the workspace version; all eight crates inherit it. `scripts/release-version.mjs sync` updates the five internal dependency constraints. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` also embed package versions. `RELEASING.md` requires clean-commit validation and verified publishing authorization before tagging. The GitHub workflows are `.github/workflows/ci.yml`, `release.yml`, `publish.yml`, and `publish-npm.yml`. Rust publishers must name BrokkAi/mjolnir, publish.yml, environment crates-io. npm publishers must name the same repository, publish-npm.yml, environment npm-publish.

## Plan of Work


First synchronize versions and regenerate notices, checking that unrelated dependencies do not change. Run formatting, tests, Clippy, release builds, portable-worker checks, license checks, and source packaging. Commit explicit changed files on master and push origin/master. CI must pass on the exact candidate, including macOS and Windows checks unavailable on this Linux host.

Once the release candidate and publisher settings are verified, create an annotated v2.2.0 tag at that candidate and push only that tag. Wait for the GitHub release and both registry workflows. Verify all archives and checksum sidecars, download the Linux archive and check its checksum and binary version, and query all package versions. Record any failure accurately and repair the cause before retrying a failed workflow.

## Concrete Steps


Run from `/home/ryan/code/mjolnir`:

    node scripts/release-version.mjs sync
    cargo update --workspace
    cargo about generate --workspace --offline --config licenses/about.toml --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html
    cargo fmt --check
    cargo test --locked
    cargo clippy --locked --all-targets -- -D warnings
    cargo build --locked --release
    cargo build --locked --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    node scripts/release-version.mjs check v2.2.0

All Cargo tests run with elevated sandbox permissions. Keep build output and validation logs under `target/`. Recheck the version and clean working tree on the committed candidate before tagging. Use `git push origin master`, then after green CI and publisher verification, `git tag -a v2.2.0 <validated-commit> -m 'Mjolnir v2.2.0'` and `git push origin v2.2.0`.

## Validation and Acceptance


All required checks must pass. The tag must identify the exact clean validated commit. GitHub must publish all three platform archives and checksum sidecars. All eight crates must exist at 2.2.0 and not be yanked. All four npm packages must publish 2.2.0 with latest pointing to that version. The checksum-verified Linux binary must report `mj 2.2.0`.

## Idempotence and Recovery


Version synchronization and notice generation are repeatable. Do not force-push master, move a published tag, or change publishers to resolve a read-access limitation. Registry workflows skip versions already published and can resume a partial release. Keep credentials out of logs and tracked files.

## Artifacts and Notes


The feature commit is `6e0d0454`; the previous release is v2.1.4. Local logs will use the `target/release-v2.2.0-` prefix.

## Interfaces and Dependencies


No external dependency or publisher changes are planned. Use the repository's Rust 1.96.0 toolchain, pinned license tools, and Node 24. Existing release automation creates a draft, attaches all assets, publishes the complete release, and starts registry publication.

Revision 2026-09-08: record the minor release scope, validation sequence, and existing publisher inspection status.

Revision 2026-09-08: record completed local validation and authenticated verification of all eight Rust publishers before committing the candidate.
