# Releasing Mjolnir

Releases are maintainer-driven. This is the tagging runbook; see
[CONTRIBUTING.md](CONTRIBUTING.md) for development setup, runtime invariants,
tests, and dependency-license maintenance.

## Versions

The root and voice-worker `Cargo.toml` files must carry the same version, with
matching workspace entries in `Cargo.lock`. `install.sh`'s `SCRIPT_VERSION` is
an independent installer logging revision and is not automatically synchronized
to product releases.

`licenses/THIRD_PARTY_LICENSES.html` embeds the workspace crate versions, so a
version bump must regenerate it. CI diffs the checked-in report against a fresh
`cargo about generate` and fails on any difference.

## What a tag triggers

A `vX.Y.Z` tag triggers the GitHub release and crates.io workflows. The publish
workflow refuses to publish when the tag differs from either crate version. The
release workflow builds Linux x86-64 and ARM64, Android ARM64, Windows x86-64,
and a universal macOS archive. Desktop archives contain `mj` and the voice
worker; Android omits the voice worker. Every archive includes the applicable
licenses and notices and is published with a SHA-256 sidecar.

## Discord announcement

To announce a published GitHub Release in Discord, set the
`DISCORD_RELEASE_WEBHOOK_URL` repository Actions secret to the target channel's
webhook URL. The release workflow reuses GitHub's generated release notes,
prevents mentions from being parsed, suppresses automatic link embeds, and
leaves a failed Discord delivery as a warning so it cannot invalidate an
already-published release.

## npm publishing

`publish-npm.yml` packages an existing GitHub Release into `@brokkai/mjolnir`
and its five platform packages. It verifies the release checksums, then
publishes every platform package before the root wrapper.

Publishing runs automatically once a GitHub Release is published. Both the
release event and the release workflow's completion trigger it, and each
publish step is skipped when that version already exists on the registry, so
the overlap cannot republish over a shipped version.

To package and smoke-test a tag without publishing, run the workflow manually
with `publish` off and inspect its tarball artifact and Linux smoke test.

## Before tagging

Confirm that:

1. Both crate manifests and their `Cargo.lock` workspace entries match the intended tag.
2. Formatting, Clippy, release builds, tests, and relevant cross-platform or
   packaging checks pass.
3. Dependency-license policy and generated notice reports are current.
4. User-facing installation, configuration, and release documentation reflects
   the shipped behavior.
5. The release commit is merged and the tagged commit is the exact commit meant
   to be published.
