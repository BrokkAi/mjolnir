# Rebuild the Mjolnir 2.0 documentation site

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. This plan follows `.agents/PLANS.md` from the repository root.

## Purpose / Big Picture

The Mjolnir 2.0 documentation site currently sends readers back to the README for nearly everything except container targets. A reader should instead be able to install Mjolnir, understand the terminal and web surfaces, configure every supported profile, bundle, target, review, and web-viewer option, operate durable sessions, and recover from failures without leaving the site. The site should regain roughly the breadth and visual confidence of the v1.17.0 site while documenting only the current 2.0 control plane. Current product screenshots should be generated from the real terminal renderer so they can be refreshed as the UI changes.

## Progress

- [x] (2026-09-04 14:20Z) Read `.agents/PLANS.md`, inspected the current documentation build, and compared the current tree with tag `v1.17.0`.
- [x] (2026-09-04 14:30Z) Located the current configuration schema, CLI declarations, README product guide, terminal renderer fixtures, target guides, and web-viewer tests that serve as sources of truth.
- [x] (2026-09-04 14:42Z) Defined and wired a six-section Starlight information architecture spanning 22 documentation pages.
- [x] (2026-09-04 14:50Z) Wrote the getting-started, concepts, operation, target, configuration, and reference pages against the current 2.0 code and canonical target guides.
- [x] (2026-09-04 14:53Z) Added deterministic current-UI screenshot generation and embedded three production-rendered SVG captures in the landing page and guides.
- [x] (2026-09-04 15:06Z) Validated both deployment bases, 1,673 internal links, desktop and phone layouts, deterministic screenshots, formatting, the full Rust workspace test suite, and all-target Clippy.
- [x] (2026-09-04 15:07Z) Updated this plan with final evidence and prepared only the documentation-rebuild files for commit.

## Surprises & Discoveries

- Observation: The v1.17.0 site contained 18 rendered guide/reference pages, five README screenshots, and 25 workflow screenshots; the 2.0 cutover deliberately deleted all of them and retained only a four-page container section.
  Evidence: `git diff --name-status v1.17.0..HEAD -- docs` reports 18 deleted content pages and all screenshot directories, while the current sidebar has only four entries.
- Observation: The current README is already a compact, accurate 2.0 operator guide, but the full configuration contract lives in `src/hel_config.rs` and the detailed target postconditions live in `docs/PODMAN.md`, `docs/DOCKER.md`, `docs/SSH.md`, and `docs/AWS.md`.
  Evidence: `README.md` has sections for install, quickstart, terminal operation, security, and durability; `HelConfig` is composed of `PhoneConfig`, `ReviewConfig`, profiles, bundles, and seven target variants.
- Observation: Mjolnir already renders realistic terminal states into Ratatui `TestBackend` buffers for behavior tests. Those buffers can be serialized to SVG without adding a browser or screenshot-only UI implementation.
  Evidence: `mj-tui/src/test_support.rs` builds populated dashboard/session fixtures and tests under `mj-tui/src/` render them through the production `render` function.
- Observation: A caller-provided `RUST_LOG=warn` changes the default-filter behavior asserted by the logging integration test, although the product and test are correct in an ordinary environment.
  Evidence: The full suite's only initial failure was `logging::respects_mj_log_over_rust_log`; `env -u RUST_LOG cargo test -p brokk-mjolnir --test logging -- --nocapture` and the subsequent full workspace run both passed.
- Observation: Root-relative links embedded in a custom landing component bypass Starlight's deployment-base rewriting.
  Evidence: Building once with `PUBLIC_DOCS_BASE=/mjolnir` exposed the issue; routing the hero links through Astro's configured base made the same 1,673-link check pass at both `/` and `/mjolnir`.

## Decision Log

- Decision: Use the v1.17.0 site as an information-architecture and visual-density benchmark, not as content to restore verbatim.
  Rationale: Mjolnir 2.0 replaces the 1.x interactive client, so pages about teams, delegated subagents, voice, and the old remote client would be actively misleading.
  Date/Author: 2026-09-04 / Codex
- Decision: Make `src/hel_config.rs`, Clap declarations, and the four target guides authoritative; use `README.md` for user-facing explanations and terminology.
  Rationale: This keeps the detailed reference aligned with the accepted schema and executable interface rather than duplicating an older prose contract.
  Date/Author: 2026-09-04 / Codex
- Decision: Generate terminal screenshots as SVG files from the production Ratatui renderer under an ignored, explicitly invoked test.
  Rationale: The result is a real current Mjolnir frame, is deterministic and reviewable, needs no display server, and can be regenerated when the UI changes.
  Date/Author: 2026-09-04 / Codex
- Decision: Synchronize all four long-form target guides—Podman, Docker, SSH, and AWS—into Starlight and give each generated page a canonical source edit URL.
  Rationale: Target setup remains useful as repository-level Markdown while the site gains complete target coverage without maintaining two prose copies.
  Date/Author: 2026-09-04 / Codex
- Decision: Keep the landing-page install choices to the release installer, npm, npx, and archive routes.
  Rationale: Those routes install the portable worker required by isolated and remote targets; a native Cargo install alone installs only the controller and would make the primary quickstart path incomplete.
  Date/Author: 2026-09-04 / Codex

## Outcomes & Retrospective

The documentation site now renders 24 HTML pages (22 guides plus the landing page and 404), exceeding the 18-page v1.17 guide/reference benchmark while describing only Mjolnir 2.0. The sidebar groups Get started, Core concepts, Operate, Targets, Configure, and Reference. The configuration reference covers every accepted schema field and default, and the CLI, target, durability, security, review, web-viewer, import, and recovery guides are grounded in the current source and behavior tests.

Three committed SVG screenshots—dashboard, new session, and command palette—come from the production Ratatui renderer. Two consecutive generations produced identical SHA-256 hashes: `680bc92a0d56f152436d4666b82de5837ca3dba0e2ec17d3e834a3a5f987ce35`, `2d573be7c4f0418636a8d89ca9e771f98a6d1014c21a475d75c9d8885ece76d4`, and `d0a571e0f5fd73a94c60d8653917b1acc610e2b4f1bd80ab2d82c429a1d81293`, respectively.

`npm run check` reported zero errors, warnings, or hints. Both `npm run build` and `PUBLIC_DOCS_BASE=/mjolnir npm run build` rendered 24 pages and checked 1,673 internal links. Browser inspection covered the home page at 1400×900 and the home and configuration pages at 390×844, plus direct inspection of the generated workflow captures. `cargo fmt --all -- --check`, `git diff --check`, the focused generator test, `env -u RUST_LOG cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` all exited successfully.

## Context and Orientation

The site is an Astro Starlight project under `docs/`. `docs/astro.config.mjs` owns global metadata and the explicit sidebar; `docs/src/content/docs/` contains rendered Markdown and MDX pages; `docs/src/components/MjolnirHeader.astro` owns the desktop header links; and `docs/src/styles/mjolnir.css` owns the product theme and landing-page layouts. `npm run build` from `docs/` first mirrors the canonical Podman and Docker guides into the content tree, builds Astro, and runs `docs/scripts/check-links.mjs` over every local link and asset.

The 2.0 configuration schema is `HelConfig` in `src/hel_config.rs`. A profile names one of five coding harnesses and its canonical local home. A bundle describes one or more repositories for isolated targets. A target describes where a session runs: a local bare worktree, local Podman, local Docker, Apple container, raw SSH machine, Podman over SSH, or EC2 instance. `PhoneConfig` controls the personal web viewer and `ReviewConfig` controls automatic or one-off independent turn review.

The terminal UI is implemented in `mj-tui/`. `DashboardState` is the renderable state model, `render` is the production frame renderer, and the colocated test support already creates representative profiles, bundles, targets, sessions, transcripts, quota reports, and capacity rows. An ignored documentation screenshot test can use those private fixtures without expanding the library's public API.

## Plan of Work

First replace the four-link sidebar and header with a complete hierarchy covering getting started, core concepts, operation, targets, configuration, and reference. Rewrite the landing page into a real product entry point with paths for a first session, isolated targets, durable operation, and remote control. Preserve the existing visual system and add only the figure/card styling needed by screenshots and guide navigation.

Next add focused pages for installation, quickstart, the terminal surface, workspaces and bundles, profiles and harnesses, session lifecycle, review, web/desktop access, configuration reference, CLI reference, security, durability/recovery, and troubleshooting. Keep the existing container pages and generated Podman/Docker pages, and render the canonical SSH and AWS guides through the same synchronization mechanism so the site does not maintain divergent copies. Each guide must link forward to a useful next task and cross-link related schema/reference material.

Then add `mj-tui/src/docs_screenshots.rs` behind `#[cfg(test)]` and declare it from `mj-tui/src/lib.rs`. It will build representative dashboard states with the existing test support, render the normal dashboard and key workflows through the real renderer, serialize Ratatui cells and their styles to accessible SVG, and write the committed files under `docs/src/assets/screenshots/` only when the ignored test is run explicitly. Embed those assets in the quickstart and terminal guides with meaningful alternative text and captions.

Finally run the screenshot generator, Astro type and build checks, base-path link checks through the production build, Rust formatting, focused screenshot test, full workspace tests, and all-target Clippy. Inspect the built home and representative guide pages at desktop and phone widths. Record exact validation output here, update the retrospective, and commit only this plan, the docs tree, and screenshot-generator changes.

## Concrete Steps

Run all commands from `/Users/ryansvihla/.codex/worktrees/ac54/mjolnir` unless noted otherwise.

Inspect the historical and current surfaces:

    git ls-tree -r --name-only v1.17.0 -- docs
    git diff --name-status v1.17.0..HEAD -- docs
    rg '^pub struct|^pub enum' src/hel_config.rs
    sed -n '40,310p' mj-cli/src/main.rs

After implementing screenshot generation, run its ignored test outside the restricted sandbox because every Cargo test in this repository requires loopback and Unix-socket access:

    cargo test -p brokk-mj-tui generate_documentation_screenshots -- --ignored --nocapture

Validate the documentation project:

    cd docs
    npm run check
    npm run build
    PUBLIC_DOCS_BASE=/mjolnir npm run build

Validate the repository:

    cargo fmt --all -- --check
    git diff --check
    env -u RUST_LOG cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings

Expected documentation build output ends with a successful Starlight build and `Checked ... internal docs links across ... HTML files`. The base-path build must report `(base: /mjolnir)` and no omitted-base errors. Cargo tests and Clippy must exit zero.

## Validation and Acceptance

The work is accepted when the site home no longer describes itself as a container-only placeholder; its sidebar exposes a complete 2.0 guide and reference hierarchy; a new user can follow install and quickstart through starting and detaching a session; an operator can find every accepted `config.toml` field and every public CLI command; each supported target has setup guidance; session durability, security boundaries, web access, review, import, and recovery are explained; and at least three screenshots show the current terminal dashboard and workflows.

The committed screenshots must be reproducible by the explicit ignored test and must visibly contain current 2.0 concepts such as the Sessions, Prompt, Targets, and Quota panes rather than 1.x teams or reviewer-seat UI. `npm run build` must prove every internal guide and image link resolves both at `/` and at a non-root deployment base.

## Idempotence and Recovery

The page edits and sidebar changes are ordinary source changes and can be rebuilt repeatedly. The screenshot generator writes complete SVG strings to a fixed set of paths deterministically; rerunning it should leave `git status` unchanged. The generated Podman, Docker, SSH, and AWS content pages are overwritten from their canonical uppercase source guides during each documentation prebuild, so edits to those generated lowercase pages would be lost and must instead be made in their canonical sources or synchronization metadata. If screenshot generation is interrupted, rerun the same ignored test to replace the fixed outputs.

## Artifacts and Notes

The historical benchmark is tag `v1.17.0`. Its sidebar grouped Get started, Workflows, Extend Mjolnir, and Reference. The rebuild intentionally uses different groups that match the 2.0 product: Get started, Concepts, Operate, Targets, Configure, and Reference.

## Interfaces and Dependencies

Do not add a runtime dependency to the Mjolnir product for screenshots. The generator uses `ratatui::backend::TestBackend`, already available to `mj-tui`, plus the existing `DashboardState`, render function, and test fixtures. It exposes no new public Rust interface. The generated SVG contract is a fixed viewport, accessible `<title>` and `<desc>`, a dark terminal background, and positioned monospace cells carrying the foreground, background, and modifier styles supplied by Ratatui.

The documentation remains on the existing Astro 7 and Starlight 0.41 dependencies. Canonical target-guide synchronization remains a Node script invoked by `predev` and `prebuild`; extend the current data list rather than create another generator.

Plan revision note (2026-09-04 14:30Z): Created the plan after comparing v1.17.0 with the current four-page site and locating the current 2.0 schema, CLI, renderer fixtures, and target guides.

Plan revision note (2026-09-04 15:07Z): Recorded the completed 24-page implementation, canonical target-guide synchronization, reproducible screenshot hashes, dual-base link/build evidence, visual review, and full repository validation.
