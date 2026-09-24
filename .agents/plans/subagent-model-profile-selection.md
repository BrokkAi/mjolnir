# Choose a sub-agent's profile by model and remaining quota, and show profile setup in `mj doctor`

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

A Mjolnir session can start child sessions, called sub-agents, through tools that Mjolnir gives the session's model: `list_profiles` lists the profiles a child may run on and the models each offers, and `spawn` starts a child. A profile is one configured harness account (a `[profiles.<id>]` table in `~/.config/mjolnir/config.toml`), for example one ChatGPT login for Codex. Each subscription profile has quota windows, a 5-hour one and a weekly one, and the daemon (the long-running background process that owns sessions) refreshes how much of each is left.

A user saw a parent ask for a child on the GPT 6 Luna model and get a profile with almost no quota left, although other profiles offering that model had plenty. After this change, `spawn` must name a model (or the word `current`, meaning the parent's own model), and unless the parent pins a profile, Mjolnir runs the child on the eligible profile that offers that model and has the most quota left, where "quota left" is the lower of the 5-hour and weekly remaining percentages. `list_profiles` no longer hides a profile's models behind another profile of the same harness. And `mj doctor` now says, for every profile, what kind it is, where its quota comes from and whether sub-agents may use it, and prints the sub-agent policy.

To see it working: in an isolated instance with two Codex profiles offering the same model and different quota, a parent's `spawn` with `model: "current"` starts the child on the profile with more quota left; `mj --instance <name> doctor` prints the new lines.

## Progress

- [x] (2026-09-24 20:19Z) User's live config: `[subagents.eligible_profiles]` set to codex, codex2, codex3, codex4, deepseek (backup `~/.config/mjolnir/config.toml.bak-20260924T201850`); installed `mj doctor --json` reports the file valid.
- [x] (2026-09-24 21:05Z) Milestone 1: shared selection for `spawn` (HTTP route and MCP tool), `list_profiles` merge, tool text. `ProfileCatalog::published` and `SubagentBackend::published_profile_config` removed (no callers left). `cargo test -p brokk-mj-controller --lib -- server_runtime::api server::api profile_catalog`: 132 passed; worker `subagent_mcp`: 17 passed.
- [x] (2026-09-24 21:30Z) Milestone 2: `mj doctor` profile summary, sub-agent policy line, unknown eligible id warning. `cargo test -p brokk-mj-controller --lib -- doctor`: 82 passed; `-- setup`: 49 passed.
- [x] (2026-09-24 21:50Z) Milestone 3: `[subagents]` documented in `docs/src/content/docs/configuration.md`. Full `cargo test`: 42 suites, 4702 passed, 0 failed, 28 ignored. `cargo clippy --all-targets -- -D warnings`: clean. `mj --instance subagent-select doctor` shows the new lines (transcript in Artifacts and Notes).
- [x] (2026-09-24 22:00Z) Removed the unknown-eligible-id warning added in Milestone 2: it could never run (see Surprises & Discoveries).
- [ ] Live spawn in an isolated instance (costs real quota; left for the user to approve).

## Surprises & Discoveries

- Observation: `list_profiles` ranked profiles by quota and kept only one per harness before it looked at models, so a pay-per-use Codex profile (which ranks as 100% left because it has no quota windows) hid every ChatGPT Codex profile and the models they offer.
  Evidence: `select_profile_per_harness` in `mj-controller/src/server_runtime/api.rs` sorted by harness and `profile_remaining_percent`, then `dedup_by` harness; `profile_remaining_percent` returns 100 for `is_usage_priced()`.
- Observation: an `eligible_profiles` entry naming no profile never reaches `subagent_eligibility_checks`; `SubagentConfig::validate` in `mj-core/src/config.rs` makes the whole file fail to load, and doctor's `config` check reports it. The planned "unknown id" warning was therefore unreachable and was removed; a test now pins the real behavior (`an_eligible_id_that_names_no_profile_fails_the_configuration_check`).
  Evidence: `mj --instance subagent-select doctor` with `codx = true` printed "fixable Mjolnir configuration: … is invalid: [subagents] eligible profile \"codx\" is not defined in this config".

- Observation: a Codex profile's quota is read through its custom provider only when the provider's `env_key` names a variable set in the profile's `[profiles.<id>.environment]`. A provider whose key is inline (`experimental_bearer_token`) or missing falls through to the ChatGPT quota query, which fails, so the profile has no usable report and ranks last for sub-agents.
  Evidence: `provider_credential` in `mj-controller/src/quota.rs` returns `None` without `env_key` and a matching environment entry; `refresh_profile` then takes the `HarnessKind::Codex` ChatGPT arm. `mj doctor` now says "no quota report, because custom provider … has no API key in this profile's environment" for that case, reusing `provider_credential` so the two cannot disagree.

## Decision Log

- Decision: `spawn` requires a model; the value `current` means the parent's current model.
  Rationale: the user's choice. Selecting a profile needs a model to match, and a required argument makes the parent state what it wants instead of inheriting silently.
  Date/Author: 2026-09-24, user.
- Decision: the wire type `SubagentToolAction::Spawn { model: Option<String> }` stays optional; the daemon refuses a missing model.
  Rationale: the worker (which runs next to the harness) and the daemon can be different builds, and both sides deserialize with `deny_unknown_fields`; keeping the type avoids a protocol break while the rule is still enforced in one place.
  Date/Author: 2026-09-24, Claude.
- Decision: selection waits for profiles whose model lists are not yet discovered; one failed discovery excludes only that profile.
  Rationale: the user chose waiting over skipping. Excluding a failed profile instead of failing the call keeps one broken login from blocking every spawn.
  Date/Author: 2026-09-24, user (waiting) and Claude (failure handling).
- Decision: the parent's own profile stays eligible whether or not it is listed; only the user's config changed.
  Rationale: the user's choice; `SubagentConfig::profile_is_eligible` in `mj-core/src/config.rs` already encodes it.
  Date/Author: 2026-09-24, user.
- Decision: ranking is remaining percent descending, unknown last, then the parent's own profile, then profile id; `profile_remaining_percent` is reused unchanged.
  Rationale: it already computes the minimum over the profile's windows, which for Codex and Claude are the 5-hour and weekly ones, and `list_profiles` already used it, so both tools rank the same way.
  Date/Author: 2026-09-24, Claude.
- Decision: an omitted effort is inherited from the parent when the chosen profile offers that value, otherwise left to the harness default.
  Rationale: before this change the parent's effort was inherited only onto the parent's own profile; children now move between profiles of the same harness, where the same effort values usually exist.
  Date/Author: 2026-09-24, Claude.
- Decision: `list_profiles` merges profiles of the same harness that offer exactly the same models, keeping the best-ranked one.
  Rationale: the answer stays short for several logins of one account type, and a profile that offers different models is never hidden.
  Date/Author: 2026-09-24, Claude.

## Outcomes & Retrospective

A `spawn` now names a model and lands on the eligible profile that offers it with the most quota left; both spawn paths share one resolver, and `list_profiles` shows every distinct set of models. `mj doctor` shows each profile's quota source and delegation, and the sub-agent policy. The one planned item that did not survive was the unknown-id warning, because configuration loading already rejects that case. Not yet shown end to end: a live spawn choosing between two real ChatGPT logins, which needs real quota. A possible further cause of the original report is unverified: `codex_usage::parse_report` reads only the `codex` bucket of `rateLimitsByLimitId`, so a model-specific limit bucket would not affect ranking.

## Context and Orientation

The daemon's HTTP server has two ways to start a sub-agent, and both must behave the same. The MCP tool path: the worker process running next to a parent's harness serves the `spawn` tool (tool schemas in `tool_definitions` in `mj-worker/src/subagent_mcp.rs`) and forwards each call to the daemon as a `SubagentToolRequest` (`mj-core/src/subagent.rs`), which `ApiBackend::execute_subagent_tool_inner` in `mj-controller/src/server_runtime/api.rs` executes; its `SubagentToolAction::ListProfiles` and `SubagentToolAction::Spawn` arms are the ones this plan changes. The HTTP path: `POST /api/v1/sessions/{id}/subagents`, handled by `spawn_subagent` in `mj-controller/src/server/api/subagents.rs`. That module talks to the daemon only through the `SubagentBackend` trait (`mj-controller/src/server/api/subagent_backend.rs`), which `ApiBackend` implements and route tests fake (`FakeBackend` in `mj-controller/src/server/api/tests.rs`).

Which profiles a parent may use comes from `profile_catalog.candidates(parent_profile)` (`mj-controller/src/server_runtime/profile_catalog.rs`): every enabled profile that `SubagentConfig::profile_is_eligible` admits, meaning the parent's own profile plus those set to true in `[subagents.eligible_profiles]`. What a profile offers (its `ProfileConfig`: default `model`, `models`, `efforts`, from `mj-core/src/worker_launch.rs`) is discovered by launching its harness once; the catalogue does that in the background for every enabled profile, and `capabilities(&[id])` waits for that discovery when it has not finished. Quota reports live in `ApiBackend::quota_reports`; `profile_remaining_percent` turns one report into the lower of its windows' remaining percentages, 100 for a pay-per-use profile, and `None` when the report is missing or failed.

After a child is registered, `start_followup` (`apply_followup` in `server_runtime/api.rs`) waits for the child's harness session, checks the model and effort against what that live session offers, sets them, and sends the first prompt. That stays as it is.

`mj doctor` builds a list of `DoctorCheck` values (`id`, `title`, `status`, `detail`, `remediation`) in `run_with_config_path` in `mj-controller/src/doctor.rs`; `harness_checks` makes one `harness.<id>` check per profile and `subagent_eligibility_checks` warns about eligible profiles that are disabled. A Codex profile may use a custom model provider, which `HarnessProfile::codex_provider()` (`mj-core/src/config/harness.rs`) reads from the profile's Codex `config.toml`; `zai_usage::serves_quota(host)` says whether that provider publishes quota.

## Plan of Work

Milestone 1 puts profile selection in one place. In `mj-controller/src/server/api/subagents.rs` add `SubagentCandidate` (profile id, harness, `ProfileConfig`, remaining percent) and `SubagentCandidates` (the offered candidates plus the profiles whose discovery failed, with reasons); a pure `rank_candidates`; a pure `choose_subagent_profile(candidates, requested_profile, parent_profile, model)`; a pure `merge_same_models` for `list_profiles`; and an async `resolve_subagent_selection` that refuses a missing model, turns `current` into the parent's live model, fetches candidates, chooses, and settles effort. Add `SubagentBackend::subagent_candidates(parent_profile)` with a refusing default, implement it in `ApiBackend` from the catalogue and quota reports, and make both spawn paths call the resolver. `ListProfiles` uses the same candidates and `merge_same_models`. `select_profile_per_harness` and the trait's `published_profile_config` go away. The `spawn` and `list_profiles` tool text in `mj-worker/src/subagent_mcp.rs` says `model` is required, what `current` means, and how the profile is chosen. The constant `CURRENT_MODEL` lives in `mj-core/src/subagent.rs` so the worker and daemon agree.

Milestone 2 changes `mj-controller/src/doctor.rs`: each `harness.<id>` detail begins with a summary clause; a `subagents.policy` check states the policy; `subagent_eligibility_checks` warns about an unknown id.

Milestone 3 documents `[subagents]` in `docs/src/content/docs/configuration.md`, runs the full suite and clippy, checks in an isolated instance, and pushes.

## Concrete Steps

From the repository root (`/home/jonathan/Projects/mjolnir2`), outside any sandbox:

    cargo test -p brokk-mj-controller -p brokk-mj-worker -p brokk-mj-core
    cargo test
    cargo clippy --all-targets -- -D warnings

Do not redirect `target/`; this host shares a build cache through it.

## Validation and Acceptance

Unit tests show the behavior: with Luna offered by `codex2` at 3% and `codex4` at 60%, the chooser picks `codex4`; a pay-per-use profile without the model is never picked; a tie goes to the parent's own profile; an explicit profile is honored; a model nobody offers is refused with the offers listed; a missing model is refused; `current` resolves to the parent's model; a failed discovery excludes only that profile; `list_profiles` merges two same-model profiles and keeps a different one. Doctor tests show the policy line, the unknown-id warning, and the summary clause for a subscription and a pay-per-use profile.

In an isolated instance (`--instance subagent-select`, whose config holds copies of the user's `codex*`, `deepseek` and one Claude profile), `mj --instance subagent-select doctor` prints the new lines.

## Idempotence and Recovery

The code steps are ordinary edits. The live config edit was backed up to `config.toml.bak-20260924T201850`; restoring that file restores the old list, and the daemon reloads `config.toml` every 500 ms.

## Artifacts and Notes

    $ target/debug/mj --instance subagent-select doctor   (excerpt)
    ready Harness profile claude3: Claude Code; Claude subscription quota; only its own sessions' sub-agents may use it. /home/jonathan/.claude3 is present and authentication is available
    ready Harness profile codex2: Codex; ChatGPT subscription quota; any session's sub-agents may use it. /home/jonathan/.codex2 is present and authentication is available
    ready Harness profile deepseek: Codex; pay-per-use through api.deepseek.com, counted as 100% left when choosing a sub-agent's profile; any session's sub-agents may use it. /home/jonathan/.codex-deepseek is present and authentication is available
    ready Sub-agent policy: On for Claude and Codex sessions, up to 6 sub-agents at once per session. A session's sub-agents may use its own profile and: codex, codex2, codex3, codex4, deepseek.

## Interfaces and Dependencies

In `mj-controller/src/server/api/subagents.rs`:

    pub struct SubagentCandidate { pub profile_id: String, pub harness: HarnessKind, pub choices: ProfileConfig, pub remaining_percent: Option<u8> }
    pub struct SubagentCandidates { pub offered: Vec<SubagentCandidate>, pub unavailable: Vec<(String, String)> }
    pub(crate) async fn resolve_subagent_selection(backend: &Arc<dyn SubagentBackend>, parent_session_id: &str, parent_profile: &str, profile_id: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Result<SubagentSelection, ApiFailure>
    pub(crate) struct SubagentSelection { pub profile_id: String, pub model: String, pub effort: Option<String> }

In `mj-controller/src/server/api/subagent_backend.rs`, on `SubagentBackend`:

    fn subagent_candidates(&self, parent_profile: String) -> BoxFuture<'_, AnyResult<SubagentCandidates>>

In `mj-core/src/subagent.rs`:

    pub const CURRENT_MODEL: &str = "current";

Revision note (2026-09-24): Milestone 2's unknown-id warning was removed after the isolated-instance run showed configuration loading already rejects that case; Purpose, Progress, Surprises, and Outcomes were updated to match.
