# Run slopcop from Claude sessions on every target

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. GitHub issue: BrokkAi/mjolnir#1397.

## Purpose / Big Picture

slopcop is a command-line program (source in the BrokkAi/slopcop repository, service at https://slopcop.com) that sends one Git patch, from a base commit to the working tree, to a hosted static-analysis service and prints the findings. It ships an agent "skill": a Markdown instruction file, `SKILL.md`, that a coding agent such as Claude Code loads from a `skills/` directory and that tells the agent to run `slopcop`, fix the new findings, and run it again.

After this plan, an agent in any Claude session that Mjolnir starts can follow that skill: the `slopcop` program exists where the session runs, and it is signed in with the same slopcop login as the person running Mjolnir. Today that is true only for sessions that run directly on the person's machine (the "localhost" target). In a container session the skill is delivered, but the program is missing and there is no login, so the agent is told to run a command that fails.

To see it working at the end: start an isolated Mjolnir instance, create a Claude session on a container target, and ask the agent to run `slopcop whoami`. It prints the same account and workspace as `slopcop whoami` on the host. Then ask it to run `slopcop check --verbose`. That prints the attribution it sends, which names `claude-code` and the model that wrote each changed file.

## Progress

- [x] (2026-10-29) Verified what already works and what is missing (see `Surprises & Discoveries`). Filed #1397 for this work and #1398 to #1404 for the other harnesses.
- [x] (2026-10-29) Milestone 1: installed the pinned slopcop 0.0.7 release in `containers/Containerfile.agent-dev` and listed it in `docs/src/content/docs/containers.md`. The `ARG`/`RUN` block, copied verbatim into a `debian:trixie-slim` build, passed its hash check and printed `slopcop version v0.0.7`, with the binary owned by root. The arm64 hash was checked against the downloaded archive; this machine cannot emulate arm64.
- [ ] Milestone 1: confirm that the image publish workflow for the pushed commit succeeds on amd64 and arm64.
- [ ] Milestone 2: synchronize the controller's slopcop login into every isolated session through a new credential-sync pair, a worker-held login file, and a `slopcop` wrapper on the session `PATH`.
- [ ] Milestone 2 validation: relay compatibility test against an older worker, wrapper test, and an end-to-end Claude container session in an isolated `--instance`.
- [ ] Outcomes & Retrospective.

## Surprises & Discoveries

- Observation: localhost Claude sessions already work end to end. `slopcop init`, `slopcop login` and every `slopcop` check write the skill into `~/.claude/skills/slopcop`, the default Claude profile home. Mjolnir's skills sync (`mj-core/src/skills/mod.rs`) copies the profile home's `skills/` tree into each session's staged home. A localhost Claude process runs with the person's real `HOME` and the login shell's `PATH`, so the host's `slopcop` and its stored login are used as they are.
  Evidence: the environment of a running Mjolnir Claude process (`/proc/<pid>/environ`) shows `HOME=/home/<user>` and `CLAUDE_CONFIG_DIR=<data dir>/workers/<id>/profile`.

- Observation: slopcop's model attribution works in Mjolnir Claude sessions without any change. slopcop finds the Claude transcript by `CLAUDE_CODE_SESSION_ID` under `$CLAUDE_CONFIG_DIR/projects/*/<id>.jsonl`. The Claude Code release Mjolnir pins, 2.1.308, builds the environment of every command it runs with `CLAUDE_CODE_SESSION_ID`, `CLAUDE_EFFORT` and `AI_AGENT`. Mjolnir launches it with `CLAUDE_CONFIG_DIR` set to the staged session home.
  Evidence: in the pinned binary, `CLAUDECODE:"1",CLAUDE_CODE_SESSION_ID:e.sessionId,CLAUDE_CODE_CHILD_SESSION:"1",...`. A live session's transcript held 9 `Edit` calls tagged `claude-opus-5-5` at effort `xhigh`.

- Observation: the agent-dev image still compiles Bifrost 0.12.0 from source, and its comment says turn review depends on it. Nothing in Mjolnir runs that binary any more: commit b535fb8d2 removed it from turn review, and `mj-core/src/worker_launch.rs` drops the retired `bifrost_binary` launch field. This plan leaves it alone; removing it is a separate decision.

- Observation: the slopcop npm package carries the binaries for all six platforms (51 MB unpacked), while one release archive is about 3.6 MB.

- Observation: the release archives store their files as uid 1001 (GitHub's `runner` user), and GNU tar running as root keeps stored owners. The install therefore extracts with `--no-same-owner`, so the binary belongs to root rather than to whatever account later gets uid 1001.
  Evidence: `tar -tvzf slopcop_v0.0.7_linux_amd64.tar.gz --numeric-owner` lists `1001/1001`. After the change, the test build lists `/usr/local/bin/slopcop` as `0 0`.

## Decision Log

- Decision: install slopcop in the image from the GitHub release archive, checked against a per-architecture sha256 pinned in the Containerfile, rather than from npm.
  Rationale: the archive is about a fourteenth of the npm package's size, and the Containerfile already installs Go and cargo-nextest this way. A pinned hash also fails the build if a release asset is ever replaced.
  Date/Author: 2026-10-29 / Claude.

- Decision: keep the slopcop version pin only in the Containerfile for now.
  Rationale: no Rust code installs or names slopcop yet. If a later change installs it on bare SSH targets, the pin moves into Rust next to the harness pins, with a test that the Containerfile matches, the same way `bridge_fallback_pins_match_the_agent_dev_containerfile` works.
  Date/Author: 2026-10-29 / Claude.

- Decision: give isolated sessions the host's slopcop login, not a separate token configured in Mjolnir.
  Rationale: this is how the GitHub token already works. The controller uses the person's own `gh` login, and every check bills the workspace that login belongs to, whether the person runs it or an agent does. A dedicated workspace token can come later as configuration if someone needs one.
  Date/Author: 2026-10-29 / Claude.

- Decision: the controller resolves the login the way slopcop itself documents it. The token comes from `SLOPCOP_TOKEN` in the daemon's environment, otherwise from `slopcop/config.json` in the user configuration directory. The service URL comes from `SLOPCOP_SERVICE`, otherwise from the same file. The file is JSON, `{"serviceUrl": ..., "token": ...}`.
  Rationale: slopcop has no command that prints its token, the way `gh auth token` does for GitHub. Its README documents this precedence and the file's location as a contract. Asking the CLI would make slopcop the only reader of its own file, so a `slopcop auth token` command is worth adding upstream and switching to later. Until then, Mjolnir reads the file and never writes it.
  Date/Author: 2026-10-29 / Claude.

- Decision: sync the slopcop login to every target except raw localhost (`TargetLocator::LocalBare`). The predicate is the same one the GitHub token uses: `target_syncs_github_token` in `mj-controller/src/pollers/worker_targets.rs`.
  Rationale: a raw localhost session already reads the host's login directly, so a second copy would only add a writer. Muse and OpenCode are an exception on Linux: they redirect `XDG_CONFIG_HOME` and so hide the host file even on localhost. #1401 and #1402 track extending the sync to them.
  Date/Author: 2026-10-29 / Claude.

- Decision: the worker keeps the login in one file, `<worker root>/slopcop-login`, and a `slopcop` wrapper reads it on every invocation. The file holds the token on its first line and the service URL, if any, on its second. The wrapper sits in the same session `bin` directory as the existing `gh` wrapper, exports `SLOPCOP_TOKEN` (and `SLOPCOP_SERVICE` when present), and runs the real `slopcop`.
  Rationale: this copies the GitHub token design (`mj-worker/src/worker_runtime/unix/git_env.rs`). A login changed on the host reaches running sessions without restarting the harness, and the token never sits in the harness process's own environment. One file keeps token and URL consistent with each other. Environment variables, unlike pointing slopcop at a different config directory, behave the same on Linux and macOS: Go's `os.UserConfigDir` ignores `XDG_CONFIG_HOME` on macOS.
  Date/Author: 2026-10-29 / Claude.

## Outcomes & Retrospective

Not started.

## Context and Orientation

Mjolnir has a control plane and a data plane. The control plane is the daemon (`mj daemon-run`, crate `mj-controller`). It owns the database and session records, and it runs a periodic background task called credential sync, in `mj-controller/src/worker_client/credential_sync.rs`, that makes every live session's copies of credentials and skills match the controller's.

The data plane is one worker process per session (crate `mj-worker`). It runs the agent's harness, the program that drives the model (here Claude Code, through the ACP bridge `@agentclientprotocol/claude-agent-acp`). The daemon talks to a worker over the "relay", a line-delimited JSON request/response protocol. Its requests are `RelayRequest` variants in `mj-core/src/relay/protocol.rs`, and its version number is `RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs`, currently 46.

A session runs on a "target": raw localhost (`LocalBare`), a local or remote container, or a remote machine over SSH. Container targets use the image built from `containers/Containerfile.agent-dev`. Pushing to `master` publishes that image as `ghcr.io/brokkai/mjolnir/agent-dev:latest` through `.github/workflows/publish-agent-dev-image.yml`.

The GitHub token reaches sessions the way the slopcop login will. The controller resolves it with `controller_github_credential` in `mj-controller/src/controller/backend.rs`. `CredentialSyncTarget.sync_github_token` (`mj-core/src/credentials.rs`) is true for every target except `LocalBare`. For those sessions, `reconcile_github_token` in `credential_sync.rs` compares fingerprints through the `GithubTokenState` request and then sends `InstallGithubToken` or `RemoveGithubToken`.

The worker handles those requests in `apply_credential_request_at` (`mj-worker/src/worker_runtime/unix/requests.rs`) by writing `<worker root>/github-token`. At startup, `configure_github_cli` (`mj-worker/src/worker_runtime/unix/git_env.rs`) writes a `gh` wrapper into `<worker root>/bin` and puts that directory first on the session `PATH`, and the wrapper reads the token file each time it runs. Credential requests are connection-only, which keeps them out of the relay journal; they are listed in `mj-worker/src/relay/requests.rs`.

AWS credentials were added the same way in commit ad7631da4, which is a good map of every file a new credential kind touches.

## Plan of Work

### Milestone 1: slopcop in the agent-dev image

In `containers/Containerfile.agent-dev`, after the harness `npm install` layer and its `ENV` lines, add a separate `RUN` layer in the style of the Go install. It selects the archive and sha256 for `amd64` or `arm64` (from `TARGETARCH`, falling back to `dpkg --print-architecture`), downloads `slopcop_v${SLOPCOP_VERSION}_linux_${ARCH}.tar.gz` from the GitHub release, and checks it with `sha256sum --check --strict`. It then extracts `slopcop` into `/usr/local/bin`, keeps its `LICENSE` under `/usr/share/doc/slopcop`, and checks that `slopcop version` prints exactly `slopcop version v${SLOPCOP_VERSION}`.

The version starts at 0.0.7, the first release whose checks send model attribution. The hashes come from that release's `checksums.txt`.

At the end of this milestone, a container session can run `slopcop`. It is not signed in yet, so a check answers `auth_required`. The person can still sign in from inside the session with `slopcop login`, the browser device flow, so the milestone is useful on its own.

### Milestone 2: the controller's slopcop login in isolated sessions

Start with a small module, `mj-core/src/slopcop.rs`, that owns Mjolnir's whole reading of a slopcop login. It holds a `SlopcopLogin { token, service_url }` type and resolves it from an environment map and a configuration directory, in slopcop's documented order. It also encodes the login into the two-line worker file and decodes it back, validating both values: each must be one line of printable text within a size bound, and the URL must be `http` or `https`. Reuse the opaque-token validation and the owner-only, no-symlink file helpers in `mj-core/src/credentials.rs` rather than writing new ones.

Next, extend the relay. Add `SlopcopLoginState`, `InstallSlopcopLogin { data }` and `RemoveSlopcopLogin` to `RelayRequest`, answered with the existing credential-state payload, with `minimum_protocol` 47. Define `RELAY_SLOPCOP_LOGIN_PROTOCOL = 47` and make it `RELAY_PROTOCOL_VERSION`. List the three requests as connection-only in `mj-worker/src/relay/requests.rs`.

On the worker, handle the three requests in `apply_credential_request_at` against `<worker root>/slopcop-login`. Beside the `gh` wrapper, write a `slopcop` wrapper. It removes its own directory from `PATH`. When the login file exists, it reads the token and the optional URL and exports them as `SLOPCOP_TOKEN` and `SLOPCOP_SERVICE`. It then runs `slopcop`, or, when no `slopcop` remains on `PATH`, says so on stderr and exits 127.

On the controller, add `sync_slopcop_login` to `CredentialSyncTarget`, set from the same predicate as `sync_github_token`. Resolve the login once per sync cycle on a blocking task, because it reads a file and the daemon's event loop must never block, and carry it in `SessionCanonical`. Add `reconcile_slopcop_login`, which mirrors `reconcile_github_token` and records new `CredentialSyncAction` variants. It skips a worker whose protocol is below 47 quietly; such a worker is replaced when its session is idle.

Finally, check whether the first credential sync of a new container session can happen after its first turn starts. If it can, seed the login at container creation, the way `seed_container_github_token` in `git_env.rs` seeds the GitHub token.

## Concrete Steps

All commands run from the repository root.

Milestone 1 validation, without building the whole image:

    podman run --rm docker.io/library/debian:trixie-slim sh -euc '<the new RUN body, with curl installed first>'

It must print `slopcop version v0.0.7`. Then commit, push to `origin/master`, and watch the publish workflow:

    gh run list --workflow publish-agent-dev-image.yml --limit 1
    gh run watch <run id> --exit-status

Milestone 2, after each round of changes:

    cargo test -p mj-core slopcop
    cargo test -p mj-worker credential
    cargo test -p mj-controller credential_sync

After the last round:

    cargo clippy --all-targets -- -D warnings
    cargo test -p mj-core -p mj-worker -p mj-controller

Run these outside the restricted sandbox.

## Validation and Acceptance

Milestone 1 is accepted when the install step prints `slopcop version v0.0.7` in a clean container and the publish workflow for the pushed commit succeeds.

Milestone 2 is accepted when, in an isolated instance (`mj --instance slopcop-test ...`), a Claude session on a container target can run `slopcop whoami` and print the host's account and workspace. A later `slopcop logout` on the host must make the session answer `auth_required` within one sync cycle.

Three tests protect this. An integration test shows that the controller skips slopcop sync for a worker that speaks protocol 46 and pushes it to one that speaks 47. A worker relay test installs, reads and removes the login over a real relay connection, in the style of the AWS tests in `mj-worker/src/worker_runtime/relay_tests.rs`. A wrapper test shows that the next invocation picks up a rotated login file, and that a symlinked login file is refused.

## Idempotence and Recovery

Every step is additive. Rebuilding the image repeats the download and hash check. Credential sync is a fingerprint comparison, so running it again with the same login does nothing. Removing a login on the host removes the session copy at the next cycle. If a wrapper or login file is damaged, resuming the session rewrites it, as for the GitHub wrapper.

## Artifacts and Notes

slopcop v0.0.7 Linux release hashes, from the release's `checksums.txt`:

    552e197f2d5c73fd43c7fc010a608c7560e0ecebd1a04a95e7901e83b0d95d3d  slopcop_v0.0.7_linux_amd64.tar.gz
    5337403cd50fb99e9b53fef9dc01473ccbfd2a5e5a9643252d4e9e9b0a3f967a  slopcop_v0.0.7_linux_arm64.tar.gz

## Interfaces and Dependencies

No new crate dependencies are expected. slopcop keeps its login in Go's `os.UserConfigDir`, which is `$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support` on macOS, and `%AppData%` on Windows. The Rust side must resolve the same directory.
