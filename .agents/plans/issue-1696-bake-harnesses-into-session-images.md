# Bake Pinned Harnesses into Session Images (`mj image build`) and Support a Generic All-Harnesses Base Image

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it in accordance with `.agents/PLANS.md` (repository root).

Tracking issue: BrokkAi/mjolnir#1696 (child of Epic BrokkAi/mjolnir#1695).

## Purpose / Big Picture

Every brand-new session container and every resumed container currently spends 11.9 to 21.0 seconds inside `prepare exact managed harness before worker startup`, downloading Node.js (58 MB) and running a cold `npm ci` (344 MB of tarballs, 512 MB installed, plus 344 MB of leftover `/root/.npm/_cacache`) into the container's ephemeral writable layer. That single step accounts for roughly 35% to 50% of warm container launch latency and adds ~850 MB of wasted overlayfs writes per container.

After this change, `mj image build` bakes Mjolnir's pinned Node.js runtime and the pinned harnesses for all enabled configured profiles (`config.enabled_profiles()`) into a read-only image store at `/opt/mj-harnesses`. At session startup, workers check `/opt/mj-harnesses` by exact `install_id` and Node version before writing to `$XDG_CACHE_HOME/mjolnir`, eliminating the 12–21 s cold install on every new container and Resume while working identically across Podman, Docker, Apple Container, and Nitro microVMs without host-local derived image management. Whenever runtime `npm ci` does run (for example on an image without baked harnesses or after a harness pin bump prior to rebuilding), it writes its npm cache into a temporary staging directory that is deleted immediately, eliminating the 344 MB `_cacache` leak. Finally, `mj image base` and `scripts/build-session-base-image.sh` generate a generic extension of `docker.io/library/buildpack-deps:noble` containing all eight pinned harnesses; when `mj image build` uses a base image whose OCI labels already declare matching `dev.mj.harnesses.*` pins, it skips reinstalling those harnesses while still hashing all required pins into the image tag so `mj image status` detects staleness on pin bumps.

To see this working end-to-end, run `mj image build --print` against a repository with enabled profiles configured and observe the deterministic multi-stage Containerfile that bakes `/opt/mj-harnesses` and stamps `dev.mj.harnesses.*` labels. Build and launch a real container session from that image in an isolated `--instance`, and observe that `prepare exact managed harness before worker startup` finishes in under one second with zero downloads, zero files created under `$HOME/.cache/mjolnir/harnesses`, and zero `/root/.npm/_cacache`.

## Progress

- [x] (2026-11-25 17:04Z) Completed read-only reconnaissance of `mj-harness-install`, `mj-server/src/server/session_images.rs`, `mj-cli/src/image_commands.rs`, `mj-worker`, `mj-accounts`, `tests/e2e/mise_images.py`, and all eight harness runtime manifests.
- [x] (2026-11-25 17:13Z) Claimed BrokkAi/mjolnir#1696 (`agent-in-progress`) and authored this ExecPlan.
- [x] Milestone 1: Shared pinned-manifest accessors, read-only `/opt/mj-harnesses` resolution in `mj-core`, `mj-harness-install`, and `mj-worker`, plus temporary `--cache` cleanup in runtime `install_npm`.
- [x] Milestone 2: Deterministic multi-stage harness baking in `mj-harness-install/src/session_image.rs`, base-image `dev.mj.harnesses.*` label detection in `mj-server/src/server/session_images.rs`, `mj image base` CLI subcommand and `scripts/build-session-base-image.sh`, plus `mj image status` wording and skill docs.
- [x] Milestone 3 (delegated scope): Targeted Cargo tests, clippy, changed-file rustfmt, crate-boundary verification, and shell syntax check completed. Review, commit, and push are explicitly excluded by the delegated assignment. The CLI build-print golden initially exposed an empty-parent bug in the shared emitter; fixed it and reran only that failing test successfully.
- [ ] Milestone 4: Isolated real Podman/container verification confirming `<1 s` harness preparation, zero `_cacache` leak, and stale-pin detection via `mj image status`, followed by updating this plan and closing #1696.

- Implemented shared typed artifact accessors, read-only baked resolution, deterministic two-stage image recipes, base image labels, local base CLI, helper script, and focused precedence/cache/determinism tests. Final validation results are recorded in the implementation agent report directory.

## Surprises & Discoveries

- Observation: `docker.io/library/buildpack-deps:noble` does not include the `zstd` CLI (`zstd: MISSING`), whereas Grok's managed runtime artifact is distributed as a `.zst` compressed binary (`grok-<version>-<target>.zst`). Node 24.22.1 includes built-in `zlib.createZstdDecompress()` in its standard library.
  Evidence: Checked via `podman run --rm docker.io/library/buildpack-deps:noble` (`zstd: MISSING`, `tar/gzip/curl/base64/sha256sum/python3` present) and `node -e "console.log(typeof require('zlib').createZstdDecompress)"` (`function`).
- Observation: Existing unit tests in `mj-accounts` (`codex_usage.rs`, `grok_usage.rs`, `harness_login/acp_login.rs`) and end-to-end fixtures in `tests/e2e/reliability_lab.py` (used by `tests/e2e/mise_images.py`) plant fake harnesses either on `PATH` (`fixture_bin/codex-acp` under `HarnessRuntimePolicy::Ambient`) or in a temporary `XDG_CACHE_HOME/mjolnir/harnesses/<kind>/<install_id>` directory. Those crates compile `mj-harness-install` without `#[cfg(test)]`.
  Evidence: Inspected `mj-accounts/src/harness_login/acp_login.rs:920-960`, `mj-accounts/src/grok_usage.rs:319-355`, `mj-accounts/src/codex_usage.rs:498-515`, and `tests/e2e/reliability_lab.py:826-830`. If `/opt/mj-harnesses` superseded an explicit writable cache install or an `Ambient` `PATH` match, running tests inside a future `mj-dev` image carrying `/opt/mj-harnesses` would launch the real baked binaries instead of the test fakes.
- Observation: `HarnessRuntimePolicy::Ambient` in `mj-harness-install/src/harness_install.rs:180-274` only version-checks Codex, Claude, and Antigravity; Kimi, Grok, OpenCode, and Muse are presence-only, and `Ambient` hits omit `CODEX_PATH`, `CLAUDE_CODE_EXECUTABLE`, `MUSE_CLI`, and `OPENCODE_DISABLE_AUTOUPDATE=1`. Placing baked harnesses in a dedicated `/opt/mj-harnesses` store (not on `PATH`) and checking it inside `resolve_at_async` gives exact `install_id` verification and full environment-variable wiring for every harness while preserving `Ambient` `PATH` priority for custom and test binaries.
- Observation: `mj-worker/src/worker_runtime/harness_launch.rs:166` calls `ensure_node_runtime_for` before calling `harness::locate`, which then calls `resolve_at_async -> ensure_node_runtime`. Without a fast-path check recognizing that the verified baked Node directory is already first on `PATH`, the second call would spawn three subprocesses (`node --version`, `npm --version`, `npx --version`) on every launch.
- Observation: `mj-client/src/session_image.rs::ImageRequest` carries `#[serde(deny_unknown_fields)]`. Because `mj-server/src/server/session_images.rs` already loads `Config` during `select` and `prepare`, baking `config.enabled_profiles()` requires zero wire or protocol changes, and `mj image base` can run purely locally without touching the daemon API.
- Observation: Repo-only GitHub credentials in coding sessions cannot push `.github/workflows/**` files. A local `scripts/build-session-base-image.sh` plus `mj image base` subcommand can land cleanly from here; any GitHub Actions workflow to auto-publish `ghcr.io/brokkai/mjolnir/session-base` must be pushed with a workflow-scoped token by the user.

## Decision Log

- Decision: Bake pinned harnesses directly into session images via `mj image build` under a read-only `/opt/mj-harnesses` tree instead of maintaining host-local `podman commit` derived images.
  Rationale: Explicitly directed by the user ("bake the harnesses used by the project's configured profiles into mj image build, i still want to do that [maybe we publish a generic extension of bp-d:n with all the harnesses, as well]"). Eliminates host-local derived-image lifecycle/GC/race complexity and works identically across Podman, Docker, Apple Container, and Nitro microVMs.
  Date/Author: 2026-11-25, user direction; planned by coordinator.
- Decision: Use a dedicated read-only store `/opt/mj-harnesses/{node,harnesses}` (not on `PATH`) consulted inside `resolve_at_async` and `ensure_node_runtime_with_minimum` only when the corresponding writable cache directory in `$XDG_CACHE_HOME/mjolnir` does not already exist, and after `Ambient` `PATH` checks.
  Rationale: Guarantees exact `install_id` and `NODE_VERSION` verification across all eight harnesses, sets every required CLI override variable (`CODEX_PATH`, `CLAUDE_CODE_EXECUTABLE`, `MUSE_CLI`, `OPENCODE_DISABLE_AUTOUPDATE`, `AGY_CLI_DISABLE_AUTO_UPDATE`), avoids `PATH` collisions with project-managed Node versions (such as Node 18 in `mise.toml`), requires zero locks, writes, or GC tasks on read-only layers, and preserves every existing `PATH` and `XDG_CACHE_HOME` test fixture even when tests execute inside a baked image.
  Date/Author: 2026-11-25, coordinator.
- Decision: Emit a multi-stage Containerfile (`FROM <base> AS mj-harnesses` followed by the final `FROM <base>` stage with `COPY --from=mj-harnesses` after `mise install`) whenever at least one requested harness or Node version is not already declared in `base`'s `dev.mj.harnesses.*` labels.
  Rationale: Keeps `ADD --checksum` archive downloads and temporary extraction tools strictly inside the intermediate stage so zero tarball or `_cacache` bytes enter the final OCI image; places `COPY --from` after `mise install` so harness pin bumps reuse the slow `mise install` layer cache while recipe changes reuse the `mj-harnesses` stage cache; copies each newly installed subtree individually so partial pin bumps over a pre-baked base do not duplicate unchanged harnesses.
  Date/Author: 2026-11-25, coordinator.
- Decision: Always stamp `LABEL dev.mj.harnesses.node` (when any selected harness uses Node) and `dev.mj.harnesses.<kind>` (for every selected harness) in the final stage even when inherited from the base image, and collect selected harnesses as the distinct `HarnessKind`s of `config.enabled_profiles()` sorted by `kind.id()`.
  Rationale: `image_tag` hashes the entire Containerfile text. Stamping every desired pin into the final `LABEL` guarantees that `mj image status` flags the project image as out of date whenever a harness pin bumps or enabled profile kinds change, even if the base image previously supplied those harnesses.
  Date/Author: 2026-11-25, coordinator.
- Decision: Bake `@brokkai/muse-acp` alongside native `bin/muse` for Muse and `@brokkai/dsh-acp` with a `dsh-acp` entrypoint symlink for DSH, and check baked entrypoints before `npx` in `harness_install.rs` (for Muse) and `dsh_acp_launch_script` (for DSH).
  Rationale: Without baking the npm adapters, Muse and DSH would still execute `npx -y` on every cold container. Both pins' `install_id` values already encode their npm package versions, so pin bumps invalidate cleanly.
  Date/Author: 2026-11-25, coordinator.
- Decision: Centralize runtime manifest JSON parsing (Node, Antigravity, Muse, Kimi, Grok, OpenCode, mise) in one shared module in `mj-harness-install` consumed by both `harness_install.rs` and `session_image.rs`.
  Rationale: Follows `AGENTS.md` single-owner rule so runtime downloads and image builds can never drift on URLs, platform keys, filenames, or SHA-256 digests.
  Date/Author: 2026-11-25, coordinator.
- Decision: Change `mj image status` stale label from `"recipe changed"` to `"out of date"`.
  Rationale: A project image now goes out of date when either the mise recipe, base digest, mise pin, or enabled harness pins change.
  Date/Author: 2026-11-25, coordinator.

## Outcomes & Retrospective

Milestones 1 and 2 are implemented. Milestone 3 code/tests/static validation is tracked in the implementation report. No commit or push is authorized for this delegated implementation; parent review and Milestone 4 real-container verification remain pending.

## Context and Orientation

Mjolnir (`mj`) runs coding agents in container sessions across local and SSH Podman, Docker, Apple Container, and EC2 Nitro microVMs. The daemon (`mj-server` / `mj-daemon`) is the control plane; per-session workers (`mj-worker`, binary `mj-worker` / `hel`) run inside each session environment and prepare harnesses before starting the ACP bridge.

Project session images are built without a local build context by streaming a deterministic Containerfile on stdin to the container engine (`docker build -` or `podman build --file -`). Four crates participate in this feature:

1. `mj-core/src/harness_runtime.rs`: Single source of truth for harness pin versions (`NODE_VERSION`, `CODEX_ACP_VERSION`, `CLAUDE_ACP_VERSION`, etc.), `pin(HarnessKind) -> HarnessPin`, `ManagedHarnessManifest`, cache root path helpers (`managed_harness_install_dir`), `harness_uses_node_runtime`, and `dsh_acp_launch_script()`.
2. `mj-harness-install`: Owns both runtime installation (`src/harness_install.rs`) and deterministic session image Containerfile generation (`src/session_image.rs`), plus embedded manifests and lockfiles under `assets/{mise,node,antigravity,muse,kimi,grok,opencode}/runtime.json` and `assets/harnesses/{claude,codex}/{package.json,package-lock.json}`.
3. `mj-worker/src/worker_runtime/`: Calls `ensure_node_runtime_for` and `harness::locate` in `harness_launch.rs` (lines 165–252) and `discovery.rs` (lines 24–82), passes optional `harness_lease` and `node_lease` into `AcpSupervisorSpec` (`unix/supervisor.rs:241-250`), and spawns background obsolete-cache GC in `unix.rs:579-580` when `managed_cache_root` is `Some`.
4. `mj-server/src/server/session_images.rs` and `mj-cli/src/image_commands.rs`: Implement `mj image build` (including `--print`) and `mj image status`. `prepare()` in `mj-server/src/server/session_images.rs:642-762` pulls and inspects the target's base image (reading `Digest`/`RepoDigests` and `Config` / `variant["config"]["config"]`), builds `ImageBase`, and calls `mj_harness_install::session_image::containerfile(...)`. `image_tag(&text)` hashes the entire Containerfile (first 16 lowercase hex chars of SHA-256) to produce the OCI tag. `status_images()` (lines 764–841) recomputes the Containerfile and compares its `image_tag` with the recorded reference tag in `[images.bundles.<id>]`.

`Config.profiles` (`mj-core/src/config.rs:1481`) is global across bundles; `config.enabled_profiles()` (`mj-core/src/config.rs:2122`) iterates over enabled profiles, each having `profile.kind: HarnessKind`.

## Plan of Work

### Milestone 1: Read-only Baked Harness Store, Shared Manifest Reader, and Runtime `_cacache` Cleanup

Create a small internal module `mj-harness-install/src/runtime_manifests.rs` (declared in `mj-harness-install/src/lib.rs`) that parses the embedded JSON manifests for mise, Node, Antigravity, Muse, Kimi, Grok, and OpenCode, validating each manifest's version against `mj_core::harness_runtime::*` constants and validating 64-character hex SHA-256 strings. Expose typed structs (`NodeArtifact`, `AntigravityArtifact`, `MuseArtifact`, `KimiArtifact`, `GrokArtifact`, `OpenCodeArtifact`, and reuse/re-export `MiseArtifact`) plus `platform_arch(platform: &str) -> Result<&'static str>` mapping `"linux/amd64" -> "x86_64"` and `"linux/arm64" -> "aarch64"`. Update `mj-harness-install/src/harness_install.rs` and `mj-harness-install/src/session_image.rs` to consume these shared readers rather than duplicating `include_str!` manifest parsing.

In `mj-core/src/harness_runtime.rs`:
- Add `pub const BAKED_HARNESS_ROOT: &str = "/opt/mj-harnesses";` and `pub const BAKED_NODE_MANIFEST_FILE: &str = "mj-node.json";`.
- Add `pub const ALL: [Self; 8]` on `HarnessKind` (in `mj-core/src/config/harness.rs`) if not already present, listing all eight variants.
- Add `BakedNodeManifest` (`schema: 1`, `version: String`, `#[serde(deny_unknown_fields)]`) with `BakedNodeManifest::current()` and `baked_node_manifest_matches(path: &Path, version: &str) -> Result<bool>`.
- Add `pub fn baked_harness_install_dir(root: &Path, harness: HarnessKind) -> PathBuf` returning `managed_harness_install_dir(&root.join("harnesses"), harness)` and `pub fn baked_node_install_dir(root: &Path, folder: &str) -> PathBuf` returning `root.join("node").join(folder)`.
- Update `dsh_acp_launch_script()` so that after checking `dsh-acp` on `PATH` and before falling back to `npx -y @brokkai/dsh-acp@<version>`, it checks `[ -x '<BAKED_HARNESS_ROOT>/harnesses/dsh/<install_id>/<entrypoint>' ] && command -v node >/dev/null 2>&1 && <node_check>` and executes that baked entrypoint when present.

In `mj-harness-install/src/harness_install.rs`:
- Make `ManagedHarness.lease_path: Option<PathBuf>`, `ManagedHarness.cache_root: Option<PathBuf>`, `ManagedHarness._lease: Option<FileLock>`, and `ManagedNodeRuntime` store an optional lease (`lease: Option<NodeLease>` where `NodeLease { path: PathBuf, _lock: FileLock }`) with `pub fn lease_path(&self) -> Option<&Path>`.
- Update the four worker call sites (`mj-worker/src/worker_runtime/harness_launch.rs`, `mj-worker/src/worker_runtime/discovery.rs`, `mj-worker/src/worker_runtime/unix.rs`) and any test assertions in `mj-harness-install/src/harness_install.rs` for the `Option` fields. Because `AcpSupervisorSpec.{harness_lease, node_lease}` and `managed_cache_root` are already `Option<PathBuf>`, a baked harness cleanly skips both supervisor lease locking and 30-second obsolete-cache GC.
- Add `fn baked_harness_store_root(environment: &BTreeMap<String, String>) -> Option<PathBuf>`: in `#[cfg(test)]` it returns `environment.get("MJ_TEST_BAKED_HARNESS_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from)` (so existing tests never accidentally pick up host/container `/opt/mj-harnesses`); in `#[cfg(not(test))]` it returns `Some(PathBuf::from(BAKED_HARNESS_ROOT))`.
- Extract `build_managed_harness(...)` from `resolve_at_async` so both writable-cache installs and baked installs construct `ManagedHarness` through one code path (preserving `PATH`, `MUSE_CLI`, `CODEX_PATH`, `CLAUDE_CODE_EXECUTABLE`, `OPENCODE_DISABLE_AUTOUPDATE`, `AGY_CLI_DISABLE_AUTO_UPDATE`, and OpenCode's wrapper script). For `HarnessKind::Muse`, if `install.join("node_modules/.bin/muse-acp")` is executable, launch it directly instead of `sh -c <bootstrap_script>`.
- In `resolve_at_async`: after resolving Node, compute `install = managed_harness_install_dir(root, harness)`. If `!install.exists()`, check `baked = baked_harness_install_dir(&baked_root, harness)` via `complete_install(&baked, harness, selected)?`; on match, return `build_managed_harness` with `lease: None, cache_root: None` without creating any cache directories or locks.
- In `ensure_node_runtime_with_minimum`: check `baked_node_dir(environment, &metadata)` (returning `Ok(None)` if the platform has no Node artifact entry). If `baked_node` is already the first entry of `PATH` and `baked_node_is_usable(&baked, minimum_version)?` is true, return `Ok(None)` immediately (zero subprocesses). Otherwise run `system_node_is_compatible(environment, minimum_version).await`. If false, check `!cached.exists()` and `baked_node_is_usable(&baked, minimum_version)?` (`baked_node_manifest_matches(&baked, NODE_VERSION)?`, `node_version_is_at_least(NODE_VERSION, minimum_version)`, `bin/{node,npm,npx}` executable, and `ensure_node_artifact_platform` passes); on match, `prepend_path(environment, &baked.join("bin"))?` and return `Ok(Some(ManagedNodeRuntime::baked(baked)))`. Otherwise fall through to `install_managed_node`.
- In `install_npm`: create a temporary directory `tempfile::Builder::new().prefix(".install-npm-cache-").tempdir_in(staging.parent().unwrap_or(staging))?`, pass `--cache <npm_cache.path()>` to `npm ci --omit=dev --no-audit --no-fund`, and ensure `npm_config_cache` is set to that path as well (overriding any inherited value). Dropping `TempDir` removes the ~344 MB cache immediately on success, error, or cancellation; its `.install-` prefix ensures `remove_abandoned_staging` cleans it up even after `SIGKILL`.

### Milestone 2: Deterministic Harness Baking in `session_image.rs`, `session_images.rs`, `mj image base`, and Helper Script

In `mj-harness-install/src/session_image.rs`:
- Define label constants `pub const BAKED_NODE_LABEL: &str = "dev.mj.harnesses.node";` and `pub const BAKED_HARNESS_LABEL_PREFIX: &str = "dev.mj.harnesses.";`.
- Extend `ImageBase` with `pub baked_node: Option<String>` and `pub baked_harnesses: BTreeMap<String, String>` (implementing `Default` and providing a helper `ImageBase::parse_baked_labels(labels: &serde_json::Value) -> (Option<String>, BTreeMap<String, String>)` so `session_images.rs` parses `Config.Labels` in one place).
- Update `pub fn containerfile(base: &ImageBase, platform: &str, mise: &MiseArtifact, files: &BTreeMap<String, String>, harnesses: &[HarnessKind]) -> Result<String>` and add `pub fn session_base_containerfile(base: &ImageBase, platform: &str) -> Result<String>` (which bakes all `HarnessKind::ALL` with no mise/recipe layer, sharing the exact same harness-stage builder).
- Extract a shared `emit_embedded_file(text: &mut String, dest_path: &str, bytes: &[u8], mode: &str)` helper using the existing 48 KiB base64 chunking (`MAX_ARG_STRLEN`-safe), reused for both recipe files and npm harness `package.json`/`package-lock.json`.
- For any non-empty deduplicated sorted `harnesses` (sorted by `kind.id()`):
  - Compute `needs_node = harnesses.iter().any(|k| harness_uses_node_runtime(*k))`.
  - Compute `missing_node = needs_node && base.baked_node.as_deref() != Some(NODE_VERSION)`.
  - Compute `missing_harnesses` as those kinds whose `base.baked_harnesses.get(k.id()).map(String::as_str) != Some(pin(*k).install_id)`.
  - Only when `missing_node || !missing_harnesses.is_empty()`, emit Stage 1 (`FROM <base.reference> AS mj-harnesses` + `USER root`):
    - If `missing_node`: `ADD --checksum=sha256:<sha256> <url> /tmp/mj-node.tar.gz`, then one `RUN` that extracts into `/opt/mj-harnesses/node`, removes `/tmp/mj-node.tar.gz`, runs `bin/node --version && bin/npm --version && bin/npx --version`, writes `mj-node.json` (serialized `BakedNodeManifest::current()`), and runs `chmod -R a+rX`.
    - For each missing harness:
      - `Codex` and `Claude`: embed `package.json` and `package-lock.json` into `/opt/mj-harnesses/harnesses/<kind>/<install_id>`, then one `RUN` with `PATH="/opt/mj-harnesses/node/<folder>/bin:$PATH"` and `HOME=/tmp/mj-harness-home` running `npm ci --omit=dev --no-audit --no-fund --cache /tmp/mj-npm-cache`, checking `npm install-scripts ls --json` (matching `verify_install_scripts_ran`), running `./<cli_entrypoint> --version`, verifying both entrypoints are executable, deleting `/tmp/mj-npm-cache` and `$HOME`, writing `mj-harness.json`, and `chmod -R a+rX`.
      - `Antigravity`: `ADD --checksum=sha256:<sha256> <url> /tmp/mj-antigravity.tar.gz`, extract `antigravity` to `<dir>/bin/agy`, `chmod 0755`, delete temp files, write `mj-harness.json`, `chmod -R a+rX`.
      - `OpenCode`: `ADD --checksum=sha256:<sha256> <url> /tmp/mj-opencode.tgz`, extract `package/bin/opencode` to `<dir>/opencode`, `chmod 0755`, delete temp files, write `mj-harness.json`, `chmod -R a+rX`.
      - `Kimi`: `ADD --checksum=sha256:<sha256> <primary_url> <dir>/bin/kimi`, `RUN chmod 0755 <dir>/bin/kimi && write mj-harness.json && chmod -R a+rX <dir>`.
      - `Grok`: `ADD --checksum=sha256:<sha256> <gcs_url> /tmp/mj-grok.zst` (using the GCS artifact URL to avoid Cloudflare WAF blocks on container engines), then `RUN` decompressing via `zstd -d -f` if present, else `/opt/mj-harnesses/node/<folder>/bin/node`'s built-in `zlib.createZstdDecompress()` if present, else `apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends zstd && zstd -d -f`, creating `ln -s grok <dir>/bin/agent`, deleting `/tmp/mj-grok.zst`, writing `mj-harness.json`, and `chmod -R a+rX`.
      - `Muse`: `ADD --checksum=sha256:<sha256> <url> <dir>/bin/muse`, then `RUN` setting `chmod 0755 <dir>/bin/muse`, installing `@brokkai/muse-acp@<MUSE_ACP_VERSION>` into `<dir>` via `npm install --no-save --omit=dev --no-audit --no-fund --cache /tmp/mj-npm-cache --prefix <dir>` (using baked Node `PATH` and `HOME=/tmp/mj-harness-home`), verifying `bin/muse` and `node_modules/.bin/muse-acp` are executable, deleting `/tmp/mj-npm-cache` and `$HOME`, writing `mj-harness.json`, and `chmod -R a+rX`.
      - `Dsh`: `RUN` installing `@brokkai/dsh-acp@<DSH_ACP_VERSION>` into `<dir>` via `npm install --no-save --omit=dev --no-audit --no-fund --cache /tmp/mj-npm-cache --prefix <dir>`, symlinking `ln -s node_modules/.bin/dsh-acp <dir>/dsh-acp`, verifying `<dir>/dsh-acp` is executable, deleting `/tmp/mj-npm-cache` and `$HOME`, writing `mj-harness.json`, and `chmod -R a+rX`.
  - In Stage 2 (final stage): after the `mise install` `RUN` (or directly after `USER root` in `session_base_containerfile`), emit one `COPY --from=mj-harnesses <path> <path>` per newly installed Node/harness subtree.
  - Emit `dev.mj.harnesses.node` (if `needs_node`) and `dev.mj.harnesses.<kind>` (for every requested harness, whether inherited from base or newly baked) in the final `LABEL` block.

In `mj-server/src/server/session_images.rs`:
- Compute `enabled_harnesses(config: &Config) -> Vec<HarnessKind>` collecting distinct `profile.kind` values from `config.enabled_profiles()`, sorted by `kind.id()`.
- In `prepare()`: parse `image_config["Labels"]` via `ImageBase::parse_baked_labels`, pass `baked_node` and `baked_harnesses` into `ImageBase`, and pass `&enabled_harnesses(config)` to `containerfile(...)`. Use `mj_harness_install::session_image::platform_arch(platform)?` inside `mise_artifact`.

In `mj-cli/src/image_commands.rs`, `mj-cli/tests/golden/image-status.txt`, `mj-core/assets/skills/mj/SKILL.md`, and `scripts/build-session-base-image.sh`:
- Add `ImageCommand::Base(BaseArgs)` (`mj image base --base <ref@sha256:...> --platform <linux/amd64|linux/arm64> [--user <user>] [--workdir <dir>]`) that prints `session_base_containerfile(...)` to stdout locally without requiring a running daemon.
- Change `render_status` stale wording in `mj-cli/src/image_commands.rs:220` from `"recipe changed"` to `"out of date"` and update `mj-cli/tests/golden/image-status.txt` and `mj-core/assets/skills/mj/SKILL.md`.
- Add `scripts/build-session-base-image.sh` (modeled on `scripts/build-daemon-image.sh`): pulls/inspects `docker.io/library/buildpack-deps:noble` (or an override base image) for the requested platform to resolve its immutable `@sha256:` digest, `User`, and `WorkingDir`, generates the Containerfile via `mj image base` (using `$MJ_BIN` or `cargo run`), and pipes it into `$engine build --platform <platform> --tag <tag>`.

### Milestone 3: Tests, Static Checks, Review, and Push

Add focused integration and tricky-condition tests (following `AGENTS.md` "Which tests to add"):
1. In `mj-harness-install/src/harness_install.rs`:
   - Baked read-only harness resolution under `HarnessRuntimePolicy::Ambient` and `Managed` hits `/opt/mj-harnesses` with zero writes to `XDG_CACHE_HOME`, `lease_path: None`, `cache_root: None`, sets `CODEX_PATH` / `CLAUDE_CODE_EXECUTABLE` / `MUSE_CLI` / `OPENCODE_DISABLE_AUTOUPDATE` / `AGY_CLI_DISABLE_AUTO_UPDATE`, launches baked `muse-acp` directly when present, and falls back to managed install when `install_id` in baked store is stale.
   - Writable `XDG_CACHE_HOME` install takes precedence over baked store (protecting test fixtures and corrupt-cache repair).
   - Baked Node resolution prepends baked Node when system Node is absent or below minimum (e.g. Node 18), skips subprocess probes on a repeated `ensure_node_runtime` call when baked Node is already first on `PATH`, and returns `lease_path() == None`.
   - Runtime `install_npm` passes a temporary `.install-npm-cache-*` `--cache` directory and removes it upon completion (leaving zero `_cacache` under `HOME` or `harness_root`).
2. In `mj-harness-install/src/session_image.rs`:
   - `containerfile` with Node-using and native harnesses emits multi-stage `FROM ... AS mj-harnesses`, `COPY --from` after `mise install`, `--cache /tmp/mj-npm-cache` cleanup, Grok zstd decompression fallback, and `LABEL dev.mj.harnesses.*`.
   - Base image with matching `dev.mj.harnesses.*` labels skips Stage 1 completely while still stamping `LABEL dev.mj.harnesses.*` so a pin mismatch changes `image_tag` and triggers partial Stage 1 rebuild of ONLY the mismatched harness (no duplicate `COPY --from` of unchanged harnesses).
   - `session_base_containerfile` bakes all eight `HarnessKind::ALL` cleanly for both `linux/amd64` and `linux/arm64` without creating `/opt/mj-image/recipe`.
3. In `mj-core/src/harness_runtime.rs`:
   - `dsh_acp_launch_script` checks `dsh-acp` on `PATH` first, then the baked `/opt/mj-harnesses/harnesses/dsh/<install_id>/dsh-acp` entrypoint, before `npx`.
4. In `mj-cli/src/image_commands.rs`:
   - Existing `image_build_print_golden` and updated `image_status_golden` pass.

Run targeted tests with `BypassSandbox: true`, `cargo clippy --all-targets -- -D warnings` on edited crates (`brokk-mj-core`, `brokk-mj-harness-install`, `brokk-mj-worker`, `brokk-mj-server`, `brokk-mjolnir`), `rustfmt --edition 2024 --check` on changed files, and `node scripts/check-crate-boundaries.mjs`. Commit on `master`, run `request_review`, resolve/override findings, and push to `origin/master`.

### Milestone 4: Real Podman Verification

Using an isolated `--instance` and `/usr/bin/env -i` (never touching the default daemon or live credentials):
1. Build a real session image with `mj image build` on `docker.io/library/buildpack-deps:noble` with enabled Claude (and/or Codex) profile(s) into a disposable local `registry:2`, verifying the build succeeds, `mj image status` reports `current`, and inspecting the built image confirms `/opt/mj-harnesses` contents, `dev.mj.harnesses.*` labels, and zero `/root/.npm/_cacache`.
2. Start a real container from that image and run `mj-worker` / `prepare-harness` (or real session launch with a synthetic profile) verifying that `prepare exact managed harness before worker startup` completes in under one second with zero downloads and zero cache writes under `$HOME/.cache/mjolnir`.
3. Verify `mj image build --print` using the newly built image as the base skips Stage 1 completely (`FROM ... AS mj-harnesses` absent) while still stamping `dev.mj.harnesses.*` labels.
4. Clean up all temporary containers, registry, images, and isolated daemon. Record exact measured timings in `Outcomes & Retrospective`, update `.agents/plans/remote-wan-perf-epic-1695.md`, and close #1696.

## Concrete Steps

All commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/112f9f78f35b003b036e070e798e1feb` unless noted.

Targeted test and lint suite after implementation (with `BypassSandbox: true` for `cargo test`):

    cargo test -p brokk-mj-core harness_runtime
    cargo test -p brokk-mj-harness-install
    cargo test -p brokk-mj-worker harness_launch discovery
    cargo test -p brokk-mj-server session_images
    cargo test -p brokk-mjolnir image_commands
    cargo clippy -p brokk-mj-core -p brokk-mj-harness-install -p brokk-mj-worker -p brokk-mj-server -p brokk-mjolnir --all-targets -- -D warnings
    rustfmt --edition 2024 --check <changed .rs files>
    node scripts/check-crate-boundaries.mjs

Expect every suite to report zero failures and non-zero executed test counts per filter, clippy clean with `-D warnings`, rustfmt clean, and crate boundaries valid.

## Validation and Acceptance

Acceptance requires all of the following observable outcomes:

1. Automated tests: All targeted unit/integration/golden suites above pass with exact per-filter counts reported, including new tests proving baked harness/Node resolution, stale-pin fallback, writable-cache precedence, temporary npm `--cache` cleanup, multi-stage Containerfile generation for all eight harnesses, base-image label skip + partial rebuild, and `image_tag` sensitivity to pin changes.
2. Live container verification: A real Podman build of the generated Containerfile on `docker.io/library/buildpack-deps:noble` succeeds, leaves zero `_cacache` in the final image, stamps `dev.mj.harnesses.*` labels, causes a subsequent build from that image to omit Stage 1, and reduces real `prepare-harness` execution inside a fresh container from 12–21 s to under 1.0 s with zero network traffic and zero writes under `$HOME/.cache/mjolnir`.
3. Clean pre-push review via `request_review` and push to `origin/master`.

## Idempotence and Recovery

All Containerfile builds are deterministic and content-addressed by `image_tag(&containerfile)`. Re-running `mj image build` against an already-published tag pulls and skips rebuilding cleanly. If a harness pin changes before a user rebuilds their project image, workers fall back seamlessly to runtime managed installation into `$XDG_CACHE_HOME/mjolnir` (now without leaking `_cacache`), and `mj image status` immediately marks the image `out of date`. Live verification uses an isolated `--instance`, disposable local registry, and explicit `finally` cleanup of containers, images, and daemon.

## Artifacts and Notes

Reconnaissance report: `.mj/agents/e70279d962124705e5d35f3c088cfea3/recon-1696.md`.
Empirical baseline timings for #1696 (11.9–21.0 s cold `prepare-harness`, 344 MB `_cacache` leak) are recorded on BrokkAi/mjolnir#1696 and in `.agents/plans/remote-wan-perf-epic-1695.md`.

## Interfaces and Dependencies

In `mj-core/src/config/harness.rs`:

    impl HarnessKind {
        pub const ALL: [Self; 8] = [ ... ];
    }

In `mj-core/src/harness_runtime.rs`:

    pub const BAKED_HARNESS_ROOT: &str = "/opt/mj-harnesses";
    pub const BAKED_NODE_MANIFEST_FILE: &str = "mj-node.json";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct BakedNodeManifest {
        schema: u32,
        version: String,
    }

    pub fn baked_node_manifest_matches(path: &Path, version: &str) -> Result<bool>;
    pub fn baked_harness_install_dir(root: &Path, harness: HarnessKind) -> PathBuf;
    pub fn baked_node_install_dir(root: &Path, folder: &str) -> PathBuf;

In `mj-harness-install/src/harness_install.rs`:

    pub struct ManagedHarness {
        pub command: PathBuf,
        pub args: Vec<String>,
        pub environment: BTreeMap<String, String>,
        pub entrypoint: PathBuf,
        pub cli_entrypoint: PathBuf,
        pub lease_path: Option<PathBuf>,
        pub cache_root: Option<PathBuf>,
        _lease: Option<FileLock>,
        _managed_node: Option<ManagedNodeRuntime>,
    }

    impl ManagedNodeRuntime {
        pub fn path(&self) -> &Path;
        pub fn lease_path(&self) -> Option<&Path>;
    }

In `mj-harness-install/src/session_image.rs`:

    pub const BAKED_NODE_LABEL: &str = "dev.mj.harnesses.node";
    pub const BAKED_HARNESS_LABEL_PREFIX: &str = "dev.mj.harnesses.";

    #[derive(Debug, Clone, Default)]
    pub struct ImageBase {
        pub reference: String,
        pub user: String,
        pub workdir: String,
        pub baked_node: Option<String>,
        pub baked_harnesses: BTreeMap<String, String>,
    }

    pub fn platform_arch(platform: &str) -> Result<&'static str>;
    pub fn containerfile(
        base: &ImageBase,
        platform: &str,
        mise: &MiseArtifact,
        files: &BTreeMap<String, String>,
        harnesses: &[HarnessKind],
    ) -> Result<String>;
    pub fn session_base_containerfile(base: &ImageBase, platform: &str) -> Result<String>;
