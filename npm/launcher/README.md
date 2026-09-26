# Mjolnir

Mjolnir runs Codex, Claude Code, Kimi Code, Grok Build, and Muse Code sessions
across local checkouts, containers, SSH hosts, and EC2, with terminal, web, and
desktop controls.

```sh
npm install -g @brokkai/mjolnir
mj
```

For a one-shot run, use `npx -y @brokkai/mjolnir`. The package installs the
native release bundle for the current platform; it does not download Mjolnir at
first launch. It requires Node.js 18+ on macOS or glibc-based Linux (x86_64 or
ARM64); Windows users can run it in WSL2. See the
[installation guide](https://mjolnir.brokk.ai/install/) for supported platforms,
updates, and alternatives.
