---
title: Install and run
description: Choose the release bundle, Cargo crates, or source build and connect a provider.
---

Mjolnir needs at least one supported provider account and a launchable ACP
adapter. Provider use may incur cost. The first launch can also download an ACP
bridge, the managed Anvil runtime, registry metadata, model rankings, or voice
assets. Review [Data and trust boundaries](/data-boundaries/) before using a
private repository.

## Choose an installation

| Method | Platforms | Installs |
| --- | --- | --- |
| Release installer | macOS/Linux on x86-64 or ARM64; Android ARM64 | `mj`, Bifrost, and on desktop `mj-voice-worker` |
| crates.io | Platforms supported by the Rust crates | `mj` and whichever crates you name; it does not install Bifrost |
| Release archive | Linux, macOS, Windows, Android release targets | The binaries and legal files packaged for that target |
| Build from source | Rust-supported development hosts | The workspace members you build |

### Release installer

```bash
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash
```

The script installs into `~/.local/bin` by default and can offer to update a
shell profile when that directory is not on `PATH`. It selects the latest
Mjolnir and Bifrost releases separately.

Useful environment variables:

```bash
MJOLNIR_INSTALL_DIR=/opt/bin \
MJOLNIR_VERSION=v1.0.2 \
BIFROST_VERSION=v0.8.5 \
bash install.sh
```

`INSTALL_DIR` is an alias for `MJOLNIR_INSTALL_DIR`; `GITHUB_TOKEN` can avoid
anonymous GitHub API rate limits. A release asset is verified when its
`.sha256` sidecar is available. The installer warns and continues when the
sidecar is absent.

Windows is not supported by the shell installer. Use the Windows release
archive or Cargo.

### crates.io

Install the terminal client and voice worker together on desktop:

```bash
cargo install --locked brokk-mjolnir brokk-mj-voice-worker
```

Installing only `brokk-mjolnir` is supported but disables Ctrl-R dictation.
Android users should omit the voice worker. The Cargo route does not install
Bifrost.

### Build from source

```bash
git clone https://github.com/BrokkAi/mjolnir.git
cd mjolnir
cargo build --release
./target/release/mj --cwd .
```

You only need Rust to build from source or contribute. See
[CONTRIBUTING.md](https://github.com/BrokkAi/mjolnir/blob/master/CONTRIBUTING.md)
for voice prerequisites and the full validation matrix.

## Connect a provider

Run `mj`, then open `/mjconfig`:

1. In **Accounts**, sign in or verify an existing provider credential.
2. In **ACP Servers**, confirm at least one adapter is available.
3. In **Council**, keep the three roles on Auto or select explicit models.
4. Start a new session after changing models or adapters.

Existing Codex or Claude credentials can be detected without launching their
ACP bridges during discovery. Launch still requires Node.js/npm, `npx`, and the
corresponding PATH-visible vendor CLI. The official `codex` or `claude` CLI is
also used when you choose Mjolnir's sign-in action for that vendor.
Mjolnir can install Kimi and supported binary agents from the ACP registry.

Adapters used by the Council must advertise ACP Streamable HTTP MCP support;
Mjolnir uses that capability to expose its authenticated Eitri tools to Thor.

## Verify the installation

```bash
mj --version
```

Then run the [10-minute evaluation](/evaluate/). A successful `mj --version`
only proves the binary starts; it does not prove that a provider route can
launch or that the Council can delegate.

## Update and uninstall

Interactive startup checks GitHub for a newer Mjolnir release unless
`MJOLNIR_NO_UPDATE_CHECK=1` or `--no-update-check` is set. The in-app updater
requires the matching checksum asset.

To uninstall a release-installer deployment, remove `mj`, `bifrost`, and
`mj-voice-worker` from the selected install directory. Review [Storage and
network activity](/storage-network/) before removing configuration, sessions,
managed agents, worktrees, or caches.
