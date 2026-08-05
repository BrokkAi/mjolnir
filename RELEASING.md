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

A `vX.Y.Z` tag triggers the GitHub release and docs workflows.

The release workflow opens with a coverage gate and builds nothing until it
passes. CI's branch and pull request triggers do not match tags, so this is the
only check that re-runs against the tagged tree. Collecting coverage runs the
whole workspace test suite, which means a failing test and a coverage
regression both stop the release; tagging a commit whose coverage run was red
on master fails here rather than shipping.

The gate covers Linux tests and the coverage baseline only. Formatting, Clippy,
the macOS and Windows test runs, the Android target check, and the
dependency-license checks stay pull request checks, so a tag still relies on the
tagged commit having passed CI on master.

The builds cover Linux x86-64 and ARM64, Android ARM64, Windows x86-64, and a
universal macOS archive. Desktop archives contain `mj` and the voice worker;
Android omits the voice worker. Every archive includes the applicable licenses
and notices and is published with a SHA-256 sidecar.

Neither registry publish runs off the tag push. Both wait for the release
workflow to succeed, so the coverage gate and a build failure on any target
each stop the release before anything reaches crates.io or npm.

## Discord announcement

To announce a published GitHub Release in Discord, set the
`DISCORD_RELEASE_WEBHOOK_URL` repository Actions secret to the target channel's
webhook URL. The release workflow reuses GitHub's generated release notes,
prevents mentions from being parsed, suppresses automatic link embeds, and
leaves a failed Discord delivery as a warning so it cannot invalidate an
already-published release.

## crates.io publishing

`publish.yml` publishes `brokk-mj-voice-worker` and `brokk-mjolnir`. It refuses
to publish when the tag differs from either crate version, and packages both
crates ahead of the `crates-io` environment gate so a packaging failure surfaces
without spending an approval.

Publishing runs automatically once the release workflow succeeds. Both the
release event and the release workflow's completion trigger it, and each crate
is skipped when that version is already on the registry, so the overlap cannot
fail a run with nothing left to publish. That skip is also the recovery path: if
one crate publishes and the other fails, re-running resumes at the crate that
did not land. crates.io reserves a version number permanently once published and
yanking does not release it, so a shipped version can never be republished.

To package a tag without publishing, run the workflow manually with `publish`
off and inspect its `.crate` artifact.

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
