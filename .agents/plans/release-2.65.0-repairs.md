# Repair release CI and publish Mjolnir 2.65.0

This living ExecPlan follows `.agents/PLANS.md` and `RELEASING.md`.

## Purpose / Big Picture

The user requested release 2.65.0. The prepared release commit is `2f3c0c88f`, with matching workspace version, internal requirements, and Cargo.lock. Its tag started release run `41692026236`, but dependency, web, and Windows CI failed before publication. Repair the actual defects, retain version 2.65.0, and publish through the normal tagged workflow and registry channels.

## Progress

- [x] Prepared and pushed release commit `2f3c0c88f` and tag `v2.65.0`.
- [x] Identified three failed CI jobs, canceled the run, and verified no GitHub release or registry publication exists.
- [x] Diagnosed all completed failures and implemented the three minimal repairs; targeted verification passes.
- [x] Removed the four obsolete Auto-policy lines from the Code Review settings dialog, retaining reviewer-count guidance; both scoped goldens pass.
- [x] Ran only checks justified by the fixes; prepared the coherent repair checkpoint for commit.
- [x] Committed and pushed the repair as `fe6711857` on the current branch.
- [x] Required before-push review returned Clean for tree `c8fbfe0` before pushing.
- [x] Reconfirmed the release was absent, replaced only the unpublished tag at `fe6711857`, and started the new release run.
- [x] Release retry passed, GitHub release is published, crates.io publication passed, and the manual platform nightly passed.
- [x] Npm stable publication passed; final delivery is complete.

## Surprises & Discoveries

Local workspace tests and Clippy passed, but unused-dependency CI and the self-contained browser lane exercise additional contracts. Windows runs only on tags/nightly. The first run's completed-job logs were unavailable through `gh run view --log-failed` while other jobs remained active; cancellation and per-job log downloads are used to obtain the precise errors.

The dependency job reports exactly unused `serde` in `mj-review`; `serde_json` remains used by snapshot tests. The browser rendered four valid daemon/quota settings lines missing from its checked-in golden. Windows Clippy rejects `IsolatedTest::include_ignored`, whose sole caller is Unix-only; the helper needs the same platform guard. No additional job failed before cancellation; macOS tests were canceled and remain unverified until the retry.

During repair the user explicitly requested removal of the four outdated Auto-policy lines in the terminal Code Review settings dialog. This is a copy removal, not a change to reviewer selection; retain the one-to-three count recommendation and update the existing dialog and nested settings goldens.

## Decision Log

- Cancel the failed release and repair on top of its prepared commit, retaining version 2.65.0. This is the explicit failed-CI recovery procedure in `RELEASING.md`; never move a published tag.
- Delegate the three independent diagnoses before changing code. The parent owns design, Cargo.lock, commits, tag replacement, and publication acceptance. Do not weaken assertions or add skips to hide failures.
- Do not repeat local release CI. Targeted verification of necessary repairs is allowed; the new tagged workflow remains the full release gate.
- The session now exposes the before-push review tools. Commit the repair and call `request_review` before further pushes; a clean group or an explicitly justified complete-findings override is required.

## Outcomes & Retrospective

The CI repairs and requested settings copy removal are committed, reviewed, and pushed. Release retry `41698016483` passed at `fe6711857`; GitHub, crates.io, and npm stable publication are complete. Manual macOS/Windows nightly checks passed. No published tag was moved. The user's four obsolete Auto-policy lines are absent from the released Code Review dialog, while reviewer-count guidance remains.

## Context and Orientation

Run `41692026236`: <https://github.com/BrokkAi/mjolnir/actions/runs/41692026236>. Failed job ids: unused dependencies `126112761675`, web viewer `126112761928`, Windows `126112762336`. The version check, Linux main tests, desktop, voice, reliability, portable workers, and Linux CLI compatibility checks passed. Additional platform builds and macOS CI were still active when cancellation was requested.

## Plan of Work

Download completed failed-job logs and identify precise causes. Assign disjoint minimal fixes after reviewing the evidence. Preserve the plaintext review contract, all-clean parallel gate, durable recovery, configuration version 21, and daemon placement. Reserve Cargo.lock for the parent. Commit a coherent validated repair checkpoint on the current branch. If upstream moves, merge it rather than rebasing.

Verify GitHub has no published v2.65.0 release. Delete only the unpublished tag, recreate it at the fixed version-matching commit, and push it. Monitor the tagged release workflow and subsequent crates.io/npm jobs until publication completes or a concrete external approval blocks it. Do not install a test build into the live default instance.

## Validation and Acceptance

Acceptance requires repaired job contracts, a successful tag workflow, a published GitHub release with its platform archives, and successful normal stable package publication. Capture decisive logs, commands, and final links here. Every Cargo test runs elevated in dev profile with existing isolation; do not change cargo configuration or target storage.

## Idempotence and Recovery

Never move a published tag. Failed unpublished attempts follow the same runbook recovery. Registry jobs skip versions already published, so resume partial publication through their normal workflow rather than overwriting packages. Watch timeouts stop monitoring only and do not imply release failure or success.

## Artifacts and Notes

Initial watcher evidence: `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/release-{failed-jobs-window-1,watch-window-1}.log`.

Final canceled-run status and release absence: `release-final-status.json`, `release-presence-check.json`, and `release-cancelled-failed-logs.log` in that same report directory. Dependency diagnosis: `.mj/agents/57bbea10c099c4902c60532570967dcb/unused-dependencies-job126112761675.log` lines 502–519. Browser diagnosis and CI actual diff: `.mj/agents/b24130a99cf76839673ff79da9a2a39e/web-viewer-settings-actual.diff`. Windows diagnosis: `.mj/agents/6a2b350443f8aad357629ebf6f6210ae/windows-job-126112762336.log`.

The browser golden now matches CI's actual text exactly. The focused deterministic Playwright test passed (1/1), using the installed binary from `tests/e2e/web`: `./node_modules/.bin/playwright test settings.spec.js --project=deterministic --grep=golden_viewer_settings`; log `.mj/agents/b24130a99cf76839673ff79da9a2a39e/golden_viewer_settings-playwright-direct.log`. The initial npx wrapper attempt selected zero tests and is not counted.

Both scoped terminal goldens passed update and ordinary verification (four commands, one test each). Their only changes replace the four removed Auto-policy lines with blank rows. Logs `.mj/agents/b24130a99cf76839673ff79da9a2a39e/golden_{review_settings,settings_root}-autocopy-{update,verify}.log`. A source/expected-output search confirms the four quoted lines are absent from the dialog and those goldens.

`cargo metadata --offline --format-version 1 --filter-platform x86_64-unknown-linux-gnu` refreshed Cargo.lock by removing only the `serde` edge from `brokk-mj-review`; package versions remain 2.65.0. Cargo-machete 0.9.2 is already installed and matches CI's pin. Targeted dependency, review, and Clippy checks are running after all source writers finished.

Targeted repair verification passes: `cargo machete`; elevated dev-profile `TZ=UTC cargo test -p brokk-mj-review` (29 tests); `cargo clippy -p brokk-mj-controller -p brokk-mj-tui --all-targets -- -D warnings` (no warnings); `cargo fmt --all -- --check`. Logs `.mj/agents/15c4b40f651b0c5ef4f6ac34f01339ed/repair-{cargo-machete,review-tests,controller-tui-clippy,rustfmt}.log`. No test or build commands remain active before committing.

Repair commit `fe6711857` passed the required review, then pushed to master. GitHub release absence was reconfirmed immediately before deleting the failed remote tag. `v2.65.0` was recreated and pushed at that repair commit. Retry: <https://github.com/BrokkAi/mjolnir/actions/runs/41698016483>. The platform-sensitive helper fix also triggered the required manual nightly dispatch on master: <https://github.com/BrokkAi/mjolnir/actions/runs/41698027454>. Manual nightly runs do not publish releases. Monitoring now reports any failed job promptly rather than waiting for an entire long build window.

The retry, all platform archives, and platform nightly passed. Published GitHub release: <https://github.com/BrokkAi/mjolnir/releases/tag/v2.65.0>. Crates workflow completed successfully: <https://github.com/BrokkAi/mjolnir/actions/runs/41705977414>. Npm workflow completed successfully, including verification, packaging, and publishing platform packages before the wrapper: <https://github.com/BrokkAi/mjolnir/actions/runs/41706002125>. Its checkout resolved `v2.65.0`. Its metadata branch is `master` because it is a `workflow_run` trigger; filtering all workflow runs only by `headBranch == "v2.65.0"` incorrectly hides this publication. The workflow resolves its release tag from the triggering release workflow's head branch.

Final acceptance: tag `v2.65.0` remains at reviewed repair commit `fe6711857dd12f5a4ea1763ef8a1d760b386a318`. All release and package-channel workflows succeeded; no approval wait or active monitoring process remains. The initial failed attempt was never published and was replaced only after confirming GitHub release absence.

## Interfaces and Dependencies

No new product interface is planned. The driver owner removes the unused dependency from `mj-review/Cargo.toml`; the parent updates Cargo.lock. The presentation owner updates only `tests/e2e/web/golden/viewer_settings.txt` against the captured actual browser text. The host owner adds the platform guard in `mj-controller/src/controller/test_support.rs`. The parent owns integration, commit, tag replacement, and publication.

The presentation owner's follow-up ownership also includes `mj-tui/src/review_settings.rs` and the existing `review-settings.txt`/`settings-root.txt` goldens for the user-requested copy removal.
