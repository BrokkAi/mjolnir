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
- [x] (2026-09-15 20:10Z) Milestone 3: the ZCode harness is gone from code, assets, the container image, and the documentation. `HarnessKind` has six variants; `captures_native_session`, `pins_startup_config_by_environment`, the `account/usage_stats` credit probe, `mj-worker/src/acp/zcode_usage.rs`, the `credits` turn-usage field, the AppImage installer, the assets, and the Containerfile layer are all removed. Migration 32 and the tolerated `'zcode'` CHECK value stay, with a comment saying why.
- [x] (2026-09-15 20:45Z) Milestone 4, documentation half: `docs/src/content/docs/profiles.md` gained a "Codex with a custom provider" section with the exact `config.toml` and profile block, the six-kind harness table now names `config.toml` as the API-key marker, and `docs/src/content/docs/configuration.md` states the `environment` requirement. `npx astro check` reports 0 errors.
- [x] (2026-09-15 23:30Z) Milestone 5: `mj_core::codex_catalog::parse` accepts the Codex catalog shape and OpenAI's plain model list, `parse_codex_shape` and `merge_overrides` apply a profile's own `models.json`, `HarnessProfile::guardian_review_model` selects the reviewer, and `stage_codex_catalog` merges, then stamps accordingly. `docs/src/content/docs/profiles.md` documents the DeepSeek example, the override file, and the setting; `docs/src/content/docs/configuration.md` lists the new field.
- [x] (2026-09-15 17:05Z) Live validation in an isolated instance (`MJ_CONFIG_DIR`/`MJ_DATA_DIR` under the session scratchpad, `MJ_WORKER_BINARY` pointed at the dev worker), all with the user's real keys. GLM: doctor reports the profile authenticated; `mj models --profile glm` lists `glm-5.3`, `glm-5.3-flash`, `glm-5-turbo` with efforts low/high/max; a local bare session wrote `hello.txt`; the staged home holds `models.json` stamped with `glm-5.3-flash` and a `config.toml` beginning `model_catalog_json = "models.json"`; usage recorded whole-turn reports; the viewer snapshot shows Coding Plan 5H and Week windows; `mj login --profile glm` exits 1 naming `ZAI_API_KEY`; an escalated write was reviewed by a `glm-5.3-flash` thread. DeepSeek: `mj models --profile deepseek` lists `deepseek-flash` and `deepseek-v4-pro`, the override file gives `deepseek-v4-pro` efforts low/high; a `deepseek-v4-pro` session wrote `ds.txt`; an escalated write was reviewed by a `deepseek-flash` thread and allowed; the staged catalog stamps `deepseek-flash` on both entries. Reviewer setting on GLM: `"nonexistent"` fails discovery with `guardian_review_model "nonexistent" is not in the provider's model catalog, which lists glm-5.3, glm-5.3-flash, glm-5-turbo`; `"session"` stages no override; `"glm-5.3"` stamps that slug. Not exercised live: the container image build (the Containerfile no longer has a ZCode layer; a fresh image build is still pending) and the legacy `zcode` session row (covered by the unit test only).
- [x] (2026-09-15 17:12Z) Deployment, part one: `scripts/install.sh` installed `mj` 2.9.0, `mj-worker`, the musl worker, and `mj-voice-worker` into `~/.cargo/bin`; `~/.config/mjolnir/config.toml` (backup `config.toml.bak-20260915T121000`) lost `[profiles.zcode]` and `zcode = true`, and gained `[profiles.glm]` (home `~/.codex-glm`) and `[profiles.deepseek-codex]` (home `~/.codex-deepseek`, with a `models.json` override giving `deepseek-v4-pro` efforts low and high), both eligible as sub-agents; `mj doctor` on the new CLI reports both authenticated.
- [ ] Deployment, part two: the live daemon must run the 2.9.0 build. The user's dashboard process (started from `target/release/mj` with `MJ_DEV_RESTART_STALE_DAEMON=1`) restarts the daemon from its own older image whenever the daemon changes, so the daemon reverted to 2.7.2 twice. Remaining step: quit that dashboard and start `mj` again (from `~/.cargo/bin` or `scripts/run.sh`), then confirm `mj daemon status` shows 2.9.0 and `mj models --profile glm` lists only GLM models.

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

- Observation: The flash reviewer denied a benign, explicitly requested write outside the workspace, while the `glm-5.3` reviewer allowed the identical action earlier. Its rationale cited "the current review environment's approval policy is never": Codex runs every Guardian review in a read-only session whose own `approval_policy` is `never`, and the flash model mistook that for the session's policy. The Mjolnir session itself ran `approval_policy = on-request` with `approvals_reviewer = auto_review`, identical to the direct probe. A Guardian deny is final in Codex (`GuardianAssessmentOutcome::Deny` maps to not approved in `codex-rs/core/src/guardian/review.rs`); it does not fall through to asking the user.
  Evidence: reviewer rollout `task_complete` message `{"outcome":"deny","risk_level":"low","user_authorization":"low","rationale":"... the current review environment's approval policy is never, so the require_escalated request cannot be granted here; intrinsically the write itself is benign."}`; reviewer `turn_context` `{'approval_policy': 'never', 'sandbox_policy': {'type': 'read-only'}, 'model': 'glm-5.3-flash'}`; session `turn_context` `{'approval_policy': 'on-request', 'approvals_reviewer': 'auto_review', 'sandbox_policy': {'type': 'workspace-write'}}`.

- Observation: DeepSeek serves the Responses API at `https://api.deepseek.com/v1/responses` (and without `/v1`) for the key stored in `~/.dsh/.credentials.yaml` under `refs.DEEPSEEK_API_KEY`. Codex 0.153.4 with `base_url = "https://api.deepseek.com/v1"`, `env_key = "DEEPSEEK_API_KEY"`, `wire_api = "responses"` answered `pong`, wrote `hello.txt` through its shell tool, and produced reasoning tokens for `deepseek-v4-pro` at `model_reasoning_effort = "high"`. DeepSeek's `GET /models` returns OpenAI's plain list `{"object":"list","data":[{"id":"deepseek-flash",...},{"id":"deepseek-v4-pro",...}]}`, not Codex's catalog format, so Mjolnir must translate it. One first run of `codex exec` completed its turn but did not exit for over seven minutes; three later runs exited in 2 to 3 seconds and the hang did not reproduce.
  Evidence: `codex exec` transcripts `pong` (1,449 tokens for the file write; 6,712 tokens with `"reasoning_output_tokens":24` for the reasoning run); model list body above.

- Observation: A dashboard started with `MJ_DEV_RESTART_STALE_DAEMON=1` restarts the daemon from its own executable image whenever it finds the daemon running a different one. After installing new binaries, the still-running old dashboard replaced the new daemon within seconds, twice. `mj daemon restart` itself re-executes `/proc/self/exe` of the old daemon, so it cannot upgrade either; the old dashboard has to exit and a new client has to start the daemon.
  Evidence: `ps` showed the relaunched daemon as `/proc/self/exe daemon-run` reporting `version 2.7.2`, and the dashboard's `/proc/<pid>/exe` pointed at a deleted `release/mj` inode.

- Observation: A dev-built controller launches the pinned released worker for local bare targets unless `MJ_WORKER_BINARY` names the dev worker. The released 2.9.0 worker rejects the new `authentication_marker` launch-config field because `WorkerLaunchConfig` is `deny_unknown_fields`. Releases ship matching controller and worker versions, so this is a dev-only mismatch, not a compatibility break.
  Evidence: two isolated sessions failed with `worker bootstrap failed: unknown field authentication_marker` until the daemon was restarted with `MJ_WORKER_BINARY=target/debug/mj-worker`.

- Observation: A config whose `[subagents.eligible_profiles]` names a profile id that no longer exists fails validation with "is not defined in this config". The user's live config has `zcode = true` there, so deployment must remove that line as well as `[profiles.zcode]`.

- Observation: Codex's workspace-write sandbox allows writes under `/tmp`, so the first escalation probe under the session scratchpad was not an escalation. The valid probe wrote under the user's home directory.

- Observation: The catalog and guardian probe was rerun on the exact pinned stack, Codex 0.153.4 (the `@openai/codex` dependency of `@brokkai/codex-acp` 1.11.4, selected with `CODEX_PATH`) against an unmodified codex-acp 1.11.4. The ACP `model` config option listed only `glm-5.3` and `glm-5.3-flash`; `session/set_mode` to `agent` succeeded; an escalated write under the user's home produced a "Guardian Review" tool call; and with `auto_review_model_override = "glm-5.3-flash"` stamped on every catalog entry, the session rollout recorded only `"model":"glm-5.3"` while the reviewer thread's rollout recorded only `"model":"glm-5.3-flash"`. No change to the Codex fork or the codex-acp fork is required: every feature this plan uses is stock Codex configuration present in 0.153.4.

- Observation: `model_catalog_json` is a top-level key in Codex's `config.toml`, not a key inside the `[model_providers.<id>]` table. Appending it to the end of a staged file would therefore land it inside whatever table comes last and Codex would ignore it. Staging prepends the line instead, which is valid TOML because top-level keys must precede the first table header, and leaves the rest of the user's file byte-identical.

- Observation: On a local bare target a Codex session runs directly from the user's own profile home (`target_profile_home` in `mj-controller/src/controller.rs` returns `profile.home` for `LocalBare`, and `prepare_worker_files` skips staging entirely). A generated catalog would therefore either be missing or written into the user's home. Staging is now forced for any profile with a custom provider, through `requires_private_profile_home`, and the local bare home becomes `<worker_root>/profile` as it already did for Claude.
  Evidence: the test `a_custom_provider_session_carries_its_key_and_runs_from_a_private_home` asserts `CODEX_HOME` is `/home/me/.local/share/hel/worker/profile`, and `staging_a_custom_provider_profile_writes_a_catalog_the_session_can_pick_from` asserts the user's home keeps no `models.json`.

- Observation: The catalog cache reuses the existing `profile_config_cache` table, whose rows are treated as stale after 24 hours (`load_profile_config_cache_from` in `mj-controller/src/database.rs`). A provider outage longer than a day therefore fails the launch rather than staging a very old catalog. That is the table's existing behaviour and this plan does not change it.

- Observation: Removing `captures_native_session` made three further things dead rather than merely simpler. `native_continuity_lost` could only ever be set by the ZCode-only reload-fallback arm in `mj-worker/src/acp.rs`, so `mj-controller/src/native_continuity.rs` and its two callers could never fire; `Controller::install_adopted_native_session_id` existed only for that path. All three are removed. The wire field `native_continuity_lost` stays on the relay protocol so older workers still deserialize.
  Evidence: `grep -rn native_continuity_lost` showed the only assignment was inside the removed arm; `cargo clippy --all-targets -- -D warnings` reported `install_adopted_native_session_id` as never used once the caller went.

- Observation: A `zcode` session row used to fail the whole session listing, because `load_state_from` in `mj-controller/src/database.rs` turned an unknown harness into a `FromSqlConversionFailure`. It now skips such a row with a warning, and `load_targets`, `load_mounts`, and `load_checkpoints` no longer unwrap on the missing session. `hidden_native_sessions_from` ignores unknown rows the same way.
  Evidence: the test `a_session_for_a_removed_harness_is_skipped_without_hiding_the_others` fails on an `Option::unwrap` in `load_targets` before that change and passes after.

- Observation: The Coding Plan key is rejected by the chat-completions path under `https://api.z.ai/api/v1` but accepted under `https://api.z.ai/api/coding/paas/v4`. This matters only for Mjolnir's utility model (anvil's client speaks chat completions and appends `/v1` to its base URL), not for Codex.
  Evidence: `POST /api/v1/chat/completions` returned `403 model_access_denied` for `glm-5.3`; `POST /api/coding/paas/v4/chat/completions` returned 200 `pong`.

- Observation: `models.json` was never added to the Codex staging allowlist in
  `stage_profile`, although the Milestone 5 text assumed Milestone 2 had done
  so. Nothing depends on it: `stage_codex_catalog` writes the merged, stamped
  catalog over `<staged home>/models.json` on every launch, and it reads the
  user's override from `profile.home`, not from the staged copy. The allowlist
  is left unchanged so the only `models.json` Codex can ever see is the one
  Mjolnir generated.
  Evidence: the allowlist in `mj-controller/src/controller/worker_binary.rs`
  lists `auth.json`, `config.toml`, `AGENTS.md`, `instructions.md`, `rules`, and
  `skills` for Codex; the test
  `a_plain_model_list_becomes_a_catalog_the_profiles_overrides_refine` writes an
  override in the profile home and asserts the staged catalog carries it.

- Observation: `parse` accepts a `data` array whether or not the body also says
  `"object": "list"`. The plan named the shape with that key, but providers vary
  on whether they send it and the `data` array alone is unambiguous once the
  Codex `models` key is absent. A body with neither array still fails, naming
  both shapes, which is the behaviour the plan asked for.
  Evidence: the test `parse_rejects_a_body_in_neither_shape_and_names_both`
  asserts the error names `models` and `data`.

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

- Decision: Delete `mj-controller/src/native_continuity.rs` and `Controller::install_adopted_native_session_id` rather than rewire them to a new condition.
  Rationale: Their only trigger was a worker reporting lost native continuity, which only the ZCode reload fallback produced. Rewiring them to fire for every harness would change behaviour for Codex and Claude, where a native-session mismatch is already handled by the checkpoint restore's own identity check. Deleting unreachable code changes nothing a user can observe.
  Date/Author: 2026-09-15, Claude.

- Decision: A session row for a harness this release no longer supports is skipped from the listing with a warning instead of failing the listing.
  Rationale: The plan keeps `'zcode'` readable in the CHECK constraint so old rows survive. That is only useful if a store holding one still opens. Such a session cannot be resumed either way, so omitting it is the honest result.
  Date/Author: 2026-09-15, Claude.

- Decision: Support providers whose `/models` endpoint returns OpenAI's plain list by translating it into Codex catalog entries with conservative defaults, and let the user refine entries with an optional `models.json` in the profile home that Mjolnir merges by slug over the fetched catalog.
  Rationale: DeepSeek's list carries only ids. Codex needs a full entry per model (context window, reasoning levels, tool style). Defaults make the profile work at once; the override file lets the user add reasoning levels or a larger context window for a specific model without Mjolnir carrying per-vendor tables. The rejection of a user-written `model_catalog_json` in the Codex `config.toml` stays, because Mjolnir writes that key.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Make the Guardian reviewer choice a profile setting, `guardian_review_model`, with values `newest-flash` (default), `session`, or an explicit catalog slug.
  Rationale: The flash reviewer denied a benign action that the full model allowed. The user wants flash for cost, but needs a one-line switch to the session model or a named model when a provider's small model reviews badly. An explicit slug that the fetched catalog does not list fails the launch with an error naming the slug, rather than silently reviewing with something else.
  Date/Author: 2026-09-15, user and Claude.

- Decision: Keep a separate `parse_codex_shape` for the user's override file
  rather than letting `parse` serve both jobs.
  Rationale: `parse` guesses between two shapes because a provider's response is
  not under the user's control. An override file is, and a file that silently
  parsed as an OpenAI list would replace the user's carefully written entries
  with Mjolnir's conservative defaults. Refusing anything but the Codex shape
  tells the user their file is wrong.
  Date/Author: 2026-09-15, Claude.

- Decision: A `guardian_review_model` naming a slug the merged catalog does not
  list fails the whole launch before anything is staged.
  Rationale: The alternative is stamping nothing, which silently falls back to
  reviewing with the session model: the expensive outcome the setting exists to
  avoid, with no signal that the setting did nothing. The error names the
  profile, the slug, and the slugs the catalog lists, so the fix is a copy and
  paste. The test asserts no `models.json` is written in that case.
  Date/Author: 2026-09-15, Claude.

- Decision: Reject `guardian_review_model` on a profile with no custom Codex
  provider at validation time.
  Rationale: Mjolnir generates a catalog only for those profiles, so on any
  other profile the setting can have no effect. Accepting it would be a
  configuration that reads as if it does something.
  Date/Author: 2026-09-15, Claude.

## Outcomes & Retrospective

Milestones 1 to 3, the documentation half of Milestone 4, and Milestone 5 are
complete and committed. A Codex profile can now name any Responses-API model provider and
authenticate with an API key from the profile's `environment`: it reports as
authenticated without a login, exchanges no credential file, advertises only the
provider's own models, runs Guardian reviews on the provider's newest flash
model, reports Coding Plan quota for Z.ai and Zhipu hosts, and is excluded from
utility-model duty. The ZCode harness is gone from code, assets, the container
image, and the documentation; only migration 32 and the tolerated `'zcode'`
CHECK value remain, so stores written by earlier releases still open.

A Codex profile can additionally name a provider that serves only OpenAI's plain
model list, refine the translated catalog with its own `models.json`, and choose
which model runs Guardian reviews.

Validation: after Milestone 5, `cargo test` passes with 3,381 tests and no
failures, and `cargo clippy --all-targets -- -D warnings` is clean, both on the
dev profile outside the sandbox. Earlier, after Milestone 4, `cargo test` passed
with 3,375 tests and
`cargo clippy --all-targets -- -D warnings` is clean, both on the dev profile
outside the sandbox. Two suites (`controller::update::tests::npm_upgrade_*` and
`mj-cli`'s `store_divergence`) failed once each under full-suite parallelism and
passed in isolation and on a rerun; they are pre-existing contention flakes, not
regressions from this work.

What remains: the behavioural acceptance in `Validation and Acceptance` has not
been run, including the Milestone 5 steps 9 (a DeepSeek profile, its override
file, and its reviewer) and 10 (the three `guardian_review_model` settings). It needs the user's real Z.ai Coding Plan key on their own install.
The user should work through steps 1 to 8 there, in particular that the session
offers only `glm-5.3` and `glm-5.3-flash`, that the staged home's `models.json`
carries `"auto_review_model_override": "glm-5.3-flash"` on every entry and its
`config.toml` begins with `model_catalog_json = "models.json"`, that the quota
panel shows Coding Plan windows, and that a freshly built `agent-dev` image has
no `/opt/zcode` path. The user must also remove `[profiles.zcode]` from
`~/.config/mjolnir/config.toml`: a configuration that still names that kind
fails to load with "unknown harness kind".

Lessons: two assumptions in the original plan were wrong in ways that would have
shipped a silently broken feature, and both were caught only by writing the
behaviour test rather than the code. `model_catalog_json` is a top-level Codex
key, so the planned "append a line" would have buried it in the last table; and
a local bare Codex session runs straight from the user's own profile home, so
there was no staged copy to put the catalog in. Both are recorded above.

Optional follow-up not in this plan: codex-acp discards Codex's `guardianWarning` app-server event, so surfacing the reviewer's reasoning in the transcript would be a small bridge-only change. A second optional follow-up: let API-key Codex profiles serve as utility models by giving anvil's `OpenAiClient` a constructor that uses its base URL verbatim, publishing that anvil-client patch, bumping the `brokk-anvil-client` pin in the workspace `Cargo.toml`, and adding a `backend_for_profile` arm that builds the client from the provider's chat base URL (`https://api.z.ai/api/coding/paas/v4` for Z.ai) and the key from the profile environment.

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

### Milestone 5: OpenAI-format model lists, catalog overrides, and the reviewer setting

In `mj-core/src/codex_catalog.rs`, extend `parse` to accept two shapes. The Codex shape is `{"models": [...]}` and is kept as is. The OpenAI shape is `{"object": "list", "data": [{"id": "...", ...}]}`; translate each `data` entry into a Codex catalog entry whose `slug` and `display_name` are the id, whose `description` is the id followed by the `owned_by` value in parentheses when present, and whose remaining fields take these defaults: `default_reasoning_level` absent, `supported_reasoning_levels` empty, `shell_type = "shell_command"`, `visibility = "list"`, `supported_in_api = true`, `priority` = position in the list, `base_instructions = ""`, `supports_reasoning_summaries = false`, `default_reasoning_summary = "none"`, `support_verbosity = false`, `apply_patch_tool_type = "freeform"`, `truncation_policy = {"mode": "bytes", "limit": 10000}`, `context_window = 128000`, `max_context_window = 128000`, `effective_context_window_percent = 95`, `supports_parallel_tool_calls = true`, `experimental_supported_tools = []`, `input_modalities = ["text"]`. A body that matches neither shape is an error naming both.

Add `merge_overrides(catalog, overrides)` to the same module: `overrides` is a parsed Codex-shape catalog; for each override entry, the fetched entry with the same `slug` gets every override field copied over it, and an override slug the fetch did not list is appended as a new entry. In `stage_codex_catalog` (`mj-controller/src/controller/worker_binary.rs`), read `<profile.home>/models.json` when it exists, parse it with the Codex shape only, and merge it before stamping the reviewer. The override is read from the profile home, not the staged copy, and `models.json` is deliberately not on the Codex staging allowlist, so the only `models.json` Codex ever sees is the merged, stamped catalog Mjolnir writes. (The earlier text claimed Milestone 2 had added `models.json` to the allowlist; it had not, and nothing needs it to.)

Add `guardian_review_model: Option<String>` to `HarnessProfile` in `mj-core/src/config.rs` (serde default, skipped when absent), with the two literal values as the constants `GUARDIAN_REVIEW_NEWEST_FLASH` and `GUARDIAN_REVIEW_SESSION` so the controller and the validator agree on their spelling. Validation accepts the literal strings `newest-flash` and `session`, or any non-empty slug; it rejects the field on a profile that is not a Codex profile with a custom provider. Replace the fixed rule in `stage_codex_catalog` with: `newest-flash` (or absent) uses `guardian_review_model(slugs)`; `session` skips stamping; a slug must appear in the merged catalog, else fail the launch with an error naming the profile, the slug, and the slugs the catalog does list. `mj-tui/src/setup/schema.rs` does enumerate profile fields, in three places: the profile default object, the field label table, and the help text table. Add the field to all three. Also add a row to the profile field table in `docs/src/content/docs/configuration.md`.

Update `docs/src/content/docs/profiles.md`: add a DeepSeek example (`base_url = "https://api.deepseek.com/v1"`, `env_key = "DEEPSEEK_API_KEY"`), explain the override file with an example that gives `deepseek-v4-pro` reasoning levels `low` and `high`, and document `guardian_review_model`. Note that quota reporting is available only for Z.ai hosts and that a DeepSeek profile shows no quota.

Tests: parse both shapes and reject a third; merge overrides by slug including an appended slug; reviewer selection for each of the three settings including the missing-slug error; profile validation of `guardian_review_model`.

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

Optionally add a DeepSeek profile the same way, with a home whose `config.toml` is:

    model = "deepseek-v4-pro"
    model_provider = "deepseek"
    model_reasoning_effort = "high"

    [model_providers.deepseek]
    name = "DeepSeek"
    base_url = "https://api.deepseek.com/v1"
    env_key = "DEEPSEEK_API_KEY"
    wire_api = "responses"

and `DEEPSEEK_API_KEY` (from `~/.dsh/.credentials.yaml`, key `refs.DEEPSEEK_API_KEY`) in `[profiles.deepseek-codex.environment]`. Also remove `zcode = true` from `[subagents.eligible_profiles]`.

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
9. A second profile `deepseek` with `kind = "codex"`, a home whose `config.toml` names `base_url = "https://api.deepseek.com/v1"`, `env_key = "DEEPSEEK_API_KEY"`, `wire_api = "responses"`, and `DEEPSEEK_API_KEY` in its environment: `mj models --profile deepseek` lists `deepseek-flash` and `deepseek-v4-pro`; with a `models.json` override giving `deepseek-v4-pro` reasoning levels `low` and `high`, discovery lists those efforts; a session on it writes a file; its Guardian reviewer thread runs on `deepseek-flash`.
10. Setting `guardian_review_model = "session"` on the GLM profile stages a catalog with no `auto_review_model_override`; setting it to `glm-5.3` stamps that slug; setting it to `nonexistent` fails the launch with an error naming the slug.
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
    /// Milestone 5: the Codex shape only, for a user-written override file.
    pub fn parse_codex_shape(bytes: &[u8]) -> anyhow::Result<CodexCatalog>;
    pub fn guardian_review_model(slugs: impl IntoIterator<Item = String>) -> Option<String>;
    pub fn stamp_reviewer(catalog: &mut CodexCatalog, reviewer: &str);
    /// Milestone 5: `parse` accepts the Codex shape and OpenAI's `{"data": [...]}` list.
    pub fn merge_overrides(catalog: &mut CodexCatalog, overrides: &CodexCatalog);

In `mj-core/src/config.rs`, add to `HarnessProfile`:

    /// Guardian reviewer choice for a Codex profile with a custom provider:
    /// "newest-flash" (default when absent), "session", or a catalog slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_review_model: Option<String>,

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

## Revision notes

2026-09-15, Milestone 5 implementation. Corrected the Milestone 5 text, which
claimed Milestone 2 had put `models.json` on the Codex staging allowlist. It had
not, and the feature does not need it to: the override is read from the profile
home and the merged result is written over the staged file. Recorded the
correction in `Surprises & Discoveries` so a future contributor does not add the
allowlist entry expecting it to matter. Named `parse_codex_shape` in
`Interfaces and Dependencies`, since the override file must not be parsed with
the shape-guessing `parse`. Replaced "if profile fields are enumerated there"
with the three places in `mj-tui/src/setup/schema.rs` that do enumerate them,
and added the `docs/src/content/docs/configuration.md` field table, which the
original text did not mention.
