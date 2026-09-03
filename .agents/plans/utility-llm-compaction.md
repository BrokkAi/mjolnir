# Implement utility-model compaction through brokk-anvil-llm

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept current in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Cross-harness resumes currently ask the destination harness to summarize the old transcript through an ACP scratch session. After this work, Mjolnir will instead use a direct, tool-free Anvil inference client and configured utility profiles. It will dynamically select the newest Codex Luna, Grok, Kimi, or DeepSeek Flash model using quota health and the stated provider precedence, summarize independent pages concurrently on the selected profile, and fail over individual requests when necessary. Claude credentials will never be considered for utility work. A manual ignored test will prove all four real provider paths.

## Progress

- [x] (2026-09-02 00:00Z) Inspected both repositories, current compaction/relay/quota paths, Anvil infer code, provider authentication, and release requirements.
- [x] (2026-09-03 00:40Z) Added reusable structured inference and explicit profile authentication APIs to `brokk-anvil-llm`.
- [x] (2026-09-03 01:20Z) Validated, committed, pushed, tagged, and published Anvil 0.27.1, including `brokk-anvil-llm`.
- [x] (2026-09-03 00:55Z) Added Mjolnir utility-profile discovery, quota ranking, caching, and direct Anvil backends.
- [x] (2026-09-03 00:58Z) Parallelized compaction with ordered concurrency two and removed the ACP compaction protocol.
- [x] (2026-09-03 01:01Z) Added focused automated tests and the ignored four-profile live test plus manual runner.
- [x] (2026-09-03 02:15Z) Updated licensing and documentation; regenerated notices; passed formatting, license policy, full tests, and all-target Clippy against the published crate.
- [x] (2026-09-03 02:18Z) Ran the ignored live test concurrently against all four real profiles: `gpt-5.6-luna`, `grok-4.6`, `k3-256k`, and `deepseek-v4-flash-vision-exp`.
- [x] (2026-09-03 02:20Z) Kept hours and minutes in the five-hour quota reset countdown, including the requested `4h50m` rendering, with focused behavior tests.

## Surprises & Discoveries

- Observation: Anvil's CLI already implements tool-free structured inference, but the reusable logic is in the root binary rather than `brokk-anvil-llm`.
  Evidence: `/home/jonathan/Projects/anvil/src/infer.rs` constructs `StreamChatRequest { tools: None, structured_output: Some(...) }` directly.
- Observation: Mjolnir's quota cache currently lives in UI pollers, while resume runs inside the daemon.
  Evidence: `mj-cli/src/pollers.rs` owns `QuotaManager`; `mj-cli/src/daemon.rs` loads a controller inside each lifecycle operation.
- Observation: both existing compaction and Anvil infer contain duplicated adaptive/retry branches that this refactor can remove at their source.
- Observation: carrying Bedrock credit support in the LLM crate's default graph imposed the AWS SDK on utility-only consumers.
  Evidence: `brokk-anvil-llm` now has an empty default feature set; `bedrock-credits` owns the five optional AWS dependencies and Anvil opts in explicitly.
- Observation: Mjolnir's final registry-backed license validation cannot resolve `brokk-anvil-llm` 0.27.1 before that release is published.
  Evidence: publishing 0.27.1 unblocked normal locked dependency resolution; final Mjolnir tests and Clippy use the crates.io package with no path patch.
- Observation: DeepSeek's JSON response mode rejects a request unless the prompt explicitly contains the word `JSON`.
  Evidence: the first live run returned HTTP 400 for `deepseek-v4-flash-vision-exp`; adding an explicit JSON-object instruction made all four concurrent live requests pass.
- Observation: the supplemental notice generator retained the pre-2.0 `src/fonts` path after the fonts moved into `mj-controller/src/fonts`.
  Evidence: the required generator failed with `ENOENT`; updating both its inventory path and emitted source path restored deterministic generation.

## Decision Log

- Decision: Production schedules at most two concurrent requests on the highest-ranked profile; lower-ranked profiles are failover-only.
  Rationale: This is the user's chosen scheduling policy and avoids consuming lower-priority quotas during healthy operation.
  Date/Author: 2026-09-02 / Codex
- Decision: Quota health sorts before provider precedence, with usage-priced DeepSeek considered healthy, exact-zero windows excluded, reserve windows retained, and unknown reports attempted last.
  Rationale: This implements the requested quota protection while remaining provider-aware when a provider reports fewer windows.
  Date/Author: 2026-09-02 / Codex
- Decision: ACP may be used by the existing Grok billing-only quota probe, but never for utility inference or transcript content.
  Rationale: The user explicitly limited the ACP prohibition to inference and allowed cached quota.
  Date/Author: 2026-09-02 / Codex
- Decision: The four-provider live test is ignored and manually invoked.
  Rationale: It requires real credentials, network access, and paid quota and therefore must not run in CI.
  Date/Author: 2026-09-02 / Codex
- Decision: Bedrock credit support is opt-in rather than a default feature of `brokk-anvil-llm`.
  Rationale: utility inference does not use Bedrock and should not pull the AWS SDK; the Anvil application explicitly enables the feature because it owns the Bedrock credit surface.
  Date/Author: 2026-09-03 / Codex
- Decision: Local cross-repository development uses Cargo's command-line `patch.crates-io` configuration only.
  Rationale: committed manifests must be portable and must never contain a developer-specific `/home/jonathan` path.
  Date/Author: 2026-09-03 / Codex

## Outcomes & Retrospective

The utility-model path is complete and validated. Cross-harness compaction now runs directly through Anvil without tools or ACP sessions, selects only Luna/Grok/Kimi/DeepSeek Flash from live catalogs, honors quota-aware precedence, and runs independent compaction work with bounded parallelism. Claude-only configurations correctly have no utility model. Anvil 0.27.1 is public, Bedrock dependencies are opt-in, and Mjolnir resolves the published crate without a machine-local path. The manual paid test passed all four configured profiles and remains ignored by ordinary CI commands.

## Context and Orientation

`mj-controller/src/hel_compaction.rs` turns an archived canonical transcript into an at-most-8-KiB handoff. `mj-controller/src/hel_controller/resume.rs` currently performs that work after starting the destination worker by sending a `Compact` request through the relay. The relay request is declared in `src/hel_worker.rs`, implemented in `src/hel_acp.rs`, routed by `mj-worker`, and called by `mj-controller`.

Profiles are configured by harness kind and home directory. `mj-controller/src/hel_quota.rs` can query their quota, but long-lived reports are currently cached by `mj-cli/src/pollers.rs`. A utility profile is a configured Codex, Grok, Kimi, or DeepSeek profile whose credentials can create an Anvil backend and whose live model catalog contains the requested family. Claude is deliberately absent.

The sibling repository `/home/jonathan/Projects/anvil` publishes `brokk-anvil-llm`. Its root `src/infer.rs` is a one-shot CLI adapter; `crates/anvil-llm` owns the actual clients. Codex, Grok, and Kimi currently resolve authentication partly through process-global environment variables, which is unsafe when one Mjolnir daemon serves several profiles concurrently.

## Plan of Work

First, move the generic inference loop into `crates/anvil-llm/src/infer.rs`. Expose system/user-only messages, schema-constrained requests, options, usage-bearing responses, cancellation, and stable error categories. The API must have no tools field and must always send `tools: None`. Refactor the root CLI adapter to use it without changing CLI JSON.

Add explicit authentication locations to the Codex client, Grok client configuration, and Kimi backend configuration. Every credential refresh must read and atomically write the configured profile file. Keep environment-resolving constructors as wrappers so existing Anvil behavior is unchanged. Add concurrent isolation tests. Then bump every Anvil package/binding to 0.27.1, validate per Anvil's repository instructions, commit, push, tag `v0.27.1`, monitor publication, and verify the crate is available.

In Mjolnir, add `brokk-anvil-llm = "0.27.1"` to `mj-controller`. Create a utility runtime that caches quota reports in the daemon and constructs isolated profile backends for live discovery. Reports remain fresh for twenty minutes; missing or stale supported profiles refresh through existing quota adapters. Candidate discovery queries each profile's Anvil model catalog. Model families are Codex `gpt-*` containing `luna`, any `grok-*`, Kimi `kimi-*` or `k` followed by a digit, and DeepSeek `deepseek-*` containing `flash`. Dynamic newest selection ranks `latest` and `next` aliases first, numeric components descending next, and the complete ID last.

Quota categories are healthy, reserve, unknown, and excluded. API-priced profiles are healthy. Percentage-based profiles are healthy only when every reported percentage exceeds ten, reserve when the minimum is one through ten, unknown when reports or percentages are unavailable, and excluded when any reported percentage is zero. Sort category first, provider precedence Codex/Grok/Kimi/DeepSeek second, same-kind minimum quota descending third, and profile ID last. Never enumerate or read Claude authentication.

Refactor compaction to accept a thread-safe backend and process independent pages and each reduction level with ordered concurrency capped at two. Each item starts on the top candidate. Authentication, rate-limit, and exhausted provider failures disable that candidate for the job; structured-output errors fail over only the item; context-length errors try all candidates before adaptive splitting. Cancellation terminates all requests. Perform compaction before destination state mutation or provisioning, then install only the final prompt context through the real relay.

Remove the ACP compaction configuration, command, relay request/response variants, worker routing, and tests. Preserve unrelated scratch-session behavior and `install_prompt_context`.

Add an ignored live test and `scripts/test-utility-llm-live.sh`. Four environment variables identify real configured Codex, Grok, Kimi, and DeepSeek profiles. The test validates kinds, quota, and discovered models, then concurrently runs one forced single-request compaction through every profile and prints only safe metadata. It finally checks normal resolver ordering. CI compiles but never executes this test.

Finally update user documentation and LGPL license policy/notices, run Mjolnir's required elevated tests, clippy, formatting, and license checks, then commit only the changed files on the current branch and push it as subsequently requested. Do not tag or release Mjolnir.

## Concrete Steps

In `/home/jonathan/Projects/anvil`, edit the library and root adapter, then run:

    cargo fmt --check
    cargo test --all
    cargo clippy --all-targets --all-features -- -D warnings
    cargo build --release
    python python/scripts/check_version.py

Run the remaining release and license commands prescribed by Anvil's `CONTRIBUTING.md`, commit the clean tree, push it, tag/push `v0.27.1`, and verify publication.

In `/home/jonathan/Projects/hel2`, implement the integration and run outside the restricted sandbox:

    cargo fmt --check
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo deny --workspace --config licenses/deny.toml --locked check licenses

Regenerate and compare cargo-about and supplemental notices using the commands in `.github/workflows/ci.yml`. Build the worker explicitly for the required musl targets.

The manual live test is invoked only by a human with four profile IDs:

    MJ_UTILITY_LIVE_CODEX_PROFILE=... \
    MJ_UTILITY_LIVE_GROK_PROFILE=... \
    MJ_UTILITY_LIVE_KIMI_PROFILE=... \
    MJ_UTILITY_LIVE_DEEPSEEK_PROFILE=... \
    scripts/test-utility-llm-live.sh

## Validation and Acceptance

Automated tests must prove dynamic family selection, quota ordering, Claude exclusion, cache freshness, concurrency capped at two, ordered reductions, adaptive splitting over inputs larger than 64 KiB, item-level failover, cancellation, redacted errors, and removal of the relay compaction operation. Same-harness resume tests must remain unchanged in behavior.

The ignored live test passes only when all four named real profiles discover a matching current model and return a valid structured handoff. Exhausted quota, missing authentication, missing models, or inference failure is a test failure rather than a skip. No normal CI command executes it.

## Idempotence and Recovery

All generated license/version commands are repeatable. Credential tests use isolated temporary paths. Live tests read and may refresh real credentials but never print secrets. If Anvil publication fails, fix and retag only according to its release policy; do not point committed Mjolnir code at an unpublished path dependency. Existing user changes must remain untouched, and commits stage explicit paths only.

## Artifacts and Notes

The primary artifacts are the new Anvil public infer/profile APIs, Anvil release 0.27.1, Mjolnir's utility runtime and concurrent compactor, the deleted ACP compaction protocol, the ignored four-provider live test, and regenerated license reports.

## Interfaces and Dependencies

`anvil_llm::infer::infer_structured` accepts `&dyn LlmBackend`, a structured request, and a `CancellationToken`; it returns a structured response or typed inference error. Its request type cannot contain tools.

`CodexClient::with_auth_path`, `GrokClientConfig`, and `KimiBackendConfig` provide explicit profile-local authentication. Default constructors retain existing environment behavior. `brokk-anvil-llm` has no default features; consumers that call its Bedrock credit probe enable `bedrock-credits` explicitly.

Mjolnir's `UtilityLlmRuntime` owns the process-local quota cache and constructs profile-local backends. The compaction backend becomes `Send + Sync` and callable through shared references. Existing resume entry points remain source-compatible through an ephemeral default runtime, while the daemon uses its persistent runtime.

Revision note (2026-09-02): Created the implementation plan from the approved design and added the user's requirement for a manually invoked four-profile live test.

Revision note (2026-09-03): Recorded the opt-in Bedrock feature decision, portable command-line-only local patching, completed implementation milestones, and the release-publication gate.

Revision note (2026-09-03): Recorded publication, final registry-backed validation, the four-profile live result, DeepSeek JSON compatibility, the moved-font audit repair, and the follow-up five-hour countdown behavior.
