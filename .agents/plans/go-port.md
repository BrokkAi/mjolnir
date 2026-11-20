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
- [ ] Owner confirms the approach before the work fans out across agents.
- [ ] Milestone 1 rest: all of `mj-core` (config, state, relay protocol, transcript, subprocess, targets, credentials, harness install/runtime, acp on acp-go).
- [ ] Milestone 2: leaf crates `mj-transcript`, `mj-checkpoint`, `mj-review`, `mj-session-types`, `mj-targets`, `mj-client`.
- [ ] Milestone 3: `mj-worker` and `mj-worker-bin` (the data plane: harness process, ACP connection, relay journal).
- [ ] Milestone 4: `mj-store` and the crates on it (`mj-accounts`, `mj-import`, `mj-sessionwiki`, `mj-build-cache`, `mj-worker-staging`, `mj-worker-launch`, `mj-profile-catalog`, `mj-session-runtime`, `mj-review-host`, `mj-github-relevance`).
- [ ] Milestone 5: control plane `mj-controller`, `mj-server`, `mj-daemon`, `mj-setup`.
- [ ] Milestone 6: `mj-chat` and `mj-tui` on Bubble Tea and Lip Gloss; `mj-cli` commands and dashboard.
- [ ] Milestone 7: end-to-end parity: the shell/expect suite in `tests/e2e/` passes against `go/bin/mj`.


## Surprises & Discoveries

- Observation: clap lists a command's own options interleaved with the global ones by declaration index (`--instance`, `--session`, `--all`, `--no-splash`, ...), and shows the current environment value in `[env: NAME=value]`.
  Evidence: `mj sessions --help` on the Rust build. `go/internal/cli/clapstyle.go` reproduces the ordering with an `mj_order` annotation per flag.

- Observation: cobra sorts subcommands alphabetically and adds its own `help` and `completion` commands; both must be turned off for clap's output.


## Decision Log

- Decision: the Go build is a separate program in `go/`, not a component the Rust daemon drives.
  Rationale: the owner asked for a replacement; mixing the two was never asked for.
  Date/Author: 2026-11-20, agent.

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
