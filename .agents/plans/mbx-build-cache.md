# Share an mbx build cache with Rust container sessions

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Today every Mjolnir (mj) container session starts with an empty Cargo target directory, so the first build of a Rust project compiles every dependency from scratch, even when another session on the same machine built the same code an hour earlier. After this change, a Rust session that runs in a container shares a build cache with every other mj container on the same host, and with the user's own native builds when the user has installed mbx natively. The second session on a project finds most compiler outputs already cached and builds in a fraction of the cold time.

mbx ("Mr. Boxington", source in the sibling repository `../mr-boxington`) is a Rust build cache. It wraps Cargo: when a program named `cargo` that is really the `mbx` binary runs, mbx starts Cargo with its own compiler wrapper, looks up each compiler invocation in a content-addressed store, and restores cached outputs instead of recompiling. Its cache is an ordinary directory, called the cache directory in this plan.

The user sees two new settings and no prompts. A global switch, "Enable MBX for Rust builds", turns the feature on or off everywhere. Each container target can override three values: whether mbx is enabled on that target, where the cache directory lives on that target's host, and how large the cache may grow. With no overrides, mj picks sensible defaults per host, described below. To see it working, create two sessions on the same Rust repository on one container target, run `cargo build` in the first, then run `cargo build` in the second: the second build reports cache hits (visible with `mbx stats` inside the session) and finishes much faster.

## Progress

- [x] (2026-09-16) Design settled with the user; this ExecPlan written.
- [x] (2026-09-16) Milestone 0 on morannon (rootless Podman 5.7.0, cache on ZFS): ownership, session separation, mount path and garbage collection, limits approach, speed. All five answered; see Surprises & Discoveries and Decision Log.
- [ ] Milestone 0 on localhost: blocked. Rootless Podman cannot run in the current WSL session because `/run/user/1000` does not exist (`loginctl`: "User ID 1000 is not logged in or lingering"). Needs the user to enable a systemd user session or lingering; repeat the morannon checks afterwards against `/mnt/optane/mbx-cache`.
- [ ] Milestone 1: configuration types and setup screens.
- [ ] Milestone 1: host cache resolution, limits, and reflink probe (`mj-controller/src/controller/mbx.rs`).
- [ ] Milestone 1: pinned mbx binary download and installation into containers.
- [ ] Milestone 1: Rust detection, session record flag, and migration.
- [ ] Milestone 1: three-way mount access mode (read-only, copy-on-write, read-write) in the model, database, container arguments, and the attached-directory editors. (2026-09-16: implemented and validated in the working tree together with unconditional Podman `keep-id`; awaiting review fixes and commit.)
- [x] (2026-09-16) Milestone 1a: per-session container workspace path. New sessions record `/workspace/<session id>` (`SessionRecord.container_workspace`, compatible migration 35); sessions created earlier keep `/workspace`; children use the parent's path; orphan adoption probes the container for the path. Consequence for 1b: mbx is enabled only for sessions with a recorded per-session workspace, because legacy sessions could still collide.
- [ ] Milestone 1: mbx cache mount and environment.
- [ ] Milestone 1: native mbx version check against the pin.
- [ ] Milestone 1: end-to-end validation on localhost and morannon.

## Surprises & Discoveries

- Observation: the user's native mbx cache is not in the default location.
  Evidence: `mbx cache dir` prints `/mnt/optane/mbx-cache/actions`; `~/.config/mbx/config.toml` sets `cache_dir = "/mnt/optane/mbx-cache"`, `gc.max_size = "500GiB"`, `target.root = "/mnt/optane/mbx-targets"`. `/mnt/optane` is XFS with a 331 GB store (56,706 CAS files). `~/.cache/mbx` also exists but is an older, unused store on ext4. Lesson: only mbx itself resolves its cache directory reliably.

- Observation: hosts differ in which filesystems support reflinks, so one global cache location cannot work.
  Evidence: morannon's root filesystem is ext4 (no reflinks), while `/mnt/nvme` on morannon is ZFS 2.4.1 with `/sys/module/zfs/parameters/zfs_bclone_enabled` = 1. `/mnt/nvme/mbx` already exists and contains only three empty directories (`.agents`, `.codex`, `.git`, dated Sep 13); they must be left alone.

- Observation: mj has never had a read-write attached directory.
  Evidence: `git log` shows `ad825512` ("Persist additional container mount points") introduced Podman `:O` copy-on-write overlay mounts and Apple read-only binds, and `a2b78ff6` ("Add read-only attached directories with automatic overlay fallback") added `read_only`. No commit on any branch contains a `:rw` attached-mount form. Writes inside the overlay never reach the host.

- Observation: mbx keys per-checkout state by the absolute workspace path, and every mj container uses the same path.
  Evidence: `CONTAINER_WORKSPACE` is `/workspace` in `mj-core/src/targets.rs`. mbx names managed target directories with `CacheDigest::blake3(workspace_root)` (`../mr-boxington/crates/mbx/src/target.rs`, around line 986) and incremental state with a 16-character prefix of the same digest (`crates/mbx/src/incremental.rs`, around line 266).

- Observation: without a user-namespace mapping, the container user cannot write to a host directory at all; with `--userns=keep-id` it writes files owned by the host user.
  Evidence (morannon): `podman run -v $C:$C:rw IMG touch $C/no-keepid` gives `touch: cannot touch ...: Permission denied`. With `--userns=keep-id`, `ls -ln` shows the new file and directory owned by `1000 1000`. The image user `hel` is UID 1000, which happens to equal the host user's UID on morannon; `/etc/subuid` maps `jonathan:100000:65536`.

- Observation: two sessions at the same container path share one managed target directory, confirming the collision; a different checkout path or a per-session target root separates them.
  Evidence: containers A and B at `/workspace/proj` both got `target -> .../targets/v1/f0be10cad904...`, and B's build did no work. With `MBX_TARGET_ROOT=$C/mj-targets/sess-a` (and `sess-b`) plus `MBX_INCREMENTAL=1`, concurrent builds produced `target -> .../mj-targets/sess-a/v1/f0be10...` and `.../mj-targets/sess-b/v1/f0be10...`, each with `43 compilations replayed from cache` in 0.69 s wall.

- Observation: the cache works and is fast with reflinks on ZFS.
  Evidence: a small project (anyhow, serde, serde_json, regex, clap) built cold with an empty cache in `real 0m7.872s` (`user 0m29.963s`); the same project at a different path against the shared cache took `real 0m0.368s` with `cache hits 44` and `copying avoided 178.8 MiB reflinked`.

- Observation: garbage collection with a per-session target root cannot reach other sessions' targets.
  Evidence: host-side `mbx gc --dry-run` with `MBX_TARGET_MAX_AGE=1s` listed only the two directories in the default `targets/` root and nothing under `mj-targets/`; container A's dry run with its own root listed only `1 target directories`, and evicted 0 store objects under its default store budget.

- Observation: mbx has no command that prints its effective configuration.
  Evidence: `mbx config --help` is forwarded to Cargo (`Usage: cargo config`). `crates/mbx/src/cli/` has no config subcommand. Environment variables override the config file for every key (`crates/mbx/src/config.rs`).

- Observation: `--userns=keep-id` is compatible with the options mj already uses.
  Evidence: one `podman run --init --userns=keep-id` with `--volume VOL:/workspace:rw,U`, an `:O` overlay attachment, and the `:rw` cache mount: the workspace file is owned by `1000 1000`, the overlay write is visible inside the container but the host file still contains only `hi`, and the cache probe file is owned by `1000 1000`.

- Observation: mbx accepts `/mnt/nvme/mbx` with its pre-existing directories and detects reflinks there.
  Evidence: `MBX_CACHE_DIR=/mnt/nvme/mbx mbx doctor` reports `ok cache /mnt/nvme/mbx is writable`, `ok reflink ... cloning is supported`, `ok config 240.0 GiB budget`; afterwards `ls -la` shows only `.agents`, `.codex`, `.git` (the `cargo`/`rustc` failures are because morannon's host has no Rust toolchain).

- Observation: mbx's learned incremental reuse engages on the first edit of a workspace crate, not after three misses.
  Evidence: `WORKSPACE_HOT_STREAK_THRESHOLD: u32 = 1` (`../mr-boxington/crates/mbx/src/rustc.rs:46`); `HOT_STREAK_THRESHOLD = 3` applies only to crates outside the workspace. After an edit, build 2 compiles with private incremental state and build 3 reuses it. `MBX_INCREMENTAL=1` disables learned reuse entirely (`crates/mbx/src/cli/cargo.rs:142-144`), so every workspace edit becomes a full recompile of that crate and everything above it. Disabling it costs hours-long agent sessions real time.

- Observation: a per-session `MBX_TARGET_ROOT` separates only one of several path-keyed records. With every container at `/workspace/proj`, these still collide: `incremental/<blake3(path)[..16]>/` (rustc `-Cincremental` session directories shared by concurrent containers, churn counters overwritten, `remove_dir_all` when a unit exceeds `learned_incremental_max_size`, and `mbx clean` deleting another session's state), `actions/checkouts/v1/<identity>/<blake3(path)>.json` and `build-receipts/v1/checkouts/<blake3(path)>.json` (last-writer-wins; harmless for GC roots but `mbx stats`, `mbx projects`, `mbx clean`, and `mbx cache export` then read or delete another session's state). mbx has no namespace or salt for local checkout identity. Per-session target roots are collected by nobody, since each mbx process collects only the root it was given.
  Evidence: `crates/mbx/src/session.rs:237` (incremental root is `cache_dir/incremental`, unaffected by `MBX_TARGET_ROOT`), `crates/mbx/src/incremental.rs:265-294` (path-keyed directory; leases only block GC, never a second builder), `crates/mbx/src/rustc.rs:331-335` (learned compilations skip the flight lock), `crates/mbx-cache-store/src/lib.rs:1382-1421` (checkout records, "no lock" by design), `crates/mbx/src/cli/gc.rs:55-63`.

- Observation: host-side mbx judges container checkouts dead when the cache shares a device with `/`.
  Evidence: `checkout_is_live_on` (`crates/mbx-cache-store/src/lib.rs:1815-1837`) treats `/workspace/...` as deleted when its nearest existing ancestor is on the store's device. Cost is lost blob protection (recompiles), never wrong builds. On this host `/mnt/optane` is a separate device, so it does not apply; morannon has no native mbx.

- Observation: `--userns=keep-id:uid=0,gid=0` keeps a root image's process as root, but plain `--userns=keep-id` demotes it to the host uid.
  Evidence (localhost, Podman 5.7.0, `ubuntu:24.04`): `podman run --userns=keep-id:uid=0,gid=0 ubuntu:24.04 id` prints `uid=0(root)`; `podman run --userns=keep-id ubuntu:24.04 id -u` prints `1000`. In all three variants (no `--userns`, plain keep-id, `keep-id:uid=0`) a file the container writes to a host-owned bind mount is owned by the host user.

## Decision Log

- Decision: each container host keeps one ordinary mbx cache directory, and that host's mj containers mount it read-write. There is no synchronization between hosts.
  Rationale: an earlier design gave every session a private cache, seeded it at start by exporting from a host store, and imported results back at close. That required a new upstream mbx export mode (existing `mbx cache export` selects only by build receipts, which a new session does not have), per-host mirrors, controller-side import and garbage collection, and copying many gigabytes per session (the user's store is 331 GB). Every piece existed only to avoid a shared read-write mount. A shared mount of a normal cache needs none of it and mbx already supports concurrent processes sharing one cache (atomic renames and kernel file locks). The user judged cross-host sharing to be of little value.
  Date/Author: 2026-09-16, user and agent.

- Decision: mj manages mbx only in container sessions (local and SSH Podman and Docker). Bare ("raw") sessions and AWS EC2 sessions are out of scope.
  Rationale: a bare session runs in the user's own checkout, where a native mbx install (or none) is the user's choice. mj should not install a Cargo shim there.
  Date/Author: 2026-09-16, user.

- Decision: settings are a global switch plus per-target overrides of enabled, directory, and size. With no override, the directory is the host's native mbx cache if one exists, otherwise `~/.cache/mbx`; the size is the host's native limit if one exists, otherwise the smaller of 100 GB and one quarter of the free space on that volume; the target is enabled only when the resolved directory's filesystem supports reflinks. mj never prompts.
  Rationale: the user wants the feature to be invisible when defaults fit and adjustable when they do not. Reflinks (copy-on-write file clones) are what make restoring cached outputs nearly free; without them mbx copies bytes, which is still correct but slower, so a host without them defaults to off. Per-target overrides exist because morannon needs `/mnt/nvme/mbx` on ZFS instead of its ext4 home directory.
  Date/Author: 2026-09-16, user.

- Decision: the mbx binary used inside containers is a pinned upstream release (static musl build), downloaded and checksum-verified by the controller on first use, with `MJ_MBX_BINARY` as a local override.
  Rationale: mbx releases independently of mj; bundling it into mj releases would couple the two. Baking it into the default image would not help custom images.
  Date/Author: 2026-09-16, user.

- Decision: any failure in the mbx path (download, probe, detection, mount) makes the session run without mbx and logs the reason; it never fails or blocks session creation, and close-time work never delays removing the session entry.
  Rationale: the cache is an optimization. The repository requires background work off the UI loops, and the user specifically asked that close-time work not hold up clearing the session.
  Date/Author: 2026-09-16, user.

- Decision: attached directories get a user-visible access mode with three choices: read-only (`ro`, the default for newly added directories), copy-on-write (`cow`, today's overlay), and read-write (`rw`, a plain bind mount whose writes reach the host). Existing stored mounts keep their current behavior: `read_only = true` stays read-only and `read_only = false` stays copy-on-write.
  Rationale: the mbx cache needs a read-write mount, and once that mode exists in the model, the user wants it available for any attached directory. Read-only becomes the default for new directories because it is the safest choice. Existing sessions must not change behavior on upgrade.
  Date/Author: 2026-09-16, user.

- Decision (SUPERSEDED, see the OPEN entry below): separate sessions with `MBX_TARGET_ROOT=<cache>/mj-targets/<session id>` and `MBX_INCREMENTAL=1`; do not change the container workspace path.
  Rationale at the time: the prototype showed this separates concurrent sessions' target directories at the same `/workspace` path. Why it was dropped: the code dive above showed that learned incremental engages on the first edit (so `MBX_INCREMENTAL=1` is a real loss), that `incremental/` and the checkout and receipt records still collide on the workspace path, and that per-session target roots need an mj-side reaper, which the user does not want.
  Date/Author: 2026-09-16, agent; superseded the same day.

- Decision: option A. Each session gets its own container workspace path so that concurrent sessions' path-keyed mbx state never collides.
  Option A: give each session its own container workspace path, for example `/workspace/<session id>/<directory>`, stored on the session record so containers created before the change keep `/workspace`. mbx then sees genuinely distinct checkouts and needs no special handling: learned incremental stays on, managed targets live under mbx's normal root and are collected by its normal age and size rules, `mbx stats` and `mbx projects` are truthful, and containers receive only `MBX_CACHE_DIR` plus the host configuration described below. Cost: `CONTAINER_WORKSPACE` is used by clone commands, volume arguments, worker launch, checkpoints, and move, plus about 45 literal `/workspace` strings; this is a medium refactor that should land as its own commit with its own tests before the mbx work.
  Option B: keep `/workspace` and bind-mount `<cache>/mj-incremental/<session id>` over `<cache>/incremental` inside the container, keep the per-session target root, and add the reaper. Fixes only the dangerous collision; the metadata collisions remain and mbx's inspection commands cannot be trusted inside sessions.
  Rationale for recommending A: the user's stated preferences are one mechanism rather than two and letting mbx do its normal work; every part of B is a workaround carried forever.
  Date/Author: 2026-09-16, user accepted the agent's recommendation of A.

- Decision: mount the cache at the same absolute path inside the container as on the host.
  Rationale: records mbx writes (target records, cache paths) then name paths that exist on both sides. `validate_mount_destination` accepts any absolute path without `..`.
  Date/Author: 2026-09-16, agent.

- Decision: every Podman session container (local and SSH) runs with `--userns=keep-id:uid=<image user uid>,gid=<image user gid>`, regardless of its mounts. mj learns the image user's numeric ids before creating the container by running `id -u; id -g` in a throwaway container of that image with the same `--pull` policy the launch will use, cached per host and image reference for the daemon's lifetime. If the probe fails, the container runs with no `--userns` option at all (Podman's default mapping, as before this change) and the user gets a notice. The `uid=`/`gid=` form needs Podman 4.3.0, so the preflight floor and the docs move from 4.0.0 to 4.3.0.
  Rationale: without the mapping the container user cannot write to a host-owned directory at all, which makes both the mbx cache and a user's `rw` attachment unusable. The user prefers one code path to a mount-dependent one. The fallback is "no option" rather than plain `keep-id` because plain `keep-id` demotes a root image's process to the host uid (see Surprises). The explicit ids are needed because the host uid matches `hel` only by coincidence on morannon. Resume and move both re-enter the same provisioning path, so nothing is stored on the session.
  Date/Author: 2026-09-16, user and agent.

- Decision: give containers the host's mbx limits by mounting the host's mbx config file read-only at the container user's `~/.config/mbx/config.toml` when it exists, and set `MBX_CACHE_DIR` in the environment. When that file relocates `target.root` outside the cache directory (this host uses `/mnt/optane/mbx-targets`), mount that directory read-write at the same path too, so the container's layout is the host's layout. A per-target `max_size` override is passed as `MBX_GC_MAX_SIZE`. Without a host config file, pass `MBX_GC_MAX_SIZE` from the mj setting or its default formula.
  Rationale: mbx has no command that prints effective configuration, and reimplementing its config parsing in mj would drift. Reading the same file gives containers exactly the host's budgets. The user accepts that the file may contain remote-cache settings.
  Date/Author: 2026-09-16, user and agent.

- Decision: mj does not run, schedule, or second-guess mbx garbage collection. Automatic GC inside containers and the host's own mbx are the only collectors; a closed session's managed target ages out or is evicted by size under mbx's normal rules. mj adds no reaper.
  Rationale: the user's instruction. This is workable because of the per-session workspace path: managed targets live under mbx's normal root, where its age and size rules see them.
  Date/Author: 2026-09-16, user.

- Decision: Rust detection is `HEAD:Cargo.toml` at the root of the primary repository's host mirror, and nothing more. Repositories whose Cargo workspace lives in a subdirectory, and sessions whose Git cache preparation failed, run without mbx.
  Rationale: acceptable coverage for the first version; the user accepted the limitation.
  Date/Author: 2026-09-16, user.

- Decision: mj checks the host's native mbx version against the pinned version and does not use a native cache whose mbx is older than the pin; it logs why and runs the session without mbx. This host was upgraded to mbx 1.12.0 (the pin) on 2026-09-16 with `cargo install mbx --locked --version 1.12.0`; `mbx doctor` against `/mnt/optane/mbx-cache` reports 0 failures.
  Rationale: containers run the pinned mbx; an older native mbx writing the same store is the one version-skew case the user wants excluded rather than trusted to mbx's forward compatibility.
  Date/Author: 2026-09-16, user.

## Outcomes & Retrospective

Milestone 0 (morannon): a shared read-write cache is safe and effective under rootless Podman once the user namespace is mapped and each session has its own target root. The second build of a small project dropped from 7.9 s to 0.4 s with 44 cache hits and all restored output reflinked. Localhost validation is still pending because rootless Podman cannot run in the current WSL session.

## Context and Orientation

The workspace is a Rust Cargo workspace. The pieces this plan touches:

Configuration. `mj-core/src/config.rs` defines `struct Config`, loaded from `config_dir()/config.toml` (`MJ_CONFIG_DIR` overrides the directory). It uses `#[serde(deny_unknown_fields)]`, and every optional section uses `#[serde(default, skip_serializing_if = ...)]` so older files still load. Writes go through `Config::update`, a locked load-edit-validate-save. User-facing targets are the enum `mj_core::config::TargetTemplate`; the container kinds (`LocalPodman`, `LocalDocker`, `AppleContainer`, and the SSH Podman and Docker kinds) embed `ContainerTemplate` with `#[serde(flatten)]`. `backend_target()` in `mj-controller/src/controller/backend.rs` converts them to the runtime enum `mj_core::targets::TargetTemplate`. The TUI setup dialog is `mj-tui/src/setup.rs`; the plain-terminal first-run setup is `mj-controller/src/setup.rs`; saving from the dashboard happens in `mj-cli/src/dashboard/io.rs` (see `spawn_spinner_style_save` for the simplest background save).

Provisioning. A new session is provisioned by `Controller::provision_session_controlled_with_commit` in `mj-controller/src/controller/provisioning.rs`. Its target step computes the runtime target, the list of mounts the container will run with (`runtime_mounts`), prepares the host-side Git clone cache with `git_cache::prepare`, and builds a provisioning plan with `targets::provision_plan`. Then two lanes run concurrently: cloning repositories, and `install_worker_payload`, which copies the worker binary and its `launch.json` into the target. Finally the worker starts.

Git clone cache. `mj-controller/src/controller/git_cache.rs` is the model for this feature. It keeps mirrors under `~/.cache/mjolnir/git` on the container host, reaches that host through a `CacheHost` enum (local Podman, local Docker, Apple, SSH Podman, SSH Docker) whose `command` and `shell_command` methods run a command locally or over SSH, adds a read-only mount to `runtime_mounts`, and treats every failure as a notice rather than an error.

Container arguments. `container_run_args` in `mj-controller/src/targets.rs` builds `podman run` / `docker run` arguments. Each `AdditionalMount { source, destination, read_only }` (defined in `mj-core/src/targets.rs`) becomes a Podman `--volume src:dst:O` (overlay) or `:ro`, a Docker overlay volume or `:ro`, or an Apple read-only bind. `enforce_overlay_capable_mounts` in `provisioning.rs` uses `probe_filesystem_types` (runs `stat -f` on the container host) and `overlay_unsupported_filesystem` to force unsafe filesystems read-only.

Worker environment. `worker_launch_config()` in `mj-controller/src/controller/worker_binary.rs` builds `WorkerLaunchConfig` (in `mj-core/src/worker_launch.rs`), whose `environment` map becomes the harness environment inside the target. The worker rebuilds a clean login environment (`mj-core/src/login_environment.rs`) and then applies these values. `install_worker_files` in the same file copies files into containers with `podman cp` / `docker cp`; `download_worker` shows how a pinned download with a SHA-256 check is done.

Session records and migrations. `SessionRecord` is in `mj-core/src/state.rs` and persisted in SQLite. Every schema change must be classified as compatible or breaking beside the migration, advance the migration revision, and be tested with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` if breaking (see the repository `CLAUDE.md`).

mbx terms used here. The cache directory (`MBX_CACHE_DIR`) contains `actions/`, the store, whose portable parts are `cas/v1` (content-addressed output blobs), `action-results/v1` (records mapping a compiler action to its blobs), and `task-manifests/v1` (per-build predictions, merged under file locks). Outside `actions/` are `targets/` (managed target directories: when a checkout has no `target/`, mbx makes `target` a symbolic link into this tree), `incremental/` (rustc incremental state per checkout), and `scheduler/` (a CPU and memory pool shared by all mbx processes using the directory). Garbage collection ("GC") removes old entries: `gc.max_size` bounds the store, `target.max_size` and `target.max_age` bound managed targets, and it runs automatically after a build at most once per `gc.interval`. A checkout whose path no longer exists is considered deleted only if the nearest existing ancestor of its path is on the same device as the store (`checkout_is_live_on` in `crates/mbx-cache-store/src/lib.rs`); mbx then removes that checkout's managed target and incremental state. A reflink is a copy-on-write clone of a file that shares disk blocks until modified; XFS, Btrfs, and ZFS with block cloning support it, ext4 does not, and the `FICLONE` system call fails across two different mounts even on the same filesystem.

## Plan of Work

### Milestone 0: prototype the shared mount by hand

This milestone writes no mj code. At its end, the Decision Log records concrete container run options and mbx environment variables that make a shared read-write cache safe on localhost (rootless Podman, cache on XFS at `/mnt/optane/mbx-cache`) and on morannon (rootless Podman over SSH, cache on ZFS at `/mnt/nvme/mbx`). Use the default image `ghcr.io/brokkai/mjolnir/agent-dev:latest`, whose agent runs as the non-root user `hel`, and the mbx release that milestone 1 will pin (musl build, copied in with `podman cp`). Use a scratch copy of a real Rust project, such as this repository, as the build input. Remove prototype containers and scratch directories when finished; do not delete anything that already existed in either cache directory.

Answer five questions, in order, recording the evidence in `Surprises & Discoveries`:

1. Ownership. Find run options under which `hel` can create files in a host directory owned by the host user and the created files are owned by the host user on the host. Try `--userns=keep-id` (Podman maps the host user's UID and GID to the same numbers in the container; check whether `hel`'s UID matches, and if not try `--userns=keep-id:uid=<hel uid>,gid=<hel gid>`). For Docker, record the equivalent (`--user` with the host UID) but Docker hosts are not available for testing, so mark it untested. Confirm that host-side native `mbx gc --dry-run` on localhost can read the new entries.

2. Separating sessions. With two containers mounting the same cache and each holding a checkout at the same container path, start builds in both at once. Determine whether they share a managed target directory and incremental state (they are expected to, given the path-keyed digests). Then try, in order of preference: (a) giving each container a unique checkout path by setting the workspace to `/workspace/<session id>` or similar; (b) keeping `/workspace` and setting `MBX_TARGET_ROOT` to a per-session directory inside the cache mount plus a per-session bind mount over `incremental/`. Prefer the option that keeps the managed target inside the same mount as `actions/cas` so that reflinks work, and that needs the fewest mj changes. Note that changing `CONTAINER_WORKSPACE` affects resuming containers created before the change; if (a) wins, the path must be recorded per session so old containers keep `/workspace`.

3. Mount path and GC safety. Mount the cache at the same absolute path inside the container as on the host (for example `/mnt/optane/mbx-cache`), so records written by either side name paths that exist on both. Check `validate_mount_destination` in `mj-core/src/targets.rs` permits it. Then deliberately trigger GC (`mbx gc` with a small `--max-size`, and a short `MBX_TARGET_MAX_AGE` if such a variable exists; check `docs/cli/configuration.md` in mbx) from the host and from a container, and confirm that neither removes the other's live managed target. On morannon the default `~/.cache/mbx` would be on the same device as `/`, the dangerous case; with `/mnt/nvme/mbx` it is not. If any case is unsafe, decide between disabling automatic target and incremental GC in containers (record the variable) and having mj delete a session's managed target at close.

4. Limits. Containers cannot read the host's `~/.config/mbx/config.toml`. Determine how to learn a host's effective limits (look for a command such as `mbx config` or a `--json` output in `crates/mbx/src/cli`; otherwise read the TOML file directly and apply environment variables on top) and confirm that passing them as `MBX_GC_MAX_SIZE`, `MBX_TARGET_MAX_SIZE`, and so on prevents a container's GC from shrinking the store below the host's budget.

5. Speed. On localhost, build the project cold in container A, then in container B, and record both wall times, the hit count from `mbx stats`, and the reflinked byte count. Repeat once on morannon.

Promotion criterion: milestone 1 proceeds only if questions 1 to 3 have safe answers on both hosts. If ownership cannot be made to work under rootless Podman, stop and record the finding before writing code.

### Milestone 1: implement

At the end of this milestone the two settings exist, and a Rust container session on an enabled target gets the shared cache automatically.

Configuration. In `mj-core/src/config.rs`, add a top-level optional section `build_cache` with `enabled: bool` (default true, meaning "Enable MBX for Rust builds"). Add to `ContainerTemplate` an optional `build_cache` field whose type has three optional fields, `enabled`, `directory` (an absolute path on the target's host), and `max_size` (a byte count; reuse an existing size type if the config already parses sizes, otherwise store bytes as `u64` and parse human sizes at the edges). Both must follow the `default` plus `skip_serializing_if` pattern so existing configs load and are not rewritten. Carry the per-target field into the runtime `ContainerTemplate` through `backend_target()`. Show the global switch in `mj-tui/src/setup.rs` and `mj-controller/src/setup.rs`, and the three overrides in each container target's editor, showing resolved defaults as placeholder text where known.

Host resolution. Create `mj-controller/src/controller/mbx.rs`, next to `git_cache.rs`, reusing `git_cache`'s `CacheHost` (move it to a shared place if needed rather than copying it). It resolves, for one target: the cache directory (target override, else native `mbx cache dir --json` on that host, else `~/.cache/mbx` expanded on that host), the limits (target override, else the host's native limits found in milestone 0, else min(100 GB, free space / 4) computed from `df -B1 -P` on the nearest existing ancestor of the directory), and whether reflinks work (create two temporary files in the directory or its nearest existing ancestor and attempt a clone, as `reflink_check_with` in `../mr-boxington/crates/mbx/src/doctor.rs` does; `cp --reflink=always` is an acceptable shell equivalent). Cache the resolution per target in controller state with a reasonable lifetime so it does not run for every session. All of this runs on supervised background tasks.

Binary. Pin an mbx version and the SHA-256 digests of `mbx-x86_64-unknown-linux-musl.tar.gz` and `mbx-aarch64-unknown-linux-musl.tar.gz` from the upstream GitHub release (`jdx/mr-boxington`). Download on first use into `data_dir()/mbx/<version>/<triple>/mbx`, verifying the digest, following `download_worker`. `MJ_MBX_BINARY` overrides it. Install `mbx` and a `cargo` hard link or copy of it into a `bin` directory next to the worker files in the container, alongside `install_worker_files`, and do the same on the worker refresh and resume paths. The shim works because mbx checks whether it was invoked under the name `cargo`, removes its own directory from `PATH`, and runs the real Cargo.

Environment. When the session uses mbx, add to `WorkerLaunchConfig.environment`: `PATH` with the mbx `bin` directory first, `MBX_CACHE_DIR` set to the cache path (identical inside and outside the container), and `MBX_GC_MAX_SIZE` when mj supplies the budget. When the host has an mbx config file, add a read-only mount of it at the container user's `~/.config/mbx/config.toml`, and a read-write mount of a relocated `target.root` at its own path. Nothing else: no `MBX_TARGET_ROOT`, no `MBX_INCREMENTAL`. This relies on the per-session workspace path from milestone 1a. Confirm with a test that the login environment rebuild keeps the `PATH` prefix.

Detection and the session record. Mounts are fixed when a container is created, so detection must precede `targets::provision_plan`. After `git_cache::prepare`, check the primary repository's host mirror with `git --git-dir <mirror> cat-file -e HEAD:Cargo.toml` through `CacheHost`. If the mirror is unavailable, the session runs without mbx. Add a field to `SessionRecord` recording the decision and the resolved directory so resume and move use the same values; write the migration and classify it (an added nullable column read as "no mbx" by older code is expected to be compatible, but apply the repository's rule that uncertainty means breaking).

Mount access mode. Replace the boolean `AdditionalMount::read_only` in `mj-core/src/targets.rs` with an access mode enum whose variants are read-only, copy-on-write, and read-write, serialized as `ro`, `cow`, and `rw`. Deserialization must still accept existing JSON that has `read_only: true` (read-only) or `read_only: false` or no field (copy-on-write), because session archives, move operations (`mj-controller/src/server.rs`, `source_additional_mounts`), and web API requests carry this shape. In the database, `session_mounts` has an additive `read_only INTEGER` column (`ensure_session_mount_read_only_column` in `mj-controller/src/database/schema.rs`). Add the new mode in a way older builds can still read, keep writing `read_only` consistently for them, and classify the migration beside it; if an older build rewriting a session's mounts would silently turn `rw` into `cow`, treat the change as breaking under the repository rules.

In `container_run_args`, read-write becomes Podman `--volume src:dst:rw` and Docker `--volume src:dst` (a plain bind, not the overlay volume); copy-on-write and read-only keep today's forms. Apple `container` supports only read-only today; check whether its `--mount type=bind` accepts a writable bind, and if not, reject copy-on-write and read-write for Apple targets with a clear message. `enforce_overlay_capable_mounts` keeps downgrading only copy-on-write mounts on filesystems that cannot host an overlay; read-write binds do not need the overlay and are not downgraded.

Attached-directory editors. In `mj-tui/src/wizards.rs`, the mount editor currently has a read-only checkbox (`read_only`, `toggle_read_only`, `forced_read_only`, `read_only_marker`) used by the session wizards and the Ctrl+E dialog. Replace it with a three-way selector (`ro` default, `cow`, `rw`); when the filesystem probe forces a downgrade, lock the selector on `ro` and show the existing reason. The list marker shows ` · ro`, ` · cow`, or ` · rw`. Update the save path in `mj-cli/src/dashboard/io.rs` and any web UI form that edits attached directories to carry the mode.

mbx cache mount. Podman containers already run with the image user mapped onto the host user (see the Decision Log; implemented with the access-mode change). For Docker, pass `--user <host uid>:<host gid>` only if the image user differs; this path is untested because no Docker host is available, and the Decision Log should say so when implemented. Push a read-write cache mount into `runtime_mounts` in `provisioning.rs`, as `git_cache` does, rather than storing it as a user mount. Skip mbx on Apple `container`, on Docker Desktop VMs, and when `probe_filesystem_types` reports a filesystem for which `overlay_unsupported_filesystem` returns a network, FUSE, virtiofs, or 9p reason.

Close. Nothing. mbx's own garbage collection handles a closed session's state (Decision Log).

## Concrete Steps

Prototype commands are run from a scratch directory under the session scratchpad, not the repository. Illustrative localhost commands (adjust after reading results):

    podman pull ghcr.io/brokkai/mjolnir/agent-dev:latest
    podman run --rm ghcr.io/brokkai/mjolnir/agent-dev:latest id
    podman run -d --name mbx-proto-a --userns=keep-id \
        -v /mnt/optane/mbx-cache:/mnt/optane/mbx-cache:rw \
        ghcr.io/brokkai/mjolnir/agent-dev:latest sleep infinity
    podman cp mbx mbx-proto-a:/usr/local/bin/mbx
    podman exec mbx-proto-a sh -c 'touch /mnt/optane/mbx-cache/.proto && ls -ln /mnt/optane/mbx-cache/.proto'
    ls -ln /mnt/optane/mbx-cache/.proto

Expected: the file is owned by the host user's UID on the host. Remove `.proto` afterwards.

Milestone 1 validation, from the repository root:

    cargo test
    cargo clippy --all-targets -- -D warnings

Run `cargo test` outside the restricted sandbox (the suite uses loopback sockets).

## Validation and Acceptance

Unit tests, colocated in `#[cfg(test)] mod tests` blocks: target resolution with and without a native mbx (hand-written fake `CommandExecutor` returning canned `mbx cache dir --json`, `df`, and reflink probe output); the default size formula; the precedence of global switch, target `enabled`, and reflink default; deserializing old `AdditionalMount` JSON (`read_only` true, false, and absent) and old database rows into the right access mode; `container_run_args` output for each engine and each access mode; the mount editor's selector transitions (default `ro`, cycling, locked when forced); and the provisioning decision (Rust or not, enabled or not, supported engine and filesystem) exercised as a state transition.

End to end on localhost (local Podman target, native cache on `/mnt/optane`): create two sessions on the same Rust repository. In the first, run `cargo build`; in the second, run `cargo build`. The second reports hits in `mbx stats`, its wall time is well below the first's, and new files under `/mnt/optane/mbx-cache/actions/cas/v1` are owned by the host user. Run both builds at the same time and confirm they do not share a target directory. Run native `mbx gc --dry-run` on the host and confirm it does not select the live sessions' targets. A session on a non-Rust repository has no cache mount and `command -v cargo` inside it is not the mbx shim.

End to end on morannon (SSH Podman target): with no overrides, the target resolves to `~/.cache/mbx` on ext4 and mbx is off. With `directory = "/mnt/nvme/mbx"` on the target, the reflink probe passes, mbx is on, and the two-session checks above pass; the pre-existing empty `.agents`, `.codex`, and `.git` directories are untouched. Turning the global switch off disables mbx on both targets.

## Idempotence and Recovery

Prototype containers use the `mbx-proto-` name prefix; remove them with `podman rm -f` by name. The prototype writes only new entries into cache directories, which mbx treats as disposable. Settings are additive and absent by default. The download is keyed by version and digest and can be deleted and re-fetched. If a session's mbx setup fails at any step, the session proceeds without mbx, so a partial failure never leaves a session unusable.

## Artifacts and Notes

None yet.

## Interfaces and Dependencies

In `mj-core/src/config.rs`:

    pub struct BuildCacheConfig { pub enabled: bool }
    pub struct TargetBuildCache {
        pub enabled: Option<bool>,
        pub directory: Option<PathBuf>,
        pub max_size: Option<u64>,
    }

In `mj-core/src/targets.rs`:

    #[serde(rename_all = "snake_case")]
    pub enum MountAccess { Ro, Cow, Rw }
    pub struct AdditionalMount {
        pub source: PathBuf,
        pub destination: PathBuf,
        pub access: MountAccess, // deserialized from legacy `read_only` when absent
    }

In `mj-controller/src/controller/mbx.rs`:

    pub(super) struct ResolvedBuildCache {
        pub directory: PathBuf,
        pub limits: BuildCacheLimits,
        pub reflinks: bool,
    }
    pub(super) fn resolve(target: &targets::TargetTemplate, global: &BuildCacheConfig,
        executor: &impl CommandExecutor) -> Result<Option<ResolvedBuildCache>>;

No new crates are expected. mbx itself is an external binary, not a Cargo dependency.

## Revision notes

- 2026-09-16: added the user-visible three-way access mode for attached directories (read-only default, copy-on-write, read-write), at the user's request, because the read-write mode needed for the mbx cache should be available to users too.
- 2026-09-16 (review): code dives into mbx showed that learned incremental engages on the first edit and that `MBX_TARGET_ROOT` alone does not separate concurrent same-path sessions; the target-root decision is superseded and the workspace-path choice (A or B) is open. Podman `keep-id` became unconditional with a no-option fallback and a 4.3.0 floor. mj no longer manages mbx GC; the native mbx version is checked against the pin.
