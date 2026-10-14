# Launch Codex sessions with Codex profile files

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Codex 0.160 reads "profile files": `codex -p ds` layers `$CODEX_HOME/ds.config.toml` over `$CODEX_HOME/config.toml`. A person who keeps, for example, a DeepSeek provider in `ds.config.toml` could not use it from Mjolnir, because every Mjolnir session ran Codex on `config.toml` alone. After this change, each such file is a Mjolnir profile of its own (`codex-ds`), detected automatically when `mj` launches, and the person picks it when creating a session, in the Create dialog's Profile step or with `mj new --profile codex-ds`. The session runs Codex with the file layered in, and Mjolnir's own view of the provider (authentication, model catalog, quota, default model) follows the layered configuration.

## Progress

- [x] (2026-10-14 12:40Z) Confirm how Codex treats profile files and whether the bridge can pass `-p`.
- [x] (2026-10-14 13:00Z) Add `codex_profile` to `HarnessProfile`, the single merged-configuration reader, staging, provider-auth polling, detection, Settings labels.
- [x] (2026-10-14 13:05Z) Tests: staging layers the overlay and refuses a missing one; detection offers overlays once.
- [x] (2026-10-14 13:08Z) Isolated-instance check with the real pinned Codex (see Artifacts).
- [x] (2026-10-14 13:15Z) User documentation in `docs/src/content/docs/profiles.md`.
- [x] (2026-10-14 14:10Z) Full workspace clippy and touched-crate test suites.
- [x] (2026-10-14 14:20Z) Review fix: the provider-auth poller caches layering failures by the raw files.

## Surprises & Discoveries

- Observation: `codex app-server`, which `@brokkai/codex-acp` spawns with no arguments, refuses `--profile`, and the legacy `profile = "<name>"` key is rejected outright, so no flag or `-c` override can select a profile file through the bridge.
  Evidence: `codex -p ds app-server` prints "--profile only applies to runtime commands and `codex mcp`"; `codex app-server -c profile=ds` prints "legacy `profile = \"ds\"` config is no longer supported; use `--profile ds` with `ds.config.toml` instead". `ThreadStartParams` in the generated app-server schema has no profile field.
- Observation: Codex merges a profile file into `config.toml` table by table at every depth; a scalar or array in the file replaces the base value.
  Evidence: with `[mcp_servers.a]` in `config.toml` and `[mcp_servers.b]` in `ds.config.toml`, `codex -p ds mcp list` lists both `a` and `b`.
- Observation: Layering inside the poller's read step meant a missing overlay or a parse error was never cached, so the 500 ms poller warned on every tick.
  Evidence: review finding; fixed by splitting `ConfigFiles::read` (raw texts, the cache key) from `ConfigFiles::layer`.
- Observation: Codex silently runs without a missing profile file (`codex -p nope mcp list` succeeds), and accepts only names of ASCII letters, digits, `-` and `_` (`a.b`, `../x`, `a b` and the empty name are refused).

## Decision Log

- Decision: Model a Codex profile file as a property of a Mjolnir profile (`codex_profile = "ds"` on `[profiles.<id>]`), not as a new per-session field.
  Rationale: A profile file usually changes `model_provider`, and every fact Mjolnir derives from Codex configuration (authentication scheme, API key, catalog, quota, default model, capability cache key) is keyed by Mjolnir profile. Making the overlay part of the profile keeps one owner for those facts. A per-session field would have had to be threaded through the database, HTTP API, CLI, TUI, web viewer and worker launch, and every profile-keyed reader would have had to learn a second key. The existing Profile step already is the "choose at launch" surface.
  Date/Author: 2026-10-14 / Claude.
- Decision: Mjolnir layers the file itself, in one function, `mj_core::codex_provider::effective_config`, and writes the result as the staged `config.toml`.
  Rationale: The bridge cannot pass `-p` (see Surprises). Provider parsing (`codex_provider`), `HarnessProfile::codex_config`, the provider-auth poller and staging all call this one function, so the session and Mjolnir cannot disagree about the configuration.
- Decision: A named but missing or unparsable profile file makes the profile unusable (`ensure_ready` refuses, staging fails) instead of falling back to `config.toml`.
  Rationale: Codex's silent fallback would run a session on a different provider than the one chosen.
- Decision: Detection adds `codex-<name>` for every valid `<name>.config.toml` in each detected Codex home; `HarnessProfile::same_installation` includes `codex_profile`, so the base home and each overlay are distinct and are added once.
- Decision: `ProfileDiscoveryInputs` carries `codex_profile` with `skip_serializing_if = "Option::is_none"`.
  Rationale: The overlay changes advertised models, so it must be in the capability cache key, and leaving it out when unset keeps the keys of every existing profile unchanged.

## Outcomes & Retrospective

Codex profile files work as Mjolnir profiles: detected on launch, chosen in the Profile step or with `mj new --profile`, layered into the staged `config.toml`, and read through one function everywhere Mjolnir interprets Codex configuration. Workspace clippy and the full mj-core, mj-controller, mj-tui, mj-cli and mj-chat suites pass; two Settings goldens were updated because profile pages list one more setting. Not verified: a real model turn on an overlay profile, which needs a live provider login; the isolated check reached Codex with the overlay's catalog and stopped at the same login refusal as a plain profile.

## Context and Orientation

A "profile" in Mjolnir is `[profiles.<id>]` in Mjolnir's `config.toml`, deserialized as `HarnessProfile` in `mj-core/src/config/harness.rs`; it names a harness `kind` and a `home` (for Codex, the directory Codex calls `CODEX_HOME`). A "staged home" is the per-session copy of that home that the worker runs the harness from; `stage_profile` in `mj-controller/src/controller/worker_binary/harness.rs` builds it for sessions, reviewers (`controller/reviewer.rs`) and model discovery probes (`controller/profile_config.rs`). `mj-core/src/codex_provider.rs` reads the provider out of a Codex configuration. `mj-controller/src/pollers/provider_auth.rs` re-reads it every 500 ms to decide whether login-file syncing applies. `mj-controller/src/setup.rs` detects installed harness homes on every plain `mj` launch, and `setup/startup.rs` adds the new ones to the configuration.

## Plan of Work

In `mj-core/src/codex_provider.rs`, add `OVERLAY_SUFFIX`, `is_overlay_name`, `overlay_names(home)`, `describe_config(home, overlay)`, `effective_config(home, overlay)` and a private `merge_tables`; change `codex_provider` to take the overlay and `parse_config` to take a display string. In `HarnessProfile`, add `codex_profile: Option<String>`, validate it (Codex only, valid name), include it in `discovery_inputs` and `same_installation`, add `codex_config()` and `describe_codex_config()`, and use the latter in readiness messages. In `stage_profile`, when `codex_profile` is set, write `codex_config()` over the copied `config.toml`. Key the provider-auth cache by `(home, overlay)` and read through `effective_config`. In setup, give `DiscoveredHome` a `codex_profile`, expand each Codex home into one entry per overlay, and name the added profile `codex-<name>`. In `mj-tui/src/setup/schema.rs`, add the Settings default, label, placeholder and help. Document the feature under "Codex profile files" in `docs/src/content/docs/profiles.md`.

## Concrete Steps

From the repository root:

    cargo test -p brokk-mj-controller --lib -- staging_a_codex_profile_overlay setup::
    cargo clippy --workspace --exclude brokk-mj-desktop --all-targets -- -D warnings
    cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir -p brokk-mj-chat

`brokk-mj-desktop` needs GTK development libraries; exclude it where they are not installed (it does not construct `HarnessProfile`).

## Validation and Acceptance

With `~/.codex/ds.config.toml` present, launching `mj` adds a `codex-ds` profile; creating a session on it runs Codex with the file layered in. `staging_a_codex_profile_overlay_layers_it_over_the_base_configuration` proves the merge, that the overlay's provider is the one whose catalog is fetched, that the source files are untouched, and that a missing file is refused. The extended `discovers_default_and_overridden_homes_with_authentication_markers` and `every_launch_adds_newly_installed_agents_and_keeps_disabled_profiles_off` prove detection and idempotence.

## Idempotence and Recovery

Detection is additive and idempotent. A person who does not want `codex-ds` sets `enabled = false` on it; deleting it lets the next launch add it again, as for any detected profile.

## Artifacts and Notes

Isolated instance (`--instance codexprof`, `MJ_CONFIG_DIR`/`MJ_DATA_DIR` under `/tmp`) with `codex-ds` whose `ds.config.toml` names a custom provider at a local fake `/v1/models` server: `mj models --profile codex-ds` fetched the catalog from the overlay's provider (the fake server logged `GET /v1/models`), then failed with codex-acp's "create ACP session: Authentication required". A control profile with the same provider written directly in `config.toml` failed identically, so the failure belongs to the fake provider, not the overlay.

## Interfaces and Dependencies

    // mj-core/src/codex_provider.rs
    pub const OVERLAY_SUFFIX: &str = ".config.toml";
    pub fn is_overlay_name(name: &str) -> bool;
    pub fn overlay_names(home: &Path) -> Vec<String>;
    pub fn describe_config(home: &Path, overlay: Option<&str>) -> String;
    pub struct ConfigFiles { pub base: Option<String>, pub overlay: Option<(String, Option<String>)> }
    impl ConfigFiles { pub fn read(home: &Path, overlay: Option<&str>) -> Result<Self>; pub fn layer(&self, home: &Path) -> Result<Option<String>>; }
    pub fn effective_config(home: &Path, overlay: Option<&str>) -> Result<Option<String>>;
    pub fn codex_provider(home: &Path, overlay: Option<&str>) -> Result<Option<CodexProvider>>;
    pub fn parse_config(text: &str, path: &str) -> Result<Option<CodexProvider>>;

    // mj-core/src/config/harness.rs, on HarnessProfile
    pub codex_profile: Option<String>;
    pub fn codex_config(&self) -> Result<Option<String>>;
    pub fn describe_codex_config(&self) -> String;
