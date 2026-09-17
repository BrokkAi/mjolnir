# Replace release bookkeeping with Cargo and tag-driven packaging

This plan follows `.agents/PLANS.md` and is maintained as implementation proceeds.

## Purpose / Big Picture

Maintainers should use cargo-release to bump workspace versions and commit the result, then push a release tag after CI passes. They should not synchronize duplicated files or regenerate committed license reports. Tag-triggered GitHub Actions generate and ship notices for the exact release revision. This task changes tooling, not the current version, and does not publish a release.

## Progress

- [x] Inspect current release scripts, Cargo packaging, notice generation, and CI consumers.
- [x] Configure cargo-release and remove the custom Node version/asset synchronization scripts.
- [x] Remove generated notice reports and duplicated assets; preserve source license texts and embedded runtime guides.
- [x] Generate notices in the tag workflow and include them in every final archive; update CI, tests, and maintainer instructions.
- [x] Verify real Cargo packages, plugin version changes in an isolated fixture, archive tests, and required Rust checks; commit only scoped changes.

## Surprises & Discoveries

Cargo supports workspace-inherited license-file and readme paths, and automatically includes those files in packages. Ordinary include patterns cannot reach out of a crate. Two target guides are embedded by mj-controller, so their single canonical copy must remain inside that crate or be represented by portable links. Generated HTML and supplemental reports currently exist twice and CI compares regeneration against both copies.

## Decision Log

Use cargo-release 1.1.5, with a shared workspace version and publication/tagging/pushing disabled for the preparation command. Keep publication triggered by explicit tags. Retain cargo-deny policy checks and audited native/font license inputs; remove generated aggregate reports, not the evidence needed to generate them. Generate reports once per release and pass them as artifacts to the three archive assembly jobs. Keep node-based supplemental native inventory because it performs project-specific auditing, not version/copy bookkeeping.

## Context and Orientation

`Cargo.toml` defines workspace versions and internal dependencies. `scripts/release-version.mjs` repeats logic cargo-release already supplies. `scripts/sync-package-assets.mjs` maintains committed copies. `.github/workflows/release.yml` builds binaries in parallel and assembles platform archives; `publish.yml` and `publish-npm.yml` distribute source crates and installers. `licenses/about.toml`, `about.hbs`, native legal files, and `generate-supplemental-third-party-notices.mjs` are source inputs for notices.

## Plan of Work

First configure the Cargo plugin and shared package metadata. Replace duplicated guides with one canonical crate-local source and update documentation-site input routing. Remove generated reports and copying scripts using explicit tracked paths. Next add a release-notices job that generates into target/release-notices, uploads them, and gates archive assembly but not binary compilation. Remove committed-report comparison from CI while preserving license policy and source-package checks. Finally test the plugin and package outputs, run required checks, and commit.

## Validation and Acceptance

Run `node --test scripts/release-workflow.test.mjs`, actual `cargo package --list` checks for all crates, and extract representative packages to verify LICENSE, README, and embedded guides. Exercise cargo-release in a disposable repository copy, never bumping or tagging the working repository during validation. Run all cargo tests outside the sandbox and Clippy with warnings denied. Archive tests must prove generated notices are included despite being absent from the source checkout. No remote release publication is part of validation.

## Idempotence and Recovery

Generated output stays under ignored target directories or workflow artifacts. Removed tracked copies remain recoverable in Git. Keep the root project LICENSE and audited third-party license sources. Never move an existing release tag. Cargo plugin dry runs are the default; execution is only used in isolated validation fixtures.

## Outcomes & Retrospective

Implemented and validated. The maintenance surface is Cargo metadata plus standard cargo-release configuration, not a replacement home-grown release script. The real workspace remains at 2.9.0; no release tag or publication was performed.

Plan created 2026-09-16 following the user's request for dramatically simpler Cargo-plugin and tag-driven releases.

Validation completed 2026-09-16: cargo-release 1.1.5 bumped an isolated project copy to 2.10.0, updated only Cargo.toml and Cargo.lock, and created one commit with no tag or push. All twelve source archives were assembled and their LICENSE contents compared against the shared root file; README and embedded guides matched canonical inputs, and the extracted core package built successfully. Full cargo tests, Clippy, cargo-deny, crate ownership checks, five workflow behavior tests, YAML parsing, changed-source formatting, and diff checks passed. Both release reports generated successfully under target/release-notices. The docs site built and checked 1,867 internal links across 25 pages.

Cargo emits a manifest warning when both SPDX license and license-file metadata are present. Both are intentionally retained: the SPDX declaration preserves the existing GPL-3.0-only policy, while license-file makes Cargo include the shared text automatically. Removing SPDX caused the detector to infer GPL-3.0-or-later and fail existing policy; that experiment was reverted rather than broadening allowed licenses. Existing upstream license inputs remain tracked, while removed copies and reports are recoverable from Git.

Follow-up 2026-09-16: the manifest warning is gone. `license-file` was removed
and each published crate now carries a `LICENSE` symlink to the root file,
which `cargo package` dereferences into the archive, so the SPDX declaration,
the single source text, and a warning-free build all hold at once.
