# Run OpenCode 2 sessions with the person's own OpenCode logins

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.


## Purpose / Big Picture

A person who signs in to a model provider with their own OpenCode (for example `opencode auth login`, or `/connect` in the OpenCode TUI, choosing DeepSeek and pasting an API key) and then starts an OpenCode session in Mjolnir expects that session to offer that provider's models. Today it does not: the session lists only OpenCode's free models plus whatever providers environment variables enable. After this change, a Mjolnir OpenCode session, on this machine or in a container, offers the providers the person signed in to, and Mjolnir runs the current OpenCode 2 release instead of OpenCode 1.

To see it working: sign in to DeepSeek with OpenCode 2 on the host, start an OpenCode session in an isolated Mjolnir instance, and run `mj --instance <name> sessions --session <id> --json`; the `model` configuration option lists `deepseek/deepseek-flash` and `deepseek/deepseek-v4-pro`.


## Orientation

Mjolnir (`mj`) is a control plane (the daemon, crate `mj-controller`) that starts per-session workers (crate `mj-worker`). A worker runs a coding agent ("harness") such as Codex, Claude Code, or OpenCode, and talks to it over the Agent Client Protocol (ACP, JSON-RPC over the harness's standard input and output). Shared types live in `mj-core`; checkpoint capture and restore live in `mj-checkpoint`.

A profile (`[profiles.<id>]` in `~/.config/mjolnir/config.toml`) names a harness kind and a home, the directory the harness reads its configuration from. For OpenCode the home is an XDG configuration directory that must end in `/opencode`, such as the person's own `~/.config/opencode`. Mjolnir never runs a session from the profile home itself. It "stages" a private copy of selected files into a session home (`stage_profile` in `mj-controller/src/controller/worker_binary/harness.rs`), uploads it to the target, and points the harness at it with environment variables (`HarnessKind::configure_home_environment` in `mj-core/src/config/harness.rs`). For OpenCode, `XDG_CONFIG_HOME` is the parent of the staged home and `XDG_DATA_HOME` is `<staged home>/.data`, so OpenCode's data lands in `<staged home>/.data/opencode`.

Codex and Claude keep their login in a file inside their home (`auth.json`, `.credentials.json`). Staging copies that file, and the credential sync (`mj-controller/src/worker_client/credential_sync.rs`, targets chosen in `mj-controller/src/pollers/worker_targets.rs`) copies refreshed logins between the profile home and live sessions. That pattern is what the person asked OpenCode to follow.

OpenCode is different in two ways, both verified on the host on 2026-10-21. First, its login does not live in its configuration home. OpenCode keeps data under `$XDG_DATA_HOME/opencode`, which is `~/.local/share/opencode` for the person's own install, while Mjolnir looked for `<home>/.data/opencode/auth.json`, a layout that exists only in homes Mjolnir created. Muse, the other harness with an XDG home, already had this bug fixed for its model catalog by a private helper `muse_data_dir` in the same file. Second, OpenCode 2 keeps logins in its SQLite database `opencode.db` (table `credential`), not in `auth.json`. The same database also holds every OpenCode session the person ever ran.

How OpenCode 2 moves logins between stores, verified against version 2.1.2 and its source at tag `v2.1.2` of `github.com/anomalyco/opencode`: `opencode auth export` prints a JSON array of entries `{id, integrationID, label, active, value}`, where `value` is the stored credential (`{"type":"key","key":...}` for an API key, `{"type":"oauth",...}` for OAuth). `opencode auth import <file>` adds each entry whose `id` the destination does not already have, so repeating an import is harmless ("Imported 0 credentials, skipped 1 already present"). Both accept `--standalone`, which runs a private OpenCode server against the directories named by `XDG_CONFIG_HOME` and `XDG_DATA_HOME` instead of the person's background service. The database row behind an entry is `credential(id, integration_id, label, value JSON text, connector_id, method_id, active)` (`packages/core/src/credential/sql.ts`); an export entry is exactly `id`, `integration_id` as `integrationID`, `label`, `active` as a boolean, and `value` parsed from JSON. A legacy `auth.json` placed in a fresh OpenCode 2 data directory is not imported, and `auth import` rejects the legacy format ("Expected array").

OpenCode 2 is published on npm as `@opencode/cli`, with one package per platform, `@opencode/cli-<target>` where target is `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64` (and musl, baseline, and Windows variants). Each platform tarball is `https://registry.npmjs.org/@opencode/cli-<target>/-/cli-<target>-<version>.tgz` and contains the executable at `package/bin/opencode`. The official installer for containers and other unmanaged machines is `curl -fsSL https://opencode.ai/v2/install | bash -s -- --version <version> --no-modify-path`, which installs `~/.opencode/bin/opencode`. OpenCode 1's installer (`https://opencode.ai/install`) refuses version 2. GitHub releases of the repository carry only OpenCode 1 archives.

`opencode acp` in 2.1.2 starts a private child `opencode serve --stdio --port 0` in its own process group. The child reads the session's XDG directories, offers configuration options `model`, `effort`, and `mode`, and exits when its parent dies, including when the parent's process group receives SIGKILL, so session teardown leaves nothing behind.

The pinned version lives in `mj-core/src/harness_runtime.rs` (`OPENCODE_VERSION`, and the `HarnessPin` for `HarnessKind::OpenCode`). The worker's managed installer is `install_opencode` in `mj-worker/src/worker_runtime/harness.rs`, which reads per-platform file names and SHA-256 digests from `mj-worker/assets/opencode/runtime.json`. Local, SSH, and EC2 targets use this managed install. Container targets instead run the controller's shell script from `bridge_launch` in `mj-controller/src/controller/worker_binary/harness.rs`, which uses an `opencode` already on `PATH`, else `~/.opencode/bin/opencode`, else the official installer. The worker's model discovery (`mj-worker/src/worker_runtime/discovery.rs`) and session launch (`mj-worker/src/worker_runtime/harness_launch.rs`) both take the managed command from `harness::resolve`.

Checkpoints (recovery copies of a session) capture OpenCode's native state as the session's `.data/opencode/opencode.db` and its write-ahead log, byte for byte (`is_opencode_native_artifact` in `mj-checkpoint/src/checkpoint/native_scan.rs`). Every other harness's login file is kept out of checkpoints by name (`is_secret_like_component` in `mj-checkpoint/src/archive.rs`).


## Plan of Work

Milestone 1 moves the pin to OpenCode 2.1.2. `OPENCODE_VERSION` becomes `2.1.2` and the pin's install id and display version follow. `runtime.json` lists the four npm platform tarballs with their SHA-256 digests (each checked against npm's published sha512 integrity). `install_opencode` downloads from the npm registry URL, verifies the digest as before, extracts, and moves `package/bin/opencode` to the install root, where `validate_entrypoint` expects it. The container fallback script switches to the v2 installer. Documentation that names GitHub releases or the v1 installer is updated.

Milestone 2 gives every reader one answer to "where is this profile's OpenCode or Muse data". A `HarnessKind` method in `mj-core/src/config/harness.rs` returns, for a nested (XDG) home, `$XDG_DATA_HOME/<id>` or `~/.local/share/<id>` when the home is the person's own (`$XDG_CONFIG_HOME/<id>` or `~/.config/<id>`), and `<home>/.data/<id>` otherwise. `muse_data_dir` is replaced by it. `configure_profile_home_environment`, which `mj login` and quota probes use to act on the person's own login, leaves the XDG variables unset for the person's own home, the same way it already leaves `CLAUDE_CONFIG_DIR` unset for Claude on macOS, so `mj login --profile <opencode profile>` signs in to the store the person's sessions are staged from.

Milestone 3 carries the logins. Staging reads the `credential` rows of `<data dir>/opencode.db` read-only with SQLite and writes them, as an `auth export` document, to `.data/opencode/mjolnir-credentials.json` in the staged home. The name matches the archive's credential-file rule (`<vendor>-credentials.json`), so it is never archived or collected. A missing database means no logins and stages nothing; an unreadable database or an unexpected schema fails staging with the path and the SQLite error, so the failure is visible. Every OpenCode launch, managed or container, runs one shared shell snippet from `mj-core`: if that file exists, `opencode auth import --standalone <file>` (its output on standard error), and a failed import fails the launch; then `exec opencode acp`. `credential_file_name` for OpenCode names the staged document. The credential sync skips OpenCode, because its live login is a database row that the worker cannot copy and `auth import` neither updates an entry it already has nor activates one for an integration it already has, so a pushed document would change nothing; new and resumed sessions get the profile's current logins from staging. `mj login` and setup and doctor read the profile's logins from the database instead of a file that never exists.

Milestone 4 keeps secrets out of checkpoints. Capturing OpenCode's native state takes a consistent snapshot of the live database with SQLite's backup into a private temporary file, deletes the rows of `credential`, `account`, and `control_account` (the tables that hold API keys and OAuth tokens), vacuums it so deleted pages do not survive, and captures that single file as `opencode.db`. Restore removes any `opencode.db-wal` and `opencode.db-shm` left beside the database it replaces, so SQLite cannot replay a stale log over it; the next launch imports the staged logins again.

Milestone 5 validates end to end in an isolated instance and updates this plan.


## Concrete Steps

All commands run from the repository root, `/home/ryansvihla/code/mjolnir/.mj/clones/2d9ede370f54e65f3357ce77cad672b3` in this workspace. Cargo tests run outside the sandbox.

Compute a digest for `runtime.json` (repeat for each target):

    curl -fsSLO https://registry.npmjs.org/@opencode/cli-linux-x64/-/cli-linux-x64-2.1.2.tgz
    npm view @opencode/cli-linux-x64@2.1.2 dist.integrity    # must equal sha512-<base64 of openssl dgst -sha512 -binary>
    sha256sum cli-linux-x64-2.1.2.tgz

Run the affected tests per milestone, for example:

    cargo test -p mj-core opencode
    cargo test -p mj-controller opencode
    cargo test -p mj-worker opencode
    cargo test -p mj-checkpoint opencode

Final validation, once:

    cargo clippy --all-targets -- -D warnings
    cargo test -p mj-core -p mj-checkpoint -p mj-worker -p mj-controller -p mj-cli

End-to-end, with an isolated instance named `opencode2` (never the default instance):

    cargo build -p mj-cli -p mj-worker
    target/debug/mj --instance opencode2 daemon status
    target/debug/mj --instance opencode2 new --workspace <name> --profile opencode --target localhost ...
    target/debug/mj --instance opencode2 sessions --session <id> --json

Expect the `model` option to include `deepseek/` entries.


## Validation and Acceptance

Acceptance is behavior. With the person's OpenCode 2 store holding a DeepSeek credential, a new OpenCode session in an isolated instance lists `deepseek/deepseek-flash` among its models, its worker runs `opencode-2.1.2/opencode acp`, and its staged home's database holds the imported credential. A checkpoint of that session contains no copy of the key. `mj login --profile opencode` runs `opencode auth login` against `~/.local/share/opencode`. Tests protect the conditions that would otherwise break unnoticed: the reader against a database written by the real pinned OpenCode (an ignored live test that needs the pinned binary), and the checkpoint capture of a WAL-mode database whose key must appear nowhere in the captured bytes.


## Idempotence and Recovery

Staging and import are repeatable: the document is rewritten on every staging and `auth import` skips entries already present. A failed managed install leaves an `.install-*` directory that the next install removes. The old `opencode-1.18.34` managed install is collected once nothing holds its lease. Sessions started before the upgrade keep their 1.18 database; OpenCode 2 migrates it on next launch.


## Progress

- [x] (2026-10-21) Diagnose: the session runs pinned 1.18.34 against a staged home with no login; the person's DeepSeek login is a `credential` row in `~/.local/share/opencode/opencode.db` written by OpenCode 2.1.2.
- [x] (2026-10-21) Prototype outside Mjolnir: `auth export --standalone` from the real store, `auth import --standalone` into a fresh home, and `opencode acp` there offers `deepseek/deepseek-flash` and `deepseek/deepseek-v4-pro`.
- [x] (2026-10-21) Milestone 1: pin 2.1.2 from npm tarballs (`runtime.json` digests checked against npm's sha512 integrity); container script uses the v2 installer; `docs/src/content/docs/profiles.md` updated.
- [x] (2026-10-21) Milestone 2: `HarnessKind::is_own_nested_home` and `HarnessKind::nested_data_dir` in `mj-core/src/config/harness.rs` replace `muse_data_dir`; `configure_profile_home_environment` leaves XDG unset for the person's own home.
- [x] (2026-10-21) Milestone 3: `AuthScheme::NativeLoginStore` for OpenCode; `mj-controller/src/opencode_logins.rs` reads the `credential` rows; `stage_profile` writes `.data/opencode/mjolnir-credentials.json`; `mj_core::harness_runtime::opencode_acp_script` imports it for managed and container launches; credential sync, shared logins and rejected-login tracking skip OpenCode through `uses_native_login_file`; doctor, setup and `mj login` read the database. Ignored live test `live_opencode_profile_discovery_offers_logins_from_its_database` passes against OpenCode 2.1.2.
- [x] (2026-10-21) Milestone 4: `opencode_database_without_logins` in `mj-checkpoint/src/checkpoint/native_scan.rs`; test `an_opencode_checkpoint_keeps_live_sessions_and_no_login_secret` fails against a raw copy ("the checkpoint carries sk-checkpoint-api-key") and passes with the snapshot.
- [x] (2026-10-21) Review round: carry OpenCode 1 `auth.json` logins (data directory, and `<home>/.data/opencode` where Mjolnir kept them) unless OpenCode 2 has taken that directory over; export each integration's selected login as active; snapshot with SQLite's backup API so a non-UTF-8 temporary directory works; restore removes stale `opencode.db-wal` and `-shm` before writing a restored database. Tests: `moving_to_opencode_2_keeps_every_login_and_revives_none` (fails without the conversion or without the selection), the checkpoint test now restores into a retained home with a stale log (fails without the sidecar removal: `credential: left 2, right 0`), and the live discovery test adds an OpenCode 1-only profile.
- [x] (2026-10-21) Milestone 5: `cargo clippy --all-targets -- -D warnings` clean; full suites of mj-core, mj-checkpoint, mj-worker, mj-controller and mj-cli pass (the host-specific `shasum` test skipped). Isolated instance `opencode2` with the person's own `~/.config/opencode`: a local session ran `opencode-2.1.2/opencode acp`, offered `deepseek/deepseek-v4-pro` and `deepseek/deepseek-flash`, and its checkpoint archive held only `native/.data/opencode/opencode.db` with no byte of the key. The exact container fallback script, run in `ghcr.io/brokkai/mjolnir/agent-dev:2.51.0` with no OpenCode installed, installed 2.1.2 through the v2 installer, imported a staged login, and offered both DeepSeek models over ACP.


## Surprises & Discoveries

- Observation: OpenCode 2 is on npm as `@opencode/cli`, not `opencode-ai` (which stops at 1.19.3), and not on GitHub releases.
  Evidence: `npm view @opencode/cli dist-tags` gives `latest: 2.1.2`; the opencode.ai download page lists `npm install -g @opencode/cli` and `https://opencode.ai/v2/install`.
- Observation: a legacy `auth.json` in a fresh OpenCode 2 data directory is ignored, and `auth import` rejects the legacy shape.
  Evidence: `auth list --standalone` showed no DeepSeek entry; `auth import legacy.json` printed `Expected array`.
- Observation: the daemon's `PATH` does not contain `~/.opencode/bin`, so the daemon cannot rely on the person's own `opencode` binary to export logins.
- Observation: the `opencode serve --stdio` child runs in its own process group but exits when `opencode acp` is killed with SIGKILL to its group.
- Observation: profile discovery runs its worker with the login-shell environment, so a test's `XDG_CACHE_HOME` does not isolate the managed harness cache; the live test installs into the shared `~/.cache/mjolnir/harnesses`, which is content-addressed and leased, so running sessions on another pin are unaffected.
- Observation: released OpenCode channels always name the database `opencode.db` (`packages/cli/src/database-path.ts`); only unreleased channels or `OPENCODE_DB` choose another name.
- Observation: `installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths` fails on this Fedora host before and after the change because `shasum` is not installed.
- Observation: inside a Mjolnir session, `MJ_CONFIG_DIR` and `MJ_DATA_DIR` are set and take precedence over `--instance`, so isolated runs must unset both (`env -u MJ_CONFIG_DIR -u MJ_DATA_DIR target/debug/mj --instance <name> ...`); otherwise the dev build talks to the default daemon.
- Observation: a container worker needs the musl target, which this host's toolchain lacks, so the container path was validated by running the fallback script itself in the agent-dev image.
- Observation: OpenCode 2 bootstraps an empty database with every migration marked complete, so a fresh database never imports `auth.json`; only an existing OpenCode 1 database runs `20260805200742_import_legacy_credentials`. Its rows have no active flag, and OpenCode selects the newest login of an integration.
  Evidence: `packages/core/src/database/migration.ts` and `packages/core/src/credential.ts` at `v2.1.2`; a 1.18 database lists 38 migrations without it, the person's 2.x store 50 with it.
- Observation: rusqlite's `Backup::run_to_completion` rejects `-1` pages per step ("pages_per_step must be positive"), so the snapshot calls `step(-1)` itself.


## Decision Log

- Decision: carry logins as an `auth export` document imported at launch, not by copying the data directory.
  Rationale: the person asked to follow the Codex and Claude pattern, which copies the login and not the session history; OpenCode's data directory is one live database that also holds every session.
  Date/Author: 2026-10-21, agent.
- Decision: read the profile's `credential` rows with SQLite read-only instead of running `opencode auth export`.
  Rationale: the daemon may not find the person's binary, and running Mjolnir's pinned binary against the person's store would migrate a database the person's own OpenCode owns. A read-only query cannot change it, and a schema change fails visibly at staging.
  Date/Author: 2026-10-21, agent.
- Decision: skip credential sync for OpenCode.
  Rationale: OpenCode's live login is a database row; `auth import` neither updates an entry it already has nor activates a new one for an integration it already has, so pushing a document into a live session would change nothing. New and resumed sessions get current logins from staging.
  Date/Author: 2026-10-21, agent.
- Decision: convert an OpenCode 1 `auth.json` exactly as OpenCode 2's migration does, unless the database beside it lists that migration.
  Rationale: profiles signed in under OpenCode 1 must keep their logins without intervention. Once OpenCode 2 has opened a directory it has decided about the file itself, and converting it again would revive logins removed since. Converted entries get stable ids, so repeated imports add nothing.
  Date/Author: 2026-10-21, agent, from review.
- Decision: restore removes `opencode.db-wal` and `opencode.db-shm` before writing an archived `opencode.db`.
  Rationale: SQLite replays a log it finds beside a database; snapshots carry no log, so a home kept across a resume would replay stale pages, credentials included, over the restored database. Older archives that carry a log still write it after the removal.
  Date/Author: 2026-10-21, agent, from review.
- Decision: scrub credential tables from captured OpenCode databases.
  Rationale: importing logins puts them in the session database, which checkpoints capture whole; every other harness's login is kept out of checkpoints.
  Date/Author: 2026-10-21, agent.


## Outcomes & Retrospective

OpenCode sessions now run OpenCode 2.1.2 and offer the providers the person signed in to with their own OpenCode, on this machine and in containers, without copying OpenCode's session history. Logins are read from the person's own data directory, imported at each launch, and kept out of checkpoints. OpenCode 1 logins carry over until OpenCode 2 takes their directory over. Remaining limitations: a login added or changed after a session started reaches it only when the session is next launched, because OpenCode's import neither updates nor activates entries for an integration it already has; an OpenCode 1 well-known login converts to a key, as OpenCode 2's migration does, but the session lacks the origin list that migration also records; and container sessions still do not set `OPENCODE_DISABLE_AUTOUPDATE`, as before this change.


## Interfaces and Dependencies

`mj-checkpoint` gains the workspace `rusqlite` dependency (already built into `mj-worker`, `mj-controller`, and `mj-cli`). No new crates. No database migration in Mjolnir's own store. No ACP protocol change.
