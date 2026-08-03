# @brokkai/mjolnir npm packaging

This directory builds the public `@brokkai/mjolnir` npm package and its native
platform payloads. The root package provides the `mj` launcher. Each platform
payload contains the existing Mjolnir release bundle so `anvil` and
`mj-voice-worker` remain siblings of the native `mj` executable. Desktop
package construction fails when either sibling is missing. Android packages do
not contain the unsupported voice worker.

Build packages from extracted release bundle directories:

```bash
node npm/scripts/build-packages.mjs \
  --version 1.4.0 \
  --output-dir dist/npm \
  --repository-root /path/to/mjolnir-v1.4.0 \
  --bundle linux-x64=/path/to/brokk-mjolnir-v1.4.0-x86_64-unknown-linux-gnu \
  --bundle linux-arm64=/path/to/brokk-mjolnir-v1.4.0-aarch64-unknown-linux-gnu \
  --bundle android-arm64=/path/to/brokk-mjolnir-v1.4.0-aarch64-linux-android \
  --bundle win32-x64=/path/to/brokk-mjolnir-v1.4.0-x86_64-pc-windows-msvc \
  --bundle darwin-universal=/path/to/brokk-mjolnir-v1.4.0-universal-apple-darwin
```

Run the packaging tests with:

```bash
npm --prefix npm test
```

Platform-suffixed versions must be published under non-`latest` dist-tags
before publishing the root version as `latest`. Do not publish from a developer
checkout until the generated tarballs have been inspected and installed in a
clean test environment.

The `Build and publish npm packages` GitHub Actions workflow packages an
existing GitHub release. It defaults to build and smoke-test only. Set its
`publish` input only after the initial `@brokkai/mjolnir` package exists and its
npm trusted publisher is configured for `BrokkAi/mjolnir` and
`publish-npm.yml`.

The package belongs to the `@brokkai` organization scope. For the initial
registration, publish one inspected platform tarball manually under its
non-`latest` platform tag and configure the trusted publisher. The workflow
safely skips an already-published platform version, so it can publish the
remaining variants and the root `latest` package afterward.
