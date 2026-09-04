---
title: Configuration reference
description: Complete reference for Mjolnir 2 config.toml, including profiles, bundles, targets, review, viewer, paths, and environment overrides.
---

Mjolnir keeps per-user configuration in `config.toml`. Workspaces, sessions,
prompt history, per-session resource choices, drafts, and read markers live in
Mjolnir's state database instead; they are not fields in this file.

Run `mj setup` to generate a starting configuration, then edit the file for
additional profiles, bundles, or targets. `mj setup` replaces an existing file
rather than merging it, so do not rerun it over hand-written configuration
without keeping a copy. After a manual edit, restart the daemon and validate the
result:

```console
mj daemon restart
mj doctor
```

See [Profiles and harnesses](/profiles/), [Workspaces and bundles](/workspaces-bundles/),
and [Targets](/targets/) for the concepts behind these fields.

## Location and version

The default path is the operating system's configuration directory followed by
`mjolnir/config.toml`. On a typical Linux installation that is
`~/.config/mjolnir/config.toml`. Set `MJ_CONFIG_DIR` to replace the directory;
Mjolnir appends `config.toml` to it.

Every current file starts with the required schema version:

```toml
version = 2
```

The only accepted top-level keys are:

| Key | TOML type | Required | Default | Purpose |
| --- | --- | --- | --- | --- |
| `version` | integer | yes | none | Configuration schema version; use `2`. |
| `phone` | table | no | default `[phone]` values | Browser and desktop viewer settings. |
| `review` | table | no | default `[review]` values | Independent turn-review settings. |
| `profiles` | table of named tables | no | empty | Named harness accounts and homes. |
| `bundles` | table of named tables | no | empty | Named repository sets for managed targets. |
| `targets` | table of named tables | no | empty | Named places where sessions run. |

A missing or empty file is treated as an empty version 2 configuration. Unknown
fields in the current top-level, viewer, review, profile, bundle, and repository
schemas are errors. If a file declares a version newer than this build
understands, Mjolnir salvages the sections it can read but treats the file as
read-only. `mj doctor` reports that state; update Mjolnir before changing it.

Profile, bundle, repository, and target IDs all use the same rule: 1–64 ASCII
letters, digits, `.`, `-`, or `_`. The IDs `.` and `..` are not allowed. IDs are
the TOML table names, for example `work` in `[profiles.work]`.

## Web viewer `[phone]`

The historical section name remains `phone`, although it controls both the web
viewer and the native desktop shell.

```toml
[phone]
enabled = true
bind = "127.0.0.1:3765"
tailscale_detect = true
# tls_cert = "/absolute/path/fullchain.pem"
# tls_key = "/absolute/path/private-key.pem"
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `enabled` | boolean | no | `true` | Starts the viewer with the daemon. |
| `bind` | string | no | `"127.0.0.1:3765"` | Must parse as a numeric socket address, including a port. |
| `tailscale_detect` | boolean | no | `true` | Allows automatic trusted `ts.net` certificate discovery and renewal. |
| `tls_cert` | path string | no | unset | Certificate-chain path. Must be paired with `tls_key`. |
| `tls_key` | path string | no | unset | Private-key path. Must be paired with `tls_cert`. |

A non-loopback `bind` is rejected unless both explicit TLS paths are present.
When Tailscale detection succeeds, Mjolnir may advertise a secure non-loopback
listener without changing the configured loopback fallback. Explicit TLS takes
precedence. See [Web viewer and desktop app](/web-viewer/) for access and login.

## Automatic review `[review]`

```toml
[review]
enabled = true
tier = "quick"
profile = "reviewer"
# model = "provider-model-id"
# effort = "high"
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `enabled` | boolean | no | `false` | Examines each eligible completed turn after queued work drains; an unchanged delta resolves without launching reviewers. |
| `tier` | string enum | no | `"quick"` | `quick` or `extended`. |
| `profile` | string | when enabled | unset | Must name a profile in this file. May be set while disabled to enable one-off `/review`. |
| `model` | string | no | unset (harness default) | Model applied to every review role when the harness exposes model selection. The config loader does not validate provider model IDs. |
| `effort` | string | no | unset (harness default) | Effort applied to every review role when supported. The config loader does not validate harness-specific effort names. |

Use a reviewer profile different from the profile doing the primary work. The
quick tier runs one general reviewer and validates reported findings. Extended
review may add intent analysis, a supervisor, and specialist lanes. See
[Independent turn review](/turn-review/).

## Profiles `[profiles.<id>]`

Each profile names one harness installation or account on the controller:

```toml
[profiles.codex-work]
kind = "codex"
home = "/home/me/.codex-work"
# context_window_bytes = 131072

[profiles.codex-work.environment]
# PATH = "/opt/node/bin:/usr/local/bin:/usr/bin:/bin"
# PROVIDER_SETTING = "value"
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `kind` | string enum | yes | none | `codex`, `claude`, `kimi`, `grok`, or `deepseek`. |
| `home` | path string | yes | none | Non-empty controller-side harness home. An absolute path is strongly recommended. |
| `environment` | table of strings | no | empty | Environment passed to harness/profile commands. Keys cannot be blank or contain `=`. |
| `context_window_bytes` | integer | no | unset (`262144`-byte fallback) | Conservative byte budget for cross-harness transcript compaction; when set, must be at least `32768`. |

The profile's harness-home variable cannot appear in `environment`; set `home`
instead. Those variables are `CODEX_HOME`, `CLAUDE_CONFIG_DIR`,
`KIMI_CODE_HOME`, `GROK_HOME`, and `DSH_HOME` respectively.

Profiles do not select target-side executables. Mjolnir owns the bridge command:
raw SSH and EC2 workers resolve an exact pinned runtime from their managed cache,
while local and container targets use their target runtime. `mj login` invokes
the harness's canonical controller-side command from `PATH`.

Profiles do not accept `model` or `reasoning_effort`. Use `/model` and `/effort`
inside a session, or configure the harness's own defaults in its home. For
credential files, staging allowlists, skills, quotas, and harness limitations,
see [Profiles and harnesses](/profiles/).

## Bundles `[bundles.<id>]`

A bundle is a non-empty repository set. The primary repository becomes the
agent's working directory; other repositories are additional workspace roots.

```toml
[bundles.product]
primary_repo = "app"

[[bundles.product.repositories]]
id = "app"
github = "acme/app"
destination = "app"
git_ref = "main"

[[bundles.product.repositories]]
id = "shared"
local = "/home/me/src/shared"
destination = "shared"
```

Bundle fields:

| Field | TOML type | Required | Validation and behavior |
| --- | --- | --- | --- |
| `primary_repo` | string | yes | Must exactly match one repository `id` in this bundle. |
| `repositories` | array of tables | yes | Must contain at least one repository. |

Repository fields:

| Field | TOML type | Required | Validation and behavior |
| --- | --- | --- | --- |
| `id` | string | yes | Valid, unique ID within the bundle. |
| `github` | string | exactly one source | GitHub source in a supported form; cannot be combined with `local`. |
| `local` | path string | exactly one source | Absolute controller-side path; cannot be combined with `github` or `git_ref`. |
| `destination` | path string | yes | Non-empty relative path below the target workspace; no `.` or `..` components. |
| `git_ref` | string | no | Non-blank branch, tag, or commit selection for a GitHub source. |

Supported GitHub forms are `owner/repository`,
`https://github.com/owner/repository`, `git@github.com:owner/repository`, and
`ssh://git@github.com/owner/repository`, with an optional `.git` suffix. Sources
cannot contain whitespace or begin with `-`. Repository destinations may not
be equal, ancestors, or descendants of one another.

Bundles are used by container and EC2 sessions. New bare sessions select an
existing Git project directory instead. See [Workspaces and bundles](/workspaces-bundles/)
for local-repository bridging, dirty-state handling, and project memory.

## Targets `[targets.<id>]`

Every target requires a `kind`. The accepted fields after it depend on that
kind. `permissions` is valid only on `ssh-bare`; setting it on another target is
an error.

| Field | TOML type | Required | Default | Accepted values |
| --- | --- | --- | --- | --- |
| `kind` | string enum | yes | none | `local-bare`, `local-podman`, `local-docker`, `apple-container`, `ssh-bare`, `ssh-podman`, or `aws-ec2`. |

### `local-bare`

```toml
[targets.localhost]
kind = "local-bare"
```

There are no additional fields. The new-session wizard asks for an existing
absolute Git project directory. The harness retains its configured approval
behavior because there is no container or instance boundary.

### Common container fields

`local-podman`, `local-docker`, `apple-container`, and `ssh-podman` accept the
same container fields. SSH Podman also requires the SSH fields described below.

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `image` | string | yes | none | Non-blank image reference. |
| `pull_policy` | string enum | no | `"auto"` | `auto`, `always`, `newer`, `missing`, or `never`. |
| `platform` | string | no | unset (runtime selection) | Image platform such as `linux/amd64` or `linux/arm64`; it also determines the required worker architecture when recognizable. |
| `cpus` | string | no | unset (no template override) | Runtime CPU value, for example `"8"`. Per-session selection can override it. |
| `memory` | string | no | unset (no template override) | Runtime memory value, for example `"32g"`. Per-session selection can override it. |
| `environment` | table of strings | no | empty | Environment placed inside the target container. Keys cannot be blank or contain `=`. |
| `workspace_storage` | table | no | `{ kind = "podman-volume" }` | All variants work on local or SSH Podman. Docker and Apple Container reject non-default variants. |

The schema checks that `image` is non-blank but leaves CPU, memory, and platform
syntax to the selected runtime. Profile `environment` and target `environment`
are different: profile values configure the harness/bridge, while target values
become container environment variables.

Pull-policy behavior:

- `auto` starts from an existing image and pulls only when absent. The daemon
  refreshes eligible moving tags for Podman, Docker, and SSH Podman in the
  background. Versioned tags, digest references, and local images remain pinned
  or cached. Apple Container resolves `auto` during provisioning.
- `always` and `newer` request a launch-time refresh; Docker treats `newer` like
  `always` because it has no distinct newer-only mode.
- `missing` pulls only when no local copy exists.
- `never` requires a local copy.

The Podman workspace-storage table can be written inline:

```toml
workspace_storage = { kind = "podman-volume" }
# workspace_storage = { kind = "container-layer" }
# workspace_storage = { kind = "host-helper", root = "/srv/mj-workspaces", helper = ["sudo", "-n", "/usr/local/libexec/mj-workspace-helper"] }
```

The inline table's `kind` is a required string enum:

| `kind` value | Additional fields and TOML types | Validation and behavior |
| --- | --- | --- |
| `podman-volume` | none | Default; a named Podman volume backs `/workspace`. |
| `container-layer` | none | Stores `/workspace` in the disposable container layer. |
| `host-helper` | `root` (path string), `helper` (array of strings) | Both fields are required. `root` must be absolute. `helper` must contain at least one non-empty argument. The helper owns host storage lifecycle. |

See [Container targets](/containers/) and [Custom images](/custom-images/) for
runtime behavior.

### `local-podman`

```toml
[targets.podman]
kind = "local-podman"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
platform = "linux/amd64"
cpus = "8"
memory = "32g"
workspace_storage = { kind = "podman-volume" }

[targets.podman.environment]
EXAMPLE = "value"
```

All common container fields are accepted. `workspace_storage` supports all
three Podman variants. See [Podman](/podman/).

### `local-docker`

```toml
[targets.docker]
kind = "local-docker"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
platform = "linux/amd64"
cpus = "8"
memory = "32g"

[targets.docker.environment]
EXAMPLE = "value"
```

All common fields except a non-default `workspace_storage` are supported. See
[Docker](/docker/).

### `apple-container`

```toml
[targets.apple]
kind = "apple-container"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
platform = "linux/arm64"
cpus = "8"
memory = "32g"

[targets.apple.environment]
EXAMPLE = "value"
```

All common fields except a non-default `workspace_storage` are supported. See
[Apple container](/apple-container/).

### Common SSH fields

`ssh-bare` and `ssh-podman` accept:

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `host` | string | yes | none | OpenSSH host or config alias; non-blank and contains no whitespace. |
| `user` | string | no | unset (SSH/config default) | Cannot be empty, contain whitespace, or contain `@`. |
| `identity_file` | path string | no | unset (SSH/config default) | Private-key path passed to SSH; it is not required to be absolute. |
| `extra_args` | array of strings | no | empty | Additional OpenSSH arguments, passed in order. |

`host` and `user` are combined as `user@host`; put only the host or alias in
`host`.

### `ssh-bare`

```toml
[targets.builder]
kind = "ssh-bare"
host = "builder.example.com"
user = "ubuntu"
identity_file = "/home/me/.ssh/mjolnir"
extra_args = ["-o", "ServerAliveInterval=30"]
permissions = "guardian"
workspace_prefix = ".local/share/hel/workspaces"
```

In addition to the common SSH fields:

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `permissions` | string enum | yes | none | `guardian` preserves harness approvals; `yolo` disables approval and sandbox checks. |
| `workspace_prefix` | path string | no | `".local/share/hel/workspaces"` | Per-session lifecycle/cleanup path prefix. It does not select or relocate the remote Git project. May be home-relative or safely absolute. |

`workspace_prefix` cannot be empty, `/`, `.`, bare `~`/`~/`, or contain `..`.
A leading `~/` on a longer path is interpreted relative to the remote login
home. The wizard separately asks for an existing absolute remote Git directory;
when that is a primary checkout, its managed worktree is created below the
repository's own `.mj/worktrees/` tree. The legacy `hel` segment shown above is
the current default. See [SSH and SSH Podman](/ssh/).

### `ssh-podman`

```toml
[targets.remote-podman]
kind = "ssh-podman"
host = "builder.example.com"
user = "ubuntu"
identity_file = "/home/me/.ssh/mjolnir"
extra_args = ["-o", "ServerAliveInterval=30"]
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
platform = "linux/amd64"
cpus = "8"
memory = "32g"
workspace_storage = { kind = "podman-volume" }

[targets.remote-podman.environment]
EXAMPLE = "value"
```

This kind combines every common SSH field with every common container field,
including all Podman workspace-storage variants. It always runs the harness
unconstrained inside the container boundary. See [SSH and SSH Podman](/ssh/).

### `aws-ec2`

```toml
[targets.aws]
kind = "aws-ec2"
aws_profile = "default"
region = "eu-west-1"
launch_template = "lt-0123456789abcdef0"
# launch_template_version = "3"
ssh_user = "ubuntu"
address_source = "public-dns"
# identity_file = "/home/me/.ssh/mjolnir-ec2"
ssh_args = ["-o", "ServerAliveInterval=30"]
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `aws_profile` | string | no | unset (runtime uses `"default"`) | AWS CLI profile; cannot be an empty string when set. |
| `region` | string | yes | none | Non-blank AWS region. |
| `launch_template` | string | yes | none | Non-blank launch-template ID or name. |
| `launch_template_version` | string | no | unset (runtime uses `"$Default"`) | Cannot be an empty string when set. |
| `ssh_user` | string | yes | none | Non-blank login user for the launched instance. |
| `address_source` | string enum | no | `"public-dns"` | `public-dns`, `public-ip`, `private-dns`, or `private-ip`. |
| `identity_file` | path string | no | unset (SSH/config default) | Private-key path passed to SSH; it is not required to be absolute. |
| `ssh_args` | array of strings | no | empty | Additional SSH arguments, passed in order. Note the field name differs from SSH targets' `extra_args`. |

The launch template owns networking, security groups, storage, AMI, and any
default instance type. The new-session wizard may override the instance type
for one session. EC2 targets do not accept container fields or a target
`environment` table. See [AWS EC2](/aws/).

## Complete compact example

This example contains the sections most installations need. Add other target
kinds from the examples above rather than mixing fields between variants.

```toml
version = 2

[phone]
enabled = true
bind = "127.0.0.1:3765"
tailscale_detect = true

[review]
enabled = false
tier = "quick"
profile = "claude-review"

[profiles.codex-work]
kind = "codex"
home = "/home/me/.codex"

[profiles.claude-review]
kind = "claude"
home = "/home/me/.claude"

[bundles.product]
primary_repo = "product"

[[bundles.product.repositories]]
id = "product"
github = "acme/product"
destination = "product"

[targets.localhost]
kind = "local-bare"

[targets.podman]
kind = "local-podman"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
```

## Process and path overrides

These variables affect the running controller or its companion processes. Set
them in the environment that starts the daemon, then run `mj daemon restart`.

| Variable | Purpose |
| --- | --- |
| `MJ_CONFIG_DIR` | Directory containing `config.toml`. |
| `MJ_DATA_DIR` | Directory containing the SQLite store, recovery archives, logs, viewer state, diagnostics, and project memory. |
| `MJ_WORKER_BINARY` | Exact worker binary; highest-priority worker override and must name a file. |
| `MJ_WORKER_DIR` | Directory containing architecture-named portable Linux workers. |
| `MJ_WORKER_URL` | Fallback worker URL template; `{target}` expands to the target triple. Requires `MJ_WORKER_SHA256`. |
| `MJ_WORKER_SHA256` | Required 64-character hexadecimal digest for `MJ_WORKER_URL`. |
| `MJ_DESKTOP_BINARY` | Path to `mj-desktop` used by `mj app`. |
| `MJ_CONTROLLER_BINARY` | Path to `mj` when `mj-desktop` cannot find its sibling controller. |
| `MJ_VOICE_WORKER` | Path to the local dictation helper. |
| `MJ_BIFROST_BIN` | Path or command name for the review analyzer. |
| `RUST_LOG` | Tracing/log filter for Mjolnir processes. |
| `GH_TOKEN`, `GITHUB_TOKEN` | GitHub token source, checked in that order before `gh auth token`, for private clones and live non-local session sync. |
| `GIT_SSH_COMMAND` | Overrides Mjolnir's non-interactive SSH command for checkpoint/archive Git operations. |

Worker lookup checks `MJ_WORKER_BINARY`, `MJ_WORKER_DIR`, packaged or sibling
workers, compatible native executables, and finally the verified URL fallback.
The normal release installer already supplies both supported portable Linux
worker architectures.

Harness-home variables influence `mj setup` discovery when no profile is yet
written:

| Harness | Discovery variable | Conventional home |
| --- | --- | --- |
| Codex | `CODEX_HOME` | `~/.codex` |
| Claude Code | `CLAUDE_CONFIG_DIR` | `~/.claude` |
| Kimi Code | `KIMI_CODE_HOME` | `~/.kimi-code` |
| Grok Build | `GROK_HOME` | `~/.grok` |
| DeepSeek Harness | `DSH_HOME` | `~/.dsh` |

The release installer separately accepts `MJOLNIR_INSTALL_DIR` (preferred over
`INSTALL_DIR`), `MJOLNIR_GITHUB_OWNER`, `MJOLNIR_VERSION`, `GITHUB_TOKEN`, and
`PROFILE`. See [Install Mjolnir](/install/).

## Data that is not in `config.toml`

The platform data directory, or `MJ_DATA_DIR`, contains operational state:

- `mj.sqlite3` for sessions, workspaces, drafts, read frontiers, remembered
  resource choices, and prompt history;
- `sessions/` for recovery archives;
- `projects/<project-key>/memory/` for canonical project memory;
- `logs/` and `daemon.log` for logs;
- `daemon.json` for daemon discovery; and
- `viewer/` and `diagnostics/` for viewer security material and diagnostic
  reports.

Do not hand-edit the database or daemon files. Use the TUI, viewer, and commands
in the [CLI reference](/cli-reference/). See [Durability and recovery](/durability/)
before moving or deleting session archives.
