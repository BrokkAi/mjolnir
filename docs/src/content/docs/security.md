---
title: Security boundaries
description: Understand execution policies, copied credentials and data, network Git clones, attachments, and web-viewer authentication.
---

Mjolnir gives coding agents the ability to execute commands. Its security model
is therefore a set of explicit boundaries, not a promise that agent-generated
commands are harmless.

The central rule is simple: **bare runtimes preserve approvals; isolated
runtimes run unrestricted.** A disposable boundary limits damage to the
runtime, but it does not prevent the agent or its model provider from reading
data and credentials intentionally placed inside that boundary.

## What leaves this machine by default

Your agent's own traffic goes to its model provider under your account, as it
would without Mjolnir. Apart from that, a default installation sends the
following.

| Feature | What is sent | Where it goes | How to turn it off |
| --- | --- | --- | --- |
| Jev turn classifier | After a minute of silence in a turn, and after a reply ends: up to 1 KiB of your latest prompt, up to 2 KiB of the latest assistant text, a size-limited transcript summary, tool titles (up to 128 bytes each), the harness name, and counts of background and queued commands. | The public Cloudflare proxy `mj-jev-proxy.eng-admin-a63.workers.dev`, which forwards it to TypeSafe (`api.typesafe.ai`). With `TYPESAFE_API_KEY` set, or a key in `~/.secrets/typesafe_api_key`, it goes straight to `api.typesafe.ai` with your key. | `[jev] enabled = false`, or **Setup → Privacy → Jev (hosted service)**. A blank key does not disable it; it falls back to the proxy. |
| Automatic continuation | When a reply ends and the agent may have stopped early: your messages since the last context reset and recent assistant replies. Tool history is not sent. | The same proxy (`/v2/continuation-verdict`), or `api.typesafe.ai` with your key. | `[continuation] enabled = false` or `[jev] enabled = false` in `config.toml`, or **Settings → Automatically continue unfinished requests**. |
| Semantic help search | While you type in the help filter, 200 ms after the last key: the filter text and the text of every help row. | The same proxy (`/v1/help-search`), or `api.typesafe.ai` with your key. | `[jev] enabled = false`. It runs only while the help filter has text. |
| Update check | The installed version's channel is asked for the latest release. No session content is sent. | GitHub Releases, the npm registry, or the Homebrew tap on GitHub. | Set `MJOLNIR_NO_UPDATE_CHECK`. |

One switch stops every Jev request: set `enabled = false` under `[jev]` in
`config.toml`, or clear **Setup → Privacy → Jev (hosted service)**. Then
nothing in the first three rows leaves the machine. What you give up: a turn
ends only when the harness ends it, so a turn that goes quiet while the agent
waits for you stays **Working** until the harness reports the end of the
turn; automatic continuation does not run; and help search matches text
only. Workers read the switch when they start, so resume or restart a
running session for it to apply there. See
[Configuration](/configuration/#hosted-jev-service-jev).

The proxy source is in `services/jev-proxy/`. It does not write request bodies
to logs, and it limits requests per client IP address. Mjolnir keeps a local
copy of each Jev request in `jev-decisions/` under its data directory; see
[Session lifecycle](/sessions/).

## Execution policy is selected by the runtime

| Runtime | Policy |
| --- | --- |
| `bare` on the `local` machine | Preserve the selected profile and harness's configured approvals |
| `bare` on an `ssh` machine with `permissions = "guardian"` | Preserve configured approvals |
| `bare` on an `ssh` machine with `permissions = "yolo"` | Force unconstrained execution |
| `podman`, `docker`, `apple-container`, and `bare` on an `aws-ec2` machine | Force unconstrained execution |

Mjolnir translates the unconstrained policy into the selected harness's own
controls:

| Harness | Unconstrained enforcement |
| --- | --- |
| Codex | `agent-full-access` |
| Claude Code | `bypassPermissions` with its sandbox disabled |
| Kimi Code | `auto` |
| Grok Build | always approve with its sandbox disabled |
| Muse Code | the `:unrestricted` permission profile in its staged settings, `allowAll` approvals, and `--disable-sandbox` |

Kimi's mode is named `auto`, but in this context it approves every call. It is
not a low-risk guardian policy.

Codex, Claude Code, and Grok Build can preserve guardian-style approvals on a
bare runtime. Kimi Code cannot, and neither can Muse Code: its permission
profile is a host-lifetime setting that the wire cannot select,
so every Muse session runs unconstrained. Mjolnir warns when a harness without
guardian support is paired with a raw target, but a warning is not a
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

Symbolic links encountered while copying an allowlisted profile entry are
skipped. Files outside the allowlist—such as general shell state, unrelated
cloud credentials, and arbitrary caches—do not enter the session merely
because they live beneath your home directory.

The staged skills tree also carries the Mjolnir-authored `mj` skill.
The `mj-memory` MCP history tools can read the controller's indexed session
corpus, including conversations from other projects. Historical conversations
are reference data, not instructions for the current session.

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

## Isolated sessions use independent network clones

A bundle repository declared with `local = "/absolute/path"` supplies its
configured default network fetch and push destinations. Mjolnir clones the
fetch remote's default branch into the target and starts on that branch.
The target has no Git connection back to the controller checkout.

Local unpublished commits and staged, unstaged, or untracked files are not
copied. Normal Git pushes go to the configured network push destination(s).
Suspending saves a checkpoint without publishing a branch into the host
repository. Only raw local sessions support repositories without network
remotes. See [Workspaces and bundles](/workspaces-bundles/).

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
HTTPS, and expire after 30 days without authenticated requests by default.
Authenticated requests renew new phone-login cookies while preserving the viewer
identity. Desktop bootstrap cookies and cookies issued by older builds retain
their original expiry; sign in again to opt an older phone cookie into renewal.
A zero configured cookie lifetime still produces a browser-session cookie.

The six-digit code is intentionally convenient rather than high entropy. Five
wrong codes lock the login endpoint. Repeated lockouts back off from 30 seconds
to a maximum of one hour, and a correct code clears the failure history. Do not
publish the code or an authenticated browser session.

The QR login URL contains a strong secret derived from the cookie-signing key
in Mjolnir's private data directory. It stays valid across daemon restarts.
Save the original QR login URL as a private bookmark if you want to sign in
again after the browser clears its cookies; the redirected `/` URL does not
contain that credential. The six-digit code still changes at restart.

Anyone who captures the QR or its login URL can sign in until the key is
rotated. Signing out revokes that viewer identity and clears its cookie, but
does not revoke the QR URL or sign other viewers out. Revocations persist in
`phone-cookie-revocations.json` beside the signing key, preventing delayed
responses from restoring access even after a daemon restart. If saving logout
fails, the server reports an error; retry logout once storage is writable.
Keep this file with the signing key; an unreadable or corrupt file prevents
viewer startup rather than accepting revoked cookies.

To revoke all cookies and saved login URLs, stop the daemon, remove
`phone-cookie-key` from its instance data directory, then start it again.
Keep the same durable data directory across restarts to preserve access.

Cookie rejection diagnostics at debug level distinguish an absent, malformed,
expired, revoked, or incorrectly signed cookie without recording credentials. An absent
cookie may mean the browser evicted it; a bad signature may indicate key rotation.
Mjolnir has one personal viewer trust domain; it does not provide per-user roles
or session-level authorization.

Without trusted TLS, the server remains on loopback. With automatic Tailscale
detection, Mjolnir exposes the viewer on the tailnet only after it obtains a
trusted `ts.net` certificate. Explicit non-loopback service requires both a TLS
certificate and key. Mjolnir refuses a non-loopback plaintext listener.

See [Web viewer and desktop app](/web-viewer/) for setup and
[Troubleshooting](/troubleshooting/) for connection and login failures.
