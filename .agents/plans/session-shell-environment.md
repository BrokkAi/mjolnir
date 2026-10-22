# Give sessions the toolchain the user's terminal has

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain it according to `.agents/PLANS.md`.

## Purpose / Big Picture

Today a Mjolnir session does not see the tools its owner's terminal can run, whenever those tools are put on `PATH` by a shell startup file that only interactive shells read. The most common case is Node.js installed with nvm: the nvm block lives in `~/.zshrc` or `~/.bashrc`, while Mjolnir starts the account's shell as a non-interactive login shell, which reads neither file. The session then fails with `Mjolnir needs Node.js, npm, and npx on PATH; install Node in the target environment`, even though `node` works in the user's terminal. The same failure hits pyenv, asdf, mise, volta, sdkman and any other version manager that initializes from an rc file.

After this change, a user who installed their toolchain the ordinary way can start a session and it works, with no Mjolnir-specific shell edit, no `PATH` copied into `config.toml`, and no documentation telling them to move their nvm block. Mjolnir reads the same startup files a terminal reads, while still refusing to inherit variables from whatever process launched Mjolnir.

A second, independent defect breaks the same users once Node is visible. npm 12 blocks dependency install scripts unless a package is explicitly allowed. Mjolnir installs its pinned Claude Code harness with `npm ci`, so the package's `postinstall` never runs and `bin/claude.exe` stays the "native binary not installed" stub. The bridge then fails with `claude auth status failed: spawn ENOEXEC`. After this change, the pinned install runs the install scripts it declares, and Mjolnir fails loudly at install time if a pinned package's scripts were blocked, instead of producing a harness that cannot start.

The observable result: on a machine whose only `node` comes from nvm in `~/.zshrc`, a Claude or Codex session starts, and the failure that used to say only "install Node in the target environment" no longer happens.

## Progress

- [x] (2026-10-22) Write this plan.
- [x] Probe the account's interactive login shell in `mj-core/src/login_environment.rs`.
- [x] Extend the checked-in fixture shell and the login-environment tests to cover interactive startup files.
- [x] Add `allowScripts` to the pinned Claude harness asset and assert after `npm ci` that no install scripts are blocked.
- [x] Bump the Claude install identity so caches written before the `allowScripts` change are replaced instead of reused.
- [x] Update `docs/src/content/docs/profiles.md`, `docs/src/content/docs/troubleshooting.md` and `docs/src/content/docs/custom-images.md`.
- [x] Run the workspace suite and clippy in the session clone.
- [ ] Commit.

- [ ] (Deferred) Report the session toolchain from `mj doctor`.

## Surprises & Discoveries

- Observation: the daemon process itself had nvm on `PATH` and `NVM_DIR` set, but its workers did not. `ps eww` on `mj daemon-run` shows `NVM_DIR=/Users/ryansvihla/.nvm` and `.../v24.18.0/bin` on `PATH`; the identical probe Mjolnir runs (`env -i HOME=… SHELL=/bin/zsh /bin/zsh -l -c '…'`) resolves no `node`.
  Evidence: `/Users/ryansvihla/Library/Application Support/mjolnir/logs/mj-daemon-20261022T063718.632Z-26545.log` records `verify Node.js 22 or newer for managed harness installation: start bounded subprocess: No such file or directory (os error 2)` for the `codex`, `codex-ds` and `claude` profiles.

- Observation: adding `-i` to the probe makes `.zshrc` run. A fixture home with `export FROM_ZSHRC=zshrc` in `.zshrc` yields `FROM_ZSHRC=zshrc` and the rc-prepended `PATH` entry, with exit status 0 and no stdout after the environment frame.

- Observation: on this machine `zsh -l -c 'exec /usr/bin/env -0'` takes about 0.7 s and `zsh -l -i -c …` about 1.5 s, because both `.zprofile` (temporarily) and `.zshrc` source nvm. The probe runs once per worker process, not once per request.

- Observation: npm 12.0.2 refuses `--allow-scripts` on project installs (`EALLOWSCRIPTS`) and rejects unknown flags, but honors `allowScripts` in the root `package.json` and reports pending approvals through `npm install-scripts ls --json` as `{"allowScripts":[{"name":"esbuild","changes":[{"key":"esbuild@0.25.0","change":"pending"}]}]}`.

- Observation: the managed-harness manifest is `{schema, harness, install_id}`, and `install_id` is a static pin, so changing the asset alone does not replace an existing install. The Claude pin therefore carries a `pkg2` suffix; older workers keep using the old directory and the garbage collector removes it, which is the same coexistence a version bump already produces.

- Observation: with the probe corrected, the whole interactive shell environment arrives, including variables a user exported in `~/.zshrc`. On the development machine that includes `DEEPSEEK_API_KEY` and `OPENROUTER_API_KEY`, which `mj doctor` previously reported as missing for the `codex-ds` profile. This is the same contract a terminal has and is not a leak from the launcher.

- Observation: an isolated `--instance` run from inside a session needs `MJ_CONFIG_DIR`, `MJ_DATA_DIR` and the other `MJ_*` session variables unset; the session environment pins them to the default instance, and they take precedence over the instance flag.

- Observation: `verify_install_scripts_ran` failed the install loudly when the `allowScripts` entry was removed: `managed Claude Code harness install skipped install scripts for @anthropic-ai/claude-code@2.1.306. npm gates dependency install scripts, so add each package to allowScripts in mj-worker/assets/harnesses/claude/package.json and reinstall.` Restoring the entry installed a Mach-O arm64 binary that reports `2.1.306 (Claude Code)`.

## Decision Log

- Decision: probe the account's shell as an interactive login shell (`-l -i -c`) instead of a noninteractive login shell (`-l -c`).
  Rationale: the fact Mjolnir needs is "which commands this account can run in a terminal". That fact is owned by the shell startup sequence; reproducing it means starting the shell the same way a terminal does. Every version manager that people actually use (nvm, pyenv, asdf, mise, volta, sdkman, Homebrew on Linux) initializes from `.zshrc`/`.bashrc`, which only an interactive shell reads. Isolation from the launcher is unchanged: the probe still starts from a cleared, seeded environment, so neither the daemon's variables nor the parent agent's variables leak in.
  Date/Author: 2026-10-22, primary agent.

- Decision: reject the two alternatives — merging the daemon's `PATH` with the login `PATH`, and probing login plus interactive shells and merging results.
  Rationale: both create two owners for one fact and an arbitrary precedence rule. Merging the launcher `PATH` also reinstates exactly the pollution the existing design removed. One shell, run the way a terminal runs it, has one answer.

- Decision: reject the shipped workaround of telling users to duplicate their nvm block into `~/.zprofile`.
  Rationale: nvm is the default way Node is installed, and a product that requires editing shell startup files to be seen is broken for most of its Node users. The workaround is acceptable only as a temporary local unblock, never as the documented answer.

- Decision: keep the five-second deadline but raise it to ten seconds for the probe, and name the shell and the deadline in the failure.
  Rationale: an interactive seed can run plugin managers and completion setup; the observed 1.5 s is well inside the bound, and ten seconds still fails a hung startup file visibly instead of hanging a session.

- Decision: for managed harness installs, list the packages whose install scripts are required in the pinned asset `package.json` (`allowScripts`) and verify afterwards that npm reports no pending install scripts, rather than setting `NPM_CONFIG_DANGEROUSLY_ALLOW_ALL_SCRIPTS`.
  Rationale: the asset already pins exact versions with a lockfile, so naming the package is as maintainable as the pin itself, keeps the permission explicit, and produces no warnings on npm versions that do not gate scripts. The post-install assertion turns a forgotten entry into a clear installer error instead of an unusable harness.

- Decision: defer the `mj doctor` session-toolchain check to a follow-up, and leave the launch preflight as the authoritative Node check.
  Rationale: naming which harnesses need Node in doctor would create a second owner for a fact the managed installer already owns, and the mismatch doctor showed here (profiles "ready" while launches failed) is better fixed by one authoritative check than two. Recorded as a follow-up rather than silently dropped.
  Date/Author: 2026-10-22, primary agent.

## Outcomes & Retrospective

Sessions now start from the environment the account's terminal has. The probe starts the shell as an interactive login shell, so nvm, pyenv, asdf, mise, volta, sdkman and Homebrew on Linux are visible without editing dotfiles or writing `PATH` into `config.toml`. This was proven by hand on the development machine with the temporary nvm block removed from `~/.zprofile`: the probe resolved `node`, `npm` and `npx` from `~/.nvm/versions/node/v24.18.0/bin`, and an isolated instance answered `models --profile claude`. The launcher-isolation tests still pass, so nothing the process that started Mjolnir exported leaks into a session. One consequence worth knowing: variables exported by `~/.zshrc`/`~/.bashrc` now reach sessions, exactly as they reach a terminal.

The managed Claude install now runs the package's install script. With `allowScripts` present the cached entrypoint is a Mach-O arm64 binary reporting `2.1.306 (Claude Code)`, and `npm install-scripts ls --json` reports nothing pending; with the entry removed, the installer fails loudly and names the package and the asset to edit. The `pkg2` install identity makes caches written by the earlier packaging get replaced instead of reused.

Remaining: `mj doctor` still reports harness homes and authentication rather than the toolchain a session will have; that check is the natural follow-up. The preparation process-group test failed twice on this machine while it was heavily loaded and passed alone and in a later full run; it waits two seconds for a fake harness to start, which is sensitive to load and independent of this change.

## Context and Orientation

Mjolnir runs each session in a worker process (`mj-worker`). Before a worker starts the harness, it needs the environment the user's terminal would have: `PATH`, `HOME`, and the user's tool settings. That environment is discovered by `mj-core/src/login_environment.rs`. Its `discover_account` function starts the account's shell (from the passwd database, never from `$SHELL`), prints a random NUL-framed marker, and then `exec`s `/usr/bin/env -0` so the account's exported variables can be parsed from stdout. The capture runs through `BoundedProcessExecutor` in `mj-core/src/targets.rs` with a cleared environment and a deadline, so a startup file that hangs cannot hang Mjolnir. Successful discovery is cached in a `tokio::sync::OnceCell` per process (`resolve`), and `with_overrides` applies explicit profile settings on top. `mj-worker/src/main.rs` re-execs the worker with this environment before the runtime starts (`initialize_from_parent`), so the harness, reviewers, probes, installers, user shells and ACP bridges all see it.

The startup files a shell reads depend on two flags. A *login* shell reads the files for a session's first shell (`/etc/profile`, `~/.zprofile` and `~/.zlogin` for zsh; `~/.bash_profile` or `~/.profile` for bash). An *interactive* shell reads the files for a terminal (`~/.zshrc` for zsh; `~/.bashrc` for bash). A terminal normally starts one shell that is both, so users put tool initialization wherever their shell's documentation says: nvm's installer writes to `~/.zshrc` on zsh and to `~/.bashrc` on bash.

Managed harnesses are installed by `mj-worker/src/worker_runtime/harness.rs`. `install_npm` writes the asset `package.json` and `package-lock.json` from `mj-worker/assets/harnesses/<harness>/` into a staging directory and runs `npm ci --omit=dev --no-audit --no-fund` there. The result is validated by `validate_entrypoint`, published by rename into `~/.cache/mjolnir/harnesses/<harness>/<install-id>/`, and leased for the life of the session. npm 12 (shipped with Node 24 and later) blocks dependency install scripts by default; a blocked script is a warning, not an error, so `validate_entrypoint` still sees the stub file the package shipped.

`mj doctor` is implemented in `mj-controller/src/doctor.rs` and rendered by `mj-cli/src/main.rs`. Each check is a `DoctorCheck` with the status values `ready`, `fixable`, `warning` and `unsupported`. The new session-toolchain check belongs beside `harness_discovery_check`.

## Plan of Work

First, change the probe. In `mj-core/src/login_environment.rs`, pass `-l`, `-i` and `-c` to the account's shell and give the command a purpose that says what it reproduces ("initialize target shell environment"). Introduce a named timeout constant of ten seconds with a comment explaining the interactive startup cost, and update the doc comments on `discover` so a reader learns that the probe deliberately reproduces a terminal, not a cron job. Keep `bootstrap` unchanged: recovery that cannot run shell files must still work.

Second, make the tests prove the interactive contract. `mj-core/tests/fixtures/login-shell.sh` is the stand-in account shell; it must accept the new flags and source an interactive startup file in addition to the login file, so the existing capture test fails if `-i` is dropped. Add a test that a variable exported only by the interactive startup file reaches the discovered environment, and keep the existing tests for hostile startup output, failure status and the deadline.

Third, fix the managed install. Add an `allowScripts` entry for the pinned `@anthropic-ai/claude-code` version in `mj-worker/assets/harnesses/claude/package.json`. Extend `install_npm` in `mj-worker/src/worker_runtime/harness.rs` with a post-install step that runs `npm install-scripts ls --json` in the staging directory, parses the `pending` entries, and fails with a message naming the packages and the asset file to edit. Treat a missing `install-scripts` command (npm versions that do not gate scripts) as nothing to check; do not swallow other failures. Pass the harness kind into `install_npm` so the message names the right asset.

Fourth, rewrite the documentation that currently teaches the opposite. In `docs/src/content/docs/profiles.md`, `docs/src/content/docs/troubleshooting.md` and `docs/src/content/docs/custom-images.md`, replace "a binary visible only by `.zshrc` can work interactively and still be absent from Mjolnir's runtime" with the new contract: the probe starts the account's shell as an interactive login shell, so those files are read; explain the remaining differences (Mjolnir never uses shell aliases or functions, and `!` commands run through `bash -lc`).

## Concrete Steps

Work in the repository root. After each step run its focused tests from the repository root, outside the restricted sandbox, because the suite uses loopback sockets:

    cargo test -p mj-core login_environment
    cargo test -p mj-worker worker_runtime::harness

To see the behavior by hand, run a session against a throwaway instance, which never touches the default instance, and read the worker log it writes:

    cargo build --bin mj --bin mj-worker
    target/debug/mj --instance shell-probe models --profile claude

Before the change, the instance's log shows `verify Node.js 22 or newer for managed harness installation: No such file or directory`. After the change, the command lists the profile's models.

To prove the interactive probe without touching the real account, create a throwaway home whose `.zshrc` exports a variable and a `PATH` entry, then run the same command the probe runs:

    mkdir -p /private/tmp/probe-home
    printf 'export FROM_ZSHRC=zshrc\n' > /private/tmp/probe-home/.zshrc
    env -i HOME=/private/tmp/probe-home USER=u LOGNAME=u SHELL=/bin/zsh \
      PATH=/usr/bin:/bin /bin/zsh -l -i -c 'printf "\000MARK\000"; exec /usr/bin/env -0' \
      | tr '\0' '\n' | grep FROM_ZSHRC

The last command must print `FROM_ZSHRC=zshrc`. Running the same command without `-i` prints nothing, which is the defect this plan fixes.

To validate the harness install, remove only the affected cache install directory and ask the throwaway instance for the profile's models; the installer must reinstall, the entrypoint must be the native binary, and npm must report no pending install scripts:

    rm -rf ~/.cache/mjolnir/harnesses/claude/claude-agent-acp-*/
    target/debug/mj --instance shell-probe models --profile claude
    file ~/.cache/mjolnir/harnesses/claude/claude-agent-acp-*/node_modules/@anthropic-ai/claude-code/bin/claude.exe

A Mach-O or ELF executable is correct; a text file that echoes "native binary not installed" means the install scripts were skipped.

Finish with the workspace checks once the change is complete:

    cargo test
    cargo clippy --all-targets -- -D warnings

Stage only the files this plan touches, commit on the current branch, and do not push unless asked.

## Validation and Acceptance

Acceptance is behavioral. A user whose only Node.js comes from nvm in `~/.zshrc` can start a session; `mj doctor` names the Node.js, npm and npx paths the session will use; and the managed Claude harness installs a real native binary, so `claude auth status` runs instead of failing with `spawn ENOEXEC`.

The tests must fail for the right reasons when the fix is removed. Dropping `-i` from the probe must fail the interactive-startup test. Removing the `allowScripts` entry must fail the managed install with an error that names the package and the asset file. A startup file that exits non-zero, prints hostile output or exceeds the deadline must still fail without echoing secrets and without inheriting the launcher's environment.

Do not accept a green suite as the only evidence: run the probe by hand as shown above, and confirm in the doctor output that the paths point into the nvm version directory rather than to a system Node.
