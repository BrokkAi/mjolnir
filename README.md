# Mjolnir

**Run many coding agents at once, and keep them working when you walk away.**

Mjolnir (`mj`) is a terminal dashboard and background service for Claude Code,
Codex, Kimi Code, Grok Build, Muse Code, OpenCode, and DSH. Each session runs
on your laptop, in a container, on any machine you can SSH to, or on EC2, and
keeps going after you close the terminal. You can move a session to another
account, another agent, or another machine without starting over.

By default, Mjolnir sends recent conversation text to **Jev**, a hosted
classifier (Fastino's GLiDE, reached through a Brokk-run proxy). It uses Jev to
tell when an agent is waiting for you or has finished. Turn it off in
**Settings → Privacy** or with `[jev] enabled = false` in `config.toml`; the
[security page](https://mjolnir.brokk.ai/security/#what-leaves-this-machine-by-default)
lists everything that leaves your machine.

[Documentation](https://mjolnir.brokk.ai/) ·
[Quickstart](https://mjolnir.brokk.ai/quickstart/) ·
[Releases](https://github.com/BrokkAi/mjolnir/releases)

<table>
  <tr>
    <td align="center" valign="top">
      <img width="400" alt="Sessions view with targets and quota minimized" src="https://github.com/user-attachments/assets/e444edcd-e9d7-4349-ba70-1ed3dc36d21e" />
    </td>
    <td align="center" valign="top">
      <img width="400" alt="Targets and quota view with sessions minimized" src="https://github.com/user-attachments/assets/84ece206-f3c9-4489-85b6-5d7cf42a08c1" />
    </td>
    <td align="center" valign="top">
      <img width="400" alt="Sessions and quota on a larger screen" src="https://github.com/user-attachments/assets/bd31352c-a961-4bfc-9874-3dc71175937e" />
    </td>
  </tr>
</table>

## Why use Mjolnir

Running one agent in one terminal needs no help. Mjolnir is for when you have
several repositories, several subscriptions, or work that should continue
without you.

- **Sessions outlive your terminal.** Detach, close the window, or restart the
  daemon; the agent keeps working and you reattach where you left off.
- **Isolation by default.** Each session can work in its own checkout, so
  agents do not trip over each other or over your working tree.
- **Run anywhere.** Use a local directory, a Docker, Podman, or Apple
  container, a Linux host over SSH, or an EC2 instance. Remote hosts need no
  setup beyond SSH; Mjolnir uploads what it needs.
- **Move work freely.** Switch a session from your personal account to your
  work account, from Codex to Claude Code, or from your laptop to EC2. The
  session is checkpointed and verified before anything is torn down.
- **See your quota.** Keep several accounts per agent and watch remaining
  subscription quota, and capacity on your hosts (`mj quota`, `mj hosts`), in
  one view. A session that hits its limit can resume itself after the reset.
- **Review before you push.** Independent reviewers, on a different provider
  when you have one, check committed work and report findings.
- **Shared project memory** across sessions and machines.
- **Multi-repository projects** that start, move, and restore together.
- **Scriptable.** Drive sessions with `mj new`, `mj prompt`, `mj wait`, an HTTP
  API, or `mj acp` for editors.
- **Terminal and web.** A full terminal dashboard, plus a web viewer you can
  open from your phone over Tailscale.

## Install

On Linux or macOS (use WSL2 on Windows):

```sh
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash
```

Or with npm:

```sh
npm install -g @brokkai/mjolnir
```

Linux releases need glibc 2.28 or newer on x86-64 or ARM64. See the
[installation guide](https://mjolnir.brokk.ai/install/) for Cargo, source
builds, and desktop dependencies.

## Quick start

From a project directory:

```sh
mj
```

On first launch Mjolnir finds the agents you have installed and shows what it
found. If you have none, choose **Set up a managed agent** and sign in. Press
`ctrl+b c` to create a session, pick an agent and where it runs. Settings is
`ctrl+b s`.

- `mj doctor` checks prerequisites and says how to fix anything missing.
- `mj go` returns you to a folder's last conversation.
- `mj simple` is a minimal mode for tmux (no `ctrl+b` prefix).

The [quickstart](https://mjolnir.brokk.ai/quickstart/) walks through a first
session end to end.

## Documentation

- [What is Mjolnir?](https://mjolnir.brokk.ai/overview/): architecture, goals,
  and supported harnesses and runtimes.
- [Profiles and harnesses](https://mjolnir.brokk.ai/profiles/): accounts,
  login, credentials, and skills.
- [Targets](https://mjolnir.brokk.ai/targets/) and
  [projects](https://mjolnir.brokk.ai/workspaces-bundles/): local, container,
  SSH, and EC2 environments, multi-repository projects, and shared memory.
- [Sessions](https://mjolnir.brokk.ai/sessions/) and
  [durability](https://mjolnir.brokk.ai/durability/): adoption, move, resume,
  checkpoints, and recovery.
- [Terminal](https://mjolnir.brokk.ai/terminal-surface/) and
  [web and desktop](https://mjolnir.brokk.ai/web-viewer/): controls and remote
  access.
- [Code review](https://mjolnir.brokk.ai/turn-review/),
  [configuration](https://mjolnir.brokk.ai/configuration/),
  [CLI reference](https://mjolnir.brokk.ai/cli-reference/), and
  [security](https://mjolnir.brokk.ai/security/).
- [HTTP API](https://mjolnir.brokk.ai/api-reference/) and
  [ACP agent](https://mjolnir.brokk.ai/acp-agent/): automate managed sessions.

The documentation site source lives in [docs/](docs/README.md). Looking for the
previous generation? See [Mjolnir 1.x](https://github.com/BrokkAi/mjolnir/releases/tag/v1.17.0).

## About

Mjolnir is free and open source, built by the engineers at
[Brokk](https://brokk.ai/) because we wanted to use it. It is licensed under
`GPL-3.0-only`.
