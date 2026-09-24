# Handoff: launch verification campaign (2026-09-23/24)

For the next coordinator (Fable). Read this first, then `.agents/docs/launch-verification-2026-09-23.md` (the runbook: tracks, evidence standard, Run 1/Run 2 records, fix-wave record).

## Standing rules from the user

- **At most one subagent running at a time, until the user lifts it.** Count resumed agents too. (Quota.)
- Fable designs, triages and reviews; implementation goes to Opus/Sonnet subagents in isolated worktrees; the coordinator cherry-picks onto `master`, runs the affected crates' `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all -- --check`, then pushes.
- Authorized to file and fix issues as found, commit on `master`, and push (the user asked to keep master current).
- Out of scope: `morannon-podman` (de-risked); macOS (issue #1135, user drives it on a MacBook).
- Never touch the user's default Mjolnir instance or `target/` for evidence (mbx evicts `target/`). Campaign root: `/home/jonathan/mj-campaign/launch-2026-09-23/` (`bin/` = original campaign build `8b7b1120`; `bin-fixed/` = current fixed build; `evidence/` = all captures and findings).

## State right now (updated 2026-09-24 morning)

- `master` = `origin/master` = `d501dcf9` (merge of origin into the 13 campaign commits; conflicts were in README, acp-agent.md, cli-reference.md, dashboard/actions.rs, tests/acp.rs, resume.rs, plus one semantic conflict in dialogs.rs where origin added `delete_branch_available` to `ForceDestroy`). Validated: default-member `cargo test` (42 binaries, 0 failures), clippy, fmt, web e2e (92 passed, 3 skipped). Pushed.
- Step 2 (macOS CI test) is done: origin's `e8afa5c9` already asserts `invalid peer certificate`; no user "go" was needed.
- `bin-fixed/` rebuilt from `d501dcf9`: `mj` `dc00467d`, `mj-worker` `2463f5c0`, musl worker `328b7498` (`bin-fixed/SHA256SUMS-d501dcf9.txt`).
- #1136 closed by its commit; `agent-in-progress` removed.
- R2 (F/G/H) done: `evidence/luna-manual-seed-3401-3191736/reverify-2/notes.md` (mission text: `evidence/reverify-2-mission.md`). Fix wave R2-A landed in full (R2-6, F-7, R2-5, R2-14, R2-4, R2-3, R2-9, R2-8, R2-10, R2-7; `c69618f4..aa89c000`), validated (controller, cli, core, tui tests; clippy; fmt), pushed; master = origin = `526ab788`. `bin-fixed/` rebuilt from `dd9e55ce` after R2-B (`SHA256SUMS-dd9e55ce.txt`; all three stamped `2.20.0+dd9e55ce…`).
- Fix wave R2-B landed (R2-2 `5f732831`, R2-1 `5dafd135`), validated, pushed; master = origin = `dd9e55ce`. R2-11 cause confirmed, fix deferred (see the runbook's R2 fix waves paragraph and the open list). Its worktree `agent-ab36fc8a205e13311` is still locked by the agent; remove it when unlocked. #1144 filed for the suspend handler's `Controller::load()` and the test that reads the real store.
- Stale agent worktrees and branches from the 09-23 fix waves were pruned on 09-24 at the user's request. The J-25 work in progress from the Track J worktree is saved as `evidence/track-j/j25-codex-quota-error-wip.patch`. Branch `worktree-agent-aa0dbae3ef12d579e` (pre-campaign "WIP: partial ACP 2.0 upgrade") was left for the user.
- R3 (Track J on EC2 plus the group-16 behaviors) is running as the one subagent against the `dd9e55ce` binaries; if it is lost, its EC2 host is tagged `mj-ssh-docker-run` and its ledger is under `evidence/reverify-3/ec2` with a backup in `evidence/reverify-3/ledger-backup`. R4 is next. Mission texts: `evidence/reverify-3-mission.md` and `evidence/reverify-4-mission.md`.

## Next steps, in order

1. ~~Merge origin/master~~ done (`d501dcf9`).
2. ~~macOS CI test fix~~ landed upstream in `e8afa5c9`.
3. ~~Rebuild `bin-fixed/`~~ done.
4. ~~R2 re-verification~~ done; cherry-pick fix waves R2-A and R2-B, validate, push, rebuild `bin-fixed/`. Original: (one subagent): re-run the reproductions of every fixed F, G, H finding against `bin-fixed`. Findings files: `evidence/luna-manual-seed-3206-340978/track-f/findings.md`, `…3207-341010/track-g/findings.md`, `…3208-2579821/track-h/findings.md`. Model the prompt on R1 (results in `evidence/luna-manual-seed-3301-2541214/reverify-1/notes.md`). Note for Track G: the lab needs `phone_tls=True` for a QR URL; Web dialog is `prefix+u`. For H, download old releases into `evidence/releases/`, never `target/`.
5. **R3 re-verification** for J (needs one EC2 host; `tests/e2e/ssh_docker_lab.py`, back up the ledger right after `create`, cleanup by run tag if lost) and the new-in-group-16 behaviors (mandatory `--workspace`, Jev switch off).
6. **Real-harness re-check** (one subagent, isolated instance with `[phone] bind` on its own port, `version = 13`): the items flagged "needs live check": I1-3 `/model claude-opus-5-5`, I1-6 same-profile resume keeps model, I1-11/I1-13 `/clear` incl. after resume, I1-15 review failure reported, I1-17 interrupted marker, I2-7 never-prompted Codex resume, I2-15 Kimi Esc closes permission form, I2-5 command in permission form, #1136 Kimi on a container target, J-19/J-17/J-21 on SSH.
7. **Close the runbook**: fill the Run 2 section's I2 line and the re-verification results, list what stays open, commit, push. Close #1136 if its commit didn't (`Fixes #1136`), remove `agent-in-progress`.

## Still open (not fixed; decide or schedule)

- I2-1 Codex session title comes from the injected project-memory block (design choice: move the block or prefer Mjolnir's title).
- I2-10 review "Preparing reviewer…" before discovering no files changed (capture-first refactor breaks six host tests' order).
- I2-14 Muse question delayed ~3 min (needs logs for the window).
- J-24 container mount source not validated; J-25 Codex quota error shown raw (partial work: `evidence/track-j/j25-codex-quota-error-wip.patch`). J-22 docs done (`5bf82223`).
- R2-11: destroy does not index the session first, so one created and destroyed between sync passes is never findable. Proposed `wiki().sync_now(false)` before the destroy lifecycle; needs a decision on bounding the wait behind a running first index build.
- R2-12 (open 2.19 `mj wait` fails with 503 during an upgrade; accepted trade-off of the H-2 fix), R2-13 (viewer `/api/snapshot` bursts; harm unknown).
- Resume's rollback (`controller/resume.rs`) has the same cleanup-before-save order R2-2 fixed in provisioning; `mj resume` on a failed never-prompted launch is accepted then fails in the background with "session has no checkpoint".
- A-4/E-9 dictation chord gives no feedback without a microphone.
- D-14 narrow pinned pane not following new replies after restart (may be covered by `bf09b3d6`; re-verify).
- B-11 Enter half of the workspace dialog issue still reproduces (R1 capture 015).
- Flaky under full parallel load (pass alone): `worker_environment.rs` re-exec test, controller `web_viewer::tests::retry_uses_the_original_port_after_its_owner_releases_it`, `mj-cli/tests/store_divergence.rs`, and (seen 09-24 after merging `69e2f7a6`) controller `move_session::tests::terminal_move_recovery_finishes_interrupted_close_before_phase_retry` and `in_place_move_recovery_after_restart_during_swap_rolls_back_to_stopped`.
- Since origin's `51dccacd` (#1138) the controller refuses a worker whose `version+commit` stamp differs from its own. Every `bin-fixed/` rebuild must build `mj`, `mj-worker` and the musl worker from the same commit in one go, and the old-release labs (Track H) are unaffected because they use their own workers.

## Useful mechanics

- Conflict helper for "both sides appended tests": `/tmp/claude-1000/-home-jonathan-Projects-mjolnir/54c16526-8fdf-497a-93bb-dd889fe4a87f/scratchpad/join_conflict.py <file>` (joins HEAD block then incoming block). Use only on test files; resolve code conflicts by hand — it produced a broken `api_client.rs` once.
- Subagent worktrees often start from a stale base (`18eeb2f0`); tell every agent to branch from master's tip.
- The harness often refuses subagents' `findings.md` writes; have them return findings text and write the file yourself.
- Lab cleanup: `mj daemon stop` leaves workers running and they carry no `MJ_DATA_DIR`; kill by exact PID matching the lab runtime path in `/proc/<pid>/cmdline`.
- Memory notes for this project live in `~/.claude3/projects/-home-jonathan-Projects-mjolnir/memory/` (campaign scope, lab mechanics, the subagent cap rule).
