# Mjolnir documentation site

The public site is an Astro Starlight project. Use a Node.js version supported
by the locked Astro dependency (CI uses Node.js 24). From this directory:

```sh
npm ci
npm run check
npm run build
PUBLIC_DOCS_BASE=/mjolnir npm run build
```

`npm run build` checks every local page and asset link after rendering. The
non-root build proves those links also work when the site is deployed below a
path prefix.

## Target guides

`mj-controller/docs/PODMAN.md` and `mj-controller/docs/DOCKER.md` at the repository
root are the canonical embedded runtime guides; this directory owns `SSH.md`
and `AWS.md`. The `predev`, `precheck`, and `prebuild` hooks copy them into
`src/content/docs/` with Starlight frontmatter. Edit the uppercase source,
not the generated lowercase page.

## Current UI screenshots

The screenshots in `src/assets/screenshots/` are SVG captures produced by the
real Ratatui dashboard renderer. Regenerate all of them after a relevant TUI
change from the repository root:

```sh
cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture
```

Review the resulting dashboard, new-session wizard, and command-palette images,
then run the site checks above. The generator is an ignored test so ordinary
test runs never rewrite documentation assets.

## Keeping documentation aligned

Update the relevant guide and reference together when behavior changes:

| Surface | Implementation to check | Documentation |
| --- | --- | --- |
| Commands and flags | `mj-cli/src/main.rs`, `mj-cli/src/api_commands.rs`, `mj-cli/src/acp.rs`; `mj --help` | CLI reference, ACP agent, README examples |
| Configuration fields and defaults | `mj-core/src/config.rs`, `mj-core/src/config/`, `mj-core/src/continuation.rs` | Configuration reference and affected task guide |
| Harness capabilities and launch policy | `mj-core/src/config/harness.rs`, `mj-core/src/harness_runtime.rs`, worker launch code | Profiles, targets, overview, security |
| Session and workspace lifecycle | `mj-controller/src/controller/`, `mj-controller/src/daemon/`, `mj-controller/src/server/api/` | Sessions, durability, workspaces, CLI and API references |
| Packaging and prerequisites | `install.sh`, `scripts/install.sh`, `npm/scripts/package-release.mjs`, Cargo manifests | Install, README, contributing guide |

Paths in that table are relative to the repository root. Treat the source as
the authority for shipped behavior; planning notes in `.agents/` may describe
unfinished work. Keep historical release notes and vendored material identified
as such. Prefer links to the relevant guide over repeating version pins and
capability lists across pages. The build checks links; it cannot verify that a
documented default or lifecycle claim matches the code.
