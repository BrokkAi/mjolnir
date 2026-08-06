---
title: Install and run
description: Install Mjolnir, connect Codex, and launch the recommended first session.
---

The recommended setup needs an authenticated Codex CLI and a launchable Codex
ACP bridge. Codex use may incur cost. The first launch can also download a
managed runtime, ACP bridge, Bifrost package, model rankings, or voice assets.
Review [Data and trust boundaries](/data-boundaries/) before
using a private repository.

## Choose an installation

| Method | Platforms | Installs |
| --- | --- | --- |
| npm / npx | macOS universal; Linux x86-64 or ARM64 glibc; Windows x86-64; Android ARM64 | `mj` and, on desktop, `mj-voice-worker`; no first-run Mjolnir download |
| Homebrew | macOS on Apple Silicon or Intel; Linux on x86-64 or ARM64 glibc | `mj` on `PATH`, with `mj-voice-worker` in the formula's `libexec` |
| Release installer | macOS/Linux on x86-64 or ARM64; Android ARM64 | `mj` and, on desktop, `mj-voice-worker` |
| crates.io | Platforms supported by the Rust crates | `mj` and whichever crates you name |
| Release archive | Linux, macOS, Windows, Android release targets | The binaries and legal files packaged for that target |
| Build from source | Rust-supported development hosts | The workspace members you build |

Discrete review runs Bifrost through `npx -y @brokkai/bifrost`. On Linux,
macOS, and Windows, Mjolnir uses `npx` from `PATH` when available and otherwise
installs an embedded Node.js 24 runtime automatically. Android users already
need Node.js/npm for the built-in ACP bridges; the npm installation route
provides that prerequisite by construction.

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
Cargo, or a release archive if you do not want Node.js as a system dependency;
Mjolnir can manage its own Node.js runtime for `npx` commands.

### Homebrew

```bash
brew install brokkai/tap/mjolnir
mj --version
```

The formula in [BrokkAi/homebrew-tap](https://github.com/BrokkAi/homebrew-tap)
installs the release archive for your platform and verifies its published
SHA-256 checksum. `mj` lands on `PATH`; `mj-voice-worker` stays in the
formula's `libexec` next to `mj`.

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
shell profile when that directory is not on `PATH`.

Useful environment variables:

```bash
MJOLNIR_INSTALL_DIR=/opt/bin \
MJOLNIR_VERSION=v1.0.2 \
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
Android users should omit the voice worker.

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

Run `mj`. First launch asks you to choose one of four team presets: Codex,
Claude, Codex code with Claude review, or Claude code with Codex review. The
coding provider backs both the primary and builder seats; the review provider
backs the reviewer seat. Use **Customize every route** for model, review,
parallelism, and appearance controls.

Return to the same settings later with `/mjconfig`.

Existing Codex credentials can be detected without launching the ACP bridge
during discovery. Launch requires the PATH-visible `codex` CLI. For `npx` ACP
bridges and Bifrost, Mjolnir uses a PATH-visible `npx` or, on Linux, macOS, and
Windows, installs embedded Node.js 24 automatically. Mjolnir uses the `codex`
CLI for its Codex sign-in action as well.

Claude and custom ACP servers are optional. The ACP Servers panel configures
only the built-in Codex and Claude routes. Custom ACP commands can still be
declared directly in `config.toml` for advanced deployments.

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

To uninstall a release-installer deployment, remove `mj` and
`mj-voice-worker` from the selected install directory. Review [Storage and
network activity](/storage-network/) before removing configuration, sessions,
worktrees, or caches.
