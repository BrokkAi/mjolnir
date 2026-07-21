---
title: Configuration
description: Configure Council roles, permissions, ACP servers, review, and appearance.
---

Open `/mjconfig` to edit settings from the TUI. `/models` opens the same editor
on the Council tab. Model and ACP-server changes apply to the next session.

The config schema is versioned. A missing or incompatible `version` starts from
fresh defaults rather than guessing a field-by-field migration.

## Minimal config

```toml
version = 2

[thor]
model = "auto"
discrete_review = true

[loki]
model = "auto"

[eitri]
model = "auto"
max_parallel_explores = 6

[council]
auto_failover = true
permission_mode = "auto"
```

Set Loki or Eitri to `disabled` to turn off that optional role. Thor cannot be
disabled. Explicit model IDs can come from `/models`; availability is checked
when the next session starts.

## ACP policy

Built-in adapters can stay on Auto or be explicitly enabled or disabled. Custom
servers accept a command, arguments, environment values, origin, and policy.

```toml
[acp.policies]
codex-acp = "auto"
claude-agent-acp = "disabled"

[[acp.servers]]
id = "custom:company"
label = "Company agent"
command = "/opt/company/bin/acp-server"
args = ["--stdio"]
origin = "custom"
policy = "enabled"
```

Custom commands inherit Mjolnir's environment and use the workspace as their
working directory. See [Data and trust boundaries](/data-boundaries/).

## One-shot overrides

Headless runs can override roles without changing the saved file:

```bash
mj --print \
  --thor provider/model-id \
  --loki disabled \
  --eitri disabled \
  "summarize this repository"
```

Role overrides require explicit model IDs; `auto` is not accepted as a one-shot
override. The saved configuration remains unchanged.

## Appearance and session controls

Theme and spinner preferences are persistent. Thor's ACP session controls are
available on F1–F9, but model and thought-level selection belong to the Council
and are edited through `/mjconfig` rather than those session controls.

Platform config locations come from the operating system rather than a literal
cross-platform `~/.config` contract. See [Storage and network activity](/storage-network/).
