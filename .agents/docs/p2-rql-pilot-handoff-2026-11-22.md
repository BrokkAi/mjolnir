# P2 handoff: review-tool MVP and the RQL census lean pilot (2026-11-22)

Written by the outgoing P2 lead (session dbcece95f1cdc80785d2df3f995138f2) for its Gemini Neon/xhigh
successor. The plan of record is `.agents/plans/rql-census-lean-pilot.md` (ExecPlan: Progress,
Surprises & Discoveries, Decision Log) and the study plan `.agents/evals/review-arms/brief/PLAN-rql-census.md`.
Read both before acting; this brief points into them and adds the operating detail they lack.

## 1. Mission and priority order

P2 is epic BF-13143's review-tool MVP. Its current deliverable is the **RQL census lean pilot**:
"can a suitably enhanced RQL help brief writers and reviewers catch bugs?" A mock RQL engine
(Claude Sonnet/high, one sealed session per query, checked by a deterministic Rust verifier)
answers queries; Sol/high brief writers use it in arms G, R and R-loc; Terra/high reviewers
review the briefs; Sonnet/high judges score catches. Coordinator: session 5992a2122571ac7c7e303ec069a19314.

Priority order now:
1. **Finish the fidelity gate** (`rql_mock/fidelity_live.py`, full phase). It certifies the engine
   before any writer runs. It is running (section 2).
2. On a passing gate: **run the pilot** (writers then reviewers per diff, on codex3 then codex4),
   then judges and the readout (`pilot_run.py`, `pilot_review.py`, `pilot_judge.py`, `pilot_readout.py`).
3. If pooled prescience passes but an added step misses its own fidelity bar: re-freeze the manifest
   with `--served-steps` minus that step (`brief/rql-census/pilot/select_pilot.py`), as the plan says.
   If **pooled prescience fails, the pilot stops**: the live runner refuses every new run while that
   report exists, for any manifest (study plan, fidelity gate section). Bring it to the user.
4. Bifrost follow-ups from the census are filed; none is assigned (#15167 needs a cross-target design).

## 2. Pilot state and how to operate it (all on host hel)

### State at 2026-11-22T17:05Z
- Gate **full phase running** on commit `d7de33a91` from worktree
  `/home/jonathan/Projects/mjolnir/.mj/clones/dbcece95f1cdc80785d2df3f995138f2/.mj/worktrees/rql-gate-d7de33a91`,
  in systemd user unit `rql-gate-full-d7de33a91-1795364697.service`.
- Engine receipts saved: 55 of 92, all accepted (`status: ok`; no recorded failures yet). Full
  measurements (engine + Terra + adjudication): 3.
  Query 4 (`8d4255915b2d…`, the ~250-candidate panic query) was in its Terra enumeration.
- Quota at last check: claude5 5-hour 99%, week 29% (week resets 2026-11-26 ~07Z); codex3 week 99%
  (resets 2026-11-29).

### Where things live
- Run root: `/mnt/optane/review-arms-pilot/` (hel local disk; not in git).
  - `engine.json`: engine config. `logins` is `[{"profile":"claude5","engine_profile":"claude5-engine"}]`.
    `manifest` and `seal_probe[2]` point into the current gate worktree; each start script repoints them.
  - `bin/mj-3f774fb7`, `bin/mj-worker-3f774fb7`, `bin/pinned.json`: the frozen mj build (2.73.0+3f774fb7),
    re-frozen 2026-11-22 after the drift recorded in the ExecPlan. `bin/rql-verify-7672df81c`: the verifier.
  - `fidelity/88095b963e2819c6157f03b23320568d661ad8eb992b8f1b300f958e716b65b6/896aec04ff2c55ca86d343293ba5fcf607c5ba946d96e7704ca4b9041a9dbd9d/<query_key>/`:
    per-query checkpoints: `engine.json`, `fresh-engine.json`, `terra/`, `adjudicator/`, `measurement.json`.
    Failed attempts are set aside beside them (`terra-failed-<sid>`, `adjudicator-shell*`); keep them as evidence.
  - `fidelity/gate-report.json`: written when the full phase finishes; holds the verdict.
  - `engine-cache/<commit>/{active,retired/<sid>}`: engine session evidence (transcripts, audits, voids).
  - `gate-full/<sha>/`: per-run `run-gate.sh`, `start.sh`, `live.log`, `exit.json`, set-aside `live-N.log`.
  - `engine-trees.git`: the orphan snapshot repo served to containers.
- Private mj instance `rql-census-pilot`: config `~/.config/mjolnir/instances/rql-census-pilot/config.toml`
  (profiles claude5, claude5-engine, codex3; `claude-engine/` holds the engine-only `settings.json` and an
  access-only credential the runner re-seeds every launch); store `~/.local/share/mjolnir/instances/rql-census-pilot/`
  (schema revision 149).
- systemd user units: `rql-pilot-git.service` (git daemon on 127.0.0.1:9438 serving `engine-trees.git`),
  and one `rql-gate-full-<sha>-<ts>.service` per run (`KillMode=process`, so the private mj daemon it
  starts survives its exit). Use `XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user ...`.

### Rules for touching it
- **Reach the pilot instance only through `./mjx`** in `.agents/evals/review-arms/`. It forces the pinned
  3f774fb7 pair for any `rql-census-pilot` call and refuses a different `REVIEW_ARMS_MJ`. Never run the
  installed `mj` against it: on 2026-11-21 that replaced the frozen daemon and migrated the store.
- Never start pilot services from a session's own process tree (`setsid nohup` stays under the worker):
  use `systemd-run --user` as the start scripts do.
- `pgrep -f` matches your own shell's command line; use `ps -eo args | grep "fidelity_liv[e]"`.

### Gate phases and operations
- Full phase command (in `run-gate.sh`): `python3 -B -m rql_mock.fidelity_live --live --terra-profile codex3
  --manifest brief/rql-census/pilot/manifest.json --run-root /mnt/optane/review-arms-pilot`, with
  `REVIEW_ARMS_ENGINE_ROOT`, `REVIEW_ARMS_MJ`, `REVIEW_ARMS_MJ_WORKER` exported to the pinned paths.
- **Start or resume**: `/mnt/optane/review-arms-pilot/gate-full/<sha>/start.sh`. It refuses if a gate runs,
  sets aside the previous `live.log`/`exit.json`, repoints `engine.json`, checks the deployed engine
  settings byte for byte, waits until claude5's report shows both windows above zero, refreshes codex3,
  and launches the unit. Use it after a quota hold (`quota_hold` in `live.log`, exit 1) too.
- **New code**: commit, review, push; `git worktree add --detach .mj/worktrees/rql-gate-<sha> <sha>`; copy
  `run-gate.sh`/`start.sh` from the last run dir with the sha replaced; stop the running gate cooperatively
  with `touch /mnt/optane/review-arms-pilot/fidelity/stop-requested` (it exits 3 at the next query boundary);
  remove the file and run the new `start.sh`. Engine identity must not change, or all 55 answers are void.
- **"an incomplete fidelity launch needs reconciliation at <dir>"**: a Terra or adjudicator checkpoint has a
  session and intent but no completion. **First check for a double Terra stall**: if a `terra-stalled/`
  directory sits beside `terra/` and both hold a `failed.json`, the query has no gold and the gate stopped on
  purpose. The stop message "Terra repair stalled in both sessions" is in the `live.log` of the run that
  stopped, which `start.sh` moves to `live-N.log` on the next start, so check the archived logs too. Do not rename and restart, which would only buy a
  third enumeration; bring it to the coordinator or the user, since how to score such a query is unruled.
  Otherwise, if `<dir>/destroy-receipt.json` exists and no `completed.json`, rename the dir to
  `<name>-failed-<first 8 of session_id>` and restart. If no destroy receipt, the session may be live:
  inspect with `./mjx --instance rql-census-pilot sessions --json` before touching anything.
- Monitoring: in the active checkpoint directory only (the `88095b…/896aec04…` path above; another
  engine identity gets its own directory), count `measurement.json`, and count `engine.json` and
  `fresh-engine.json` by their `status`: `ok` receipts are accepted answers, `engine_error` receipts
  with `failure: "repair_stalled"` are recorded engine failures. Read `gate-full/<sha>/live.log` and
  `exit.json`. An engine query gets two sessions in all, shared between voids and repair stalls. A first
  void or stall is retried in a fresh session. Two stalls are recorded as an engine failure. Any other pair
  (two voids, or a void and a stall) stops the gate with an `engine_error` and saves no receipt.

### What is fixed (all reviewed and pushed) and what to expect
- Engine: no shell (Read/Grep/Glob/Write/Edit only), host-side offsets and site lists, audit fixes for
  location-only writes, tracked-symlink targets, saved tool results (Read and exact-file Grep), Glob
  reachability, Grep `--no-follow` pinned to Claude Code 2.1.326, context-compaction cards; credential
  sync across claude5 refreshes; stalls get one fresh session; repair progress = fewer distinct errors.
- Terra: per-audit Codex rollout capture; model/effort from the rollout `turn_context` when mj usage omits
  effort; prompts carrying mj's `mj://prompt-context` block; repair loop shared with the engine, stages separate;
  a double Terra stall stops the gate (no gold to score).
- Adjudicator: shell-less on the engine's restricted profile and audit.
- Every new session type so far surfaced one gap on first contact; expect more of the same kind. Fix the
  evidence or the check exactly; never loosen the isolation audit without the user's approval.

### After the gate
- Verdict in `fidelity/gate-report.json`: pooled prescience must pass, and per-step bars.
- Writers (Sol/high) and reviewers (Terra/high) run on codex3 then codex4 (week above 25%); they have not run
  on the 3f774fb7 build yet, so run the N/G smoke first (ExecPlan Progress) and confirm the Codex model check
  and rollout capture on that path.

## 3. Rulings and standing rules

User rulings (2026-11-21/22; all in the ExecPlan Decision Log):
- Engine, adjudicator, judges and hand checks on Sonnet/high plus the deterministic verifier (not Fable).
- Writers stay Sol/high (Gemini writers rejected: agy web search can't be disabled, shell cwd unverifiable).
- Engine may use claude5 now, and claude4 and claude7 after their resets; claude5 has **no floor**; other
  Claude logins keep 25% weekly and 20% five-hour; the `claude` login is always refused. Not yet added:
  claude4 (reset ~2026-11-23 05Z) and claude7 (~2026-11-24 13Z), only if pace requires; deployment steps in
  `.mj/agents/8b56d5e7a536a7cfbb3e3df33fc3e92e/deployment.md`.
- A query whose repair stalls in both engine sessions counts as an engine failure: zero rows, against recall,
  reported by name. A quota hold or an infrastructure error stops the gate at once. A void or stall is
  retried once in a fresh session; two voids, or a void and a stall, stop the gate with no receipt.
- Writers' suffix-free in-place edits of their own files pass the audit, narrowly (Decision Log 2026-11-21).

Standing rules:
- Plain language; never give the user raw hex session IDs.
- Commit on master, review (`request_review`), push only after a clean review or `override_review`
  (which needs a clean tree, before committing fixes); after about 3 rounds of minor findings, override and push.
- Loosening the isolation audit needs the user's approval. No paid API. Never pass `--acknowledge-unpublished-work`.
- mbx only for Rust builds; never plain cargo or a redirected target. No Bifrost indexing on hel.
- On hel never copy onto an existing file: write a temp file and rename.
- Don't edit the main `~/.config/mjolnir/config.toml`. Keep the user's email private.
- Test runs for review-arms are Python, outside the sandbox, from `.agents/evals/review-arms`:
  `python3 -B -m pytest -q -p no:cacheprovider test_audit.py rql_mock/engine rql_mock/test_server.py`,
  `python3 -B -m pytest -q -p no:cacheprovider --ignore=carrier_test.py --ignore=rql_mock`,
  `python3 -B -m unittest rql_mock.test_fidelity`.

## 4. Pending user questions owned by P2

None open. The coordinator holds the decision on host access (section 7).

## 5. Children and helpers

All children of session dbcece95 are finished or stopped; none holds unsaved work. **Retire all.** A
successor cannot inherit them; spawn fresh children with the pointers above.

Recently useful (context in their report dirs under `.mj/agents/<id>/`):
- e29e5ab98b09aa6a81a75eb08cfa1bc3: Terra rollout capture, model check, adjudicator, Terra repair. RETIRE.
- 08cdf5e8dded7d057328166db469f6dc: engine-failure counting. RETIRE.
- 8b56d5e7a536a7cfbb3e3df33fc3e92e: multi-login admission (deployment.md). RETIRE.
- 20d084605011b58ce1c787e21ecfb519: credential sync. RETIRE.
- aa4ac248f053279a2a33c3d6aa410b05: audit fixes and replays. RETIRE.
- 376666348c4bc458c9cb2dd35ac6fd3f: Gemini writer map. RETIRE.
- 07417fdedc72ec416fff34c531613df6: instance setup and smoke. RETIRE.
- All other children in `list_agents` (37 completed, 30 stopped in total): RETIRE.

Coordinator-created P2 helpers: 0e5ef7864879365fe336766214dc2649 (#15152) and 1e33203c4d4f4fcf8793a95b5dc9a1c9
(#15120), both done: RETIRE. cde7c78577262c6f6dbe0ecab1e642c9 was moved to P1 by the coordinator.

## 6. State not in git that must survive

- Everything under `/mnt/optane/review-arms-pilot/` (checkpoints, engine cache, bins, pin, engine.json,
  snapshot repo, run dirs). Losing the fidelity checkpoints voids the gate's progress.
- `~/.config/mjolnir/instances/rql-census-pilot/` and `~/.local/share/mjolnir/instances/rql-census-pilot/`.
- The systemd user units on hel (running gate, git listener).
- Gate worktree `.mj/worktrees/rql-gate-d7de33a91` in this clone (the gate runs from it).
- Report dirs cited above under `/home/jonathan/Projects/mjolnir/.mj/clones/dbcece95f1cdc80785d2df3f995138f2/.mj/agents/`.
- Census measurement archive `/mnt/optane/review-arms-pilot/bifrost-today/raw.zip` (sha256 in `brief/rql-census/bifrost-today/PROVENANCE.md`).

## 7. Host access

The pilot is bound to hel: the gate and git listener are systemd user units on hel, the engine, Terra and
adjudicator sessions are Podman containers on hel's podman target created by the private daemon on hel, and
all state is on hel's `/mnt/optane`. Operating it needs, on hel as user jonathan:
- read files under `/mnt/optane/review-arms-pilot/` (monitoring);
- run `start.sh`, `touch`/`rm` the stop file, and rename failed checkpoints (control);
- run `./mjx --instance rql-census-pilot ...` from a worktree on hel (quota, sessions);
- `git worktree add` in a clone on hel for each code change, and run the Python test suites there;
- `XDG_RUNTIME_DIR=/run/user/$(id -u) systemctl --user` for unit status.

From a container on another host this works over **ssh to hel as jonathan**, if the container has an ssh
key accepted by hel and the successor's command reviewer allows ssh. Everything above is plain shell; the
login session's user bus needs `XDG_RUNTIME_DIR` set (lingering is on). It needs no localhost session as
such. What it does need is that every write to the pilot happens on hel, and that no pilot service is
started inside the successor's own process tree. A localhost session on hel is simplest. Over ssh, the
successor must also keep code edits and test runs in a clone on hel, because the gate runs from a worktree
of that clone. Children the successor spawns in its own container cannot reach hel without the same ssh access.
