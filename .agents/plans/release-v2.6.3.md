# Publish Mjolnir v2.6.3

This living ExecPlan follows `.agents/PLANS.md` and the release requirements in `RELEASING.md`.

## Purpose / Big Picture

Publish the terminal dialog unification (predictable activation, protected drafts) and the tool-expansion hitbox fix merged since v2.6.2. Completion means a public GitHub release with complete verified archives, all nine Rust packages and four npm packages published at 2.6.3, and the Homebrew formula updated.

## Progress

- [x] (2026-09-11) Read the release runbook and prior release plans; confirmed origin/master is at 43b4fb5b with green CI run 34537117210; selected patch version 2.6.3.
- [x] (2026-09-11) Synchronized workspace metadata (six constraints, nine lockfile entries, license report versions) to 2.6.3; supplemental notices unchanged; no doc updates needed (dialog behavior matches documented click/double-click semantics).
- [ ] Run the full local validation suite against the clean candidate.
- [x] (2026-09-11) Authenticated read-back verified all nine crates.io publishers name BrokkAi/mjolnir, publish.yml, and crates-io (config ids incl. 19515 for brokk-mj-client); all nine show max_version 2.6.2. npm workflow and package identities unchanged since v2.6.2.
- [ ] Commit and fast-forward the candidate to origin/master; wait for exact-commit CI success.
- [ ] Create and push annotated tag v2.6.3 at the validated candidate.
- [ ] Monitor GitHub release, crates.io, and npm workflows; verify archives, registry versions, notes, and Homebrew.
- [ ] Record final publication evidence in this plan.

## Surprises & Discoveries

## Decision Log

- Decision: Release 2.6.3 as the next patch version and retain the normal release workflows.
  Rationale: The changes since v2.6.2 are TUI behavior refinements (dialog unification, hitbox fix) with no new advertised surface; prior patch releases shipped comparable user-facing improvements.
  Date/Author: 2026-09-11.
- Decision: Use the existing unchanged npm trusted publishers under the explicit user direction recorded in `.agents/plans/release-v2.3.0.md` and carried forward through v2.6.1.
  Rationale: That direction says to use the established pipeline without repeating the npm settings-inspection detour. Package names and workflow identities are unchanged.
  Date/Author: 2026-09-11.
- Decision: Prepare the release commit on the session branch and fast-forward it to origin/master after validation, instead of committing on a master checkout.
  Rationale: This session runs in an isolated worktree where the harness owns branch placement; the release commit lands on the same master line with identical content, satisfying the runbook's merged-candidate requirement.
  Date/Author: 2026-09-11.

## Outcomes & Retrospective

## Context and Orientation

`Cargo.toml` owns the workspace version and six internal dependency constraints. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` contain nine workspace package versions. `.github/workflows/ci.yml` validates Linux, macOS, and Windows, desktop, voice, licenses, and the three-client reliability scenario. `.github/workflows/release.yml` builds Linux x86-64, Linux ARM64, and universal macOS archives with bundled workers and checksum sidecars, then publishes a complete GitHub release. It dispatches Rust publication through `publish.yml`; `publish-npm.yml` publishes the npm platform packages and wrapper after the release exists. `BrokkAi/homebrew-tap` contains the manually maintained `Formula/mjolnir.rb`.

## Plan of Work and Milestones

First synchronize metadata, this plan, and `.agents/docs/release-v2.6.3-notes.md`. Run the complete local validations against that clean commit while CI validates the pushed candidate. Cargo uses a workspace-local `CARGO_HOME` seeded from the warm registry cache because the sandbox mounts the home cargo directory read-only. If any real validation fails, fix it, commit and push the correction, and validate that replacement candidate before tagging.

Second verify publisher authorization, a clean working tree, the release-version check, and successful exact-commit CI. Create the annotated tag at that exact commit and push it. Do not move a published tag.

Third monitor the GitHub Release and both registry workflows through completion. Verify all published checksums and bundled executables, every Rust version and npm latest tag, update the GitHub release body from the prepared notes, and update the Homebrew version and archive checksums while preserving its managed-install wrapper. Finally commit and push this plan's outcome record.

## Concrete Steps

From this worktree, keep logs and generated validation artifacts under `target/release-v2.6.3-checks/` with `CARGO_HOME=$PWD/target/cargo-home`:

    node scripts/release-version.mjs check v2.6.3
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

Generate a fresh report with cargo-about 0.9.1 and a supplemental report with `scripts/generate-supplemental-third-party-notices.mjs`, then compare both with the checked-in versions. Fast-forward the candidate to origin/master, inspect the CI run for the candidate SHA, and after every gate passes run `git tag -a v2.6.3 <validated-SHA> -m 'Mjolnir v2.6.3'` and `git push origin v2.6.3`. Set notes with `gh release edit v2.6.3 --notes-file .agents/docs/release-v2.6.3-notes.md --repo BrokkAi/mjolnir`.

## Validation and Acceptance

The tag resolves to the clean candidate that passes the full local checks and every CI job. GitHub publishes all three archives and SHA-256 sidecars. Downloaded archives match checksums and contain the controller, desktop, voice worker, and two portable workers; the Linux controller and worker report 2.6.3. Nine Rust crates expose non-yanked 2.6.3 and four npm packages expose 2.6.3 as latest. The remote Homebrew formula selects 2.6.3 with the published checksums.

## Idempotence and Recovery

Version sync, notice generation, checks, packaging, and downloads are repeatable. Never force-push or rewrite a published tag. Existing registry workflows skip versions already published, allowing recovery from partial publication. A settings-read failure is not proof of a broken publisher; preserve working configuration. Fix actual build or publication failures and rerun the affected workflow. Do not declare success while required channels are still pending.

## Artifacts and Notes

Publisher read-back is saved locally at `target/release-v2.6.3-checks/publisher-verification.json`, without credentials. Store release assets in `target/release-v2.6.3-assets/`. Prepared public notes are in `.agents/docs/release-v2.6.3-notes.md`.

## Interfaces and Dependencies

Use the pinned Rust 1.96.0 toolchain, cargo-about 0.9.1, cargo-deny 0.20.2, Node 24, Git, and GitHub CLI. Keep the existing package identities, workflow triggers, registry publishers, and dependency versions.
