# Port Mjolnir to Go, running beside the Rust implementation

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This document is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Mjolnir is written in Rust: 29 crates, about 459,000 lines of production code and 386,000 lines of tests (6,144 tests). The owner wants the same program in Go, in this repository, built and run beside the Rust one so the two can be compared. "The same program" is meant literally. The Go `mj` and `mj-worker` accept the same commands, print the same text, speak the same protocols, read and write the same files and database, and pass the same tests. A Rust daemon can drive a Go worker and a Go daemon can drive a Rust worker. Shortcuts that make the two merely look alike are out of scope by the owner's instruction, and so is persuading the owner to do something else.

After this plan is complete, a person can run `scripts/go/build.sh` to get `target/go/release/mj` and `target/go/release/mj-worker`, start `target/go/release/mj --instance go-demo` next to `target/release/mj --instance rust-demo`, and use either the same way. They can run `go/port/gotest.sh ./go/...` to run the Go counterpart of every Rust test, `python3 go/port/status.py` to see what is ported and whether any Rust file has changed since its Go counterpart was written, and `go/parity/run.sh` to drive both implementations with the same inputs and see that their outputs are identical.

Until then, `python3 go/port/status.py` always says exactly how far the port is.

## Progress

- [x] (2026-11-20 08:45Z) Surveyed the workspace: 29 crates, 1,004 Rust files, 459,085 production lines, 385,557 test lines, 6,144 tests, 88 direct third-party crates.
- [x] (2026-11-20 08:55Z) Built `go/port/rsindex` (parses every Rust file) and `go/port/portmap.py` (module graph, schedule, Rust-to-Go names, test names).
- [x] (2026-11-20 09:25Z) Wrote the translation rules (`.agents/docs/go-port/translation-rules.md`) and the runtime package catalogue (`.agents/docs/go-port/runtime-packages.md`).
- [x] (2026-11-20 09:40Z) Milestone 0, skeleton: root `go.mod`, `assets.go` embedding the 98 files the Rust code includes, `go/assets`, `go/rs` core (Option, Result, tuples, panics, type registry, formatter and container interfaces).
- [x] (2026-11-20 10:25Z) Milestone 0, instruments: `go/port/oracle` with every planned module registered, the Go oracle helper, `go/port/status.py`, `go/port/gotest.sh`, `scripts/go/build.sh`, `.github/workflows/go.yml`.
- [x] (2026-11-20 10:20Z) Milestone 0, declarations generator: `portmap.py declarations` writes a Go skeleton for all 849 production files (974 Go files, about 181,000 lines) with no syntax errors; about 1,200 places are marked `PORT:` for a person to settle, of which about 400 are ACP types awaiting `acp-mapping.md`.
- [ ] Milestone 0, remaining: a check that every `rs.Format` string parses and its arguments match (the `rsfmt` vet), the `exhaustruct` and `go-check-sumtype` checks in the Go lane.
- [ ] Milestone 1: runtime packages. Started 2026-11-20 09:50Z with twenty parallel units (the harness runs at most twenty at once; eight more assignments wait in `.agents/docs/go-port/briefs/`): the formatter; clone, equality, ordering and counted references; numbers, sorting and slices; `strs`; `collections`; `rspath` and `rsenv`; `rsio` and `rsfs`; `rssync`, `rsthread` and `rstime`; `serde` and `serdejson`; `toml` and `tomledit`; `anyhow`, `tracing`; the `tokio` core; `clap`; `chrono`; `similar`, `linkify`, `semver`, `bytesize`, `base64`; `unicodewidth`, `unicodesegmentation`; `vt100`, `vte`; `pulldowncmark`; `url`, `qrcode`, `regex`; and four analysis documents (`acp-mapping.md`, `counted-arcs.md`, `compressed-bytes.md`, `library-usage.md`). None is merged yet.
- [ ] Milestone 2: foundation crates (`mj-core`, `mj-review`, `mj-transcript`, `mj-checkpoint`, `mj-build-id`).
- [ ] Milestone 3: the Go worker (`mj-worker`, `mj-worker-bin`), driven by the Rust daemon.
- [ ] Milestone 4: client, targets, store and the session crates.
- [ ] Milestone 5: controller, server and daemon; the Go daemon serving Rust clients.
- [ ] Milestone 6: terminal stack, `mj-chat`, `mj-tui`.
- [ ] Milestone 7: `mj-cli`: the whole `mj` program, and the end-to-end suite.
- [ ] Milestone 8: packaging, platforms, and keeping up with Rust changes.

## Surprises & Discoveries

- Observation: Rust modules inside a crate depend on each other in large cycles, so a crate cannot be ported one file at a time in dependency order.
  Evidence: `python3 go/port/portmap.py schedule` reports one group of 67 files and 49,566 production lines in `mj-core`, 59 files in `mj-tui`, 56 in `mj-daemon`, 37 each in `mj-worker` and `mj-store`.
- Observation: Code measures the length of its own serialized messages, so the JSON encoding has to match byte for byte, not just decode to the same value.
  Evidence: `mj-core/src/relay/protocol.rs`, `transfer_chunk_capacity`: `serde_json::to_vec(envelope)?.len()` decides how large a chunk fits in a relay frame.
- Observation: Reference counts are program logic in about thirty places, so `Arc` cannot always become a garbage-collected pointer.
  Evidence: `mj-session-runtime/src/worker_client/credential_sync.rs:461` keeps a profile only while `Arc::strong_count(profile) > 1`; `mj-controller/src/controller/worktree.rs:732` keeps a map of `Weak<Mutex<()>>` locks.
- Observation: `serde_json` is built with `preserve_order`, so a JSON object keeps insertion order, and the map type is `BTreeMap` almost everywhere else (2,406 uses against 156 of `HashMap`). Ordering is part of the output throughout.
  Evidence: `cargo tree -p brokk-mjolnir -e features` lists `serde_json ... preserve_order`.
- Observation: Go names made by joining PascalCase parts collide where Rust keeps items apart by namespace and case.
  Evidence: type-checking the generated declarations of `mj-core` reported 78 "is not a type" and 120 redeclaration errors: the method `RelayCommand::kind` and the type `RelayCommandKind` both became `RelaySnapshotRelayCommandKind`. With `<ModulePrefix>_<rust name>` the same check reports none.
- Observation: `cargo machete`, the product's "Unused dependencies" job, scans every `Cargo.toml` in the repository, not only workspace members.
  Evidence: the job failed from commit `1c3f41552` (the first push of `go/port/oracle`) until `63fe420b3` listed the oracle's dependencies under `package.metadata.cargo-machete`. Reproduced and verified with cargo-machete 0.9.2 on an export of the commit.
- Observation: The "Tests (worker-cli)" job on `master` was already failing before the port's first commit, so a red CI run on a port commit needs its failing jobs compared with the run before it.
  Evidence: runs for `63c80bc5a` and `bedb7fcae` fail only in that job.
- Observation: The owner's `github.com/BrokkAi/acp-go` (v0.14.0, Apache-2.0, no dependencies) already reproduces the Rust ACP crate's JSON exactly and exposes `jsonvalue`, an exact `serde_json::Value`.
  Evidence: its README: "Over 14,000 decode and encode cases, captured from the Rust schema crate itself".

## Decision Log

- Decision: The Go code lives in this repository under `go/`, in one Go module whose `go.mod` is at the repository root.
  Rationale: The Rust build embeds about a hundred files from many directories (`mj-core/assets`, `mj-server/src/web`, `mj-store/src/database/*.sql`, `Cargo.lock`, ...). Go can embed only files below the embedding package inside one module. A root module lets one small root package embed those exact files, so the two implementations cannot drift apart on assets, and the Go tests can read the Rust goldens and fixtures in place.
  Date/Author: 2026-11-20, Claude.
- Decision: Translate structure for structure: one flat Go package for each crate, one Go file for each Rust file, names derived by a fixed rule from the Rust path.
  Rationale: The owner asked for Mjolnir itself and allowed non-idiomatic Go. A mechanical mapping lets many people or agents port different files at once and still agree on every name, makes each Go file reviewable against one Rust file, and makes later Rust changes portable file by file.
  Date/Author: 2026-11-20, Claude.
- Decision: Build Go packages that stand in for Rust's standard library and each third-party crate, with the same names and the same observable behaviour, before porting product code.
  Rationale: Most visible behaviour (error text, number and path formatting, channel closing, scheduling order, help text, terminal cells) comes from libraries. Go's own libraries differ in each of these. Reproducing the libraries once is what lets product code be translated call for call instead of re-decided at every call site.
  Date/Author: 2026-11-20, Claude.
- Decision: Reproduce tokio's scheduling and cancellation: a current-thread flavour that runs one task at a time and switches only at awaits, cancellation that ends a goroutine at the call it is blocked in and runs its deferred calls, and a pausable clock.
  Rationale: 1,574 tests are `#[tokio::test]`, 111 start with the clock paused, and the code has 265 `select!` blocks and 131 `Drop` implementations whose behaviour depends on where a future stops. Goroutines and `context` alone give different interleavings and no drop-at-await.
  Date/Author: 2026-11-20, Claude.
- Decision: Within a crate, port in two passes: first every type and function signature with bodies that panic (`rs.Todo`), then the bodies and tests file by file.
  Rationale: The module cycles above. Once declarations compile, files can be ported in parallel and in any order.
  Date/Author: 2026-11-20, Claude.
- Decision: Record progress in the Go files themselves (a header naming the Rust file and its Git blob), not in a separate ledger.
  Rationale: A ledger edited by many hands conflicts and goes stale. The header is written once with the file; `go/port/status.py` derives everything else.
  Date/Author: 2026-11-20, Claude.
- Decision: Use `github.com/BrokkAi/acp-go` for the Agent Client Protocol instead of porting the Rust crate, and its `jsonvalue` as the representation of `serde_json::Value`.
  Rationale: The owner's instruction (2026-11-20).
  Date/Author: 2026-11-20, owner; recorded by Claude.
- Decision: No C in the Go build (`CGO_ENABLED=0`). SQLite through `modernc.org/sqlite`; tree-sitter's C sources compiled to WebAssembly and run by a pure-Go runtime; ONNX Runtime loaded at run time.
  Rationale: The Rust build already avoids a C toolchain where it can (see the comments in `Cargo.toml`), and pure Go cross-compiles every platform from one Linux machine, which also lets CI check macOS and Windows builds without macOS runners.
  Date/Author: 2026-11-20, Claude.
- Decision: A product item is named `<ModulePrefix>_<rust name as written>` (`Runtime_block_on`, `Runtime_ShutdownState_new`, `RelayProtocol_RelayRequest_Hello`); methods and fields stay PascalCase or camelCase. Runtime packages keep ordinary PascalCase names.
  Rationale: Joined names collide (see Surprises). Keeping the Rust name verbatim after a separator makes collisions impossible, needs no case conversion, and lets a Go name be read back to its Rust path. Runtime packages are a small vocabulary that is looked up in the package, so they can read like ordinary Go.
  Date/Author: 2026-11-20, Claude.
- Decision: A Go function takes `ctx` if and only if the Rust function is `async`. Functions that are synchronous in Rust but need the runtime (`tokio::spawn`, `Handle::current`) find it through a table of the runtime's goroutines.
  Rationale: Rust reaches the runtime through a thread-local from synchronous code, including from `Drop`. Threading `ctx` into those functions would change signatures the translation cannot derive locally. The rule as stated is checkable from the Rust signature alone.
  Date/Author: 2026-11-20, Claude.
- Decision: Declarations (types, tags, registrations, signatures) are generated from the parsed Rust source; bodies and tests are translated by hand.
  Rationale: About 1,000 serialized types carry 2,100 serde attributes. A retyped attribute or a missing variant compiles and only shows up as a wire incompatibility. The generator also makes every signature of a crate exist before any body is written, which is what lets files inside a module cycle be ported in parallel.
  Date/Author: 2026-11-20, Claude.
- Decision: The Go programs build into `target/go/<profile>/`, and a Go build run from there refuses to own the default instance, as a Rust build run from a Cargo target directory does.
  Rationale: AGENTS.md forbids pointing a test build at the default instance. `target*/` is already ignored by Git.
  Date/Author: 2026-11-20, Claude.

## Outcomes & Retrospective

Nothing to report yet. This section is updated at the end of each milestone.

## Context and Orientation

The Rust workspace is described in `CONTRIBUTING.md` under "Crate Layout". In short: `mj-core` is the foundation every other crate uses. `mj-worker` (with the program `mj-worker` in `mj-worker-bin`) runs beside an agent on the machine or container where a session works. The daemon (`mj daemon-run`: `mj-daemon` over `mj-controller`, `mj-store`, `mj-server` and the session crates) owns the database and talks to each worker over the relay protocol defined in `mj-core/src/relay`. The terminal dashboard (`mj-tui`, `mj-chat`), the web viewer (served by `mj-server`) and the one-shot commands (`mj-cli`, which builds the program `mj`) are clients of the daemon's HTTP API.

Terms used in this plan:

A "crate" is a Rust library or program; each is a top-level directory with a `Cargo.toml`. A "module" is a Rust source file or an inline block inside one, addressed as `crate::a::b`. A "runtime package" is a Go package under `go/rs` or `go/crates` that stands in for part of Rust's standard library or for a third-party crate. A "product package" is a Go package under `go/mj*` or `go/cmd` that is the port of one of Mjolnir's own crates. The "oracle" is a small Rust program under `go/port/oracle` that runs the real Rust code on inputs a Go test supplies, so a Go test can compare its result with Rust's. A "golden" is a checked-in file holding the exact expected output of a test, under `<crate>/tests/golden/`. An "instance" is a named, separate set of Mjolnir configuration and data directories, selected with `--instance <name>`; test builds must use one and never the default.

The baseline of the port is commit `63c80bc5ae2088957bb7ab488db90d08a1c73fc5`. The Rust code keeps changing on `master` while the port proceeds; Milestone 8 describes how changes are carried over.

Files that already exist for the port:

`go/port/rsindex/` is a standalone Rust program (its `Cargo.toml` has an empty `[workspace]` table, so it is not part of the product workspace) that parses Rust source files with the `syn` crate and prints, for each file, its module path, items with line spans, imports, referenced paths and tests.

`go/port/portmap.py` runs `rsindex` and resolves references. `python3 go/port/portmap.py stats` prints one line per crate in dependency order with its Go directory, line counts and test count. `schedule [crate]` prints groups of files that must be ported together. `file <path.rs>` prints one file's Go file name, the Go name of each item and each test, and the files it needs. `names <crate>` and `tests <crate>` print the whole name tables.

`.agents/docs/go-port/translation-rules.md` is the rulebook every porter follows: layout, the naming rule, the type table, ownership and `Drop`, errors, async code, formatting, serialization, conditional compilation, tests, and what a hand-back report contains.

`.agents/docs/go-port/runtime-packages.md` lists every runtime package: which Rust library it stands in for, whether it is a port or a wrapper, and what must match.

## Plan of Work

The work is nine milestones. Each ends with something that can be run and checked. Within a milestone the units of work are independent enough to be done in parallel by separate agents, each given the rulebook, the files it owns, and the checks it must pass.

### Milestone 0: skeleton and instruments

Create `go.mod` at the repository root (module `github.com/BrokkAi/mjolnir`, Go 1.27). Create the root package file `assets.go` that embeds every file the Rust code includes with `include_str!` or `include_bytes!`, and `go/assets`, which serves them by repository path; a test compares the embedded list with the paths `portmap.py` finds in the Rust sources. Create `go/port/oracle` with one empty module for each runtime package and the Go helper `go/rs/rstest/oracle`. Write `go/port/status.py`, which reads the `// Port of` and `// rust-blob:` headers of every Go file and prints, for each crate: Rust files ported and not, files whose Rust blob has changed since the port, `rs.Todo` calls left, and Rust tests with no Go test of the mapped name. Write `go/port/gotest.sh` (runs `go test` with the feature tags and the environment the rulebook requires) and `scripts/go/build.sh`. Add `.github/workflows/go.yml`, which on changes under `go/`, `go.mod` or `go.sum` checks formatting, vets, builds for linux, darwin and windows on amd64 and arm64 from a Linux runner, and runs the tests on Linux. Add a `declarations` command to `portmap.py` that writes the Go type declarations, tags and serde registrations for a Rust file.

Acceptance: `go build ./go/...` succeeds; `python3 go/port/status.py` prints a table showing 0 of 1,004 files ported; the workflow passes on the push.

### Milestone 1: runtime packages

Build the packages in `.agents/docs/go-port/runtime-packages.md`. The order inside the milestone: `go/rs` (Option, formatting, structural clone and equality, panics) first, because everything uses it; then `strs`, `collections`, `rspath`, `rsfs`, `rsio`, `rssync`, `rstime`, `rsthread`, `rsprocess`, `rsenv`, `rsnet`, `rstest`; `serde`, `serdejson`, `toml`, `anyhow`, `tracing`; `tokio`, `tokioutil`, `futures`; then the rest, which are independent of each other. The terminal stack (`ratatui`, `crossterm`, `vt100`, ...) and `clap` are needed only from Milestone 6 and 7 and proceed in the background.

Each package follows the "Rules for every runtime package" in the catalogue: the crate's own tests ported, a differential test against the oracle with a recorded corpus, and a `doc.go` saying what is covered.

Acceptance: `go/port/gotest.sh ./go/rs/... ./go/crates/...` passes, and with `MJ_ORACLE=live` the same tests pass against the real crates.

### Milestone 2: foundation crates

Port `mj-core` (138 files, 65,329 production lines, 684 tests), then `mj-build-id`, `mj-review`, `mj-transcript` and `mj-checkpoint` (together 46 files, 23,021 production lines, 321 tests). For each crate: first the declarations pass (every type, constant and function signature; bodies are `rs.Todo`), which must compile; then bodies and tests file by file, in the order `portmap.py schedule` gives where it gives one.

Add an oracle module for each crate that exposes its pure functions (encode and decode of every relay message, snapshot digests, transcript projection, diff, configuration loading) and differential tests over recorded and generated inputs.

Acceptance: every Rust test of these crates has a passing Go test of the mapped name (`status.py` shows no missing tests and no `rs.Todo`), including the golden tests reading the Rust golden files in place.

### Milestone 3: the Go worker

Port `mj-worker` (116 files, 46,749 production lines, 892 tests) and `mj-worker-bin`. Build `target/go/release/mj-worker`.

Acceptance: the Go tests pass, and the first side-by-side proof: a Rust `mj` in an isolated instance, with `MJ_WORKER_BINARY` pointing at the Go worker, creates a local session, runs a turn with a recorded harness, checkpoints, restarts its daemon and reattaches. `go/parity/worker.sh` runs this and compares the relay journal the Go worker writes with the one the Rust worker writes for the same recorded turn.

### Milestone 4: client, targets, store and session crates

Port, in dependency order: `mj-client`, `mj-github-relevance`, `mj-targets`, `mj-session-types`, `mj-store`, `mj-accounts`, `mj-import`, `mj-worker-staging`, `mj-build-cache`, `mj-profile-catalog`, `mj-sessionwiki`, `mj-worker-launch`, `mj-session-runtime`, `mj-review-host`.

Acceptance: their tests pass in Go, and a store parity test: a database created and migrated by the Rust code is opened, read and written by the Go code and then read back by Rust with the same rows, and the reverse.

### Milestone 5: controller, server, daemon

Port `mj-controller`, `mj-setup`, `mj-server`, `mj-daemon`.

Acceptance: their tests pass in Go; the Go daemon serves the Rust `mj` client and the web viewer in an isolated instance; `go/parity/api.sh` sends the same requests to a Rust daemon and a Go daemon over equal stores and compares every response body; a handoff between a Rust daemon and a Go daemon of the same store completes in both directions with a session running.

### Milestone 6: terminal stack, chat and dashboard

Finish the terminal runtime packages, then port `mj-chat` and `mj-tui`.

Acceptance: their tests pass, including the 83 golden screen files under `mj-chat/tests/golden` and `mj-tui/tests/golden` read in place.

### Milestone 7: the `mj` program

Port `mj-cli` into `go/cmd/mj` and `voice-worker` into `go/cmd/mj-voice-worker`.

Acceptance: its tests pass; `mj --help` and every subcommand's help are identical in both programs; the scripts under `tests/e2e` that take an `mj` binary pass when given the Go one; `go/parity/run.sh` runs the Rust and Go programs through the same scripted dashboard session in two instances and compares screens and store contents.

### Milestone 8: packaging, platforms, staying current

Give the Go build what the Rust build has for release: version and build identity, static Linux worker for both architectures, macOS and Windows builds, a licence report for Go dependencies, and install paths that do not collide with the Rust programs. Run the Go tests on macOS and Windows in the existing nightly lanes without adding parallel macOS jobs.

Staying current: `python3 go/port/status.py --drift` lists every Rust file whose blob differs from the one its Go file names. For each, read `git diff <old blob> <new blob>`, apply the same change to the Go file and its tests, and update the header. New Rust files appear as "not ported". This is done at the end of every milestone and then continuously.

Acceptance: `status.py` reports every Rust file ported at its current blob, no `rs.Todo`, no missing tests; the Go lanes pass on all three platforms.

## Concrete Steps

All commands run from the repository root.

To see the size and order of the work:

    python3 go/port/portmap.py stats
    python3 go/port/portmap.py schedule mj-core

To see what one Rust file becomes:

    python3 go/port/portmap.py file mj-core/src/runtime.rs

To fetch the Rust sources the runtime packages are checked against (standard library and all crates):

    go/port/fetch-reference.sh

To build and test the Go code (once Milestone 0 is done):

    go build ./go/...
    go/port/gotest.sh ./go/...
    python3 go/port/status.py

When testing a built Go program, isolate it exactly as a Rust development build must be. Inside a Mjolnir session the environment carries the default instance's directories, which override `--instance`, so clear them:

    env -u MJ_CONFIG_DIR -u MJ_DATA_DIR -u MJ_SESSION_ID -u MJ_SESSION_BASH_ENV \
        target/go/release/mj --instance go-port-test daemon status

and stop the instance's daemon when done (`mj --instance go-port-test daemon stop`).

How work is handed out. A unit is either one runtime package or a set of Rust files of one crate. The person or agent doing it is given: this plan, the rulebook, the catalogue, the exact files, and the command that must pass. They write only those files, do not commit, and hand back the report the rulebook describes. The coordinator runs the checks again, reads the diff against the Rust source for the kinds of mistake the rulebook warns about (missing drops, reordered arms, changed text, values copied where Rust borrows), commits, and pushes. A second reader who did not write the port compares each product file with its Rust file before it is counted as done.

## Validation and Acceptance

The port is accepted when all of the following hold, and each can be checked by running the command named.

Every Rust test has a Go test of the mapped name and it passes: `go/port/gotest.sh ./go/...` passes and `python3 go/port/status.py` shows zero missing tests, zero `rs.Todo`, zero unported files, zero drifted files.

The Go tests read the Rust goldens and fixtures in place and never rewrite them.

The runtime packages agree with the real crates: `MJ_ORACLE=live go/port/gotest.sh ./go/rs/... ./go/crates/...`.

The two implementations interoperate: `go/parity/run.sh` runs the worker, store, API, handoff and dashboard comparisons described in Milestones 3, 4, 5 and 7, in isolated instances, and reports no difference.

The end-to-end scripts under `tests/e2e` pass against the Go `mj`.

## Idempotence and Recovery

Every tool here only reads the Rust sources and prints; rerunning is safe. Porting a file again overwrites one Go file and its test file. A half-ported file is visible as `rs.Todo` calls and missing tests in `status.py`, so work can stop and resume at any file boundary from this plan and that output alone. Nothing in this plan changes the Rust workspace, its lockfile or its build: `go/port/rsindex` and `go/port/oracle` are separate Cargo workspaces, and the Go module adds only `go.mod`, `go.sum`, one root `.go` file and the `go/` tree. Removing those returns the repository to its previous state.

Parity runs create instances named `go-parity-*` and remove them and stop their daemons on exit; if a run is interrupted, `mj --instance <name> daemon stop` and deleting `~/.config/mjolnir/instances/<name>` and its data directory cleans up.

## Artifacts and Notes

The size of the work, from `python3 go/port/portmap.py stats` at the baseline:

    crate                  go package                 files    prod    test  tests
    mj-core                go/mjcore                    138   65329   26351    684
    mj-build-id            go/mjbuildid                   2     253       0      0
    mj-checkpoint          go/mjcheckpoint               21   13026   10004    181
    mj-review              go/mjreview                    6    1067     807     30
    mj-transcript          go/mjtranscript               17    8675    6565    110
    mj-client              go/mjclient                   33   13435    3091     65
    mj-github-relevance    go/mjgithubrelevance          11    2192    1046     10
    mj-worker              go/mjworker                  116   46749   60675    892
    mj-chat                go/mjchat                     54   30283   22286    415
    mj-targets             go/mjtargets                  31   10227    8421    176
    mj-worker-bin          go/cmd/mj-worker               9     849    3251     37
    mj-session-types       go/mjsessiontypes              1     117       0      0
    mj-store               go/mjstore                    45   25927   20674    306
    mj-tui                 go/mjtui                     106   54468   41598    612
    mj-accounts            go/mjaccounts                 22    9077    6920    137
    mj-import              go/mjimport                   25    8398    6138    133
    mj-worker-staging      go/mjworkerstaging            21    7856    9352    125
    mj-build-cache         go/mjbuildcache                8    4025    4689     87
    mj-profile-catalog     go/mjprofilecatalog            3    2806    3702     67
    mj-sessionwiki         go/mjsessionwiki              10    4123    3549     55
    mj-worker-launch       go/mjworkerlaunch              4    1345     176      3
    mj-session-runtime     go/mjsessionruntime           38   16321   13830    220
    mj-controller          go/mjcontroller               62   32393   36457    430
    mj-review-host         go/mjreviewhost               18    4753    5912     84
    mj-server              go/mjserver                   36   13709   15554    274
    mj-setup               go/mjsetup                    13    6225    4406     84
    mj-daemon              go/mjdaemon                   94   43426   50062    599
    mj-cli                 go/cmd/mj                     58   31256   19968    325
    voice-worker           go/cmd/mj-voice-worker         2     775      73      3
    TOTAL                                              1004  459085  385557   6144

## Interfaces and Dependencies

The Go toolchain is the one `mise.toml` pins (Go 1.27). The module path is `github.com/BrokkAi/mjolnir`.

Go modules the port depends on, all pure Go: `github.com/BrokkAi/acp-go` (Agent Client Protocol and `jsonvalue`), `golang.org/x/sys`, `golang.org/x/term`, `golang.org/x/text`, `golang.org/x/crypto`, `modernc.org/sqlite` (SQLite), `github.com/klauspost/compress` (zstd), a BLAKE3 implementation, a WebAssembly runtime for tree-sitter, and a library that loads shared objects without C for ONNX Runtime. Each is recorded in the catalogue when it is added.

The interfaces every product package relies on are fixed by the rulebook and must exist at the end of Milestone 1: `rs.Option[T]`, `rs.Format`, `rs.Clone`, `rs.Eq`, `rs.Cmp`, `rs.Todo`, `rs.RegisterStruct` and `rs.RegisterEnum` in `go/rs`; `collections.BTreeMap[K, V]` and its siblings; `rspath`, `rsfs`, `rsio`, `rssync`, `rstime`, `rsthread`, `rsprocess`; `rstest.AssertEq` and the oracle helper; `serde` and `serdejson`; `anyhow`; `tracing`; and in `go/crates/tokio`: `tokio.Fn[T]`, `tokio.Spawn`, `tokio.Select` with `Arm`, `ArmIf`, `ArmMatch`, `ArmFn`, `ArmElse`, `tokio.Start`, `tokio.Test`, the `Sync*` channel types and the `Time*` functions.

Change note (2026-11-20): initial version, written after the survey and the first two port tools.

Change note (2026-11-20, later): Progress records Milestone 0 and the start of Milestone 1. The naming rule changed from joined PascalCase to `<ModulePrefix>_<rust name>` after generated declarations were type-checked; the Decision Log and Surprises say why. The rule for which functions take `ctx` and the decision to generate declarations were added.
