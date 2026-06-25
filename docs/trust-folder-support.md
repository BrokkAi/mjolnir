# Trust Folder Support

Issue: https://github.com/BrokkAi/mjolnir/issues/228

Date: 2026-06-25

## Recommendation

Mjolnir should not own a general "trusted folder" decision today.

Folder trust changes what an agent may read, write, execute, or load from a
workspace. Those decisions are only meaningful when the component performing the
operation can enforce them. For agent-internal tools, project configuration, and
sandbox policy, that component is the ACP agent. For ACP client capabilities
that Mjolnir hosts, Mjolnir must enforce the boundary it claims.

Mjolnir should instead:

- pass workspace scope to agents through ACP session fields;
- render agent-provided permission requests and session config options;
- avoid persisting a Mjolnir-local trust store that agents may ignore;
- add client support for ACP additional workspace roots as a separate feature.

If ACP later standardizes a trust or safety-policy field, Mjolnir should surface
that field as protocol state and send the user's choice back to the agent. Until
then, trust must remain agent-owned or protocol-owned, not Mjolnir-owned.

## What Mjolnir Does Today

Mjolnir is an ACP client and terminal UI. It does not execute the agent's
general tool policy, but it does host some ACP client capabilities.

Current session setup:

- `src/acp.rs` opens new sessions with `NewSessionRequest::new(cwd.clone())`.
- `src/acp.rs` resumes or loads sessions with `ResumeSessionRequest::new(session_id, cwd)`
  or `LoadSessionRequest::new(session_id, cwd)`.
- `RuntimeSessionState::set_active_session` remembers the active session and
  `cwd`.

Current Mjolnir-hosted client capabilities:

- ACP filesystem reads and writes are checked against the active session root.
- ACP filesystem writes also ask for permission through Mjolnir's permission UI.
- ACP terminal requests are tied to the active session, require a non-empty
  command, and require an absolute `cwd` when one is supplied.
- ACP terminal `cwd` is not currently constrained to the active session root.

Current permissions:

- Agents send `session/request_permission`.
- Mjolnir converts that request into `UiEvent::PermissionRequest`.
- The UI shows the agent-supplied tool call and permission options.
- The selected option id is returned to the agent.

Current session configuration:

- Agents return `SessionConfigOption`s from session setup or config updates.
- Mjolnir renders those options in the config picker.
- When the user changes one, Mjolnir sends `session/set_config_option` with the
  selected value.

Current persisted config:

- `src/config.rs` stores global UI preferences, the default agent command,
  favorite agents, and custom agents.
- It does not store per-folder safety, trust, sandbox, or policy state.

## ACP State

The `agent-client-protocol` schema Mjolnir uses has workspace scope and generic
configuration hooks, but no first-class "trusted folder" field.

Relevant fields and methods:

- `NewSessionRequest` has `cwd` and `additional_directories`.
- `ResumeSessionRequest` and `LoadSessionRequest` also have `cwd` and
  `additional_directories`.
- `SessionConfigOption` is generic and agent-defined.
- `RequestPermissionRequest` is agent-driven and returns only the selected
  permission option.
- ACP `_meta` is explicitly extensibility metadata that implementations must not
  assume semantics for, so it should not be used as a portable trust contract.

`additional_directories` matters for workspace scope, but it is not equivalent
to trust. It tells an agent which additional absolute paths are part of the
session. It does not say whether those paths are trusted, whether project-local
configuration can run, or whether tools may bypass prompts.

## Compatibility Impact

Mjolnir discovers agents through the live ACP registry. On 2026-06-25 that
registry listed 37 agents, including `claude-acp`, `codex-acp`, `gemini`,
`opencode`, `goose`, `cursor`, `github-copilot-cli`, `qwen-code`, `kilo`, and
many others.

A Mjolnir-local trust database would create inconsistent behavior across that
set:

- agents that implement their own trust model might ignore Mjolnir's decision;
- agents without folder trust would receive a misleading UI promise;
- custom ACP commands have unknown trust and sandbox semantics;
- "always allow" permission choices are already agent-defined, so Mjolnir cannot
  know whether they mean command trust, edit trust, project trust, or only a
  specific tool rule.

This is especially risky because Mjolnir can display permission prompts and
enforce Mjolnir-hosted filesystem checks, but cannot guarantee that every
state-changing operation flows through either path. Agents may apply edits, run
commands, load project files, or read agent-specific configuration through their
own runtimes. Mjolnir-hosted terminal execution is also a separate enforcement
surface because it directly spawns the requested command.

## Implementation Follow-Up

Track ACP additional workspace roots in
https://github.com/BrokkAi/mjolnir/issues/236. The invariant for that work is
that additional roots expand workspace scope, not trust.

That follow-up must also define:

- how to gate the feature on ACP `sessionCapabilities.additionalDirectories`;
- how additional roots affect Mjolnir's ACP filesystem root validation;
- whether Mjolnir-hosted terminal `cwd` remains unbounded agent-trusted state or
  becomes constrained to `cwd` plus additional roots.

Do not add a Mjolnir-owned folder trust prompt or persisted trust list unless ACP
standardizes enforceable trust semantics or a specific agent integration can
prove that Mjolnir's stored decision is honored by the enforcing component.
