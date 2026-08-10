---
title: Install and run
description: Install Mjolnir, connect Codex or Claude, and launch your first coding team.
---

The recommended setup needs authenticated Codex or Claude credentials and its
launchable ACP bridge. Provider use may incur cost. The first launch can also
download a managed runtime, ACP bridge, Bifrost package, model rankings, or
voice assets.
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
MJOLNIR_VERSION=v1.7.0 \
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

The default terminal build needs no WebView development package. To compile
the optional `desktop-app` feature used for native desktop-shell development
on macOS, install Apple's Command Line Tools. The shell uses the WebKit
framework included in the macOS SDK:

```bash
xcode-select --install
```

On Linux, install the WebKitGTK 4.1 development package first:

```bash
# Ubuntu or Debian
sudo apt-get update
sudo apt-get install libwebkit2gtk-4.1-dev

# Fedora
sudo dnf install webkit2gtk4.1-devel
```

Fedora's `webkit2gtk4.1-devel` package provides the GTK 3 and libsoup 3 API
expected by Wry; `webkitgtk6.0-devel` is the incompatible GTK 4 API. Build the
feature with:

```bash
cargo build --release --features desktop-app
```

The default terminal client only needs Rust. On Linux, the optional desktop
shell needs WebKitGTK and the voice worker needs ALSA development files. On
macOS, both use frameworks from the SDK installed with the Command Line Tools.
See
[CONTRIBUTING.md](https://github.com/BrokkAi/mjolnir/blob/master/CONTRIBUTING.md)
for voice prerequisites and the full validation matrix.

## Connect Codex or Claude

You need credentials for at least one provider; a mixed team needs both.

On first launch, open the **ACP Servers** tab in onboarding and select the
OpenAI or Anthropic account row to sign in. Mjolnir launches the compatible
provider CLI bundled through that account's ACP package; no global Codex or
Claude installation is required.

For Codex, the equivalent manual command is:

```bash
npx --yes --package=@agentclientprotocol/codex-acp codex login
```

For a Claude subscription, the equivalent manual command is:

```bash
npx -y @agentclientprotocol/claude-agent-acp --cli auth login --claudeai
```

The Anthropic account row also offers Anthropic Console sign-in for API usage
billing.

These commands do not install a second global provider CLI. The ACP packages
bring compatible platform-specific Codex and Claude executables as transitive
dependencies, and Mjolnir uses those same package entry points for login and
quota queries.

Set `CODEX_PATH` only when you intentionally want both `codex-acp` and
Mjolnir's Codex quota poller to use a specific compatible Codex executable.
Without that override, both use the version supplied transitively by
`@agentclientprotocol/codex-acp`.

Run `mj`. First launch opens onboarding on the Team tab and asks you to choose
one of four teams: **Codex**, **Claude**, **Codex coder + Claude reviewer**,
or **Claude coder + Codex reviewer**. The coder backs the primary session; the
reviewer backs the independent review pass and the default subagent pool. The
other tabs hold model, review, parallelism, and appearance controls.

Press **Ctrl+Tab** during a session to switch between the four teams, or return
to the same choice on the **Team** tab in `/mjconfig`. Start a new session after
switching so the new coder owns the complete turn.

Existing credentials are detected without launching the ACP bridge during
discovery. For the ACP bridges, provider CLIs, and Bifrost, Mjolnir uses a
PATH-visible `npx` or, on Linux, macOS, and Windows, installs embedded Node.js
24 automatically. npm's cache location is an implementation detail; Mjolnir
addresses the provider executables through their ACP package entry points.

The ACP Servers panel configures the built-in Codex and Claude routes, which
are the only supported ACP servers.

Adapters must advertise ACP Streamable HTTP MCP support; Mjolnir uses that
capability to expose its authenticated `mj-subagents` tools to the primary
agent.

## Verify the installation

```bash
mj --version
```

Then run the [10-minute evaluation](/evaluate/). A successful `mj --version`
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
