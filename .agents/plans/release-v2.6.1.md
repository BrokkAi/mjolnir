# Publish Mjolnir v2.6.1


This living ExecPlan follows `.agents/PLANS.md` and the release requirements in `RELEASING.md`.

## Purpose / Big Picture


Publish the fix preventing unused Codex sessions from entering a resume crash loop, together with the changes already merged since v2.6.0. Completion means a public GitHub release with complete verified archives, all nine Rust packages and four npm packages published at 2.6.1, and the Homebrew formula updated.

## Progress


- [x] (2026-09-10) Read the release runbook, fetched origin, merged latest upstream changes on master, and pushed the fix in master commit 05dba932.
- [x] (2026-09-10) Selected patch version 2.6.1, synchronized six internal dependency constraints and nine lockfile entries, and regenerated the license report.
- [x] (2026-09-10) Authenticated read-back verified all nine crates.io publishers name BrokkAi/mjolnir, publish.yml, and crates-io. The npm package identities and publish-npm.yml are unchanged from v2.6.0.
- [x] (2026-09-10) Committed and pushed release candidate 9ffa67f4798d76d51dd78cde2f33c19f99390f9b; all 18 local release checks passed and exact-commit CI run 34499111503 completed successfully in all seven jobs.
- [x] (2026-09-10) Created and pushed annotated tag v2.6.1 at validated candidate 9ffa67f4; GitHub release run 34502375337 started.
- [x] (2026-09-10) GitHub release run 34502375337, crates.io run 34505711367, and npm run 34505723719 all succeeded. Verified all three public archives, nine non-yanked Rust versions, and four npm latest versions at 2.6.1. Published the prepared release notes and Homebrew formula commit 8158f43f9cbaf0c038841f9cc61635ca508d6c8c.
- [x] (2026-09-10) Recorded final publication evidence in this plan for the final documentation commit and public push.

## Surprises & Discoveries


Origin advanced during the preceding fix. The normal merge brought in shared path-input handling and home-directory expansion across terminal and web editors without conflicts. These changes are included in the release and its notes. No dependency changes beyond the nine workspace version entries were needed.

The first local parallel test invocation hit an existing updater executable fixture with `Text file busy (os error 26)`. The full suite then passed with `cargo test -- --test-threads=1`, including that exact test. Standard parallel CI tests passed on Linux and macOS. All other required local validation passed. Windows CI spent several minutes saving its cache after all validation steps had passed; the tag was pushed only after the whole run completed successfully.

While CI ran, master advanced with README-only commit 0aa24d2a. The release tag intentionally names the original exact candidate tested by CI; the README update remains intact on master.

## Decision Log


- Decision: Release 2.6.1 as the next patch version and retain the normal release workflows.
  Rationale: The requested release primarily fixes existing session reliability. The user explicitly authorized public source pushes, tagging, and publication.
  Date/Author: 2026-09-10, Codex.
- Decision: Use the existing unchanged npm trusted publishers under the explicit user direction recorded in `.agents/plans/release-v2.6.0.md`, carried forward from v2.3.0.
  Rationale: That direction says to use the established pipeline without repeating the npm settings-inspection detour. Package names and workflow identities have not changed. Rust publisher configurations were freshly verified; no settings will be changed.
  Date/Author: 2026-09-10, Codex.

## Outcomes & Retrospective


Mjolnir v2.6.1 is fully published at https://github.com/BrokkAi/mjolnir/releases/tag/v2.6.1. The exact candidate passed all 18 local release checks and all seven CI jobs before tagging. All three public archives match their checksum sidecars and contain the required executables and notices. The Linux controller and portable worker report 2.6.1; the macOS controller, desktop, voice worker, and local worker each contain Intel and Apple Silicon architectures. All nine Rust crates are published and non-yanked at 2.6.1, all four npm packages expose 2.6.1 as latest, and the public Homebrew formula matches the release checksums. There are no remaining publication steps.

The tag remains at 9ffa67f4798d76d51dd78cde2f33c19f99390f9b. Concurrent README changes through 84adf2ca were fast-forwarded on master before recording completion; no published release commit or tag was rewritten.

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

Public verification evidence:

Candidate CI: https://github.com/BrokkAi/mjolnir/actions/runs/34499111503 completed successfully.

GitHub release: https://github.com/BrokkAi/mjolnir/actions/runs/34502375337 completed successfully.

crates.io publishing: https://github.com/BrokkAi/mjolnir/actions/runs/34505711367 completed successfully.

npm publishing: https://github.com/BrokkAi/mjolnir/actions/runs/34505723719 completed successfully.

Homebrew formula: https://github.com/BrokkAi/homebrew-tap/commit/8158f43f9cbaf0c038841f9cc61635ca508d6c8c. The formula was regenerated using the tap script, reviewed to contain only version, URL, and checksum changes, published with a content-SHA check, and read back byte-for-byte. Its managed-install wrapper is preserved.

    brokk-mjolnir-v2.6.1-aarch64-unknown-linux-gnu.tar.gz
    SHA-256 943e0f7813716a825d42b62d45754056c506d578b2ab79f03f44011b563bf9a2

    brokk-mjolnir-v2.6.1-universal-apple-darwin.tar.gz
    SHA-256 dd364487f0ada250d6a9646d51b856b0ec2bb966973640bad4d9c4c8f0a36239

    brokk-mjolnir-v2.6.1-x86_64-unknown-linux-gnu.tar.gz
    SHA-256 b5bb1db2d95dba281aaa76b384cd9aef3b5a310750b15824e0ecdd49d40df7fa

## Interfaces and Dependencies


Use the pinned Rust 1.96.0 toolchain, cargo-about 0.9.1, cargo-deny 0.20.2, Node, Git, and GitHub CLI. Keep the existing package identities, workflow triggers, registry publishers, and dependency versions.

Revision 2026-09-10: created the release plan after pushing the fix, integrating upstream work, synchronizing metadata, and verifying Rust publishers.

Revision 2026-09-10: recorded successful exact-commit validation, tag publication, all registry and archive verification, the public Homebrew update, and preservation of concurrent README changes. This closes the release after every required channel became available.
