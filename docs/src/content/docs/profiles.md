---
title: Profiles and harnesses
description: Configure Codex (including custom model providers), Claude Code, Kimi Code, Grok Build, and Muse Code accounts, credentials, skills, runtimes, and quota reporting.
---

A profile connects Mjolnir to one installed coding-agent harness and one account.
Create multiple profiles for multiple accounts, even when they use the same
harness. Every new or resumed session chooses a profile; the profile ID is also
the row shown in the dashboard's Profiles pane.

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

A session never runs from the profile home itself. Every session, on every
target including a bare runtime on this machine, runs from a staged copy of the
home that belongs to the session, and the harness's home variable points at
that copy. See [What a session's staged home holds](#what-a-sessions-staged-home-holds).

`mj setup` checks the home variable first and otherwise looks in the
conventional location. A detected home becomes the explicit `home` path in
`config.toml`; subsequent sessions use that configured path.

Kimi Code does not expose a guardian approval mode. Mjolnir warns before using
it on a `bare` runtime on this machine, or on a `bare` runtime on an SSH
machine configured with `permissions = "guardian"`. Container runtimes and EC2
machines instead run every harness unconstrained inside the isolation
boundary. See
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
- **The catalog stays in the staged home.** Like every session, the session
  runs from a staged copy of the profile home, so the generated catalog never
  lands in your own Codex directory.
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

A Codex profile that signs in with ChatGPT never gets an API key. Mjolnir
removes `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` and
`OPENAI_BASE_URL` from its harness environment on every target, wherever they
were set: in the profile or target settings, in the target's shell profile, or
in a container image. Otherwise Codex could use the key in place of the login.
The worker log says when it removed one. A profile signs in with ChatGPT unless
its `config.toml` names a custom model provider or its `auth.json` records
`"auth_mode": "apikey"`. Those profiles keep the variables.

### PATH discovery

Mjolnir-owned workers and bridges use non-login shells. On every bare runtime,
whether on this machine, an SSH machine, or an EC2 machine, each worker
performs one bounded login-shell probe and carries only
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
tree, including on a bare runtime on this machine. Do not override `XDG_CONFIG_HOME` or
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

Staging follows symbolic links: a linked file or directory is copied with the
contents of its target, even when the target is outside the harness home. A link
whose target is missing is skipped with a warning, and a link back into a
directory already being copied is skipped, so a loop is copied once.

Claude Code's own `skills/synced/` and `skills/.trash/` directories, and
Codex's own `skills/.system/`, are not copied; see
[Skills synchronization](#skills-synchronization).

Mjolnir then adds its own files to the staged home:

- the managed `mj` skill (see [Managed skills](#managed-skills));
- the session's replica of its project memory, under `projects/`;
- for a Claude session with Mjolnir sub-agents, or a Claude sub-agent, the
  `mj-agents` MCP server in `.claude.json`, and an allow rule in
  `settings.json` for each of its tools, so Claude never asks before a
  sub-agent hands back its report or a parent starts or waits for one;
- for a Codex profile with a custom provider, the generated `models.json` and
  the `config.toml` line that points at it;
- for Kimi Code on a target other than this machine, the `mj-memory` MCP server
  in `mcp.json`;
- for Muse, the permission profile in its settings;
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
the profile home. When the daemon starts, it removes each replica whose session
has ended. It keeps a replica while its session is in this instance's store or
a running process names the session, so it does not remove one that another
Mjolnir instance still uses. A Claude Code project directory also holds the
session's native transcripts; those stay.

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

Every supported harness resolves user skills from `skills/` beneath its profile
home. Mjolnir treats the controller copy as authoritative and pushes it to live
sessions on the same reconciliation cycle. This direction is deliberate: a
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

Mjolnir installs the `mj` skill (driving Mjolnir sessions from an agent) into
every session's staged home. It is written at launch and merged into the tree
pushed on every reconciliation. A user skill at the same path is replaced by the
managed copy in the session only; your own `skills/` directory is left alone.

Session recall and file provenance are provided by the `mj-memory` MCP server:
`search_sessions`, `get_session_brief`, `search_session`, `read_session`,
`trace_file`, `session_files`, and `blame_file`. They query the controller's
session index on local, container, and SSH targets without installing `sw` or
copying the index to the target. Claude receives these history tools and keeps
native project notes; other MCP-capable harnesses also receive the document
tools `list`, `read`, and `write`. Muse currently cannot receive injected MCP
tools. Applicable tools are registered again when a session resumes.

History reads are bounded and include continuation information. The index can
lag active sessions, and older transcripts may lack timestamps or file evidence.
`blame_file` runs Git in the target checkout and reports heuristic attribution;
uncommitted lines remain unattributed. Queries require a connected controller
and time out after 60 seconds. Local project document tools remain usable while
a history query waits. History tools never resume or restore old sessions.
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
