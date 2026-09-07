# Release Mjolnir v2.1.1


This living ExecPlan follows `.agents/PLANS.md` and `RELEASING.md`.

## Purpose / Big Picture


Publish the session-opening deadlock fix, web viewer port recovery, and concurrent master fixes as patch release 2.1.1. The user explicitly authorized pushing, validating master, and cutting a release. Complete GitHub artifacts and both registry publications, verifying the actual shipped version.

## Progress


- [x] Confirm latest release 2.1.0, clean local master, and upstream origin/master.
- [x] Fetch and merge concurrent upstream work after the initial push was rejected. No conflicts occurred.
- [x] Verify all eight current crates.io trusted publishers match BrokkAi/mjolnir, publish.yml, crates-io.
- [ ] Verify npm trusted-publisher settings; API read currently returns 401, and the user has been asked for settings confirmation or login.
- [x] Synchronize 2.1.1 versions and license report; repair readiness detection using structured daemon state; document cancellable session attachment and unconfirmed draft saves. License policy, supplemental report comparison, formatting, npm packaging tests, docs build/1691 links, and isolated reliability plus HTTPS recovery scenarios pass.
- [x] Commit and push release preparation as fcb08c72; docs CI passed.
- [x] Correct the upstream Linux-only test guard and push release candidate e7868de8701aa3b89eea00aae49f92fcf7186146.
- [x] Complete local full-workspace tests, strict clippy, release builds, all eight source archives, portable musl build/clippy, license checks, documentation and focused end-to-end scenarios.
- [x] (2026-09-07 14:40 UTC) Pass exact-candidate CI run 34131797482: all seven jobs concluded successfully across Linux, macOS, Windows, desktop, voice, licenses and reliability.
- [ ] Resolve npm access verification before tagging.
- [ ] Check version against clean commit, create and push annotated v2.1.1, and monitor artifacts plus both registry workflows.

## Surprises & Discoveries


Upstream master advanced to c68f1356 with project grouping, move-dialog fixes, and daemon ownership handoff. The merged candidate retains these. Earlier CI 34127806733 failed because the reliability harness expected a viewer URL without a trailing slash, while the recovered viewer now reports its canonical URL with a slash. Replace presentation-text parsing with the existing authenticated daemon request helper and structured ready state.

CI 34130309251 additionally showed an upstream test calling Linux-only `executable_file_identity` on macOS and Windows. Gate that test with the same `target_os = "linux"` condition as the helper. The actual release runtime behavior is unchanged by this test-only correction.

## Decision Log


Use the next patch version, 2.1.1, because this release repairs the existing 2.1.0 behavior. Stay on master and merge upstream normally. The release request authorizes source/tag pushes and normal automatic publication, but does not waive the repository's per-package publisher verification. All eight crates.io configurations were read back with the existing credential. The npm settings endpoint requires authentication unavailable in this tool session; obtain the missing verification while preparing everything else.

## Context and Orientation


`Cargo.toml` owns the workspace version, and `scripts/release-version.mjs sync` updates internal dependency constraints. `Cargo.lock` and `licenses/THIRD_PARTY_LICENSES.html` embed all eight workspace versions. `.github/workflows/ci.yml` validates master across Linux, macOS, Windows, desktop, voice, licensing, and a three-client scenario. `.github/workflows/release.yml` builds and publishes platform archives after a version tag; `publish.yml` and `publish-npm.yml` then publish registries. `tests/e2e/reliability_lab.py` provides the daemon readiness helper used by multiple scenarios.

## Plan of Work


First repair readiness detection, update the workspace to 2.1.1, synchronize constraints, refresh only workspace lock entries, regenerate notices with pinned cargo-about, and commit these reviewable release files. This milestone is complete when version checking and the corrected isolated smoke scenario pass.

Then run the required clean-commit validations: formatting, full tests, clippy, release build, portable worker build, license policy/report checks, packaging and relevant workspace checks. Push the exact candidate to master and wait for every required CI job. Resolve failures before tagging; if the candidate changes, validate affected code and wait for CI on that new commit.

Finally verify publisher access for every package and confirm CI run 34131797482 concluded successfully. The intended tag target is e7868de8701aa3b89eea00aae49f92fcf7186146, which passed validation on clean master. Subsequent release-record-only commits do not change that candidate. Recheck the version and explicitly create the annotated tag at that validated commit, then push it. If runtime or packaging changes occur before release, choose and validate a new candidate instead. Monitor the GitHub Release and registry workflows to completion; verify all three platform archives/checksums, eight crates, and four npm packages. Record outcome in this plan in a subsequent documentation commit without changing the release tag.

## Concrete Steps


Work from `/home/ryan/code/mjolnir`. Run `node scripts/release-version.mjs sync`, `cargo update --workspace`, and `cargo about generate --workspace --offline --config licenses/about.toml --locked --fail licenses/about.hbs -o licenses/THIRD_PARTY_LICENSES.html` after the manifest bump. Commit explicit changed paths. Run `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build --release`, `cargo build --release --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker`, `cargo deny --workspace --config licenses/deny.toml --locked check licenses`, and `cargo package --locked --workspace --no-verify`. All Cargo tests run outside the restricted sandbox. Build output stays in the repository's target directory.

## Validation and Acceptance


The exact tagged commit must be clean and pass local release validation plus master CI. Use `gh run list/view` to check the exact head SHA and each job. Use `gh release view v2.1.1` and registry metadata to verify actual publication, not merely workflow dispatch. Failed publication is retried only after inspecting which immutable versions already shipped.

## Idempotence and Recovery


Never force-push master or move a published tag. Keep upstream work and user's state intact. Version sync and notice generation are repeatable. Registry workflows skip existing versions and resume after a partial failure. Do not tag with unresolved validation or publishing authorization.

## Outcomes & Retrospective


Release 2.1.1 is prepared and pushed, with local validation complete and every CI build/test check passing on e7868de8701aa3b89eea00aae49f92fcf7186146. CI run 34131797482 concluded successfully with all seven jobs green. No tag or release has been created. Publication is blocked by the required npm publisher configuration verification: the authenticated settings endpoint returns 401, and no authenticated browser or npm credential is available in this session. The user has been asked to confirm the exact settings or sign in to npm. Do not treat elapsed time, the successful 2.1.0 release, or package existence as that confirmation.

Once access is verified, target the validated candidate explicitly with `git tag -a v2.1.1 e7868de8701aa3b89eea00aae49f92fcf7186146 -m "Mjolnir 2.1.1"` and push the tag after the clean-commit version and completed-CI checks. The documentation-only checkpoint recording this outcome is intentionally outside the release candidate. It uses `[skip ci]` because it changes only this internal record; the tag remains fixed to the fully validated code commit rather than the record commit.

## Artifacts and Notes


Verified crate configuration IDs are voice 14285, core 17008, worker 19051, controller 19050, chat 19052, TUI 16990, desktop 17009, and CLI 8869. Every configuration names BrokkAi/mjolnir, publish.yml, environment crates-io. No credentials belong in this plan.

The four npm packages requiring confirmation are `@brokkai/mjolnir`, `@brokkai/mjolnir-darwin-universal`, `@brokkai/mjolnir-linux-x64-gnu`, and `@brokkai/mjolnir-linux-arm64-gnu`. Each must trust repository `BrokkAi/mjolnir`, workflow `publish-npm.yml`, environment `npm-publish`. Read with `npm trust list PACKAGE --json --registry=https://registry.npmjs.org` after authentication; this still returned E401 at 14:27 UTC on 2026-09-07.

CI evidence: https://github.com/BrokkAi/mjolnir/actions/runs/34131797482. Local `cargo test --workspace --locked` and `cargo clippy --workspace --all-targets --locked -- -D warnings` passed. The final affected CLI test rerun passed all 194 tests. `cargo build --release --locked --workspace` and `cargo package --locked --workspace --no-verify` passed. The musl worker has no ELF interpreter or dynamic dependencies. The isolated three-client scenario reported zero leaks; HTTPS recovery covered identifying the occupied port owner, confirmed daemon shutdown and retry, and choosing another port without stopping the existing owner.

## Interfaces and Dependencies


No new runtime dependencies are planned. Use the existing structured daemon protocol for readiness rather than parsing human-facing status prose. Use the repository's pinned cargo-about 0.9.1 and cargo-deny 0.20.2.

Revision: initial release plan records the upstream merge, known CI failure, and publisher verification status.

Revision 2026-09-07: record completed release preparation and validation, fix the tag target to the validated candidate, and preserve the exact npm prerequisite so a later continuation can finish publication without repeating completed work.
