# Release Mjolnir v2.1.0

This ExecPlan follows `.agents/PLANS.md` and `RELEASING.md`.

## Purpose / Big Picture

Publish the current master work, including Muse Code support and durable session moves, as the next minor release after v2.0.0. Push the release commit before tagging, as explicitly requested by the user.

## Progress

- [x] Confirm upstream latest v2.0.0 and current master includes origin/master without missing upstream commits.
- [x] Select v2.1.0 and synchronize workspace manifest and lockfile versions.
- [ ] Regenerate and validate license reports; validate release builds, tests, formatting, Clippy and packaging.
- [ ] Commit and push release preparation, verify clean-commit release version, and wait for master CI.
- [ ] Create/push annotated v2.1.0 tag after validations pass; monitor release and publication workflows.

## Surprises & Discoveries

The image workflow still used containers/ as its build context, but Muse installation now copies its script and manifest from the repository root. Change the context to the repository root and include both new inputs in path triggers before publishing. Local full-image validation already used that correct root context.

## Decision Log

Use a minor version because Muse support adds functionality. Keep all work on current master. The user's release request authorizes pushing Mjolnir and its release tag and the normal automatic registry/image publication workflows. Do not tag until the release commit passes required local checks and master CI.

## Context and Orientation

Cargo.toml owns the workspace version; scripts/release-version.mjs synchronizes internal dependencies. Cargo.lock and licenses/THIRD_PARTY_LICENSES.html must embed the new version. .github/workflows/ci.yml validates master, release.yml publishes complete archives after platform builds, and publish.yml/publish-npm.yml publish registries afterward. publish-agent-dev-image.yml updates the agent image on relevant master changes.

## Plan of Work

Synchronize versions and generate legal reports using pinned cargo-about 0.9.1 and cargo-deny 0.20.2. Fix image build context/trigger inputs. Review the diff and commit only release files. Run clean-commit cargo fmt --check, cargo test, cargo clippy --all-targets -- -D warnings, cargo build --release, a portable musl worker build, license checks and node scripts/release-version.mjs check v2.1.0. Push master and wait for CI, correcting any failures before tagging. Push an annotated version tag, monitor GitHub artifacts and registry publishing, and report actual completion or concrete external blockers.

## Validation and Acceptance

All required checks pass on the exact release commit. Published GitHub archives carry v2.1.0 and checksums; automatic registry workflows finish or have explicitly reported external gates. The Muse integration already passed real authenticated checkpoint restoration and full local container build/runtime tests in the previous commits.

## Idempotence and Recovery

Regenerate files safely and preserve unrelated changes. Never move an already-published release tag or overwrite immutable registry versions. Before retrying failed publication, inspect which assets/crates already exist and follow RELEASING.md's recovery path. Never push a tag from a dirty or unvalidated commit.

## Outcomes & Retrospective

Release preparation in progress.
