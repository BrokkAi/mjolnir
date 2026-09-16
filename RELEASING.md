# Releasing Mjolnir

Pick a known-good commit, tag it, and push the tag. The commit does not need to
be current master or merged into master. Passing checks on that exact commit
count: do not rerun formatting, Clippy, tests, builds, or license generation
just to release it. A broken newer master does not block the release.

## Tag and release

The tag must match the version already committed in the selected commit's
`Cargo.toml`, internal dependency requirements, `Cargo.lock`, and license report.
For example, to release a known-good commit whose version is `1.2.3`:

```sh
git tag v1.2.3 <known-good-commit>
git push origin v1.2.3
```

Use the configured upstream remote if it is not `origin`. No branch switch,
merge, or new release commit is needed when the version is already prepared.
The workflow checks version consistency before building. Tags run the workflow
stored in the tagged commit, so older commits retain their older release workflow.

A release request authorizes pushing the tag and publishing through the normal
workflows. Registry authorization is configured once; do not repeat an ownership
or trusted-publisher audit for every release. If publication fails, fix the
reported problem and retry the failed job.

## When a version bump is needed

Prepare the version as an ordinary change before choosing the release commit:

1. Set `[workspace.package].version` in the root `Cargo.toml`.
2. Run `node scripts/release-version.mjs sync` and `cargo update --workspace`.
3. Regenerate `licenses/THIRD_PARTY_LICENSES.html` with the pinned license tools
   described in [CONTRIBUTING.md](CONTRIBUTING.md), then run
   `node scripts/sync-package-assets.mjs sync`.
4. Commit the changes and validate that commit once through the normal checks.

Use `node scripts/release-version.mjs check vX.Y.Z` to check a prepared version
locally if needed. Changing source or version creates a new commit; results
from a different commit do not establish that the new commit passes.
`install.sh`'s `SCRIPT_VERSION` is an independent installer logging revision.

## What runs automatically

The tag workflow checks the version, then builds Linux workers, Linux platform
binaries, and both macOS architectures concurrently. Only archive assembly waits
for the binaries it needs. It attaches all archives and SHA-256 sidecars to a
draft GitHub Release, then publishes the complete release. It does not rerun CI.

Linux x86-64 and ARM64 archives contain `mj`, `mj-desktop`, `mj-voice-worker`,
and both static musl session workers. The universal macOS archive also contains
a native `mj-worker`. All archives include licenses and notices. Linux CLI builds
retain their glibc 2.28 ELF checks; CI covers older-distribution runtime tests.

After the GitHub Release succeeds, crates.io and npm publication run independently.
Rust source packages are assembled and checked for presence, then published in
dependency order with `--no-verify`, reusing the selected commit's validation
instead of recompiling the workspace and every extracted package. npm verifies
release checksums and smoke-tests the packaged installation. Its platform
packages publish concurrently; the wrapper waits until all platforms are readable.

A `vX.Y.Z-PRERELEASE` tag creates a GitHub prerelease and uses npm's `next`
dist-tag; stable releases use `latest`.

## Recovery and optional channels

Rerun failed workflow jobs for the same tag. Registry jobs skip versions that
already exist, allowing partial publication to resume. Never move a published
tag or attempt to overwrite a published package version. To inspect packages
without publishing, manually run `publish.yml` or `publish-npm.yml` with the
release tag and `publish` disabled.

The release workflow dispatches crates.io publication explicitly because its
workflow token does not trigger a second workflow through release events, and
crates.io trusted publishing does not accept `workflow_run`. npm also listens
for successful release-workflow completion. Existing environment approvals,
when configured, are handled by GitHub.

Discord announcements use `DISCORD_RELEASE_WEBHOOK_URL` and generated release
notes. Delivery failures are warnings and do not invalidate a published release.

Homebrew is a separate manual update in `BrokkAi/homebrew-tap`. Update its formula
when shipping to that channel. Its `mj` wrapper must export
`MJOLNIR_MANAGED_BY_HOMEBREW`, and must not export `MJOLNIR_NO_UPDATE_CHECK`, so
updates go through Homebrew.
