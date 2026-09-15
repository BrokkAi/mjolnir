# Run GLM through Codex on the Z.ai Coding Plan and remove the ZCode harness

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` at the repository root and must be maintained in accordance with that file.

## Purpose / Big Picture

Today Mjolnir reaches Z.ai's GLM models only through a dedicated `zcode` harness: a patched `zcode-acp-server` adapter driving a headless Node runtime extracted from the ZCode desktop AppImage. That harness costs a second adapter fork, an AppImage download pipeline with per-architecture checksums, a container image layer, a shared native SQLite store that defeats checkpoints and native import, and a long tail of special cases in the worker and controller.

After this change, a user configures an ordinary `codex` profile whose Codex configuration names Z.ai as its model provider and whose environment carries the Coding Plan API key. Mjolnir launches it through the same `codex-acp` bridge every Codex profile already uses, the session shows `glm-5.3` and `glm-5.3-flash` as its models, guardian mode works on raw targets, quota reporting shows the Coding Plan allowance, and no ZCode code, assets, or image layers remain in the repository.

The user-visible proof is: add the profile shown in `Concrete Steps`, run `mj doctor` and see the profile reported as authenticated, start a session on it, pick `glm-5.3-flash`, ask it to create a file, and see the Coding Plan quota in the dashboard.

## Progress

- [x] (2026-09-15 14:40Z) Probed Codex 0.154.0 against Z.ai outside Mjolnir: Responses endpoint, `env_key`, both GLM models, tool calls, and guardian review all work. Evidence in `Surprises & Discoveries`.
- [x] (2026-09-15 15:05Z) Confirmed codex-acp advertises exactly the models in `models.json`, and that a relative `model_catalog_json` resolves against `CODEX_HOME`.
- [x] (2026-09-15 15:40Z) Confirmed Z.ai `GET /api/v1/models` returns a Codex-format catalog (`glm-5.3`, `glm-5.3-flash`, `glm-5-turbo`) and that Codex reads the reviewer model from the catalog field `auto_review_model_override`.
- [x] (2026-09-15 17:20Z) Milestone 1: provider descriptor and per-profile capabilities in `mj-core`. Added `mj-core/src/codex_provider.rs`, `mj-core/src/codex_catalog.rs`, `HarnessProfile::{codex_provider, auth_scheme, authentication_marker, credential_freshness, credential_expiry, supports_guardian_approvals}`, the `AuthScheme` enum, provider validation, and `login_command` returning `Result`.
- [x] (2026-09-15 18:40Z) Milestone 2: controller, worker, and CLI consumers use the per-profile capabilities. Auth gate takes the profile; `QuotaRefreshRequest::for_profile` resolves the provider and key and routes Z.ai hosts to the renamed `mj-controller/src/zai_usage.rs`; credential sync skips the file exchange for an API-key profile; `WorkerLaunchConfig::authentication_marker` carries the marker name to the worker; API-key profiles are excluded from utility duty; staging fetches, stamps, and installs `models.json` with a cached fallback.
- [ ] Milestone 3: remove the ZCode harness from code, assets, container image, and docs.
- [ ] Milestone 4: documentation (in scope) and live validation on the user's install (out of scope for the implementing agent; the real Z.ai key is the user's, so every behavioural step in `Validation and Acceptance` remains unperformed and must be run by the user).

## Surprises & Discoveries

- Observation: Codex no longer accepts `wire_api = "chat"`. Only the Responses API is supported, and Z.ai serves it at `https://api.z.ai/api/v1` for Coding Plan keys.
  Evidence: `codex exec` printed `Error loading config.toml: wire_api = "chat" is no longer supported. How to fix: set wire_api = "responses"`. With `base_url = "https://api.z.ai/api/v1"` and `wire_api = "responses"` the same command replied `pong` using 3,382 tokens.

- Observation: The Coding Plan key is a plain long-lived bearer key stored in `~/.zcode/v2/config.json` under `provider["builtin:zai-coding-plan"].options.apiKey`. Codex's standard `env_key` mechanism accepts it, so the key never has to be written into the staged `config.toml`.
  Evidence: `codex exec` succeeded with `env_key = "ZAI_API_KEY"` and the key exported only in the process environment.

- Observation: The Coding Plan serves `glm-5.3-flash` through the Responses endpoint even though Z.ai's Codex documentation lists only `glm-5.3`.
  Evidence: `codex exec -m glm-5.3-flash "Reply with exactly the word pong"` replied `pong` using 5,313 tokens.

- Observation: Tool calls work. GLM ran `printf 'hi' > hello.txt` through Codex's shell tool in the workspace-write sandbox and the file appeared.

- Observation: codex-acp advertises the model list from Codex's `model_catalog_json` file. With the catalog present the `model` config option lists only `glm-5.3` and `glm-5.3-flash`; without it the option lists OpenAI's built-in models, which would let a user pick `gpt-5.5` and send that name to Z.ai. A relative `model_catalog_json = "models.json"` resolves against `CODEX_HOME`, so the staged copy of the profile home works unchanged on any target.
  Evidence: three `session/new` probes printed `model current= glm-5.3 options= ['glm-5.3', 'glm-5.3-flash']` for absolute and relative catalog paths, and `options= ['glm-5.3', 'gpt-6-astra', 'gpt-5.6-sol', ...]` with no catalog.

- Observation: Codex's guardian mode does function with GLM. In the `agent` mode (the mode Mjolnir selects for configured approvals) Codex runs a separate reviewer thread called "Guardian V2" for each action that leaves the sandbox. That reviewer thread used `model=glm-5.3` at `reasoning_effort=low` and returned `{"outcome":"allow"}` for an explicitly requested write outside the workspace. Whether GLM's safety judgement is as good as OpenAI's is not something Mjolnir can measure; the mechanism itself runs.
  Evidence: the ACP stream showed two `tool_call "Guardian Review"` updates; the reviewer rollout `rollout-...-01a0a584-8ff2-...jsonl` recorded nine `"model":"glm-5.3"` entries and two `task_complete` events with `last_agent_message: {"outcome":"allow"}`; the Codex log showed the reviewer turn tagged `model=glm-5.3 codex.turn.reasoning_effort=low`. Codex also emitted one `guardianWarning` app-server event whose payload codex-acp discards, so its content is unknown.

- Observation: Z.ai serves a Codex-format catalog at `GET https://api.z.ai/api/v1/models` for the Coding Plan key. The response is `{"models":[...]}` with the same 21 fields per entry Codex's `model_catalog_json` loader expects. Codex itself will not fetch it for an `env_key` provider.
  Evidence: the probe printed `top-level keys: ['models']` and slugs `glm-5.3`, `glm-5.3-flash` (efforts low/high/max) and `glm-5-turbo` (no efforts). `should_refresh_models` in `codex-rs/models-manager/src/manager.rs` returns true only for `uses_codex_backend()` or `has_command_auth()`.

- Observation: The Guardian reviewer model comes from the parent model's catalog entry. `select_review_model` uses `parent_model.auto_review_model_override`, else the provider's default reviewer slug if it is in the catalog, else the parent model itself. It prefers `low` effort when the reviewer supports it.
  Evidence: `codex-rs/ext/guardian-reviewer/src/model.rs` lines 31 to 66; the probe's reviewer thread ran on `glm-5.3` because no override was set and OpenAI's default reviewer slug is not in the Z.ai catalog.

- Observation: Codex's workspace-write sandbox allows writes under `/tmp`, so the first escalation probe under the session scratchpad was not an escalation. The valid probe wrote under the user's home directory.

- Observation: The catalog and guardian probe was rerun on the exact pinned stack, Codex 0.153.4 (the `@openai/codex` dependency of `@brokkai/codex-acp` 1.11.4, selected with `CODEX_PATH`) against an unmodified codex-acp 1.11.4. The ACP `model` config option listed only `glm-5.3` and `glm-5.3-flash`; `session/set_mode` to `agent` succeeded; an escalated write under the user's home produced a "Guardian Review" tool call; and with `auto_review_model_override = "glm-5.3-flash"` stamped on every catalog entry, the session rollout recorded only `"model":"glm-5.3"` while the reviewer thread's rollout recorded only `"model":"glm-5.3-flash"`. No change to the Codex fork or the codex-acp fork is required: every feature this plan uses is stock Codex configuration present in 0.153.4.

- Observation: `model_catalog_json` is a top-level key in Codex's `config.toml`, not a key inside the `[model_providers.<id>]` table. Appending it to the end of a staged file would therefore land it inside whatever table comes last and Codex would ignore it. Staging prepends the line instead, which is valid TOML because top-level keys must precede the first table header, and leaves the rest of the user's file byte-identical.

- Observation: On a local bare target a Codex session runs directly from the user's own profile home (`target_profile_home` in `mj-controller/src/controller.rs` returns `profile.home` for `LocalBare`, and `prepare_worker_files` skips staging entirely). A generated catalog would therefore either be missing or written into the user's home. Staging is now forced for any profile with a custom provider, through `requires_private_profile_home`, and the local bare home becomes `<worker_root>/profile` as it already did for Claude.
  Evidence: the test `a_custom_provider_session_carries_its_key_and_runs_from_a_private_home` asserts `CODEX_HOME` is `/home/me/.local/share/hel/worker/profile`, and `staging_a_custom_provider_profile_writes_a_catalog_the_session_can_pick_from` asserts the user's home keeps no `models.json`.

- Observation: The catalog cache reuses the existing `profile_config_cache` table, whose rows are treated as stale after 24 hours (`load_profile_config_cache_from` in `mj-controller/src/database.rs`). A provider outage longer than a day therefore fails the launch rather than staging a very old catalog. That is the table's existing behaviour and this plan does not change it.

- Observation: The Coding Plan key is rejected by the chat-completions path under `https://api.z.ai/api/v1` but accepted under `https://api.z.ai/api/coding/paas/v4`. This matters only for Mjolnir's utility model (anvil's client speaks chat completions and appends `/v1` to its base URL), not for Codex.
  Evidence: `POST /api/v1/chat/completions` returned `403 model_access_denied` for `glm-5.3`; `POST /api/coding/paas/v4/chat/completions` returned 200 `pong`.

## Decision Log

- Decision: Represent a GLM-backed Codex profile as `kind = "codex"` rather than a new `HarnessKind` variant.
  Rationale: Codex treats its `config.toml` as the source of provider identity, and Mjolnir already copies that file verbatim into the staged home. A new variant would ripple through every exhaustive match, the TUI dropdown, the import CLI, and require a breaking SQLite migration, all to encode something the config file already states.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Derive the profile's provider from the profile home's `config.toml` (`model_provider` and the matching `model_providers.<id>` table) instead of adding a new profile field.
  Rationale: One source of truth. The same parse yields the API-key environment variable name and the base URL that quota reporting needs.
  Date/Author: 2026-09-15, Claude.

- Decision: The API key lives in the profile's `environment` table in Mjolnir's `config.toml`, under the name the provider's `env_key` declares. Mjolnir never reads the key out of the ZCode configuration.
  Rationale: The user pastes it once. Reading ZCode's config would keep a dependency on a format this plan deletes.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Keep guardian support enabled for Codex profiles regardless of provider, and run the Guardian reviewer on the newest flash model the provider lists.
  Rationale: The probe showed Codex's Guardian V2 reviewer running on GLM and producing outcomes. Codex picks the reviewer from the parent model's catalog entry field `auto_review_model_override` (see `select_review_model` in the Codex source, `codex-rs/ext/guardian-reviewer/src/model.rs`); without it the reviewer falls back to the parent model, so a heavyweight session model would also review itself. Mjolnir generates the catalog, so it stamps the override on every entry. The user chose the newest flash model rather than a fixed slug so the choice tracks Z.ai's releases.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Exclude API-key Codex profiles from utility-model duty in this plan.
  Rationale: The utility path uses anvil's chat-completions client, whose URL joining cannot reach the Coding Plan's chat endpoint without an anvil change and a version bump. The user's OpenAI Codex profiles continue to serve as utility models. Recorded as an optional follow-up in `Outcomes & Retrospective`.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Keep `'zcode'` as a tolerated value in the `harness_kind` CHECK constraint and do not add a migration.
  Rationale: Removing the value would require another breaking migration and would delete or rewrite the user's existing ZCode session rows. Leaving the value makes old rows readable; the code path behind them is gone, so such sessions cannot be resumed, and the controller reports that plainly.
  Date/Author: 2026-09-15, user and Claude.

- Decision: A Mjolnir config that still contains `kind = "zcode"` fails to load with the existing "unknown harness kind" error.
  Rationale: Silently skipping unknown kinds would hide typos in every other profile. The user removes the stale block once; the deployment step in Milestone 4 says so.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Mjolnir generates the Codex model catalog for a custom-provider profile by fetching `{base_url}/models` with the profile's key, and stages it as `models.json` with `model_catalog_json = "models.json"` written into the staged `config.toml`.
  Rationale: Codex's own remote catalog fetch (`should_refresh_models` in `codex-rs/models-manager/src/manager.rs`) runs only for ChatGPT or command auth, and for other auth it merges the remote list into the bundled OpenAI list. Without a static catalog codex-acp therefore advertises OpenAI model names that would be sent to Z.ai. Z.ai's `/models` endpoint already returns the Codex catalog format, so Mjolnir needs no translation, only the reviewer override stamp. The last successful catalog is cached in the existing `profile_config_cache` table so a provider outage does not block launches.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Staging writes `model_catalog_json = "models.json"` as the first line of the staged `config.toml` rather than appending it, and `codex_provider` reads the key at the top level.
  Rationale: The key is top-level in Codex's configuration. A bare key appended after a `[model_providers.zai]` header would become `model_providers.zai.model_catalog_json`, which Codex ignores, so the profile would silently advertise OpenAI models. Prepending is the only placement that both keeps the key top-level and leaves the user's own lines untouched.
  Date/Author: 2026-09-15, Claude.

- Decision: Keep a `native_login_command` function beside the new `login_command` that returns `Result`.
  Rationale: Setup discovery in `mj-controller/src/setup.rs` calls the login command only to learn which program to run `--version` against, before any profile exists. "This profile needs no login" is not a useful answer there, and an error would make discovery skip an installed Codex. `login_command` is the gate; `native_login_command` is the lookup.
  Date/Author: 2026-09-15, Claude.

- Decision: A profile with a custom provider always runs from a private staged home, even on a local bare target.
  Rationale: Mjolnir generates `models.json` and prepends `model_catalog_json` to `config.toml` for each launch. Writing either into the user's own profile home would edit a file the user owns and would leak one session's catalog into every other tool using that home. The controller already had this exception for Claude, so the change is one predicate, `requires_private_profile_home`.
  Date/Author: 2026-09-15, Claude.

- Decision: The catalog fetch and the catalog cache are both injected into `stage_codex_catalog`, as a `CatalogFetch` function and a `CatalogCache` trait.
  Rationale: Tests cannot reach Z.ai, and the live cache is a process-global store. The production implementations are `fetch_catalog_over_https` (the same bounded client shape the quota reader uses) and `SharedCatalogCache` (the `profile_config_cache` table). Tests supply a fake body and an isolated SQLite store that still uses the real table and schema, so the fallback path is proven against the real store rather than a stub.
  Date/Author: 2026-09-15, Claude.

- Decision: Exclusion from utility duty is decided by `profile_serves_as_utility(profile)` rather than by making `utility_precedence` take a profile.
  Rationale: `utility_precedence` ranks candidates by harness kind inside `candidate_order`, where only the kind is available. One profile-aware predicate at the two places that select a profile keeps the ranking logic unchanged.
  Date/Author: 2026-09-15, Claude.

## Outcomes & Retrospective

To be written at completion. Optional follow-up not in this plan: codex-acp discards Codex's `guardianWarning` app-server event, so surfacing the reviewer's reasoning in the transcript would be a small bridge-only change. A second optional follow-up: let API-key Codex profiles serve as utility models by giving anvil's `OpenAiClient` a constructor that uses its base URL verbatim, publishing that anvil-client patch, bumping the `brokk-anvil-client` pin in the workspace `Cargo.toml`, and adding a `backend_for_profile` arm that builds the client from the provider's chat base URL (`https://api.z.ai/api/coding/paas/v4` for Z.ai) and the key from the profile environment.

## Context and Orientation

Mjolnir (crates prefixed `mj-`, workspace root `/home/jonathan/Projects/hel`) runs coding agents ("harnesses") through the Agent Client Protocol (ACP), a JSON-RPC protocol over stdin/stdout. For each harness kind a bridge process translates ACP into the agent's native protocol. For Codex the bridge is the npm package `@brokkai/codex-acp`, pinned in `mj-core/src/harness_runtime.rs`, which in turn drives the `codex` CLI.

A profile is one harness kind plus a source home directory and an environment map. The type is `HarnessProfile` in `mj-core/src/config.rs` (fields `enabled`, `kind`, `home`, `environment`, `context_window_bytes`). Profiles are stored in Mjolnir's own `config.toml` under `[profiles.<id>]`; the user's file is `/home/jonathan/.config/mjolnir/config.toml`. A profile deliberately holds no model or provider; those live in the harness's own configuration inside `home`.

Before a session starts, the controller copies an allowlisted subset of the profile home into a worker-private directory on the target ("staging"; see `stage_profile` in `mj-controller/src/controller/worker_binary.rs`). For Codex the allowlist is `auth.json`, `config.toml`, `AGENTS.md`, `instructions.md`, `rules`, and `skills`. The worker then launches the bridge with the harness's home environment variable (`CODEX_HOME` for Codex) pointing at the staged copy, and merges the profile's `environment` map into the process environment (`worker_binary.rs`, the launch environment composition around line 459).

Codex's own configuration file, `config.toml` inside `CODEX_HOME`, can name a custom model provider. The relevant keys are `model_provider = "<id>"`, a table `[model_providers.<id>]` with `base_url`, `wire_api = "responses"`, and either `env_key = "<ENV VAR>"` (the key is read from that environment variable) or `experimental_bearer_token = "<key>"` (the key is inline), plus `model_catalog_json = "<path>"` naming a JSON file that lists the models to advertise. Mjolnir has never parsed any of these; it copies the file as-is.

Everywhere Mjolnir needs to know how a Codex profile authenticates, it currently assumes ChatGPT OAuth with tokens in `auth.json`:

- `harness_authentication_marker` in `mj-core/src/config.rs` returns `auth.json` for Codex. It is read by `harness_is_authenticated_with` in `mj-controller/src/setup.rs` (discovery), `mj-controller/src/doctor.rs` (health report), `refresh_profile` in `mj-controller/src/quota.rs`, `reconcile_session` in `mj-controller/src/worker_client.rs` (credential sync into running sessions), `credential_endpoint` in `mj-worker/src/worker_runtime.rs`, and the `mj login` command in `mj-cli/src/main.rs`.
- `credential_freshness` and `credential_expiry` in `mj-core/src/credentials.rs` parse `last_refresh` and a JWT expiry out of `auth.json` for Codex.
- `login_command` in the same file returns `codex login`, which starts OAuth.
- `refresh_profile` in `mj-controller/src/quota.rs` refreshes the ChatGPT login and reads ChatGPT rate-limit windows for every Codex profile.
- `backend_for_profile` in `mj-controller/src/utility_llm.rs` builds a `CodexClient` from `auth.json`, and `family_matches` only accepts `gpt-*` model names for Codex. The utility model is the backend Mjolnir uses for its own inference, such as compacting a transcript for a resume handoff.

The ZCode harness is `HarnessKind::Zcode` in `mj-core/src/config.rs` plus arms in every exhaustive match on `HarnessKind`. Its distinctive pieces are: the `ZCODE_*` environment handling in `configure_home_environment`, `home_env`, and `execution_enforcement`; the `captures_native_session` and `pins_startup_config_by_environment` capability methods, which are false and true respectively only for ZCode; the AppImage installer in `mj-worker/src/worker_runtime/harness.rs`; the ACP `account/usage_stats` credit probe in `mj-worker/src/acp.rs` and `mj-worker/src/acp/zcode_usage.rs`; the quota reader `mj-controller/src/zcode_usage.rs`, which calls `GET {host}/api/monitor/usage/quota/limit` with the Coding Plan key as a bearer token; the assets `mj-worker/assets/harnesses/zcode/` and `mj-worker/assets/zcode/runtime.json`; the ZCode layer in `containers/Containerfile.agent-dev`; and the pins `ZCODE_ACP_VERSION`, `ZCODE_VERSION`, `ZCODE_CLI_VERSION` in `mj-core/src/harness_runtime.rs`. The design record for that integration is `.agents/plans/add-zcode-support.md`; it is retained as history and is not needed to execute this plan.

The `harness_kind` column on the `sessions` and `hidden_native_sessions` tables in `mj-controller/src/database/schema.rs` is a TEXT column with a CHECK constraint listing the allowed values. Migration 32 (`migrate_zcode_harness_kind`) added `'zcode'` and raised the minimum compatible revision. This plan does not touch that migration or add another.

## Plan of Work

### Milestone 1: provider descriptor and per-profile capabilities in `mj-core`

Add a module `mj-core/src/codex_provider.rs` that parses the Codex `config.toml` inside a profile home and returns the custom provider, if any. Parsing uses the `toml` crate `mj-core` already depends on. The module reads only the keys named in `Interfaces and Dependencies`; every other key is ignored so the file stays user-owned. A missing file or a file without `model_provider` yields `None`, meaning the profile uses Codex's native OpenAI provider. A `model_provider` whose table is missing, whose `wire_api` is not `responses`, or which names neither `env_key` nor `experimental_bearer_token` is an error naming the profile and the key.

Add an `AuthScheme` enum next to it: `NativeLogin` (the harness's own login, today's behaviour for every kind) or `ApiKey { env_key: String }`. Add `HarnessProfile::codex_provider()` and `HarnessProfile::auth_scheme()` in `mj-core/src/config.rs`. `auth_scheme()` returns `ApiKey` only for a Codex profile whose provider declares `env_key`; a provider that uses `experimental_bearer_token` is accepted but reported as `NativeLogin`-equivalent for the auth gate because the key is inside the staged file (document that the `env_key` form is preferred so the key never lands in a staged file).

Extend `HarnessProfile::validate` so that a Codex profile with a custom provider that uses `env_key` must have that variable present and non-empty in `environment`. Error text must name the profile id, the variable, and the file so a user can fix it without reading code. Validation must not fail when the profile home does not exist yet (discovery creates profiles before homes are populated); treat an unreadable home as "no provider". A user-supplied `model_catalog_json` in the profile's `config.toml` is rejected with a message saying Mjolnir generates the catalog for custom providers.

Add a module `mj-core/src/codex_catalog.rs` holding the catalog shape and the reviewer rule. The catalog is parsed as `serde_json::Value` objects with only `slug` and `supported_reasoning_levels` read by name; every other field is passed through untouched so new Codex fields survive. The reviewer rule `guardian_review_model(slugs) -> Option<String>` picks the newest flash model: among slugs containing `flash` (case-insensitive), the one with the greatest version, where the version is the dotted number sequence following `glm-` compared numerically segment by segment (`glm-5.3-flash` beats `glm-5.2-flash`; `glm-5.10-flash` beats `glm-5.3-flash`). `stamp_reviewer(catalog, reviewer)` sets `auto_review_model_override` on every entry to that slug. When no flash model exists the catalog is left unstamped and Codex falls back to reviewing with the session model. Unit-test the version ordering and the pass-through of unknown fields.

Move the four capability answers that currently live on `HarnessKind` behind the profile so consumers stop asking the kind. Add to `HarnessProfile`: `authentication_marker(&self) -> PathBuf` (for `ApiKey` returns `home/config.toml`, else the kind's marker), `credential_freshness(&self, bytes) -> Option<i64>` and `credential_expiry(&self, bytes) -> Option<i64>` (both `None` for `ApiKey`, else delegate), and `supports_guardian_approvals(&self) -> bool` (delegates to the kind; see the decision log). Keep the kind-level functions for the kinds that have no provider notion, but make every Codex consumer go through the profile.

Change `login_command` in `mj-core/src/credentials.rs` to return a `Result`; for `ApiKey` it returns an error saying the profile authenticates with `<ENV_KEY>` from its environment and has no interactive login.

Add unit tests in `mj-core/src/codex_provider.rs` covering: no file, file without provider, valid `env_key` provider, valid `experimental_bearer_token` provider, `wire_api = "chat"` rejected, and missing table rejected. Add a test in `config.rs` that a Codex profile with an `env_key` provider and no matching environment entry fails validation with a message naming the variable.

### Milestone 2: controller, worker, and CLI consumers use the profile capabilities

In `mj-controller/src/setup.rs`, `harness_is_authenticated_with` and its `_with_executor` wrapper currently take `(kind, home)`. Change them to take the profile (or the marker path computed by the caller from the profile) so an API-key profile whose `config.toml` exists counts as authenticated. `mj-controller/src/doctor.rs` and the setup report use the same path.

In `mj-controller/src/quota.rs`, `refresh_profile` matches on `request.harness`. Add the provider and the key to `QuotaRefreshRequest` (the request is built from the profile, so the builder can resolve `env_key` from `profile.environment`). For a Codex request whose provider base URL host is `api.z.ai` or `open.bigmodel.cn`, call the Z.ai quota reader with that host and key; for any other custom provider return an `Unavailable`-style error stating that quota is not supported for that provider; for a native Codex profile keep today's ChatGPT path. Rename `mj-controller/src/zcode_usage.rs` to `mj-controller/src/zai_usage.rs` and change its entry point from `query(home)` to `query(host, api_key)`, deleting the code that reads ZCode's `v2/config.json`. Keep its response parsing and its tests, adapting the fixtures.

In `mj-controller/src/worker_client.rs`, `reconcile_session` syncs the credential file into running sessions. For an `ApiKey` profile there is nothing to sync; skip the credential exchange and keep the skills sync. `credential_endpoint` in `mj-worker/src/worker_runtime.rs` derives the marker from the launch config on the target; the launch config must carry the marker file name (add a field to `WorkerLaunchConfig` in `mj-core/src/worker_launch.rs`, defaulting to the kind's marker for older persisted configs) so the worker does not re-derive it from the kind.

In `mj-controller/src/utility_llm.rs`, `backend_for_profile` returns `Ok(None)` for an `ApiKey` Codex profile, and `utility_precedence` returns `None` for it so it is never ranked. Add a test with a fake profile home containing a Z.ai provider that asserts no backend and no precedence.

In `mj-controller/src/controller/worker_binary.rs`, after `stage_profile` copies the Codex allowlist, a custom-provider profile gets its catalog: fetch `GET {base_url}/models` with header `Authorization: Bearer <key>` (key from `profile.environment[env_key]`), an 8 second timeout, and no redirects, mirroring the client setup in the Z.ai quota reader. Parse with `mj_core::codex_catalog`, stamp the reviewer, write it as `models.json` in the staged home, and append `model_catalog_json = "models.json"` to the staged `config.toml` (append a line rather than rewriting the TOML, so the user's file stays byte-identical otherwise; `validate` already guarantees the key is absent). On success store the raw catalog body in `profile_config_cache` keyed by profile id with fingerprint `catalog:{base_url}`; on failure fall back to that cached body and log a warning naming the provider; with neither, fail the launch with an error that names the profile and the URL. Staging already runs in the background launch path, so this fetch adds no work to any UI loop. `probe_profile` in `mj-controller/src/controller/profile_config.rs` stages the profile the same way, so discovery sees the same catalog.

In `mj-cli/src/main.rs`, the `mj login` command reads the marker and runs `login_command`; with `login_command` now returning `Result`, print the error and exit non-zero.

Add a controller test that builds a profile with a Z.ai provider in a temporary home and checks: it is reported authenticated, `stage_profile` copies `models.json`, and the launch environment contains the API key variable.

### Milestone 3: remove the ZCode harness

Delete `HarnessKind::Zcode` and every arm that matches it. The list of files that mention ZCode is: `mj-checkpoint/src/checkpoint.rs`, `mj-cli/src/import.rs`, `mj-controller/src/controller.rs`, `mj-controller/src/controller/profile_config.rs`, `mj-controller/src/controller/resume.rs`, `mj-controller/src/controller/worker_binary.rs`, `mj-controller/src/database.rs`, `mj-controller/src/database/schema.rs`, `mj-controller/src/database/tests.rs`, `mj-controller/src/import.rs`, `mj-controller/src/lib.rs`, `mj-controller/src/native_continuity.rs`, `mj-controller/src/quota.rs`, `mj-controller/src/setup.rs`, `mj-controller/src/utility_llm.rs`, `mj-controller/src/zcode_usage.rs`, `mj-core/src/acp/surface.rs`, `mj-core/src/config.rs`, `mj-core/src/credentials.rs`, `mj-core/src/harness_runtime.rs`, `mj-core/src/skills.rs`, `mj-core/src/usage.rs`, `mj-tui/src/setup.rs`, `mj-tui/src/setup/schema.rs`, `mj-worker/src/acp.rs`, `mj-worker/src/acp/tests.rs`, `mj-worker/src/acp/zcode_usage.rs`, and `mj-worker/src/worker_runtime/harness.rs`. Run `grep -rin zcode --include=*.rs --include=*.toml --include=*.json --include=*.md --include=Containerfile* .` after the deletion and expect hits only in `mj-controller/src/database/schema.rs` (the historical migration 32 and the tolerated `'zcode'` constraint value), `.agents/plans/add-zcode-support.md`, and this plan.

Two capability methods become constant once ZCode is gone: `captures_native_session` is true for every remaining kind and `pins_startup_config_by_environment` is false for every remaining kind. Delete both methods and the branches that consult them in `mj-worker/src/acp.rs` (mode enforcement and model/effort reapply), `mj-controller/src/native_continuity.rs`, `mj-controller/src/controller/resume.rs`, and `mj-checkpoint/src/checkpoint.rs`. Delete the `credits` field of the turn usage type in `mj-core/src/usage.rs` together with the `account/usage_stats` probe in `mj-worker/src/acp.rs` and the whole `mj-worker/src/acp/zcode_usage.rs` module, since only ZCode produced it; remove its mention from `docs/src/content/docs/api-reference.md`.

Delete `mj-worker/assets/harnesses/zcode/` and `mj-worker/assets/zcode/`, the `install_zcode` function and its dispatch in `mj-worker/src/worker_runtime/harness.rs`, the `ZCODE_*` constants and the Zcode arm of `pin()` in `mj-core/src/harness_runtime.rs`, and the ZCode layer (the AppImage download, extraction, and `ENV ZCODE_*` lines, plus the matching lines in the `/etc/profile.d/20-mjolnir-tools.sh` block) in `containers/Containerfile.agent-dev`. The test in `worker_binary.rs` that asserts Containerfile pins match the Rust constants must drop its ZCode expectations.

In `mj-controller/src/database/schema.rs`, leave `migrate_zcode_harness_kind` untouched and add a comment above the current CHECK constraint text stating that `'zcode'` is retained only so rows written by earlier releases remain readable, and that no code accepts it. `HarnessKind::from_str` already rejects `"zcode"`, so a stored `zcode` session surfaces as an unknown-kind error where the controller reads it; find that read (the session row decoder in `mj-controller/src/database.rs`) and make sure the error message says the session's harness is no longer supported rather than panicking. Add a database test that inserts a `zcode` session row into an isolated store and asserts that listing sessions reports it as unsupported without failing the whole listing.

Remove the `zcode` entry from the TUI setup dropdown in `mj-tui/src/setup/schema.rs` and the test in `mj-tui/src/setup.rs` that adds a `zcode` profile. Remove ZCode from `docs/src/content/docs/durability.md`, `docs/src/content/docs/api-reference.md`, and `docs/src/content/docs/security.md`.

### Milestone 4: documentation and live validation

Add a section to `docs/src/content/docs/profiles.md` titled "Codex with a custom provider" that shows the exact Codex `config.toml` and `models.json` from `Concrete Steps`, explains that the key goes in the profile's `environment` under the `env_key` name, that Mjolnir fetches the provider's model catalog and runs Guardian reviews on the newest flash model it lists, that `mj login` does not apply, and that quota reporting is available for Z.ai hosts. Also add the missing ZCode rows' replacement: the supported-harness table should list six kinds after this plan.

Then follow `Validation and Acceptance`.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel` unless stated.

Create the profile home. The directory name is the user's choice; `~/.codex-glm` keeps it separate from the OAuth Codex home.

    mkdir -p ~/.codex-glm
    cat > ~/.codex-glm/config.toml <<'EOF'
    model = "glm-5.3"
    model_provider = "zai"
    model_reasoning_effort = "high"

    [model_providers.zai]
    name = "Z.ai coding plan"
    base_url = "https://api.z.ai/api/v1"
    env_key = "ZAI_API_KEY"
    wire_api = "responses"
    EOF

Do not create `models.json`; Mjolnir fetches the catalog from Z.ai at launch and stages it.

Add the profile to Mjolnir's config (`~/.config/mjolnir/config.toml`), pasting the Coding Plan key from `~/.zcode/v2/config.json` (`provider."builtin:zai-coding-plan".options.apiKey`), and delete the `[profiles.zcode]` block:

    [profiles.glm]
    kind = "codex"
    home = "/home/jonathan/.codex-glm"

    [profiles.glm.environment]
    ZAI_API_KEY = "<key>"

Build and check:

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo run -p mj-cli -- doctor

Expected: the doctor report lists profile `glm` as authenticated with no login remediation line, and no profile of kind `zcode`.

## Validation and Acceptance

Automated: `cargo test` and `cargo clippy --all-targets -- -D warnings` pass on the dev profile, run outside the sandbox. The new tests named in Milestones 1 to 3 fail before their milestone and pass after.

Behavioural, on the user's install after Milestone 4:

1. `mj doctor` shows `glm` authenticated. Editing the profile to remove `ZAI_API_KEY` and rerunning shows a config error naming `ZAI_API_KEY` and profile `glm`.
2. Starting a session with profile `glm` on the local bare target shows `glm-5.3` and `glm-5.3-flash` as the only model choices, with efforts `low`, `high`, `max` for the former.
3. Asking the session to create a file inside the project succeeds. Asking it to write a file under `~` produces a Guardian Review entry in the transcript before the write, and the Codex rollout for the reviewer thread (under the staged home's `sessions/` directory) records `"model":"glm-5.3-flash"`.
8. The staged home contains `models.json` whose entries all carry `"auto_review_model_override": "glm-5.3-flash"`, and its `config.toml` ends with `model_catalog_json = "models.json"`.
4. The quota panel for `glm` shows the Coding Plan windows (used, remaining, reset time) rather than an error.
5. Starting a session with profile `glm` inside the default container image works with no ZCode layer present; `podman image inspect` of a freshly built `agent-dev` image shows no `/opt/zcode` path.
6. `mj login --profile glm` prints that the profile authenticates with `ZAI_API_KEY` and exits non-zero.
7. A store that contains a pre-existing `zcode` session row still lists its other sessions; the `zcode` row is reported as an unsupported harness.

## Idempotence and Recovery

All code edits are ordinary commits on the current branch and can be re-applied or reverted with git. The profile home under `~/.codex-glm` can be recreated from `Concrete Steps` at any time. No database migration is added, so the live store needs no backup for this change; the only live-config edit is removing `[profiles.zcode]` and adding `[profiles.glm]`, both reversible by editing the file. The container image rebuild is repeatable; the previous image tag remains available.

## Artifacts and Notes

Probe transcript excerpts (Codex 0.154.0, codex-acp 1.11.4, isolated `CODEX_HOME`):

    == 1. plain reply (glm-5.3, env_key)
    codex
    pong
    tokens used
    3,382

    == 3. codex-acp: modes, models, guardian
    modes: {"availableModes": [{"id": "read-only", ...}, {"id": "agent", "name": "Approve for me", ...}, {"id": "agent-full-access", ...}], "currentModeId": "agent"}
    configOption: model current= glm-5.3 options= ['glm-5.3', 'glm-5.3-flash']
    configOption: reasoning_effort current= high options= ['low', 'high', 'max']
      update: tool_call "mkdir -p /home/jonathan/mj-glm-probe-outside && printf 'escalated\n' > .../escalated.txt"
      update: tool_call "Guardian Review"
    outside file exists: True

Reviewer thread evidence:

    task_complete {"last_agent_message": "{\"outcome\":\"allow\"}", "duration_ms": 1585}
    log: turn{... model=glm-5.3 codex.turn.reasoning_effort=low} post sampling token usage total_usage_tokens=5358

## Interfaces and Dependencies

In `mj-core/src/codex_provider.rs`, define:

    /// A custom model provider named in a Codex `config.toml`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CodexProvider {
        pub id: String,
        pub base_url: String,
        /// Environment variable that carries the API key, when the provider
        /// uses `env_key`.
        pub env_key: Option<String>,
        /// True when the key is inline as `experimental_bearer_token`.
        pub inline_bearer_token: bool,
        pub model_catalog_json: Option<PathBuf>,
    }

    /// Reads `<home>/config.toml`. `Ok(None)` when the file is absent or names
    /// no `model_provider`.
    pub fn codex_provider(home: &Path) -> anyhow::Result<Option<CodexProvider>>;

In `mj-core/src/config.rs`, define:

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum AuthScheme {
        NativeLogin,
        ApiKey { env_key: String },
    }

    impl HarnessProfile {
        pub fn codex_provider(&self) -> anyhow::Result<Option<CodexProvider>>;
        pub fn auth_scheme(&self) -> AuthScheme;
        pub fn authentication_marker(&self) -> PathBuf;
        pub fn credential_freshness(&self, bytes: &[u8]) -> Option<i64>;
        pub fn credential_expiry(&self, bytes: &[u8]) -> Option<i64>;
        pub fn supports_guardian_approvals(&self) -> bool;
    }

In `mj-core/src/codex_catalog.rs`, define:

    /// A Codex `models.json` document. Entries are kept as JSON objects so
    /// unknown Codex fields pass through unchanged.
    pub struct CodexCatalog { pub models: Vec<serde_json::Map<String, serde_json::Value>> }
    pub fn parse(bytes: &[u8]) -> anyhow::Result<CodexCatalog>;
    pub fn guardian_review_model(slugs: impl IntoIterator<Item = String>) -> Option<String>;
    pub fn stamp_reviewer(catalog: &mut CodexCatalog, reviewer: &str);

In `mj-core/src/credentials.rs`, change:

    pub fn login_command(profile: &HarnessProfile) -> anyhow::Result<(String, Vec<String>)>;

In `mj-controller/src/zai_usage.rs` (renamed from `zcode_usage.rs`), define:

    pub async fn query(host: &str, api_key: &str) -> anyhow::Result<Vec<ZaiUsageWindow>>;

In `mj-core/src/worker_launch.rs`, add to `WorkerLaunchConfig`:

    /// File name inside the staged home that proves authentication.
    /// Older persisted configs omit it; the worker falls back to the kind's marker.
    #[serde(default)]
    pub authentication_marker: Option<String>,

Dependencies: no new crates. `toml` 0.8 and `serde_json` are already dependencies of `mj-core`; `reqwest` is already a dependency of `mj-controller`. The anvil-client pin is unchanged.
