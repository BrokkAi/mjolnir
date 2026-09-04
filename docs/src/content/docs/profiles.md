---
title: Profiles and harnesses
description: Configure Codex, Claude Code, Kimi Code, Grok Build, and DeepSeek Harness accounts, credentials, skills, runtimes, and quota reporting.
---

A profile connects Mjolnir to one installed coding-agent harness and one account.
Create multiple profiles for multiple accounts, even when they use the same
harness. Every new or resumed session chooses a profile; the profile ID is also
the row shown in the dashboard's Quota pane.

Profiles point at controller-side harness homes. They do not pin a model or
reasoning effort. Choose those per session with `/model` and `/effort`, so the
available choices can come from the provider's current catalog.

## Supported harnesses

| Harness | `kind` | Home variable | Conventional home | Authentication marker | Guardian approvals on a raw target |
| --- | --- | --- | --- | --- | --- |
| Codex | `codex` | `CODEX_HOME` | `~/.codex` | `auth.json` | yes |
| Claude Code | `claude` | `CLAUDE_CONFIG_DIR` | `~/.claude` | `.credentials.json` | yes |
| Kimi Code | `kimi` | `KIMI_CODE_HOME` | `~/.kimi-code` | `credentials/kimi-code.json` | no |
| Grok Build | `grok` | `GROK_HOME` | `~/.grok` | `auth.json` | yes |
| DeepSeek Harness | `deepseek` | `DSH_HOME` | `~/.dsh` | `.credentials.yaml` | no |

`mj setup` checks the home variable first and otherwise looks in the
conventional location. A detected home becomes the explicit `home` path in
`config.toml`; subsequent sessions use that configured path.

Kimi Code and DeepSeek Harness do not expose a guardian approval mode. Mjolnir
warns before using either on a raw `local-bare` target or an `ssh-bare` target
configured with `permissions = "guardian"`. Container and EC2 targets instead
run every harness unconstrained inside the target's isolation boundary. See
[Targets](/targets/) and [Security boundaries](/security/).

## Configure a profile

The minimal shape is:

```toml
[profiles.codex-work]
kind = "codex"
home = "/home/me/.codex-work"
```

Optional environment and compaction controls are useful for nonstandard target
installs:

```toml
[profiles.codex-work]
kind = "codex"
home = "/home/me/.codex-work"
context_window_bytes = 131072

[profiles.codex-work.environment]
PATH = "/opt/node/bin:/usr/local/bin:/usr/bin:/bin"
PROVIDER_SETTING = "value"
```

`home` must be non-empty; use an absolute controller-side path. Environment
keys cannot be blank or contain `=`, and the harness's own home variable cannot
be overridden there. `context_window_bytes`, when present, must be at least
32768. It is a conservative byte budget used when Mjolnir has to compact a
transcript across harnesses; it is not a model-token claim.

For the complete field and validation table, see the
[Configuration reference](/configuration/).

### PATH discovery

Mjolnir-owned workers and bridges use non-login shells. On local-bare, SSH, and
EC2 targets, each worker performs one bounded login-shell probe and carries only
the discovered `PATH` into the non-login runtime. An explicit
`[profiles.<id>.environment]` `PATH` wins. Agent-requested `!` shell commands are
different: they intentionally run through `bash -lc` in the session user's
login environment.

Profile configuration changes take effect after the worker restarts or the
session resumes. Credential and skill reconciliation has its own live sync path
described below.

## Log in

Run:

```console
mj login --profile codex-work
```

When exactly one profile exists, `--profile` may be omitted. With several
profiles it is required. Mjolnir sets the selected home variable and profile
environment before starting the harness's interactive login:

| Harness | Command run by `mj login` |
| --- | --- |
| Codex | `codex login` |
| Claude Code | `claude auth login` |
| Kimi Code | `kimi login` |
| Grok Build | `grok login` |
| DeepSeek Harness | `dsh web` |

The login command is always resolved from the controller's `PATH`. A profile
selects credentials and environment, not another harness executable.

After login, Mjolnir compares the authentication marker before and after the
command and reports whether it changed. A successful update is reconciled into
live sessions while the daemon is running.

### Claude long-lived setup token

Claude's normal OAuth grant rotates. A controller and a copied session can race
to spend the same refresh token when it expires. For long-running managed
sessions, create a non-rotating setup token:

```console
mj login --profile claude-work --setup-token
```

This runs `claude setup-token`, verifies it with `claude auth status`, and stores
it under Mjolnir's configuration directory at:

```text
profiles/<profile-id>/claude-oauth-token
```

New and resumed sessions receive it as `CLAUDE_CODE_OAUTH_TOKEN`. It covers
model requests, not Claude Remote Control or claude.ai connectors. Remove that
file to return the profile to its normal synced credentials. `--setup-token` is
valid only for Claude profiles.

## What enters a managed target

Mjolnir does not copy an entire home directory. When it stages a profile into a
container, remote host, or instance, it copies only this allowlist and skips
symbolic links:

| Harness | Staged home entries |
| --- | --- |
| Codex | `auth.json`, `config.toml`, `AGENTS.md`, `instructions.md`, `rules/`, `skills/` |
| Claude Code | `.claude.json`, `.credentials.json`, `settings.json`, `CLAUDE.md`, `skills/`, `plugins/` |
| Kimi Code | `credentials/`, `config.toml`, `device_id`, `AGENTS.md`, `SYSTEM.md`, `mcp.json`, `skills/`, `agents/`, `plugins/` |
| Grok Build | `auth.json`, `config.toml`, `AGENTS.md`, `agent_id`, `skills/`, `plugins/` |
| DeepSeek Harness | `.credentials.yaml`, `settings.yaml`, `AGENTS.md`, `skills/`, `.agent-presets/` |

History, caches, SSH and GPG keys, shell dotfiles, cloud configuration, editor
state, and package-registry credentials are not copied merely because they sit
under your user home. A raw local session uses the configured local harness home
directly, so it does not gain this target boundary.

Credential bytes travel only in direct controller-to-worker messages. They are
excluded from the durable event journal and recovery archives. Fingerprints and
freshness timestamps may appear in logs; credential contents do not.

## Credential reconciliation

The daemon reconciles every profile with its live sessions about once per
minute and may trigger an immediate pass after an authentication failure.
Normally the controller-side profile home is canonical and replaces older
session copies. If a rotating login becomes fresher inside a session, that copy
can become canonical and then propagate to sibling sessions.

Codex logins are refreshed ahead of expiry when possible. For Claude, prefer the
long-lived setup token above. A reconciliation failure is surfaced rather than
silently discarded.

GitHub authentication is separate from harness authentication. Mjolnir reads
`GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token --hostname github.com`. It
pushes the active token to every live non-local session, including raw SSH, so
HTTPS Git and `gh` work without copying SSH keys. Raw localhost is excluded.
The GitHub token is also excluded from checkpoints and archives.

## Skills synchronization

Every supported harness resolves user skills from `skills/` beneath its profile
home. Mjolnir treats the controller copy as authoritative and pushes it to live
sessions on the same reconciliation cycle. This direction is deliberate: a
session cannot overwrite the canonical skills tree on your machine.

The sync has protective limits:

- 4 MiB maximum encoded skills archive;
- 1 MiB maximum per file;
- 1024 files maximum; and
- no symbolic-link traversal.

The destination tree is replaced atomically. Removing the controller-side
`skills/` directory therefore removes the synced tree on the next successful
reconciliation. Other allowlisted directories such as harness plugins are
staged when a session is created but are not part of this continuous skills
sync.

## Harness runtimes

Mjolnir talks to harnesses through the Agent Client Protocol (ACP). The
published agent image already carries the supported bridge stack. Local bare
and container targets retain that target-provided runtime behavior.

Raw SSH and EC2 workers instead install the exact versions pinned by the
Mjolnir release into `$XDG_CACHE_HOME/mjolnir/harnesses`, or
`$HOME/.cache/mjolnir/harnesses` when `XDG_CACHE_HOME` is unset. They launch
only the resulting absolute path—never an arbitrary compatible executable from
`PATH`. Codex, Claude, and DeepSeek require Node.js 22 or newer plus npm on the
host. Kimi and Grok require curl and Bash for their official installers.
Mjolnir reports a missing prerequisite and leaves the existing worker alone;
it does not invoke sudo or a system package manager.

Installs are content-addressed and shared across sessions for the same remote
user. A cache hit performs only local manifest and executable checks. Upgrades
prepare a new version before replacing a quiet worker. Old versions remain
leased for the complete ACP process lifetime—including busy turns that last
hours—and are garbage-collected only after the final user exits. For custom
container images, see [Custom images](/custom-images/).

## Quota pane

The dashboard asks every configured profile for current capacity and refreshes
profiles independently, so a slow provider does not delay the others. Press
`F5` to refresh Targets and Quota immediately.

| Harness | Quota source shown by Mjolnir |
| --- | --- |
| Codex | Provider usage windows and reset times. |
| Claude Code | Five-hour and weekly subscription windows when reported. |
| Kimi Code | Usage windows returned by the configured Kimi service. |
| Grok Build | The harness's ACP billing extension. |
| DeepSeek Harness | `API`, because it is usage-priced rather than a subscription window. |

An unavailable reading is displayed as an error for that profile; it does not
make the profile disappear. Quota is advisory rather than an admission-control
scheduler. Session creation remains your decision.

Mjolnir may use a currently available non-Claude profile for internal utility
work such as transcript compaction when appropriate. It does not silently move
the primary coding session to another profile.

## Harness limitations

- DeepSeek Harness ACP supports exactly one workspace root. Use either a
  single-repository bundle or one bare project directory, with no attached
  directories.
- Kimi Code and DeepSeek Harness have no guardian approval mode. Prefer an
  isolated [container target](/containers/) or EC2 rather than raw execution.
- A custom bridge must speak the ACP version and features Mjolnir expects.
- A profile home is account-scoped. Do not point two profiles at the same home
  and expect them to represent different accounts.
- Profile `environment` is stored as plain text in `config.toml`; do not use it
  as a general secret store.

Continue with [Session lifecycle](/sessions/) to see how profile, model, effort,
plan mode, and resume choices interact.
