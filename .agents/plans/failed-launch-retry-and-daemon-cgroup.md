# Retry a failed first launch, and start the local daemon under the systemd user manager (#1584)

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md`.

## Purpose / Big Picture

Issue #1584 has two parts.

1. On a local rootless Podman target, `podman exec` into a running session container sometimes fails with
   `crun: write file /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-<id>.scope/container/cgroup.procs: Permission denied`.
   After this change the host daemon runs inside a systemd user-manager scope, so every Podman subprocess it starts (provisioning execs, worker staging, relay proxies) begins under `user@<uid>.service`. The failure cannot happen from there.
2. A session whose first launch failed before it ever ran is a dead end today. Resume returns 409 "no checkpoint", and the owner has to archive the record and create a new session by hand. After this change, Resume on such a session (from the TUI, `mj resume`, the HTTP API, or the web viewer) runs the session's create again under the same session id from its stored launch request. The user sees the same session go back to Starting and then Running.

## Progress

- [ ] M1: launch the host daemon under a systemd user scope (mj-cli).
- [x] M2 implementation: failed-first-launch retry through Resume (mj-controller, mj-tui, web viewer).
- [x] Traced startup-step failure settlement. The parent resolved the startup-step policy below.
- [x] M2 targeted API and wait tests, terminal event test, web golden, and four-crate Clippy.
- [x] M2 isolated restart lab, including draft retention, settled prompt dismissal, a second failed retry, and Resume racing Destroy.
- [x] M2 final lab run with the phone action cancellation check.
- [ ] M1 validation and review.

## Surprises & Discoveries

- Observation: the evening failure's conmon (PID 479145) ran in `/user.slice/user-1000.slice/session-c14.scope`, the login scope the daemon inherited, not under `user@1000.service`. Cgroup v2 requires write access on the common ancestor of the source and destination cgroups, and that ancestor is `user-1000.slice`, whose `cgroup.procs` is root-owned with mode 0644.
  Evidence: `.mj/agents/1ba960e61b4835c56268213e25d381ea/findings.md`, `journal.log`. The error reproduces deterministically with direct `crun --systemd-cgroup exec` from the login scope, and the same command under `systemd-run --user --scope` succeeds. Podman 5.7.0 normally moves conmon into a `libpod-conmon-<id>.scope` first. When that placement fails, rootless Podman logs it at INFO and continues (`libpod/oci_conmon_linux.go:175-231`). That intermittent placement failure is what exposes the problem. crun 1.30 fixes this upstream with `AttachProcessesToUnit`, but the host package is crun 1.21.
- Observation: no Mjolnir code changes the cgroup limits of a running container, and the reproduction found no race between exec and `podman update` or sibling container starts.
- Observation: startup-step rows lack a structured distinction between pre-handoff failure and other settled failures.
  Evidence: `daemon/state.rs::wait_for_ready_session` formats Error and StartupCleanup as ordinary errors. Its drain forwards the formatted text to `database/startup.rs::fail_unavailable_startup_groups`, which only updates pending grouped rows, but stores them as the generic `failed` phase. `fail_startup_group` can write the same phase for pending steps for other reasons. An in-flight step can also end in `failed` through `rejecting` without an acceptance ordinal. The only legacy difference is human-readable error text. The readiness failure can mark pending tail steps even when the group's head was already delivering. The Decision Log records the resulting dismissal policy.

## Decision Log

- Decision: fix the cgroup failure by placing the daemon, not by retrying exec or serializing it against container starts.
  Rationale: the failure depends on where the source process sits, and nothing points to a race in Mjolnir. A retry would hide the cause. Placing the daemon fixes every Podman subprocess it starts, including the relay proxies.
  Date/Author: 2026-11-17 / parent session.
- Decision: wrap only the detached host launch in `mj-cli/src/daemon.rs::launch_daemon`, with `systemd-run --user --scope --quiet --collect`, and only when all of these hold: the target is Linux; `/proc/self/cgroup` is not already under `user@<uid>.service`; `systemd-run` is on PATH; and the user manager is reachable (`$XDG_RUNTIME_DIR/systemd/private` exists). Otherwise launch exactly as today.
  Rationale: without a user manager there is no delegated subtree, and Podman then uses its own cgroup handling. `systemd-run --scope` moves its own PID into the scope and then `exec`s the command, so the PID returned by `spawn_detached` is still the daemon's PID, and the environment and process group pass through unchanged. If `systemd-run` fails while the manager is reachable, the daemon does not start, its error lands in the daemon log, and the CLI reports it like any other daemon start failure. The plan does not fall back silently to the login scope.
  Date/Author: 2026-11-17 / parent session.
- Decision: the executable passed to `systemd-run` is `/proc/<client pid>/exe`, not `/proc/self/exe`.
  Rationale: `daemon_launch_executable()` uses `/proc/self/exe` so the daemon executes the client's running inode, which survives upgrade replacement. Inside `systemd-run`, `/proc/self/exe` would name `systemd-run` itself. The client stays alive until the daemon is ready, so its `/proc/<pid>/exe` link is valid when `systemd-run` `exec`s it. Keep `arg0` set to `mj` if `systemd-run` allows it. If it does not, record that here: only argv[1] (`daemon-run`) is read to recognize a daemon.
  Date/Author: 2026-11-17 / parent session.
- Decision: a "failed first launch" is a top-level session (no sub-agent relation) that is not archived, has no Move in progress, has `checkpoint == None` and `native_session_id == None`, and is in `Error` (target cleared after cleanup) or `StartupCleanup` (cleanup still pending). Exactly one predicate in one place answers this. The API capability, the viewer's `has_resume_source`, TUI gating, and the daemon's admission all call it. None of them recomputes it from its own copy of the inputs.
  Rationale: with no checkpoint and no native session id, nothing exists that a resume could restore, so a fresh launch is the only recovery and loses nothing recoverable. The launch retry never re-arms startup input. Legacy session rows qualify on the same durable session evidence.
  Date/Author: 2026-11-17 / parent session.
- Decision: the retry reuses the durable interrupted-create machinery from #1548. Under one Create lifecycle admission for the session, one database transaction revalidates the predicate against the durable row (compare-and-set) and writes `state = Provisioning` plus a fresh create intent with a new operation id and `remember_launch_choices = false`. The existing create body then runs with `recovering = true`. Its `reset_interrupted_create` retires whatever the ledger still owns (process group first, files second) and resets the record to its registered form. A daemon restart mid-retry is handled by `recover_creates`, unchanged.
  Rationale: this keeps one create mechanism. Crash safety comes from the existing intent row. There is no gap between "retry accepted" and "retry durable", because acceptance is reported only after that transaction commits. If the transaction finds the row no longer eligible (a concurrent destroy, archive, or retry), the Resume is refused with the current reason.
  Date/Author: 2026-11-17 / parent session.
- Decision: do not nest admissions. The Resume entry point decides between ordinary resume and launch retry from the durable row. For a launch retry it admits Create, not Resume, and the create provisions without holding handoff admission, as creates do today.
  Date/Author: 2026-11-17 / parent session.

- Decision: dismiss settled failed startup groups in the retry transaction through the same database write as API dismissal. Pending and uncertain groups keep their original identities. The daemon reports that Retry launch uses the stored request and that prompts that failed with the previous launch are not resent. Independently saved terminal drafts remain available.
  Rationale: an explicit retry acknowledges the earlier failure, so `mj wait` follows the new attempt. A new submission requires a new request. The existing rows lack structured pre-handoff failure provenance.
  Date/Author: 2026-11-17 / parent session.
- Decision: publish launch-retry eligibility in the existing runtime feed metadata. The field defaults to empty and the metadata accepts additive fields, so it needs no daemon-client or relay protocol bump.
  Rationale: the viewer and terminal consume the same daemon eligibility projection. Admission and its durable compare-and-set use the same classifier.
  Date/Author: 2026-11-17 / M2 implementer.

## Outcomes & Retrospective

M2 accepts the same session id and stored destination only after the Create intent commits. The interrupted-create recovery path resumes that intent after daemon replacement. The terminal and viewer offer Retry launch without a restore wizard. The isolated run also confirmed that a failed startup prompt is retired, the next prompt runs once, and an unsent terminal draft survives. The isolated lab uses a local bare target and deterministic ACP harness. M1 validation covers the Podman cgroup behavior separately.

## Context and Orientation

Part 1:

- `mj-cli/src/daemon.rs:610-680` `launch_daemon` spawns `mj daemon-run` with `mj_core::subprocess::spawn_detached`. `daemon_launch_executable()` is at line 1420.
- `mj-controller/src/targets/container.rs:681-859` runs Podman. No cgroup flags are set. This plan does not change it.

Part 2 (map: `.mj/agents/64df2a6614f1a8fc1813ac9636d425f9/issue-1584-retry-map.md`):

- `mj-controller/src/server/api/sessions.rs:392-549`: the v1 resume handler, `resume_refusal`, and `no_checkpoint_refusal`.
- `mj-controller/src/server/viewer_types.rs:557-565`: `resume_source_available` / `has_resume_source`. `mj-controller/src/server_runtime/snapshot.rs:263-269`: the runtime resume capability.
- `mj-controller/src/daemon/resume.rs:41-171`: the per-session resume entry, reached from HTTP, TUI IPC (`daemon/session_lifecycle.rs:64`), and legacy actions (`daemon/actions.rs:1149`).
- `mj-controller/src/controller/resume.rs:1636-1694`: the controller resume, which ends in `checkpoint.context("session has no checkpoint")`.
- `mj-controller/src/daemon/create.rs:581-788`: `start_create_lifecycle` and `recover_creates`. `mj-controller/src/controller/provisioning.rs:280-350`: `reset_interrupted_create` / `retire_interrupted_environment`.
- `mj-controller/src/database/session_create.rs`: create intents, `restore_create_intent_for_retry`, and `reset_session_for_create_retry`.
- Startup steps: `mj-controller/src/daemon/startup_followup.rs`, `mj-controller/src/database/startup.rs` (failing and returning to draft at 282-335, pruning at 555-615), and `daemon/state.rs:568-630` `wait_for_ready_session`.
- TUI: `mj-tui/src/dashboard_input.rs:742-786` (RecoverFailed), `mj-tui/src/dialogs/render.rs:1616-1643`, `mj-tui/src/wizards/dashboard/begin.rs:151-167`, `mj-tui/src/resume.rs:2139-2155`, and `mj-cli/src/dashboard/actions.rs:1996-2048`.
- Web: `mj-controller/src/web/viewer.js` resume card (4320, 4973-5194, 10073-10110) and `tests/e2e/web/golden/viewer_resume.txt`.
- Prompt refusal advice: `mj-controller/src/server/api/turns.rs:289-319`.

## Plan of Work

M1. In `launch_daemon`, build the command through a small helper next to it. The helper decides whether to wrap, using the conditions in the Decision Log; the `/proc/self/cgroup` read and the existence checks are non-blocking enough for this already-blocking task. When it wraps, it runs `systemd-run --user --scope --quiet --collect --description="Mjolnir daemon (<instance>)" -- /proc/<client pid>/exe daemon-run`, applies the same environment edits and `env_remove` calls as today, and spawns with `spawn_detached`. Add a short comment that names #1584 and the cgroup-v2 common-ancestor rule. Check that no other host daemon spawn site exists, including the upgrade handoff and `mj daemon restart`. If one does, route it through the same helper.

M2.
1. Add the single predicate, for example `fn failed_first_launch(record, is_subagent) -> bool`. Put it where both the controller and the viewer projection can call it; if the TUI reads it, it must come from the projection or the IPC record, not from a recomputation. Use it in `resume_source_available` and the runtime capability, in the v1 handler and `resume_refusal`, in `turns.rs` refusal advice, and at the daemon resume entry.
2. Add an IMMEDIATE database transaction next to `reset_session_for_create_retry`. Re-read the row, apply the eligibility predicate, verify the placement choices still match, and write `Provisioning` with `last_error = None` plus a new create intent. Dismiss wholly settled failed startup groups before committing. Keep unfinished startup groups, the draft, the target, and the environment ledger in place. The create reset retires the previous environment.
3. At the daemon resume entry, when the durable row satisfies the predicate, take the launch-retry path instead of the ordinary resume. Admit capacity the way registration does, since overcommit only warns. Keep the workspace assignment. Under Create admission, call `begin_failed_launch_retry`, then run the create body with `recovering = true` and the new operation id. Requested profile, target, or workspace overrides that differ from the stored launch are refused with a plain reason: this retry relaunches the stored request. Report acceptance to the caller only after the transaction commits. The HTTP response stays 202 with the same session id and destination.
4. Surfaces:
   - TUI: the RecoverFailed dialog offers "Retry launch" for eligible rows and sends the ordinary Resume request, skipping the checkpoint restore wizard. Wizard gating accepts eligible rows.
   - Web: the resume card treats an eligible row as a retry. It shows "Retry launch" and skips checkpoint conversion preflight.
   - Refusals for ineligible rows keep today's text.
5. A retry that fails again goes back to Error with a fresh launch diagnostic and can be retried again.

## Concrete Steps

Work in your own detached worktree (`git worktree add --detach ~/Projects/mjolnir-wt/<name> HEAD`). Build and test only there. Commit on the detached HEAD and do not push.

Targeted checks:

    cargo test -p brokk-mj-controller --lib -- <your filters>
    cargo clippy -p <crate> --all-targets -- -D warnings

Run tests outside the sandbox, on the dev profile.

## Validation and Acceptance

M1: in an isolated instance (`env -i` launcher with its own MJ_CONFIG_DIR, MJ_DATA_DIR and MJ_INSTANCE, plus the real `XDG_RUNTIME_DIR`), `mj --instance <name> daemon start` and then `cat /proc/<daemon pid>/cgroup` shows a path under `/user.slice/user-<uid>.slice/user@<uid>.service/`. `ps` still shows `daemon-run` as argv[1]. Without `XDG_RUNTIME_DIR`, the daemon starts unwrapped as before. Stop the daemon afterwards. Run a local Podman exec smoke through the isolated daemon if a cached image is available.

M2: in an isolated instance with a deterministic one-shot failure after target reservation and before worker start, a session fails its launch, `mj resume <id>` is accepted, and the same id reaches Running with the same project and options. The startup prompt that failed with the old launch is not resent. `mj wait` reports the retry, a new prompt submitted after retry runs once, and the TUI draft is preserved. Reuse an existing test hook if there is one; otherwise add the smallest test-only one. Also check:

- A previously used session with no checkpoint, an archived row, or a sub-agent still gets today's 409 text.
- A daemon restart during the retry finishes it.
- Concurrent Resume and Destroy result in exactly one of them owning the session.

Tests to add, per AGENTS.md "Which tests to add":

- Extend the API contract test near `mj-controller/src/server/api/tests.rs:5661-5839` (accepted retry for an eligible row; 409 for ineligible ones).
- Update `tests/e2e/web/golden/viewer_resume.txt` if the card changes.
- Add a TUI RecoverFailed event or render test near `mj-tui/src/tests.rs:1855-1921`.
- Add one isolated e2e in the `tests/e2e/create_survives_restart.py` lab that covers the retry and a restart during it.

## Idempotence and Recovery

The retry transaction is a compare-and-set, so a repeated Resume after acceptance joins or is refused. Retirement is idempotent. No migration is added. If one turns out to be necessary, stop and record it here before writing it.

## Interfaces and Dependencies

No new crates. Use `systemd-run` from the host systemd. Use the shared subprocess helpers. No relay protocol change is expected. If the TUI needs a new projected field, add it with a serde default and record whether the protocol revision must rise.
