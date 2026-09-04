# Releasing Mjolnir

Releases are maintainer-driven. This is the tagging runbook; see
[CONTRIBUTING.md](CONTRIBUTING.md) for development setup, runtime invariants,
tests, and dependency-license maintenance.

## Versions

The release version is set once, in `[workspace.package]` in the root
`Cargo.toml`; every workspace crate inherits it via `version.workspace = true`,
so they cannot drift apart. After changing that one value, run
`node scripts/release-version.mjs sync` to project it into the published
internal dependency requirements under `[workspace.dependencies]`, then run
`cargo update --workspace` to refresh the workspace entries in `Cargo.lock`.
CI runs the script's `check` mode so generated dependency versions cannot
drift. Member manifests inherit the dependencies and contain no release
versions. `install.sh`'s `SCRIPT_VERSION` is an independent installer logging
revision and is not automatically synchronized to product releases.

`licenses/THIRD_PARTY_LICENSES.html` embeds the workspace crate versions, so a
version bump must regenerate it. CI diffs the checked-in report against a fresh
`cargo about generate` and fails on any difference.

## What a tag triggers

A `vX.Y.Z` or `vX.Y.Z-PRERELEASE` tag triggers the GitHub release workflow.
Prerelease tags create a GitHub prerelease, and npm publishes them under the
`next` dist-tag rather than moving `latest`.

The release workflow verifies that the tag matches every workspace version
before building artifacts. It creates the GitHub Release as a draft, attaches
all assets, and only then publishes it so immutable releases cannot expose a
partially uploaded asset set. CI's branch and pull request triggers do not match
tags, so a release relies on the tagged commit having passed CI on master. Run
the required formatting, Clippy, build, test, license, and release-version
validations against the clean release commit before creating the tag.

The builds cover Linux x86-64 and ARM64 and a universal macOS archive; the
controller supports Linux and macOS (Windows stays a CI compile gate only —
use WSL2). Every archive contains the headless `mj` controller, the separate
`mj-desktop` native application, the voice worker, and the two static musl
session workers (`mj-worker-x86_64-unknown-linux-musl` and
`mj-worker-aarch64-unknown-linux-musl`) that the controller uploads into
disposable targets. The macOS archive additionally contains a universal native
`mj-worker` for `local-bare`. Every archive includes the applicable licenses
and notices and is published with a SHA-256 sidecar.

Neither registry publish runs off the tag push. Both wait for the release
workflow to succeed, so a version mismatch or build failure on any target stops
the release before anything reaches crates.io or npm.

## Discord announcement

To announce a published GitHub Release in Discord, set the
`DISCORD_RELEASE_WEBHOOK_URL` repository Actions secret to the target channel's
webhook URL. The release workflow reuses GitHub's generated release notes,
prevents mentions from being parsed, suppresses automatic link embeds, and
leaves a failed Discord delivery as a warning so it cannot invalidate an
already-published release.

## crates.io publishing

`publish.yml` publishes `brokk-mj-voice-worker`, `brokk-mj-core`,
`brokk-mj-worker`, `brokk-mj-controller`, `brokk-mj-chat`, `brokk-mj-tui`,
`brokk-mj-desktop`, and `brokk-mjolnir` in that order, which is dependency
order: each library crate must reach the registry before anything that depends
on it.
It refuses to publish when the tag differs from any workspace crate version. It
assembles the whole workspace in one `cargo package --workspace --no-verify`
run, because extracted packages cannot resolve same-release path dependencies
from the registry before publication. It then checks every target in the whole
workspace on GNU/Linux and asserts that every publishable package produced its
`.crate` artifact ahead of the `crates-io` environment gate, so a failure
surfaces without spending an approval. Each `cargo publish` performs extracted
package verification again after the loop has published the dependencies it
needs.

Publishing runs automatically once the release workflow succeeds. The automated
release job explicitly dispatches `publish.yml` after creating the GitHub
Release. This uses a trigger supported by crates.io trusted publishing; GitHub
does not emit a second workflow from release events created with its workflow
token, and crates.io rejects the `workflow_run` trigger. A release published by
another actor also starts `publish.yml` through its release event.

Each crate is skipped when that version is already on the registry. That is the
recovery path if some crates publish and a later one fails: re-running resumes
at the crate that did not land. crates.io reserves a version number permanently
once published and yanking does not release it, so a shipped version can never
be republished. Every publish is retried, because a crate cannot be packaged
until the sibling it depends on has propagated through the sparse index.

To package a tag without publishing, run the workflow manually with `publish`
off and inspect its `.crate` artifact.

## npm publishing

`publish-npm.yml` packages an existing GitHub Release into `@brokkai/mjolnir`
and its three platform packages. It verifies the release checksums, then
publishes every platform package before the root wrapper.

Publishing runs automatically once a GitHub Release is published. Both the
release event and the release workflow's completion trigger it, and each
publish step is skipped when that version already exists on the registry, so
the overlap cannot republish over a shipped version.

To package and smoke-test a tag without publishing, run the workflow manually
with `publish` off and inspect its tarball artifact and Linux smoke test.

## Before tagging

Confirm that:

1. Every workspace crate manifest and its `Cargo.lock` workspace entry matches
   the intended tag.
2. Formatting, Clippy, release builds, tests, and relevant cross-platform or
   packaging checks pass.
3. Dependency-license policy and generated notice reports are current.
4. User-facing installation, configuration, and release documentation reflects
   the shipped behavior.
5. The release commit is merged and the tagged commit is the exact commit meant
   to be published.
