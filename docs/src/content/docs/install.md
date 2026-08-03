---
title: Install and run
description: Install Mjolnir, connect Codex, and launch the recommended first session.
---

The recommended setup needs an authenticated Codex CLI and a launchable Codex
ACP bridge. Codex use may incur cost. The first launch can also download an ACP
bridge, the managed Anvil runtime, registry metadata, model rankings, or voice
assets. Review [Data and trust boundaries](/data-boundaries/) before using a
private repository.

## Choose an installation

| Method | Platforms | Installs |
| --- | --- | --- |
| npm / npx | macOS universal; Linux x86-64/ARM64 (glibc); Windows x86-64; Android ARM64 | Native `mj` and bundled Anvil; desktop also `mj-voice-worker`; not Bifrost |
| Release installer | macOS/Linux on x86-64 or ARM64; Android ARM64 | `mj`, Bifrost, and on desktop `mj-voice-worker` |
| crates.io | Platforms supported by the Rust crates | `mj` and whichever crates you name; it does not install Bifrost |
| Release archive | Linux, macOS, Windows, Android release targets | The binaries and legal files packaged for that target |
| Build from source | Rust-supported development hosts | The workspace members you build |

### npm and npx

The npm package ships the native release bundle for your platform — no Rust
toolchain and no first-run product download:

```bash
npm install -g @brokkai/mjolnir
mj --version
```

Run one-shot without a global install, optionally pinned to an exact
release:

```bash
npx -y @brokkai/mjolnir
npx -y @brokkai/mjolnir@1.4.0
```

Upgrade and uninstall through npm:

```bash
npm update -g @brokkai/mjolnir
npm uninstall -g @brokkai/mjolnir
```

Desktop installs place `anvil` and the `mj-voice-worker` voice sidecar next
to `mj`, so voice support works out of the box; Android includes `mj` and
`anvil` only. The npm path does not install Bifrost — use the release
installer if you want it. Linux builds are glibc-only; musl systems should
use another method. Because npm owns the installation, Mjolnir's in-place
self-updater is disabled under this method.

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

## Connect Codex

Install and authenticate the official Codex CLI if it is not already
available:

```bash
npm install -g @openai/codex
codex login
```

Run `mj`. First launch opens Mjolnir's configuration screen:

1. In **Accounts**, verify that OpenAI / ChatGPT reports you are signed in.
2. In **ACP Servers**, confirm that Codex is detected and enabled.
3. In **Agents**, keep the primary model on Auto or select a Codex model.
4. Keep discrete review enabled if you want delegated workspace changes
   challenged before completion.
5. Start a new session after changing models or adapters.

Return to the same settings later with `/mjconfig`.

Existing Codex credentials can be detected without launching the ACP bridge
during discovery. Launch still requires Node.js/npm, `npx`, and the
PATH-visible `codex` CLI. Mjolnir uses that CLI for its Codex sign-in action as
well.

Claude, Kimi, Anvil, and custom ACP servers are optional. Configure them after
the Codex path works if you want alternative primary, subagent, or review
routes. Mjolnir can install Kimi and supported binary agents from the ACP
registry.

Adapters must advertise ACP Streamable HTTP MCP support; Mjolnir uses that
capability to expose its authenticated `mj-subagents` tools to the primary
agent.

## Verify the installation

```bash
mj --version
```

Then run the [10-minute Codex evaluation](/evaluate/). A successful `mj --version`
only proves the binary starts; it does not prove that a provider route can
launch or that delegation works end to end.

## Update and uninstall

Interactive startup checks GitHub for a newer Mjolnir release unless
`MJOLNIR_NO_UPDATE_CHECK=1` or `--no-update-check` is set. The in-app updater
requires the matching checksum asset. npm installations set
`MJOLNIR_NO_UPDATE_CHECK` automatically and are upgraded with
`npm update -g @brokkai/mjolnir` instead.

To uninstall a release-installer deployment, remove `mj`, `bifrost`, and
`mj-voice-worker` from the selected install directory. Review [Storage and
network activity](/storage-network/) before removing configuration, sessions,
managed agents, worktrees, or caches.
