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
happens. mise cannot say whether a `[hooks] postinstall` command has run, so
a recipe with one counts as satisfied only on an image built from exactly that
recipe. Otherwise the agent starts at once and the worker runs, in the
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
without building. For Nitro, mj builds on a configured container target for
the same platform and reports which builder it chose. Build every Nitro
image for the host's architecture. Sessions run as root inside the guest.

Pushing and pulling use the engine's own registry login on that host. If the
registry refuses, the error shows the login command to run there, for example
`podman login ghcr.io` or `ssh host podman login ghcr.io`. Nitro pulls with
crane as root on the host. For a private registry, run
`ssh host sudo crane auth login ghcr.io -u <user> --password-stdin`, replacing the
host, registry, and credentials.

`mj ec2 create` warms the base image and the bundle images assigned for the new
host's platform. The refresh poller reclaims obsolete images on Nitro after
all pulls for that host succeed. Images in use by retained sandboxes or staged
root replacements stay available.

`mj image status` reports, for each recorded platform, whether the image still
matches the current recipe (`current` or `recipe changed`), or `unknown` when
no target identifies the base. Pass `--target` to resolve it. When a Nitro target
has no available builder for the platform, status still lists its assignments
with freshness `unknown (no builder for <platform>)`.

## Shared build cache on Nitro

Each Nitro sandbox keeps its mbx build cache inside its VM. Use an S3 remote
to reuse Rust and C/C++ build outputs published by CI:

```sh
mj ec2 create c8id.4xlarge --zone us-east-1a --runtime nitro --cpus 4 --memory-gib 8 \
  --mbx-remote s3://your-build-cache/mbx
```

Mjolnir creates or reuses a read-only EC2 instance profile and records the
remote in the machine's configuration. Use `--mbx-remote-region` if the
bucket is in a different region. If AWS denies profile setup or attachment,
Mjolnir reports the denial and creates the host with the remote disabled.

For an existing host, attach an instance profile and set the same keys in
`~/.config/mjolnir/config.toml`:

```sh
aws ec2 associate-iam-instance-profile --instance-id i-0123456789abcdef0 \
  --iam-instance-profile Name=your-nitro-cache-reader
```

```toml
[machines.nitro-builder.build_cache]
remote = "s3://your-build-cache/mbx"
remote_region = "us-east-1" # Optional. Defaults to the Nitro host's region.
```

A role you attach yourself needs `s3:GetObject` for objects beneath the cache
prefix and unconditional `s3:ListBucket` on the cache bucket. S3 can report
a missing object as AccessDenied when the role cannot list the bucket.
The Nitro proxy never exposes bucket listing to guests. Check the host's
access with `mj doctor`. [S3 GetObject permissions](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)

The namespace defaults to the GitHub `owner/repository` as written in the
bundle's `github` setting, including its case. Set `mbx_namespace` on a bundle
repository to override it or enable a local repository with no GitHub source.
Match this value in CI. A sandbox can read the namespaces of all repositories
in its bundle. Its primary repository supplies the default namespace, or the
first repository with a namespace supplies it when the primary has none.

Only trusted CI should have write access. A writer can overwrite build
results that another session will use. Scope the CI role's S3 permissions
to its own cache namespace. The host reads S3 with its instance role and
serves the guest through a read-only proxy. mbx warns about unauthenticated,
plain HTTP on the guest-to-host link because plain HTTP does not protect
traffic from reading or modification in transit. Use the private Nitro
link for this endpoint.

The following workflow assumes your CI setup has installed the pinned
`mbx` and project toolchain. Replace `main` with your protected default
branch. Configure the GitHub OIDC subject template with
`include_claim_keys: ["repo", "ref", "event_name"]`. The AWS trust policy
must compare the audience with `sts.amazonaws.com` and the subject with the
exact value for the trusted repository, branch and push event. For a
name-based subject, use `repo:your-org/your-repo:ref:refs/heads/main:event_name:push`.
Include the owner and repository IDs if your repository uses immutable
subjects. This scopes the AWS credentials even if someone changes the
workflow trigger. [GitHub OIDC subject customization](https://docs.github.com/en/actions/reference/security/oidc#customizing-the-subject-claims-for-an-organization-or-repository)

```yaml
on:
  push:
    branches: [main]

jobs:
  seed-build-cache:
    if: github.event_name == 'push' && github.ref == format('refs/heads/{0}', github.event.repository.default_branch) && github.ref_protected
    runs-on: ubuntu-latest
    permissions:
      contents: read
      id-token: write
    env:
      MBX_REMOTE_URL: s3://your-build-cache/mbx
      MBX_REMOTE_NAMESPACE: ${{ github.repository }}
      MBX_REMOTE_S3_REGION: us-east-1
    steps:
      - uses: actions/checkout@v6
        with:
          persist-credentials: false
      # Install the pinned mbx and build toolchain here.
      - uses: aws-actions/configure-aws-credentials@v6.3.0
        with:
          role-to-assume: arn:aws:iam::123456789012:role/your-mbx-ci-writer
          aws-region: us-east-1
      - run: mbx build --locked
```

## Limits

mise covers language toolchains, command-line tools and system packages.
Anything it cannot express belongs in a custom image: build it however you
like and set the target's `image`. mj still supplies its worker and harness
bridges there, and still runs the project-tools step on top. See
[Custom images](/custom-images/).
