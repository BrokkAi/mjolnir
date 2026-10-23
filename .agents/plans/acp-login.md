# Sign in to a profile through ACP

This is a living ExecPlan maintained under `.agents/PLANS.md`.

## Purpose / Big Picture

A person should not have to install Codex, Claude, Muse, OpenCode, Kimi, Grok,
MiMo, or DSH separately just to create a Mjolnir profile. Mjolnir already
installs an exact pinned harness for sessions. `mj login` should use that same
installation as an ACP agent and drive its advertised authentication methods:
a `terminal` method's command, a known terminal fallback, or ACP
`authenticate`. Only when the bridge advertises no usable method should it fall
back to the harness CLI's own login command.

The user-visible result is that `mj login --profile <id>` can sign in a profile
using the pinned harness Mjolnir manages, with no ambient harness installation.

## Progress

- [x] Expose the worker's managed harness resolver to the CLI.
- [x] Add the ACP auth client and terminal-method runner.
- [x] Wire `mj login` to prefer ACP and fall back to the CLI.
- [x] Cover discovery, method selection, terminal execution, and fallback with tests.
- [x] Update the login documentation.
- [x] Run formatting, focused tests, full tests, and Clippy.

## Surprises & Discoveries

- Muse advertises a real `terminal` auth method: `muse-acp login`, with the
  exact command and args in `_meta.terminal-auth`. `authenticate` returns an
  empty result because the client is expected to run that command. This is the
  same flow Zed uses.
- OpenCode 2.1.2 advertises `opencode-login` with only a description ("Run
  `opencode auth login` in the terminal"); its `authenticate` returns an empty
  result and sends no client request. It therefore needs a known fallback
  command for that method id.
- MiMo is OpenCode underneath and advertises the same `opencode-login` method.
- Codex advertises `chat-gpt` and `api-key`. `authenticate` performs the flow:
  the browser path needs a desktop opener, and the API-key path needs
  `CODEX_API_KEY` or `OPENAI_API_KEY`.
- Claude agent-acp 0.94.0 advertises no auth methods; its login remains the
  Claude CLI's.
- DSH advertises only an API-key method; API-key and inline-key profiles have no
  interactive login.

## Decision Log

- Decision: put the ACP auth client in `mj-cli`, not the worker. `mj login`
  runs in the person's terminal, which is where an interactive terminal-auth
  command must run, and the command must be able to use inherited stdio.
  Rationale: no new relay protocol or PTY transport is required.
- Decision: make the existing managed-harness resolver available from
  `brokk-mj-worker` and depend on it from `mj-cli` only on unix. The installer,
  its pins, its checksums, its leases and its garbage collection stay in one
  place.
  Rationale: duplicating the installer or moving its assets would create a
  second source of truth for pinned harnesses.
- Decision: prefer a terminal method, then a method whose id names an
  interactive login, then the first method. `--method <id>` overrides.
  Rationale: Muse and OpenCode expose one usable terminal action; Codex exposes
  two agent methods and needs a deterministic default.
- Decision: fall back to the existing CLI login when the managed install fails,
  the bridge cannot start, or it advertises no methods. A failure inside a
  chosen method is reported, not silently replaced by a different flow.
- Decision: leave the existing credential verification in place. ACP login is a
  different way to write the same profile credential file or database.

## Outcomes & Retrospective

`mj login` now resolves the same pinned harness installation a session uses,
starts it as an ACP agent, reads `authMethods`, and performs the selected login:
a `terminal` method's advertised command, a known terminal action for OpenCode
and MiMo, or ACP `authenticate` for Codex. `--method` chooses when a bridge
offers several. Empty method lists, a failed installation, and a bridge that
cannot start fall back to the existing harness CLI login; a failure inside a
chosen method is reported. The command no longer needs the daemon or database;
it loads configuration directly, so a fresh machine can sign in before its
first session.

Validation: six focused tests exercise a scripted ACP bridge (terminal command
with the profile environment, `authenticate`, unknown-method listing, empty
methods, the OpenCode fallback, and selection). An isolated run with a pinned
OpenCode installation printed the exact managed path, selected
`opencode-login`, and launched `opencode auth login --standalone` under a PTY.
`cargo test`, `cargo clippy --all-targets -- -D warnings`, and formatting pass.

Remaining limitation: Claude agent-acp 0.94.0 advertises no ACP methods, so its
login still uses an installed `claude` CLI. Kimi and Grok were not installed in
the validation environment, so their bridges' methods remain unverified; a
terminal method they advertise is handled generically, and otherwise their CLI
fallback is used.

## Context and Orientation

`mj-cli/src/main.rs::login` currently resolves `mj_core::credentials::login_command`
and runs the harness CLI with the profile environment. `mj-worker` already owns
the pinned installer in `mj-worker/src/worker_runtime/harness.rs`, including
`resolve`, which installs or reuses an exact version and returns the ACP bridge
command, arguments, extra environment and an install lease. The bridge commands
and auth methods are pinned in `mj-core/src/harness_runtime.rs`.

The ACP initialize response carries `authMethods`. A method may be a terminal
method (`type: "terminal"`, optional `args`, and optional
`_meta.terminal-auth.command`/`args`) or an agent method, which is handled by
the ACP `authenticate` request with `methodId`.

## Milestones

1. Public managed-harness resolution from the worker crate.
2. ACP discovery, selection and execution in `mj-cli`.
3. `mj login` integration with fallback and verification.
4. Tests and documentation.

## Concrete Steps

    cargo test -p brokk-mjolnir acp_login
    cargo test -p brokk-mj-worker worker_runtime
    cargo test
    cargo clippy --all-targets -- -D warnings

## Validation and Acceptance

A fake ACP bridge proves that `mj login` reads `authMethods`, selects the
terminal method, runs its command with the profile environment and inherited
stdio, and reports a useful failure for an unknown method. An empty method list
or a failed managed installation must fall back to the existing CLI login. The
real pinned Muse and OpenCode bridges must expose the methods recorded above.

## Idempotence and Recovery

The managed resolver is atomic, reuses a complete installation, and holds a
lease while the login runs. A failed login leaves the profile home unchanged
except for whatever the harness itself wrote, exactly as the CLI login does.
The existing fingerprint check reports whether anything changed.

## Artifacts and Notes

Probe evidence from the pinned bridges is recorded in Surprises & Discoveries.
No live credentials were read or written while probing.

## Interfaces and Dependencies

`mj-worker` gains a public unix-only `managed_harness` resolver returning the
command, arguments, environment, entrypoint and lease. `mj-cli` gains a
terminal-side `acp_login` module. No new crate is introduced. `mj-cli` depends
on `brokk-mj-worker` for unix targets only.
