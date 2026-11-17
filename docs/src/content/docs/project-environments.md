---
title: Project environments
description: Declare a repository's tools with mise, and build images from that recipe with mj image build.
---

A repository says which tools its sessions need with [mise](https://mise.jdx.dev/),
a tool-version manager. Container sessions install those tools after the
clone, in the background while the agent starts. For faster starts,
`mj image build` builds an image from the same recipe and records it for the
project.

## Declaring tools

mise reads `mise.toml` at the repository root:

```toml
[tools]
node = "24"
python = "3.13"
"cargo:cargo-nextest" = "0.9.143"

[bootstrap.packages]
# System packages, installed with the image's package manager.
"apt:libssl-dev" = "latest"
"apt:pkg-config" = "latest"
```

It also reads the version files many repositories already have:
`rust-toolchain.toml` (or `rust-toolchain`), `.nvmrc`, `.node-version`,
`.python-version`, `.ruby-version`, `.go-version` and `.tool-versions`. A
repository with only those files needs no `mise.toml`. A `mise.lock` next to
the recipe pins download URLs and checksums.

## What happens when a session starts

On a container target (Podman, Docker, Apple container, their SSH forms, and
Nitro), after cloning each repository the worker asks mise what every
checkout with a recipe is missing. This is the launch's short "Install project
tools" step. The worker uses the image's own `mise` if it has one, and
otherwise installs Mjolnir's pinned, checksum-verified mise. Installed tools
are on `PATH` for the agent and its commands through mise's shims directory.

If nothing is missing, as on an image built from the recipe, nothing else
happens. Otherwise the agent starts at once and the worker runs, in the
background, in each checkout that needs it:

    mise bootstrap packages apply --yes
    mise install

The agent gets two messages through its mailbox, the same one that carries
sub-agent reports. The first, waiting for it when it starts, lists what is
being installed and warns that commands needing those tools may wait or fail
until the second. The second says the install finished, or names the failing
command and the log file, `project-tools.log` in the session's worker
directory, so the agent can repair the recipe and rerun `mise install`. A
failure also shows a notice in the session; the session itself is
unaffected. Closing the session stops the install.

System packages need root, which Podman sessions have; on Docker and Apple
container the image user needs root or passwordless `sudo`. Sessions on bare
targets never run this step: system packages would otherwise be installed on
your own machine.

The install runs in every new container, so a cold toolchain download is
repeated per session. Rust and Node on the default image take roughly twenty
seconds; large recipes take minutes. Build an image to avoid that.

## Building a project image

Name a registry once, in `~/.config/mjolnir/config.toml`:

```toml
[images]
repository = "ghcr.io/your-org/mj-images"
```

Then, from inside the repository:

    mj image build

mj builds on the host of a container target (by default the session's or your
remembered target) with that host's container engine. The base is the
target's configured image, pinned by digest, plus mise and the recipe. The
tag is a hash of the whole build input, so the same recipe on the same base
and platform always gives the same tag; if that tag is already in the
registry, mj pulls it instead of building. The image is pushed and recorded
for the project and platform:

```toml
[images.bundles.my-project]
"linux/amd64" = "ghcr.io/your-org/mj-images/my-project:0123456789abcdef"
```

New sessions of that project on a target of that platform run the recorded
image instead of the target's image. In them the "Install project tools" step
finds everything installed, and the agent gets no messages about it. Delete the entry to
go back to the target's image.

Options: `--target` chooses the target whose image is the base and whose host
builds, `--bundle` the project to record it for, `--name` the image name
(default: the project id), and `--print` prints the generated Containerfile
without building. A Nitro target has no engine of its own, so mj builds on a
configured container target of the same platform and says which one.

Pushing and pulling use the engine's own registry login on that host. If the
registry refuses, the error shows the login command to run there, for example
`podman login ghcr.io` or `ssh host podman login ghcr.io`.

`mj image status` reports, for each recorded platform, whether the image still
matches the current recipe (`current` or `recipe changed`), or `unknown` when
no target identifies the base; pass `--target` to resolve it.

## Limits

mise covers language toolchains, command-line tools and system packages.
Anything it cannot express belongs in a custom image: build it however you
like and set the target's `image`. mj still supplies its worker and harness
bridges there, and still runs the project-tools step on top. See
[Custom images](/custom-images/).
