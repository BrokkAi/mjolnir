# Support Muse Spark in Anvil and Mjolnir

This ExecPlan is maintained according to `.agents/PLANS.md`. It covers the sibling Anvil checkout at `/home/jonathan/Projects/anvil` and this Mjolnir checkout.

## Purpose / Big Picture

Anvil callers will be able to select `meta::muse-spark-1.3` using their normal Muse login. Mjolnir will use the newest regular Muse Spark model as its second utility choice after Codex Luna, subject to existing quota health and exhaustion rules. The user requested live inference verification, an Anvil release, and pushes of both repositories; they explicitly authorized sending the saved Muse token to Meta's API.

## Progress

- [x] (2026-09-08) Inspect both repositories and native authentication; verify the Meta model catalog and a schema-constrained Spark inference live.
- [x] (2026-09-08) Implement Anvil's Meta client and routing; six local tests and the live prefixed structured-inference test pass. Full release gates are running.
- [x] (2026-09-08) Mj utility unit tests and live Muse compaction pass against the local release client; Spark 1.3 returned a 107-byte structured snapshot.
- [x] (2026-09-08) Prepare, validate, push, and publish all three Anvil 0.28.2 crates in version lockstep (trusted publication run 34277179722).
- [x] (2026-09-08) Cross-platform CI passed, v0.28.2 was tagged, and GitHub, npm (34281751313), and PyPI (34282247064) publication succeeded.
- [x] (2026-09-08) Consume registry client 0.28.2 in Mjolnir; full tests, Clippy, licenses, and live compaction pass (120-byte Spark 1.3 snapshot).
- [x] (2026-09-08) Commit and push Mjolnir's Muse utility update as d324aaa0.

## Surprises & Discoveries

The native catalog includes both regular Spark and contributor variants, plus image and voice models. Utility selection must choose regular Spark rather than sorting the contributor suffix above it. The catalog currently reports IDs only; do not invent context-window or reasoning metadata.

Initial broad validation failed when WSL's init.scope exhausted its 32,768-task limit. The host had accumulated many old test daemons. After the user stopped hel, 90 confirmed deleted-fixture mj daemons were terminated, reducing tasks from roughly 17,400 to 6,800. Validation resumed with CARGO_BUILD_JOBS=4, RUST_TEST_THREADS=4, and TOKIO_WORKER_THREADS=4. Anvil's entire workspace suite then passed; this infrastructure failure was not accepted as a code test result. The user separately requested a subsequent fix and push for test-daemon leakage, after the Muse work.

Muse uses a standard native credential exchange: `providers.meta.access_token` in its XDG configuration `muse/auth.json` authenticates `POST https://api.meta.ai/muse-code/key` with `{"onboard":false}`. Its returned `api_key` authenticates the normal model and Responses endpoints. A live `muse-spark-1.3` response with low reasoning and a strict JSON schema returned `{"ok":true}` and `response.completed`. No CLI scraping or invented credential format is necessary.

## Decision Log

Use Anvil's existing Responses request builder, streaming parser, retry classification, and cancellation machinery. The authentication exchange is specific to Meta; retain the minted API key in memory and remint once after rejection, without rewriting native credentials. This follows the observed native flow and prevents a parallel protocol implementation.

Keep quota health ahead of provider preference in Mjolnir. Within a quota class the order becomes Codex, Muse, Grok, Kimi, DeepSeek; exhausted candidates remain excluded. This adds the requested preference without discarding established quota protection.

Publish Anvil 0.28.2 as the next patch release, including all three lockstep Rust packages and the Python launcher version. Mjolnir will depend on the published registry version, not a permanent local patch. The user requested no Mjolnir release.

The saved Cargo token cannot inspect trusted-publisher configuration (HTTP 403 for insufficient token scope). After all local release gates pass, run the existing publish-crate workflow manually with publish=true while master CI runs. Successful OIDC publication verifies authorization for all three crates directly. The tag must wait for successful publication and master CI. The tag-triggered workflow can safely skip the immutable versions already published. No owners or publisher settings are changed.

## Context and Orientation

Anvil's `crates/anvil-client/src/meta_client.rs` will implement `LlmBackend`, the interface for model discovery and streamed inference. `MultiBackend` routes provider-qualified IDs such as `meta::muse-spark-1.3` by stripping the prefix before calling the provider. The source registry constant lives in `discovery.rs`, and `src/main.rs::build_multi_backend` constructs Anvil's usable providers.

Mjolnir's `mj-controller/src/hel_utility_llm.rs` resolves model candidates, ranks quota and provider preferences, and requests schema-constrained compaction snapshots. Its existing Muse quota collection is already implemented and live-verified. The workspace dependency in `Cargo.toml` currently uses `brokk-anvil-client` 0.28.1.

## Plan of Work

Milestone one adds the standalone Meta backend and native home configuration, registers the `meta` provider in Anvil, and documents native login discovery. Local HTTP fixtures must prove bearer exchange, source routing, streamed structured output, and rejected-key recovery without credential persistence. An ignored opt-in live test must exercise the prefixed model through `MultiBackend` and `infer_structured` with synthetic content only.

Milestone two updates Anvil root, minimizer, client, root dependency requirements, lockfile, Python launcher, and generated legal notices to 0.28.2. Validate the gates in `.github/workflows/ci.yml`, `publish-crate.yml`, and `release.yml` before tagging. Push the current master branch, verify CI, then push the new immutable version tag and monitor crate and platform publication. Never move an existing tag or republish an immutable version.

Milestone three adds the Muse backend and Spark family selection to Mjolnir. Update the registry dependency after publication, use actual discovered metadata, and retain cancellation and quota ordering. Add a dedicated live test selected by a configured Muse profile and prove a real structured compaction result. Validate and commit on the current branch, then push to its configured upstream, merging remote changes if needed without rebasing or forcing.

## Concrete Steps

In Anvil, run focused Meta tests and the ignored live Meta test with the native Muse login. Then run `cargo fmt --check`, `cargo test --workspace`, both default and ACP-only Clippy, `cargo build --release`, Python version/tests, documentation checks/build, license checks, and client/minimizer package dry runs. Record exact live-test names and publication IDs here as they become available.

In Mjolnir, run the focused utility tests and `MJ_UTILITY_LIVE_MUSE_PROFILE=<configured Muse id> cargo test -p brokk-mj-controller utility_llm_live_muse -- --ignored --nocapture`. Complete `cargo test` and `cargo clippy --all-targets -- -D warnings` before pushing. Run Cargo outside the restricted sandbox, using normal build storage and no Cargo output under `/tmp`.

## Validation and Acceptance

The prefixed Anvil live request must return valid JSON from Spark. Mjolnir must discover a regular Spark candidate with Muse credentials and produce a nonempty schema-validated snapshot. Unit tests must show Muse below Luna but above Grok for equal quota health, retain exhausted exclusion and quota-class ordering, and reject contributor/image/voice IDs for utility selection. Logs must contain no credentials, and native authentication files must remain unchanged.

## Idempotence and Recovery

Local checks and synthetic live tests may be rerun. Release retries must first inspect existing tags, published versions, and workflow status; retry failed workflows rather than moving tags or uploading duplicate versions. Preserve unrelated working-tree changes and commit only this task's files. Read the host NFS runbook before investigating stalled shared build storage.

## Interfaces and Dependencies

Expose `anvil_client::meta_client::{MetaClient, MetaClientConfig}`. Configuration contains `auth_path`, `base_url`, and `mint_base_url`; `from_home` resolves a Muse profile directory. `load` discovers the native XDG home, while `load_with_config` permits explicit profile isolation and returns an optional shared `LlmBackend`. Authentication and inference network operations are asynchronous. The implementation uses existing Anvil dependencies and transport helpers.

## Artifacts and Notes

Live protocol proof: `muse-spark-1.3` returned `{"ok":true}` with `response.completed` using native access-token exchange and strict JSON schema.

## Outcomes & Retrospective

Anvil implementation and all local release gates passed. Commit 0dda93f was pushed to master; trusted publication run 34277179722 published all three crates, and master CI 34276918491 passed before tagging v0.28.2. Tag CI 34279410346, Docs 34279410361, and idempotent crate publication 34279410374 passed. Release run 34279410324 built all five platform bundles and published https://github.com/BrokkAi/anvil/releases/tag/v0.28.2; npm 34281751313 and PyPI 34282247064 publication succeeded.

Both the prefixed client test and actual CLI returned valid live JSON without modifying native authentication. Mj's manifest, lockfile, and license report use registry client 0.28.2. Its complete registry-based suite, Clippy, focused ordering tests, and final live structured compaction passed. The final live run returned a 120-byte Spark 1.3 snapshot. Mj's Muse integration was committed and pushed as d324aaa0. The separately requested daemon cleanup fix was subsequently pushed as 63b2ec37.

Revision note: initial plan records the confirmed native authentication flow, provider boundaries, release ordering, and acceptance criteria.
