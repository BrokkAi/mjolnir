---
title: Configuration reference
description: Complete reference for Mjolnir 2 config.toml, including profiles, bundles, machines, runtimes, review, viewer, paths, and environment overrides.
---

Mjolnir keeps per-user configuration in `config.toml`. Workspaces, sessions,
prompt history, per-session resource choices, drafts, and read markers live in
Mjolnir's state database instead; they are not fields in this file.

Open **Settings** with **prefix+s** to add or edit agent profiles, SSH and EC2
connections, projects, runtime overrides, and interface options. The command
palette (**prefix+:**) also provides **Manage agent profiles**, **Manage
machines**, and **Manage runtimes**. No setup command or file editing is
required. **Detect machine** on the **Agent Profiles** page can import existing
agent accounts for review.

Standard local targets are supplied automatically: localhost, Podman, Docker,
and Apple container on macOS. Saved entries override their defaults. The new,
resume, and move target pickers check every target in the background, including
SSH connectivity and AWS credentials and launch templates. Pending or failed
checks block Next and show the reason; press **prefix+shift+r** to recheck after fixing it.
Checks do not start stopped services, and launch performs a final preflight.

Settings refuses to remove or rewrite configuration used by an active session;
add an alternative entry or stop the session first. Global defaults and profile
enablement remain editable. The optional `mj setup` command and direct
`config.toml` editing remain available for users who prefer them.

If an active session references a missing profile, bundle, or target, Mjolnir
still opens and marks that session as needing configuration repair. Select it
and press Enter for repair details, its retained transcript, or Settings.
The web session menu also provides repair guidance. Restore the named entry in
Settings and retry. Detect machine can rediscover installations, but cannot
reconstruct an arbitrary deleted bundle or custom target. Other
sessions remain accessible, and configuration diagnostics do not change the
affected session's stored lifecycle state.

See [Profiles and harnesses](/profiles/), [Workspaces and bundles](/workspaces-bundles/),
and [Targets](/targets/) for the concepts behind these fields.

## Location and version

The default path is the operating system's configuration directory followed by
`mjolnir/config.toml`. On a typical Linux installation that is
`~/.config/mjolnir/config.toml`. Set `MJ_CONFIG_DIR` to replace the directory;
Mjolnir appends `config.toml` to it.

Pass `--instance <name>` (short `-i`, or `MJ_INSTANCE=<name>`) to run a fully
isolated copy: configuration, database, daemon, and logs move under
`instances/<name>` inside the default directories (for example
`~/.config/mjolnir/instances/dev/config.toml` and
`~/.local/share/mjolnir/instances/dev/mj.sqlite3`). Each instance runs its own
daemon, so parallel instances never share sessions. The name may only use ASCII
letters, digits, `.`, `-`, and `_`. Explicit `MJ_CONFIG_DIR`/`MJ_DATA_DIR`
still take precedence over the instance directories.

Every current file starts with the required schema version:

```toml
version = 12
```

The only accepted top-level keys are:

| Key | TOML type | Required | Default | Purpose |
| --- | --- | --- | --- | --- |
| `version` | integer | yes | none | Configuration schema version; use `12`. |
| `sessions_side` | string enum | no | `"left"` | Place the Sessions sidebar on the `left` or `right`. |
| `show_stopped_sessions` | boolean | no | ignored | Deprecated compatibility field. It is accepted when reading configuration files but has no effect and is omitted on the next save. Use `advanced.show_stopped_sessions` instead. |
| `spinner` | string enum | no | `"scan"` | Activity animation: `scan`, `pulse`, `wave`, `bars`, `shimmer`, or `globe`. |
| `theme` | string enum | no | `"midnight"` | Terminal color palette: `midnight`, `light`, `darcula`, `high-contrast`, or `mono` (no colors). A non-empty `NO_COLOR` environment variable selects `mono` regardless of this setting. |
| `phone` | table | no | default `[phone]` values | Browser and desktop viewer settings. |
| `advanced` | table | no | default `[advanced]` values | Optional terminal display settings. |
| `notify` | table | no | default `[notify]` values | How the terminal dashboard reports sessions that need you. |
| `review` | table | no | default `[review]` values | Independent turn-review settings. |
| `sessionwiki` | table | no | default `[sessionwiki]` values | Full-text session index and automatic archiving. |
| `keys` | table | no | default `[keys]` values | The prefix key and every command's key bindings. |
| `profiles` | table of named tables | no | empty | Named harness accounts and homes. |
| `bundles` | table of named tables | no | empty | Named repository sets for managed targets. |
| `machines` | table of named tables | no | empty | Named hosts sessions run on. `local` is implied even when it is absent. |
| `targets` | table of named tables | no | empty | Named runtimes, each naming the machine it runs on. |
| `subagents` | table | no | default `[subagents]` values | Policy for Mjolnir-owned child agents. |
| `build_cache` | table | no | default `[build_cache]` values | Global switch for the shared mbx build cache. |

The terminal Setup screen groups `sessions_side`, `spinner`, and `theme` under
**Interface**. This is only a presentation grouping; the fields remain at the
top level in `config.toml`.

A missing or empty file is treated as an empty version 12 configuration. Older
versions acquire defaults in memory and upgrade on the next ordinary save. Unknown
fields in the current top-level, viewer, review, profile, bundle, and repository
schemas are errors. If a file declares a version newer than this build
understands, Mjolnir salvages the sections it can read but treats the file as
read-only. `mj doctor` reports that state; update Mjolnir before changing it.

Profile, bundle, repository, machine, and runtime IDs all use the same rule: 1–64 ASCII
letters, digits, `.`, `-`, or `_`. The IDs `.` and `..` are not allowed. IDs are
the TOML table names, for example `work` in `[profiles.work]`.

Sessions are created explicitly through the New session wizard or the CLI/API.
Opening an empty workspace does not create a session. Older `[startup]` settings
are ignored and removed the next time configuration is saved.

## Advanced display options `[advanced]`

The optional `[advanced]` table controls detail that is useful while diagnosing
activity without changing how sessions run:

```toml
[advanced]
detailed_activity_clocks = false
show_stopped_sessions = false
session_order = "project"
# symbols = "ascii"
```

| Field | TOML type | Default | Behavior |
| --- | --- | --- | --- |
| `detailed_activity_clocks` | boolean | `false` | When enabled, normal session rows and the conversation header show separate turn, step, and background clocks. |
| `show_stopped_sessions` | boolean | `false` | When enabled, stopped sessions appear in the terminal Sessions pane for their workspace. |
| `session_order` | `"project"` or `"priority"` | `"project"` | `project` groups sessions under a heading per project in creation order. `priority` lists sessions that need you first (waiting, failed, unread, working, idle) with no project headings. |
| `symbols` | `"unicode"` or `"ascii"` | unset | Which glyphs the dashboard draws status marks, borders, chart bars, and separators with. Unset follows the terminal: ASCII when `TERM` is `linux` or the locale (`LC_ALL`, `LC_CTYPE`, `LANG`) names no UTF-8 encoding, Unicode otherwise. |

The terminal Setup screen edits these settings under **Advanced**. Detailed
clocks do not change how sessions run: the normal `Running` status continues
across the originating turn and its background work.

## Keys `[keys]`

The optional `[keys]` table rebinds the prefix key and every command it
drives. Mjolnir follows tmux's model, described in [Terminal
surface](/terminal-surface/#prefix-key): press the prefix, release it, then
press a second key. An edit here takes effect the next time the daemon
reloads the file, within about a second; the terminal Setup screen does not
show this section, so it is edited by hand.

```toml
[keys]
prefix = "ctrl+b"
new_session = "prefix+c"
refresh = ["prefix+shift+r", "f5"]
switch_workspace = "prefix+1..9"
pane_preset = ""
```

Each field takes one key string or a list of them; a list binds every string
in it to the same command. An empty string (`""`) unbinds a default. The
`1..9` range form, optionally with a leading modifier such as `ctrl+1..9`, is
accepted only for `switch_workspace`; it expands to nine bindings, one per
digit.

A key string is modifier tokens joined by `+`, then a key name. Modifiers are
`ctrl`/`control`, `alt`/`option`/`meta`, `shift`, and `cmd`/`command`/`super`.
Key names include `space`, `enter`/`return`, `esc`/`escape`, `tab`,
`backspace`/`bs`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, the
arrow keys, `f1`–`f12`, punctuation names such as `slash`, `semicolon`, and
`backtick`, and any single character. An uppercase letter is read as the
lowercase letter plus `shift`. A binding that should fire only after the
prefix key carries a `prefix+` marker, for example `prefix+shift+n`; a
binding written without that marker fires directly, with no prefix.

The default bindings:

| Field | Default | Action |
| --- | --- | --- |
| `prefix` | `ctrl+b` | The prefix key itself |
| `help` | `prefix+?` | Open help |
| `settings` | `prefix+s` | Open Settings |
| `detach` | `prefix+q` | Detach this terminal |
| `new_session` | `prefix+c` | Open the session creation wizard |
| `resume` | `prefix+g` | Open the session dialog on every running session, with the resume, import, and archive lists on its other tabs |
| `workspace_manager` | `prefix+shift+n` | Open workspace management |
| `focus_workspaces` | `prefix+w` | Focus the workspace tab row |
| `next_workspace` | `prefix+n` | Select the next workspace |
| `previous_workspace` | `prefix+p` | Select the previous workspace |
| `switch_workspace` | `prefix+1..9` | Select a workspace by number |
| `next_pane` | `prefix+tab` | Focus the next support pane |
| `previous_pane` | `prefix+shift+tab` | Focus the previous support pane |
| `pane_size` | `prefix+shift+z` | Cycle the focused support pane's size |
| `pane_preset` | `prefix+b` | Toggle the dashboard pane preset |
| `refresh` | `prefix+shift+r` | Refresh target capacity and profile quota |
| `palette` | `prefix+:` | Open the command palette |
| `cancel_operation` | `prefix+shift+c` | Cancel an in-flight launch, resume, or stop |
| `mark_all_read` | `prefix+a` | Mark unread session activity as read |
| `next_attention` | `prefix+o` | Open the next session that needs you |
| `previous_attention` | `prefix+shift+o` | Open the previous session that needs you |
| `web_viewer` | `prefix+u` | Show the web viewer address and access code |
| `rename_session` | `prefix+shift+t` | Rename the selected session |
| `toggle_transcript_rendering` | `prefix+t` | Toggle rendered/raw transcript |
| `toggle_dictation` | `prefix+m` | Start or stop dictation |
| `changed_files` | `prefix+d` | List the selected session's changed files |
| `split_vertical` | `prefix+v` | Open the selected session in a pane beside this one |
| `split_horizontal` | `prefix+-` | Open the selected session in a pane below this one |
| `close_pane` | `prefix+x` | Close the conversation pane you are in |
| `focus_pane_left` | `prefix+h` | Move the keyboard to the pane on the left |
| `focus_pane_down` | `prefix+j` | Move the keyboard to the pane below |
| `focus_pane_up` | `prefix+k` | Move the keyboard to the pane above |
| `focus_pane_right` | `prefix+l` | Move the keyboard to the pane on the right |
| `zoom` | `prefix+z` | Fill the conversation area with the pane you are in, or put the others back |
| `last_pane` | `prefix+;` | Move the keyboard back to the pane it was in before |
| `stop_session` | unbound | Stop the selected session |
| `restart_session` | unbound | Restart the selected session |
| `move_session` | unbound | Move the selected session to another target |
| `delete_session` | unbound | Delete the selected session |
| `container_settings` | unbound | Edit container settings for the selected session |
| `manage_profiles` | unbound | Open profile management |
| `manage_targets` | unbound | Open target management |
| `manage_machines` | unbound | Open machine management |
| `restart_daemon` | unbound | Restart the Mjolnir daemon |
| `notice_log` | unbound | Show the last notices the footer reported |
| `change_go_setup` | unbound | Change the `mj go` fast-start setup |
| `cycle_spinner` | unbound | Cycle the activity spinner style |
| `resize_pane_left` | unbound | Move the conversation pane's border left |
| `resize_pane_down` | unbound | Move the conversation pane's border down |
| `resize_pane_up` | unbound | Move the conversation pane's border up |
| `resize_pane_right` | unbound | Move the conversation pane's border right |

The actions listed as unbound have no default key because they are
destructive, infrequent, or fine adjustments that a mis-hit key should not run;
use the command palette (`prefix+:`) instead, or bind them here.

The conversation-pane keys follow herdr and tmux; see [Conversation
panes](/terminal-surface/#conversation-panes). `prefix+-` can also be written
`prefix+minus`.

`Config::validate` rejects an invalid `[keys]` table fatally, the same as any
other configuration error, with one exception: a binding you write silently
displaces a default binding on the same key, because rebinding a command onto
a key another command defaults to is the normal way to move a key.

- The prefix must be a modified chord (carrying `ctrl`, `alt`, or `super`) or
  a function key; a bare letter or a `shift`-only combination is rejected.
- A direct binding (no `prefix+` marker) on an unmodified printable character
  is rejected, because the composer reads that key as text; write
  `"prefix+<key>"` instead.
- A direct binding on `ctrl+c` or `ctrl+v` is rejected: the dashboard and
  composer already treat them as cancel and paste.
- A direct binding whose key equals the prefix is rejected with `the prefix
  key cannot also be a direct binding`, because that key already arms the
  prefix and would never reach the command.
- A prefix binding whose key equals the prefix itself is rejected, because
  that combination is reserved for sending the literal prefix key through
  (see [Terminal surface](/terminal-surface/#prefix-key)).
- Two user-written bindings on the same key is rejected with an error naming
  both fields, for example `keys.pane_preset = "prefix+space": already bound
  by keys.help`.

This key-string syntax matches [herdr](https://github.com/herdrdev/herdr)'s
own `[keys]` table, so a line can be copied between the two configuration
files unchanged.

## Notifications `[notify]`

The optional `[notify]` table controls how the terminal dashboard reports a
session you are not looking at when it asks a question, fails, or finishes
with an answer you have not read. The session whose conversation is on
screen never notifies.

```toml
[notify]
mode = "terminal"
bell = true
delay_seconds = 2
title = true
```

| Field | TOML type | Default | Behavior |
| --- | --- | --- | --- |
| `mode` | `"off"`, `"terminal"`, or `"system"` | `"terminal"` | `terminal` rings the terminal bell, which reaches you through SSH and multiplexers. `system` also posts a desktop notification through `osascript` on macOS or `notify-send` on Linux. `off` reports nothing. |
| `bell` | boolean | `true` | Whether each notification rings the terminal bell. Turn it off with `mode = "system"` for silent desktop notifications. |
| `delay_seconds` | integer | `2` | How long a session must keep needing you before it is reported, so a question the agent answers itself stays quiet. |
| `title` | boolean | `true` | Keep the terminal window title showing the counts, for example `mj · 2 waiting · 1 unread`, independently of `mode`. |

The terminal Setup screen edits these settings under **Notifications**.

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
| `enabled` | boolean | no | `false` | Examines each eligible completed turn after queued work drains; an unchanged delta resolves without a review prompt. |
| `tier` | string enum | no | `"quick"` | `quick` or `extended`. |
| `profile` | string | no | Auto (unset) | Auto selects an eligible profile by provider and quota. A named enabled review-capable profile is honored, including the primary profile. |
| `model` | string | no | unset (harness default) | Main-reviewer override for a named profile. Auto uses fixed model-family defaults; specialist lanes use provider-specific overrides. |
| `effort` | string | no | unset (harness default) | Main-reviewer effort override for a named profile. Auto does not accept manual overrides. Required effort is checked against the selected model. |

These settings also select the plan second-opinion reviewer. Auto prefers another provider, falling back to another profile or the primary profile when needed. The
quick tier runs one general reviewer and validates reported findings. Extended
review may add intent analysis, a supervisor, and specialist lanes. See
[Independent turn review](/turn-review/).

In the terminal, these review fields are edited inside **Setup** so one Save or
Cancel applies to the entire configuration draft. Setup can discover the
selected review profile's supported model and effort choices and filters the
profile list to compatible reviewer profiles.

## Session index `[sessionwiki]`

[SessionWiki](https://github.com/jbellis/sessionwiki) is a separate tool that
indexes AI coding sessions from many tools into one full-text SQLite index. The
daemon always writes Mjolnir's sessions into that index under the tool name
`mjolnir`, and Resume searches it. This section only chooses whether old
sessions are archived, because archiving is the only part that deletes
anything.

```toml
[sessionwiki]
archive_after_days = 30
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `archive_after_days` | integer | no | unset (keep every session) | Stopped sessions older than this many days are removed from Mjolnir once SessionWiki has indexed them. `0` is rejected. |

An `enabled` key written by an earlier build is still read and then ignored;
indexing is no longer optional.

Because indexing is always on and Resume searches the index and nothing else,
the first upgrade to this build builds the index before Resume can be searched.
The search box says **Indexing…** until that first build finishes, which on a
large corpus of other tools' sessions can take several minutes. The list and the
tabs work throughout, and the box opens by itself when the build ends.

The index is the user's own SessionWiki index, in SessionWiki's default
location. There is no index path setting; set `SESSIONWIKI_DATA` if you move it.
A daemon running against an overridden `MJ_DATA_DIR` indexes into
`$MJ_DATA_DIR/sessionwiki` instead, so a test or lab daemon never writes your
own index. Each Mjolnir instance indexes only its own sessions, and every
instance shares the one tool name, so a single search covers them all.

See [Search and restore archived sessions](/sessions/#search-and-restore-archived-sessions)
for what archiving deletes and keeps, and for the rule that the `sessionwiki`
command-line tool must match the version Mjolnir links.

## Profiles `[profiles.<id>]`

Each profile names one harness installation or account on the controller:

```toml
[profiles.codex-work]
kind = "codex"
home = "/home/me/.codex-work"
# enabled = false
# context_window_bytes = 131072
# guardian_review_model = "newest-flash"

[profiles.codex-work.environment]
# PATH = "/opt/node/bin:/usr/local/bin:/usr/bin:/bin"
# PROVIDER_SETTING = "value"
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `enabled` | boolean | no | `true` | Disabled profiles stay configured but cannot be selected for new work, login, import, review, quota reporting, or utility-model inference. Existing running sessions continue. |
| `kind` | string enum | yes | none | `codex`, `claude`, `kimi`, `grok`, or `muse`. |
| `home` | path string | yes | none | Non-empty controller-side harness home. An absolute path is strongly recommended. |
| `environment` | table of strings | no | empty | Environment passed to harness/profile commands. Keys cannot be blank or contain `=`. A Codex profile whose `config.toml` names a custom model provider with `env_key` must set that variable here, with a non-empty value. |
| `context_window_bytes` | integer | no | unset (`262144`-byte fallback) | Conservative byte budget for cross-harness transcript compaction; when set, must be at least `32768`. |
| `guardian_review_model` | string | no | unset (`newest-flash`) | Which model reviews escalated actions in Codex's guardian mode: `newest-flash`, `session`, or a slug from the provider's model catalog. Only valid on a Codex profile whose `config.toml` names a custom model provider. See [Profiles](/profiles/#choose-the-guardian-review-model). |

The profile's harness-home variable cannot appear in `environment`; set `home`
instead. Those variables are `CODEX_HOME`, `CLAUDE_CONFIG_DIR`,
`KIMI_CODE_HOME`, and `GROK_HOME` respectively.

Profiles do not select target-side executables. Raw SSH and EC2 workers resolve
an exact pinned runtime from their managed cache, while local and container
targets use their target runtime. `mj login` invokes the harness's canonical
controller-side command from `PATH`.

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
| `local` | path string | exactly one source | Absolute controller-side Git path used to resolve its network remotes; cannot be combined with `github`. |
| `destination` | path string | yes | Non-empty relative path below the target workspace; no `.` or `..` components. |

Supported GitHub forms are `owner/repository`,
`https://github.com/owner/repository`, `git@github.com:owner/repository`, and
`ssh://git@github.com/owner/repository`, with an optional `.git` suffix. Sources
cannot contain whitespace or begin with `-`. At session creation, Mjolnir uses
the source's default network fetch remote and preserves its configured push
destination(s). A `local` path is inspected for that configuration; its local
commits, staged changes, unstaged changes, and untracked files are excluded
from an isolated clone. Repository destinations may not be equal, ancestors,
or descendants of one another.

Bundles are used by container and EC2 sessions. New bare sessions select an
existing Git project directory instead. `git_ref` is no longer accepted; remove
it and use the source remote's default branch. See [Workspaces and bundles](/workspaces-bundles/)
for network remotes, isolated clones, and project memory.

## Machines `[machines.<id>]`

A machine is a host sessions run on: this computer, an SSH host, or an EC2
launch template. It owns the connection details, the remote workspace
directory bare runtimes use, and the mbx build cache every runtime on it
shares.

The machine `local` is this computer. It always exists, so a file only names
it when it carries a setting of its own.

| Field | TOML type | Required | Default | Accepted values |
| --- | --- | --- | --- | --- |
| `kind` | string enum | yes | none | `local`, `ssh`, or `aws-ec2`. Only the id `local` may hold `kind = "local"`. |

### `local`

```toml
[machines.local.build_cache]
max_size = "50GiB"
```

The only setting is the shared build cache below. Omit the table entirely to
use this machine's own defaults.

### `ssh`

```toml
[machines.builder]
kind = "ssh"
host = "builder.example.com"
user = "ubuntu"
identity_file = "/home/me/.ssh/mjolnir"
extra_args = ["-o", "ServerAliveInterval=30"]
workspace_prefix = ".local/share/hel/workspaces"
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `host` | string | yes | none | OpenSSH host or config alias; non-blank and contains no whitespace. |
| `user` | string | no | unset (SSH/config default) | Cannot be empty, contain whitespace, or contain `@`. |
| `identity_file` | path string | no | unset (SSH/config default) | Private-key path passed to SSH; it is not required to be absolute. |
| `extra_args` | array of strings | no | empty | Additional OpenSSH arguments, passed in order. |
| `workspace_prefix` | path string | no | `".local/share/hel/workspaces"` | Per-session lifecycle/cleanup path prefix for bare runtimes on this machine. It does not select or relocate the remote Git project. May be home-relative or safely absolute. |

`host` and `user` are combined as `user@host`; put only the host or alias in
`host`. `workspace_prefix` cannot be empty, `/`, `.`, bare `~`/`~/`, or contain
`..`. A leading `~/` on a longer path is interpreted relative to the remote
login home.

Two machines may not describe the same SSH connection; one machine is one
host.

### `aws-ec2`

```toml
[machines.fleet]
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
| `ssh_args` | array of strings | no | empty | Additional SSH arguments, passed in order. Note the field name differs from an SSH machine's `extra_args`. |

The launch template owns networking, security groups, storage, AMI, and any
default instance type. The new-session wizard may override the instance type
for one session. An EC2 machine runs a bare harness only: it accepts no
container runtime and no build cache, because each session gets its own
instance. See [AWS EC2](/aws/).

### Build cache `[machines.<id>.build_cache]`

Every container runtime on a machine shares one mbx build cache, so the
settings belong to the machine.

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `enabled` | boolean | no | unset (decided by the machine's filesystem) | `false` runs sessions on this machine without the cache. |
| `directory` | path string | no | unset (the machine's native mbx cache, else `~/.cache/mbx`) | Must be absolute. It is a path on that machine, not on the controller. |
| `max_size` | string | no | unset (the machine's own mbx limits, else `min(100 GB, ¼ of free space)`) | An mbx size such as `100GiB`. Caps the whole cache: build outputs, target directories, and incremental state together. |

A section with every field unset is the same as no section at all.

## Runtimes `[targets.<id>]`

A runtime says how a session runs on a machine: a bare checkout, Podman,
Docker, or Apple `container`. Every runtime requires a `kind` and names the
machine it runs on; `machine` defaults to `local` and is omitted from the file
when it is `local`.

| Field | TOML type | Required | Default | Accepted values |
| --- | --- | --- | --- | --- |
| `kind` | string enum | yes | none | `bare`, `podman`, `docker`, or `apple-container`. |
| `machine` | string | no | `"local"` | The id of a `[machines.<id>]` entry, or `local`. |

`permissions` is valid only on `bare`; setting it on another runtime is an
error. Build cache settings are valid only on a machine; setting them on a
runtime is an error.

### `bare`

```toml
[targets.localhost]
kind = "bare"
```

```toml
[targets.builder]
kind = "bare"
machine = "builder"
permissions = "guardian"
```

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `permissions` | string enum | only on an SSH machine | `"guardian"` | `guardian` preserves harness approvals; `yolo` disables approval and sandbox checks. It has no meaning on `local`, where the harness keeps its configured approvals, or on an EC2 machine. |

The new-session wizard asks for an existing absolute Git project directory on
the runtime's machine. On `local` the harness retains its configured approval
behavior because there is no container or instance boundary. On an SSH machine
the wizard separately asks for an existing absolute remote Git directory; when
**Create managed worktree** is checked on the final review, the new checkout is
created below the repository's own `.mj/worktrees/` tree. A bare runtime on an
EC2 machine launches one instance per session. See
[SSH and SSH Podman](/ssh/) and [AWS EC2](/aws/).

### Common container fields

`podman`, `docker`, and `apple-container` accept the same container fields.

| Field | TOML type | Required | Default | Validation and behavior |
| --- | --- | --- | --- | --- |
| `image` | string | yes | none | Non-blank image reference. |
| `pull_policy` | string enum | no | `"auto"` | `auto`, `always`, `newer`, `missing`, or `never`. |
| `platform` | string | no | unset (runtime selection) | Image platform such as `linux/amd64` or `linux/arm64`; it also determines the required worker architecture when recognizable. |
| `cpus` | string | no | unset (no template override) | Runtime CPU value, for example `"8"`. Per-session selection can override it. |
| `memory` | string | no | unset (no template override) | Runtime memory value, for example `"32g"`. Per-session selection can override it. |
| `environment` | table of strings | no | empty | Environment placed inside the target container. Keys cannot be blank or contain `=`. |
| `workspace_storage` | table | no | `{ kind = "podman-volume" }` | Podman accepts all variants. Docker and Apple Container reject non-default variants. |

The schema checks that `image` is non-blank but leaves CPU, memory, and platform
syntax to the selected runtime. Profile `environment` and target `environment`
are different: profile values configure the harness and profile commands, while
target values become container environment variables.

Pull-policy behavior:

- `auto` starts from an existing image and pulls only when absent. The daemon
  refreshes eligible moving tags for Podman and Docker in the background.
  Versioned tags, digest references, and local images remain pinned or cached.
  Apple Container resolves `auto` during provisioning.
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

### `podman`

```toml
[targets.podman]
kind = "podman"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
platform = "linux/amd64"
cpus = "8"
memory = "32g"
workspace_storage = { kind = "podman-volume" }

[targets.podman.environment]
EXAMPLE = "value"
```

All common container fields are accepted, and `workspace_storage` supports all
three Podman variants. On an SSH machine the container and any workspace volume
live on that host, so supplemental directory sources are paths on it rather
than on the controller. It always runs the harness unconstrained inside the
container boundary. See [Podman](/podman/) and [SSH and SSH Podman](/ssh/).

```toml
[targets.remote-podman]
kind = "podman"
machine = "builder"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
```

### `docker`

```toml
[targets.docker]
kind = "docker"
image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
pull_policy = "auto"
platform = "linux/amd64"
cpus = "8"
memory = "32g"

[targets.docker.environment]
EXAMPLE = "value"
```

All common fields except a non-default `workspace_storage` are supported, on
this machine or on an SSH machine. See [Docker](/docker/).

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

All common fields except a non-default `workspace_storage` are supported. Apple
`container` runs only on this machine. See [Apple container](/apple-container/).

### Files written before version 12

Before version 12 one `[targets.<id>]` table fused the machine and the runtime,
with kinds named `local-bare`, `local-podman`, `local-docker`,
`apple-container`, `ssh-bare`, `ssh-podman`, `ssh-docker`, and `aws-ec2`.
Mjolnir still reads such a file when its `version` is 11 or lower: it derives
the machines, moves each container's `build_cache` onto the machine that owns
it, and writes the new shape on the next save. A file that already says
`version = 12` must use the new kinds; an old one is refused with the spelling
to write instead.

## Complete compact example

This example contains the sections most installations need. Add other machine
and runtime kinds from the examples above rather than mixing fields between
variants.

```toml
version = 12

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

[machines.local.build_cache]
max_size = "100GiB"

[targets.localhost]
kind = "bare"

[targets.podman]
kind = "podman"
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
| `MJ_INSTANCE` | Instance name; same effect as `--instance`. |
| `MJ_SSH_MAX_CONCURRENT` | Cap on concurrent SSH connections per host; see the SSH target guide. |
| `MJ_SSH_CONTROL_MASTER` | Set to `0` to disable SSH connection sharing (ControlMaster) for diagnosis. |
| `MJ_DEV_RESTART_STALE_DAEMON` | When set to any value, a client restarts a running daemon whose executable was replaced, or whose development workers changed, since it started. For development checkouts. |
| `MJ_TURN_STALL_TIMEOUT_MS` | Milliseconds of harness silence, with no tool call open, after which the worker ends the turn with the reason `harness_inactive`. Off unless set to a positive value. |
| `MJ_TURN_TOOL_STALL_TIMEOUT_MS` | Milliseconds one tool call may run before the worker ends the turn the same way. Off unless set to a positive value. |
| `MJ_GITHUB_CLI_BIN` | Path or command name for the GitHub CLI used to read tokens. |
| `RUST_LOG` | Tracing/log filter for Mjolnir processes. |
| `GH_TOKEN`, `GITHUB_TOKEN` | GitHub token source, checked in that order before `gh auth token`, for private clones and live non-local session sync. |
| `GIT_SSH_COMMAND` | Overrides Mjolnir's non-interactive SSH command for checkpoint/archive Git operations. |

Worker lookup checks `MJ_WORKER_BINARY`, `MJ_WORKER_DIR`, packaged or sibling
workers, the native `mj-worker` companion for a bare runtime on this machine,
and finally the verified URL fallback.
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
| Muse Code | `XDG_CONFIG_HOME` (parent of home) | `~/.config/muse` |

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
