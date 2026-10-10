# Native build cache validation, 2026-10-09

The implementation shares native Go, Gradle, Turbo, Nx and Bazel caches and
extends the existing mbx integration to standalone C/C++ projects. It discovers
existing checkout manifests and writes worker-owned configuration and launchers.
It does not change repository build files or install project build systems.

## Real build-tool checks

The ignored integration test
`worker_runtime::tool_cache::tests::real_tools_reuse_invalidate_and_build_concurrently`
creates two independent Git checkouts of the same revision. Each tool must reuse
completed output in the second checkout, rebuild after changing input, and
complete two simultaneous builds with correct outputs. CMake explicitly selects
`/usr/bin/cc`, so its hit also proves configure-time compiler interception.

| Tool | Local raw | Morannon raw | Morannon Podman | Reuse evidence |
| --- | --- | --- | --- | --- |
| Go 1.26.1 | Pass | Pass | Pass | No compiler invocation in second checkout |
| Gradle 9.1.0 | Pass | Pass | Pass | `:compileJava FROM-CACHE` with private Gradle homes |
| Turbo 2.11.7 | Pass | Pass | Pass | Output restored without executing the build script |
| Nx 23.3.0 | Pass | Pass | Pass | Output restored without executing the build script |
| Bazel 8.3.1 / Bazelisk 1.28.1 | Pass | Pass | Pass | Disk cache hit across independent output bases |
| CMake / mbx 1.22.0 | Pass | Pass | Pass | Second build: one hit, zero misses; object reflinked |

Every Podman command runs in a fresh container, including the two simultaneous
builds. The image is `ghcr.io/brokkai/mjolnir/agent-dev:latest`, ID
`46c93615e284a7f358cd6e6b3272377ea44c928a982183017eda491e207cb36a`.
The disposable tool installation supplies Go, Gradle, Nx, Turbo, Bazelisk,
CMake 3.31.6 and mbx. Morannon uses a private copy of the image's JDK 25.0.4.1;
the local raw run uses JDK 21. No host packages were changed.

The local run passed in 71.32 seconds, Morannon raw in 65.09 seconds, and
Morannon Podman in 79.94 seconds. These are correctness checks, not benchmarks.

Evidence is retained at:

- `/mnt/optane/mj-tool-cache-validation/{raw-tools,morannon-raw,morannon-podman}.log`
- `/mnt/optane/mj-tool-cache-validation/raw/build-caches-tpGIJd`
- `morannon:/mnt/nvme/mj-tool-cache-validation/raw/build-caches-bLMZ9o`
- `morannon:/mnt/nvme/mj-tool-cache-validation/podman/build-caches-JLhxzc`

To rerun, supply the required tools on PATH, `JAVA_HOME`, a local-disk
`MJ_CACHE_TEST_ROOT`, and `MJ_CACHE_TEST_NODE_MODULES` containing Nx/Turbo/Bazelisk.
Run `cargo test -p brokk-mj-worker --lib real_tools_reuse_invalidate_and_build_concurrently -- --ignored --nocapture`.
For Morannon, build the test executable with `--target x86_64-unknown-linux-musl`,
copy it to the host, and set `MJ_CACHE_TEST_PODMAN_IMAGE` for the container run.
The tools directory and fixture root are bind mounted into each container.

## Mjolnir provisioning checks

A separate `--instance tool-cache-c9c409` uses disposable config, data, repository
and fake ACP profile homes. The daemon, worker and Git/Podman operations are real;
no agent prompts or model requests are needed. A disposable Git daemon serves
only the fixture repository. Its container URL uses `host.containers.internal`
because rootless Podman does not route the host's own LAN address back to it.

The user-shell action checks `MJ_TOOL_CACHE_DIR`, `GOCACHE` and `GOFLAGS`, runs
`go build`, and executes the resulting binary. Both sessions returned
`NATIVE_CACHE_SMOKE_OK`:

- Raw: `2db89627d2c063b96a9772a8b9c1d4b6`, cache at
  `/mnt/optane/mj-tool-cache-validation/session-native-cache/go`.
- Morannon Podman: `dba9d05b30dcebe01abd455a6dcd4a4b`, cache at
  `/mnt/nvme/mj-tool-cache-validation/session-native-cache/go`.

Transcripts are `/mnt/optane/mj-tool-cache-validation/session-localhost.json`
and `session-remote.json`. The driver is `session_smoke.py` in the same artifact
root. All disposable sessions and their containers were destroyed, the isolated
daemons stopped, and the fixture Git daemon stopped. Label-scoped Podman checks
found no remaining test containers. Live session data and profile homes were
not used.

## Regression coverage

The regular shell integration covers literal spaces, quotes and dollar signs,
explicit Bazel flags, repeated preparation without recursive launchers, one
BASH_ENV owner shared with Git, and preparing another checkout from an already
prepared environment. Database integration preserves recorded mount placement
across lifecycle writes and rejects older writers that would lose it. A failed
mount preparation cannot publish a partial mount set. The Settings golden and
its 68 interaction/layout checks pass.

Final affected-crate suites and Clippy results are recorded in the ExecPlan.
