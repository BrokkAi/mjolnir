# Publish Mjolnir v2.6.1

This living ExecPlan follows `.agents/PLANS.md` and the release requirements in `RELEASING.md`.

## Purpose / Big Picture

Publish the fix preventing unused Codex sessions from entering a resume crash loop, together with the changes already merged since v2.6.0. Completion means a public GitHub release with complete verified archives, all nine Rust packages and four npm packages published at 2.6.1, and the Homebrew formula updated.

## Progress

- [x] (2026-09-10) Read the release runbook, fetched origin, merged latest upstream changes on master, and pushed the fix in master commit 05dba932.
- [x] (2026-09-10) Selected patch version 2.6.1, synchronized six internal dependency constraints and nine lockfile entries, and regenerated the license report.
- [x] (2026-09-10) Authenticated read-back verified all nine crates.io publishers name BrokkAi/mjolnir, publish.yml, and crates-io. The npm package identities and publish-npm.yml are unchanged from v2.6.0.
- [ ] Commit and push the release candidate; pass local release validation and exact-commit CI before tagging.
- [ ] Create and push v2.6.1 at the validated clean release commit.
- [ ] Verify successful GitHub, crates.io, and npm publication and update public notes and Homebrew.
- [ ] Record final publication evidence and push the release record.

## Surprises & Discoveries

Origin advanced during the preceding fix. The normal merge brought in shared path-input handling and home-directory expansion across terminal and web editors without conflicts. These changes are included in the release and its notes. No dependency changes beyond the nine workspace version entries were needed.

## Decision Log

- Decision: Release 2.6.1 as the next patch version and retain the normal release workflows.
  Rationale: The requested release primarily fixes existing session reliability. The user explicitly authorized public source pushes, tagging, and publication.
  Date/Author: 2026-09-10, Codex.
- Decision: Use the existing unchanged npm trusted publishers under the explicit user direction recorded in `.agents/plans/release-v2.6.0.md`, carried forward from v2.3.0.
  Rationale: That direction says to use the established pipeline without repeating the npm settings-inspection detour. Package names and workflow identities have not changed. Rust publisher configurations were freshly verified; no settings will be changed.
  Date/Author: 2026-09-10, Codex.

## Outcomes & Retrospective

The fix is already public on master. Release metadata and notes are prepared; tagging and publication remain pending validation.

## Context and Orientation

`Cargo.toml` owns the workspace version and six internal dependency constraints. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` contain nine workspace package versions. `.github/workflows/ci.yml` validates Linux, macOS, and Windows, desktop, voice, licenses, and the three-client reliability scenario. `.github/workflows/release.yml` builds Linux x86-64, Linux ARM64, and universal macOS archives with bundled workers and checksum sidecars, then publishes a complete GitHub release. It dispatches Rust publication through `publish.yml`; `publish-npm.yml` publishes the npm platform packages and wrapper after the release exists. `BrokkAi/homebrew-tap` contains the manually maintained `Formula/mjolnir.rb`.

## Plan of Work and Milestones

First commit synchronized metadata, this plan, and `.agents/docs/release-v2.6.1-notes.md`. Run the complete local validations against that clean commit while CI validates the pushed candidate. All tests run outside the restricted sandbox. If any real validation fails, fix it on master, commit and push the correction, and validate that replacement candidate before tagging.

Second verify publisher authorization, a clean working tree, the release-version check, and successful exact-commit CI. Create the annotated tag at that exact commit and push it. Do not move a published tag.

Third monitor the GitHub Release and both registry workflows through completion. Verify all published checksums and bundled executables, every Rust version and npm latest tag, update the GitHub release body from the prepared notes, and update the Homebrew version and archive checksums while preserving its managed-install wrapper. Finally commit and push this plan's outcome record.

## Concrete Steps

From `/home/ryan/code/mjolnir`, keep logs and generated validation artifacts under `target/release-v2.6.1-checks/`:

    node scripts/release-version.mjs check v2.6.1
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
    cargo build --release
    cargo build --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
    cargo clippy --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker -- -D warnings
    cargo deny --workspace --config licenses/deny.toml --locked check licenses
    cargo package --locked --workspace --no-verify
    npm --prefix npm test
    npm --prefix tests/e2e/web run test:unit
    node --test scripts/run.test.mjs
    npm --prefix docs run check
    npm --prefix docs run build

Generate a fresh report with cargo-about 0.9.1 and a supplemental report with `scripts/generate-supplemental-third-party-notices.mjs`, then compare both with the checked-in versions. Use `git push origin master`, inspect the CI run for the candidate SHA, and after every gate passes run `git tag -a v2.6.1 <validated-SHA> -m 'Mjolnir v2.6.1'` and `git push origin v2.6.1`. Set notes with `gh release edit v2.6.1 --notes-file .agents/docs/release-v2.6.1-notes.md --repo BrokkAi/mjolnir`.

## Validation and Acceptance

The tag resolves to the clean candidate that passes the full local checks and every CI job. GitHub publishes all three archives and SHA-256 sidecars. Downloaded archives match checksums and contain the controller, desktop, voice worker, and two portable workers; the Linux controller and worker report 2.6.1. Nine Rust crates expose non-yanked 2.6.1 and four npm packages expose 2.6.1 as latest. The remote Homebrew formula selects 2.6.1 with the published checksums.

## Idempotence and Recovery

Version sync, notice generation, checks, packaging, and downloads are repeatable. Never force-push or rewrite a published tag. Existing registry workflows skip versions already published, allowing recovery from partial publication. A settings-read failure is not proof of a broken publisher; preserve working configuration. Fix actual build or publication failures and rerun the affected workflow. Do not declare success while required channels are still pending.

## Artifacts and Notes

Publisher read-back is saved locally at `target/release-v2.6.1-checks/publisher-verification.json`, without credentials. Store release assets in `target/release-v2.6.1-assets/`. Prepared public notes are in `.agents/docs/release-v2.6.1-notes.md`.

## Interfaces and Dependencies

Use the pinned Rust 1.96.0 toolchain, cargo-about 0.9.1, cargo-deny 0.20.2, Node, Git, and GitHub CLI. Keep the existing package identities, workflow triggers, registry publishers, and dependency versions.

Revision 2026-09-10: created the release plan after pushing the fix, integrating upstream work, synchronizing metadata, and verifying Rust publishers.
