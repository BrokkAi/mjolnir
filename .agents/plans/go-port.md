# Port Mjolnir to Go, validated end to end at every step

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`. It was restructured on 2026-11-24 after the owner's verdict that the earlier way of working was a fundamental error; read "How to work" before doing anything.


## Purpose / Big Picture

Mjolnir (`mj`) is a terminal dashboard and background service that runs coding agents (Claude Code, Codex, OpenCode and others) as persistent sessions. It is written in Rust: about 925,000 lines across 32 crates (Rust packages) in this repository. The team wants a Go build of the same product that replaces it feature for feature, with the same behaviour a user sees, built on Go's standard library or established Go packages, using `github.com/BrokkAi/acp-go` for the Agent Client Protocol (ACP: the JSON-RPC protocol over standard input and output that Mjolnir uses to drive each agent) and Charm's libraries (Bubble Tea, Lip Gloss, Bubbles, ultraviolet) in place of ratatui for the terminal interface.

When this plan is done, someone can build `go/bin/mj` and `go/bin/mj-worker` with `go/scripts/build.sh`, run them against an isolated instance (`mj --instance <name> ...`), and use every command, the dashboard, the daemon and remote workers exactly as they would use the Rust `mj`, proven by the same end-to-end scenarios passing on both builds.

The Go build lives in `go/` (Go module `github.com/BrokkAi/mjolnir/go`). Rust and Go share the configuration format, the SQLite store, the relay protocol between daemon and worker, the daemon's HTTP API and every persisted file format. That shared ground is what makes this plan work: any Go part can be swapped into a running Rust Mjolnir and judged by whether real sessions still work.


## How to work (read this first)

Two earlier efforts failed on basic mistakes. The first (2026-11-20, reverted in 7b78e794a) ported Rust's own libraries into Go and produced no Mjolnir. The second (2026-11-20 to 2026-11-24) translated 64% of the Rust code bottom-up, crate by crate, and validated it only with Rust tests ported by the same agents; nothing could run until the last crate, and the first live run of the Go worker under a real daemon failed at session start. The owner called the bottom-up order "a fatal and fundamental error": a lot of work, nothing functional, and no way to know how close it is. These rules exist to make those mistakes impossible to repeat.

1. Nothing counts until it runs against the real system. A Go part is done only when the end-to-end scenario suite (defined below under Validation and Acceptance) passes with that part swapped into a working Mjolnir. Passing Go unit tests is necessary, never sufficient.
2. Work in vertical slices from the user's side, in the milestone order below. Never port a layer that nothing runnable needs yet.
3. Use the swap-in method. Rust and Go speak the same protocols, so: the Go worker runs under a Rust daemon (`MJ_WORKER_BINARY`); the Go `mj` CLI and dashboard talk to a Rust daemon; the Rust CLI talks to a Go daemon. Every scenario also runs on the all-Rust control in a second isolated instance, which shows what correct means.
4. Every merge passes three gates, in order: (a) `go build ./...` and `go vet ./...`; (b) the scenario suite for every part that is already Go, plus the Go tests of the changed packages; (c) an independent reviewer agent, who did not write the change, compares every added or changed Go test with its Rust original and rejects weakened ones (see "Test fidelity rules" below). A change that fails a gate is not merged.
5. Small batches. Fan out agents only inside one slice, and only as much work as can be gated the same day. Before any fan-out, state what the owner will be able to run when it lands and roughly what it costs.
6. One Rust target, a fixed commit. The Go build matches Rust commit 5b68df737775f748e6d3e9207930e5467c31e397 (5b68df737), recorded in `go/RUST_TARGET`. It was the `v2.75.0` tag on 2026-11-24, but that release was still in flight and the tag may move: the commit is the target, never the tag. Until the replay below lands, the translated code follows commit 3dde0c11b plus five later commits teammates carried into `go/` (0fc96c573, b1200383e, e14e4a6fe, e818721b6, 8a8ea94d0, all in v2.75.0). The gap is closed commit by commit: each Rust commit between 3dde0c11b and v2.75.0 that touches a ported crate (119 of them) is replayed in order as one Go commit titled `go port: <Rust subject> (rust <short sha>)`, gated like any change; the five carried commits are checked for completeness and recorded as applied, not ported twice. Crates not yet ported are ported from the target commit. Later releases are taken the same way, commit by commit, never piecemeal.
7. Report progress as the list of scenarios that pass on Go, never as lines or percentages ported.
8. Operating rules for agents and the coordinator:
   - Never `cd` out of the repository in a shell that later launches worktree agents; use absolute paths, `git -C` and `go -C`. A worktree workflow launched from outside the repository fails every agent.
   - After launching agents, confirm from the workflow journal that they started before saying they run.
   - Never delete a worktree or branch that holds commits; pin them first (`git update-ref refs/go-port/wip/<name> <sha>`).
   - The daemon deletes a session's worker directory, including `worker.log`, when its worker dies. Copy logs out continuously (`tail -F ... > file &`) before signalling a process for a goroutine dump.
   - Test only in isolated instances, with `env -u MJ_CONFIG_DIR -u MJ_DATA_DIR -u MJ_SESSION_ID -u MJ_SESSION_BASH_ENV -u MJ_INSTANCE -u MJ_NO_SPLASH` in front of every `mj` command (inside a Mjolnir session those variables otherwise point at the live default instance). Stop test daemons and kill leftover test workers when done.
   - Push merged, gated work promptly; the owner does not want intermediate work lost.

Translation rules (which Go package replaces which Rust library, naming, serde, tests) are in `.agents/docs/go-port/core-brief.md`. In short: translate Mjolnir's own code onto Go equivalents; never port a Rust library.

    Rust crate                  Go replacement
    tokio, futures              goroutines, channels, context.Context, sync
    serde, serde_json           encoding/json (custom MarshalJSON where serde attributes need it)
    toml, toml_edit             BurntSushi/toml; go-toml/v2 unstable parser with byte splicing for format-preserving edits
    clap                        github.com/spf13/cobra, help rendered in clap's layout by go/internal/cli/clapstyle.go
    ratatui, crossterm          charm.land/bubbletea/v2, lipgloss/v2, charmbracelet/ultraviolet cell buffers
    agent-client-protocol       github.com/BrokkAi/acp-go
    rusqlite                    database/sql with modernc.org/sqlite
    reqwest, axum, hyper        net/http
    anyhow / tracing            wrapped errors / log/slog


## Test fidelity rules

The 2026-11-24 audit of the ported tests (below) found the same weakening habits in every agent group. A reviewer rejects a Go test that: discards an error that Rust `unwrap()`s (Rust's panic is an assertion); compares an "unchanged" value through a shallow copy that shares maps, slices or pointers; narrows a whole-value equality to an ID, length, prefix or JSON shape; replaces a paused clock with a short real sleep or overrides a production timeout; drops the positive half of a redaction or secret-stripping check; skips with `t.Skip` for any reason other than a missing ported dependency (named) or the same platform condition Rust uses; or uses `strings.Index` without checking for -1. Golden and fixture files must be byte-identical to the pinned Rust ones.


## Progress

- [x] (2026-11-20) First slice: Go module, instance and directory resolution, clap-identical `mj --help`/`--version`/usage errors (golden files from the Rust `mj`).
- [x] (2026-11-20 to 2026-11-24) Translated, bottom-up, with ported Rust tests passing but not validated against a real system: mj-core, mj-transcript, mj-checkpoint, mj-review, mj-build-id, mj-client, mj-worker, mj-harness-install, mj-microvm, mj-worker-mcp, mj-worker-review, mj-chat, mj-targets, mj-github-relevance, voice-worker, mj-session-types, mj-store, mj-tui (Charm), mj-worker-bin (`go/cmd/mj-worker`), mj-accounts, mj-build-cache, mj-github-watch, mj-import, mj-worker-staging. About 590k of 925k Rust lines; 4,351 Go tests. Declarations only (bodies unported, stopped 2026-11-24): mj-profile-catalog, mj-sessionwiki, mj-worker-launch.
- [x] (2026-11-24) Independent test-fidelity audit of all ported tests (workflow wf_c827a56b-45c): 4,329 Rust tests compared, 97.7% faithful; about 100 not, including 8 high-severity gaps (listed under Surprises).
- [x] (2026-11-24) Swap-in harness proven: a Rust daemon at 3dde0c11b in instance `go-e2e` ran the Go worker (stamped with the Rust worker identity) via `MJ_WORKER_BINARY`; control instance `go-e2e-rust` with the Rust worker completed a tool-using prompt in 13 s.
- [x] (2026-11-24) Milestone 1: the Go worker passes all eight scenarios of `tests/e2e/go_port_scenarios.py` under the Rust daemon at 3dde0c11b, and the suite passes on the all-Rust control, including both run concurrently (merged 3ed962bf1 through all three gates). Two defects, both relay JSON bytes that must match serde exactly: nil collections written as `null` (one owner now: `SerdeCollections`, 29 types were affected) and string escaping (U+2028/U+2029 and `<>&`; one owner now: `core.SerdeJSON`), which broke the daemon's relay event digests. Regression tests replay frames and events recorded from the Rust worker. The independent reviewer blocked the first version of the suite because its process checks matched host-wide; fixed with per-run markers (b00ac7c26).
- [ ] Milestone 1 follow-ups (from the review): about 25 encoder copies outside core and worker still use plain encoding/json (matters once Go writes checkpoints a Rust worker reads, or a Go daemon or CLI exists); a non-nil pointer to a nil slice still writes `null`; per-type nil-to-empty helpers duplicate `SerdeCollections`; raw harness JSON keeps its number spelling and key order where Rust's `Value` normalises them; the suite should check that "Allow once" ran the command and finish destroy before `daemon stop` on failure. Not yet exercised: worker replacement or upgrade, container and SSH targets, harnesses other than OpenCode, sub-agents, review, mailbox, project memory.
- [ ] Milestone 1: fix the audit's high-severity test gaps in the worker and core (Antigravity module, reload secret stripping, protocol-82 guardian skip) and the medium weakenings, through the reviewer gate.
- [ ] Milestone 2: Go `mj` CLI and Charm dashboard against the Rust daemon.
- [ ] Milestone 3: Go daemon serving the Rust CLI and Go workers, slice by slice.
- [ ] Milestone 4: all Go; the credential-free scripts in `tests/e2e/` pass against `go/bin/mj`.
- [x] (2026-11-24) Target chosen with the owner: Rust commit 5b68df737 (what the in-flight v2.75.0 tag pointed at that day; the commit, not the tag, is the target); it is ahead of the 3dde0c11b baseline and contains the five Rust commits already carried into `go/`.
- [ ] Replay the 119 Rust commits from 3dde0c11b to v2.75.0 that touch ported crates, one Go commit each, in order, after milestone 1's current debugging lands (they touch the same worker code). Then rebuild the Rust reference at v2.75.0 and rerun the scenario suite.
- [ ] Later releases: replay commit by commit after milestone 4.


## Resume here

State on 2026-11-24, 19:50: milestone 1 is done. Milestone 2, slice 2a (the Go `mj` command surface against the Rust daemon) is ported, merged and pushed, with its code review approved. Gate (b) passed on a Codex profile (`cx`, Codex with the owner's DeepSeek Flash provider; authorised paid testing): all eight scenarios pass with `go/bin/mj` and the Go worker under the Rust daemon, and on the all-Rust control, each alone and both concurrently, with no Go defect found. Evidence: `/home/ryansvihla/.cache/mjolnir/build/go-port-tools/e2e/cx-runs/`. On Codex, scenario 5's tool-permission half does not apply (Codex's own reviewer answers tool approvals in codex-acp agent mode at the pinned commit); the suite prints it as N/A with the reason and drives Codex's plan approval instead.

Not yet merged: the suite's Codex support (commit 11393f22a, tests/e2e/go_port_scenarios.py) and the agent's plan notes (dea4ec9b2), on the pinned branch `refs/go-port/wip/worktree-agent-ae858b1ed8854616c`. They need gate (c), an independent review of the suite change, before merging; then cherry-pick both onto master. Test Codex home (private copy of ~/.codex/config.toml, mode 600, holds the DeepSeek token): `/home/ryansvihla/.cache/mjolnir/build/go-port-tools/e2e/codex-home`; delete it when testing ends. Rerun (build both Go binaries with the Rust daemon's identity first, see Concrete Steps): `tests/e2e/go_port_scenarios.py --profile cx --mj go/bin/mj --daemon-mj <rust mj> --worker-binary go/bin/mj-worker --instance go-e2e`, and the control with the Rust mj on go-e2e-rust.

Still open for slice 2a: the OpenCode path of scenario 5 (tool permission through `oc-ask`) has not run with `go/bin/mj` (OpenCode free quota). Known side effect of test daemons: a 2.74.0 test daemon installing codex-acp 1.15.0 removed the live instance's unused 1.15.1 install from the shared `~/.cache/mjolnir/harnesses/codex` (running sessions are protected; the live daemon reinstalls it when needed). Named instances' viewer ports (38000-38999) overlap Linux's ephemeral port range; a lingering connection once blocked a test daemon's viewer. Reviewer follow-ups and open output differences from slice 2a are listed in Progress and Surprises.

Next: review and merge the suite change; slice 2b (the Charm dashboard against the Rust daemon); then the replay to the target commit 5b68df737; then milestone 3. Start the recurring push job again whenever work resumes.


## Milestones

Milestone 1, the Go data plane under the Rust control plane. The worker is where sessions live (the ACP connection to the agent, the relay journal, checkpoints, sub-agent tools), and the Go worker already exists, so it is validated first. Setup is under Concrete Steps. Work through the scenario suite with the Go worker; for each failure find the root cause against the pinned Rust, fix the Go code, and add a regression test where AGENTS.md "Which tests to add" allows (an integration test of the relay protocol against a recorded Rust worker or daemon exchange, or of ACP against a recorded harness transcript). At the end the suite passes on both `go-e2e` (Go worker) and `go-e2e-rust` (control). Decision point for the owner: if the worker reaches that state after a handful of seam defects, the translated code is worth keeping and milestone 2 starts; if failures turn out to be spread through the translation, stop and rethink before spending more.

Milestone 2, the Go `mj` users run. Port mj-cli's command surface and dashboard on top of the already-translated mj-client, mj-chat and mj-tui, against the Rust daemon at 3dde0c11b with the Go worker. Commands that need the daemon in-process (`daemon-run`, `setup`, `doctor`, `import`, `recover`) say plainly that they are not ported yet. At the end the scenario suite driven by `go/bin/mj` passes, the TUI scripts in `tests/e2e/` that need no credentials (for example `tui_components_*`, `tui_quit_persistence.py`, `tui_selection.py`) pass against `go/bin/mj`, and the owner can use the Go dashboard.

Milestone 3, the Go daemon. Port the control plane (mj-session-runtime, mj-controller, mj-server, mj-review-host, mj-setup, mj-daemon, and finishing mj-profile-catalog, mj-sessionwiki, mj-worker-launch) slice by slice, each slice driven by a scenario: first `daemon-run` serving workspace and session listing, then create, then prompt and wait, then suspend and resume, and so on. Validate each slice with the Rust CLI against the Go daemon, and the Go CLI against the Go daemon. The store is shared: a Go daemon must open a store a Rust daemon wrote, and the reverse, with the same migration rules as AGENTS.md.

Milestone 4, all Go and parity. Every credential-free script in `tests/e2e/` passes against `go/bin/mj`; scripts that need paid harness logins are run once with the owner's agreement. Then the one sync to current Rust.


## Surprises & Discoveries

- Observation: passing ported tests did not mean working code. The Go worker under a real Rust daemon hung at session start; the same setup with the Rust worker completed a tool-using prompt in 13 s.
  Evidence: instance `go-e2e` session a8e1fe5b..., worker-startup.json last step `acp-session-open`, daemon log "the worker process is gone; it reached the startup step acp-session-open" after 133 s; control session c7c53955... "turn finished (completed, end of turn) in 13.1s · 1 tool call".

- Observation: the Go build still said version 2.73.0 after the sync to Rust 2.74.0, so a Rust daemon would have rejected the Go worker. Fixed in ad4d80209; `cli.Version` now reads `core.PackageVersion`.

- Observation: the test audit (wf_c827a56b-45c) found 97.7% of ported tests faithful, and these high-severity gaps: the whole Antigravity dispatch test module (8 tests on Stop ordering, stale-state fencing, refusing an idle reservation while busy) has no Go port; the harness in-place reload test lost its accepted half, so secret stripping (`FASTINO_KEY`, excluded secrets) is untested; `guardian_requests_remain_supported_on_protocol_82_workers` is skipped with a false reason. mj-import is the weakest crate (86.9%: 17 sessionwiki skips leave transcript import untested). Recurring habits are listed under Test fidelity rules. The Go code also took some Rust commits after the pin (through teammates' updates, for example #1670's daemon image) and not others.

- Observation: open differences of the Go `mj` (slice 2a), not yet fixed or covered by a test:
  - `mj transcript --role tool --json` prints each tool call indented. Rust's `OpaqueToolCall` (mj-core/src/transcript.rs) serializes as a `RawValue` holding `serde_json::to_string` of the call, and serde_json's pretty printer writes a `RawValue` verbatim, so the Rust mj prints the call on one line inside the indented page. `printJSON` uses `core.SerdeJSONPretty`, which indents the compact encoding as a whole and cannot tell the raw fragment apart. The fix needs a pretty encoder that writes raw fragments verbatim; then add the case to `TestTranscriptPrintsRecordedRustDaemonPagesAsTheRustMjDoes` with output captured from the Rust mj. The scenario suite reads this output with `--json` and still parses it.
  - clap's usage errors add a `tip:` the Go `mj` omits, and with it name the suggested argument in the usage line. Rust `mj respond --session abc --request r` prints `tip: to pass '--request' as a value, use '-- --request'`; Rust `mj wait --session abc --since x` prints `tip: a similar argument exists: '--instance'` and `Usage: mj wait --session <SESSION> --instance <NAME>`, where Go prints `Usage: mj wait --session <SESSION>`.
  Evidence: the slice 2a review (5425a414d's message for the first); the second reproduced 2026-11-24 against the Rust mj at 3dde0c11b.

- Observation: mj-core needed declarations before bodies; clap orders options by declaration index and shows current env values; cobra sorts commands and adds `help`/`completion` unless told not to. (Kept from the first slice.)


## Decision Log

- Decision: restructure the plan around end-to-end milestones that swap Go parts into a working Rust Mjolnir, with merge gates (build, scenario suite, independent test-fidelity review), one Rust baseline, and progress reported as passing scenarios.
  Rationale: the owner's verdict (2026-11-24) that bottom-up translation was a fundamental error and that confidence in the existing work is zero; the team very much wants a Go Mjolnir, and repeated false starts came from basic process mistakes, not from the goal.
  Date/Author: 2026-11-24, agent with the owner.

- Decision (retrospective): the bottom-up order (mj-core first, mj-cli last) was a fundamental mistake: a lot of work produced nothing runnable and no measure of closeness.
  Date/Author: 2026-11-24, owner's assessment.

- Decision: all of mj-core is one Go package, `go/internal/core`, one file per Rust module; other crates are one package each under `go/internal/<crate without mj- and dashes>` (`mj-import` is `go/internal/mjimport`, since `import` is a Go keyword); binaries under `go/cmd/`.
  Rationale: 42 of mj-core's modules form one dependency cycle, which Go packages cannot express.
  Date/Author: 2026-11-20, agent.

- Decision: keep tree-sitter-bash through cgo for mj-transcript's shell command summaries.
  Rationale: Rust uses the same C parser and its error recovery, which the summaries depend on. Cost: a C cross-compiler for Windows/macOS cross builds and static Linux workers.
  Date/Author: 2026-11-20, agent.

- Decision: when Rust extracts modules into a new crate, the Go code moves to the new crate's package and keeps its Go names.
  Date/Author: 2026-11-23, agent.

- Decision: parse with cobra, render help and usage errors in clap's layout; recognise development builds by a `.mj-development-build` directory beside the binary or Go's `go-build` temporary directories.
  Date/Author: 2026-11-20, agent.

- Decision: the Go build targets one fixed Rust commit, 5b68df737 (v2.75.0 as tagged on 2026-11-24; the tag may move, the commit does not), and reaches it, and every later release, by replaying Rust commits one by one.
  Rationale: the owner wants the Go build to match a version of mj, and commit-by-commit porting to be easy at the end; per-crate bases (`go/PORTED_FROM`) and piecemeal syncs left the Go code matching no single Rust revision. `go/RUST_TARGET` replaces `go/PORTED_FROM`.
  Date/Author: 2026-11-24, agent with the owner.


## Outcomes & Retrospective

As of 2026-11-24: about 590k of 925k Rust lines are translated and their ported tests pass, but no Go part has yet been shown to work in a real system, and the first attempt to do so failed. Spent so far: about 28M agent tokens in the workflows that completed and reported totals (mj-core 8.4M, layer 1 4.8M, layer 2 7.8M, audit 6.8M), plus the layers 1 to 5 run, the sync and several fix agents, which were stopped before reporting and are not measured; the true total is well above that. Lessons are encoded in "How to work": validate against the real system from the first slice, go top-down from what users run, gate every merge, keep one Rust baseline, report by working scenarios.


## Context and Orientation

The daemon (`mj daemon-run`, crates mj-daemon over mj-controller, mj-server and mj-session-runtime) is the control plane: it owns the SQLite store, session records, lifecycle operations and the HTTP API. Workers (mj-worker, binary `mj-worker`, one per session, copied into the session's worker directory as `hel`) are the data plane: they run the harness over ACP and own the relay journal. The `mj` CLI (mj-cli, with mj-client for the API, mj-tui and mj-chat for the dashboard) talks to the daemon over HTTP. AGENTS.md describes the contracts between them.

The current Rust reference is a read-only git worktree at `/home/ryansvihla/.cache/mjolnir/build/go-port-ref/rust-3dde0c11b` (create with `git worktree add --detach <path> 3dde0c11b` if missing); its debug build is in that checkout's `target/debug` (`mj`, `mj-worker`). After the replay, a second read-only worktree at `/home/ryansvihla/.cache/mjolnir/build/go-port-ref/rust-5b68df737` (commit 5b68df737, created from the commit, not the tag) becomes the reference and the swap-in harness uses its build.


## Plan of Work

Milestone 1 is the next and only authorised work: write `tests/e2e/go_port_scenarios.py` (modelled on the existing scripts in `tests/e2e/`, which take `--mj <binary> --instance <name>`), make it pass on the all-Rust control, then make it pass with the Go worker by fixing the Go code, each fix through the gates. In parallel, fix the audit's high-severity test gaps in the worker and core through the reviewer gate. Report to the owner per scenario, then stop for the owner's decision.


## Concrete Steps

Build the Rust reference (once) and the Go worker with the Rust worker identity, from the repository root, without changing directory:

    cargo build --manifest-path /home/ryansvihla/.cache/mjolnir/build/go-port-ref/rust-3dde0c11b/Cargo.toml -p brokk-mjolnir -p brokk-mj-worker
    grep -ao 'MJ-WORKER-BUILD:[0-9.]*+[0-9a-f]*' /home/ryansvihla/.cache/mjolnir/build/go-port-ref/rust-3dde0c11b/target/debug/mj-worker
        MJ-WORKER-BUILD:2.74.0+893f80b9daead06cf443cd0e6ca8275ab3e9f239
    MJ_WORKER_INPUTS_ID=893f80b9daead06cf443cd0e6ca8275ab3e9f239 go/scripts/build.sh

Isolated instances: `~/.config/mjolnir/instances/go-e2e/config.toml` and the identical `go-e2e-rust` (control) hold `version = 24`, `[jev] enabled = false`, an OpenCode profile `oc` (`kind = "open-code"`, `home` ending in `/opencode` with an `opencode.json` selecting `opencode/big-pickle`, placeholder `DEEPSEEK_API_KEY` under `[profiles.oc.environment]`), and `[targets.localhost] kind = "bare"`. OpenCode's free models need no login; keep prompts synthetic because content goes to OpenCode's hosted service.

Run a session with the Go worker (prefix from rule 8; T is the Rust debug directory):

    E="env -u MJ_CONFIG_DIR -u MJ_DATA_DIR -u MJ_SESSION_ID -u MJ_SESSION_BASH_ENV -u MJ_INSTANCE -u MJ_NO_SPLASH"
    $E MJ_WORKER_BINARY=<repo>/go/bin/mj-worker $T/mj --instance go-e2e daemon status     # must name instances/go-e2e
    $E MJ_WORKER_BINARY=<repo>/go/bin/mj-worker $T/mj --instance go-e2e workspaces create goe2e
    $E MJ_WORKER_BINARY=<repo>/go/bin/mj-worker $T/mj --instance go-e2e --workspace goe2e new --profile oc --target localhost --project-directory <scratch git repo> --title t
    $E $T/mj --instance go-e2e prompt --session <id> "Create a file named hello.txt containing exactly: go-port works. Then reply DONE."

The same commands without `MJ_WORKER_BINARY` and with `--instance go-e2e-rust` give the control. Clean up with `mj --instance <name> destroy --session <id>` and `daemon stop`; kill leftover `instances/go-e2e*/workers/*/hel` process groups.


## Validation and Acceptance

The scenario suite (`tests/e2e/go_port_scenarios.py`, taking `--mj`, `--instance` and optionally `--worker-binary`) runs each scenario and compares the outcome with the control: create a session; a prompt that needs a tool call, with the file present in the session's checkout and the transcript showing the tool call; a follow-up prompt in the same session; interrupt a long turn, then prompt again; a question or permission request answered through `mj elicitations`/`mj respond` when the harness raises one; stop and restart the daemon during a turn, the turn finishes and the transcript is complete; suspend then resume, then a prompt; destroy, leaving no processes or files. A milestone is accepted when every scenario passes on the Go configuration of that milestone and on the control.


The scenario suite and any test that drives a real model are run by hand only, never in CI: they spend model credits or free-tier quota (OpenCode's free quota ran out after one day of testing) and need a local isolated instance holding credentials. If Go tests are added to CI, add `go test ./...` only (its relay and digest tests replay recorded frames and make no model calls), and keep `tests/e2e/go_port_scenarios.py` and the TUI scripts out of every workflow. Never put a harness credential in the repository: the test Codex home with the DeepSeek token lives at `/home/ryansvihla/.cache/mjolnir/build/go-port-tools/e2e/codex-home`, outside the repository.


## Idempotence and Recovery

Everything is additive under `go/`, `tests/e2e/go_port_scenarios.py` and this plan. Isolated instances can be deleted and recreated (`~/.config/mjolnir/instances/go-e2e*`, `~/.local/share/mjolnir/instances/go-e2e*`). Never point any of this at the default instance.


## Artifacts and Notes

    control (Rust worker, go-e2e-rust):  turn finished (completed, end of turn) in 13.1s · 1 tool call
    Go worker (go-e2e):                  session stays "launching"; last startup step acp-session-open; daemon gives up after 133 s


## Interfaces and Dependencies

Go 1.27.1 or newer, cgo with a C compiler (tree-sitter-bash). Modules include cobra, BurntSushi/toml, go-toml/v2, acp-go v0.14.0, bubbletea/v2, lipgloss/v2, ultraviolet, modernc.org/sqlite, klauspost/compress, yaml v3, hujson. Host prerequisites on Fedora for some tests: `perl-Digest-SHA perl-File-Copy perl-JSON-PP` (provides `shasum`); run tests with `LC_ALL=C.UTF-8 LANGUAGE=en`.
