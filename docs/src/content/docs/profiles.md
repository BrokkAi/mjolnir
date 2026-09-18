---
title: Profiles and harnesses
description: Configure Codex (including custom model providers), Claude Code, Kimi Code, Grok Build, and Muse Code accounts, credentials, skills, runtimes, and quota reporting.
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
| Codex | `codex` | `CODEX_HOME` | `~/.codex` | `auth.json`, or `config.toml` for an API-key provider | yes |
| Claude Code | `claude` | `CLAUDE_CONFIG_DIR` | `~/.claude` | `.credentials.json` | yes |
| Kimi Code | `kimi` | `KIMI_CODE_HOME` | `~/.kimi-code` | `credentials/kimi-code.json` | no |
| Grok Build | `grok` | `GROK_HOME` | `~/.grok` | `auth.json` | yes |
| Muse Code | `muse` | `XDG_CONFIG_HOME` (parent of home) | `~/.config/muse` | `auth.json` | yes |

There are five harness kinds. A Codex profile can also authenticate with an API
key against a model provider other than OpenAI; see
[Codex with a custom provider](#codex-with-a-custom-provider).

`mj setup` checks the home variable first and otherwise looks in the
conventional location. A detected home becomes the explicit `home` path in
`config.toml`; subsequent sessions use that configured path.

Kimi Code does not expose a guardian approval mode. Mjolnir warns before using
it on a raw `local-bare` target or an `ssh-bare` target configured with
`permissions = "guardian"`. Container and EC2 targets instead run every harness
unconstrained inside the target's isolation boundary. See
[Targets](/targets/) and [Security boundaries](/security/).

## Codex with a custom provider

A Codex profile does not have to talk to OpenAI. Codex's own `config.toml`
decides which service it uses, and Mjolnir copies that file into the staged
profile home unchanged. Point it at another service that speaks the Responses
API and the profile runs through the same Codex bridge as any other, with no
`mj login` and no OAuth.

Write the provider into the profile home's `config.toml`. This example uses
Z.ai's GLM Coding Plan:

```toml
model = "glm-5.3"
model_provider = "zai"
model_reasoning_effort = "high"

[model_providers.zai]
name = "Z.ai coding plan"
base_url = "https://api.z.ai/api/v1"
env_key = "ZAI_API_KEY"
wire_api = "responses"
```

`wire_api` must be `responses`; Codex no longer supports the chat-completions
form. `env_key` names the environment variable that carries the API key, and the
key itself goes in the Mjolnir profile, not in the Codex file:

```toml
[profiles.glm]
kind = "codex"
home = "/home/me/.codex-glm"

[profiles.glm.environment]
ZAI_API_KEY = "<your Coding Plan key>"
```

Mjolnir refuses to load a configuration whose provider names a variable the
profile's `environment` does not set, and the error names both the profile and
the variable. A provider may instead inline its key as
`experimental_bearer_token`; prefer `env_key`, because the inline form writes the
key into a file that is copied to every target.

What changes for such a profile:

- **No login.** `mj login` reports that the profile authenticates with its API
  key and exits non-zero. The profile counts as set up as soon as its Codex
  `config.toml` exists, so `mj doctor` reports it authenticated with no
  remediation line. There is no credential file to expire, refresh, or sync into
  a running session.
- **Models come from the provider.** Mjolnir fetches the provider's model catalog
  from `{base_url}/models` before each launch and stages it as `models.json`
  beside the staged `config.toml`. Without it Codex would offer OpenAI's built-in
  model names and send them to your provider. Do not set `model_catalog_json`
  yourself; Mjolnir owns that file and rejects a profile that writes one. If the
  provider is unreachable, the last catalog Mjolnir fetched for the profile is
  staged instead.
- **Guardian reviews run on the newest flash model** the catalog lists. Mjolnir
  stamps that choice on every catalog entry, so a heavyweight session model is
  not also its own reviewer. When the catalog lists no flash model, Codex reviews
  with the session model. Change the choice with
  [`guardian_review_model`](#choose-the-guardian-review-model).
- **A private staged home, always.** Even on a raw local target, the session runs
  from a copy of the profile home rather than the home itself, so the generated
  catalog never lands in your own Codex directory.
- **Quota** is reported for Z.ai (`api.z.ai`) and Zhipu (`open.bigmodel.cn`)
  hosts, which publish the Coding Plan windows. Any other provider reports that
  quota is unavailable for it; the profile still runs sessions.
- **Not a utility model.** Mjolnir's own inference, such as compacting a
  transcript for a handoff, uses chat completions, which these profiles cannot
  serve. Keep an OpenAI Codex or other profile configured for that work.

### Providers that serve a plain model list

Some providers answer `GET {base_url}/models` with Codex's own catalog format,
`{"models": [...]}`, which carries a full description of each model. Z.ai is one
of them. Most OpenAI-compatible providers instead answer with OpenAI's plain
list, `{"object": "list", "data": [{"id": "..."}]}`, which carries only model
ids. DeepSeek is one of those:

```toml
model = "deepseek-v4-pro"
model_provider = "deepseek"
model_reasoning_effort = "high"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com/v1"
env_key = "DEEPSEEK_API_KEY"
wire_api = "responses"
```

Mjolnir accepts both shapes. For a plain list it builds a catalog entry per id
with conservative defaults: a 128,000-token context window, the plain shell tool,
and no reasoning levels, so a session on such a model offers no effort choice
rather than offering one the provider would reject.

Quota reporting is separate from the catalog and covers Z.ai (`api.z.ai`) and
Zhipu (`open.bigmodel.cn`) hosts only. A profile on any other provider, such as
DeepSeek, shows `API` in the quota column, because it is usage-priced rather
than a subscription window. Sessions on it run normally.

A DeepSeek profile can also supply utility inference for Mjolnir's own work,
such as transcript compaction. It has the lowest precedence for that work and
uses the newest `deepseek-*flash*` model in the merged catalog. A Z.ai (GLM)
profile does not supply utility inference. See
[Durability and recovery](/durability/).

### Refine the catalog with your own `models.json`

Put a `models.json` in the profile home to correct or extend what the provider
advertises. It uses Codex's catalog format. Each entry is matched to the fetched
catalog by `slug`; the fields you write replace those on the fetched entry and
every other field survives. A `slug` the provider did not list is added to the
catalog. For example, to give DeepSeek's reasoning model its two effort levels:

```json
{
  "models": [
    {
      "slug": "deepseek-v4-pro",
      "supported_reasoning_levels": ["low", "high"]
    }
  ]
}
```

Mjolnir merges this file into the fetched catalog for every launch and writes
the merged result as the staged `models.json`. Your own file is never edited.
This is the supported way to shape the catalog; `model_catalog_json` in the
Codex `config.toml` is still rejected, because Mjolnir writes that key itself.

### Choose the guardian review model

Set `guardian_review_model` on the profile to decide which model reviews an
escalated action:

```toml
[profiles.glm]
kind = "codex"
home = "/home/me/.codex-glm"
guardian_review_model = "glm-5.3"
```

The accepted values are:

- `newest-flash`, the default when the setting is absent: the newest flash model
  in the merged catalog reviews. Versions compare numerically, so `glm-5.10-flash`
  is newer than `glm-5.3-flash`.
- `session`: nothing is stamped, so Codex reviews with whichever model the
  session is running. This costs more and is the setting to reach for when a
  provider's small model reviews badly, for instance by denying benign actions.
- Any model slug from the merged catalog, such as `glm-5.3` above. A slug the
  catalog does not list fails the launch with an error naming the slug and the
  slugs the catalog does list, rather than quietly reviewing with something else.

The setting applies only to a Codex profile with a custom provider, because
Mjolnir generates a catalog only for those. Setting it on any other profile is a
configuration error.

## Configure a profile

The minimal shape is:

```toml
[profiles.codex-work]
kind = "codex"
home = "/home/me/.codex-work"
```

Profiles are enabled by default. Set `enabled = false` to keep a profile's
configuration without allowing Mjolnir to select or probe it:

```toml
[profiles.codex-standby]
enabled = false
kind = "codex"
home = "/home/me/.codex-standby"
```

A disabled profile is omitted from new, resume, move, review, login, import,
Quota, and utility-model choices. Sessions already running under that profile
continue to operate. Re-enable it before starting new work with it.

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

For Muse, configure `kind = "muse"` and a home ending in `muse`, for example
`home = "/home/me/.config/muse"` or `/home/me/accounts/work/muse`. Discovery
uses `$XDG_CONFIG_HOME/muse` when set. Mjolnir gives each session a private
configuration copy and stores native history under that copy's `.data/muse/`
tree, including on local-bare targets. Do not override `XDG_CONFIG_HOME` or
`XDG_DATA_HOME` in the profile environment.

Run:

```console
mj login --profile codex-work
```

When exactly one profile exists, `--profile` may be omitted. With several
profiles it is required. Mjolnir sets the selected home variable and profile
environment before starting the harness's interactive login:

| Harness | Command run by `mj login` |
| --- | --- |
| Codex | `codex login` (not applicable to an API-key provider) |
| Claude Code | `claude auth login` |
| Kimi Code | `kimi login` |
| Grok Build | `grok login` |
| Muse Code | `muse login` |

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
| Muse Code | `auth.json`, `settings.json`, `trust.json`, `AGENTS.md`, `skills/`, `rules/` |

History, caches, SSH and GPG keys, shell dotfiles, cloud configuration, editor
state, and package-registry credentials are not copied merely because they sit
under your user home. A raw local session uses the configured local harness home
directly, except for Muse's private copy. A private copy isolates agent state;
it does not sandbox a raw local process.

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
published agent image already carries the supported bridge stack. Container
targets use that target-provided runtime.

Local-bare, raw SSH, and EC2 workers instead install the exact versions pinned by the
Mjolnir release into `$XDG_CACHE_HOME/mjolnir/harnesses`, or
`$HOME/.cache/mjolnir/harnesses` when `XDG_CACHE_HOME` is unset. They launch
only the resulting absolute path—never an arbitrary compatible executable from
`PATH`. Codex and Claude require Node.js 22 or newer plus npm on the host.
Kimi and Grok require curl and Bash for their official installers.
Muse requires curl and tar; Mjolnir downloads the pinned native Muse binary and
`muse-acp` adapter and verifies both SHA-256 checksums. Linux and macOS, on
x86-64 and ARM64, are supported. The adapter's Apache-2.0 LICENSE and NOTICE
are retained with the installation; the native Muse binary retains its own
upstream terms.
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
`prefix+shift+r` to refresh Targets and Quota immediately.

| Harness | Quota source shown by Mjolnir |
| --- | --- |
| Codex | Provider usage windows and reset times. For a custom provider, the provider's own windows when it publishes them. |
| Claude Code | Five-hour and weekly subscription windows when reported. |
| Kimi Code | Usage windows returned by the configured Kimi service. |
| Grok Build | The harness's ACP billing extension. |
| Muse Code | Native subscription usage windows and reset times, when reported. |

A Codex profile on a provider that publishes no quota endpoint shows `API`
instead of a window, because it is usage-priced. An unavailable reading is
displayed as an error for that profile; it does not make the profile
disappear. Quota is advisory rather than an admission-control scheduler. Session creation remains your decision.

Mjolnir may use a currently available non-Claude profile for internal utility
work such as transcript compaction when appropriate. It does not silently move
the primary coding session to another profile.

## Harness limitations

Muse supports streamed chat and tools, images, model and effort selectors,
approval questions, cancellation, and native resume. `/plan` invokes Muse's
advertised planning skill; it is not an approval-mode toggle, so no plan-mode
indicator appears and plan approval arrives as chat text rather than a choice
dialog. Muse has no guardian mode: every Muse session runs unconstrained,
whatever the target's policy says. Mjolnir writes the `:unrestricted`
permission profile into the staged Muse settings and uses `allowAll` approvals
and `--disable-sandbox`. Muse decides a session's permission profile from its
settings file and nothing on the wire can change it, so the staged profile is
what lets the session start. The target wizard warns when you pair Muse with a
raw target.

Muse accepts one workspace root, without attached directories. Native import
and checkpoint restore can relocate that workspace while retaining the session
identity. Archives include the selected session and its child streams, not
other sessions or credentials. External Muse sessions normally come from
`~/.local/share/muse/sessions` (`XDG_DATA_HOME/muse/sessions` when set); mj
restores them into the destination profile’s isolated data directory. The adapter does not accept
injected MCP servers, so Muse cannot act as a reviewer or use Mjolnir's
project-memory tools. Another supported reviewer can still review a Muse
primary session. Muse Spark can also supply utility inference for cross-harness handoffs; see
[Durability and recovery](/durability/).

- Kimi Code has no guardian approval mode. Prefer an isolated
  [container target](/containers/) or EC2 rather than raw execution.
- A custom bridge must speak the ACP version and features Mjolnir expects.
- A profile home is account-scoped. Do not point two profiles at the same home
  and expect them to represent different accounts.
- Profile `environment` is stored as plain text in `config.toml`; do not use it
  as a general secret store.

Continue with [Session lifecycle](/sessions/) to see how profile, model, effort,
plan mode, and resume choices interact.
