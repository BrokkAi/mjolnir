# Release Mjolnir v2.1.0

This ExecPlan follows `.agents/PLANS.md` and `RELEASING.md`.

## Purpose / Big Picture

Publish the current master work, including Muse Code support and durable session moves, as the next minor release after v2.0.0. Push the release commit before tagging, as explicitly requested by the user.

## Progress

- [x] Confirm upstream latest v2.0.0 and current master includes origin/master without missing upstream commits.
- [x] Select v2.1.0 and synchronize workspace manifest and lockfile versions.
- [x] Regenerate and validate license reports; pass formatting, full tests, Clippy, release CLI and portable musl worker builds, all-workspace checks, and archive creation for all eight packages.
- [x] Commit and push release preparation and cross-platform fixes through 75be7e68; pass clean-commit release version check.
- [x] Publish brokk-anvil-client 0.28.1 under foundev ownership, configure its trusted publisher, and replace every old-package consumer with the registry dependency.
- [x] Read back and verify the exact trusted publisher for all three Anvil and all eight Mjolnir release crates.
- [x] Pass full workspace tests, workspace/all-target Clippy, formatting, version and license checks against the published replacement.
- [ ] Finish release builds and extracted-package verification; push the final candidate and await CI.
- [ ] Wait for master CI on the final release commit; workflow 34108548549 for 75be7e68 remains in progress, with voice and three-client checks successful.
- [ ] Create/push annotated v2.1.0 tag after validations pass; monitor release and publication workflows.

## Surprises & Discoveries

The image workflow still used containers/ as its build context, but Muse installation now copies its script and manifest from the repository root. Change the context to the repository root and include both new inputs in path triggers before publishing. Local full-image validation already used that correct root context.

Packaging from the developer checkout exposed recursive README/LICENSE glob matches that included ignored node_modules files. Anchor those two allowlist entries to the crate root; all eight workspace archives then package successfully. The previous master CI run also had a real macOS Ctrl-C cancellation failure and Unix-only test imports/constants compiled unused on Windows. Keep Ctrl-C cancellation platform-independent while retaining the existing dashboard accelerator, and gate only the Unix-only test helpers. The previous voice failure was a registry download error; current voice CI passes. The Muse-enabled agent image is already published for amd64 and arm64 by workflow 34108118216.

Successful source builds and archive creation do not prove registry publication works. Running `cargo package --locked -p brokk-mj-voice-worker` on the clean release candidate failed while compiling the extracted package: `unresolved import anvil_llm::transcribe` at src/backend.rs:1 and `no method named transcribe found for struct CodexClient` at src/backend.rs:647. The workspace uses Anvil Git revision 73ca21b23632a5082884ab0adbc774eb5c64e4da with version 0.27.1, but Cargo replaces Git dependencies with their registry versions when packaging. The latest published brokk-anvil-llm is 0.27.1 and lacks those APIs. Normal voice CI uses the Git revision and therefore passes.

## Decision Log

Use a minor version because Muse support adds functionality. Keep all work on current master. The user's release request authorizes pushing Mjolnir and its release tag and the normal automatic registry/image publication workflows. Do not tag until the release commit passes required local checks and master CI.

Hold the Mjolnir tag rather than trigger a known failing, potentially partial registry release. Ask for authorization to publish the updated BrokkAi/anvil dependency; prior upstream release authorization concerned muse-acp. Once authorized, follow that repository's release instructions, publish the required API, update Mjolnir's dependency and lockfile/license report, repeat affected validations including extracted voice-package verification, push the final candidate, and await its CI before tagging.

The subsequently authorized Anvil v0.28.0 attempt published its minimizer, then failed with HTTP 403 for the old LLM crate because the release identity lacked access. The user now explicitly chooses to replace that package completely with brokk-anvil-client owned by foundev and shared like the other Anvil crates. This supersedes the interrupted proposal to disable voice: no voice-removal edits were made. Anvil's replacement plan is /home/ryan/code/anvil/.agents/plans/replace-llm-crate.md. Prepare a source-only exact Git pin for development until the replacement is published; do not tag Mjolnir until it uses the published registry version and extracted-package verification passes. Verify publishing access per package, not merely registry existence, before any further release mutation.

The replacement is now published from Anvil commit 3e7ba29acf799658b7c91c528f850494b45fb093. Its direct owner is foundev, the Brokk engineering team is added, and jbellis and DavidBakerEffendi have pending owner invitations. Trusted publisher configuration 19317 matches BrokkAi/anvil, publish-crate.yml, environment release. Authenticated registry read-back confirmed the expected configuration for all eleven release crates across both repositories. Mjolnir now uses registry-only brokk-anvil-client 0.28.1; no temporary Git pin was needed. Controller utility inference, chat auth reading, and the voice worker all use the replacement.

## Context and Orientation

Cargo.toml owns the workspace version; scripts/release-version.mjs synchronizes internal dependencies. Cargo.lock and licenses/THIRD_PARTY_LICENSES.html must embed the new version. .github/workflows/ci.yml validates master, release.yml publishes complete archives after platform builds, and publish.yml/publish-npm.yml publish registries afterward. publish-agent-dev-image.yml updates the agent image on relevant master changes.

## Plan of Work

Synchronize versions and generate legal reports using pinned cargo-about 0.9.1 and cargo-deny 0.20.2. Fix image build context/trigger inputs. Review the diff and commit only release files. Run clean-commit cargo fmt --check, cargo test, cargo clippy --all-targets -- -D warnings, cargo build --release, a portable musl worker build, license checks and node scripts/release-version.mjs check v2.1.0. Push master and wait for CI, correcting any failures before tagging. Push an annotated version tag, monitor GitHub artifacts and registry publishing, and report actual completion or concrete external blockers.

## Validation and Acceptance

All required checks pass on the exact release commit. Published GitHub archives carry v2.1.0 and checksums; automatic registry workflows finish or have explicitly reported external gates. The Muse integration already passed real authenticated checkpoint restoration and full local container build/runtime tests in the previous commits.

## Idempotence and Recovery

Regenerate files safely and preserve unrelated changes. Never move an already-published release tag or overwrite immutable registry versions. Before retrying failed publication, inspect which assets/crates already exist and follow RELEASING.md's recovery path. Never push a tag from a dirty or unvalidated commit.

## Outcomes & Retrospective

Mjolnir v2.1.0 preparation and implementation fixes are pushed. No v2.1.0 tag or GitHub release has been created. The replacement package is published with foundev ownership and verified trusted publishing. Mjolnir validation against that registry version is in progress; final candidate commit, CI, tagging, and release publication remain.

Revision note: recorded the completed local validation and pushed checkpoints, and the experimentally confirmed dependency publication blocker so release work can resume without repeating discovery.

Revision note: recorded the explicit package-replacement decision, retained voice support, and the ownership/publication gates learned from the failed Anvil publication.

Revision note: recorded actual replacement publication, ownership/invitations, and verified publisher configurations; the registry access blocker is resolved.
