---
title: Install Mjolnir
description: Install the Mjolnir 2 controller and its bundled workers on Linux, macOS, or WSL2.
---

Mjolnir's controller runs on Linux and macOS. On Windows, use a Linux distribution under WSL2; there is no native Windows controller package.

The release installer is the simplest route. The npm package installs the same native release bundle. Cargo and source builds are useful when you want only the headless controller or are developing Mjolnir itself, but they do not automatically install the portable Linux workers required by managed and remote targets.

## Release installer

```sh
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash
```

The installer selects the release for the current Linux or macOS architecture, verifies the downloaded artifacts when checksum sidecars are available, and installs them in `~/.local/bin` by default. A release bundle contains:

- `mj`, the terminal controller and CLI
- `mj-desktop`, used by `mj app`
- `mj-voice-worker`, used for local dictation
- static x86_64 and ARM64 Linux session workers for remote and container targets

If `~/.local/bin` is not already on `PATH`, the installer offers to update the detected shell profile. Open a new shell before running `mj`, or add the directory to `PATH` yourself.

Pin a release or choose another destination with environment variables:

```sh
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | MJOLNIR_VERSION=v2.0.0 bash
```

```sh
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | MJOLNIR_INSTALL_DIR="$HOME/bin" bash
```

`MJOLNIR_INSTALL_DIR` takes precedence over the compatible `INSTALL_DIR` variable. The script also accepts `MJOLNIR_GITHUB_OWNER` for a different release owner, `GITHUB_TOKEN` for authenticated GitHub API requests, and `PROFILE` to select the shell profile it may update.

Re-run the installer to update or repair an installation. Its checksum cache avoids downloading an unchanged archive again.

## npm or npx

The npm package requires Node.js 18 or newer and supports macOS and glibc-based Linux on x86_64 and ARM64. A WSL2 Linux environment is supported when it meets those requirements.

Install it globally:

```sh
npm install -g @brokkai/mjolnir
```

Or run it without a global installation:

```sh
npx -y @brokkai/mjolnir
```

The package includes the native bundle for the selected platform; it does not download a binary on first launch. Native Windows is not an npm target.

## Cargo

Mjolnir requires Rust 1.96 or newer. Install the headless controller from crates.io with:

```sh
cargo install --locked brokk-mjolnir
```

This installs only `mj`. Install `brokk-mj-worker` beside it to use `local-bare`; on macOS that worker is native. Container and remote targets need a target-compatible static Linux worker: install an architecture-named worker beside `mj`, point `MJ_WORKER_DIR` or `MJ_WORKER_BINARY` at one, or configure the verified `MJ_WORKER_URL` and `MJ_WORKER_SHA256` fallback. The release installer and npm package supply both supported portable worker architectures automatically, so use one of those complete bundles unless you intend to build and manage workers yourself.

```sh
cargo install --locked brokk-mjolnir brokk-mj-worker
```

Add the local dictation sidecar when voice input is wanted:

```sh
cargo install --locked brokk-mjolnir brokk-mj-voice-worker
```

To use the desktop viewer through `mj app`, install the desktop crate as well:

```sh
cargo install --locked brokk-mjolnir brokk-mj-desktop
```

Cargo places the executables in its configured binary directory, normally `~/.cargo/bin`. Ensure that directory is on `PATH`.

## Build from source

Clone the repository, enter it, and build the native headless controller for development:

```sh
git clone https://github.com/BrokkAi/mjolnir.git
cd mjolnir
cargo build --release -p brokk-mjolnir
./target/release/mj --version
```

The development wrapper builds the correct pair for the host:

```sh
scripts/run.sh --release -- --version
```

On Linux this builds the controller natively and cross-compiles only
`brokk-mj-worker` for the matching musl target. On macOS both binaries are
native, enabling `local-bare` without a Linux cross toolchain. Managed Linux
targets from macOS still need a packaged worker or an explicit override.

To build a portable worker manually, use:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl \
  -p brokk-mj-worker --bin mj-worker
```

On ARM64 Linux, substitute `aarch64-unknown-linux-musl`. Place a worker for
another target architecture beside the controller as `mj-worker-<target-triple>`,
for example `mj-worker-aarch64-unknown-linux-musl`. The [configuration
reference](/configuration/#process-and-path-overrides) covers the worker path
and verified-download overrides.

To work only on controller commands without launching sessions, use:

```sh
cargo run
```

To build the desktop controller on x86_64 GNU/Linux:

```sh
cargo build --release -p brokk-mjolnir -p brokk-mj-desktop \
  --target x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/mj app
```

Use the corresponding host target on ARM64 Linux or macOS. Linux desktop builds require the WebKitGTK 4.1 development package, and running `mj app` requires the distribution's WebKitGTK 4.1 runtime. These GUI libraries are not required for the headless `mj` controller. Building the optional voice worker on Linux also requires ALSA development headers.

## Verify the installation

```sh
mj --version
mj doctor
```

`mj doctor` checks the controller, configuration, harness credentials, targets, and required workers. Resolve reported errors before launching a session; the [troubleshooting guide](/troubleshooting/) explains the common failures.

Next, follow the [quickstart](/quickstart/) for first-run setup. For unattended or advanced setup, see [configuration](/configuration/), [profiles](/profiles/), [targets](/targets/), and the complete [CLI reference](/cli-reference/).
