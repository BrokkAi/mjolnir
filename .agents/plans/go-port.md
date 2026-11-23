# Port Mjolnir to Go, feature for feature

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.


## Purpose / Big Picture

Mjolnir (`mj`) is a terminal dashboard and background service that runs coding agents (Claude Code, Codex, OpenCode and others) as persistent sessions. It is written in Rust: about 935,000 lines across 29 crates (Rust packages) in this repository. The owner asked for a Go build of the same product that replaces it feature for feature, with the same behaviour a user sees, built on Go's standard library or established Go packages, using `github.com/BrokkAi/acp-go` for the Agent Client Protocol (ACP, the JSON-RPC protocol over standard input and output that Mjolnir uses to drive each agent) and Charm's libraries (Bubble Tea, Lip Gloss, Bubbles) in place of ratatui for the terminal interface.

When this plan is done, someone can build `go/bin/mj` with `go/scripts/build.sh`, run it against an isolated instance (`mj --instance <name> ...`), and use every command, the dashboard, the daemon and remote workers exactly as they would use the Rust `mj`.

The Go build lives in `go/` as a complete, separate program (Go module `github.com/BrokkAi/mjolnir/go`). It does not call into the Rust build and the Rust build does not call into it. Both use the same configuration format, database schema and wire formats, so a user can switch between them on the same data.


## How to approach the port (read this first)

An earlier attempt on 2026-11-20 ported Rust's own libraries (tokio, serde, clap, ratatui and others) into Go before touching Mjolnir's code. It produced about 270,000 lines of infrastructure and no Mjolnir, and was reverted in commit 7b78e794a. Do not repeat it. Translate Mjolnir's own code, crate by crate, onto Go equivalents:

    Rust crate                  Go replacement
    tokio, futures              goroutines, channels, context.Context, sync
    serde, serde_json           encoding/json (custom MarshalJSON where serde attributes need it)
    toml, toml_edit             github.com/BurntSushi/toml (toml_edit's format-preserving edits: decide when config saving is ported)
    clap                        github.com/spf13/cobra, with help rendered in clap's layout by go/internal/cli/clapstyle.go
    ratatui, crossterm          charm.land/bubbletea/v2, charm.land/lipgloss/v2, Bubbles
    agent-client-protocol       github.com/BrokkAi/acp-go
    rusqlite                    a SQLite driver for database/sql (modernc.org/sqlite, pure Go, when the store is ported)
    reqwest, axum, hyper        net/http
    sha2, base64, flate2, tar   crypto/sha256, encoding/base64, compress/*, archive/tar
    anyhow                      wrapped errors with fmt.Errorf("...: %w")
    tracing                     log/slog
    dirs                        os.UserConfigDir plus a small data-directory helper in go/internal/core/config

Each Rust crate maps to a Go package under `go/internal/` with the same name minus the `mj-` prefix (`mj-core` is `go/internal/core/...`, one subpackage per Rust module where that keeps packages acyclic). Binaries live under `go/cmd/`. Port a crate's tests together with its code: the Rust tests are the behavioural specification. Keep test names readable in Go form (`instance_names_accept_single_segment_identifiers` becomes `TestInstanceNamesAcceptSingleSegmentIdentifiers`) and keep `Hard-won:` markers.

User-visible output must match the Rust build. Prove it with golden files captured from the Rust `mj` (see `go/internal/cli/testdata/`).


## Progress

- [x] (2026-11-20) Milestone 1 slice: Go module, `go/internal/core/config` instance and directory resolution, ID validation and atomic writes (port of `mj-core/src/config/loading.rs`) with its Rust tests; `go/cmd/mj` root command tree with clap-identical top-level help, version and usage errors, proven by golden files.
- [x] (2026-11-20) Owner confirmed the approach and chose the crate-by-crate fan-out.
- [x] (2026-11-20) First slice moved into package `core`; `go/scripts/sync-assets.sh` copies mj-core/assets for `//go:embed`.
- [x] (2026-11-20) mj-core declarations phase (ten groups) merged and compiling (823be3abb).
- [x] (2026-11-20) mj-core bodies and tests phase merged: no unported bodies left, 678 of mj-core's 683 Rust tests ported, `go test ./...` passes, `go vet` passes for linux, windows and darwin. The five not ported: four test the Rust build's `worker_build_inputs.py` worker identity (the Go build needs its own; port with mj-worker-bin), plus one missed state test now added.
- [ ] Milestone 1 rest: all of `mj-core` (config, state, relay protocol, transcript, subprocess, targets, credentials, harness install/runtime, acp on acp-go).
- [x] (2026-11-20) Layer 1: mj-transcript, mj-checkpoint, mj-review, mj-build-id ported (323 tests; 2 benchmark tests skip as in Rust), merged d98013ebf.
- [x] (2026-11-20) Layer 2 declarations: mj-client (`go/internal/client`) and mj-worker (`go/internal/worker`) declared in 14 groups and merged (6a0035221); builds and vets, 971 bodies still `panic("unported: ...")`, no tests ported yet.
- [x] (2026-11-23) Layer 2 bodies and tests: mj-client and mj-worker fully ported, 1030 tests, all Go packages pass (merged 1ba15ffb6 after three rounds). Six groups had finished before the 2026-11-20 stop and were reused from the workflow cache; eight reran. Some groups read Rust at 4273087e6, others at f470ca8f7; `go/PORTED_FROM` records the older one so the sync pass re-checks the difference.
- [x] (2026-11-23) Mirrored Rust's crate extractions up to 3dde0c11b: `go/internal/harnessinstall` (mj-harness-install: harness_install, login_environment, program_path, session_image and their assets and fixtures, out of core) and `go/internal/microvm` (mj-microvm, out of worker). The guardian policy files were already in core. mj-worker-mcp and mj-worker-review, extracted in the same Rust commit, followed later the same day (next entry).
- [x] (2026-11-23) Split `go/internal/workermcp` (mj-worker-mcp: subagent_mcp, mcp_stdio, memory_mcp) and `go/internal/workerreview` (mj-worker-review: capture, hook) out of worker, and brought both to 3dde0c11b, including 35aecf3e1's worker/standalone orchestrator tool split (`spawn_worker`, `spawn_orchestrator`, `list_targets`) and its worker-side importers (Claude allow-list capabilities, no `--agent-mailboxes-enabled`).
- [ ] Sync pass: port the Rust changes made since each crate's base in `go/PORTED_FROM`.
- [ ] Milestone 2: leaf crates `mj-transcript`, `mj-checkpoint`, `mj-review`, `mj-session-types`, `mj-targets`, `mj-client`.
- [ ] Milestone 3: `mj-worker` and `mj-worker-bin` (the data plane: harness process, ACP connection, relay journal).
- [ ] Milestone 4: `mj-store` and the crates on it (`mj-accounts`, `mj-import`, `mj-sessionwiki`, `mj-build-cache`, `mj-worker-staging`, `mj-worker-launch`, `mj-profile-catalog`, `mj-session-runtime`, `mj-review-host`, `mj-github-relevance`).
- [ ] Milestone 5: control plane `mj-controller`, `mj-server`, `mj-daemon`, `mj-setup`.
- [ ] Milestone 6: `mj-chat` and `mj-tui` on Bubble Tea and Lip Gloss; `mj-cli` commands and dashboard.
- [ ] Milestone 7: end-to-end parity: the shell/expect suite in `tests/e2e/` passes against `go/bin/mj`.


## Resume here

State on 2026-11-20: mj-core and layer 1 are fully ported (all ported tests pass). Layer 2 (mj-client, mj-worker) has merged declarations only. Every phase so far ran as a Workflow script that fans out one agent per module group, each in a git worktree, then merges with cherry-picks: declarations, merge, bodies and tests, merge, then up to two fix rounds for failing tests. The script and the layer-2 group list (which Rust files each of the 14 groups owns) are in the session that ran it; to restart without it, rerun the bodies phase only: one agent per group with the prompt "BODIES phase" described in `.agents/docs/go-port/core-brief.md`, then a merge agent that cherry-picks, builds, runs `go test ./...` and reports failures by group. Layer 2's groups split mj-worker by area: acp (drive, features, tests.rs), relay (core, verdict and journal, tests.rs), worker_runtime (unix, core, relay_tests.rs), microvm, services (subagent_mcp, review, work_facts, stand_in_relay, move_workspace), misc (the rest); mj-client into daemon and the rest.

Cost measured so far: about 100 subagent tokens per Rust line (mj-core 8.4M tokens for 93k lines; layer 1 4.8M for 40k). The remaining ~730k lines, layer 2 included, are on that basis roughly 70M tokens.

After layer 2: the sync pass (`go/scripts/rust-changes.sh` per crate in `go/PORTED_FROM`), then the next layers in dependency order: mj-targets and mj-session-types; mj-store; mj-accounts, mj-import, mj-sessionwiki, mj-build-cache, mj-worker-staging, mj-worker-launch, mj-profile-catalog; mj-session-runtime, mj-review-host, mj-github-relevance; mj-controller; mj-server, mj-setup; mj-daemon; mj-chat, mj-tui (Charm); mj-cli and mj-worker-bin.

Stale worktrees from the stopped run may remain under `.claude/worktrees/`; they hold only uncommitted partial work and can be removed with `git worktree remove --force`.


## Surprises & Discoveries

- Observation: mj-core needed declarations first: in the bodies phase, agents reported dozens of tests blocked on other groups' functions, and all of them passed once merged. Two real failures remained after the first merge (a serde `default` missed in a custom decoder, and a test fixture built from Go zero values instead of `DefaultConfig()`).
  Evidence: bodies workflow wf_0dc3082d-c19, merge round 1 (675 of 677 passing) and round 2 (all passing).

- Observation: clap lists a command's own options interleaved with the global ones by declaration index (`--instance`, `--session`, `--all`, `--no-splash`, ...), and shows the current environment value in `[env: NAME=value]`.
  Evidence: `mj sessions --help` on the Rust build. `go/internal/cli/clapstyle.go` reproduces the ordering with an `mj_order` annotation per flag.

- Observation: cobra sorts subcommands alphabetically and adds its own `help` and `completion` commands; both must be turned off for clap's output.


## Decision Log

- Decision: the Go build is a separate program in `go/`, not a component the Rust daemon drives.
  Rationale: the owner asked for a replacement; mixing the two was never asked for.
  Date/Author: 2026-11-20, agent.

- Decision: all of mj-core becomes one Go package, `go/internal/core`, with one file per Rust module (`config_loading.go` for `config/loading.rs`). Rules for agents are in `.agents/docs/go-port/core-brief.md`.
  Rationale: 42 of mj-core's modules (71k of 84k lines) form one dependency cycle, which Go packages cannot express. Only 30 top-level names collide across modules; the brief settles them with a module prefix.
  Date/Author: 2026-11-20, agent.

- Decision: mj-core is ported in two fan-out phases over ten module groups: declarations (every type, constant and signature) so the package compiles, then bodies and tests against those declarations. Each agent works in its own git worktree and the results are merged.
  Rationale: parallel agents in one package need each other's signatures; declaring first means the bodies phase compiles against the real shapes instead of guesses.
  Date/Author: 2026-11-20, agent.

- Decision: keep tree-sitter-bash through cgo (`github.com/tree-sitter/go-tree-sitter`) for mj-transcript's shell command summaries.
  Rationale: Rust uses the same C parser, including its error recovery on broken shell text, which the summaries and their tests depend on; a pure-Go shell parser (mvdan.cc/sh) fails on such input instead. The cost is a C cross-compiler for Windows/macOS cross builds and for static Linux workers, which the Rust build also needs for this parser.
  Date/Author: 2026-11-20, agent.

- Decision: the Go tree records, per crate, the Rust commit it was translated from (`go/PORTED_FROM`); `go/scripts/rust-changes.sh <crate>` shows what Rust changed since. Sync passes port those diffs.
  Rationale: Rust development continues during the port (about 30 commits touched ported crates within hours); without a recorded base the Go port silently falls behind.
  Date/Author: 2026-11-20, agent.

- Decision: when Rust extracts modules into a new crate, the Go code that ports them moves to the new crate's package (git mv, same file names relative to the new crate), keeps the Go names it had, and `go/PORTED_FROM` records the new crate with the base the code was ported from; `go/scripts/rust-changes.sh` follows the old paths for it.
  Rationale: later syncs map one Rust crate to one Go package. Renaming the collision-prefixed names (`LoginEnvironmentEnvironment`, `MicrovmVMRun`) would churn every importer for no behaviour change; the final cleanup can drop the prefixes.
  Date/Author: 2026-11-23, agent.

- Decision: parse with cobra but render help and usage errors with a small clap-layout renderer.
  Rationale: cobra is the established Go command-line library; the help text is user-visible behaviour and must match.
  Date/Author: 2026-11-20, agent.

- Decision: a development build is recognised by a `.mj-development-build` directory beside the binary (created by `go/scripts/build.sh`), or by Go's temporary `go-build` directories, in place of Cargo's `.fingerprint`.
  Rationale: keeps the rule that development builds never take over the default instance (Hard-won f7272bb0).
  Date/Author: 2026-11-20, agent.


## Outcomes & Retrospective

Not yet: the port is at its first slice.


## Context and Orientation

The Rust workspace's crates, from the bottom of the dependency graph up: `mj-core` (no internal dependencies; configuration, state model, relay protocol, subprocess helpers, targets), then `mj-transcript`, `mj-checkpoint`, `mj-review`, `mj-build-id`, then `mj-client`, `mj-worker`, `mj-targets`, `mj-store`, and so on up to `mj-cli` (the `mj` binary). `AGENTS.md` describes the control plane (the daemon, `mj daemon-run`) and the data plane (one `mj-worker` per session). The Go packages follow the same layering.


## Plan of Work

Port bottom-up so every package compiles and its tests pass before anything depends on it, but surface each piece through the `mj` binary as soon as a command can use it, so progress is visible from the command line. Each Rust command currently returns "this command is not yet ported to the Go build of mj" in the Go build; replace that as each command is ported, and add a golden file of its Rust output.


## Concrete Steps

From the repository root:

    cd go
    ./scripts/build.sh                 # builds go/bin/mj
    go vet ./... && go test ./...      # all ported tests
    diff <(mj --help) <(bin/mj --help) # against an installed Rust mj

Test the Go build only in an isolated instance (`bin/mj --instance go-test ...`) and stop that instance's daemon when done.


## Validation and Acceptance

For the first slice: `go test ./...` passes, and `bin/mj --help`, `bin/mj -h`, `bin/mj help`, `bin/mj --version` and `bin/mj nosuch` print exactly what the Rust `mj` prints (the golden tests in `go/internal/cli/golden_test.go` check `--help` and the unknown-command error). The final acceptance is the end-to-end suite in `tests/e2e/` passing against `go/bin/mj`.


## Idempotence and Recovery

Everything is additive under `go/` and `.agents/plans/go-port.md`. Rebuilding is safe. Set `MJ_UPDATE_GOLDEN=1` only when deliberately recapturing a golden file from the Rust build.


## Artifacts and Notes

    $ diff <(mj --help) <(go/bin/mj --help) && echo HELP-SAME
    HELP-SAME


## Interfaces and Dependencies

Go 1.27.1 or newer. Modules: `github.com/spf13/cobra`, `github.com/BurntSushi/toml`, `github.com/BrokkAi/acp-go`, `charm.land/bubbletea/v2`, `charm.land/lipgloss/v2`; more are added only as the crate that needs them is ported, each replacing the Rust dependency listed in the table above.
