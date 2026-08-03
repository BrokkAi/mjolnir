# npm packaging for brokk-mjolnir

This directory builds the public npm package `brokk-mjolnir` (executable
`mj`) from an existing GitHub release. Nothing here compiles Rust; the npm
payloads are the release bundles, repackaged.

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

Platform payloads are published as platform-suffixed versions of
`brokk-mjolnir` itself (for example `1.4.0-universal-apple-darwin`), each
under a non-`latest` dist-tag (`platform-darwin-universal`, …) and with npm
`os`/`cpu` (and `libc` on Linux) constraints. The root wrapper
`brokk-mjolnir@<version>` declares one aliased optional dependency per
variant (`"brokk-mjolnir-darwin-universal": "npm:brokk-mjolnir@<version>-universal-apple-darwin"`),
so npm installs exactly one native payload per machine. The root package is
published `latest` only after every variant exists, and never becomes
installable before its payloads.

Because the suffixed variant versions are semver prereleases, they can never
satisfy a `^<version>` range or be served as `latest` by accident.

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

The unscoped name is created by the first authenticated publisher; do not
publish a placeholder to reserve it. In order:

1. Run the packaging tests and a full local build; inspect
   `npm pack` output and the tarball contents; install the tarballs in a
   clean environment and verify `mj --version`, plus executable `anvil` and
   `mj-voice-worker` siblings on desktop; scan the tarballs for credentials
   and unintended files (the build's forbidden-file scan also enforces this).
2. With an npm account that has 2FA enabled, manually publish one inspected
   platform variant under its platform dist-tag, for example:
   `npm publish brokk-mjolnir-1.4.0-x86_64-unknown-linux-gnu.tgz --tag platform-linux-x64-gnu`
3. Grant the BrokkAI team access:
   `npm access grant read-write brokkai:developers brokk-mjolnir`
4. On npmjs.com, configure the package's Trusted Publisher for the
   `BrokkAi/mjolnir` repository and the `publish-npm.yml` workflow.
5. Run the `Publish npm` GitHub workflow with `publish: true`. It skips the
   already-published bootstrap variant, publishes the remaining variants
   under their platform dist-tags, and publishes the root package as
   `latest` last. The workflow is retry-safe after partial publishes.

## Routine releases

After a GitHub release exists, run the `Publish npm` workflow with the
release tag. Leave `publish` unchecked for a dry run (build + smoke test +
tarball artifacts); check it to publish through npm trusted publishing
(OIDC) — no long-lived npm token is stored in the repository.
