# Resolve Mjolnir tickets filed after 989

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. Maintain this document in accordance with `.agents/PLANS.md`.

## Purpose / Big Picture

Validate and resolve every Mjolnir issue filed after issue 989. A user should be able to run ZCode on a managed container target, retain and inspect sessions whose asynchronous launch fails, discover Muse models while seeing an honest statement when per-turn usage is unavailable, read concise ZCode quota output, and move a Claude session without silently losing its model and effort choices. The fixes must be proven with focused behavior tests, the full Rust test suite, Clippy, and live terminal testing where the configured environment permits it.

## Progress

- [x] (2026-09-14 05:35Z) Listed issues 990 through 996 and read every open report; issues 990 and 991 were already closed, while 992 through 996 require validation.
- [x] (2026-09-14 06:20Z) Validated every open report against current source and live profile data; added behavior tests for every source defect.
- [x] (2026-09-14 06:20Z) Resolved issue 992 in source: the managed image already contains the pinned ZCode runtime and staged profile files, and old images now fail with a direct backend diagnostic.
- [x] (2026-09-14 06:20Z) Resolved issue 993 in source: failed launches remain durable, idempotency is recorded before follow-up, wait reports failure, and unknown-session event requests return immediately.
- [x] (2026-09-14 06:20Z) Resolved the actionable part of issue 994: Muse advertises its native configured model; native transcript inspection confirmed that trustworthy per-turn token values are not exposed, so existing `missing_reports` coverage remains honest.
- [x] (2026-09-14 06:20Z) Resolved issue 995 by removing the redundant `GLM Coding Plan` quota suffix.
- [x] (2026-09-14 06:20Z) Resolved issue 996 by freshly probing a changed same-harness destination profile and rejecting a move before mutation if it would drop the accepted model or effort pin.
- [ ] Run focused tests, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and tmux-driven live acceptance tests.
- [ ] Commit validated fixes, update the issues with evidence, and close the resolved tickets.

## Surprises & Discoveries

- Observation: The repository has two Git remotes. `hel` points to `BrokkAi/hel`, whose issue numbers are small, while `origin` points to the requested tracker `BrokkAi/mjolnir`, where issues 990 through 996 exist.
  Evidence: `git remote -v` and `gh issue list --repo BrokkAi/mjolnir --state all`.

- Observation: Muse's ACP catalogue exposes effort choices but no model selector; its native `settings.json` does identify the selected model.
  Evidence: live `mj models --profile muse --json` returned an empty model catalogue before the fix, while `/home/jonathan/.config/muse/settings.json` selects `muse-spark-1.3-contributor`.

- Observation: Muse's checkpointed native `session.jsonl` contains message content but no token, usage, or cost fields.
  Evidence: inspection of the 206 KiB native transcript from live session `09bae...` found no trustworthy accounting fields, matching its durable `missing_reports=1` coverage.

- Observation: Claude model catalogues genuinely vary by authenticated profile: `claude2` offers `opus` and `opus[1m]`, while `claude3` offers only `opus[1m]`.
  Evidence: fresh live profile discovery for both profiles. A global normalization would advertise a value the destination cannot accept.

- Observation: The first live ZCode image build contained the checksum-verified backend, but AppImage extraction preserved root-only directory modes, so the runtime `hel` user received `Permission denied` for `/opt/zcode/glm/zcode.cjs`.
  Evidence: direct `podman run` as the image's default user could read the environment but could not `ls` the backend. The container recipe now applies `chmod -R a+rX /opt/zcode` before switching users.

## Decision Log

- Decision: Treat issues 992 through 996 as the active resolution set and audit already-closed issues 990 and 991 only for regression coverage rather than reopening them without evidence.
  Rationale: The user asked for tickets newer than 989; closed tickets already have a recorded resolution, while all five open tickets need current validation.
  Date/Author: 2026-09-14 / Codex

- Decision: Keep unrelated untracked files untouched and stage only files changed by this work.
  Rationale: `.agents/plans/restore-tui-workspaces-and-status.md`, `1q`, and `mj.sqlite3` predate this work and belong to the user or another agent.
  Date/Author: 2026-09-14 / Codex

- Decision: Preserve the exact model and effort values accepted by the source relay and reject an incompatible move rather than translating `opus` to `opus[1m]`.
  Rationale: Those are distinct destination-advertised choices, and silently changing one would recreate the reported pin-loss behavior under a more plausible label.
  Date/Author: 2026-09-14 / Codex

- Decision: Keep Muse's existing missing-report accounting instead of deriving token counts from text length or reporting zero.
  Rationale: Neither ACP nor the native transcript supplies authoritative usage, and an estimate would be indistinguishable from measured billing data in the current schema.
  Date/Author: 2026-09-14 / Codex

## Outcomes & Retrospective

Source fixes are complete and the controller regression suite passes 1,188 tests with six live-only tests ignored. Repository-wide validation, installation, tmux acceptance, issue updates, and the final commit remain in progress.

## Context and Orientation

The controller owns durable session state, target provisioning, API endpoints, model discovery, quota polling, and move/resume behavior. Its code lives in `mj-controller/src/`. The worker launches agent harnesses, translates the Agent Client Protocol (ACP), and records turn events; its code lives in `mj-worker/src/`. Shared configuration and durable state types live in `mj-core/src/`. Managed container contents are defined by `containers/Containerfile.agent-dev`.

An idempotency key is a caller-supplied token that makes retrying session creation return the first session instead of creating a duplicate. A launch follow-up is the asynchronous phase after the API has returned a new session ID: it provisions the target, starts the harness, applies model and effort choices, and submits the initial prompt. A failed launch must remain a durable session result rather than deleting the only observable record.

ZCode uses `zcode-acp-server` as its ACP bridge and the headless `zcode.cjs` backend extracted from the official ZCode AppImage. Its native profile configuration is under `.zcode`, including `.zcode/v2/config.json`. Muse Code currently has one managed model, `muse-spark-1.3-contributor`; model discovery and per-turn usage reporting are separate capabilities and must not be conflated. Claude's ACP session advertises selectable model and effort values, which can differ across installed CLI versions and authenticated profiles.

## Plan of Work

First trace each report to its ownership boundary and capture a regression test at that boundary. For ZCode, inspect the managed container recipe, the controller's profile archive rules, the worker runtime preflight, and actual target staging paths. Install the pinned runtime in the image, include the minimal authenticated profile files needed by the remote backend without copying mutable conversation state, and make a missing runtime fail before ACP startup with a direct diagnostic.

For launch failure durability, move idempotency recording to the point where the session ID becomes committed, before any asynchronous follow-up can fail. Trace controller actions that remove a failed provisional session and instead transition them to the existing error state. Make wait and event streaming observe that terminal record and ensure event streams close rather than await events from a nonexistent relay forever.

For Muse, inspect ACP notifications and durable Muse logs to determine whether token usage exists. Add the managed model to profile discovery even when ACP discovery returns no model option. If no trustworthy token values exist, expose an explicit unavailable reason through the current usage schema instead of inventing zero values.

For ZCode quota rendering, remove the redundant provider-plan suffix while retaining the actual quota windows. For Claude moves, normalize discoveries so local preflight cannot advertise a union that the launched target rejects. Preserve the selected model and effort when the destination advertises them; when it does not, reject the move with a useful incompatibility message instead of resetting to defaults.

After each coherent group passes focused tests, commit it to the current branch. Build the final binaries and use a dedicated tmux server to exercise command-line behavior without stealing the user's terminal. Run the full required Rust validations outside the restricted sandbox. Update every validated GitHub issue with concise reproduction and validation evidence and close it only after its fix is committed.

## Concrete Steps

Run from `/home/jonathan/Projects/hel`:

    rg -n "start_followup|idempotency|launch.*fail|config_options|usage" mj-controller mj-worker mj-core
    cargo test -p brokk-mj-controller <focused-test-name>
    cargo test -p brokk-mj-worker <focused-test-name>
    cargo fmt --all -- --check
    cargo test
    cargo clippy --all-targets -- -D warnings

Every Cargo test invocation must run with elevated permissions because the suite requires loopback TCP and Unix sockets. Live checks must run in tmux and should use isolated configuration/data directories unless the report specifically requires the already-configured live target.

## Validation and Acceptance

Issue 992 is accepted when a managed target containing the new image starts a ZCode session with its authenticated profile, and an old image produces a direct missing-runtime preflight diagnostic before ACP launch. Issue 993 is accepted when an intentionally failed launch remains in `mj sessions`, a repeated idempotency key returns the same ID, `mj wait` returns its error, and `mj events` terminates rather than hanging. Issue 994 is accepted when `mj models --profile muse` reports `muse-spark-1.3-contributor` and usage output distinguishes unsupported native usage from measured zero. Issue 995 is accepted when the TUI quota row contains only the profile, harness, quota bars, and window durations. Issue 996 is accepted when a move either retains a compatible model/effort pair or refuses an incompatible pair without mutating the session.

The final repository acceptance is a clean `cargo fmt --all -- --check`, passing `cargo test`, and passing `cargo clippy --all-targets -- -D warnings`. The working tree may retain only the three known unrelated untracked paths plus committed work.

## Idempotence and Recovery

Focused and full tests are repeatable. Use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` for migrations and live fault injection. Do not modify or delete the live database to reproduce issue 993. Container image builds may be rerun because their pinned downloads and checksums are deterministic. If a live tmux test stalls, capture its pane and daemon diagnostics before terminating only the dedicated test server.

## Artifacts and Notes

Initial tracker state:

    #990 closed: Kimi checkpoint checksum mismatch
    #991 closed: worker restart during checkpointed destroy
    #992 open: ZCode container runtime and profile staging
    #993 open: failed launch durability and idempotency
    #994 open: Muse model discovery and usage
    #995 open: redundant ZCode quota label
    #996 open: Claude model identity and move pin preservation

## Interfaces and Dependencies

Use the existing `ApiBackend` idempotency methods and durable database writer rather than a second ledger. Use current session error/status types and relay events rather than adding a parallel failure protocol. Extend existing profile archive allowlists for ZCode credentials and existing profile configuration discovery for Muse. Continue using pinned managed harness metadata in `mj-core/src/harness_runtime.rs` and checksum-verified downloads in the container recipe. Use current move/resume validation in `mj-controller/src/controller/resume.rs` to preserve or reject requested settings atomically.

Revision 2026-09-14: created after locating the correct Mjolnir tracker and reading issues 992 through 996; records the complete validation and implementation strategy before edits.

Revision 2026-09-14 06:20Z: records source validation, implemented behavior, the Muse usage limitation, live Claude catalogue differences, and the passing controller suite.

Revision 2026-09-14 06:21Z: live image validation exposed and fixed AppImage-extracted backend permissions before rollout.
