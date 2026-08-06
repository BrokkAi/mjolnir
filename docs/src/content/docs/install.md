---
title: Install and run
description: Install Mjolnir, connect Codex, and launch the recommended first session.
---

The recommended setup needs an authenticated Codex CLI and a launchable Codex
ACP bridge. Codex use may incur cost. The first launch can also download an ACP
bridge, registry metadata, model rankings, or voice assets. Review [Data and trust boundaries](/data-boundaries/) before using a
private repository.

## Choose an installation

| Method | Platforms | Installs |
| --- | --- | --- |
| npm / npx | macOS universal; Linux x86-64 or ARM64 glibc; Windows x86-64; Android ARM64 | `mj` and, on desktop, `mj-voice-worker`; no first-run Mjolnir download |
| Homebrew | macOS on Apple Silicon or Intel; Linux on x86-64 or ARM64 glibc | `mj` on `PATH`, with `mj-voice-worker` in the formula's `libexec`; not Bifrost |
| Release installer | macOS/Linux on x86-64 or ARM64; Android ARM64 | `mj`, Bifrost, and on desktop `mj-voice-worker` |
| crates.io | Platforms supported by the Rust crates | `mj` and whichever crates you name; it does not install Bifrost |
| Release archive | Linux, macOS, Windows, Android release targets | The binaries and legal files packaged for that target |
| Build from source | Rust-supported development hosts | The workspace members you build |

### npm and npx

Install Mjolnir permanently with npm:

```bash
npm install -g @brokkai/mjolnir
mj --version
```

Or run it once without installing globally:

```bash
npx -y @brokkai/mjolnir --version
```

The package contains the native Mjolnir release bundle for your platform. It
does not download a product binary on first run. On macOS, Linux, and Windows,
the bundle keeps `mj-voice-worker` beside `mj`; Android omits voice support. Linux npm packages require glibc.

Upgrade or remove a global install with:

```bash
npm update -g @brokkai/mjolnir
npm uninstall -g @brokkai/mjolnir
```

The npm route requires Node.js 18 or later. Use Homebrew, the release installer,
Cargo, or a release archive if you do not want Node.js. npm does not install
Bifrost; the release installer does.

### Homebrew

```bash
brew install brokkai/tap/mjolnir
mj --version
```

The formula in [BrokkAi/homebrew-tap](https://github.com/BrokkAi/homebrew-tap)
installs the release archive for your platform and verifies its published
SHA-256 checksum. `mj` lands on `PATH`; `mj-voice-worker` stays in the
formula's `libexec` next to `mj`. Bifrost is packaged separately:

```bash
brew install brokkai/tap/bifrost
```

Upgrade and uninstall through Homebrew:

```bash
brew upgrade mjolnir
brew uninstall mjolnir
```

The tap regenerates its formulae from tagged releases on a schedule, so `brew
upgrade` follows new Mjolnir releases without manual checksum changes.

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

Claude and custom ACP servers are optional. Configure them after
the Codex path works if you want alternative primary, subagent, or review
routes. Mjolnir can install supported binary agents from the ACP
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
requires the matching checksum asset.

To uninstall a release-installer deployment, remove `mj`, `bifrost`, and
`mj-voice-worker` from the selected install directory. Review [Storage and
network activity](/storage-network/) before removing configuration, sessions,
managed agents, worktrees, or caches.
