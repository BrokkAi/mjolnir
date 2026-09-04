---
title: Security boundaries
description: Understand execution policies, copied credentials and data, local Git bridging, attachments, and web-viewer authentication.
---

Mjolnir gives coding agents the ability to execute commands. Its security model
is therefore a set of explicit boundaries, not a promise that agent-generated
commands are harmless.

The central rule is simple: **raw targets preserve approvals; isolated targets
run unrestricted.** A disposable boundary limits damage to the target, but it
does not prevent the agent or its model provider from reading data and
credentials intentionally placed inside that boundary.

## Execution policy is selected by the target

| Target kind | Policy |
| --- | --- |
| `local-bare` | Preserve the selected profile and harness's configured approvals |
| `ssh-bare` with `permissions = "guardian"` | Preserve configured approvals |
| `ssh-bare` with `permissions = "yolo"` | Force unconstrained execution |
| `local-podman`, `local-docker`, `apple-container`, `ssh-podman`, `aws-ec2` | Force unconstrained execution |

Mjolnir translates the unconstrained policy into the selected harness's own
controls:

| Harness | Unconstrained enforcement |
| --- | --- |
| Codex | `agent-full-access` |
| Claude Code | `bypassPermissions` with its sandbox disabled |
| Kimi Code | `auto` |
| Grok Build | always approve with its sandbox disabled |
| DeepSeek Harness | `danger-full-access` |

Kimi's mode is named `auto`, but in this context it approves every call. It is
not a low-risk guardian policy.

Codex, Claude Code, and Grok Build can preserve guardian-style approvals on a
raw target. Kimi Code and DeepSeek Harness cannot. Mjolnir warns when a harness
without guardian support is paired with a raw target, but a warning is not a
sandbox—choose a container or instance instead.

Mjolnir does not expose arbitrary extra container-runtime arguments. Container
names, ownership labels, capabilities, and generated mount modes come from
Mjolnir and the selected runtime rather than from `config.toml`. A chosen image
and runtime must therefore already provide any capability their workload or
mount contract requires. See [Container targets](/containers/) and the
[Configuration reference](/configuration/).

## Decide what belongs inside the trust boundary

The controller host is trusted with the canonical configuration, session
database, recovery archives, profile credentials, and cookie-signing key. A
live target is trusted with:

- the repositories and uncommitted work placed in its workspace;
- the allowlisted profile files needed to run the selected harness;
- live harness and, on non-local sessions, GitHub credentials;
- any extra directories or images attached to the session; and
- prompt text, tool output, and conversation state delivered to the harness.

Anyone who controls the target at a sufficiently privileged level can inspect
those live copies. Isolation protects the rest of the controller host; it does
not make copied secrets unreadable inside the target. Likewise, provider-side
handling of prompts and repository content is governed by the selected
harness and account, not by Mjolnir.

## Profile staging is allowlisted

Mjolnir never copies a harness home wholesale. It builds a per-session profile
home from a harness-specific allowlist:

| Harness | Categories staged into a session |
| --- | --- |
| Codex | Authentication, config, instructions, rules, and skills |
| Claude Code | Authentication and account config, settings, `CLAUDE.md`, skills, and plugins |
| Kimi Code | Authentication, config, device ID, instructions, MCP config, skills, agents, and plugins |
| Grok Build | Authentication, config, agent ID, instructions, skills, and plugins |
| DeepSeek Harness | Authentication, settings, instructions, skills, and agent presets |

Symbolic links encountered while copying an allowlisted profile entry are
skipped. Files outside the allowlist—such as general shell state, unrelated
cloud credentials, and arbitrary caches—do not enter the session merely
because they live beneath your home directory.

The staged profile is still active configuration. Instructions, plugins,
skills, and MCP settings can execute code or direct an agent to external
services. Audit them as part of the selected profile's trust boundary. See
[Profiles and harnesses](/profiles/) for profile setup.

## Credential synchronization and exclusion

The profile on the controller is the canonical copy. While sessions are live,
Mjolnir reconciles rotating harness credentials between that profile and its
session copies about once a minute. Skills are synchronized from the canonical
profile into live sessions on the same cadence.

Credential bytes travel only in private controller-to-worker request and
response frames. They are structurally excluded from the durable event journal
and recovery archive. Non-secret fingerprints and freshness timestamps may
appear in diagnostics.

If the controller's `gh` CLI is authenticated, Mjolnir also pushes its active
GitHub token into every live non-local session, including raw SSH targets. The
token is not included in recovery archives. Its effective authority is still
the authority granted to that GitHub account, so a remote target receiving it
belongs inside the token's trust boundary. Raw local sessions do not receive
this synchronized GitHub token.

Rotating OAuth grants can race when several live copies refresh at once.
Mjolnir refreshes Codex credentials ahead of expiry and distributes the newer
copy. For Claude Code, use a long-lived setup token when running concurrent or
unattended sessions:

```console
mj login --profile <claude-profile-id> --setup-token
```

That token is stored in Mjolnir's private controller data, outside the Claude
profile home, and is passed to new and resumed Claude sessions as
`CLAUDE_CODE_OAUTH_TOKEN`. It authorizes model requests; it does not enable
Claude Remote Control or claude.ai connectors.

## Local repositories use a confined Git bridge

A bundle repository declared with `local = "/absolute/path"` is not exposed to
an isolated target as a writable host mount. Mjolnir serves it through an
authenticated, per-session Git protocol bridge carried over the session's
existing transport. No inbound listener is opened on the controller.

The bridge is confined to the exact repositories named by that session. It
allows Git's upload and receive services so the target can fetch and perform a
normal fast-forward push back to the local checkout. It also:

- disables receive hooks;
- rejects non-fast-forward updates and ref deletion;
- rejects an update to a checked-out branch while the local checkout is dirty;
  and
- rejects Git LFS repositories, which the bridge does not support.

This is a deliberately bounded write path, not a read-only mirror: a successful
fast-forward push can update your local branch and working tree. Review the
agent's branch before accepting that boundary. For repository shapes and dirty
worktree handling, see [Workspaces and bundles](/workspaces-bundles/).

## Directory and image attachments

An additional directory selected in the new-session or resume flow makes that
directory's contents readable by the agent. Do not attach a parent directory
when the agent only needs one child, and never use a read-only choice as a
confidentiality control—it prevents writes, not reads.

Podman, Docker, and Podman-over-SSH normally present writable attachments
through a copy-on-write overlay. Agent writes go to session-owned storage and
do not modify the source directory. A source on a known-incompatible filesystem
such as NFS, SMB, FUSE, FAT, or another overlay is downgraded to read-only and
the launch reports it. Apple containers use read-only attachments. EC2 receives
a copied resource directory rather than a host mount. Bare local and bare SSH
targets do not accept additional directories.

Attachment storage is not the recovery boundary. Put durable results under the
session's project workspace or push them to a repository before stopping. See
[Container targets](/containers/) for runtime-specific behavior.

The web viewer can also attach images to a prompt when the harness advertises
image support. Unsent images remain only in that browser's memory; stored
drafts retain text, not images. Once sent, the image is delivered to the
controller and the selected harness as prompt content. Treat it exactly like
any other data disclosed to the agent and model provider.

## Web-viewer authentication and transport

The personal web viewer is enabled by default. `mj daemon status` prints its
URL and a six-digit login code. The code is exchanged for a signed session
cookie; protected snapshot, transcript, draft, and action APIs all require a
valid cookie. Cookies are HTTP-only and same-site, are marked secure under
HTTPS, and expire after 30 days by default.

The six-digit code is intentionally convenient rather than high entropy. Five
wrong codes lock the login endpoint. Repeated lockouts back off from 30 seconds
to a maximum of one hour, and a correct code clears the failure history. Do not
publish the code or an authenticated browser session.

The cookie-signing key is stored in Mjolnir's private data directory, so signed
in viewers survive daemon restarts. Removing or replacing that key signs every
viewer out. Mjolnir has one personal viewer trust domain; it does not provide
per-user roles or session-level authorization.

Without trusted TLS, the server remains on loopback. With automatic Tailscale
detection, Mjolnir exposes the viewer on the tailnet only after it obtains a
trusted `ts.net` certificate. Explicit non-loopback service requires both a TLS
certificate and key. Mjolnir refuses a non-loopback plaintext listener.

See [Web viewer and desktop app](/web-viewer/) for setup and
[Troubleshooting](/troubleshooting/) for connection and login failures.
