# Release Mjolnir v2.6.0

This ExecPlan is maintained according to `.agents/PLANS.md`.

## Purpose / Big Picture


Publish the work merged after v2.5.0 and summarize it for users. Version 2.6.0 adds install-aware update checks, targeted background-task stopping, Kimi detached-agent protection, Darcula and high contrast themes, compact Setup choices, and conversation interface improvements. Successful completion means GitHub archives, nine Rust crates, and four npm packages expose 2.6.0 with concise release notes.

## Progress


- [x] (2026-09-09) Confirm clean `master` at `eae00ad429fb3b373e67cc665f1b66849f773d8e`, matching GitHub master and its successful CI run `34400663245`.
- [x] Choose minor version 2.6.0 and synchronize the workspace manifest, six internal dependency constraints, nine lockfile entries, and generated license report.
- [x] Verify all nine crates.io publishers name `BrokkAi/mjolnir`, `publish.yml`, and `crates-io` after the user obtained access to the new client crate.
- [x] Pass formatting, default-member Clippy, npm packaging tests, 24 web unit tests, and portable x86-64 musl worker build and Clippy.
- [x] Pass the full serialized Cargo suite, host release build, documentation check/build and 1,711 internal links, license checks and fresh notice comparisons, and all nine source packages.
- [ ] Commit the release candidate on master, validate its clean state, and push it for exact-commit CI.
- [ ] Confirm CI passes, create and push the annotated version tag, and monitor all publishing workflows.
- [ ] Verify release archives, registry versions, and Homebrew availability; publish notes and record completion.

## Surprises & Discoveries


The new `brokk-mj-client` crate was manually published as 2.5.0 after the preceding release. Its sole initial owner was `jbellis`, so the local credential could not inspect its trusted publisher. After the user was added, authenticated read-back verified configuration 19515 with the same repository, workflow, and environment as the other eight crates. At the user's request, `cargo owner --add github:brokkai:brokk-eng brokk-mj-client` added the Brokk engineering team, matching the other Mjolnir crates.

The broad web test command requires an externally provisioned reliability fixture (`MJ_BROWSER_BASE_URL`). Its 24 unit tests passed, but its Playwright phase could not start without that fixture. Use the repository's deterministic reliability harness, also exercised by CI, for that browser test.

The license check reports the previously known unmatched `libbz2-rs-sys@0.2.5` exception warning and exits successfully with `licenses ok`.

## Decision Log


Choose 2.6.0 because the changes add user-facing features beyond patch-level repairs. Keep the existing branch and publication workflows. The release request authorizes the source and tag pushes needed to publish it; never force-push or move a published tag.

Retain the unchanged npm pipeline under the explicit user direction recorded in `.agents/plans/release-v2.3.0.md` and carried forward in `.agents/plans/release-v2.5.0.md`: use the existing publisher configuration without repeating the npm settings-inspection detour. The four npm package identities and `publish-npm.yml` are unchanged since v2.5.0.

Run the full Cargo suite with one test thread because the preceding Kimi validation recorded PTY fixture interference under concurrency. This preserves the full test set while avoiding competing fixture startups.

## Outcomes & Retrospective


Local release preparation is validated and ready to commit. No tag has been created and no 2.6.0 package has been published.

## Context and Orientation


`Cargo.toml` owns the workspace version. Every workspace crate inherits it, while six internal path dependencies repeat registry constraints. `Cargo.lock` records nine local packages. `licenses/THIRD_PARTY_LICENSES.html` embeds their versions. The release commit should contain these synchronized files, this plan, and the notes under `.agents/docs/release-v2.6.0-notes.md`.

`RELEASING.md` is authoritative. `.github/workflows/ci.yml` checks master across Linux, macOS, and Windows, including desktop, voice, licenses, and a daemon/two-terminal/browser reliability test. `release.yml` creates complete Linux x86-64, Linux ARM64, and universal macOS archives before publishing the GitHub Release. It starts `publish.yml` for crates.io; `publish-npm.yml` packages and publishes the npm bundles. The Homebrew formula lives in the separate `BrokkAi/homebrew-tap` repository and requires a manual version/checksum update.

## Plan of Work and Milestones


First synchronize metadata and review release notes against `git log v2.5.0..HEAD` and the source changes. Run local validation and commit the coherent release preparation on master. Next check the clean candidate and push it to its configured upstream, then wait for CI on precisely that SHA. Only after successful checks and publisher verification should the annotated `v2.6.0` tag be created at that SHA and pushed. Finally monitor GitHub and registry workflows, verify downloaded archives and package versions, update public release notes and Homebrew as required, and record completion in a separate documentation commit.

## Concrete Steps


Run from `/home/ryan/code/mjolnir`; all Cargo tests run outside the restricted sandbox, and build artifacts stay under `target/`.

    node scripts/release-version.mjs check v2.6.0
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test -- --test-threads=1
    cargo build --release
    cargo build --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    cargo clippy --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker -- -D warnings
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    npm --prefix tests/e2e/web run test:unit
    npm --prefix docs run check
    npm --prefix docs run build

Generate fresh notice copies into `target/release-v2.6.0-checks/` with the pinned cargo-about 0.9.1 and supplemental notice script, then compare them to the committed reports. Review `git diff --check` and the exact staged paths. After committing, repeat clean-commit version and validation checks, push `master`, and inspect its exact-SHA CI run using `gh run view`.

Once all gates pass, run `git tag -a v2.6.0 <validated-SHA> -m 'Mjolnir v2.6.0'` and `git push origin v2.6.0`. Monitor the release and both registry workflows. Set release notes with `gh release edit v2.6.0 --notes-file .agents/docs/release-v2.6.0-notes.md`. Verify every archive and checksum, `mj --version`, the musl worker version, every Rust package version, and each npm `latest` tag.

## Validation and Acceptance


Every required local check and the exact-commit CI run must pass before tagging. The tag must resolve to the validated commit. All three published archives must match their SHA-256 sidecars; Linux binaries must report 2.6.0. Nine Rust crates must expose non-yanked 2.6.0 versions, and the root npm wrapper plus three platform packages must expose 2.6.0 with `latest` pointing to it. Release notes must accurately describe behavior since v2.5.0, including supported background-task cancellation and the remaining Codex limitation.

## Idempotence and Recovery


Metadata generation, checks, and source packaging are repeatable. Never rewrite history, force-push, or move the published tag. Fix any real validation failure at its source and validate a new candidate before tagging. Registry workflows skip versions already published, so rerunning a failed publishing workflow resumes a partial release. A local credential read error is not proof that Actions publishing is broken.

## Artifacts and Notes


Keep local reports, logs, and downloaded artifacts under `target/release-v2.6.0-checks/` and `target/release-v2.6.0-assets/`. The user-facing notes are stored as release preparation in `.agents/docs/release-v2.6.0-notes.md`, then copied into the GitHub Release body.

## Interfaces and Dependencies


Use Rust 1.96.0, Node 24, cargo-about 0.9.1, and cargo-deny 0.20.2. Do not add product dependencies, alter package identities, or change publisher settings as part of this release.

Revision 2026-09-09: record scope, synchronized metadata, initial checks, publisher verification, and remaining publication steps.
