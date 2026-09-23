# Mjolnir

**One control plane for all your coding agents.**

Mjolnir (`mj`) runs Claude Code, Codex, Kimi Code, Grok Build, and Muse Code
sessions side by side, on your laptop, in containers, over SSH, or on EC2, and
keeps them running after you close the terminal. You can move a session to
another account, another harness, or another machine without starting over.

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

## Why Mjolnir

If you use one agent, in one terminal, on one machine, you don't need Mjolnir.
It's for the point where that stops scaling: several subscriptions, several
repositories, several machines, and work that should keep going when you walk
away.

- **Sessions that outlive your terminal.** Each session runs beside a worker
  that owns its prompt queue and event journal. Detach, close the terminal, or
  restart the daemon; the agent keeps working and you reattach where you
  left off.
- **Run anywhere.** Use a local worktree, a Docker, Podman, or Apple container,
  any Linux host over SSH, or an EC2 instance provisioned from a launch
  template. Remote hosts need no resident daemon; Mjolnir uploads a worker on
  demand.
- **Move work freely.** Switch a session from your personal Codex account to
  your work account, from Codex to Claude Code, or from your workstation to
  EC2. Checkpoints are verified before anything is torn down.
- **See every account's quota.** Keep multiple named profiles per harness and
  watch remaining subscription quota and target capacity in one view.
- **Shared project memory.** Agents share synchronized project memory across
  sessions, harnesses, and targets, so what one session learns the next one
  knows.
- **Adversarial review.** Turn it on and an independent reviewer, on a different
  provider when one is available, checks each turn's work and reports
  actionable findings.
- **Multi-repo projects.** Bundle several repositories so they provision,
  checkpoint, move, and restore together.
- **Terminal, web, and desktop.** A full terminal dashboard, plus a private web
  viewer you can reach from your phone over Tailscale, and a desktop app. All
  three share the same live sessions.

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
mj go
```

The first run asks you to pick a harness account and where the session should
run. Each folder remembers its setup, and running `mj go` again returns you to
your last conversation. Plain `mj` opens the full dashboard of workspaces,
sessions, targets, and quota.

`mj doctor` checks prerequisites and tells you how to fix anything missing. The
[quickstart](https://mjolnir.brokk.ai/quickstart/) walks through a first
session end to end.

## Documentation

- [What is Mjolnir?](https://mjolnir.brokk.ai/overview/): architecture, goals,
  and supported harnesses and runtimes.
- [Profiles and harnesses](https://mjolnir.brokk.ai/profiles/): accounts,
  login, credentials, and skills.
- [Targets](https://mjolnir.brokk.ai/targets/) and
  [bundles](https://mjolnir.brokk.ai/workspaces-bundles/): local, container,
  SSH, and EC2 environments, multi-repository projects, and shared memory.
- [Sessions](https://mjolnir.brokk.ai/sessions/) and
  [durability](https://mjolnir.brokk.ai/durability/): adoption, move, resume,
  checkpoints, and recovery.
- [Terminal](https://mjolnir.brokk.ai/terminal-surface/) and
  [web and desktop](https://mjolnir.brokk.ai/web-viewer/): controls and remote
  access.
- [Turn review](https://mjolnir.brokk.ai/turn-review/),
  [configuration](https://mjolnir.brokk.ai/configuration/),
  [CLI reference](https://mjolnir.brokk.ai/cli-reference/), and
  [security](https://mjolnir.brokk.ai/security/).

The documentation site source lives in [docs/](docs/README.md). Looking for the
previous generation? See [Mjolnir 1.x](https://github.com/BrokkAi/mjolnir/releases/tag/v1.17.0).

## About

Mjolnir is free and open source, built by the engineers at
[Brokk](https://brokk.ai/) because we wanted to use it. It is licensed under
`GPL-3.0-only`.
