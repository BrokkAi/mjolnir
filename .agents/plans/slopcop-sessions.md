# Run slopcop from Claude sessions on every target

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. GitHub issue: BrokkAi/mjolnir#1397.

## Purpose / Big Picture

slopcop is a command-line program (source in the BrokkAi/slopcop repository, service at https://slopcop.com) that sends one Git patch, from a base commit to the working tree, to a hosted static-analysis service and prints the findings. It ships an agent "skill": a Markdown instruction file, `SKILL.md`, that a coding agent such as Claude Code loads from a `skills/` directory and that tells the agent to run `slopcop`, fix the new findings, and run it again.

After this plan, an agent in any Claude session that Mjolnir starts can follow that skill: the `slopcop` program exists where the session runs, and it is signed in with the same slopcop login as the person running Mjolnir. Today that is true only for sessions that run directly on the person's machine (the "localhost" target). In a container session the skill is delivered, but the program is missing and there is no login, so the agent is told to run a command that fails.

To see it working at the end: start an isolated Mjolnir instance, create a Claude session on a container target, and ask the agent to run `slopcop whoami`. It prints the same account and workspace as `slopcop whoami` on the host. Then ask it to run `slopcop check --verbose`. That prints the attribution it sends, which names `claude-code` and the model that wrote each changed file.

## Progress

- [x] (2026-10-29) Verified what already works and what is missing (see `Surprises & Discoveries`). Filed #1397 for this work and #1398 to #1404 for the other harnesses.
- [x] (2026-10-29) Milestone 1: installed the pinned slopcop 0.0.7 release in `containers/Containerfile.agent-dev` and listed it in `docs/src/content/docs/containers.md`. The `ARG`/`RUN` block, copied verbatim into a `debian:trixie-slim` build, passed its hash check and printed `slopcop version v0.0.7`, with the binary owned by root. The arm64 hash was checked against the downloaded archive; this machine cannot emulate arm64.
- [x] (2026-10-29) Milestone 1: committed as aad2a247f and pushed. Publish run 40847450565 succeeded: arm64 built in 7m40s and amd64 in 10m48s, and the in-image `slopcop version` check passed on both.
- [x] (2026-10-29) Milestone 2 code: `mj-core/src/slopcop.rs`; relay protocol 47 with `SlopcopLoginState`, `InstallSlopcopLogin` and `RemoveSlopcopLogin`; worker handling against `<harness home>/slopcop-login`; the `slopcop` wrapper; and the controller sync with `SlopcopLoginPushed` and `SlopcopLoginRemoved` notices.
- [x] (2026-10-29) Milestone 2 targeted tests pass: 1 in mj-core, 3 in mj-worker and 80 in the mj-controller `worker_client` and `pollers` tests.
- [x] (2026-10-29) Milestone 2 end to end: `tests/e2e/slopcop_login.py` passed in instance `slopcop-login`, against image `agent-dev:sha-aad2a24` with the portable worker built from this tree. The session's `slopcop whoami` matched the host's, including a non-default service URL. The login was usable 12 seconds after the viewer came up, including container provisioning.
- [x] (2026-10-29) Milestone 2 final validation:
  - `cargo fmt --check` and workspace clippy are clean.
  - mj-core passes 553 tests plus its integration tests, and mj-worker passes 720 plus its integration tests.
  - mj-controller passes 2,197 tests. One fails on this host: `installed_digest_matches_bytes_on_linux_and_darwin_with_quoted_paths` runs `shasum`, which is not installed here, and it predates this change (221e1343e).
  - slopcop reports no new findings with the analysis complete.
  - The end-to-end scenario passed again with `mj` and the worker rebuilt from the final tree.
- [ ] Not done here: installing slopcop on raw SSH targets, which also receive the login but have no binary unless the person installed one, and the other harnesses (#1398 to #1404).
- [ ] Outcomes & Retrospective.

## Surprises & Discoveries

- Observation: localhost Claude sessions already work end to end. `slopcop init`, `slopcop login` and every `slopcop` check write the skill into `~/.claude/skills/slopcop`, the default Claude profile home. Mjolnir's skills sync (`mj-core/src/skills/mod.rs`) copies the profile home's `skills/` tree into each session's staged home. A localhost Claude process runs with the person's real `HOME` and the login shell's `PATH`, so the host's `slopcop` and its stored login are used as they are.
  Evidence: the environment of a running Mjolnir Claude process (`/proc/<pid>/environ`) shows `HOME=/home/<user>` and `CLAUDE_CONFIG_DIR=<data dir>/workers/<id>/profile`.

- Observation: slopcop's model attribution works in Mjolnir Claude sessions without any change. slopcop finds the Claude transcript by `CLAUDE_CODE_SESSION_ID` under `$CLAUDE_CONFIG_DIR/projects/*/<id>.jsonl`. The Claude Code release Mjolnir pins, 2.1.308, builds the environment of every command it runs with `CLAUDE_CODE_SESSION_ID`, `CLAUDE_EFFORT` and `AI_AGENT`. Mjolnir launches it with `CLAUDE_CONFIG_DIR` set to the staged session home.
  Evidence: in the pinned binary, `CLAUDECODE:"1",CLAUDE_CODE_SESSION_ID:e.sessionId,CLAUDE_CODE_CHILD_SESSION:"1",...`. A live session's transcript held 9 `Edit` calls tagged `claude-opus-5-5` at effort `xhigh`.

- Observation: the agent-dev image still compiles Bifrost 0.12.0 from source, and its comment says turn review depends on it. Nothing in Mjolnir runs that binary any more: commit b535fb8d2 removed it from turn review, and `mj-core/src/worker_launch.rs` drops the retired `bifrost_binary` launch field. This plan leaves it alone; removing it is a separate decision.

- Observation: the slopcop npm package carries the binaries for all six platforms (51 MB unpacked), while one release archive is about 3.6 MB.

- Observation: `CredentialSyncTarget.sync_github_token` and `CredentialSyncTarget.skills_scope` already record the same fact. Both come from whether the target is `LocalBare`: `target_syncs_github_token` in `mj-controller/src/pollers/worker_targets.rs`, and `TargetLocator::skills_scope` in `mj-core/src/state.rs`. Folding the GitHub flag into `skills_scope` was tried and reverted. The skills test `mixed_localhost_and_isolated_sessions_sync_different_skills_from_one_profile` then reached `controller_github_credential`, which runs `gh auth token`, so its outcome depended on whether the developer's machine is signed in to GitHub. Making that hermetic means restructuring how the GitHub token is resolved, which is outside this plan. slopcop takes its decision from `skills_scope` and adds no third copy.

- Observation: three environment problems on a Fedora host stopped the end-to-end run before it reached slopcop. None is a defect in this change.
  - `scripts/build-linux-worker.sh podman` fails with "could not find `Cargo.toml` in `/source`". SELinux is enforcing, and the bind-mounted checkout is labelled `user_home_t`. A copy of the script that adds `--security-opt label=disable` to the build container worked.
  - Fedora packages `git daemon` separately. The container fixture in `tests/e2e/container_upgrades.py` serves the project with it, so the clone was refused. The slopcop driver serves a bare copy over Git's static HTTP protocol with `python3 -m http.server` instead.
  - Creating a container session now requires `resource_allocation`. The driver sends 2 CPUs and 4 GiB.
  Evidence: `git : 'daemon' n'est pas une commande git`, and `container sessions require an explicit CPU and RAM allocation`.

- Observation: the slopcop service can briefly answer "could not complete this request right now", even for `whoami`. One run stopped at the host's own `slopcop whoami` and passed when run again a minute later.

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

- Decision: only a session whose `skills_scope` is `Isolated` (every target except raw localhost, `TargetLocator::LocalBare`) gets a copy of the slopcop login, and it holds one exactly while the host is signed in. Sync never touches a localhost session's copy.
  Rationale: `TargetLocator::skills_scope` already answers "can this session use the host's CLI and configuration", so slopcop asks it rather than adding another predicate. A localhost session's home is staged fresh, without a copy, and a move carries only the harness's native records, so a localhost session never acquires a copy to clean up. A first version also removed copies from localhost sessions. That sent one more relay request to every localhost session each minute, guarded against a case that cannot occur, and broke five fixture tests whose workers do not answer the request; it was dropped. Muse and OpenCode are an exception on Linux: they redirect `XDG_CONFIG_HOME` and so hide the host file even on localhost. #1401 and #1402 track that.
  Date/Author: 2026-10-29 / Claude.

- Decision: the worker keeps the login in one file in the session's harness home, `<harness home>/slopcop-login`, and a `slopcop` wrapper reads it on every invocation. The file holds the token on its first line and the service URL, empty for slopcop's default, on its second. The wrapper sits in the same session `bin` directory as the existing `gh` wrapper, shares its `PATH` preamble, exports `SLOPCOP_TOKEN` and, when present, `SLOPCOP_SERVICE`, and runs the real `slopcop`.
  Rationale: the copy is renewed like the GitHub token, so a login changed on the host reaches running sessions without restarting the harness, and the token never sits in the harness process's environment. It lives where the AWS credentials do, in the home that the worker's credential handler already owns, and the wrapper takes its path from the same `credential_endpoint` lookup, so one function decides where it is. One file keeps token and URL consistent with each other. Environment variables behave the same on Linux and macOS, unlike pointing slopcop at another config directory: Go's `os.UserConfigDir` ignores `XDG_CONFIG_HOME` on macOS.
  Date/Author: 2026-10-29 / Claude.

- Decision: credential sync is the only writer of the session copy. Nothing is written at launch.
  Rationale: the coordinator reconciles a profile as soon as a new session's target is published (`publishing_new_targets_starts_reconciliation_without_waiting_for_the_tick`), so a new session gets the login within moments of starting. Writing it at launch as well would give the copy a second writer, and it would make launch code, which tests exercise, read the developer's own slopcop login.
  Date/Author: 2026-10-29 / Claude.

- Decision: the coordinator reads the host login once per profile reconciliation, on a blocking task, and only when a session needs a copy. The reader is a function parameter (`SlopcopLoginSource`): the daemon passes `mj_core::slopcop::host_login`, and tests pass `unknown_slopcop_login`. The result is one of `SignedIn`, `SignedOut` or `Unknown`. `Unknown` means the configuration exists but cannot be read; session copies are then left alone and a warning is logged, rather than failing the session's whole credential sync.
  Rationale: an unreadable file is not a sign-out, and treating it as one would sign every session out. The parameter keeps tests from ever reading `~/.config/slopcop`.
  Date/Author: 2026-10-29 / Claude.

## Outcomes & Retrospective

Both milestones are done for Claude sessions. Milestone 1 (aad2a247f) puts slopcop 0.0.7 in the agent-dev image. Milestone 2 gives every isolated session the host's slopcop login through credential sync and a `slopcop` wrapper. An agent in a container session can now follow the slopcop skill end to end: the skill arrives through skills sync, the binary is in the image, the login arrives through the sync, and model attribution already worked.

The change was checked with slopcop itself before committing. The first check reported five new findings:
- The scheme check's `"http://"` literal led to dropping that check, so slopcop alone decides which service URLs are valid.
- Three missing docstrings in the end-to-end driver were added.
- An inheritance-inference finding on the driver was ignored as a false positive, twice, because adding a docstring moved the class and gave it a new id. The analyzer cannot follow `from container_upgrades import Regression`, a sibling script resolved through `sys.path`.

The second check reported "No new findings | Analysis complete".

What remains is outside this plan:
- Raw SSH targets receive the login but have no slopcop unless the person installed it there.
- The other harnesses are tracked in #1398 to #1404: Codex's two writers of the skill, Muse's and OpenCode's redirected `XDG_CONFIG_HOME`, skill delivery for Kimi, Grok and MiMo, DSH's missing skills support, and model attribution for each.
- The duplicate GitHub predicate noted under `Surprises & Discoveries`.

Lessons: read the code that already answers a question before adding a field. `skills_scope` was the owner of "isolated", and launch-time staging would have made launch tests read the developer's own login.

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

Start with a small module, `mj-core/src/slopcop.rs`, that owns Mjolnir's whole reading of a slopcop login. It holds a `SlopcopLogin { token, service_url }` type and resolves it from an environment map and a configuration directory, in slopcop's documented order. It also encodes the login into the two-line worker file and decodes it back, validating both values: each must be one line of printable text within a size bound. Whether the URL names a usable service is left to slopcop, which accepts any `--service`; an earlier `http`/`https` scheme check was removed after a slopcop check flagged its `"http://"` literal, because it was a second, stricter opinion about URL validity than slopcop's own. Reuse the opaque-token validation and the owner-only, no-symlink file helpers in `mj-core/src/credentials.rs` rather than writing new ones.

Next, extend the relay. Add `SlopcopLoginState`, `InstallSlopcopLogin { data }` and `RemoveSlopcopLogin` to `RelayRequest`, answered with the existing credential-state payload, with `minimum_protocol` 47. Define `RELAY_SLOPCOP_LOGIN_PROTOCOL = 47` and make it `RELAY_PROTOCOL_VERSION`. List the three requests as connection-only in `mj-worker/src/relay/requests.rs`.

On the worker, handle the three requests in `apply_credential_request_at` against `<harness home>/slopcop-login`. In `mj-worker/src/worker_runtime/unix/git_env.rs`, `configure_slopcop_cli` writes a `slopcop` wrapper beside the `gh` wrapper, and worker preparation in `unix.rs` calls it right after `configure_github_cli`, with the path from `credential_endpoint`. The wrapper removes its own directory from `PATH`. When the login file exists, it reads the token and the optional URL and exports them as `SLOPCOP_TOKEN` and `SLOPCOP_SERVICE`. It then runs `slopcop`, or, when no `slopcop` remains on `PATH`, says so on stderr and exits 127.

On the controller, in `mj-controller/src/worker_client/credential_sync.rs`, `reconcile_profile_guarded` reads the host login through its `SlopcopLoginSource` parameter, once, on a blocking task, and only when some session in the profile is isolated, because it reads a file and the daemon's event loop must never block. It carries the result in `SessionCanonical.slopcop_login`. `reconcile_on` calls `reconcile_slopcop_login`, which converges the session copy, records `SlopcopLoginPushed` or `SlopcopLoginRemoved`, and quietly skips a worker whose protocol is below 47; such a worker is replaced when its session is idle. `mj-controller/src/pollers/credential_sync.rs` turns those actions into the routine notice "Synced the slopcop login to N session(s)."

## Concrete Steps

All commands run from the repository root.

Milestone 1 validation, without building the whole image:

    podman run --rm docker.io/library/debian:trixie-slim sh -euc '<the new RUN body, with curl installed first>'

It must print `slopcop version v0.0.7`. Then commit, push to `origin/master`, and watch the publish workflow:

    gh run list --workflow publish-agent-dev-image.yml --limit 1
    gh run watch <run id> --exit-status

Milestone 2 end to end, from the repository root, with the host signed in to slopcop and an image that has it:

    cargo build -p brokk-mjolnir --features test-hooks
    scripts/build-linux-worker.sh podman
    python3 tests/e2e/slopcop_login.py --mj target/debug/mj \
        --worker target/worker/x86_64-unknown-linux-musl/debug/mj-worker \
        --image ghcr.io/brokkai/mjolnir/agent-dev:latest --instance slopcop-login

It prints a JSON line with `"result": "passed"`.

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

Four tests protect this:
- `slopcop_login_sync_converges_current_workers_and_never_asks_older_ones` in `mj-controller/src/worker_client/tests.rs`: a protocol-46 worker is never sent a slopcop request, while a protocol-47 worker gets the login pushed, rotated, kept on `Unknown`, and removed on sign-out.
- `credential_exchange_stays_on_the_connection_and_out_of_relay_state` in `mj-worker/src/worker_runtime/relay_tests.rs`, now also covering slopcop: the login is installed owner-only, read back and removed over a real worker connection, and never reaches persisted relay state.
- `slopcop_wrapper_exports_each_live_login`: every invocation reads the current login file.
- `host_login_follows_slopcop_and_reports_an_unreadable_configuration_as_unknown` in `mj-core/src/slopcop.rs`: slopcop's own file format and precedence, and an unreadable file reported as unknown rather than signed out.

## Idempotence and Recovery

Every step is additive. Rebuilding the image repeats the download and hash check. Credential sync is a fingerprint comparison, so running it again with the same login does nothing. Removing a login on the host removes the session copy at the next cycle. If a wrapper or login file is damaged, resuming the session rewrites it, as for the GitHub wrapper.

## Artifacts and Notes

slopcop v0.0.7 Linux release hashes, from the release's `checksums.txt`:

    552e197f2d5c73fd43c7fc010a608c7560e0ecebd1a04a95e7901e83b0d95d3d  slopcop_v0.0.7_linux_amd64.tar.gz
    5337403cd50fb99e9b53fef9dc01473ccbfd2a5e5a9643252d4e9e9b0a3f967a  slopcop_v0.0.7_linux_arm64.tar.gz

## Interfaces and Dependencies

No new crate dependencies are expected. slopcop keeps its login in Go's `os.UserConfigDir`, which is `$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support` on macOS, and `%AppData%` on Windows. The Rust side must resolve the same directory.
