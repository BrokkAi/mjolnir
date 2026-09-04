# Mjolnir documentation site

The public site is an Astro Starlight project. From this directory:

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

`PODMAN.md`, `DOCKER.md`, `SSH.md`, and `AWS.md` are the canonical versions of
their target guides. The `predev`, `precheck`, and `prebuild` hooks copy them into
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
