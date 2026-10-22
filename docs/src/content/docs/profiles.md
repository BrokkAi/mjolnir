---
title: Profiles and harnesses
description: Configure Codex (including Amazon Bedrock and custom model providers), Claude Code, Kimi Code, Grok Build, Muse Code, OpenCode, MiMo Code, and DSH accounts, credentials, skills, runtimes, and quota reporting.
---

A profile connects Mjolnir to one installed coding-agent harness and one account.
Create multiple profiles for multiple accounts, even when they use the same
harness. Every new or resumed session chooses a profile; the profile ID is also
the row shown in the dashboard's Profiles pane.

Profiles point at controller-side harness homes. They do not pin a model or
reasoning effort. Choose those per session with `/model` and `/effort`, so the
available choices can come from the provider's current catalog.

Configure Mjolnir sub-agents in **Settings → Sub-agents**. Set a model and,
when offered, effort for **Straightforward** tasks. **Subtle** is optional; if
you set it, choose a smarter model or higher effort than Straightforward. Subtle
cannot be set without Straightforward. These choices apply globally. With both
set, parents choose a task type each time they delegate work. With only
Straightforward, parents delegate straightforward work and do work that
needs judgment themselves.

**Create** and **Move…** offer **Native** or **Mjolnir** delegation. Mjolnir
requires a Claude or Codex profile and a configured Straightforward task.
Existing sessions keep their recorded policy; leaving Move's choice unchanged
retains it.

## Usage by session

Open the dashboard's **Profiles** title menu and choose **Usage by session**,
or find it in the command palette. Each parent session shows its recorded
usage by model and estimated API cost, including its Mjolnir sub-agents. Rows
are sorted by total cost, highest first. Accounting retained for destroyed
sub-agents still contributes to their parent.

The profile dropdown starts on **All**. Choosing a profile selects parents
whose last profile matches; their complete sub-agent trees remain included,
even when children used other profiles. **Hourly** is the default period;
**Daily** covers the trailing 24 hours; **Weekly** and **Monthly** cover the
trailing seven and thirty days. Check
**Include suspended sessions** to include inactive parents. The report refreshes
every thirty seconds, and **Refresh** reloads it immediately.

Costs assume standard base API token rates, including cache reads and default
cache writes, even for subscription profiles. They exclude tools and speed,
region, or long-context premiums. Only completed turns with complete usage
reports enter the estimates. A **+** marks partial accounting, and **unpriced**
marks models or counters that cannot be priced from the recorded information.

## Supported harnesses

| Harness | `kind` | Home variable | Conventional home | Authentication marker | Guardian approvals on a raw target |
| --- | --- | --- | --- | --- | --- |
| Codex | `codex` | `CODEX_HOME` | `~/.codex` | `auth.json`, or `config.toml` for an API-key provider | yes |
| Claude Code | `claude` | `CLAUDE_CONFIG_DIR` | `~/.claude` | `.credentials.json` | yes |
| Kimi Code | `kimi` | `KIMI_CODE_HOME` | `~/.kimi-code` | `credentials/kimi-code.json` | no |
| Grok Build | `grok` | `GROK_HOME` | `~/.grok` | `auth.json` | yes |
| Muse Code | `muse` | `XDG_CONFIG_HOME` (parent of home) | `~/.config/muse` | `auth.json` | no |
| OpenCode | `opencode` | `XDG_CONFIG_HOME` (parent of home) | `~/.config/opencode` | a login in `opencode.db`, or a provider key in the environment | no |
| MiMo Code | `mimo` | `MIMOCODE_HOME` | `~/.mimocode` (stock XDG config and login when unset) | `data/auth.json`, or `~/.local/share/mimocode/auth.json` for the stock install | no |
| DSH | `dsh` | `DSH_ACP_DATA_DIR` | `~/.dsh-acp` | `DEEPSEEK_API_KEY` in the environment (no file) | no |

There are eight harness kinds. Codex can use its built-in Amazon Bedrock
providers; see [Codex on Amazon Bedrock](#codex-on-amazon-bedrock). It can also
authenticate with an API key against a custom provider; see
[Codex with a custom provider](#codex-with-a-custom-provider).

A session never runs from the profile home itself. Every session, on every
target including a bare runtime on this machine, runs from a staged copy of the
home that belongs to the session, and the harness's home variable points at
that copy. See [What a session's staged home holds](#what-a-sessions-staged-home-holds).

`mj setup` checks the home variable first and otherwise looks in the
conventional location. A detected home becomes the explicit `home` path in
`config.toml`; subsequent sessions use that configured path.

Kimi Code does not expose a guardian approval mode. OpenCode and MiMo Code ask
before tools according to their permission rules, and DSH asks before every
write, command, and MCP tool call, but all three run commands on the host
without a sandbox, so they have no guardian either. Mjolnir warns before using
any of them on a raw target. Muse's guardian is muse-acp's auto-review; see
[Harness limitations](#harness-limitations).
Container runtimes and EC2
machines instead run every harness unconstrained inside the isolation
boundary. See
[Targets](/targets/) and [Security boundaries](/security/).

## Codex on Amazon Bedrock

Codex 0.159.1 includes the built-in providers `amazon-bedrock` and
`amazon-bedrock-runtime`. Select one directly in the profile home's
`config.toml`; it does not need a `[model_providers.<id>]` table, `base_url`,
`wire_api`, or an API key. Codex signs requests with AWS SigV4 using its AWS
credential chain, which can use environment credentials, an AWS profile, or an
EC2 instance role.

For GPT-6 Luna through a Bedrock Runtime inference profile, put this in
`/home/ubuntu/.codex-bedrock/config.toml`:

```toml
model = "global.openai.gpt-6-luna"
model_provider = "amazon-bedrock-runtime"
model_reasoning_effort = "high"

[model_providers.amazon-bedrock-runtime.aws]
region = "us-east-1"
```

`aws.region` is an optional Codex override. Codex can also read
`AWS_REGION` or `AWS_DEFAULT_REGION`. Configure the Mjolnir profile like this:

```toml
[profiles.bedrock-luna]
kind = "codex"
home = "/home/ubuntu/.codex-bedrock"

[profiles.bedrock-luna.environment]
AWS_REGION = "us-east-1"
AWS_DEFAULT_REGION = "us-east-1"
```

Mjolnir passes profile environment entries to the harness on local, container,
and remote targets. Use `{ from_env = "AWS_PROFILE" }` for an AWS profile name
from the environment Mjolnir started with, or `{ from_secret = "NAME" }` for
AWS access keys or session tokens. The AWS config and credentials files named
by `AWS_CONFIG_FILE` and `AWS_SHARED_CREDENTIALS_FILE` must exist on the target
where Codex runs. An EC2 instance role needs no key variables or credential
files. For Podman sessions on EC2, set the host's instance-metadata response
hop limit to `2` so the container can reach IMDS, and leave
`AWS_EC2_METADATA_DISABLED` unset or false.

The AWS account must have access enabled for the requested Bedrock model and
inference profile. `mj models --profile bedrock-luna` asks Codex for its
built-in Bedrock catalog; Mjolnir does not try to fetch `/models` with an API
key. If Codex reports no models, the command says so and the `model` configured
in `config.toml` remains the profile's default.

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
form. `env_key` names the environment variable that carries the API key. As
with Codex on its own, exporting that variable is enough: when the profile does
not set it, Mjolnir reads it from the environment it started with and passes it
to the session. The profile needs nothing beyond its home:

```toml
[profiles.glm]
kind = "codex"
home = "/home/me/.codex-glm"
```

The daemon only sees variables from the environment it was started in, so
export the key before running `mj`, and run `mj daemon restart` after exporting
it in a new shell. To keep the key in a file instead, put it in `secrets.toml`
beside `config.toml` and name it in the profile; an entry the profile sets
always wins over the environment:

```toml
[profiles.glm.environment]
ZAI_API_KEY = { from_secret = "ZAI_API_KEY" }
```

A profile whose key is set in neither place cannot start: its sessions are
refused with an error naming the profile, the variable and how to supply it,
and `mj doctor` reports the same. The rest of the configuration, the daemon and
every other profile keep working.

A provider may instead inline its key as `experimental_bearer_token`. Mjolnir
reads that key the same way it reads `env_key`: the profile is authenticated,
the provider's catalog is fetched with the inline key and staged, the Guardian
reviewer is stamped, and the provider can serve quota and utility work. Prefer
`env_key`, because the inline form writes the key into the profile's
`config.toml`, which is copied into every staged home and travels with the
configuration.

What changes for such a profile:

- **No login.** `mj login` reports that the profile authenticates with its API
  key and exits non-zero. The profile counts as set up as soon as its Codex
  `config.toml` exists, so `mj doctor` reports it authenticated with no
  remediation line. There is no credential file to expire, refresh, or sync into
  a running session.
- **Models come from the provider.** Mjolnir fetches the provider's model catalog
  from `{base_url}/models` before each launch and stages it as `models.json`
  beside the staged `config.toml`. Without it Codex would offer OpenAI's built-in
  model names and send them to your provider. If the provider is unreachable, the
  last catalog Mjolnir fetched for the profile is staged instead. If
  `config.toml` sets `model_catalog_json`, Mjolnir ignores the file it names and
  points the staged configuration at its own catalog; your file is never
  modified. Use `models.json` to shape the catalog.
- **Guardian reviews run on the newest flash model** the catalog lists. Mjolnir
  stamps that choice on every catalog entry, so a heavyweight session model is
  not also its own reviewer. When the catalog lists no flash model, Codex reviews
  with the session model. Change the choice with
  [`guardian_review_model`](#choose-the-guardian-review-model).
- **The catalog stays in the staged home.** Like every session, the session
  runs from a staged copy of the profile home, so the generated catalog never
  lands in your own Codex directory.
- **Quota** is reported for Z.ai (`api.z.ai`) and Zhipu (`open.bigmodel.cn`)
  hosts, which publish the Coding Plan windows. A provider on any other host,
  such as DeepSeek, is usage-priced, so its profile shows `API` in the quota
  column and counts as fully available when ranking sub-agent profiles.
- **Utility inference depends on the provider.** Z.ai profiles cannot supply
  Mjolnir's own transcript compaction. DeepSeek profiles can, using their
  chat-completions endpoint; see [Durability and recovery](/durability/).

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
rather than offering one the provider would reject. DeepSeek V4 models
(`deepseek-v4-*`) instead get their 1,000,000-token window and compact
automatically at 800,000 tokens, the same point DeepSeek's own harness uses.

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
A `model_catalog_json` key in the Codex `config.toml` is ignored: the staged
configuration is pointed at Mjolnir's catalog instead, so the key never reaches
a target as a path into your own home.

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

## Codex profile files

Codex reads profile files beside its `config.toml`: `codex -p ds` layers
`$CODEX_HOME/ds.config.toml` over `config.toml`. Mjolnir offers each such file
as a profile of its own, so you choose it when you create a session, in the
**Profile** step or with `mj new --profile codex-ds`.

Launching `mj` detects the files. For every `<name>.config.toml` in a detected
Codex home it adds a profile named `codex-<name>` that uses the same home with
`codex_profile` set. You can also write one yourself:

```toml
[profiles.codex-ds]
kind = "codex"
home = "/home/me/.codex"
codex_profile = "ds"
```

The bridge Mjolnir runs Codex through starts `codex app-server`, which refuses
`--profile`. So Mjolnir layers the file itself the way Codex does. Tables merge
key by key at every depth, and any other value in the profile file replaces
the one in `config.toml`. The merged result is written into each session's
staged `config.toml`. Your own files are never edited.

Everything Mjolnir reads from Codex's configuration uses the merged result: the
model provider, how the profile authenticates, the provider's model catalog,
quota, and the default model. A `ds.config.toml` that switches
`model_provider` to a [custom provider](#codex-with-a-custom-provider) gets
that provider's catalog and key handling, while the plain `codex` profile keeps
its own.

The name must be ASCII letters, digits, `-` or `_`, as `codex -p` requires.
When the named file is missing or does not parse, the profile is unusable and
`mj doctor` and every launch say so. Codex itself would quietly run without the
file instead.

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

A Codex profile that signs in with ChatGPT never gets an API key. Mjolnir
removes `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` and
`OPENAI_BASE_URL` from its harness environment on every target, wherever they
were set: in the profile or target settings, in the target's shell profile, or
in a container image. Otherwise Codex could use the key in place of the login.
The worker log says when it removed one. A profile signs in with ChatGPT unless
its `config.toml` names a custom model provider or its `auth.json` records
`"auth_mode": "apikey"`. Those profiles keep the variables.

### PATH discovery

On every bare runtime, whether on this machine, an SSH machine, or an EC2
machine, each worker performs one bounded probe of the account's shell and
starts it the way a terminal does: as an interactive login shell (`zsh -l -i`
for zsh, `bash -l -i` for bash). The probe begins from a cleared environment, so
nothing the process that launched Mjolnir exported leaks into the session, but
it reads the same startup files a terminal reads. Tools that a version manager
adds to `PATH` — nvm, pyenv, asdf, mise, volta, sdkman, or Homebrew on Linux —
are therefore visible to sessions without extra configuration.

Shell aliases and shell functions are not environment variables, so they never
reach a session. An explicit `[profiles.<id>.environment]` `PATH` wins over the
probe. Agent-requested `!` shell commands are different: they intentionally run
through `bash -lc` in the session user's login environment.

Profile configuration changes take effect after the worker restarts or the
session resumes. Credential and skill reconciliation has its own live sync path
described below.

## Log in

For Muse, configure `kind = "muse"` and a home ending in `muse`, for example
`home = "/home/me/.config/muse"` or `/home/me/accounts/work/muse`. Discovery
uses `$XDG_CONFIG_HOME/muse` when set. Mjolnir gives each session a private
configuration copy and stores native history under that copy's `.data/muse/`
tree, including on a bare runtime on this machine. Do not override `XDG_CONFIG_HOME` or
`XDG_DATA_HOME` in the profile environment.

For OpenCode, configure `kind = "opencode"` and a home ending in `opencode`,
for example `home = "/home/me/.config/opencode"` or
`/home/me/accounts/work/opencode`. Discovery uses `$XDG_CONFIG_HOME/opencode`
when set. OpenCode 2 keeps its logins in its database, `opencode.db`, in its
data directory: `$XDG_DATA_HOME/opencode` (or `~/.local/share/opencode`) for
your own `~/.config/opencode`, and `<home>/.data/opencode` for any other home.
That database also holds every OpenCode session, so Mjolnir reads only the
logins from it, read-only. An OpenCode 1 login in `auth.json`, in the data
directory or under `<home>/.data/opencode` where Mjolnir kept it for OpenCode
1, still reaches sessions until OpenCode 2 takes that directory over, the way
OpenCode 2's own upgrade carries it. Mjolnir gives each session a private configuration
copy with those logins, which the session imports when it starts, and keeps the
session's own database under that copy's `.data/opencode/` tree, including on
a bare runtime on this machine. A login you add, change or remove reaches
running sessions within about a minute: each one reloads its OpenCode session
once it is idle, never during a turn, and continues the same conversation.
Logins come only from the profile; one made inside a session lasts until that
session next reloads. Checkpoints keep the session database without its logins. Do
not override `XDG_CONFIG_HOME` or `XDG_DATA_HOME` in the profile environment.

OpenCode also signs in to a provider from a key in its environment, such as
`DEEPSEEK_API_KEY` or `OPENROUTER_API_KEY`, with no login at all. As with
OpenCode on its own, exporting the key is enough: when the profile does not set
it, Mjolnir reads every provider key the pinned OpenCode reads from the
environment it started with and passes it to the session, along with the
variables a provider in your `opencode.json` or `opencode.jsonc` lists under
`env` and any variable that file substitutes as `{env:NAME}`. A stored login
wins over a key for the same provider, as it does in OpenCode. Export the key
before running `mj`, and run `mj daemon restart` after exporting a new one;
`mj doctor` names the keys a profile is using.

For MiMo Code, configure `kind = "mimo"`. A profile at the conventional
`~/.mimocode` home, which is where the official installer keeps its executable,
uses the person's stock XDG configuration and login:
`~/.config/mimocode` and `~/.local/share/mimocode/auth.json`. That keeps an
existing `mimo auth login` working without a second sign-in. Any other home is
a `MIMOCODE_HOME` root, for example
`home = "/home/me/accounts/work/mimo"`, with its own `config/` and `data/`
trees; discovery uses `$MIMOCODE_HOME` when set. Mjolnir gives each session a
private root with those `config/` and `data/` trees, including on a bare
runtime on this machine. Do not override `MIMOCODE_HOME` in the profile
environment.

For DSH, configure `kind = "dsh"`, for example with
`home = "/home/me/.dsh-acp"`. DSH runs BrokkAi's
[dsh-acp](https://github.com/BrokkAi/dsh-acp) 0.2.0, a DeepSeek coding agent
that speaks ACP itself, so there is no separate bridge. It is not the DeepSeek
harness earlier releases offered as `kind = "deepseek"`; that kind still fails
to load. The home is a `DSH_ACP_DATA_DIR` root. It holds an optional
`AGENTS.md` and dsh-acp's native session records, `sessions/<id>.json`; there
is no configuration file, no login file, and no skills support. With no login
to create it, create the home directory yourself; `mj doctor` reports it
missing until you do. DSH reads no
instruction file itself, so Mjolnir passes the staged `AGENTS.md`, including
its own session guidance, as dsh-acp's system prompt through
`DSH_ACP_SYSTEM_PROMPT` on every launch.

DSH authenticates with an API key in `DEEPSEEK_API_KEY`. As with a
[Codex custom provider](#codex-with-a-custom-provider), Mjolnir takes the key
from the environment it started with unless the profile's `environment` sets
it, for example from `secrets.toml`:

```toml
[profiles.dsh]
kind = "dsh"
home = "/home/me/.dsh-acp"

[profiles.dsh.environment]
DEEPSEEK_API_KEY = { from_secret = "DEEPSEEK_API_KEY" }
```

There is no interactive login, and `mj login` refuses the profile. A profile
without the key cannot start; the refused session and `mj doctor` say how to
supply it. Other dsh-acp settings can go in `environment`:
`DEEPSEEK_BASE_URL` (default `https://api.deepseek.com`), `DEEPSEEK_MODEL`
(default `deepseek-flash`), and `DSH_ACP_MODELS` (default
`deepseek-flash,deepseek-v4-pro`). Mjolnir owns `DSH_ACP_DATA_DIR` (set `home`
instead), `DSH_ACP_SYSTEM_PROMPT` (put instructions in `AGENTS.md` in the
profile home instead), and `DSH_ACP_PERMISSION_MODE` (the target's permission
mode decides it). A profile that sets any of them in `environment` fails to
load.

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
| OpenCode | `opencode auth login` |
| MiMo Code | `mimo auth login` |
| DSH | none; `mj login` refuses the profile, which authenticates with `DEEPSEEK_API_KEY` |

The login command is always resolved from the controller's `PATH`. A profile
selects credentials and environment, not another harness executable.

On macOS, Claude Code keeps its login in one Keychain item rather than in the
home ([anthropics/claude-code#20553](https://github.com/anthropics/claude-code/issues/20553)),
so `mj login` for a Claude profile leaves `CLAUDE_CONFIG_DIR` unset there and
signs in to that one item. Every Claude profile on a Mac shares it.

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

## What a session's staged home holds

Every session runs from a staged home that belongs to it alone. On a bare
runtime on this machine it is `workers/<session id>/profile` under Mjolnir's
data directory (`profiles/<session id>/muse` for Muse), in a container
`/var/lib/hel/profiles/<session id>`, and on an SSH machine or EC2 instance
`~/.local/share/hel/profiles/<session id>`. The harness's home variable, such as
`CODEX_HOME` or `CLAUDE_CONFIG_DIR`, points at it, on macOS as elsewhere.
Closing the session removes it.

Mjolnir does not copy an entire home directory. It copies only this allowlist
from the profile home:

| Harness | Staged home entries |
| --- | --- |
| Codex | `auth.json`, `config.toml`, `AGENTS.md`, `instructions.md`, `rules/`, `skills/` |
| Claude Code | `.claude.json`, `.credentials.json`, `settings.json`, `CLAUDE.md`, `skills/`, `plugins/` |
| Kimi Code | `credentials/`, `config.toml`, `device_id`, `AGENTS.md`, `SYSTEM.md`, `mcp.json`, `skills/`, `agents/`, `plugins/` |
| Grok Build | `auth.json`, `config.toml`, `AGENTS.md`, `agent_id`, `skills/`, `plugins/` |
| Muse Code | `auth.json`, `settings.json`, `trust.json`, `AGENTS.md`, `skills/`, `rules/` |
| OpenCode | `opencode.json`, `opencode.jsonc`, `AGENTS.md`, `skills/`, and the logins from `opencode.db`, as `.data/opencode/mjolnir-credentials.json` |
| MiMo Code | `config/`, `data/auth.json` |
| DSH | `AGENTS.md`, `skills/` (staged, but DSH does not read skills) |

Staging follows symbolic links: a linked file or directory is copied with the
contents of its target, even when the target is outside the harness home. A link
whose target is missing is skipped with a warning, and a link back into a
directory already being copied is skipped, so a loop is copied once.

Claude Code's own `skills/synced/` and `skills/.trash/` directories, and
Codex's own `skills/.system/`, are not copied; see
[Skills synchronization](#skills-synchronization).

Mjolnir then adds its own files to the staged home:

- on localhost, the managed `mj` skill (see [Managed skills](#managed-skills));
- the session's replica of its project memory, under `projects/`;
- for a Claude session with Mjolnir sub-agents, or a Claude sub-agent, the
  `mj-agents` MCP server in `.claude.json`, and an allow rule in
  `settings.json` for each of its tools, so Claude never asks before a
  sub-agent hands back its report or a parent starts or waits for one;
- for a Codex profile with a [profile file](#codex-profile-files), that file
  merged into `config.toml`;
- for a Codex profile with a custom provider, the generated `models.json` and
  the `config.toml` line that points at it;
- for Kimi Code on a target other than this machine, the `mj-memory` history
  MCP server in `mcp.json`;
- for Muse, the permission profile in its settings;
- for OpenCode under an unconstrained policy, the `"permission": "allow"`
  setting in `opencode.json`;
- for MiMo Code under an unconstrained policy,
  `MIMOCODE_DANGEROUSLY_SKIP_PERMISSIONS=1` in the launch environment;
- for DSH, the staged `AGENTS.md` as `DSH_ACP_SYSTEM_PROMPT` in the launch
  environment, and dsh-acp's `permission_mode` on every session open: `ask`
  under configured approvals, `allow` under an unconstrained policy;
- in a container or on an EC2 instance, a note in the instruction file
  (`AGENTS.md` or `CLAUDE.md`) that the environment is disposable.

History, caches, SSH and GPG keys, shell dotfiles, cloud configuration, editor
state, and package-registry credentials are not copied merely because they sit
under your user home. The profile home's own native history is not copied
either, and nothing Mjolnir adds is ever written to the profile home. A staged
home isolates agent state; it does not sandbox a raw local process, which runs
as your account and can read your whole home directory.

### Sessions from earlier releases

Earlier releases ran a local session of Codex, Kimi Code or Grok Build, and a
Claude Code session on macOS, from the profile home itself. Such a session that
is still open keeps running from there until it is suspended and resumed, or
moved, which stages it like any other. Until then its credentials and skills are
not synchronized.

Those sessions also left their project-memory replicas under `projects/hel-*` in
the profile home. Mjolnir does not remove them: several Mjolnir instances can
share a profile home, and one instance cannot tell another's stopped session
from an ended one. The project memory itself is kept in Mjolnir's data
directory, so you can delete a leftover `projects/hel-*` directory by hand once
its session is closed. In a Claude Code project directory, keep the session's
native transcripts.

Credential bytes travel only in direct controller-to-worker messages. They are
excluded from the durable event journal and recovery archives. Fingerprints and
freshness timestamps may appear in logs; credential contents do not.

## Native session history

A harness writes its own record of a session, such as a Codex rollout under
`sessions/` or a Claude Code transcript under `projects/`, into the home it runs
from. For a session Mjolnir runs, that is the session's staged home, not your
profile home, on this machine as on any other target. Mjolnir includes the
native record in the session's checkpoints and removes it with the session.

What this means:

- Your harness's own resume command, such as `codex resume` or
  `claude --resume`, does not list sessions that Mjolnir runs. Find and resume
  them through Mjolnir instead: the **Mjolnir** and **Archived** tabs of the
  session dialog (`prefix+g`), `mj resume`, or `sessionwiki search`.
- The **Import** tab and `mj import` read your profile homes, so they list only
  sessions you ran outside Mjolnir.

## Credential reconciliation

The daemon reconciles every profile with its live sessions about once per
minute and may trigger an immediate pass after an authentication failure.
Every session, including one on this machine, has its own copy of the login in
its staged home. Normally the controller-side profile home is canonical and
replaces older session copies. If a rotating login becomes fresher inside a
session, that copy can become canonical and then propagate to sibling sessions.
Several sessions of one profile therefore hold copies of one rotating grant,
just as container sessions do.

Codex logins are refreshed ahead of expiry when possible. For Claude, prefer the
long-lived setup token above. A reconciliation failure is surfaced rather than
silently discarded.

GitHub authentication is separate from harness authentication. Mjolnir reads
`GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token --hostname github.com`. It
pushes the active token to every live non-local session, including raw SSH, so
HTTPS Git and `gh` work without copying SSH keys. Raw localhost is excluded.
The GitHub token is also excluded from checkpoints and archives.

## Skills synchronization

Every supported harness except DSH, which has no skills support, resolves user
skills from `skills/` beneath its profile home. Mjolnir treats the controller
copy as authoritative and pushes it to live sessions on the same reconciliation
cycle. This direction is deliberate: a
session cannot overwrite the canonical skills tree on your machine.

Claude Code and Codex also keep skills of their own under `skills/`. Claude
Code provisions `skills/synced/` from your claude.ai account and moves skills it
removes into `skills/.trash/`. Codex writes its built-in skills into
`skills/.system/`. These directories belong to the harness, which writes them
in whatever home it runs from, including a session's home. Mjolnir does not
stage, compare, or push them, and it never replaces or removes them in a
session.

The sync reads the controller-side `skills/` the way launch staging copies it.
It follows symbolic links, leaves out a link whose target is missing, and reads
a directory that links back into itself once, so a linked skill is part of both
the staged and the synced tree. It writes only into the session's staged home,
never into your own `skills/` directory.

The whole tree travels as one gzip-compressed archive, base64-encoded, in a
single 8 MiB relay frame. The size limits apply to compressed sizes:

- **1 MiB per file, compressed** (`MAX_SKILLS_FILE_BYTES`). Mjolnir measures a
  file by compressing it on its own; a file of 1 MiB or less always fits. A
  larger file that is still over 1 MiB once compressed, or a file that cannot
  be read, is left out of the sync, and the rest of the tree still syncs. The
  daemon and each session's worker log a warning the first time they skip a
  file, with the file in its `path` field: `skills file is <size> bytes and
  compresses to <compressed> bytes, above the 1048576 byte limit; leaving it
  out of skills sync`. Later skips are logged only at debug level. Launch
  staging has no file size limit, so a new session starts with the file; the
  next push of a changed tree removes it from the session.
- **4 MiB per tree, compressed** (`MAX_SKILLS_ARCHIVE_BYTES`), counting file
  contents, paths, and a few bytes per file. Base64 makes 4 MiB about 5.3 MiB,
  which leaves room in the frame for the rest of the message.
- **64 MiB per tree, uncompressed** (`MAX_SKILLS_TREE_BYTES`). This limit
  keeps a small archive from expanding into more memory than a worker should
  use. Real skills reach the compressed limit long before it. A single file
  larger than this is left out without being read.
- **1024 files per tree** (`MAX_SKILLS_FILES`).

In practice, text compresses well, so skills with several megabytes of
Markdown, HTML, or scripts fit. For example, a 2.2 MB HTML demo page compresses
to about 300 KB. A checked-in binary, such as an image, an archive, or a model
file, hardly compresses at all, so one over 1 MiB is still left out.

Directories a harness keeps for itself do not count toward these limits. A
tree over either per-tree size limit, or with more than 1024 files, is not
trimmed: reconciliation fails for every session of that profile, credential
sync included, until the tree is back within the limits.

A session whose worker comes from a release before compressed archives (relay
protocol 23) reads only the uncompressed format. Until that worker is replaced
at the session's next quiet point after the upgrade, Mjolnir sends it the tree
uncompressed, with the limits counting raw sizes: 1 MiB per file and 4 MiB per
tree. A file over 1 MiB is left out of that session, and the daemon's warning
says it is `above the 1048576 byte limit of an uncompressed skills archive`.

### Managed skills

Mjolnir installs the `mj` skill into localhost sessions. You can ask the model
to "configure mj with ..."; the skill explains editing your Mjolnir
configuration and includes the [configuration reference](/configuration/).
It also teaches session orchestration through the `mj` CLI. Localhost agents
receive the owning daemon's configuration and data paths so their commands
address the same Mjolnir instance.

Container, SSH, and EC2 sessions do not receive this host CLI skill. Their
delegation and session-history MCP tools carry their own instructions. Launch
staging and ongoing skills reconciliation both enforce this distinction,
including removing an older copy from an isolated session. The `skills/mj/`
directory is reserved: localhost sessions receive the managed copy and isolated
sessions omit it, even if the profile contains a user copy. Your own `skills/`
directory is left alone.

Session recall and file provenance are provided by the `mj-memory` MCP server:
`search_sessions`, `get_session_brief`, `search_session`, `read_session`,
`trace_file`, `session_files`, and `blame_file`. They query the controller's
session index on local, container, and SSH targets without installing `sw` or
copying the index to the target. Claude receives these history tools and keeps
native project notes. Other harnesses read and write their session's project
memory replica with their own file tools. Applicable history tools are
registered again when a session resumes.

History reads are bounded and include continuation information. The index can
lag active sessions, and older transcripts may lack timestamps or file evidence.
`blame_file` runs Git in the target checkout and reports heuristic attribution;
uncommitted lines remain unattributed. Queries require a connected controller
and time out after 60 seconds. Local project-memory files remain usable while a
history query waits. History tools never resume or restore old sessions.
User-supplied `recall` and `provenance` skills are preserved.

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
`PATH`. Codex, Claude, and MiMo Code require Node.js 22 or newer plus npm on
the host.
Kimi and Grok require curl and Bash for their official installers.
Muse requires Node.js 22 or newer, npm, and curl; Mjolnir launches the pinned
`muse-acp` adapter through npx and downloads the pinned native Muse binary,
verifying its SHA-256 checksum. Linux and macOS, on x86-64 and ARM64, are
supported. The native Muse binary retains its own upstream terms.
OpenCode requires curl and tar; Mjolnir downloads the pinned OpenCode 2 package
for the platform from npm (`@opencode/cli-<platform>`), the same package
OpenCode's own installer fetches, and verifies its SHA-256 checksum. Linux and macOS, on
x86-64 and ARM64, are supported. The pinned installation disables OpenCode's
self-update so a running session never replaces its own binary.
MiMo Code installs from the pinned `@mimo-ai/cli` npm package. Its managed
launch disables automatic updates so a running session never replaces its own
binary.
DSH requires curl and tar, and no Node.js; Mjolnir downloads the pinned
dsh-acp 0.2.0 GitHub release archive for the platform and verifies its SHA-256
checksum. Linux and macOS, on x86-64 and ARM64, are supported. Linux needs
glibc 2.34 or newer, so musl distributions such as Alpine cannot run it.
Mjolnir reports a missing prerequisite and leaves the existing worker alone;
it does not invoke sudo or a system package manager.

Installs are content-addressed and shared across sessions for the same remote
user. A cache hit performs only local manifest and executable checks. Upgrades
prepare a new version before replacing a quiet worker. Old versions remain
leased for the complete ACP process lifetime—including busy turns that last
hours—and are garbage-collected only after the final user exits. For custom
container images, see [Custom images](/custom-images/).

## Profiles pane

The dashboard asks every configured profile for current capacity and refreshes
profiles independently, so a slow provider does not delay the others. Press
`prefix+shift+r` to refresh Targets and Profiles immediately.

The pane's title, `Profiles ▾`, opens a small menu. Click it, or press `.`
while the pane has focus. **Refresh** runs the same refresh as
`prefix+shift+r`, and **Settings…** opens the Agent Profiles page of Settings.

| Harness | Quota source shown by Mjolnir |
| --- | --- |
| Codex | Provider usage windows and reset times. For a custom provider, the provider's own windows when it publishes them. |
| Claude Code | Five-hour and weekly subscription windows when reported. |
| Kimi Code | Usage windows returned by the configured Kimi service. |
| Grok Build | The harness's ACP billing extension. |
| Muse Code | Native subscription usage windows and reset times, when reported. |
| OpenCode | No published quota endpoint; the profile shows an unavailable reading. |
| MiMo Code | No published quota endpoint; the profile shows an unavailable reading. |
| DSH | No quota endpoint; DeepSeek bills by usage, so the profile shows `API`. |

A Codex profile on a provider that publishes no quota endpoint, and a DSH
profile, show `API` instead of a window, because they are usage-priced. An
unavailable reading is displayed as an error for that profile; it does not make the profile
disappear. Quota is advisory rather than an admission-control scheduler. Session creation remains your decision.

Mjolnir may use a currently available non-Claude profile for internal utility
work such as transcript compaction when appropriate. It does not silently move
the primary coding session to another profile.

## Harness limitations

Muse supports streamed chat and tools, images, model and effort selectors,
approval questions, cancellation, and native resume. `/plan` and `/implement`
switch muse-acp's Plan mode, which moves the session to a Muse host that cannot
write files or run shell commands; plan approval arrives as chat text rather
than a choice dialog.

Muse decides a session's permission profile from its settings file, so
Mjolnir writes it into the staged Muse settings. With guardian approvals it
stages `:ask-me`, keeps Muse's sandbox on (removing `--disable-sandbox` from a
profile's `MUSE_SERVE_ARGS`), selects `promptUnmatched`
approvals, and turns on muse-acp's auto-review for every session: a second,
read-only Muse model reviews each approval request with a Codex-style safety
policy, and a failed or unusable review denies the action. Muse's own
`:auto-review` profile is never staged, because `muse serve` refuses it.
Unconstrained sessions stage `:ask-me` as well, keep `promptUnmatched`
approvals and auto-review on, and disable only the sandbox
(`--disable-sandbox`): `allowAll` would emit no permission requests, leaving
the reviewer nothing to decide.

Muse accepts one workspace root, without attached directories. Native import
and checkpoint restore can relocate that workspace while retaining the session
identity. Archives include the selected session and its child streams, not
other sessions or credentials. External Muse sessions normally come from
`~/.local/share/muse/sessions` (`XDG_DATA_HOME/muse/sessions` when set); mj
restores them into the destination profile’s isolated data directory.

Unconstrained Muse reads and writes its session's project-memory files
directly because its sandbox is disabled; with guardian approvals, writes
outside the workspace go to auto-review. It receives Mjolnir's session-history and review MCP tools
through muse-acp 0.8.0 or newer with Muse Code 1.3.0 or newer, which managed
targets and the agent-dev image install. A session whose Muse runtime does not
accept MCP servers, such as one in a container created from an older image,
continues without those tools and says so; suspend and resume it once the image
has updated. A Muse reviewer needs its review MCP tools, so it does not start
without them. Muse Spark can also supply utility inference for cross-harness
handoffs; see
[Durability and recovery](/durability/).

- Kimi Code, OpenCode, MiMo Code, and DSH have no guardian approval mode.
  Prefer an isolated [container target](/containers/) or EC2 rather than raw
  execution.
- OpenCode talks to third-party providers through its own gateway, so it
  publishes no quota endpoint and its profile shows quota as unavailable. It
  reads permission rules from `opencode.json`. Its sessions live in a SQLite
  database, so native import and SessionWiki search do not list them, and it
  never serves Mjolnir's delegation or utility inference.
- MiMo Code also publishes no quota endpoint, accepts one workspace root, and
  keeps native sessions in a SQLite database. Native import and SessionWiki
  search do not list them yet, and it does not serve Mjolnir's delegation or
  utility inference.
- DSH has no sandbox: commands run directly on the host, and only its file
  tools are confined to the workspace roots. Under configured approvals
  Mjolnir selects dsh-acp's `permission_mode` `ask` on every session open, so
  every write, command, and MCP tool call asks for permission; an
  unconstrained policy selects `allow`. The Plan toggle switches dsh-acp to its
  `plan` mode and back to its `agent` mode. DSH accepts multiple workspace
  roots and attached directories. Checkpoints keep the session's
  `sessions/<id>.json`, and a restore at a different workspace path rewrites
  the record's working directory so the session resumes. Native import and
  SessionWiki search do not support DSH yet. It does not serve Mjolnir's
  delegation, which needs a Claude or Codex profile. It serves DeepSeek-family
  utility inference with its flash model only when its base URL is DeepSeek's
  own `api.deepseek.com` and its key is set.
- A custom bridge must speak the ACP version and features Mjolnir expects.
- A profile home is account-scoped. Do not point two profiles at the same home
  and expect them to represent different accounts.
- Profile `environment` is stored as plain text in `config.toml`; do not use it
  as a general secret store.

Continue with [Session lifecycle](/sessions/) to see how profile, model, effort,
plan mode, and resume choices interact.
