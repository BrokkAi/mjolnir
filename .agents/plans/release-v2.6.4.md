# Publish Mjolnir v2.6.4

This living ExecPlan follows `.agents/PLANS.md` and the release requirements in `RELEASING.md`.

## Purpose / Big Picture

Publish safe setup discovery and session configuration repair. Setup preserves working settings while discovering new installations; a broken session configuration leaves the dashboard usable with clear repair guidance. Completion means a public GitHub release with complete verified archives, all nine Rust packages and four npm packages published at 2.6.4, and the Homebrew formula updated.

## Progress

- [x] (2026-09-11) Pushed implementation c4fbb1a6 and merged latest origin/master a91b267e into the isolated worktree, producing 4126b256.
- [x] (2026-09-11) Selected 2.6.4 because the existing v2.6.3 tag is already publishing.
- [x] (2026-09-11) Synchronized six dependency constraints, nine lockfile entries, and regenerated notices at 2.6.4. Formatting, Clippy, npm/browser/script tests, and docs checks/build pass. Full Rust tests and release build are running.
- [x] (2026-09-11) Continued the untagged release at the maintainer's request to publish current master. The candidate became 93d7fde8, which adds four commits after the preparation commit 0fa84fd5. The version stayed 2.6.4 because 2.6.4 was never tagged or published. Added the new user-facing changes to the release notes.
- [x] (2026-09-11) Validated 93d7fde8 locally: release-version check, formatting, Clippy, full Rust tests, release build, cargo-deny licenses, workspace packaging, npm, web unit, script, and docs checks all pass. Exact-commit CI run 34621595655 passed all nine jobs, including dependency licenses.
- [x] (2026-09-11) Verified publisher authorization from registry records. crates.io API tokens cannot read trusted-publisher settings, so the check used published versions instead. All nine crates published 2.6.3 today through GitHub trusted publishing from BrokkAi/mjolnir (run 34584449106). All four npm packages published 2.6.3 through GitHub Actions trusted publishing with SLSA provenance. The publish workflows are unchanged since v2.6.3. Evidence is in `target/release-v2.6.4-checks/publisher-verification.json`.
- [x] (2026-09-11) Pushed annotated tag v2.6.4 (25db6b6e) at 93d7fde8. Release run 34645623949 started.
- [x] (2026-09-11) Release run 34645623949 passed all seven jobs and published https://github.com/BrokkAi/mjolnir/releases/tag/v2.6.4 as latest. All three archives match their SHA-256 sidecars. Every archive contains `mj`, `mj-desktop`, `mj-voice-worker`, and both portable workers; the macOS archive also has the native `mj-worker`, and its four binaries are universal. The Linux x86-64 controller and worker report 2.6.4. Set the release body from `.agents/docs/release-v2.6.4-notes.md`.
- [x] (2026-09-11) Regenerated the Homebrew formula with the tap script. Only `Formula/mjolnir.rb` changed (version, URLs, checksums), its checksums match the downloaded archives, and the managed-install wrapper is unchanged. Pushed BrokkAi/homebrew-tap commit 46d22a3 and read it back from GitHub.
- [x] (2026-09-11) crates.io run 34648484912 succeeded. All nine crates expose non-yanked 2.6.4 as their newest version, each published through GitHub trusted publishing from BrokkAi/mjolnir by that run.
- [x] (2026-09-11) npm run 34648497466 succeeded on attempt 3. All four packages expose 2.6.4 as latest, each published through GitHub Actions trusted publishing with SLSA provenance.

## Surprises & Discoveries

Version 2.6.3 was publishing when this task began (GitHub Release run 34581835511). It must finish before 2.6.4 publication to avoid older package channels overwriting latest.

The npm workflow waits about one minute (12 checks, 5 seconds apart) for each published package to appear on the registry. The 45 to 95 MB packages took longer, so attempts 1 and 2 each published one package and then failed while waiting. Each rerun skipped the packages already visible, and attempt 3 published the rest. Lengthening that wait in `publish-npm.yml` would prevent this.

crates.io API tokens cannot read trusted-publisher settings, even for crate owners; the endpoint answers that the action is only available on the website. Version records expose `trustpub_data`, which verified the publishers instead.

## Decision Log

Use the requested worktree and current fix branch. Merge upstream with a normal merge and fast-forward the validated candidate to master without rewriting history. Use patch version 2.6.4 because these are compatibility and recovery fixes. Existing publisher identities and workflows remain unchanged; same-day authenticated Rust configuration evidence is available in the earlier worktree.

## Outcomes & Retrospective

Mjolnir v2.6.4 is fully published at https://github.com/BrokkAi/mjolnir/releases/tag/v2.6.4 from 93d7fde8. The candidate passed the full local checks and all nine CI jobs before tagging. All three archives match their checksums and contain the required executables. All nine Rust crates and all four npm packages expose 2.6.4, and the Homebrew formula selects 2.6.4 with the published checksums. No publisher configuration was changed. No publication steps remain.

## Context and Orientation

`Cargo.toml` owns the workspace version and six internal dependency constraints. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` contain nine workspace package versions. `.github/workflows/ci.yml` validates Linux, macOS, and Windows, desktop, voice, licenses, and the three-client reliability scenario. `.github/workflows/release.yml` builds Linux x86-64, Linux ARM64, and universal macOS archives with bundled workers and checksum sidecars, then publishes a complete GitHub release. It dispatches Rust publication through `publish.yml`; `publish-npm.yml` publishes the npm platform packages and wrapper after the release exists. `BrokkAi/homebrew-tap` contains the manually maintained `Formula/mjolnir.rb`.

## Plan of Work and Milestones

First synchronize metadata, this plan, and `.agents/docs/release-v2.6.4-notes.md`. Run the complete local validations against that clean commit while CI validates the pushed candidate. Cargo commands run with elevated permissions and normal local build storage. If any real validation fails, fix it, commit and push the correction, and validate that replacement candidate before tagging.

Second verify publisher authorization, a clean working tree, the release-version check, and successful exact-commit CI. Create the annotated tag at that exact commit and push it. Do not move a published tag.

Third monitor the GitHub Release and both registry workflows through completion. Verify all published checksums and bundled executables, every Rust version and npm latest tag, update the GitHub release body from the prepared notes, and update the Homebrew version and archive checksums while preserving its managed-install wrapper. Finally commit and push this plan's outcome record.

## Concrete Steps

From this worktree, keep logs and generated validation artifacts under `target/release-v2.6.4-checks/`:

    node scripts/release-version.mjs check v2.6.4
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo build --release
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    npm --prefix tests/e2e/web run test:unit
    node --test scripts/run.test.mjs
    npm --prefix docs run check
    npm --prefix docs run build

Generate a fresh report with cargo-about 0.9.1 and a supplemental report with `scripts/generate-supplemental-third-party-notices.mjs`, then compare both with the checked-in versions. Fast-forward the candidate to origin/master, inspect the CI run for the candidate SHA, and after every gate passes run `git tag -a v2.6.4 <validated-SHA> -m 'Mjolnir v2.6.4'` and `git push origin v2.6.4`. Set notes with `gh release edit v2.6.4 --notes-file .agents/docs/release-v2.6.4-notes.md --repo BrokkAi/mjolnir`.

## Validation and Acceptance

The tag resolves to the clean candidate that passes the full local checks and every CI job. GitHub publishes all three archives and SHA-256 sidecars. Downloaded archives match checksums and contain the controller, desktop, voice worker, and two portable workers; the Linux controller and worker report 2.6.4. Nine Rust crates expose non-yanked 2.6.4 and four npm packages expose 2.6.4 as latest. The remote Homebrew formula selects 2.6.4 with the published checksums.

## Idempotence and Recovery

Version sync, notice generation, checks, packaging, and downloads are repeatable. Never force-push or rewrite a published tag. Existing registry workflows skip versions already published, allowing recovery from partial publication. A settings-read failure is not proof of a broken publisher; preserve working configuration. Fix actual build or publication failures and rerun the affected workflow. Do not declare success while required channels are still pending.

## Artifacts and Notes

Publisher read-back is saved locally at `target/release-v2.6.4-checks/publisher-verification.json`, without credentials. Store release assets in `target/release-v2.6.4-assets/`. Prepared public notes are in `.agents/docs/release-v2.6.4-notes.md`.

## Interfaces and Dependencies

Use the pinned Rust 1.96.0 toolchain, cargo-about 0.9.1, cargo-deny 0.20.2, Node 24, Git, and GitHub CLI. Keep the existing package identities, workflow triggers, registry publishers, and dependency versions.

Revision 2026-09-11: initialized for the requested release after pulling latest master.

Revision 2026-09-11: recorded publication of 93d7fde8 as v2.6.4, the registry-based publisher verification, the npm reruns, and verification of every channel.
