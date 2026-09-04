---
title: What is Mjolnir?
description: Understand Mjolnir's purpose, boundaries, supported coding harnesses, and execution targets.
---

Mjolnir (`mj`) is a terminal control plane for long-lived coding-agent
sessions. It starts Codex, Claude Code, Kimi Code, Grok Build, and DeepSeek
Harness through the Agent Client Protocol (ACP), gives each session a durable
worker, and presents all of them in one terminal dashboard and personal web
viewer.

Mjolnir is designed for the point where “one agent in one terminal” stops
scaling: several accounts, several repositories, several machines, and work
that should continue after the terminal window closes.

Mjolnir 2.x is a new product generation. It is a session control plane, not a
continuation of the 1.x interactive client. Concepts and configuration from a
1.x installation should not be copied into a version 2 configuration.

## How the pieces fit together

A Mjolnir installation has four main parts:

1. The **controller daemon** owns the local session database, provisions
   targets, synchronizes credentials, and serves the web viewer.
2. A **target** is the place where a session runs: a local worktree, a
   container, a named SSH machine, or an EC2 instance.
3. A **session worker** runs on that target beside the selected harness. It
   owns the durable command queue and event journal, so it does not depend on
   an attached terminal client.
4. The **terminal dashboard**, **web viewer**, and **desktop app** are clients
   of the daemon. Detaching a client does not stop the session.

A profile selects a harness account, and a target selects the execution
boundary. Managed container and EC2 sessions use a bundle describing the
repositories in their workspace; bare sessions instead select an existing Git
project directory. See [Profiles and harnesses](/profiles/), [Workspaces and
bundles](/workspaces-bundles/), and [Targets](/targets/).

## Goals

Mjolnir is built to:

- run many concurrent sessions and keep their prompts, output, and queued work
  durable;
- pair unrestricted agent modes with disposable, isolated targets;
- checkpoint a session into a verified recovery archive and resume it on a
  freshly provisioned target;
- keep one canonical harness profile on the controller while synchronizing an
  allowlisted subset into live sessions;
- show sessions, target capacity, account quota, and credential status in one
  control plane;
- fail visibly when a checkpoint, target operation, or version boundary cannot
  be proved safe; and
- remain usable by people through the terminal and web surfaces, and by coding
  agents through machine-readable diagnostics and a scriptable CLI.

## Non-goals

Mjolnir deliberately does not try to be all of the following:

- **A coding agent.** It does not plan work, edit code, or select a model on
  your behalf. The configured harness does that.
- **A privileged host installer.** It does not install Podman or Docker, edit
  subordinate-ID mappings, create AWS networking, or make an SSH host
  reachable. `mj doctor` checks prerequisites and reports remediations.
- **A machine-cloning tool.** It does not copy an entire home directory into a
  target. Shell startup files, SSH and GPG keys, editor state, cloud
  configuration, and arbitrary package credentials are outside the profile
  staging contract.
- **A multi-user team service.** One per-user daemon owns the session store.
  The web viewer is a personal remote control with one viewer trust domain,
  not an authorization system for a team.
- **A scheduler.** Mjolnir reports target capacity but does not load-balance or
  reject work based on utilization.
- **An in-place target manager.** Disposable containers and instances are
  rebuilt from recovery archives. Packages and files outside the declared
  workspace are not durable target state.
- **A best-effort format converter.** Unsupported relay and recovery formats
  are rejected rather than partially restored.

## Supported harnesses

These are the integrations shipped with Mjolnir 2.x today:

| Harness | Config value | Credentials | Quota view | Native state in checkpoints | Guardian approvals on raw targets |
| --- | --- | --- | --- | --- | --- |
| Codex | `codex` | Yes | Yes | Yes | Yes |
| Claude Code | `claude` | Yes | Yes | Yes | Yes |
| Kimi Code | `kimi` | Yes | Yes | Yes | No |
| Grok Build | `grok` | Yes | Yes | Yes | Yes |
| DeepSeek Harness | `deepseek` | Yes | No subscription quota | Yes | No |

“Native state” means Mjolnir can resume the harness's own session when the
same harness is selected again. A cross-harness resume instead restores the
workspace and supplies a size-bounded handoff derived from the canonical
transcript. See [Durability and recovery](/durability/).

Kimi Code and DeepSeek Harness do not provide a guardian approval mode. They
should not be used on a raw, unsandboxed target. DeepSeek Harness currently
accepts one workspace root, so use either a one-repository bundle or one bare
project directory, without attached directories.

## Supported targets

| Target | Config kind | Runs on | Execution policy | Session boundary |
| --- | --- | --- | --- | --- |
| Local Git worktree | `local-bare` | Linux controller host | Configured approvals | A Mjolnir-managed local worktree |
| Podman container | `local-podman` | Linux or WSL2 | Unconstrained | Disposable container |
| Docker container | `local-docker` | Linux or WSL2 | Unconstrained | Disposable container |
| Apple container | `apple-container` | macOS 26+ on Apple silicon | Unconstrained | Disposable container |
| SSH machine | `ssh-bare` | Named Linux host | Guardian or unconstrained | Managed workspace on the named host |
| Podman over SSH | `ssh-podman` | Named Linux host | Unconstrained | Disposable remote container |
| AWS EC2 | `aws-ec2` | Your AWS account | Unconstrained | Disposable instance |

“Configured approvals” means the harness profile remains in control of its
approval behavior. “Unconstrained” means Mjolnir deliberately selects the
harness's full-access mode and relies on the target boundary to contain the
blast radius. The exact controls and data boundaries are documented in
[Security boundaries](/security/).

Use a raw local worktree when you specifically want the agent to operate on
your machine under its normal approvals. Use a container for a disposable
full-access environment. Use SSH or EC2 when the work needs a different host,
architecture, or capacity pool. Target-specific requirements are collected in
the [Targets guide](/targets/).

The controller and viewer run on Linux and macOS, but current `local-bare`
worker launch is Linux-only. On macOS, use Apple Container or a remote target
for sessions. Native Windows is not supported; run Mjolnir under WSL2 instead.

## Where to go next

- [Install Mjolnir](/install/) and follow the [Quickstart](/quickstart/) for a
  first session.
- Read [Terminal surface](/terminal-surface/) to learn the dashboard and its
  global keys.
- Review [Security boundaries](/security/) before selecting an unconstrained
  or remote target.
- Read [Durability and recovery](/durability/) before relying on a session for
  unattended work.
