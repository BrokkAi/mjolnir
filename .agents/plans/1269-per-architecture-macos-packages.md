# Publish per-architecture macOS release archives and npm packages

This ExecPlan is a living document. The sections `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to
date as work proceeds.

This plan is maintained in accordance with `.agents/PLANS.md` from the
repository root.

## Purpose / Big Picture

The macOS npm release channel is broken. Mjolnir's release workflow builds one
"universal" macOS binary that contains both Intel and Apple-silicon code
inside a single executable, then wraps that fat binary in one npm package
named `@brokkai/mjolnir-darwin-universal`. That npm tarball has grown past the
package registry's request-size limit, and `npm publish` fails with HTTP 413.
The Linux npm packages publish, so `@brokkai/mjolnir@2.35.0` and `2.36.0` are
unusable on any Mac.

After this change, Mjolnir ships one macOS release archive and one npm platform
package per CPU architecture: `x86_64-apple-darwin` (Intel) and
`aarch64-apple-darwin` (Apple silicon). The npm packages become
`@brokkai/mjolnir-darwin-x64` and `@brokkai/mjolnir-darwin-arm64`, each about
the size of the Linux packages that already publish successfully. A user on
any Mac can run `npm install -g @brokkai/mjolnir` and get a working `mj`.

You can see it working by running the packaging test suites (below) and by
checking that `npm run package-release` produces four `.tgz` platform
packages, two of them for macOS, none of which contains a fat binary.

## Progress

- [x] Claim issue #1269 and label it `agent-in-progress`.
- [x] Record the scoping notes already on the issue in this plan's Context.
- [x] Rewrite the macOS build/packaging jobs in `.github/workflows/release.yml`.
- [x] Update the two Linux packaging jobs to carry both per-architecture Darwin
      workers.
- [x] Update `.github/workflows/publish-npm.yml` for ten assets and four
      platform packages.
- [x] Declare four platforms with explicit `sessionWorkers` in
      `npm/scripts/package-release.mjs`.
- [x] Point `npm/launcher/mj.js` at the per-architecture Darwin packages.
- [x] Update `npm/test/launcher.test.mjs` and `npm/test/package-release.test.mjs`.
- [x] Update `scripts/release-workflow.test.mjs` fixtures for the new targets,
      job names, asset count, and upload concurrency barrier.
- [x] Drop the macOS universal pattern from `install.sh`.
- [x] Remove the universal branch from `select_mj_asset` in
      `mj-controller/src/controller/update.rs` and add a macOS selection test.
- [x] Describe per-architecture macOS workers in `docs/SSH.md`.
- [x] Run `npm test`, `node scripts/release-workflow.test.mjs`,
      `cargo test -p brokk-mj-controller`, and
      `cargo clippy --all-targets -- -D warnings`.
- [ ] Commit the change. The `agent-in-progress` label stays on issue #1269
      until the commit is pushed, since the issue is not yet resolved upstream.

## Surprises & Discoveries

- Observation: The universal tarball is rejected only by npm. GitHub release
  assets and `shasum`-verified downloads have no such limit, so the release
  archives can keep shipping both Darwin workers while the npm packages stay
  per-architecture.
  Evidence: issue #1269 records `npm error code E413` on
  `@brokkai/mjolnir-darwin-universal` while the Linux packages published at
  ~118 MB.
- Observation: The universal archive ships `mj-worker` and
  `mj-worker-universal-apple-darwin` as byte-identical 89,181,104-byte copies.
  Replacing the single universal worker with two per-architecture workers is
  byte-size neutral, which is why the Linux packages can carry both Darwin
  workers for free.
  Evidence: issue #1269 member table.
- Observation: This dev container has no Perl `shasum`, only GNU `sha256sum`,
  and `scripts/release-workflow.test.mjs` shells out to `shasum` and spawns
  background shells that the restricted sandbox blocks.
  Evidence: issue #1269 environment notes.
- Observation: A `shasum` shim earlier on `PATH` is enough for
  `scripts/release-workflow.test.mjs`, but not for
  `installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths`. That
  test replaces `PATH` with `<tempdir>:/usr/bin:/bin`, so the shim must sit in
  `/usr/bin` to be seen. Only that one test fails on this host, with exit 127
  and `exec: shasum: not found`, exactly as the issue predicts.
  Evidence: `cargo test -p brokk-mj-controller` reported 1953 passed, 1 failed,
  10 ignored; the failure text is `mj-worker-digest: line 1: exec: shasum: not
  found`.
- Observation: The updated `publish-npm.yml` cannot backfill the
  already-published `v2.35.0` and `v2.36.0` tags by a plain re-run. It now
  requires ten per-architecture assets, but those releases carry the old
  eight-asset universal set, and a `workflow_dispatch` run still uses the
  workflow definition from the dispatched ref while the `package` job checks
  out the tag. Backfilling needs the GitHub Release workflow re-run on those
  tags (or hand-uploaded per-architecture assets) first.
  Evidence: `.github/workflows/publish-npm.yml` `jq -e 'length == 10'` plus the
  v2.35.0 asset table in issue #1269.

## Decision Log

- Decision: Keep the `mj-worker-universal-apple-darwin` fallback lookup in
  `mj-controller/src/controller/worker_binary/binary_source.rs`.
  Rationale: An already-installed bundle may still contain the universal
  worker, and the extra lookup costs nothing. Removing it would break
  in-place use of old installs.
  Date/Author: 2026-10-08 / scoping notes on issue #1269.
- Decision: macOS npm packages ship only their own architecture's Darwin
  worker, while Linux npm packages and every release archive ship both.
  Rationale: A macOS package carrying both Darwin workers would grow past the
  size known to publish; a Linux controller still needs both so it can drive
  either Mac over SSH.
  Date/Author: 2026-10-08 / scoping notes on issue #1269.
- Decision: Drop the universal preference from the updater's
  `select_mj_asset`.
  Rationale: The updater only ever targets the newest release, which now has
  per-architecture assets, and older binaries already fall back from the
  universal lookup to the exact target suffix.
  Date/Author: 2026-10-08 / scoping notes on issue #1269.
- Decision: Treat Intel Macs as legacy for the npm bundle.
  Rationale: An arm64 npm install driving an Intel Mac over SSH is no longer
  supported by the npm bundle, but the release archives still carry both
  Darwin workers, so Intel Macs are still served by the installer and updater.
  Date/Author: 2026-10-08 / scoping notes on issue #1269.
- Decision: Run the two macOS packaging jobs on `ubuntu-latest`.
  Rationale: They only run `tar`, `chmod`, and `shasum`, exactly like the
  Linux packaging jobs. The `lipo` tool and the macOS SDK are only needed in
  `build-macos`, which stays on `macos-latest`.
  Date/Author: 2026-10-08 / issue #1269 touch points.

## Outcomes & Retrospective

The universal macOS fat binary is gone from the release pipeline. Mjolnir now
builds and publishes `x86_64-apple-darwin` and `aarch64-apple-darwin`
separately: two release archives, two npm platform packages
(`@brokkai/mjolnir-darwin-x64`, `@brokkai/mjolnir-darwin-arm64`), and the same
per-architecture names in the installer and the updater. Each macOS npm
package carries only its own Darwin worker, which is what keeps it near the
Linux package size that is known to publish.

Validation evidence:

- `npm test` (from `npm/`): 2 suites, 0 failures.
- `node scripts/release-workflow.test.mjs` with a `shasum` shim on `PATH`:
  9 tests, 0 failures, including one archive-assembly test per platform and the
  four-way concurrent publish barrier.
- `cargo test -p brokk-mj-controller`: 1953 passed, 10 ignored, and one
  environmental failure (`shasum` missing from `/usr/bin`) in the unrelated
  worker-digest test. The new
  `macos_update_selects_the_running_architecture_even_with_a_universal_asset`
  passes.
- `cargo clippy --all-targets -- -D warnings`: clean.

Remaining work, deliberately not part of this commit: pushing the commit, and
deciding whether to backfill macOS npm for `v2.35.0`/`v2.36.0`. A plain re-run
of `publish-npm.yml` cannot backfill them; see `Surprises & Discoveries`.

Lesson: three separate surfaces encode the platform list (release workflow
asset names, the npm `PLATFORMS` table, and the Rust updater), so the fixtures
that read the workflow text are the only thing keeping them in agreement. The
fixtures now cover both Apple targets, which means a future rename breaks the
build in one place instead of shipping a broken channel.

## Context and Orientation

Mjolnir is a Rust workspace plus a thin npm distribution layer. Three
independent surfaces decide which release binary a user gets, and all three
encode the current universal-macOS design:

1. The release workflow `.github/workflows/release.yml` builds the binaries and
   assembles the downloadable archives. Its `build-macos` job cross-compiles
   both Apple targets and then runs `lipo -create` to merge each pair of
   binaries into one "fat" file under `target/universal-apple-darwin/release/`.
   Its `package-macos` job turns that directory into
   `brokk-mjolnir-<tag>-universal-apple-darwin.tar.gz`. The two Linux packaging
   jobs also copy `workers/mj-worker-universal-apple-darwin/mj-worker` into
   their archives so a Linux controller can drive a Mac.

2. The npm packaging workflow `.github/workflows/publish-npm.yml` downloads the
   published release assets, verifies their checksums, builds the platform npm
   tarballs through `npm/scripts/package-release.mjs`, smoke-tests Linux, and
   then publishes every platform package before the `@brokkai/mjolnir`
   wrapper. The wrapper's `optionalDependencies` are generated from the
   `PLATFORMS` list, so a new platform is published automatically once it is
   declared in `npm/scripts/package-release.mjs`.

3. The running client picks the right archive. `npm/launcher/mj.js` maps
   `{platform, arch}` to an npm package name. `mj-controller`'s updater
   (`mj-controller/src/controller/update.rs`, function `select_mj_asset`)
   maps the running platform to a release asset name. `install.sh` maps the
   host to a release asset name with a grep pattern list. The worker resolver
   `mj-controller/src/controller/worker_binary/binary_source.rs` looks for a
   packaged worker beside `mj`; it already falls back from
   `mj-worker-<arch>-apple-darwin` to `mj-worker-universal-apple-darwin`.

Terms used in this plan: a "fat" or "universal" macOS binary is one file whose
header contains both an Intel and an Apple-silicon slice, produced by `lipo`.
An "npm platform package" is an architecture-specific npm package listed in the
wrapper's `optionalDependencies`, so npm installs only the one matching the
user's machine. The "wrapper" is `@brokkai/mjolnir` itself, a tiny package
whose `bin/mj.js` launcher resolves the native package at run time.

The publish limit is a registry request-size limit. It is somewhere above
117,873,887 bytes (the published
`@brokkai/mjolnir-linux-x64-gnu@2.35.0`) and at or below the ~198 MB universal
tarball. A per-architecture macOS tarball lands near the Linux package size,
which is known to publish.

## Plan of Work

The change is mechanical but spans three encodings of the same fact, so every
edit must move together or the fixtures fail.

In `.github/workflows/release.yml`, rewrite `build-macos` to keep the existing
two-target `cargo build --release --locked` and delete the `Create universal
binary` step. Stage each target's `mj`, `mj-worker`, `mj-desktop`, and
`mj-voice-worker` into `native/<target>/`, upload `native-x86_64-apple-darwin`
and `native-aarch64-apple-darwin` from those directories, and upload
`mj-worker-x86_64-apple-darwin` and `mj-worker-aarch64-apple-darwin` from
`target/<target>/release/mj-worker`. Keep the job named `build-macos` so the
`needs` lists stay readable.

Replace `package-macos` with `package-x86_64-apple-darwin` and
`package-aarch64-apple-darwin`, each on `ubuntu-latest`, each needing
`[build-macos, build-worker-x64, build-worker-arm64, release-notices]`. Each
downloads its own `native-<target>` artifact into `native/` and all
`mj-worker-*` artifacts into `workers/`, then assembles
`brokk-mjolnir-${GITHUB_REF_NAME}-<target>.tar.gz` containing the native four
binaries, both static Linux workers, and both Darwin workers. Keep the
`run: |` body shape the release-workflow fixture parses.

Update `package-x86_64-unknown-linux-gnu` and
`package-aarch64-unknown-linux-gnu` to copy
`workers/mj-worker-x86_64-apple-darwin/mj-worker` and
`workers/mj-worker-aarch64-apple-darwin/mj-worker` in place of the single
universal copy. Update the `release` job's `needs` list to name both new
package jobs.

In `.github/workflows/publish-npm.yml`, change the asset-count assertion to
`length == 10` (four tarballs plus the Windows zip, and a checksum for each),
add `x86_64-apple-darwin aarch64-apple-darwin` to the download target loop, and
list the four platform npm packages and tarballs in the publish job.

In `npm/scripts/package-release.mjs`, declare four entries in `PLATFORMS`
(`darwin-x64`, `darwin-arm64`, `linux-x64-gnu`, `linux-arm64-gnu`), each with
an explicit `sessionWorkers` array, and make `stagePlatform` iterate
`platform.sessionWorkers` instead of its hard-coded three-name list. Because
`rootManifest` builds `optionalDependencies` from `PLATFORMS`, no separate
wrapper edit is needed.

In `npm/launcher/mj.js`, map `darwin/x64` to `@brokkai/mjolnir-darwin-x64` and
`darwin/arm64` to `@brokkai/mjolnir-darwin-arm64`. Update
`npm/test/launcher.test.mjs` to match, and update
`npm/test/package-release.test.mjs` to assert the new `PLATFORMS` list and to
stage each platform's own `sessionWorkers` set.

In `scripts/release-workflow.test.mjs`, replace `universal-apple-darwin` in
the two target lists with the two Apple targets, detect macOS with
`target.endsWith('-apple-darwin')`, use `native/` as the staged directory for
every non-Windows target, give each macOS fixture a native `mj-worker`, include
both Darwin workers in every non-Windows fixture, and change the publish
fixture's concurrency barrier from three to four and its ready-file list to
the four platform package names.

In `install.sh`, delete the macOS universal pattern so the generic
`^brokk-mjolnir-.*-${RUST_TARGET}[.]tar[.]gz$` pattern matches the
per-architecture asset. The macOS companion list can stay as-is because each
archive still ships a native `mj-worker`.

In `mj-controller/src/controller/update.rs`, delete the macOS universal branch
from `select_mj_asset`. Add a test in
`mj-controller/src/controller/update/tests.rs` proving that on macOS the
running architecture's archive wins even when a universal archive is present.

In `docs/SSH.md`, describe the per-architecture macOS workers in release
archives and npm packages, keeping the universal worker named as a legacy
fallback.

## Concrete Steps

All commands run from the repository root unless stated otherwise.

Write the plan and make the edits above.

Run the npm suites (they need Node's test runner; the release-workflow suite
also shells out to `shasum` and spawns background shells, so run it outside
the restricted sandbox):

    cd npm && npm test
    cd .. && node scripts/release-workflow.test.mjs

Expected: the npm tests report the four platform packages, and the
release-workflow suite reports one passing archive-assembly test per target
(five targets) plus the npm download and publish tests.

Run the Rust checks for the updater change:

    cargo test -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

Expected: the new macOS selection test passes and clippy is clean. On a host
without Perl `shasum`,
`installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths` fails
for that environmental reason only; supply a `sha256sum` shim on `PATH` and
rerun that test.

## Validation and Acceptance

Acceptance is behavior a human can check:

1. `npm test` from `npm/` passes and its "declares every release target exactly
   once" case lists four packages, exactly one per architecture and OS.
2. `node scripts/release-workflow.test.mjs` passes. Each of the five
   `archive assembly ...` tests proves a real assembled archive contains the
   expected binaries with mode `0755`, the licence notices, and a verifiable
   SHA-256 sidecar. The npm publish test proves four platform uploads run
   concurrently and all finish before the wrapper uploads.
3. `cargo test -p brokk-mj-controller` passes, including the new test that a
   macOS update selects `...-aarch64-apple-darwin.tar.gz` (or the x86_64
   variant) rather than a universal archive.
4. Reading `npm/scripts/package-release.mjs` shows each Darwin platform's
   `sessionWorkers` contains only its own architecture's Darwin worker, while
   each Linux platform lists both.

## Idempotence and Recovery

Every edit is a text change to a checked-in file; re-running the test commands
is safe and does not mutate the repository. `npm test` and the release-workflow
suite create temporary directories under the system temp directory and remove
them. `cargo test` writes only to `target/`. No step deletes user data or
rewrites published artifacts. If a fixture fails, fix the fixture and re-run
only that test.

## Artifacts and Notes

The intended platform table after the change (this is the contract every
fixture and resolver must agree on):

    name                        target                  npm package
    x86_64-apple-darwin         x86_64-apple-darwin     @brokkai/mjolnir-darwin-x64
    aarch64-apple-darwin        aarch64-apple-darwin    @brokkai/mjolnir-darwin-arm64
    x86_64-unknown-linux-gnu    x86_64-unknown-linux-gnu @brokkai/mjolnir-linux-x64-gnu
    aarch64-unknown-linux-gnu   aarch64-unknown-linux-gnu @brokkai/mjolnir-linux-arm64-gnu

    release tar.gz assets: the four above, plus the Windows zip
    total release assets: ten (five archives and five .sha256 sidecars)

## Interfaces and Dependencies

No new libraries or crates are introduced.

`npm/scripts/package-release.mjs` must export `PLATFORMS` entries shaped as:

    {
      packageName: string,      // e.g. "@brokkai/mjolnir-darwin-x64"
      target: string,           // e.g. "x86_64-apple-darwin"
      extension: string,        // ".tar.gz"
      binary: string,           // "mj"
      desktop: boolean,         // true
      nativeWorker: boolean,    // true for the two Darwin platforms
      sessionWorkers: string[], // worker artifact base names, e.g. "mj-worker-aarch64-apple-darwin"
      description: string,
      os: string[],
      cpu: string[],
      libc?: string[],
    }

`stagePlatform(platform, version, source, stagingRoot)` must keep its
signature; only its session-worker loop changes to read
`platform.sessionWorkers`.

`mj-controller/src/controller/update.rs` keeps
`fn select_mj_asset(assets: &[ReleaseAsset], platform: &Platform) -> Result<ReleaseAsset>`
with its current signature; only the macOS branch is removed.

`mj-controller/src/controller/worker_binary/binary_source.rs` is intentionally
unchanged: `select_sibling_worker` and `resolve` keep the
`universal-apple-darwin` fallback.

## Change Log

- 2026-10-08: Initial plan written from issue #1269's scoping notes before any
  code changes, so a fresh context can implement the split end to end.
- 2026-10-08: Implementation complete and validated; recorded the missing-
  `shasum` failure mode, the backfill limitation, and the outcomes above. The
  progress list now reflects the landed state.
