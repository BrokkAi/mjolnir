# Releasing Mjolnir

## Prepare a version

Install cargo-release once: `cargo install --locked cargo-release --version 1.1.5`.

From a clean checkout, run:

```sh
cargo release minor --workspace --execute
git push
```

Use `patch`, `major`, or an exact version instead of `minor` when appropriate.
The plugin updates the workspace version, internal dependency requirements,
and Cargo.lock, then creates one commit. Repository configuration disables
tagging, publishing, and automatic pushing during this preparation step.
Without `--execute`, it only previews the change.

There are no generated notices or copied package assets to update or commit.
Cargo includes the shared project LICENSE and README directly in source
packages. Runtime guides live in the controller crate that embeds them.

## Release the green commit

Once CI is green on the exact prepared commit, tag it with its version and push
the tag:

```sh
git tag vX.Y.Z <green-commit>
git push origin vX.Y.Z
```

Use the configured upstream remote if it is not origin. The commit need not
be current master or merged into master. Reuse its passing checks; do not
rerun CI locally just to release it. Never move a published tag.

The tag is the publication trigger. GitHub Actions checks the version, builds
the platform binaries in parallel, generates third-party notices from the
tagged dependency graph, and packages those notices in every release archive.
Archive assembly waits for the binaries and notices it needs; notice generation
does not serialize the binary builds. The complete GitHub Release is then
published, followed by crates.io and npm publication. Existing environment
approvals still apply. Prerelease tags create GitHub prereleases and use npm's
`next` channel; stable versions use `latest`.

A release request authorizes the normal tag push and publication. Registry
authorization is configured once, not audited again for every release.

## Recovery and optional channels

Rerun failed workflow jobs for the same tag. Registry jobs skip versions already
published, so partial publication can resume without overwriting packages.
Manual `publish.yml` and `publish-npm.yml` runs support inspection with
`publish` disabled.

Homebrew remains a separate manual formula update in `BrokkAi/homebrew-tap`.
Its wrapper exports `MJOLNIR_MANAGED_BY_HOMEBREW`, not
`MJOLNIR_NO_UPDATE_CHECK`. Discord announcement failures are warnings and do
not invalidate a release. The installer's SCRIPT_VERSION is independent of
the application version.
