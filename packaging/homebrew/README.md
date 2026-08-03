# Homebrew packaging for Mjolnir

This directory generates the Homebrew formula published to the
[BrokkAi/homebrew-tap](https://github.com/BrokkAi/homebrew-tap) repository,
installable with `brew install BrokkAi/tap/mjolnir`. Nothing here compiles
Rust; the formula points at the existing GitHub release archives
(`brokk-mjolnir-*`) and pins their `.sha256` checksums.

The formula serves macOS (Apple Silicon and Intel, both from the single
`universal-apple-darwin` archive) and Homebrew-on-Linux x86-64/ARM64 (glibc,
from the `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
archives). It installs all three sibling binaries — `mj`, `anvil`, and
`mj-voice-worker` — so voice dictation works; it does not install Bifrost.

## Layout

- `lib.mjs` — target list and pure helpers: sidecar parsing, tag and
  Cargo.toml version checks, and the formula renderer.
- `generate.mjs` — CLI: release `.sha256` sidecars in, `mjolnir.rb` out.
- `tap-template/` — seed files for the tap repository (README and a CI
  workflow that installs and tests the formula on macOS and Linux).
- `test/` — `node --test` suite, including an end-to-end run of
  `generate.mjs` against fabricated sidecars.

## Publishing model

The tap is a separate org-owned repository, `BrokkAi/homebrew-tap`, holding
`Formula/mjolnir.rb`. The `Publish Homebrew` workflow in this repository
regenerates that file from a release and pushes it to the tap with the commit
message `mjolnir <version>`. The formula is fully version-pinned: each
release produces a new formula whose URLs and sha256s reference exactly that
release's archives, so `brew upgrade mjolnir` moves users forward only when
the tap commit lands. Mjolnir's in-place self-updater must not be used under
brew; the formula's caveats say so.

Pushes use the `HOMEBREW_TAP_TOKEN` repository secret, a fine-grained
personal access token with push access to `BrokkAi/homebrew-tap` only. The
workflow is retry-safe: when the tap already contains the generated formula,
the publish step exits cleanly without a commit.

## Local build

```bash
mkdir /tmp/mj-sidecars
gh release download v1.4.0 --repo BrokkAi/mjolnir --dir /tmp/mj-sidecars --pattern '*.sha256'
node packaging/homebrew/generate.mjs --tag v1.4.0 --assets /tmp/mj-sidecars --out /tmp/mjolnir.rb
ruby -c /tmp/mjolnir.rb
node --test packaging/homebrew/test/*.test.mjs
```

`generate.mjs` refuses to run when the tag does not match `Cargo.toml`, when
a sidecar for any of the three Homebrew targets is missing or malformed, or
when a sidecar's recorded filename does not match the expected release
archive name.

## One-time tap bootstrap

1. Create the `BrokkAi/homebrew-tap` repository (public) and seed it from
   `tap-template/`: copy `README.md` and `.github/workflows/test.yml` into
   the new repository. The tap CI needs a `Formula/mjolnir.rb` to test, which
   the first publish provides.
2. Create a fine-grained personal access token scoped to
   `BrokkAi/homebrew-tap` with read and write access to repository contents,
   and store it as the `HOMEBREW_TAP_TOKEN` secret in the BrokkAi/mjolnir
   repository.
3. Run the `Publish Homebrew` workflow with the latest release tag and
   `publish` unchecked; inspect the uploaded formula artifact.
4. Re-run with `publish: true` to push the first `Formula/mjolnir.rb`, then
   confirm the tap CI is green and `brew install BrokkAi/tap/mjolnir`
   followed by `brew test mjolnir` works on a real machine.

## Routine releases

After a GitHub release exists, run the `Publish Homebrew` workflow with the
release tag. Leave `publish` unchecked for a dry run (tests + formula
generation + `ruby -c` + artifact); check it to push the formula to the tap.
Re-running with the same tag is safe.
