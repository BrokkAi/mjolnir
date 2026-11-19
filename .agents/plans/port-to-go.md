# Port Mjolnir from Rust to Go, one process at a time

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md`.

This is the program-level plan. It fixes the order of the port, the rules every phase obeys, and the proof each phase owes before the next begins. Phases 0 and 1 are specified here in enough detail to start. Each later phase gets its own ExecPlan in `.agents/plans/` when it starts (`port-to-go-worker.md`, `port-to-go-daemon.md`, `port-to-go-clients.md`, `port-to-go-retire-rust.md`), and each of those incorporates this file by reference.

## Purpose / Big Picture

Mjolnir is about 460,000 lines of production Rust and 380,000 lines of Rust tests in 29 crates. Building it has become the main cost of working on it. `.agents/docs/build-performance-analysis.md`, measured on 2026-11-19 on a 4-vCPU machine, records a cold `cargo build` of 5 minutes 7 seconds, a further 6 minutes 38 seconds for `cargo test --no-run`, 70 to 110 seconds every time `cargo test -p` moves from one crate to another, a 32 GB `target/` directory, and one test unit (`mj-daemon`) that took 234 seconds on its own. `.agents/plans/fan-out-crates-under-size-budget.md` split the workspace from twelve crates into twenty-nine in two days. Its first split moved the whole test build from 114 seconds to 118, and after every spawned task was boxed the test build on three cores still takes about 300 seconds with a 6.5 GB peak. The causes are structural to how Rust compiles this kind of program: async state machines and tokio task machinery are about 40 percent of the generated code, serde about 22 percent, and Cargo feature sets multiply every build.

Ryan Svihla decided on 2026-11-19 to move the project to Go. Go compiles each package separately, caches each package's result, has no feature sets, and does not generate a copy of the runtime for each spawned task, so the build problem above does not exist there in the same form.

After this work, nothing is different for a person who uses `mj`. The same commands exist, the same sessions keep running across the change, the same store file is read, and upgrades complete without user intervention as they do today. What is different is for a contributor: the whole program builds with `go build ./cmd/... ./internal/...` and is tested with `go test -race ./cmd/... ./internal/...`, and the repository no longer contains a Cargo workspace. Each phase below ends in something a person can run and observe, and the plan as a whole is done when the last Rust crate that `mj` depends on is deleted and the repository's end-to-end scripts under `tests/e2e/` still pass.

The plan replaces one operating-system process at a time, in this order: the worker, then the daemon, then the terminal clients. It never links Rust and Go into one executable. Each replacement is delivered to users through an upgrade path that already exists and is already tested, so from the running system's point of view a Go build is just the next release.

## Progress

- [x] (2026-11-19) Measured the codebase: lines per crate split into production and test, the crate dependency graph, the three executables, the test corpus, and the commit rate.
- [x] (2026-11-19) Inventoried every boundary where two processes or two releases must agree on bytes (see Context and Orientation).
- [x] (2026-11-19) Inventoried the parts that lean on Rust-specific or native facilities: `unsafe`, operating-system gates, the concurrency model, the terminal stack, and third-party crates with no direct Go equivalent.
- [x] (2026-11-19) Confirmed from the Go module proxy that the candidate libraries named in Interfaces and Dependencies exist and are maintained.
- [x] (2026-11-19) Compared the seven Go libraries for the Agent Client Protocol listed on the protocol's community page by reading their released source. Ryan Svihla chose `github.com/BrokkAi/acp-go`; the comparison is in the Decision Log and in Artifacts and Notes.
- [ ] Ryan Svihla confirms or changes the five decisions marked "Proposed" in the Decision Log.
- [ ] Phase 0: the seven feasibility prototypes, each promoted or discarded with evidence.
- [ ] Phase 1: the Go module, its lint and CI lane, the contract corpus, the mixed-pair conformance lanes, and the foundation packages.
- [ ] Phase 2: the Go worker ships, soaks, becomes the only worker, and the Rust worker crates are deleted.
- [ ] Phase 3: the Go daemon and control-plane commands ship; Go `mj` becomes the installed front; the Rust daemon crates are deleted.
- [ ] Phase 4: the Go command-line and terminal clients ship; the legacy Rust client binary is deleted.
- [ ] Phase 5: the Cargo workspace, its CI, and its registry publication are retired; `AGENTS.md` and `RELEASING.md` describe a Go project.

## Surprises & Discoveries

- Observation: there are seven separate byte-level boundaries, and none is described by a schema. All are serde-derived JSON over hand-made framing.
  Evidence: no OpenAPI, protobuf, or schema generator anywhere in the tree; the types are `mj-core/src/relay/protocol.rs`, `mj-core/src/relay/snapshot.rs`, `mj-client/src/daemon.rs`, `mj-server/src/server/api/types.rs`, `mj-core/src/archive.rs`, `mj-core/src/config.rs`.

- Observation: relay event digests are SHA-256 over the exact bytes `serde_json` produces for private Rust structs, so a Go worker must reproduce `serde_json` output byte for byte, including float formatting.
  Evidence: `mj-core/src/relay/snapshot/digest.rs:13-35`: "Its exact field order and serde attributes are load-bearing — changing them would invalidate every stored v1 digest." The digest-covered types reach `f32` and `f64` fields in `mj-core/src/assessment.rs`.

- Observation: `serde_json` in this build keeps object members in insertion order and reads numbers with exact rounding, although no manifest in the workspace asks for the first.
  Evidence: `Cargo.lock` lists `indexmap` as a dependency of `serde_json` 1.0.150, which only the `preserve_order` feature pulls in, so another crate's feature reached it through Cargo's feature unification. `mj-transcript/Cargo.toml:30` enables `float_roundtrip`, which unification applies to every crate.

- Observation: the hard part of reproducing `serde_json` in Go is already written, in Brokk's own protocol library.
  Evidence: `github.com/BrokkAi/acp-go` v0.13.0 has `internal/serdejson` ("Encode is serde_json::to_string", with a float formatter) and `internal/serdewire`, tested against frames produced by Rust in `testdata/serdeframe/rust_frame.jsonl`. Two gaps for Mjolnir: the packages are `internal`, so another module cannot import them, and the float formatter takes only `float64` while the digests cover an `f32` (`mj-core/src/assessment.rs:75`).

- Observation: there are two daemon interfaces, and the larger one is not the HTTP API. The local control protocol used by the terminal clients has about 135 request variants and 95 reply variants and requires an exact version match.
  Evidence: `DaemonAction` at `mj-client/src/daemon.rs:759`, `DaemonReply` at `:1384`, `PROTOCOL_VERSION` at `:3801` (currently 108).

- Observation: a worker serves exactly one relay protocol version and the daemon converts down to it. A Go worker therefore implements only the current version, not the history.
  Evidence: `relay_protocol_rejection` in `mj-core/src/relay/protocol.rs:689`: "a relay serves only `RELAY_PROTOCOL_VERSION`".

- Observation: replacing the daemon passes no in-memory state and no file descriptors. Continuity comes from the store and from workers that keep running.
  Evidence: `mj-cli/src/daemon.rs:1622-1720` sends `DaemonAction::PrepareUpgrade`, waits for the admission gate in `mj-store/src/upgrade.rs` to drain, waits for the store lock, and starts the new daemon. This is why a Go daemon can take over from a Rust one as an ordinary upgrade.

- Observation: the test corpus contains 2,064 tests marked `// Hard-won:`, each pinning a defect found after the code was believed done.
  Evidence: `grep -rn 'Hard-won:'` over `*.rs`, `*.py`, `*.sh`, `*.mjs`. By area: worker side 384, daemon side 912, terminal clients 502, `mj-core` 175, language-neutral scripts 81.

- Observation: Python and JavaScript already speak three of the boundaries, which proves they are language-neutral in practice.
  Evidence: `tests/e2e/delegation.py:354-370` and `tests/e2e/mailbox_harnesses.py:96-180` speak the relay protocol to a worker socket; `tests/e2e/daemon_endpoint.py:50-65` speaks the control protocol; `mj-server/src/web/viewer.js` consumes the HTTP API and its event stream. Nothing outside Rust recomputes relay digests, reads checkpoint archives, or reads the worker journal; those have the least existing proof.

- Observation: the repository changes very fast, so a port that works from a snapshot is out of date before it compiles.
  Evidence: the 450 commits visible in this checkout span 2026-11-17 to 2026-11-19, 390 of them on 2026-11-18, changing 1,158 files with 92,973 insertions and 35,013 deletions.

- Observation: `.gitignore` would hide a Go package named `targets`.
  Evidence: `git check-ignore -v internal/targets/x.go` prints `.gitignore:4:target*/`. The existing exceptions cover only `**/src/**/targets/`.

- Observation: several native-library dependencies cannot be reproduced in Go without cgo (calling C from Go), which would give back the build speed and cross-compilation this port is for.
  Evidence: `tree-sitter-bash` in `mj-worker/Cargo.toml:23-24` and `mj-transcript/Cargo.toml:33-34`; `ort` (ONNX Runtime) and `tokenizers` in `mj-github-relevance/Cargo.toml`; `cpal` in `voice-worker/Cargo.toml`; `arboard` in `mj-chat`.

## Decision Log

- Decision: port by operating-system process, never by crate, and never link Rust and Go into one executable.
  Rationale: a boundary between processes carries bytes that can be recorded, replayed, and compared. A boundary between crates inside one process can only be crossed from Go through cgo, which puts two schedulers and two signal-handling models in one process and removes Go's fast builds and simple cross-compilation. The repository already has three executables with versioned protocols between them.
  Date/Author: 2026-11-19 / Claude, for Ryan Svihla's review.

- Decision: the Agent Client Protocol library is `github.com/BrokkAi/acp-go`.
  Rationale: Ryan Svihla asked for it and asked whether it is the best of the listed Go libraries. Judged by reading the released source of all seven Go libraries on the protocol's community page, it is the best fit for this port, for reasons that can be checked. It pins the schema artifacts of Rust schema crate 1.15.1 and states that it follows the behaviour of Rust `agent-client-protocol` 5.0.0; those are the versions in this repository's `Cargo.lock`, so a Go worker speaks the same protocol revision as the Rust worker it replaces. It has no dependency outside the standard library. It is the only one of the seven with an encoder written to match `serde_json` byte for byte; the others compare JSON structurally. It carries the unstable features the worker uses (system prompt, end-of-turn usage, session fork), raw `Call` and `Notify` for vendor methods, an agent runtime for `mj acp`, draft protocol v2 in separate packages, and the largest test suite of the seven. It is Apache-2.0, which this GPL-3.0-only project can use. It is behind in two respects. It is not the newest: `coder/acp-go-sdk` pins v1 schema 1.32.0 and `caelis-labs/acp-go-sdk` pins v1 1.33.0 and v2 alpha.17, against 1.29.1 and alpha.13 here. And `caelis-labs/acp-go-sdk` runs an interoperability matrix of 89 cases against the official Rust and TypeScript libraries as live peers, where `acp-go` tests against frames recorded from Rust and optionally against real agents. The first matters only when Mjolnir wants protocol features newer than the Rust library it uses today, and moving the pin is then a change in a library Brokk owns. The comparison read source and ran none of the libraries against a harness; the fourth prototype in Phase 0 does that.
  Date/Author: 2026-11-19 / Ryan Svihla (choice), Claude (comparison).

- Decision (Proposed): the order is worker, then daemon, then terminal clients.
  Rationale: the worker is the smallest piece (about 47,000 lines of its own plus the checkpoint, transcript and review libraries), it is Unix-only, it implements a single protocol version, and replacing it uses the existing idle worker upgrade. It also exercises the hardest Go patterns (process groups, pipe draining, a durable journal, the Agent Client Protocol) at a quarter of the daemon's size. The daemon is the largest piece and the worst build unit, so it comes as soon as those patterns are proven. The terminal interface comes last because it is the least mechanical to translate and has the cleanest boundary, so the Rust version can run against a Go daemon for as long as needed.
  Date/Author: 2026-11-19 / Claude, awaiting Ryan Svihla.

- Decision (Proposed): while a component is being ported, changes to the already-ported part of it are made in both languages in the same commit, enforced by CI. Feature work is not frozen.
  Rationale: at the observed commit rate a freeze of the daemon for the length of Phase 3 would stop the product. A ledger file lists the Rust paths that have a Go counterpart; a commit that touches a listed Rust path without touching its Go counterpart fails CI unless it carries a `Port-Exempt:` trailer with a reason. The cost is that work on ported layers is done twice until that process is swapped, which is the reason to keep each phase short and to port in dependency order. The alternative, a freeze per component, is cheaper per change and is the right choice if Jonathan Ellis and Ryan Svihla prefer to stop feature work on one component at a time.
  Date/Author: 2026-11-19 / Claude, awaiting Ryan Svihla.

- Decision (Proposed): Go executables are built with cgo disabled. Anything that needs a native library runs as a separate small process.
  Rationale: with cgo off, `GOOS=linux GOARCH=arm64 go build` on any machine produces the static worker that today needs a musl toolchain and its own CI runner. Three places need native code today. The bash parser (`tree-sitter-bash`) has a pure-Go replacement to be proven in Phase 0. The voice capture helper (`voice-worker/`, 776 lines, no workspace dependencies, already a separate process) stays in Rust. The local GitHub relevance model (`mj-github-relevance`, ONNX Runtime and HuggingFace tokenizers) either stays in Rust as a separate helper process or is dropped in favour of the hosted classifier it already has a client for.
  Date/Author: 2026-11-19 / Claude, awaiting Ryan Svihla.

- Decision (Proposed): the two Brokk-owned Rust libraries the daemon uses, `brokk-anvil-client` and `brokk-sessionwiki`, need an owner's answer before Phase 3: port the used surface to Go in their own repositories, or keep them in Rust behind a helper process.
  Rationale: their source is not in this repository, so this plan could not size them. Mjolnir uses about 20 items of the first (multi-provider language-model clients, structured inference, transcription) and about 50 of the second (parsers for each harness's native session files, a SQLite search index, git blame attribution). The second owns an on-disk index that other tools may read.
  Date/Author: 2026-11-19 / Claude, awaiting Ryan Svihla.

- Decision (Proposed): `cargo install brokk-mjolnir` stops being an installation channel when the last Rust code is removed. The final crates.io release says so and points to the installer script, npm, and Homebrew.
  Rationale: crates.io cannot distribute a Go program. The npm package is already a launcher that downloads a platform binary, and the Homebrew formula updates on its own.
  Date/Author: 2026-11-19 / Claude, awaiting Ryan Svihla.

- Decision: tests are carried over by kind, not by count. Golden files and the `tests/e2e/` scripts are reused unchanged. Every test marked `// Hard-won:` is either ported, with its marker, or retired with a one-line reason recorded in the phase's plan. Ordinary Rust unit tests are not translated.
  Rationale: `AGENTS.md` already says which tests are worth having (golden, end-to-end, integration, tricky condition) and forbids feature-example unit tests. The hard-won tests are the recorded knowledge of what breaks; the others are not.
  Date/Author: 2026-11-19 / Claude.

- Decision: during the hybrid period there is never an automatic fallback from a Go implementation to the Rust one.
  Rationale: `AGENTS.md` says a narrow fallback hides the primary design not working. Which implementation runs a role is an explicit setting for the soak period and a failure of the Go implementation is reported, not retried in Rust.
  Date/Author: 2026-11-19 / Claude.

## Outcomes & Retrospective

Nothing has been built yet. This section is filled at the end of each phase with what was delivered, the measured build and test times against the Rust numbers in Purpose, and what the next phase should do differently.

## Context and Orientation

This section describes the system as it is on 2026-11-19 at version 2.72.0. Read it before any phase plan.

### What Mjolnir is, in the terms this plan uses

Mjolnir runs coding agents in sessions. A "harness" is a third-party coding agent program such as Claude Code, Codex, Kimi, Grok, or OpenCode. A "session" is one harness working in one copy of a repository on one "target", which is where the work runs: the local machine, a local Podman or Docker container, an Apple container, an EC2 instance, or any of those reached over SSH.

There are three executables.

`mj` is the program users run. It is built from `mj-cli/` and contains three roles in one binary: the command-line commands, the terminal user interface (the "dashboard", in `mj-tui/` and `mj-chat/`), and the "daemon", which is the same binary started as `mj daemon-run`. The daemon is the long-running background process that owns the database, the session records, and the HTTP server. `AGENTS.md` calls it the control plane.

`mj-worker` is built from `mj-worker-bin/` and `mj-worker/`. One worker process runs per session, on the session's target. The daemon copies the worker binary to the target under the name `hel` and starts it there (`mj-worker-staging/src/worker_binary/staging.rs:147`). The worker runs the harness, owns the turn in progress, and keeps a durable record called the journal. It keeps running when no daemon is connected. `AGENTS.md` calls it the data plane. Workers run only on Unix.

`mj-voice-worker` is built from `voice-worker/`. It captures microphone audio and is started by the terminal interface as a child process. It shares no code with the rest of the workspace.

An "instance" is an isolated set of configuration and data directories selected with `mj --instance <name>`. All testing of new builds uses a named instance, never the default one; a binary run from a build directory refuses to touch the default instance.

### Size

Production lines exclude code under `#[cfg(test)]` and test files. The count comes from a script that treats everything from a file's trailing test module onward as test code, so it is approximate.

The worker's own crates (`mj-worker`, `mj-worker-bin`) are about 47,000 production lines. Three libraries used by both the worker and the daemon (`mj-checkpoint`, `mj-transcript`, `mj-review`) are about 22,000. `mj-core`, which every binary uses, is about 58,000. The daemon's seventeen crates (`mj-daemon`, `mj-controller`, `mj-server`, `mj-store`, `mj-session-runtime`, `mj-accounts`, `mj-import`, `mj-worker-staging`, `mj-worker-launch`, `mj-setup`, `mj-build-cache`, `mj-review-host`, `mj-sessionwiki`, `mj-profile-catalog`, `mj-targets`, `mj-github-relevance`, and the tiny `mj-session-types`) are about 201,000. The clients (`mj-cli`, `mj-tui`, `mj-chat`, `mj-client`) are about 132,000. That is about 460,000 in total, with another 380,000 lines of tests in 6,132 test functions.

The allowed dependencies between crates are written down in `scripts/check-crate-boundaries.mjs`. That file is the layering the Go packages must reproduce.

### The seven boundaries where bytes must agree

A port has to preserve each of these exactly, because the other side of each one is either a different process or data written by an earlier release.

The first is the relay protocol between the daemon and a worker. The daemon starts `<root>/hel worker proxy --root <root>` on the target, wrapped in `ssh`, `podman exec`, `docker exec`, or nothing, and that proxy connects its standard input and output to the worker's Unix socket `<root>/control.sock`. Messages are one JSON object per line, at most 8 MiB each (`mj-core/src/bounded_frame.rs`). The types are in `mj-core/src/relay/protocol.rs` (about 45 request methods and 44 response payloads) and `mj-core/src/relay/snapshot.rs` (about 35 commands and 39 observations). The current version is 72 (`RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs`). The two sides exchange a `Hello` and the daemon then speaks the worker's version; a worker rejects any other.

The second is what the worker keeps on disk in its root directory: the journal (`relay-journal/active.jsonl`, sealed into gzip segments at 1 MiB, written by `mj-worker/src/relay/journal.rs`), the snapshot `relay-state.json` (format version 27), `launch.json`, and several small marker files. Each journal event carries a "digest": the SHA-256 of a fixed prefix followed by the exact JSON bytes of the event's content. The daemon stores those digests and checks them again (`validate_relay_event_digest`). A worker upgraded in place reads the files its predecessor wrote.

The third is the daemon's local control protocol, used by the terminal interface and the command-line commands. It runs over a loopback TCP connection whose address and token are in `<data_dir>/daemon.json`. Each message is a 4-byte big-endian length followed by JSON. The types are in `mj-client/src/daemon.rs`. The version is 108 and the client requires an exact match, except for a small frozen subset (`Ping`, `Status`, `Stop`, `PrepareUpgrade`) that every release understands so that any client can ask any daemon to step aside.

The fourth is the HTTP API served by `mj-server`, used by `mj api` commands, `mj acp`, the browser viewer in `mj-server/src/web/`, and outside programs. It has 49 documented paths under `/api/v1` (`mj-server/src/server/api/routes.rs`), about 30 more for the viewer, and streams events as server-sent events. It is documented in prose in `docs/src/content/docs/api-reference.md`.

The fifth is the store, a SQLite database at `<data_dir>/mj.sqlite3`. Its schema revision is 140 (`SCHEMA_VERSION` in `mj-store/src/database.rs`), held in `PRAGMA user_version`. The migrations are a single sequence of 140 `if version < N` blocks in `mj-store/src/database/schema.rs` (6,793 lines) plus a handful of `.sql` files. A table records the lowest revision an older program may safely read and write. About 30 columns hold JSON text of Rust types, and about 75 queries reach into that JSON with `json_extract`, so the SQL depends on the JSON shapes. `AGENTS.md` requires that forward migrations for every shipped revision are kept forever.

The sixth is the checkpoint archive, a ZIP file with a `manifest.json` (schema versions 2 through 12, `mj-core/src/archive.rs`) containing git bundles, patches, tar files, and harness-native files, each with a SHA-256. There is also a small custom binary format for skills (`HELSKIL1` and `HELSKIL2`, `mj-core/src/skills/mod.rs`).

The seventh is the set of files users and other releases read and write: `config.toml` (version 23, edited in place so that comments survive, `mj-core/src/config/document.rs`), `secrets.toml`, the terminal upgrade file `<data_dir>/terminal-upgrades/<pid>.json` that carries unsent drafts from an old terminal interface to a new one (`UpgradeResume` in `mj-cli/src/dashboard/upgrade.rs`), and the credential files of the harnesses, whose formats Mjolnir does not own.

The worker also speaks two outside protocols: the Agent Client Protocol (ACP), which is JSON-RPC over the harness's standard input and output, through the `agent-client-protocol` crate, resolved in `Cargo.lock` to 5.0.0 with schema crate 1.15.1, with three unstable extensions; and the Model Context Protocol (MCP), for which the worker contains hand-written servers (`mj-worker/src/mcp_stdio.rs`, `memory_mcp.rs`, `subagent_mcp.rs`).

### The replacement mechanisms that already exist

Three mechanisms already replace one build of a process with another without losing work. The port uses each of them once to replace Rust with Go.

A worker is replaced in place when it is idle (`mj-daemon/src/worker_upgrade.rs`). The new binary is copied to the target and the worker restarts from its own files.

A daemon is replaced by a handoff. The newer program takes a start lock, sends `PrepareUpgrade`, the old daemon finishes its own short operations and exits, and the new daemon opens the same store and reattaches to the running workers.

A terminal interface is replaced by writing its unsent drafts and positions to the terminal upgrade file and then starting the new binary in its place (by `exec` on Unix, by spawning on Windows) with `MJ_UPGRADE_RESUME_FILE` set.

### What makes this port hard

The moving target is the first difficulty and is described under Surprises.

The second is the loss of checks the Rust compiler performs. The code has 829 enums and about 5,900 `match` expressions that the compiler proves complete, 137 types that release a resource when they go out of scope, and a guarantee that data shared between threads is locked. Go checks none of these at compile time. Interfaces and Dependencies says what replaces each check.

The third is the terminal interface: about 85,000 production lines drawing with the `ratatui` library in immediate mode, with custom text selection, mouse handling, an embedded terminal emulator for login screens, and 83 golden files that compare whole rendered screens.

The fourth is concurrency. The code has about 12,500 `.await` points, 266 `select!` expressions that hold most of the cancellation logic, about 690 spawned tasks and 420 blocking tasks. `AGENTS.md` asks for races to be made structurally impossible by giving each fact one owner. That rule transfers to Go directly, and Go's model (a goroutine that owns a state machine and receives messages on a channel) is a natural way to write it, but every cancellation decision has to be made again, because dropping a Rust future cancels it and nothing in Go does that implicitly.

## Plan of Work

### Rules every phase obeys

One language owns each process role in a release. A release never ships two daemons or two workers that could both be chosen silently.

One language writes the store. Until the Go daemon ships, only Rust migrates and writes `mj.sqlite3`. From that release on, only Go does. The Go daemon does not raise the store's minimum compatible revision until the Rust daemon crates have been deleted, so that a person can still put the last Rust daemon back by hand during the soak.

The bytes are the contract. Every boundary above gets a checked-in corpus of real examples, written by the Rust code and read by the Go code, in which every enum variant appears at least once. Go code is correct at a boundary when it decodes every example and encodes it back to the same bytes.

Go code is written as Go, not as transliterated Rust. Each state machine is owned by one goroutine. Cancellation travels in a `context.Context`. A resource acquired in a function is released with `defer` in that function. Background work is started through one supervisor helper that reports failures.

Every phase is tested in a named instance (`mj --instance <name>`), and the instance's daemon is stopped when the test is done, as `AGENTS.md` requires.

### Phase 0: prove the seven risks with prototypes

This phase is prototyping. Its code lives under `go/spikes/` in its own Go module so that it cannot be mistaken for the product, and it is deleted or promoted at the end. Each prototype has one question, a way to run it, and a rule for promoting or discarding the approach. Nothing a user runs changes.

The first prototype answers whether Go can reproduce relay event digests. A new Rust test in `mj-core/src/relay/snapshot/digest.rs` writes `contracts/relay/digest-cases.jsonl`: one line per case holding the event JSON and its expected digest, covering all 39 observation variants and the legacy shapes that file keeps. A second Rust test writes `contracts/floats.txt`: twenty thousand `f64` and `f32` bit patterns from a fixed seed, plus edge cases, each with the text `serde_json` prints for it. The Go prototype reproduces both files exactly. It starts from the `serde_json`-exact encoder that `github.com/BrokkAi/acp-go` already contains and does not write a new one. Three things have to change there first: `internal/serdejson` and `internal/serdewire` are exported from that module so Mjolnir can import them; the float formatter gains an `f32` form, because `serde_json` prints an `f32` with the shortest digits that round-trip as `f32` and the digests cover one; and its number reading is checked against Mjolnir's build, which has `float_roundtrip` on where that encoder models `serde_json` without it. The other known differences are listed under Interfaces and Dependencies. If any case cannot be reproduced, the plan stops here and is revised, because the worker cannot be ported without this.

The second prototype answers which SQLite driver the store can use without cgo. It copies the store of a throwaway instance and, with each of `modernc.org/sqlite` and `github.com/ncruces/go-sqlite3`, opens it in WAL mode, runs ten of the store's queries that use `json_extract`, registers a scalar function named `mj_changed_record` and attaches a TEMP trigger that calls it (as `mj-store/src/database/committed.rs` does), runs an `IMMEDIATE` transaction while a Rust reader holds the file open, and takes an online backup of a second database. The driver that passes everything is chosen; if both pass, the faster on the ten queries.

The third prototype answers how the terminal interface is drawn. It reproduces one existing golden file from `mj-chat/tests/golden/` byte for byte, using a small cell buffer and paragraph layout written to match `ratatui`'s semantics, once on top of `charm.land/bubbletea/v2` and once on top of `github.com/gdamore/tcell/v3`. It also reads a mouse drag and a key with keyboard enhancement flags from a real terminal under `tmux`. The base that needs less code to match the golden file and report those events is chosen.

The fourth prototype confirms that `github.com/BrokkAi/acp-go` drives the pinned harness bridges. It starts the Claude bridge pinned in `mj-core/src/harness_runtime.rs`, opens a session with a system prompt, sends one prompt, and reads the end-of-turn token usage, which are the unstable extensions the worker uses (`mj-worker/src/acp/instructions.rs:3`, `mj-worker/src/acp/session.rs:1100`), and it sends and receives one vendor extension method such as `_session/steering`. It repeats the turn against the Codex bridge. Anything missing is fixed in `acp-go`, which Brokk owns, and listed in Surprises.

The fifth prototype answers whether the shared subprocess rules in `AGENTS.md` can be met. It is a Go package that starts a child in its own process group, feeds it 1 MiB on standard input while draining 1 MiB from each of standard output and standard error, kills the whole group on cancellation, on timeout, and when the limit is exceeded, and starts a detached child that survives its parent with no inherited file descriptors. It runs on Linux and macOS. It is promoted when its tests, which use more than 64 KiB in each direction, pass under `go test -race` on both.

The sixth prototype answers whether bash commands can be classified without `tree-sitter`. The worker decides which shell commands a harness may run without asking by parsing them (`mj-worker/src/acp/permissions.rs:461`), which is a security boundary. The prototype parses the same commands with `mvdan.cc/sh/v3/syntax`, a bash parser written in Go, and must give the same verdict for every case in the existing Rust permission tests, exported to `contracts/permissions/bash-cases.jsonl`. Any case where the two parsers disagree about structure is listed in Surprises with the command.

The seventh prototype measures the thing the port is for. It translates `mj-review` (about 1,000 production lines, no dependencies but `mj-core`) with its tests, and records the wall time of a cold build, a build after touching one file, and a test run, next to the Rust figures. The numbers go into Outcomes.

### Phase 1: make the contracts executable and lay the foundation

At the end of this phase the repository builds a Go module in CI beside the Rust workspace, the boundaries are pinned by a corpus both languages are tested against, and the end-to-end scripts can be pointed at any combination of binaries. Nothing a user runs changes.

Create the Go module at the repository root: `go.mod` with module path `github.com/BrokkAi/mjolnir` and the Go version pinned in `mise.toml` (1.27), executables under `cmd/`, and everything else under `internal/`. The root is the final location, so nothing moves later. Every Go command names `./cmd/... ./internal/...` rather than `./...`, so that the tool does not walk Cargo's `target/` directory. In the same commit, add exceptions for `/cmd/**` and `/internal/**` to `.gitignore`: it ignores every directory whose name begins with `target` and every directory named `debug`, with exceptions only under `src/`, so the package that replaces `mj-targets` would otherwise be left out of commits.

Add `.golangci.yml` and a CI job in `.github/workflows/ci.yml` that runs `go build`, `go vet`, the linters named under Interfaces and Dependencies, and `go test -race`. Add `scripts/check-go-layers.mjs`, the counterpart of `scripts/check-crate-boundaries.mjs`, which reads `go list -deps -json` and fails when a package imports one above it.

Build the contract corpus under `contracts/`, one directory per boundary: `relay/`, `journal/`, `control/`, `http/`, `checkpoint/`, `config/`, `terminal-upgrade/`. Each is written by a Rust test in the crate that owns the types and follows the convention `mj-core/src/golden.rs` already uses: the test fails when the checked-in file differs from what the code produces, and `MJ_UPDATE_GOLDEN=1` rewrites it. Each exporting test contains a `match` over its enum with no wildcard arm that maps every variant to at least one example, so that adding a variant without an example does not compile. For the store, the corpus is a set of database files: one at each schema revision from the baseline (33) to the current one, produced by running the Rust migrations up to that revision on the seed data the existing migration tests use, plus the fully migrated result of each.

Make the conformance suite independent of how the binaries were built. Most scripts under `tests/e2e/` already take `--mj` and read `MJ_WORKER_BINARY`; finish that for the rest, and add `tests/e2e/conformance.toml`, which names the scripts that gate each phase. Add CI lanes that run the gate for a phase with each mixed pair it allows.

Port the foundation that every executable needs, each with its hard-won tests: the canonical JSON encoder and frame readers (`internal/wire`, from `mj-core/src/bounded_frame.rs` and the framing in `mj-client/src/daemon.rs`), the subprocess helpers (`internal/subprocess`, from `mj-core/src/subprocess.rs`, 2,020 lines), file locks (`internal/filelock`), confined and target paths (`internal/paths`), the build identity (`internal/buildid`), logging, and the crash-point test hooks driven by `MJ_TEST_HOOK` (`internal/testhooks`).

Extend `.github/workflows/release.yml` to build Go executables for Linux on amd64 and arm64, macOS on both architectures, and Windows on amd64, from one Linux runner, and to discard them. This proves the release pipeline before anything depends on it.

Start the port ledger, `contracts/port-ledger.toml`, which maps Rust source paths to their Go packages, and the CI check that enforces the dual-write decision.

### Phase 2: the Go worker

At the end of this phase every session's worker is a Go program, `mj-worker` and its staged name `hel` are built from `cmd/mj-worker`, and the crates `mj-worker` and `mj-worker-bin` are deleted. A user sees no difference except that worker binaries for every target architecture are built in seconds on any machine.

The scope is everything the worker executable contains: the relay server and its journal, the ACP client and its per-harness dialects (`mj-core/src/acp/dialect`), the MCP servers, the checkpoint capture, pack, export and restore commands (`mj-checkpoint`), review (`mj-review`), the transcript projection the worker needs (`mj-transcript`), the user shell and terminal, CPU accounting, the stop and liveness commands, and the microVM guest agent (`mj-worker/src/microvm/`), which runs as process 1 inside a virtual machine and makes raw Linux system calls. The Go worker implements relay protocol 72 or whatever is current when it ships, and reads every journal format and `relay-state.json` version that a released Rust worker may have left in a worker root.

Because `mj-checkpoint`, `mj-transcript`, `mj-review` and the relay types in `mj-core` are also used by the Rust daemon, they exist in both languages from this phase until Phase 3 ends. They are the first entries in the port ledger.

The worker is delivered in three steps. First it ships beside the Rust worker, chosen by an explicit setting per instance, with no fallback: an instance set to the Go worker that hits a defect reports it. Second, after the gate below has passed on nightly releases for the soak period the phase plan sets, the default changes and existing Rust workers are replaced through the ordinary idle upgrade. Third, the setting and the Rust worker are deleted.

The gate is: the phase's scripts in `tests/e2e/conformance.toml` pass with a Rust daemon and a Go worker on local, container and SSH targets; a session started on a Rust worker, upgraded in place to the Go worker while idle, continues its transcript with every stored digest still valid; a turn in progress on a Go worker survives `mj --instance <name> daemon restart`; and all 384 worker-side hard-won tests are ported or retired with a reason.

### Phase 3: the Go daemon and the control-plane commands

At the end of this phase the installed `mj` is a Go program that contains the daemon and every command that is not the terminal interface. It starts a legacy Rust binary, `mj-legacy`, only for the dashboard and for any command not yet ported, by `exec` on Unix and by spawning on Windows, which is how the terminal upgrade already starts a new binary. The seventeen daemon crates are deleted.

The scope is the store with all 140 migrations, the session runtime and its worker client, targets and provisioning, worker staging and launch, accounts and quota, import, build caches, the review host, setup and doctor, the control protocol server with its 135 actions, the HTTP API with its event stream, and the browser viewer, whose JavaScript, fonts and icons are embedded unchanged with Go's `embed` package.

The work proceeds up the layers that `scripts/check-crate-boundaries.mjs` defines: targets, then the store, then accounts, import, session wiki and build caches, then worker staging and launch, then the session runtime, then the profile catalog and review host, then the controller, setup, the server, and the daemon. Each layer lands with its hard-won tests and its integration tests and is added to the port ledger. The daemon cannot ship until the top layer is done, so this is the phase in which the dual-write rule costs the most and in which the layers should be taken as quickly as their tests allow.

Four pieces need design before translation, and the phase plan must settle each. The store's migrations are ported block for block and verified by migrating every database in `contracts/store/` with Go and comparing the resulting schema and every row with the Rust result. The published-snapshot type `SnapshotMap` (`mj-core/src/snapshot_map.rs`, 215 uses), which relies on a persistent ordered map to make copies cheap and to find changes by pointer comparison, is rebuilt as its own Go package with property tests before any of its users are ported. The in-place editing of `config.toml` that keeps comments has no ready Go library and is rebuilt on the token positions that `github.com/pelletier/go-toml/v2` exposes, proven against `contracts/config/`. Login terminals need a pseudo-terminal on Unix and a Windows console equivalent, because the daemon also runs on Windows.

The cutover is an ordinary upgrade. A user's Rust `mj` at the last hybrid release is replaced by the installer with Go `mj` and `mj-legacy`; the next start sends `PrepareUpgrade` to the Rust daemon, which drains and exits; the Go daemon opens the store, migrates nothing or only compatible revisions, and reattaches to the workers.

The gate is: the phase's conformance scripts pass with the Go daemon; the isolated upgrade scripts (`tests/e2e/container_upgrades.py`, `create_survives_restart.py`, `fetch_restart.py`, `daemon_endpoint.py`, `tui_quit_persistence.py`) pass from the last all-Rust release to the first Go-daemon release with a turn in progress and an unsent draft in the terminal; for one recorded store, every `GET` path of the HTTP API returns the same body from both daemons; the browser tests in `tests/e2e/web/` pass unchanged; and the 912 daemon-side hard-won tests are ported or retired.

### Phase 4: the Go terminal clients

At the end of this phase `mj-legacy` is gone and `mj` is one Go program again, as it is one Rust program today.

The remaining command-line commands and `mj acp` come first; they are thin callers of the control protocol and the HTTP API. Then `mj-chat` and `mj-tui` are ported on the drawing layer chosen in Phase 0. The existing design helps: drawing is already a set of functions from state to a frame (157 of them) and input is already reduced to action enums (`DashboardAction` with 110 variants, `ChatAction` with 26), so the structure carries over while the drawing calls are rewritten. The golden files compare text with trailing spaces trimmed and no styles, so they remain the acceptance test as they are.

Clipboard images, the embedded terminal emulator for login screens, QR codes, markdown rendering, and desktop notifications each need a Go replacement, named under Interfaces and Dependencies. Reading an image from the clipboard without cgo is the one with no known answer and gets a prototype at the start of the phase.

The cutover uses the terminal upgrade: a running Rust dashboard writes its `UpgradeResume` file and starts the Go dashboard, which reads version 1 of that format and restores drafts, questions, positions and layouts.

The gate is: all 83 terminal golden files and the 9 command-line ones match; the scripts `tests/e2e/tui_components_*.py`, `tui_mouse_commands.py`, `tui_selection.py`, `tui_review_discovery.py` and `tui_quit_persistence.py` pass; a draft typed into the Rust dashboard is present in the Go dashboard after an upgrade; the checks run on Linux, macOS and Windows through `.github/workflows/nightly.yml`; and the 502 client-side hard-won tests are ported or retired.

### Phase 5: retire Rust

Delete the remaining crates, `Cargo.toml`, `Cargo.lock`, `clippy.toml`, `release.toml`, `rust-toolchain.toml`, and the Rust-only scripts. Rewrite `.github/workflows/` for Go, including license notices. Publish the final crates.io release as decided. Rewrite the Rust-specific parts of `AGENTS.md` (Cargo commands, clippy rules, the dev profile note) and `RELEASING.md` for Go while keeping every rule about behaviour unchanged: instances, migrations, upgrades, subprocesses, and which tests to add. Remove Rust from `containers/Containerfile.agent-dev` if nothing else needs it. Decide the long-term home of `mj-voice-worker` and of the relevance helper if it was kept.

## Concrete Steps

These are the steps for Phase 0 and Phase 1. Later phases carry their own.

Work from the repository root, `/home/user/mjolnir` in a cloud session or the checkout root elsewhere. Go is installed by `mise install`, which reads `mise.toml`.

Create the prototype module and confirm the toolchain:

    mkdir -p go/spikes && cd go/spikes
    go mod init github.com/BrokkAi/mjolnir/go/spikes
    go version

Expect a line beginning `go version go1.27`.

Write the digest and float cases from Rust, then check them in:

    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-core --features golden contract_
    git status --short contracts/

Expect new files `contracts/relay/digest-cases.jsonl` and `contracts/floats.txt`. Running the same command without `MJ_UPDATE_GOLDEN` must pass and change nothing.

Run a prototype:

    cd go/spikes && go test -race ./digest/...

Expect `ok` and, in verbose mode, one passing subtest per line of the cases file. A failure prints the case number, the expected bytes and the produced bytes.

For the store prototype, give a throwaway daemon its own directories so the store's location is known, let it create the store, stop it, and copy the file:

    cargo build -p brokk-mjolnir
    export MJ_CONFIG_DIR="$PWD/go/spikes/.instance/config"
    export MJ_DATA_DIR="$PWD/go/spikes/.instance/data"
    target/debug/mj sessions
    target/debug/mj daemon stop
    cp "$MJ_DATA_DIR/mj.sqlite3" go/spikes/store/testdata/

`mj_core::config::data_dir` returns `MJ_DATA_DIR` when it is set, and a command that needs the daemon starts one (`connect_or_start` in `mj-cli/src/daemon.rs`), which creates `mj.sqlite3` there. If the file is absent after the first command, that command did not need the daemon; `target/debug/mj daemon restart` starts one. Add `go/spikes/.instance/` to `.gitignore`. Never open the original store from a prototype, only the copy.

For Phase 1, the commands a contributor runs every day are:

    go build ./cmd/... ./internal/...
    go vet ./cmd/... ./internal/...
    golangci-lint run ./cmd/... ./internal/...
    go test -race ./cmd/... ./internal/...
    node scripts/check-go-layers.mjs

Each prints nothing or `ok` lines on success. Record the wall time of the first and the fourth in Outcomes at the end of each phase.

## Validation and Acceptance

Phase 0 is accepted when each of the seven prototypes has an entry in the Decision Log saying what was decided, with the test output that shows why in Artifacts and Notes, and when the first and second prototypes pass. Those two have no fallback: without reproducible digests and a working store driver the port stops. The third chooses between two drawing bases, and the others find gaps to close.

Phase 1 is accepted when CI on a pull request runs the Go lane and the Rust lanes and both pass; when `go test -race ./internal/wire/...` decodes and re-encodes every file under `contracts/` to identical bytes; when deleting one variant's example from a Rust exporting test stops that crate from compiling; when `python3 tests/e2e/delegation.py --mj target/debug/mj` and the other scripts named in `tests/e2e/conformance.toml` still pass against the Rust build; and when a release dry run produces five Go executables.

Phases 2, 3 and 4 are accepted by the gates stated in their sections. In each, acceptance is observed by a person in a named instance as well as by the scripts: start a session, send a prompt, watch the turn finish, upgrade the piece under test, and see the same session continue.

The whole plan is accepted when `git ls-files '*.rs'` lists only `voice-worker/` and any helper kept by decision, the nightly release has shipped from the Go build on every platform, and Outcomes records the build and test times beside the figures in Purpose.

## Idempotence and Recovery

Every phase before its deletion step is additive. Go code is added beside Rust code and the Rust build is unaffected, so stopping a phase halfway leaves a working product.

The corpus tests are safe to rerun; they rewrite files only under `MJ_UPDATE_GOLDEN=1`. Prototypes work on copies in a named instance's directory and can be deleted with the instance.

Each swap can be undone until its deletion step. The worker swap is undone by changing the per-instance setting back; the Rust worker reads the journal a Go worker wrote because both write the same formats, which the corpus proves. The daemon swap is undone by a person installing the last Rust-daemon release, which works because the Go daemon does not raise the store's minimum compatible revision before the Rust daemon is deleted. `AGENTS.md` forbids automatic downgrade, and nothing here adds one; these are manual steps for the soak period.

Deleting a set of Rust crates is the one step that is not undone by a setting. It happens only after the soak the phase plan names, in its own commit, so that reverting that commit restores them.

## Artifacts and Notes

The measurements this plan relies on, taken on 2026-11-19:

    Rust lines, production / test (approximate)
      worker (mj-worker, mj-worker-bin)             46,800 /  64,300
      shared worker+daemon (checkpoint, transcript,
        review)                                     21,800 /  16,700
      mj-core                                       57,800 /  33,500
      daemon side (17 crates)                      201,200 / 182,300
      clients (cli, tui, chat, client)             131,800 /  84,000
      total                                        460,400 / 380,900

    tests: 6,132 functions; 2,064 marked Hard-won; 145 golden files
    tests/e2e: 46 Python scripts, 18,061 lines, plus Playwright specs

    boundary versions
      relay protocol 72        relay state 27       control protocol 108
      store revision 140       config 23            checkpoint schema 2..12

    Go libraries found on proxy.golang.org
      github.com/BrokkAi/acp-go v0.13.0            modernc.org/sqlite v1.62.0
      github.com/ncruces/go-sqlite3 v0.37.0        charm.land/bubbletea/v2 v2.1.2
      github.com/gdamore/tcell/v3 v3.6.0           github.com/creack/pty v1.1.24
      github.com/modelcontextprotocol/go-sdk v1.8.0
      github.com/klauspost/compress v1.20.4        github.com/pelletier/go-toml/v2 v2.4.3
      github.com/spf13/cobra v1.10.2

    the seven Go protocol libraries on the community page, latest release read
      library                    release  v1 schema  v2 schema  outside deps   code / test lines
      BrokkAi/acp-go             v0.13.0  1.29.1     alpha.13   none           142,800 / 52,300
      coder/acp-go-sdk           v0.20.0  1.32.0     none       none            25,500 /  8,600
      caelis-labs/acp-go-sdk     v1.8.0   1.33.0     alpha.17   x/sys           50,100 / 10,200
      ironpark/acp-go            v0.6.0   1.29       alpha.13   websocket       44,600 /  9,200
      Tangerg/acp                v0.3.0   1.21.0     no package jsonschema      25,800 / 15,300
      spachava753/acp-sdk        v0.7.0   not found  none       four            25,100 / 11,700
      eino-contrib/acp           v0.0.4   not found  none       hertz, others   31,400 /  7,600
    Mjolnir's Cargo.lock: agent-client-protocol 5.0.0, schema crate 1.15.1,
    which publishes v1 schema 1.29.1 and v2 alpha.13 according to acp-go's ROADMAP.md.
    Line counts include generated schema types.

## Interfaces and Dependencies

### Layout

    go.mod                      module github.com/BrokkAi/mjolnir
    cmd/mj-worker/              Phase 2
    cmd/mj/                     Phase 3 (daemon, commands), Phase 4 (dashboard)
    internal/wire/              canonical JSON, frames, float formatting
    internal/subprocess/        the only package that may start a child process
    internal/task/              the only package that may start a goroutine
    internal/relay/             protocol types, journal, digests
    internal/control/           daemon control protocol types
    internal/store/             SQLite store and migrations
    contracts/                  the corpus, and port-ledger.toml

Package names below `internal/` follow the Rust crate they replace, so that the layer check maps one to one.

### What replaces the compiler's checks

A Rust enum becomes a Go interface with an unexported marker method and one struct per variant, so that no other package can add a variant. The `exhaustive` and `gochecksumtype` linters make a `switch` that misses a variant a CI failure. This stands in for `match`.

`go test -race` runs in CI for every package. It finds data races that happen in a test; it does not prove their absence. The structural rule does the rest: state that more than one goroutine needs is owned by one goroutine and reached through its channel, as `AGENTS.md` already requires in other words.

A Rust type that releases something when dropped becomes a value with a `Close` method, and the function that creates it defers the `Close`. `ProcessGroupGuard` in `mj-core/src/subprocess.rs` is the model case: the Go helper kills the process group in a deferred call on every return path, and the tests prove it on cancellation, timeout and overflow.

A custom analyzer in `internal/lint`, run by `go vet`, forbids the `go` statement outside `internal/task` and forbids `os/exec` outside `internal/subprocess`. These are the counterparts of the `disallowed_methods` entries in `clippy.toml`. `errcheck` forbids a discarded error.

### Rules for encoding

These are the differences between `serde_json` and Go's `encoding/json` that the corpus will otherwise find one at a time.

Digest-covered types and anything compared byte for byte are written with the encoder in `internal/wire`, never with `encoding/json` directly. Go escapes `<`, `>`, `&`, U+2028 and U+2029 and replaces invalid UTF-8; `serde_json` does none of that. `serde_json` prints a whole-number float as `1.0` and switches between decimal and exponent form at different magnitudes than Go does; `FormatFloat` in `acp-go` is the implementation to use. `serde_json` prints `f32` with the shortest digits that round-trip as `f32`, not as `f64`.

Member order is part of the bytes. A struct writes its fields in declaration order. A `BTreeMap` writes its keys sorted. An untyped `serde_json::Value` object keeps insertion order in this build. A `HashMap` has no fixed order and must not appear in anything that is hashed or compared.

An `Option<T>` field is a pointer or an explicit optional type in Go. `omitempty` on a plain number, boolean or string drops a present zero and is not used. A field is omitted in Go exactly where Rust has `skip_serializing_if`, with the same condition.

A Rust `Vec<u8>` is a JSON array of numbers unless the Rust code encodes it otherwise; Go's `[]byte` is base64. Each such field gets a named type with its own encoding.

A `u64` is decoded into a `uint64` field. Nothing decodes into `any`, which would turn it into a float.

Each enum is encoded in the representation its Rust definition names: a bare string for a unit variant, `{"Variant": ...}` by default (including `Result`, which is `{"Ok": ...}` or `{"Err": ...}`), an array for a tuple variant, and the named tag and content keys where the Rust type sets `tag` and `content`.

A type marked `deny_unknown_fields` in Rust is decoded with unknown fields rejected in Go, so that an old reader still refuses what it does not understand.

A filesystem path that the store holds as raw bytes (`sessions.project_directory`) stays `[]byte` in Go.

### Libraries

Command-line parsing uses `github.com/spf13/cobra`. HTTP serving and client use the standard library, with the standard library's TLS. SQLite uses the driver chosen in Phase 0. Compression uses the standard library for gzip, zip and tar and `github.com/klauspost/compress` for zstd. Hashing uses the standard library and a pure-Go BLAKE3. Confined filesystem access (`cap-std` in `mj-core/src/confined_path.rs`) uses the standard library's `os.Root`. Comment-preserving edits of harness JSON files use `github.com/tailscale/hujson`. Pseudo-terminals use `github.com/creack/pty` on Unix. ACP uses `github.com/BrokkAi/acp-go`: the root package for the worker's client side, `agent` for `mj acp`, and `unstable` for system prompts, end-of-turn usage and session fork. The hand-written MCP servers are ported as they are; neither the server framework in `acp-go/mcp` nor `github.com/modelcontextprotocol/go-sdk` is adopted during the port, so that behaviour does not change at the same time as the language. Time zones use the standard library with embedded zone data. The allocators (`jemalloc`, `mimalloc`) have no counterpart and are dropped; the daemon's memory behaviour is measured again after Phase 3 with the method in `.agents/docs/daemon-profiling.md`.

No library is added for dependency injection or mocking; `AGENTS.md` prefers hand-written fakes.

Revision note, 2026-11-19: first version, written at Ryan Svihla's request to scope the conversion in phases. It records the measurements, proposes the order and the rules, and specifies Phases 0 and 1. Five decisions are marked Proposed and wait for his answer.

Revision note, 2026-11-19, second: Ryan Svihla asked to use his Go protocol library, `github.com/BrokkAi/acp-go`, and whether it is the best of those listed. The plan now names it in place of `coder/acp-go-sdk`, records the comparison of the seven listed libraries, and builds the digest prototype on the `serde_json`-exact encoder that library already has. The same pass corrected the build figures in Purpose, the daemon crate count and test line count, and the store prototype's commands, and added the member-order rule after `Cargo.lock` showed `serde_json` built with `preserve_order`.
