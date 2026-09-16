# Answer `list_profiles` from a background-warmed catalogue

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

A Claude or Codex session that Mjolnir manages can delegate work to child sessions through the `mj-agents` MCP tools. The first thing such a model does is usually call `mcp__mj-agents__list_profiles` to learn which profiles it may delegate to, which model each profile would use by default, and which models and reasoning efforts each offers. Today the daemon builds that answer inside the tool call: it reads `config.toml`, then, for every profile it would offer, launches the harness binary in a scratch home to ask it which models and efforts it supports. That discovery takes seconds per profile, so the tool call — and the model, and the person watching the transcript — waits for it.

After this change the daemon discovers those capabilities once in a background pass that starts with the server and re-runs whenever the profile or sub-agent part of the configuration changes. A `list_profiles` call then only filters in-memory data, ranks it with the quota report it already keeps for every profile, and answers. The tool call returns in milliseconds for as long as a warm catalogue exists. If no warm answer is available yet — the few seconds between server start and the first pass finishing, a probe that failed, or a profile the pass has not reached — the call still falls back to the existing synchronous discovery, so the answer is never wrong and never stale, only occasionally as slow as it is today.

You can see it working by opening a Claude or Codex session in Mjolnir, asking the model to call `list_profiles`, and observing that the tool call completes immediately instead of showing a multi-second `waiting` state. The same is visible from the daemon's logs: `profile discovery` work happens at startup, not inside the tool call.

## Progress

- [x] (2026-09-16 01:10Z) Traced the latency of `list_profiles` from the worker's MCP stdio server through the controller's backends to the per-profile discovery in `mj-controller/src/controller/profile_config.rs`.
- [x] (2026-09-16 01:40Z) Chose the design: one background "warm pass" per configuration generation, owned by `ApiBackend`, invalidated and restarted when the profiles or sub-agent policy change.
- [x] (2026-09-16 02:20Z) Add `mj-controller/src/server_runtime/profile_catalog.rs` with the catalogue, its warm pass, and the shared candidate filter.
- [x] (2026-09-16 02:35Z) Read `list_profiles` from the catalogue in `mj-controller/src/server_runtime/api.rs`, keeping the existing synchronous answer as the cold fallback.
- [x] (2026-09-16 02:40Z) Start the warm pass with the server and re-sync it from the controller-reload arm in `mj-controller/src/server_runtime.rs`.
- [x] (2026-09-16 03:05Z) Test the catalogue's behaviour with hand-written fakes and one end-to-end `list_profiles` test.
- [x] (2026-09-16 04:05Z) Run `cargo fmt`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` on the dev profile, then commit on the current branch.

## Surprises & Discoveries

- Observation: the tool call is a straight request/response over a Unix socket; nothing else in the path polls or sleeps, so the controller's answer time is the tool's answer time.
  Evidence: `mj-worker/src/subagent_mcp.rs` writes the request to the socket and blocks for the reply; `mj-controller/src/server_runtime.rs` handles it in a spawned `execute_subagent_tool` job that completes the worker's pending request.
- Observation: per-profile discovery is already cached twice (a per-profile in-process lock and a 24-hour SQLite cache), so the seconds come from *cache misses*, not from the cache being absent.
  Evidence: `mj-controller/src/controller/profile_config.rs` consults `profile_config_cache` before it runs a probe, and `probe_profile` runs `worker discover-config` with a 300-second deadline.
- Observation: the daemon already reloads the whole configuration whenever it publishes a runtime revision, and it already uses that reload to notice profile changes for the quota poller.
  Evidence: `spawn_manager_target_refresher` in `mj-controller/src/daemon.rs` reloads `Controller::load` every 500 ms and publishes a revision when the configuration changed; `republish_quota_profiles` in `mj-controller/src/server_runtime.rs` compares `controller.config.profiles` and restarts the quota refresher only when profiles changed.
- Observation: the quotas poller already launches one harness process per profile every ten minutes, so one extra background probe per profile at startup and per configuration change is a smaller cost than the daemon already pays.
  Evidence: `QUOTA_REFRESH_INTERVAL` in `mj-controller/src/pollers.rs` is ten minutes and `refresh_profile_quotas` refreshes every profile in the batch.
- Observation: a superseded pass still started every harness, because the loop checked its generation only after a probe finished.
  Evidence: the first version of `warm_pass` spawned all probes before its first `is_current` check, and the test `a_pass_of_a_superseded_configuration_publishes_nothing` — written to assert a superseded pass starts nothing — failed on `calls == 0`. The check now runs before each spawn, so a pass that is already stale starts no harness at all.
- Observation: the daemon does not read `config.toml` on demand for this answer any more; it answers with the configuration the reload pipeline last published to it.
  Evidence: `ApiBackend::list_profile_candidates` reads the file only while the catalogue is cold; a warm catalogue derives candidates from the `Config` that `run_server` passed to `sync`, which arrives with the `ControllerReloaded` result.

## Decision Log

- Decision: warm every enabled profile when the sub-agent policy is enabled, rather than only the profiles some parent is currently eligible for.
  Rationale: any enabled profile can host a session and is therefore its own first candidate — the parent's own profile is always offered — so the union of candidates over all possible parents is exactly the enabled profiles. Deriving that from the configuration alone keeps the pass independent of which sessions happen to exist, so starting or stopping a session never triggers a re-probe. When `[subagents] enabled = false` the candidate list is empty for every parent, so the pass probes nothing at all.
  Date/Author: 2026-09-16 / Claude
- Decision: invalidate on the profile and sub-agent policy only, not on unrelated configuration settings.
  Rationale: `enabled_profiles()` filtered by `SubagentConfig::profile_is_eligible` is the entire input to the candidate list, and the discovered capabilities depend on the profile entries (home, kind, environment) and the pinned harness build. Comparing just `profiles` and `subagents` keeps the theme, phone, target, or review editors from re-probing every harness in the background.
  Date/Author: 2026-09-16 / Claude
- Decision: keep the synchronous discovery as the cold fallback instead of failing a call or serving a stale answer.
  Rationale: the fallback preserves today's behaviour and today's truthfulness in every case a warm answer is unavailable (server just started, probe failed, configuration just changed). The alternative — answering "warming" and asking the model to retry — would turn a latency bug into a correctness bug for the first call of every daemon lifetime.
  Date/Author: 2026-09-16 / Claude
- Decision: remember a capability the fallback discovered, so a profile whose background probe failed heals on first use instead of staying slow until a restart.
  Rationale: the fallback already produced the same value the background pass would have produced, under the same configuration generation. Recording it makes the second and later calls fast, and it costs one map insert.
  Date/Author: 2026-09-16 / Claude
- Decision: cache successful discoveries only; a failed probe leaves the profile cold and is reported as a warning.
  Rationale: the existing database cache has the same rule — a failed probe is not stored, so the next attempt retries. Caching an error would freeze a transient harness failure into the tool's answer until the configuration changed or the daemon restarted.
  Date/Author: 2026-09-16 / Claude
- Decision: the catalogue derives its candidate list from the configuration the daemon last adopted, not from a fresh read of `config.toml` on every call.
  Rationale: the reload pipeline already publishes configuration changes to the daemon every 500 ms, and every other surface — the quota poller, the dashboard, the session list — reads that same adopted configuration. Taking the candidate list from it makes one `list_profiles` answer consistent with what the rest of the daemon believes, instead of a file read that can disagree with the running state; the cost is that a config edit becomes visible to the tool call one reload later, not instantly. A cold catalogue keeps the fresh read.
  Date/Author: 2026-09-16 / Claude

## Outcomes & Retrospective

Implemented and validated on the current branch. `mcp__mj-agents__list_profiles`
now answers from a catalogue the daemon fills in the background: the call path
reads the parent's record, asks the catalogue for the candidates and the
capabilities it holds, ranks them with the quota reports it already had, and
returns. The end-to-end test proves the last part — a counting probe installed
behind the catalogue is called exactly once per enabled profile by the warm
pass, and not again by the tool call — which is the property the request asked
for: the seconds moved from the model's turn to the daemon's start-up.

What is not measured here: a wall-clock comparison against a running daemon
with real harnesses, which needs a machine where the harnesses are installed.
The saving is the discovery time itself, which the tests show the call no
longer pays and which the previous code paid on every cache miss (the SQLite
cache kept that to one miss per profile per 24 hours plus one per daemon
start and per configuration change).

Two things the work turned up. First, a warm pass that is already superseded
must check its generation before it starts each probe, not after a probe
finishes; the first version started every harness and then discarded the
results. Second, the candidate list now follows the configuration the daemon
adopted, which makes a config edit visible to the tool one reload later (the
daemon's own 500 ms cadence) rather than instantly — a deliberate trade that
keeps one `list_profiles` answer consistent with the rest of the daemon, and
one that the cold fallback still avoids when no configuration has been adopted
at all.

## Context and Orientation

Mjolnir has two halves that matter here. The *worker* runs beside the harness (Claude, Codex, Kimi, Grok, or Muse) inside the session's target and serves the harness an MCP server named `mj-agents`; its `list_profiles` tool is declared in `mj-worker/src/subagent_mcp.rs` and forwarded over a Unix socket in the worker root. The *controller* (the `mj` daemon) services that socket from `mj-controller/src/server_runtime.rs`, which spawns `ApiBackend::execute_subagent_tool` for each request and writes the answer back, unblocking the worker and therefore the harness's tool call.

`ApiBackend` lives in `mj-controller/src/server_runtime/api.rs`. Its `ListProfiles` arm currently loads `Config` (the parsed `config.toml`), keeps the enabled profiles that the sub-agent policy admits for the parent session's own profile, ranks them with `select_profile_per_harness`, and then asks `profile_config::discover` for each selected profile. A *profile* is a named harness installation with a home directory and environment; a *parent* is the session whose model called the tool, and eligibility means "the parent's own profile, plus the profiles listed in `[subagents] eligible_profiles`".

`profile_config::discover` (in `mj-controller/src/controller/profile_config.rs`) is the expensive step. It serialises callers per profile, consults a 24-hour SQLite row keyed by a fingerprint of the profile and the pinned harness build, and on a miss runs the worker's `discover-config` subcommand, which stages a private copy of the harness home and starts the harness to ask for its model and effort choices. That is the multi-second work this plan moves off the tool call.

Two existing mechanisms are reused rather than invented. `ApiBackend` already receives `quota_reports`, a map from profile id to the latest quota report used by `select_profile_per_harness`; it stays a call-time input, because quotas change while the catalogue does not. And the daemon already reloads the configuration continuously: `Controller::load` runs on the daemon side every 500 ms, publishing a revision when the configuration changed, and the phone server's control loop reloads its own controller when it sees a revision. The reload arm in `run_server` is where the newly loaded `controller.config` becomes visible, so that is where the catalogue is told about configuration changes.

Terms used below: a *warm pass* is one background traversal that discovers the capabilities of every profile the catalogue should hold; a *generation* is the integer that identifies one configuration's pass, so a pass that finishes after the configuration changed discards its results; a *candidate* is one profile id and harness kind that `list_profiles` may offer a given parent; and a *capability* is the `ProfileConfig` value — default model, model list, and effort list — that the harness reports for a profile.

## Plan of Work

Create `mj-controller/src/server_runtime/profile_catalog.rs`. It defines a `candidates(profiles, subagents, parent)` free function that is the single implementation of the candidate filter; `ApiBackend`'s cold path and the catalogue both call it so the two answers cannot drift. It defines `ProfileCatalog`, which owns a mutex-guarded `Inner` (current generation, the generation of the pass in flight, the configuration inputs the current generation was built from, and the discovered capabilities keyed by profile id), a cancellation token, and an injectable probe function whose production value is `profile_config::discover(profile, None, false)`.

`ProfileCatalog::sync(&self, config: &Config)` is the only entry point the daemon needs. It compares the profiles and the sub-agent policy with the inputs of the current generation and returns immediately when they are unchanged; otherwise it bumps the generation, drops the published capabilities and candidates, and spawns the warm pass for that generation. `ProfileCatalog::view(&self, parent: &str)` returns a `CatalogView` — the generation, the parent's candidates, and the capabilities already discovered for them — or `None` while the catalogue is cold. `ProfileCatalog::discover_profile(&self, profile, generation)` is the fallback the API calls when a selected profile is not in the view: it runs the probe and, when it was given a generation that is still current, remembers the result.

The warm pass loads nothing: the inputs it needs (profiles and sub-agent policy) are already in the generation's key. It probes every enabled profile concurrently in a `JoinSet` — the same shape `QuotaManager::refresh_profiles` uses — inserting each success into the current generation and logging a warning for each failure. It stops early when the generation it belongs to has been superseded or when its cancellation token fires, and it shuts the remaining probes down rather than leaving them detached.

In `mj-controller/src/server_runtime/api.rs`, the `ListProfiles` arm asks the catalogue for a view of the parent's profile. With a view it uses the catalogue's candidates, ranks them with the live quota reports exactly as before, and takes each selected profile's capabilities from the view; only for a profile the view does not hold does it call `discover_profile` (recording the result for later calls). Without a view — a cold catalogue — it keeps today's code path, computing the candidates with the same shared filter after loading the configuration. `ApiBackend` gains a `profile_catalog` field, defaulting to a cold catalogue so existing tests are unaffected, and a `with_profile_catalog` builder used by the daemon and by the new test.

In `mj-controller/src/server_runtime.rs`, `run_server` creates the catalogue with a child of its termination token, warms it from the controller it just loaded, installs it on the `ApiBackend`, and calls `sync` again in the `ControllerReloaded` arm after `controller = reloaded` — the one place where a newly loaded configuration becomes visible.

Register the module in `mj-controller/src/server_runtime.rs` beside the existing `mod api;`.

## Concrete Steps

Work in the repository root, `/workspace/hel`, on the current branch. Read `.agents/PLANS.md` first if you have not.

1. Add the module file and register it:

        mj-controller/src/server_runtime/profile_catalog.rs   (new)
        mj-controller/src/server_runtime.rs                  (add `mod profile_catalog;`)

2. Change the tool path and its wiring:

        mj-controller/src/server_runtime/api.rs              (catalogue field, ListProfiles arm)
        mj-controller/src/server_runtime.rs                  (create, warm, sync, install)

3. Run the focused tests while iterating, then the full suite and the linter. The suite needs real loopback sockets, so run everything outside the restricted sandbox:

        cargo test -p mj-controller profile_catalog
        cargo fmt --all -- --check
        cargo test
        cargo clippy --all-targets -- -D warnings

    Expect the focused run to print the new tests passing and the full run to report the whole workspace passing, with clippy reporting no warnings.

4. Commit the validated change on the current branch, staging only the files this plan touched:

        git add mj-controller/src/server_runtime/profile_catalog.rs \
                mj-controller/src/server_runtime/api.rs \
                mj-controller/src/server_runtime.rs \
                .agents/plans/instant-list-profiles.md
        git commit -m "Answer list_profiles from a background-warmed profile catalogue"

## Validation and Acceptance

The behaviour to accept is: with a warm catalogue the tool call performs no discovery, and a configuration change invalidates and re-warms it.

Unit tests in `mj-controller/src/server_runtime/profile_catalog.rs` use a hand-written probe whose calls the test counts, so "no discovery happened" is observable:

    a_warm_pass_discovers_every_enabled_profile_for_every_parent
    a_warm_pass_probes_nothing_when_the_sub_agent_policy_is_disabled
    a_configuration_change_invalidates_and_re_warms_the_catalogue
    a_pass_of_a_superseded_configuration_publishes_nothing
    a_failed_probe_leaves_the_profile_cold_and_the_next_lookup_remembers_it
    a_discovery_of_a_superseded_generation_is_dropped

The end-to-end test in `mj-controller/src/server_runtime/api.rs`, `list_profiles_answers_from_the_warm_catalogue_without_probing_again`, builds an `ApiBackend` whose exports return one parent session record, installs a catalogue warmed through a counting fake probe, and calls `execute_subagent_tool` with `SubagentToolAction::ListProfiles`. It asserts the answer lists one profile per harness with the fake's default model, and that the probe count after the call is exactly the number of profiles the warm pass discovered — a second discovery inside the call would raise it.

Manual acceptance, for a machine with a configured daemon and at least one Claude or Codex profile: start `mj`, open a session whose profile has sub-agents enabled, and ask the model to call `list_profiles`. The call should return quickly with the same profile list as before. Editing `config.toml` to add or disable a profile should make the next call reflect the change without restarting the daemon.

## Idempotence and Recovery

Every step is additive and repeatable. `sync` is safe to call on every controller reload: it does nothing unless the profiles or the sub-agent policy changed. Superseding a pass is safe in both directions — the superseded pass discards its results because its generation no longer matches, and the new pass re-probes from scratch. If the change has to be reverted, deleting the module, the `ListProfiles` arm's catalogue use, and the three `sync`/install lines restores the previous behaviour exactly, because the cold fallback is the previous code path.

In-flight probes are bounded by `profile_config`'s own cancellation and deadlines, and the daemon already calls `profile_config::cancel_all()` on shutdown; the pass adds a cancellation check between probes so a dying server does not start new ones.

## Artifacts and Notes

The focused run, from `/workspace/hel`:

    $ cargo test -p brokk-mj-controller profile_catalog
    running 6 tests
    test server_runtime::profile_catalog::tests::a_warm_pass_discovers_every_enabled_profile_for_every_parent ... ok
    test server_runtime::profile_catalog::tests::a_warm_pass_probes_nothing_when_the_sub_agent_policy_is_disabled ... ok
    test server_runtime::profile_catalog::tests::a_configuration_change_invalidates_and_re_warms_the_catalogue ... ok
    test server_runtime::profile_catalog::tests::a_pass_of_a_superseded_configuration_publishes_nothing ... ok
    test server_runtime::profile_catalog::tests::a_failed_probe_leaves_the_profile_cold_and_the_next_lookup_remembers_it ... ok
    test server_runtime::profile_catalog::tests::a_discovery_of_a_superseded_generation_is_dropped ... ok
    test result: ok. 6 passed; 0 failed

    $ cargo test -p brokk-mj-controller list_profiles
    running 1 test
    test server_runtime::api::tests::list_profiles_answers_from_the_warm_catalogue_without_probing_again ... ok
    test result: ok. 1 passed; 0 failed

The whole workspace, then the linter, both from `/workspace/hel`:

    $ env -u CODEX_HOME cargo test -- --test-threads=16
    ...
    test result: ok. 1260 passed; 0 failed; 7 ignored    (mj-controller)
    test result: ok. 517 passed; 0 failed                (mj-chat, first of 31 binaries)
    ...
    $ echo $?
    0
    $ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
    $ cargo fmt --all -- --check
    $ echo $?
    0

Why the run unsets `CODEX_HOME`, recorded so a later reader does not mistake it
for a regression: in this container `CODEX_HOME` points at a Mjolnir profile
whose `config.toml` sets `model_catalog_json`, and two `setup::tests` cases load
the ambient configuration and fail on the key this build rejects. They pass as
soon as the variable is unset, and neither they nor the code that reads that
key is touched by this plan.

Why the suite runs at sixteen test threads, and why that is not a workaround
for this change: `nproc` reports the host's 96 CPUs while this container's
cgroup quota is 24, so libtest's default of one thread per CPU oversubscribes
it fourfold. Under that load the `codex_usage::tests` cases that write a fake
`codex` script and exec it fail intermittently — `Text file busy (os error 26)`
from a concurrent fork, or a status assertion that sees the failed spawn — and
they fail on the *base commit* too: three runs of
`cargo test -p brokk-mj-controller --lib` stashed clean of this work failed
twice, in `refresh_discards_client_when_app_server_is_unsupported` and
`refresh_uses_one_initialized_client_for_repeated_queries`, while the same
binary passes every time at a bounded thread count and the module passes in
isolation (`cargo test -p brokk-mj-controller --lib codex_usage`, 18 passed).
The race lives in that module's fixtures and predates this plan; this change
adds no process to the suite.

## Interfaces and Dependencies

In `mj-controller/src/server_runtime/profile_catalog.rs`, define:

    /// The profiles one parent may delegate to, in configuration order.
    pub(crate) fn candidates(
        profiles: &BTreeMap<String, HarnessProfile>,
        subagents: &SubagentConfig,
        parent: &str,
    ) -> Vec<(String, HarnessKind)>

    /// Discovers one profile's capabilities; production discovers through the
    /// shared per-profile discovery, tests substitute a fake.
    pub(crate) type Probe =
        dyn Fn(String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;

    pub(crate) struct ProfileCatalog { /* mutex-guarded inner state */ }

    /// One consistent answer for a parent profile.
    pub(crate) struct CatalogView {
        pub(crate) generation: u64,
        pub(crate) candidates: Vec<(String, HarnessKind)>,
        pub(crate) configs: BTreeMap<String, ProfileConfig>,
    }

    impl ProfileCatalog {
        /// Build the catalogue the daemon serves; probes through the shared
        /// per-profile discovery.
        pub(crate) fn new(cancellation: CancellationToken) -> Arc<Self>;

        /// Adopt the configuration and warm it when it changed. Returns
        /// immediately; the pass runs in the background.
        pub(crate) fn sync(self: &Arc<Self>, config: &Config);

        /// The warm answer for a parent, or `None` while the catalogue is cold.
        pub(crate) fn view(&self, parent: &str) -> Option<CatalogView>;

        /// Discover one profile now, remembering it for the given generation.
        pub(crate) async fn discover_profile(
            &self,
            profile: &str,
            generation: Option<u64>,
        ) -> Result<ProfileConfig>;

        #[cfg(test)]
        pub(crate) fn with_probe(probe: Arc<Probe>) -> Arc<Self>;

        #[cfg(test)]
        pub(crate) async fn sync_now(self: &Arc<Self>, config: &Config);
    }

In `mj-controller/src/server_runtime/api.rs`, `ApiBackend` gains:

    pub fn with_profile_catalog(self, profile_catalog: Arc<ProfileCatalog>) -> Self;

and a private helper for the cold path, which is the code the `ListProfiles`
arm used to hold inline:

    /// The profiles a parent may delegate to, for a catalogue that has not
    /// adopted a configuration yet. Reads the file on a blocking thread.
    async fn list_profile_candidates(&self, parent: &str) -> Result<Vec<(String, HarnessKind)>>;

Change note (2026-09-16): initial version of the plan, written before implementation.
Change note (2026-09-16): the catalogue's items are crate-visible rather than
public — the module itself is private to `server_runtime` — and the cold path
moved into `ApiBackend::list_profile_candidates`.
