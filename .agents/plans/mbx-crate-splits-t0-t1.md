# Speed Up `mbx` Builds with Tier 0 Feature/Edge Cleanup and Tier 1 Crate Splits

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan follows `.agents/PLANS.md` and covers the user-approved **Tier 0 + Tier 1** work to make common edit-test-clippy loops on Mjolnir substantially faster under `mbx` (`v1.30.0`). Tier 2 and Tier 3 splits are explicitly out of scope unless requested later, and host `mbx` configuration / binary upgrades are out of scope (waiting for the next `mbx` release).

## Purpose / Big Picture

Every edit in Mjolnir is currently built under `mbx 1.30.0`, which caches per-crate `rustc` invocations keyed by source files, compiler flags (`--cfg` feature sets), and exact upstream `.rlib`/`.rmeta` digests. On `rustc 1.99.1`, `.rmeta` early cutoff does not fire across private function-body edits because the Strict Version Hash (SVH) changes on every edit, and `mbx 1.30.0` runs with `CARGO_INCREMENTAL=0`. Consequently, the wall-clock cost of an edit is dominated by three things:

1. **Feature-set cache fragmentation:** `mj-core` compiles under **7 distinct `--cfg` sets** across `cargo test -p <crate>`, `cargo clippy`, `mj`, and `mj-worker`, and external dependencies (`tokio`, `serde`, `tokio-util`, `reqwest`, `image`) vary features per package. Switching `-p` flags or toggling between tests and binaries causes `mbx` cache misses across the whole dependency graph.
2. **False workspace edges and a 14-crate serial chain:** Unnecessary edges (`mj-build-cache -> mj-import`, `mj-targets`/`mj-worker-staging -> mj-client`, `mj-store -> mj-review`, `mj-server -> mj-controller`, and dead dev-dependencies) force wide rebuild cascades and prevent parallel compilation.
3. **Oversized hot crates with disjoint subsystems:** The top three crates account for **64.3%** of all uncached workspace Rust compile time across 212 real `mbx` receipts (`mj-core` 28.0%, `mj-worker` 23.9%, `mj-daemon` 12.4%). Moving independent subsystems out of them — and repointing callers directly to the narrower owning crates with **no `pub use` facade re-exports** on the old crate — shrinks both single-crate compile time and reverse-dependency blast radius.

After this plan lands, switching between `-p` packages, `mbx test`, `mbx clippy`, and binary builds will hit warm `mbx` cache entries for `mj-core` and external dependencies; edits to `mj-controller` will stop rebuilding `mj-server`; edits to `mj-worker` runtime/ACP/relay will stop recompiling Nitro microVM, MCP servers, and review logic (and vice versa); edits to harness installation/runtime manifests will stop recompiling the entire 27-crate workspace; and edits to `mj-daemon`'s GitHub watcher will stop recompiling the 40-second `mj-daemon` test harness.

## Progress

- [x] (2026-11-22 11:30Z) Measured 212 real `mbx` receipts, mapped workspace edges, audited all feature flags, verified seam feasibility, and secured user approval for Tier 0 + Tier 1.
- [x] (2026-11-22 12:04Z) Synced local `master` to `origin/master` (`96fb4b6e9`), inspected merged `141592c36` (`MJ_TEST_GLIDE_ENDPOINT` binary check), verified `worker_build::install` single-owner invariant, and authored this ExecPlan.
- [x] (2026-11-22 13:19Z) Milestone 0a: Cut cheap false edges and dead dev-dependencies (0 new crates), including repointing `mj-worker-bin/tests/` from `mj-controller` to versionless `mj-targets` dev-dep (pushed to `origin/master` at `8bc74e2af`).
- [x] (2026-11-22 13:41Z) Milestone 0b (+ Tier 1c): Unified workspace and external dependency features across 12 crates (`axum`, `chrono`, `getrandom`, `libc`, `jsonc-parser`, `serde_json`, `url`, `tokio`, `serde`, `tokio-util`, `reqwest`, `image`), moved `github-history` tokenization/`ettin-tokenizer.json` to `mj-github-relevance`, replaced library `test-hooks`/`test-support`/`test-identity`/`golden` features with strict production defaults + explicit opt-in (`enable_test_fixtures` / `enable_test_hooks`), verified single framed `\0MJ-WORKER-BUILD:` stamp and `MJ_TEST_GLIDE_ENDPOINT` gating on built `mj-worker` binaries, and enforced feature unification in `scripts/check-crate-boundaries.mjs`.
- [ ] Milestone 1d: Decouple `mj-server` from `mj-controller` (0 new crates), validate, review, commit, and push.
- [ ] Milestone 1a + 1e: Extract `brokk-mj-microvm` (`mj-microvm/`), `brokk-mj-worker-mcp` (`mj-worker-mcp/`), and `brokk-mj-worker-review` (`mj-worker-review/`) out of `mj-worker`, register crates on crates.io, validate, review, commit, and push.
- [ ] Milestone 1b: Extract `brokk-mj-harness-install` (`mj-harness-install/`) out of `mj-core`, register on crates.io, validate, review, commit, and push.
- [ ] Milestone 1f: Extract `brokk-mj-github-watch` (`mj-github-watch/`) out of `mj-daemon`, register on crates.io, validate, review, commit, and push.
- [ ] Milestone 2: Measure post-split `mbx` build/test timings, dispatch `.github/workflows/nightly.yml` for macOS/Windows lanes, clean up any temporary worktrees, and record final outcomes.

## Surprises & Discoveries

- **Discovery 1 (`mj_worker` crate-name collision in `mbx` receipts):** Both `brokk-mj-worker-runtime` (`mj-worker/src/lib.rs`) and `brokk-mj-worker` (`mj-worker-bin/src/main.rs`) normalize to crate name `mj_worker` in `mbx` telemetry. Correcting by source root showed `mj-worker` (`mj-worker/src/lib.rs`) is #2 overall at 2,072 s (23.9%), with `mj_worker --test` alone costing 1,102 s.
- **Discovery 2 (`tests/e2e/delegation.py` binary scan for `MJ_TEST_GLIDE_ENDPOINT`):** Commit `141592c36` added an `mmap` check in `tests/e2e/delegation.py` asserting that `mj-worker` built without `--features test-hooks` does not contain the literal bytes `MJ_TEST_GLIDE_ENDPOINT`. Keeping the `"MJ_TEST_GLIDE_ENDPOINT"` literal strictly inside `mj-worker-bin/src/main.rs` under `#[cfg(feature = "test-hooks")]` and passing its value into `mj_core::test_hooks::set_test_glide_endpoint` preserves this contract verbatim while still eliminating the `test-hooks` feature from `mj-worker` and `mj-core`.
- **Discovery 3 (Published libraries must default to strict production behavior):** Any library caller — including an external consumer of published `brokk-mj-*` crates that never calls `mj_core::worker_build::install(...)` — must receive strict production defaults (real disk signing key in `aws_assume_role`, read-only pooled `mj_store` readers, live `mbx::release`, cross-process image pull locks, inert `MJ_TEST_*` hooks, and required `session_id` on `recover_worker_admitted`). Inferring test mode from `!worker_build::is_program_process()` would silently put external library callers into test mode. Instead, every library defaults to production behavior, and tests opt in explicitly via `mj_core::test_hooks::enable_test_fixtures()` (for fixture behaviors) or `mj_core::test_hooks::enable_test_hooks()` (for runtime crash/env hooks; leaf `mj`/`mj-worker` binaries also call this under `#[cfg(feature = "test-hooks")]`).
- **Discovery 4 (`mj-build-id/build.rs` standalone crates.io packaging):** `mj-build-id/build.rs` reads `../mj-core/tests/fixtures/fake-command.sh` to generate `$OUT_DIR/fake-worker.sh`, which is outside the `brokk-mj-build-id` tarball when published. Making `mj-build-id/build.rs` generate `fake-worker.sh` only when `tracked_read(&fixture)` returns `Some(script)` allows removing `test-hooks` from `mj-build-id` while keeping `cargo package -p brokk-mj-build-id` working cleanly.
- **Discovery 5 (`mj-store` uses `mj-transcript::github_history` without `tokenizers`):** The `github-history` feature on `mj-core` and `mj-transcript` only gates `history_tokenizer` and four tokenizer-using functions whose sole callers are `mj-daemon/src/github_watch/service.rs` and `mj-github-relevance/src/github_relevance.rs`. Moving those tokenizer functions and `ettin-tokenizer.json` into `mj-github-relevance` eliminates `github-history` and the `tokenizers` dependency from both `mj-core` and `mj-transcript` with zero new crates.

## Decision Log

- **Decision 1: Unify features by hand in root `Cargo.toml`, not with a `workspace-hack` crate.**
  *Rationale:* The user left this choice to us. A `workspace-hack` crate cannot unify workspace-internal features (`mj-core`'s `test-hooks`, `test-identity`, `golden`, `github-history`, or `mj-store`'s `test-support`) without leaking test hooks into release binaries or creating dependency cycles, and published crates cannot depend on an unpublished workspace-hack crate. Unifying the five varying external dependencies (`tokio`, `serde`, `tokio-util`, `reqwest`, `image`) directly in root `Cargo.toml` `[workspace.dependencies]` and removing internal feature fragmentation solves both problems cleanly with zero extra crates.
- **Decision 2: Strict production defaults everywhere, with explicit opt-in test mode (`enable_test_fixtures` / `enable_test_hooks`) instead of library Cargo features.**
  *Rationale:* Production binaries and any external callers of published `brokk-mj-*` crates must default to read-only database readers, live `mbx::release`, disk-backed AWS cookie signing keys, cross-process image pull locks, strict `session_id` checks on recovery, and inert `MJ_TEST_*` environment variables unless explicitly opted in. Keeping `TEST_FIXTURES_ENABLED` and `TEST_HOOKS_ENABLED` false by default in `mj_core::test_hooks` — enabled only by `mj`/`mj-worker` under `#[cfg(feature = "test-hooks")]` (for hooks) or by test code / shared test fixtures calling `enable_test_fixtures()` / `enable_test_hooks()`, plus `cfg!(test)` inside a crate's own unit tests — preserves fail-safe production behavior for every caller with zero library-level Cargo feature multiplication.
- **Decision 3: Never leave facade re-exports for extracted crates on `mj-core`, `mj-worker`, `mj-controller`, or `mj-daemon` when callers should decouple.**
  *Rationale:* Under `mbx`, a `pub use` re-export keeps the dependency edge alive and negates the rebuild-isolation win. Every caller must depend directly on the narrowest owning crate.
- **Decision 4: Ship in reviewed, pushed checkpoints on `master`, parallelizing disjoint Tier 1 slices in detached worktrees.**
  *Rationale:* Each milestone (0a, 0b, 1d, 1a+1e, 1b, 1f) is a clean, independently verifiable checkpoint. Running disjoint Tier 1 splits in detached worktrees under `~/Projects/mjolnir-wt/` lets children build and test without colliding in the main checkout while keeping `master` clean until integrated review and cherry-pick/merge.

## Context and Orientation

The workspace root is `/home/jonathan/Projects/mjolnir/.mj/clones/d1c57e7f763ff2cbb78e63b20d43a17b` on branch `master` (currently at `96fb4b6e9`). All builds, clippy runs, and tests must invoke `mbx` (`mbx check`, `mbx clippy`, `mbx test`, `mbx build`), never plain `cargo` against an `mbx` target symlink. Non-building metadata/lint tools (`cargo fmt --all -- --check`, `cargo-machete`, `cargo metadata`, `cargo tree`, `cargo package --locked --allow-dirty --list`, `node scripts/check-crate-boundaries.mjs`, `python3 scripts/check-crate-size.py`, `node --test scripts/release-workflow.test.mjs`) may be invoked directly.

Detailed read-only research reports live under `.mj/agents/` in this checkout for reference:
- `.mj/agents/10b2788bb18464d6d45aea4c46e4c83f/mbx-deep-analysis.md` (`mbx 1.30.0` mechanics and 212 receipt timings)
- `.mj/agents/1553c7aae187501e2fefd9f01d1e7ef7/hot-crates-modules.md` (logical module LOC, churn, and caller fan-out)
- `.mj/agents/f75e41108134e075a79d9c611dea869c/chain-and-edges-complete.md` (workspace edge audit and 29-package feature matrix)
- `.mj/agents/9d1a6b53ffe702893af6c322832768cc/seam-feasibility.md` (exact symbol counts and seam specs for Tier 0 and Tier 1)
- `.mj/agents/af1b194e95668e364da81387282695e4/m0-exact-spec.md` (exhaustive `cfg` site inventory, feature forwarding, and workflow lists)

Every new workspace crate added in Tier 1 must satisfy the full publication and CI checklist before pushing:
1. Workspace `Cargo.toml`: `members`, `default-members` (unless optional like `voice-worker`), and `[workspace.dependencies]` pinned to workspace version `2.73.0`.
2. `<crate>/LICENSE` symlink pointing to `../LICENSE`, plus `package` metadata matching sibling crates (`edition = "2024"`, `version.workspace = true`, `rust-version.workspace = true`, `license.workspace = true`, `repository`, `publish = true`, `[lints] workspace = true`).
3. `.dockerignore` (`!<crate>/`) and `containers/Containerfile.agent-dev` (`COPY <crate>/ <crate>/`) when required by `scripts/release-workflow.test.mjs`.
4. `mj-core/worker-build-inputs.txt` and `.github/workflows/publish-agent-dev-image.yml` `paths:` for any crate in `brokk-mj-worker`'s normal/build dependency closure (`mj-microvm`, `mj-worker-mcp`, `mj-worker-review`, `mj-harness-install`).
5. `scripts/check-crate-boundaries.mjs` (`allowed` table) and `scripts/check-crate-size.py` (update `OVER_BUDGET` if any crate drops below 50,000 non-test lines).
6. `licenses/deny.toml` (`{ crate = "brokk-mj-<name>", allow = ["GPL-3.0-only"] }`).
7. `.github/workflows/publish.yml` (topological publish order), `.github/workflows/ci.yml` (`for package in ...` packaging check list), `.github/workflows/windows.yml` (`TEST_PACKAGES` — include cross-platform crates; handle Linux-only `brokk-mj-microvm` cleanly so Windows compiles it or skips Linux-only tests cleanly).
8. `CONTRIBUTING.md` Crate Layout section.
9. Register the new crate name on crates.io via `python3 scripts/register-crate.py brokk-mj-<name> --description "<one-line description>"` right before pushing the validated commit (never earlier, since crates.io reservations are permanent).

## Plan of Work

### Milestone 0a: Cut Cheap False Edges and Dead Dev-Dependencies (0 New Crates)

Remove seven verified false or dead edges to break unnecessary rebuild cascades and eliminate dev-dependency cycles:

1. **Delete dead dev-dependencies:**
   - `mj-chat/Cargo.toml`: remove `mj-worker` dev-dep.
   - `mj-worker/Cargo.toml`: remove `mj-controller` dev-dep.
   - `mj-worker-bin/Cargo.toml`: remove `mj-controller` dev-dep.
2. **Eliminate `mj-worker -> mj-build-cache` dev-dep:**
   - Define `pub const MBX_VERSION: &str = "1.30.0";` in `mj-core/src/lib.rs` (or a small `mj_core::mbx` module/constant — put `MBX_VERSION` in `mj-core/src/config.rs` or `mj-core/src/mbx.rs` so it has a clean home; re-export as `pub use mj_core::mbx::MBX_VERSION;` in `mj-build-cache/src/mbx.rs`).
   - Update `mj-worker/src/microvm/mbx_proxy/tests.rs` to read `mj_core::mbx::MBX_VERSION` and remove `mj-build-cache` from `mj-worker/Cargo.toml` `[dev-dependencies]`.
3. **Cut `mj-build-cache -> mj-import` normal edge:**
   - Replace `crate::import::github_repository_from_origin(source)` in `mj-build-cache/src/git_cache.rs:367` with `mj_core::remote_git::parse_github_remote(source, mj_core::remote_git::Shorthand::Allowed)` (verify identical behavior for all inputs, including shorthand and case handling).
   - Remove `mj-import` from `mj-build-cache/Cargo.toml` (`[dependencies]` and `test-hooks`/`test-support` feature forwarding), remove `pub(crate) use mj_import as import;` (or equivalent) from `mj-build-cache/src/lib.rs`, and remove `'brokk-mj-import'` from `allowed['brokk-mj-build-cache']` in `scripts/check-crate-boundaries.mjs`.
4. **Cut `mj-targets -> mj-client` and `mj-worker-staging -> mj-client` normal edges:**
   - Move `pub fn new_command_id(prefix: &str) -> Result<String>` from `mj-client/src/session.rs:539` into `mj-core` (e.g., `mj_core::command_id::new_command_id` or `mj_core::relay::new_command_id` — check existing command-id types in `mj-core` first!). Re-export `pub use mj_core::...::new_command_id;` in `mj-client/src/session.rs` so existing `mj-client` callers stay intact.
   - Update `mj-targets/src/targets/container_upgrade.rs:619` and `mj-worker-staging/src/worker_binary/{install,store,upgrade}.rs` to call `mj_core::...::new_command_id`.
   - Remove `mj-client` from `mj-targets/Cargo.toml`, `mj-worker-staging/Cargo.toml`, and their `allowed` entries in `scripts/check-crate-boundaries.mjs`.
5. **Demote `mj-store -> mj-review` to dev-dependency:**
   - Move `mj-review` from `[dependencies]` to `[dev-dependencies]` in `mj-store/Cargo.toml` using a versionless path dep (`mj-review = { path = "../mj-review", package = "brokk-mj-review" }`) so `cargo package` strips it cleanly (avoiding crates.io publish cycle).
   - Remove `'brokk-mj-review'` from `allowed['brokk-mj-store']` in `scripts/check-crate-boundaries.mjs`.

Validate with `mbx test` on affected crates, `mbx clippy --all-targets -- -D warnings` on affected crates, `cargo-machete`, `node scripts/check-crate-boundaries.mjs`, and `cargo package --locked --allow-dirty --list -p brokk-mj-store`. Commit on `master`, request `mj-review`, resolve/override, and push.

### Milestone 0b (+ Tier 1c): Feature Unification and Removing `github-history`/`tokenizers` from `mj-core` and `mj-transcript` (0 New Crates)

Eliminate all `mbx` feature-set cache fragmentation across the workspace so every package resolves identical features for `brokk-mj-core` and external crates:

1. **Move `github-history` tokenizer code and `tokenizers` dependency into `mj-github-relevance` (Tier 1c):**
   - Move `mj-core/src/github_item/ettin-tokenizer.json` and `history_tokenizer` (gated `#[cfg(feature = "github-history")]`) from `mj-core` into `mj-github-relevance`, updating `mj-core/Cargo.toml` `include` and `mj-github-relevance/Cargo.toml`.
   - Move the four `#[cfg(any(test, feature = "github-history"))]` functions (`history_scanner`, `build_evidence`, `student_segments`, and their helper/test code that requires `tokenizers`) from `mj-transcript/src/github_history.rs` into `mj-github-relevance` (keeping `SessionHistory`, `HistorySink`, `HistoryPairSpec`, `HistoryResult`, `RowMeta`, `RowFetch`, and non-tokenizer helpers in `mj-transcript`, which `mj-store` uses).
   - Update the two callers (`mj-daemon/src/github_watch/service.rs` and `mj-github-relevance/src/github_relevance.rs`) to call the moved functions in `mj_github_relevance`.
   - Delete `tokenizers` and `github-history` from `mj-core/Cargo.toml` and `mj-transcript/Cargo.toml`, add `tokenizers` unconditionally to `mj-github-relevance/Cargo.toml`, and remove `features = ["github-history"]` from all eight consumer manifests (`mj-store`, `mj-import`, `mj-sessionwiki`, `mj-session-runtime`, `mj-controller`, `mj-server`, `mj-daemon`, `mj-github-relevance`).
2. **Unify external dependency features in root `Cargo.toml`:**
   - Set in root `[workspace.dependencies]`:
     - `tokio = { version = "1", features = ["full", "test-util"] }`
     - `serde = { version = "1", features = ["derive", "rc"] }`
     - `tokio-util = { version = "0.7", features = ["rt", "compat", "io"] }`
     - `reqwest = { version = "0.13", default-features = false, features = ["rustls-no-provider", "blocking", "json", "query", "stream"] }`
     - `image = { version = "0.25", default-features = false, features = ["png", "jpeg", "webp"] }`
     - Check `axum` across `mj-worker`, `mj-server`, `mj-daemon` and unify if feature sets differ.
   - Remove redundant per-crate feature lists on those five dependencies (and redundant `[dev-dependencies] tokio = { workspace = true, features = ["test-util"] }` entries when `tokio` is already a normal dependency, keeping `tokio.workspace = true` in `[dev-dependencies]` only for crates that don't have `tokio` in `[dependencies]`).
3. **Remove `golden` and `test-identity` features:**
   - Compile `mj_core::golden` unconditionally (`#[doc(hidden)] pub mod golden;`), remove `golden` feature from `mj-core`, `mj-chat`, and all `[dev-dependencies]` entries.
   - Remove `test-identity` from `mj-core/Cargo.toml` and `mj-core/src/worker_build.rs`. Compile `BuildIdentity::for_tests` and `#[doc(hidden)] pub fn test_worker_executable()` unconditionally, keeping `TEST_WORKER_BUILD_STAMP` strictly inside `test_worker_executable()` so the linker strips it from real `mj` and `mj-worker` binaries (#1603); verify with a binary scan on built `mj` and `mj-worker`. In `installed()`, fall back to `BuildIdentity::for_tests` only when `cfg!(test) || crate::test_hooks::test_fixtures_enabled()`; otherwise keep the existing panic when a program did not call `worker_build::install(...)`.
4. **Replace library `test-hooks` with an explicit opt-in runtime gate (default `false`):**
   - Add `enable_test_hooks()`, `test_hooks_enabled()`, `enable_test_fixtures()`, `test_fixtures_enabled()`, `set_test_glide_endpoint(String)`, and `test_glide_endpoint()` to `mj_core::test_hooks`. Both `TEST_HOOKS_ENABLED` and `TEST_FIXTURES_ENABLED` default to `false` so every library caller gets strict production behavior unless it explicitly opts in.
   - Remove `test-hooks` from EVERY library crate (`mj-core`, `mj-build-id`, `mj-worker`, `mj-chat`, `mj-targets`, `mj-store`, `mj-accounts`, `mj-import`, `mj-sessionwiki`, `mj-build-cache`, `mj-worker-staging`, `mj-worker-launch`, `mj-session-runtime`, `mj-profile-catalog`, `mj-github-relevance`, `mj-controller`, `mj-setup`, `mj-server`, `mj-daemon`).
   - Keep `[features] test-hooks = []` (no forwarding!) ONLY on leaf binary crates `mj-cli` (`brokk-mjolnir`) and `mj-worker-bin` (`brokk-mj-worker`).
   - In `mj-cli/src/main.rs` right after `worker_build::install(...)`, call `mj_core::test_hooks::enable_test_hooks()` under `#[cfg(feature = "test-hooks")]`.
   - In `mj-worker-bin/src/main.rs` right after `worker_build::install(...)`, under `#[cfg(feature = "test-hooks")]`, call `mj_core::test_hooks::enable_test_hooks()`, and if `MJ_CHAOS_ISOLATED == "1"` and `MJ_TEST_GLIDE_ENDPOINT` is set, call `mj_core::test_hooks::set_test_glide_endpoint(...)`. Keep the re-exec `MJ_TEST_*` allowlist under `#[cfg(feature = "test-hooks")]`. Update `mj-worker/src/acp/verdict_client.rs` to read `mj_core::test_hooks::test_glide_endpoint()` so `"MJ_TEST_GLIDE_ENDPOINT"` never appears in a non-`test-hooks` `mj-worker` binary (preserving `tests/e2e/delegation.py`).
   - Update `mj-core/build.rs` to unconditionally write `$OUT_DIR/fake-worker.sh`, and `mj-build-id/build.rs` to write `$OUT_DIR/fake-worker.sh` when `tracked_read(&fixture)` returns `Some(script)`.
   - Gate all runtime hook sites (`reach_test_hook`, `login_environment.rs`, `provisioning.rs`, `lifecycle.rs`, `recovery_scan.rs`, `daemon/serve.rs`) with `mj_core::test_hooks::test_hooks_enabled()`, and have unit tests that exercise runtime hooks (such as `mj-accounts`'s `sign_in_environment_child`) explicitly call `mj_core::test_hooks::enable_test_hooks()`.
   - Remove `--features brokk-mj-core/test-hooks` from `.github/workflows/ci.yml` (`libraries` shard).
5. **Eliminate `test-support` across all 15 crates (strict production defaults):**
   - For items used only inside a crate's own `#[cfg(test)]` tests, change `#[cfg(any(test, feature = "test-support"))]` to `#[cfg(test)]`.
   - For items called by other crates' tests, expose them unconditionally as `#[doc(hidden)] pub` (preserving `#[cfg(unix)]` / `#[cfg(target_os = "linux")]` platform gates).
   - Remove `#![cfg_attr(all(feature = "test-support", not(test)), allow(dead_code, unused_imports))]` from all 13 `src/lib.rs` files.
   - Preserve strict production defaults at every behavioral switch, requiring explicit test opt-in (`cfg!(test) || mj_core::test_hooks::test_fixtures_enabled()`):
     - `mj-build-cache/src/mbx/release.rs`: `enabled()` defaults to `true` for all normal callers; when `cfg!(test) || mj_core::test_hooks::test_fixtures_enabled()`, releases stay off unless `enable_for_test()` is active on the thread.
     - `mj-store/src/database/schema.rs`: `open_reader(path)` calls `open_reader_strict(path)` by default; only when `cfg!(test) || mj_core::test_hooks::test_fixtures_enabled()` does it open an unpooled writable fixture `Reader`.
     - `mj-targets/src/aws_assume_role.rs`: `scope_key()` loads the real disk cookie signing key by default; only when `cfg!(test) || mj_core::test_hooks::test_fixtures_enabled()` does it check `SIGNING_KEY_UNAVAILABLE` and use the fixed test key.
     - `mj-targets/src/image_pull_gate.rs`: uses the cross-process lock file by default; skips it only when `cfg!(test)` or (`mj_core::test_hooks::test_fixtures_enabled()` and `ensure_may_control_store(&data_dir(), ...).is_err()`).
     - `mj-session-runtime/src/session_manager/recovery.rs`: `recover_worker_admitted` always requires a non-empty `session_id` in production (never accepts `None` outside `#[cfg(test)]` in `mj-session-runtime`).
     - `mj-profile-catalog/src/profile_catalog.rs` and `mj-accounts/src/quota.rs`: keep test-override fields unconditionally initialized to production defaults (`HOME_EDIT_RETRY_WAIT`, `None`).
     - `mj-server/src/server/viewer_types.rs`: increment TLS visit counters only when `cfg!(test) || mj_core::test_hooks::test_fixtures_enabled()`.
   - Have shared test fixture entry points (and any standalone cross-crate unit tests that rely on fixture defaults) call `mj_core::test_hooks::enable_test_fixtures()` explicitly, and never call it from `mj`, `mj-worker`, or any non-test library code path.
   - Remove `test-support` features from all 15 `Cargo.toml` files and all `[dev-dependencies]` entries.
6. **Add a feature-unification guard in `scripts/check-crate-boundaries.mjs`:**
   - Assert via `cargo metadata` / `cargo tree` (or static manifest check in `check-crate-boundaries.mjs`) that no library crate defines `test-hooks`, `test-support`, `test-identity`, `golden`, or `github-history`, and that no member crate adds extra feature flags on unified workspace dependencies (`tokio`, `serde`, `tokio-util`, `reqwest`, `image`).

Validate:
- Verify `cargo tree -p <pkg> -e normal,dev -i brokk-mj-core -f '{f}'` produces the exact same feature set across all 29 workspace packages, and likewise for `tokio`, `serde`, `tokio-util`, `reqwest`, `image`.
- Verify `mj-worker` built without `test-hooks` contains neither `0000000000000000000000000000000000000000` (#1603) nor `MJ_TEST_GLIDE_ENDPOINT`, while `mj-worker --features test-hooks` contains `MJ_TEST_GLIDE_ENDPOINT` and NOT `0000...`.
- Run `mbx clippy --workspace --all-targets -- -D warnings`, targeted `mbx test` covering `brokk-mj-core`, `brokk-mj-transcript`, `brokk-mj-github-relevance`, `brokk-mj-store`, `brokk-mj-targets`, `brokk-mj-accounts`, `brokk-mj-build-cache`, `brokk-mj-session-runtime`, `brokk-mj-controller`, `brokk-mj-daemon`, `brokk-mj-worker-runtime`, `brokk-mj-worker`, `brokk-mjolnir` (including `--features test-hooks` for `store_divergence`), `cargo-machete`, `node scripts/check-crate-boundaries.mjs`, `mbx package --locked --allow-dirty -p brokk-mj-core` (or `cargo package --locked --allow-dirty --list`), and `node --test scripts/release-workflow.test.mjs`.
- Commit on `master`, request `mj-review`, resolve/override, and push.

### Milestone 1d: Decouple `mj-server` from `mj-controller` (0 New Crates)

Remove `mj-controller` from `mj-server`'s dependencies (both normal and dev) so `mj-server` compiles in parallel with `mj-controller` and never rebuilds on `mj-controller` edits:

1. Repoint the 101 `mj_controller::*` imports in `mj-server/src/` directly to the lower crates that already own them (`mj-store`, `mj-accounts`, `mj-import`, `mj-targets`, `mj-session-runtime`, `mj-sessionwiki`, `mj-profile-catalog`, `mj-build-cache`, `mj-worker-staging`).
2. Move the ~350 lines of genuinely `mj-controller`-owned helpers/types that `mj-server` directly calls (`LifecycleFailureReason`, `ViewerMoveRecovery`, `RegisterSubagentRequest`, `subagent_has_handed_back`, `validate_resource_allocation`, `resolve_target_input_path`, `ImageBuilds`, host git identity helpers) down into the lowest crate that naturally owns them (`mj-session-types`, `mj-store`, `mj-targets`, `mj-session-runtime`, or `mj-worker-staging`), preserving re-exports in `mj-controller` only if external callers (`mj-daemon`, `mj-cli`, `mj-setup`) expect them or repointing them cleanly.
3. For any `mj-server` tests that previously constructed a full `mj_controller::Controller`, check whether they only need `mj_store` / `mj_server`'s backend trait fakes (which `mj-server` already defines!). If any test truly exercises `Controller` + `mj-server` together, move that test into `mj-daemon` (which already depends on both `mj-controller` and `mj-server`).
4. Remove `mj-controller` from `mj-server/Cargo.toml` (`[dependencies]` and `[dev-dependencies]`), update `scripts/check-crate-boundaries.mjs` (`allowed['brokk-mj-server']`), and update `CONTRIBUTING.md`.

Validate `mbx test -p brokk-mj-server -p brokk-mj-controller -p brokk-mj-daemon`, `mbx clippy -p brokk-mj-server -p brokk-mj-controller -p brokk-mj-daemon --all-targets -- -D warnings`, `cargo-machete`, `node scripts/check-crate-boundaries.mjs`, and verify total test counts before vs after (zero dropped tests). Commit, `mj-review`, push.

### Milestone 1a + 1e: Extract `mj-microvm`, `mj-worker-mcp`, and `mj-worker-review` out of `mj-worker` (3 New Crates)

Split `brokk-mj-worker-runtime` (`mj-worker/`, currently 31K LOC and 23.9% of workspace build time) into four cleaner crates:

1. **`brokk-mj-microvm` (`mj-microvm/`, library `mj_microvm`):**
   - Move `mj-worker/src/microvm/**` (21 files, ~10.1K LOC), `mj-worker/src/microvm/guest-init.sh` (or embedded asset path), and `mj-worker/tests/microvm_reaper.rs` into `mj-microvm/`.
   - Keep `#[cfg(target_os = "linux")]` behavior intact so non-Linux targets (macOS, Windows) still compile cleanly with zero warnings (just as `mj-worker` currently gates `#[cfg(target_os = "linux")] pub mod microvm;`).
   - Move microVM-only dependencies (`axum`, `tar`, `mbx-cache-core`, `mbx-cache-protocol`, `quick-xml`, `socket2`, `hex`, `hmac`, `url`, etc. — verify each with `cargo-machete`!) out of `mj-worker/Cargo.toml` into `mj-microvm/Cargo.toml`.
   - Wire `mj-worker-bin/src/main.rs:574` directly to `mj_microvm::main(arguments)` under `#[cfg(target_os = "linux")]`; remove `mj-microvm` completely from `mj-worker` (zero facade re-export!).
2. **`brokk-mj-worker-mcp` (`mj-worker-mcp/`, library `mj_worker_mcp`):**
   - Move `mcp_stdio`, `subagent_mcp`, and `memory_mcp` (~3.8K LOC, plus their tests) out of `mj-worker` into `mj-worker-mcp/`.
   - Depend only on `mj-core` and minimal external deps (no cycle with `mj-worker`). Have `mj-worker` depend on `mj-worker-mcp` only where it launches/wires MCP handlers (and `mj-worker-bin` directly where it dispatches MCP subcommands).
3. **`brokk-mj-worker-review` (`mj-worker-review/`, library `mj_worker_review`):**
   - Move `mj-worker/src/review/**` (~2.2K LOC, plus tests) out of `mj-worker` into `mj-worker-review/`, depending on `mj-core` and `mj-review` (and any narrow shared types moved down if needed; check exact imports first!).
   - Wire `mj-worker-bin` and `mj-worker` directly to `mj_worker_review`.
4. Complete the full New Crate Publication Checklist for all three crates (including `mj-core/worker-build-inputs.txt`, `.github/workflows/publish-agent-dev-image.yml`, `Containerfile.agent-dev`, `.dockerignore`, `licenses/deny.toml`, `publish.yml`, `ci.yml`, `windows.yml`, `check-crate-boundaries.mjs`, `CONTRIBUTING.md`).

Validate with `mbx test` on all four crates + `brokk-mj-worker` + `brokk-mj-core` (`worker_input_paths_cover_the_normal_and_build_dependency_closure`), `mbx clippy --all-targets -- -D warnings`, musl worker check (`mbx clippy --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker -- -D warnings`), `cargo-machete`, `node scripts/check-crate-boundaries.mjs`, `node --test scripts/release-workflow.test.mjs`, and `cargo package --locked --allow-dirty --list` for each new crate. Register all three crates on crates.io right before pushing, commit, `mj-review`, push.

### Milestone 1b: Extract `mj-harness-install` out of `mj-core` (1 New Crate)

Shrink `brokk-mj-core` (currently 27 normal rev-deps, 28.0% of workspace compile time) by extracting harness installation, runtime assets, session image helpers, program path discovery, and login environment probing:

1. Create `brokk-mj-harness-install` (`mj-harness-install/`, library `mj_harness_install`) depending on `mj-core`.
2. Move `harness_install`, `session_image`, `program_path`, and `login_environment` (~5.1K LOC + tests) out of `mj-core` into `mj-harness-install`.
3. Move the embedded harness/runtime assets (`assets/harnesses/**`, `assets/{antigravity,grok,kimi,mise,muse,node,opencode}/runtime.json`, and check `assets/opencode-provider-environment.txt`) cleanly from `mj-core/` to `mj-harness-install/` (update `include` in both `Cargo.toml` files, update the explicit `COPY mj-core/assets/muse/runtime.json` and `COPY mj-core/assets/antigravity/runtime.json` source paths in `containers/Containerfile.agent-dev` at lines 359 and 364 along with any other asset paths in `scripts/agent-dev-image.test.mjs`, and update any test paths such as `mj-worker-staging/src/worker_binary/tests.rs`, `AGENTS.md`, and `bridge_fallback_pins_match_the_agent_dev_containerfile` references).
4. Repoint all 8 direct callers (`mj-accounts`, `mj-worker`, `mj-cli`, `mj-setup`, `mj-targets`, `mj-controller`, `mj-session-runtime`, `mj-worker-bin`, plus `mj-worker-staging` / `mj-worker-launch` / `mj-profile-catalog` if any call these modules) directly to `mj_harness_install` with **no `pub use` re-exports on `mj-core`**.
5. Remove `tar` (and any other now-unused dependencies caught by `cargo-machete`) from `mj-core/Cargo.toml`.
6. Complete the full New Crate Publication Checklist (note `mj-harness-install` is in `brokk-mj-worker`'s dependency closure, so add it to `mj-core/worker-build-inputs.txt` and `publish-agent-dev-image.yml`, and check `mj-core` packaging via `cargo package --locked -p brokk-mj-core`).

Validate with `mbx clippy --workspace --all-targets -- -D warnings`, `mbx test` on `brokk-mj-core`, `brokk-mj-harness-install`, and affected callers (including `bridge_fallback_pins_match_the_agent_dev_containerfile`, `opencode_provider_environment_was_generated_from_the_pinned_opencode`, `worker_input_paths_cover_the_normal_and_build_dependency_closure`), `cargo-machete`, `check-crate-boundaries.mjs`, `node --test scripts/release-workflow.test.mjs`, `node --test scripts/agent-dev-image.test.mjs`, verify every `COPY` source path in `containers/Containerfile.agent-dev` resolves on disk (and run a container image build verification if Podman/Docker is available on this target), and `cargo package --locked --allow-dirty --list`. Register `brokk-mj-harness-install` on crates.io, commit, `mj-review`, push.

### Milestone 1f: Extract `mj-github-watch` out of `mj-daemon` (1 New Crate)

Keep edits to the GitHub watch poller/notifier out of `mj-daemon`'s 40–44 s test harness:

1. Create `brokk-mj-github-watch` (`mj-github-watch/`, library `mj_github_watch`) depending on `mj-core`, `mj-transcript`, `mj-store`, `mj-github-relevance`, and `mj-targets`/`mj-import` only as strictly needed (never `mj-daemon` or `mj-controller`).
2. Move `mj-daemon/src/github_watch/**` (~4.1K LOC + tests) into `mj-github-watch/`.
3. Replace the ~4 narrow daemon touchpoints (`controller_projection`, `push_public_notice`, and daemon config/runtime handles) with a clean narrow trait implemented in `mj-daemon`.
4. Repoint DB calls directly to `mj-store`. Remove any dependencies that are no longer needed in `mj-daemon/Cargo.toml` (check `cargo-machete`).
5. Complete the full New Crate Publication Checklist (not in `brokk-mj-worker` closure, so no `worker-build-inputs.txt` change).

Validate with `mbx test -p brokk-mj-github-watch -p brokk-mj-daemon`, `mbx clippy -p brokk-mj-github-watch -p brokk-mj-daemon --all-targets -- -D warnings`, `cargo-machete`, `check-crate-boundaries.mjs`, `release-workflow.test.mjs`, and `cargo package --locked --allow-dirty --list -p brokk-mj-github-watch`. Register `brokk-mj-github-watch` on crates.io, commit, `mj-review`, push.

### Milestone 2: Cross-Platform Verification and `mbx` Before/After Measurement

1. Verify `git worktree list` is clean (remove any temporary detached worktrees under `~/Projects/mjolnir-wt/`).
2. Run `scripts/check-crate-size.py`, `node scripts/check-crate-boundaries.mjs`, `node --test scripts/release-workflow.test.mjs`, `cargo-machete`, `cargo fmt --all -- --check`, and `mbx clippy --workspace --all-targets -- -D warnings` on final `master`.
3. Measure warm/incremental `mbx` build timings on common edit-test loops (no-op comment edit in `mj-controller`, `mj-harness-install`, `mj-microvm`, `mj-github-watch`, and cross-`-p` warm switches) and verify `cargo tree -i brokk-mj-core` feature unification across all workspace members.
4. Dispatch `.github/workflows/nightly.yml` on `master` via `gh workflow run nightly.yml --ref master` to verify macOS and Windows CI lanes, and verify `publish-agent-dev-image.yml` passes on `master`.
5. Record final metrics and lessons in `Outcomes & Retrospective`.

## Concrete Steps & Validation Rules

- Always run builds, clippy, and tests via `mbx` (never plain `cargo` on an `mbx` target symlink).
- Always run tests outside the restricted sandbox (`BypassSandbox: true` or via Mjolnir child agents).
- Every child implementing a milestone must report exact per-filter test counts (checking for zero-match filters), clippy results, `cargo-machete`, and boundary checks.
- Before pushing any milestone commit: commit on `master` (never `git checkout -b`, never `git add -A`), call `mcp_mj-review_request_review`, address or cleanly `override_review` findings, and push to `origin/master` (merging `origin/master` if upstream moved).

## Outcomes & Retrospective

*(To be updated as each milestone completes and after Milestone 2 measurements.)*
