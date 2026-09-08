# Mjolnir

Mjolnir (`mj`) is a session manager for coding agents that provisions their
execution environments and lets you continue work across harnesses, accounts,
and machines.

Start a task with Claude Code locally, continue with Codex in a remote container,
and restore the session on another host. Choose the harness account and execution
target independently; Mjolnir handles provisioning, credential synchronization,
checkpointing, and the handoff.

[Documentation](https://mjolnir.brokk.ai/) ·
[Quickstart](https://mjolnir.brokk.ai/quickstart/) ·
[Releases](https://github.com/BrokkAi/mjolnir/releases)

## Why Mjolnir

- **Provision the environment with the session.** Launch disposable Docker or
  Podman containers locally or over SSH, or EC2 instances from your launch
  templates. Mjolnir uploads session workers to remote hosts without requiring a
  resident Mjolnir daemon on each host. Multi-repository bundles give each session
  the complete project layout.
- **Choose the harness, account, and machine independently.** Keep multiple
  profiles for the same harness, adopt existing native sessions, and move work to
  another account or target. When switching harnesses, Mjolnir preserves repository
  state and the visible conversation and gives the new harness a condensed
  handoff. Same-harness resume restores native session state.
- **Keep credentials and project knowledge available.** Mjolnir continuously
  synchronizes whitelisted credentials into live targets and shares project memory
  across sessions and harnesses. The dashboard brings profile quota and target
  capacity together; independent adversarial review can check completed work.

The comparison below covers product capabilities. **—** means no first-class
capability; manual scripts, host setup, and filesystem access are described where
relevant. Compared against repository snapshots inspected on **September 8, 2026**:
[Herdr](https://github.com/herdrdev/herdr),
[Paseo](https://github.com/getpaseo/paseo), and
[T3 Code](https://github.com/pingdotgg/t3code).

### Provisioning

| Feature | Mjolnir | Herdr | Paseo | T3 Code |
|---|---|---|---|---|
| **Remote targets** | Anything reachable over SSH¹ | SSH machines with Herdr installed | Machines running a reachable Paseo daemon | SSH/WSL environments running a T3 backend |
| **Remote execution** | Uploads an on-demand session worker; no resident per-host daemon | Requires a Herdr server on each host | Requires a Paseo daemon on each host | Requires a T3 backend in each environment |
| **EC2** | Provisions and terminates instances from launch templates | Manual setup as a remote host | Manual setup as a remote host | Manual setup as a remote environment |
| **Session containers** | Docker, Podman | — | — | — |
| **Credential synchronization** | Continuously syncs whitelisted credentials into live targets | Host-local credentials | Per-daemon credentials | Per-environment credentials |

### Session continuity

| Feature | Mjolnir | Herdr | Paseo | T3 Code |
|---|---|---|---|---|
| **Native-session adoption** | All supported harnesses | No external adoption; only restarts sessions it was already supervising | All supported providers with native list/load support | No external adoption; continues T3-owned sessions |
| **Resume sessions across profiles and harnesses** | ✓ | Manual handoff | `/paseo-handoff` skill | Same-harness only² |
| **Cross-host move and restore** | ✓ — live Move or stop/resume onto another target | — | — | — |
| **Multi-repo projects** | Bundles provision, checkpoint, review, move, and restore member repos together³ | Filesystem access only; separate workspaces or panes | Filesystem/provider access only; one root per workspace | Filesystem access only; one workspace root per project |

### Harnesses and accounts

| Feature | Mjolnir | Herdr | Paseo | T3 Code |
|---|---|---|---|---|
| **Supported harnesses** | Claude Code, Codex, Kimi Code, Grok Build, DSH, Muse Code | Pi, OMP, Copilot, Devin, Kimi, Hermes, Qoder, Qwen, Droid, OpenCode, Kilo, MastraCode, Claude, Codex, Cursor, Amp, Grok, Antigravity, Kiro, Maki, Muse; any other CLI runs without agent-aware features | Claude, Codex, Copilot, OpenCode, Pi, OMP; catalog and custom ACP agents including Kimi, Cursor, Hermes, and Qwen | Codex, Claude, Cursor, Grok, OpenCode |
| **Multiple profiles per harness** | First-class named profiles | Manual wrappers and environment configuration | Custom provider aliases | Provider instances; continuation compatibility varies by harness |
| **Usage and quota view** | Live subscription quota by profile plus target capacity⁴ | — | Provider plan usage on demand | Token and API-cost analytics; not remaining subscription quota |

### Assistance and control

| Feature | Mjolnir | Herdr | Paseo | T3 Code |
|---|---|---|---|---|
| **Cross-session project memory** | Synchronized project memory shared across sessions, profiles, harnesses, and targets | — | — | — |
| **Automatic adversarial review** | Built-in automatic or on-demand independent review | Scriptable through agent automation; no built-in review loop | Manual `/paseo-advisor` second opinion | — |
| **Control surfaces** | TUI, web, desktop shell, CLI | TUI, CLI | Web, desktop, iOS, Android, CLI | Web, desktop, iOS, Android, CLI |
| **Voice input** | TUI and web dictation | — | Dictation and conversational voice mode | — |

### Extensibility

| Feature | Mjolnir | Herdr | Paseo | T3 Code |
|---|---|---|---|---|
| **Product plugins** | — | Workflow packages with actions, event hooks, terminal panes, and link handlers | Full-stack client/server plugins: UI surfaces, RPCs, tools, providers, themes, and commands | — |

“Native-session adoption” means discovering a session created outside the product
and bringing it under management. Ordinary same-harness continuation is excluded.

1. Mjolnir's SSH targets require a supported Linux host and the documented runtime
   prerequisites. It also supports Apple's container runtime on compatible Macs.
   See [targets](https://mjolnir.brokk.ai/targets/).
2. T3 continuation also requires compatible provider homes: Codex can share history
   across accounts using its shadow-home setup; separate Claude account homes
   cannot continue the same thread.
3. Bundles apply to managed targets. DSH (DeepSeek Harness) and Muse Code currently
   accept one workspace root. Bare sessions can access neighboring repositories
   subject to harness permissions, but do not manage them as a bundle.
4. Quota availability depends on the harness. Muse currently cannot use the
   project-memory tools or act as a reviewer. Cross-harness resume requires a
   configured utility-capable profile to generate the handoff; see
   [durability and recovery](https://mjolnir.brokk.ai/durability/).

## Get started

Install the release bundle on Linux or macOS (use WSL2 on Windows):

```sh
curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash
```

From your project directory, run:

```sh
mj
```

On first launch, Mjolnir creates a workspace from the current directory. If the
opened workspace has no live session, it starts one automatically using your
configured Codex account and prefers usable Podman, then Docker, then a local
directory target. This happens once while the dashboard opens; switching
workspace tabs only filters the list. Press **Create** for the full wizard, or
use `mj setup` to configure other harness accounts and targets. Run `mj doctor`
to check prerequisites.

Follow the [quickstart](https://mjolnir.brokk.ai/quickstart/) for your first
session. The [installation guide](https://mjolnir.brokk.ai/install/) covers npm,
source builds, desktop dependencies, and portable workers.

## Documentation

- [Profiles and harnesses](https://mjolnir.brokk.ai/profiles/): accounts, login,
  credentials, skills, and runtime prerequisites.
- [Targets](https://mjolnir.brokk.ai/targets/) and
  [bundles](https://mjolnir.brokk.ai/workspaces-bundles/): local, container, SSH,
  and EC2 environments; multi-repository projects and shared memory.
- [Session lifecycle](https://mjolnir.brokk.ai/sessions/) and
  [durability](https://mjolnir.brokk.ai/durability/): adoption, move, resume,
  checkpoints, and recovery.
- [Terminal](https://mjolnir.brokk.ai/terminal-surface/) and
  [web/desktop](https://mjolnir.brokk.ai/web-viewer/): controls and remote access.
- [Adversarial review](https://mjolnir.brokk.ai/turn-review/),
  [configuration](https://mjolnir.brokk.ai/configuration/),
  [CLI reference](https://mjolnir.brokk.ai/cli-reference/), and
  [security boundaries](https://mjolnir.brokk.ai/security/).

The website source lives in [docs/](docs/README.md). For the previous product
generation, see [Mjolnir 1.x](https://github.com/BrokkAi/mjolnir/releases/tag/v1.17.0).

## License

Mjolnir is licensed under `GPL-3.0-only`.
