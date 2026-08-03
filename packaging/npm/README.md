# npm packaging for @brokkai/mjolnir

This directory builds the public npm package `@brokkai/mjolnir` (executable
`mj`) from an existing GitHub release. Nothing here compiles Rust; the npm
payloads are the release bundles, repackaged. The GitHub release assets keep
their `brokk-mjolnir-*` names (that prefix is the crate name and belongs to
release.yml); only the npm packages are scoped.

## Layout

- `lib.mjs` — platform matrix and pure packaging helpers.
- `build.mjs` — CLI: release assets in, npm tarballs + `manifest.json` out.
- `launcher/bin/mj.js` — the JavaScript wrapper published as the root
  package's `bin`. It locates the platform payload, prepends the bundle
  directory to `PATH` (so `mj` finds `anvil` and `mj-voice-worker`), sets
  `MJOLNIR_NO_UPDATE_CHECK=1` because npm owns the installation, forwards
  arguments and signals, and preserves the exit code.
- `test/` — `node --test` suite, including an end-to-end build against
  fabricated release archives.

## Publishing model

Platform payloads are real scoped packages under the `brokkai` npm
organization — `@brokkai/mjolnir-darwin-universal`,
`@brokkai/mjolnir-linux-x64-gnu`, `@brokkai/mjolnir-linux-arm64-gnu`,
`@brokkai/mjolnir-android-arm64`, `@brokkai/mjolnir-win32-x64` — each
published at the same version as the release and carrying npm `os`/`cpu`
(and `libc` on Linux) constraints. The root wrapper `@brokkai/mjolnir`
declares each platform package as an optional dependency pinned to the exact
release version, so npm installs exactly one native payload per machine and
`npx -y @brokkai/mjolnir@<version>` always resolves the matching native
build.

Publish order is platform packages first, root last, so a root version is
never installable before its payloads exist. Because the org owns the
`@brokkai` scope, no unscoped-name reservation or dist-tag tricks are
needed; each package's own `latest` tag is fine.

## Local build

```bash
mkdir /tmp/mj-assets
gh release download v1.4.0 --repo BrokkAi/mjolnir --dir /tmp/mj-assets --pattern 'brokk-mjolnir-*'
node packaging/npm/build.mjs --tag v1.4.0 --assets /tmp/mj-assets --out /tmp/mj-npm
node --test packaging/npm/test/*.test.mjs
```

`build.mjs` refuses to run when the tag does not match `Cargo.toml`, when a
`.sha256` sidecar fails verification, when a desktop bundle is missing
`anvil` or `mj-voice-worker` (Android intentionally omits the voice worker),
or when a credential-shaped file appears in a payload.

## One-time registry bootstrap

The `brokkai` npm organization already exists. Each of the six packages
(five platform packages plus the root) must be created by a first manual
publish before npm trusted publishing can be configured for it. In order:

1. Run the packaging tests and a full local build; inspect
   `npm pack` output and the tarball contents; install the tarballs in a
   clean environment and verify `mj --version`, plus executable `anvil` and
   `mj-voice-worker` siblings on desktop; scan the tarballs for credentials
   and unintended files (the build's forbidden-file scan also enforces this).
2. With an npm account that has 2FA enabled and publish rights in the
   `brokkai` org, publish every platform package and then the root from the
   inspected tarballs:
   `npm publish <tarball> --access public`
   (platform packages first, `@brokkai/mjolnir` last).
3. On npmjs.com, for each of the six packages, configure the Trusted
   Publisher for the `BrokkAi/mjolnir` repository and the `publish-npm.yml`
   workflow, and set publishing access to require 2FA or automation.
4. Future releases go through the `Publish npm` GitHub workflow with
   `publish: true`. It skips already-published versions, publishes missing
   platform packages first, and publishes the root package last. The
   workflow is retry-safe after partial publishes.

Note on the earlier aborted publish: npm permanently refuses to republish
any name@version that was published and then unpublished, and an unpublished
package name is locked for 24 hours. If a version was burned by an
unpublish, bootstrap with the next release version instead of fighting the
registry.

## Routine releases

After a GitHub release exists, run the `Publish npm` workflow with the
release tag. Leave `publish` unchecked for a dry run (build + smoke test +
tarball artifacts); check it to publish through npm trusted publishing
(OIDC) — no long-lived npm token is stored in the repository.
