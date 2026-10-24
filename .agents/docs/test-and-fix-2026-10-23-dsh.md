# Test-and-fix campaign, 2026-10-23: the DSH harness

Agent-facing runbook for a live pass over the DSH harness: BrokkAi's
`dsh-acp` 0.2.0 run as `kind = "dsh"` (commits `1bc3cfe43` and `b7e73c46a`).
It follows `.agents/docs/test-and-fix-2026-09-29.md` (the *09-29 runbook*)
and its 09-30 successor: the coordinator (Opus) builds binaries and writes one
card per track; **Sonnet** workers test live in private tmux servers and
record findings; **workers do not fix anything**. The coordinator triages,
dispatches fixes, validates, and pushes each validated fix. Fixes that belong
in dsh-acp go to `~/Projects/dsh-acp`.

Differences from 09-30, set by the user on 2026-10-23:

- Every track uses the real DeepSeek API through the user's
  `DEEPSEEK_API_KEY`. The coordinator copies that one line into each track's
  instance `secrets.toml`; workers never read, print, or copy it. Use
  `deepseek-flash` unless a mission needs `deepseek-v4-pro`, and keep prompts
  short.
- The coordinator asks the user before a change that might affect Claude Code
  or Codex sessions, and when the right fix is not clear.

## Terms and mechanics

Everything in the 09-29 runbook's *Terms*, *Prepare*, *Evidence standard*
and *Cleanup* sections applies, with these values:

| Item | Value |
| --- | --- |
| campaign root `$MJ_CAMPAIGN` | `/home/jonathan/mj-campaign/dsh-2026-10-23` |
| campaign commit | `12c7fe87c` (2.54.0 + 1) |
| host `mj` | `$MJ_CAMPAIGN/bin/debug/mj` |
| host worker | `$MJ_CAMPAIGN/bin/debug/mj-worker` |
| portable (musl) worker | `$MJ_CAMPAIGN/bin/x86_64-unknown-linux-musl/debug/mj-worker` |
| checksums | `$MJ_CAMPAIGN/bin/SHA256SUMS-12c7fe87c.txt` |
| tmux server | `mjd-<track>` (never the user's server) |
| named instance | `dsh-<track>` (lower case) |
| viewer port | `385<n>`: A 3851, B 3852, C 3853, D 3854, E 3855 |
| evidence | `$MJ_CAMPAIGN/evidence/<track>/` |
| DSH profile home | `$MJ_CAMPAIGN/evidence/<track>/home-dsh` (holds a canary `AGENTS.md`) |

The repository for helper scripts (`tests/e2e/…`,
`tests/e2e/web/node_modules/playwright`) is
`/home/jonathan/Projects/mjolnir/.mj/clones/a1a8e5e43252117ce45e6f0aef174c07`.
Never read, write, or build under `target/`; never rebuild. dsh-acp's source
is `~/Projects/dsh-acp` (read-only for workers).

The user's own Mjolnir runs on the **default instance** with real sessions.
Nothing in this campaign may touch it: no `mj` command without
`MJ_INSTANCE=dsh-<track>` in that shell, no `--all-instances`, no `pkill` by
name, no `podman rm`/`kill` of a container you did not create (match the
session id your instance created), no edits to `~/.config/mjolnir/config.toml`.
This worker shell runs inside a Mjolnir session, so `MJ_CONFIG_DIR`,
`MJ_DATA_DIR` and `MJ_SESSION_ID` are set and override `--instance`: begin
every shell with
`unset MJ_CONFIG_DIR MJ_DATA_DIR MJ_SESSION_ID MJ_SESSION_BASH_ENV; export MJ_INSTANCE=dsh-<track>`.
Put `$MJ_CAMPAIGN/bin/debug` first on `PATH`.

**Instance config.** The coordinator pre-created
`~/.config/mjolnir/instances/dsh-<track>/config.toml` (`version = 18`, the
viewer port, a `[profiles.dsh]` that reads the key from `secrets.toml`,
`[targets.localhost] kind = "bare"`, and disabled copies of the profiles the
dashboard discovers in the user's real homes). The dashboard adds a profile
for every installed agent at each launch; a removed one comes back, so leave
those disabled entries alone and never enable them. Add only what your card
names, record each change in `notes.md`, and never copy a token, cookie, or
the daemon's `api-token`.

**tmux.** `tmux -L mjd-<track> -f /dev/null new-session -d -s v -n mj -x 160 -y 48`,
a second window `shell`; drive only with `send-keys`, read only with
`capture-pane -p`. The prefix is `ctrl+b`. An interactive launch plays a short
splash; wait 3 s before sending keys. Send text with `send-keys -l` and Enter
separately. Start every dashboard with `BROWSER=<evidence>/browser-stub.sh`
(a script that appends `$1` to `browser-opened.log` and exits 0).

**Creating a DSH session in the dashboard.** `ctrl+b c` opens the wizard:
target (`localhost` first), the project directory (prefilled with the directory
`mj` started in; `ctrl+u` clears it), then review with a DSH raw-target
DANGER notice and Create. Session states and the composer: the model chip
(`deepseek-flash ▾`) sits in the composer title; `/` lists commands (`/model`,
`/effort`, `/plan <message>`, `/implement <instruction>`, `/attach`, …); `Esc`
interrupts a running turn. A DSH permission request appears as a form above
the composer with Allow once / Allow for this session / Deny.

**Scratch project.** Each track creates its own under its evidence directory:
`git init -b main`, three small commits, `README.md`, two small Python files.
Container targets need a network `origin`: serve a bare clone with
`git daemon --reuseaddr --base-path=<evidence>/repos --export-all --listen=127.0.0.1 --port=<port>`
and point `origin` at `git://127.0.0.1:<port>/project.git`; a rootless
container reaches the host's loopback as `host.containers.internal`, so give
the target
`environment = { GIT_CONFIG_COUNT = "1", GIT_CONFIG_KEY_0 = "url.git://host.containers.internal:<port>/.insteadOf", GIT_CONFIG_VALUE_0 = "git://127.0.0.1:<port>/" }`
and use a bundle (see `~/.config/mjolnir/instances/fix-1319/config.toml` for a
working shape).

**Where things live.** Instance data:
`~/.local/share/mjolnir/instances/dsh-<track>/`. Per session:
`workers/<session>/` holds `worker.log`, `launch.json`, the staged profile
`profile/` (its `AGENTS.md`, and dsh-acp's native records
`profile/sessions/dsh-<uuid>.json`, whose `config` holds `model`, `mode`,
`permission_mode`), and `relay-journal/active.jsonl` (every ACP update the
worker saw, one JSON object per line; `tool_call`, `elicitation_requested`
and `turn_assessment_updated` are useful). Checkpoints are
`sessions/<session>-*.hel.zip`. Logs: `logs/mj-daemon-*.log`,
`logs/mj-tui-*.log`. The managed dsh-acp install is under
`~/.cache/mjolnir/harnesses/dsh/`; that cache is shared with the user's own
instance, so never delete or modify it (Track D's card says how to test an
install).

**Findings** are `<track>-<n>` entries in `<evidence>/findings.md` with the
09-29 fields and severities (*blocks*, *wrong*, *rough*, *docs*). Record
first, never fix, never work around. A runbook error is not a product
finding. Name whether the fault looks like Mjolnir's or dsh-acp's and cite the
evidence (journal line, log line, source line). Write your final message as:
the missions table, every finding in full, runbook errors, cleanup record
(processes left: 0; containers left: 0; daemon stopped; instance sessions
destroyed).

**Known before the campaign** (coordinator's own pass on instance `dsh-t1`;
do not re-file, but do record new evidence for them):

- K1 (dsh-acp): a `run_command` title joins argv without quoting
  (`Run /bin/sh -c ls -la && touch x`), and the permission form shows it as is.
- K2 (dsh-acp): an edit's permission form says only `edit file calc.py`; no
  diff is sent.
- K3 (dsh-acp): "Allow for this session" approves the whole tool, so one
  approved `ls` allows every later command.
- K4 (dsh-acp): in plan mode the model is not told it is planning and still
  gets write, edit, and command tools; the permission form appears before the
  plan-mode refusal.
- K5 (Mjolnir): each command shows two tool cards (dsh-acp's and Mjolnir's
  terminal).
- K6 (Mjolnir): `mj resume --target podman` on a project with no network
  origin prints "resuming" and exits 0; the daemon's failure is never reported.

## Commit classification

- `1bc3cfe43`: the harness. Identity and auth (`mj-core/src/config/harness.rs`:
  `api_key_variable`, `launch_owned_environment`, execution enforcement through
  the ACP `permission_mode` option), runtime pin and launch script
  (`mj-core/src/harness_runtime.rs`: `dsh_acp_script`), managed install on
  bare targets (`mj-worker/src/worker_runtime/harness.rs`: `install_dsh`,
  `mj-worker/assets/dsh/runtime.json`), container launch with the image's
  `dsh-acp` or `npx -y @brokkai/dsh-acp@0.2.0`
  (`mj-controller/src/controller/worker_binary/harness.rs`), checkpoint
  capture and `cwd` rewrite on restore (`mj-checkpoint`), migration 92,
  quota "API", DeepSeek utility inference when the base URL is
  `api.deepseek.com` (`mj-controller/src/utility_llm.rs`: used by handoff,
  project-memory merge, review selection), import and SessionWiki refusals,
  plan mode mapped to dsh-acp's `mode` `plan`/`agent`
  (`mj-core/src/acp/surface.rs`), setup/doctor, docs.
- `b7e73c46a`: staged `AGENTS.md` over 131049 bytes refused by name; docs on
  where the raw-target warning shows.

## Mission cards

Do the **Recent** probes first, then the **Surface** tour.

### Track A — dashboard conversation on localhost (instance `dsh-a`, port 3851, server `mjd-a`)

Recent. Create a DSH session on `localhost` from the dashboard. Capture the
review page's DANGER notice. **Instructions**: ask "what is the canary word?"
and confirm the answer comes from the profile `AGENTS.md`; then edit the
profile home's `AGENTS.md` (change the canary), suspend and resume, and confirm
the resumed session answers with the new word (the launch rebuilds
`DSH_ACP_SYSTEM_PROMPT`). **Permissions**: drive each answer — Allow once,
Allow for this session, Deny, and the form's Cancel — on a command, a file
write, and an edit; confirm what the agent is told and what the conversation
shows (journal `elicitation_resolved`, `tool_call_update`). Answer a
permission with `Esc` and with `Tab` navigation. Interrupt (`Esc` in the
composer) **while a permission form is open**: the form must close, the turn
end as interrupted, and no command run. **Plan mode**: `/plan <message>`,
confirm `PLAN MODE` in the composer and the native record's `mode` is `plan`;
`/implement <instruction>` returns to `agent`; find and use any other plan
toggle the dashboard offers (help overlay `ctrl+b ?`, palette `ctrl+b :`).
**Models**: open the composer's model chip (by click: SGR mouse bytes) and by
`/model`; switch to `deepseek-v4-pro` and back; confirm the native record's
`model` follows; send a prompt after each switch. `/effort high` must refuse
clearly. **Images**: `/attach` a small PNG with `deepseek-flash` (should work)
and with `deepseek-v4-pro` (dsh-acp says only flash takes images; record how
the refusal reaches the user). **Agent questions**: ask the agent to use its
`ask_user` tool to ask you to choose between two names; answer it, then repeat
and cancel it. **Plans**: ask it to make a three-step plan with its
`update_plan` tool and confirm the dashboard shows the plan. **MCP**: ask it to
use the `mj-memory` `search_sessions` tool; record the permission form text
for an MCP tool and the result. **Streaming and queueing**: while a turn runs,
type and send a follow-up; record whether it queues or steers, and that it
runs once. **Large output**: have it run a command printing more than 200 KB
(`python3 -c "print('x'*250000)"`) and confirm the dashboard stays responsive,
the turn ends, and the agent gets a truncated result. **Unicode**: a prompt
with emoji and CJK; a file name with spaces.

Surface. The Import tab (`ctrl+b g`, then `→`) with a DSH profile configured:
record what it lists or reports for DSH (the code refuses to enumerate DSH
sessions; an error banner on every open would be *rough*). Rename
(`ctrl+b shift+t`), `ctrl+b d` changed files, transcript toggle `ctrl+b t`,
the Profiles pane's DSH row ("API"), 100 and 80 columns.

### Track B — lifecycle and durability on localhost (instance `dsh-b`, port 3852, server `mjd-b`)

Use the CLI (`mj new --workspace <ws> --profile dsh --target localhost
--project-directory <project> "<prompt>"`, `mj prompt`, `mj wait`,
`mj suspend --session <id> --acknowledge-unpublished-work`, `mj resume`,
`mj checkpoint`, `mj destroy`) with a dashboard attached in tmux to watch.

Recent. **Memory across resume**: tell the session a fact ("remember the
number 7319"), suspend, `mj daemon restart`, resume, ask for the number; the
native record must be the same file (same `dsh-<uuid>`), and the model must
answer from history. **Permission policy is forced on open**: on a live
localhost session run `mj set-config --session <id> --key permission_mode
--value allow` (and the same for `deny`); record whether Mjolnir accepts it,
what the native record's `permission_mode` becomes, and whether a command
then runs without a form. Then suspend and resume: the record must say `ask`
again and a command must ask permission. Also check
whether "Allow for this session" approvals survive suspend/resume (they live
in dsh-acp memory; record what happens). **Daemon restart mid-turn**: start a
60 s command (`python3 -c "import time; time.sleep(60); print('ok')"`, allow
it), run `mj daemon restart` during it; one completion, one reply,
`mj events --session <id>` shows one `turn_ended`. **Daemon restart while a
permission form is open**: the form must survive or be withdrawn honestly;
answering after the restart must reach dsh-acp (or the dashboard must say it
cannot). **Harness death**: during a turn, kill the `dsh-acp` process by
exact PID (find it as a child of the session's worker; check its environment
names your instance) and record what the dashboard and `mj sessions` show,
whether the worker restarts dsh-acp, and whether the next prompt works with
history intact. **Interrupt fidelity**: `mj interrupt-turn` during a long
command; the command dies (check by PID), the turn says cancelled. **Routine
checkpoint**: `mj checkpoint --session <id>` during an idle session; list the
zip (`unzip -l`): it holds `native/sessions/dsh-<uuid>.json` and no
`.dsh-*.tmp` files. **Restore at another path**: `mj move --session <id>
--target localhost2` where `[targets.localhost2] kind = "bare"` (add it) or
any Move that changes the workspace path; after it, the native record's `cwd`
names the new checkout and a prompt that reads a file works (dsh-acp refuses a
`session/load` whose cwd differs). **Destroy**: destroy a live and a suspended
session; no dsh-acp or worker process survives (by PID).

Surface. `mj transcript`, `mj usage --session <id>` (DSH usage is recorded as
unspecified scope and excluded from totals: confirm the wording is clear),
`mj diff`, `mj export --patch`, `mj events` after a normal, an interrupted and
a failed turn.

### Track C — containers (instance `dsh-c`, port 3853, server `mjd-c`)

Mechanics. Add `[targets.podman] kind = "podman"` (image
`ghcr.io/brokkai/mjolnir/agent-dev:latest`, already pulled; it carries
`/usr/local/bin/dsh-acp` 0.2.0) with the git `environment` above, a bundle
for your scratch project served by your own `git daemon` (pick a port in
39500–39599), and keep `[targets.localhost]`. The musl worker beside `mj` is
selected for containers; record the "worker source" log line.

Recent. **Ambient launch and policy**: create a DSH session on `podman`;
confirm the launch used the image's `dsh-acp` (worker log / launch script),
the native record's `permission_mode` is `allow`, and a command and an edit
run **without** a permission form. **Instructions in a container**: the canary
answer, and the container's disposable-environment note in the staged
`AGENTS.md`. **Move podman → localhost**: `mj move --session <id> --target
localhost --yes` (or the dashboard Move): after it, `permission_mode` is
`ask`, a command asks permission, history survives, and the record's `cwd`
is the new checkout. **Move localhost → podman**: the reverse; `allow` again.
**npx fallback**: add a target whose image has Node.js but no dsh-acp, e.g.
`[targets.podman-node] kind = "podman"` with `image = "docker.io/library/node:22-bookworm"`
(pull it first; if the worker cannot run in it, record that and try
`node:22`); create a session and confirm it starts through
`npx -y @brokkai/dsh-acp@0.2.0` and answers a prompt. **No runtime**: a
target whose image has neither (e.g. `docker.io/library/debian:bookworm-slim`)
must fail with the message "Mjolnir needs dsh-acp or Node.js, npm, and npx on
PATH…" visible to the user (dashboard and `mj sessions`), not an obscure exit.
**Suspend/resume in a container** with the memory fact as on Track B.
Remove every container your instance created at the end (match session ids).

Surface. Daemon restart with a live container session; `mj checkpoint` of a
container session; the dashboard's container rows.

### Track D — configuration, authentication, install, limits (instance `dsh-d`, port 3854, server `mjd-d`)

Each probe edits your instance config or profile home, records the exact
change, and restores it.

Recent. **No key**: remove the `environment` table from `[profiles.dsh]`
(with no `DEEPSEEK_API_KEY` in the daemon's environment: start it from a shell
where it is unset): `mj doctor`, the dashboard's new-session wizard, and
`mj new` must refuse with the message that names `DEEPSEEK_API_KEY` and where
to set it. **Bad key**: set `DEEPSEEK_API_KEY = "sk-invalid"` directly (a
plain value, which `mj doctor` should warn about): a prompt must fail with an
authentication message the user can act on, not a hang or a generic error.
**Unreachable endpoint**: `DEEPSEEK_BASE_URL = "http://127.0.0.1:9"`: a prompt
fails promptly with a clear message; the session stays usable after the base
URL is fixed and the session resumed. **Reserved variables**: add
`DSH_ACP_PERMISSION_MODE = "allow"`, then `DSH_ACP_SYSTEM_PROMPT = "x"`, then
`DSH_ACP_DATA_DIR = "/tmp/x"` to the profile environment: each must make the
config fail to load with a message naming the variable and what to do instead
(record whether `DSH_ACP_DATA_DIR` is refused like the other two; the docs say
Mjolnir owns it). **Instructions limit**: write a profile `AGENTS.md` of
exactly 131049 bytes minus the size of Mjolnir's appended guidance (find it
from a staged `AGENTS.md`), and one over the limit: the large one must refuse
with the size, the limit, and what to shorten, shown to the user in the
dashboard and `mj sessions`; the small one starts. Also a non-UTF-8 byte and a
file with only whitespace. **Missing home**: point `home` at a directory that
does not exist: `mj doctor` says to create it; the wizard and `mj new`
refuse clearly. **Settings editor**: add a DSH profile through Settings →
Profiles (record whether `dsh` is offered, what fields it asks for, and
whether the saved TOML matches `docs/src/content/docs/profiles.md`).
**`mj login --profile dsh`** refuses as documented. **`mj models --profile
dsh`** lists `deepseek-flash` and `deepseek-v4-pro`. **Profile models
variables**: `DEEPSEEK_MODEL = "deepseek-v4-pro"` makes new sessions start on
it; `DSH_ACP_MODELS = "deepseek-flash"` limits the chip's list. **Managed
install**: do not touch `~/.cache/mjolnir/harnesses/`; instead run the worker
against a private cache by starting your daemon with `HOME` unchanged but
`XDG_CACHE_HOME=<evidence>/cache` exported (check in the worker log that the
install went there; if it did not, record "not exercised" rather than
touching the shared cache). Then corrupt the cached `dsh-acp` in that private
cache (append a byte) and start a new session: record whether the install is
re-verified, replaced, or used as is. **Import and SessionWiki**: `mj import`
of a DSH native record (copy one from a worker's `profile/sessions/` into the
profile home's `sessions/`) refuses with "cannot import DSH sessions yet";
record the exact UX in the dashboard Import tab and `mj resume --wiki`.
**Utility model**: with only the DSH profile enabled, trigger utility work —
a turn review (`mj review --session <id>`, and Settings → Review), a Move
handoff summary — and record whether DeepSeek serves it and what the user
sees; then set `DEEPSEEK_BASE_URL` to a non-DeepSeek host and confirm
utility work is not sent there.

Surface. `mj doctor` and `mj doctor --json` DSH lines; `mj setup`;
`docs/src/content/docs/profiles.md`, `targets.md`, `security.md`, and
`cli-reference.md` against what you saw; the mj skill's
`mj-core/assets/skills/mj/references/configuration.md` DSH text.

### Track E — web viewer, CLI answers, and `mj acp` (instance `dsh-e`, port 3855, server `mjd-e`)

Mechanics. Open the viewer from the dashboard's Web dialog (`ctrl+b u`):
address `https://127.0.0.1:3855`, six-digit code. Drive Chromium with Node
scripts that `require('<repo>/tests/e2e/web/node_modules/playwright')` with
`ignoreHTTPSErrors`; register `page.on('dialog')` before lifecycle clicks.
Evidence is accessibility snapshots, console messages and request status;
screenshot only for layout.

Recent. **New session from the viewer** with the DSH profile on localhost:
record whether any raw-target warning appears (the docs say only the
dashboard warns; confirm, and record the wording of the docs). **Permission
in the viewer**: a command's permission form, Allow once, Deny; the same form
answered from the dashboard while the viewer shows it (both converge).
**Plan mode and model** from the viewer's controls. **CLI answers**: while a
DSH permission is pending, `mj elicitations --session <id>` lists it and
`mj respond …` answers it (allow and deny); the dashboard and viewer update.
**`mj prompt --wait`** on a DSH session that will ask permission: what does
the CLI do while the form is pending? **`mj new` raw-target warning**: record
that `mj new --profile dsh --target localhost` gives no warning (documented)
and whether `--help` or the output mentions it. **`mj acp`**: run
`mj acp --workspace <ws>` with the DSH profile from a small Python ACP client
(initialize, session/new, one prompt, read updates, session/cancel during a
command) and record whether DSH sessions work through it, including the
permission request's round trip.

Surface. Viewer conversation rendering of DSH tool cards, thoughts, plans and
usage at 1280×800 and 390×844; the quota page's DSH row; console free of
first-party errors.

## Cleanup

Per the 09-29 runbook: destroy every session your instance created, stop your
daemon (`mj daemon stop`), kill your tmux server, stop your `git daemon`,
remove containers your instance created (by session id), and confirm with
`ps` that no process whose environment names your instance survives. Leave
`~/.config/mjolnir/instances/dsh-<track>/` and your evidence in place.

## Outcomes

Five Sonnet workers ran tracks A–E on 2026-10-23 against `12c7fe87c`, using
the real DeepSeek API. Evidence is under `$MJ_CAMPAIGN/evidence/<track>/`
(`notes.md`, `findings.md`, numbered captures). Every track finished its
cleanup with no surviving processes or containers.

What worked throughout: staged instructions and Mjolnir's guidance reach the
model; the three permission answers, Cancel, and interrupt with a form open;
`/model`, the model chip, `/plan` and `/implement`; suspend, daemon restart
and resume with history (including mid-turn and with a form open); routine
checkpoints holding only `sessions/<id>.json`; container launches through the
image's `dsh-acp` and through the `npx` fallback; `permission_mode` forced to
`ask` on raw targets and `allow` in containers on every open; reserved
variables refused; the 131049-byte instructions limit; `mj models`,
`mj login` refusal, the viewer's permission form, `mj elicitations` and
`mj respond`.

### Fixed in dsh-acp (released as 0.2.1)

- K4: plan mode told the model nothing and asked permission before refusing
  (`47b5604`).
- K1: command titles joined argv unquoted (`0e7a19e`).
- K3: "Allow for this session" approved the whole tool; now scoped as Codex
  scopes it (exact command and cwd, file, MCP tool) (`3141931`).
- B-2 (blocks): a dsh-acp crash during a tool call left every later request
  failing with HTTP 400 (`6ff7200`).
- B-3: the prompt response reported the session's running usage total; it now
  reports the turn's and declares `mjolnir.dev/usage-scope = turn` (`ab45b1e`).
- D-2: a rejected key and an unreachable endpoint now name
  `DEEPSEEK_API_KEY` and `DEEPSEEK_BASE_URL` (`2d0a47b`).

### Fixed in Mjolnir

- D-6: Settings showed Codex-only fields for other kinds (`5bb63acfd`).
- A-5/D-7: the Import tab reported unsupported DSH, OpenCode and MiMo scans as
  failures (`9ca25dbd2`).
- Wording and docs: the skill's removed `deepseek` example, doctor's
  `mj setup` advice, the Auto-reviewer message, container rsync advice (C-3),
  the staged-AGENTS.md sentence (`0b48a7790`).
- E-4/A-3: the viewer offered `/fast` everywhere; refusals for settings an
  agent does not offer were internal wording (`ed66eed71`).
- D-3/D-4: instructions dsh-acp cannot receive (too long, not UTF-8) are
  refused before launch with one message naming the profile's file, and
  `mj doctor` reports them (`d5c0b5b2d`).
- A-6: changed files showed git's octal escapes for non-ASCII names
  (`815a3174a`).
- dsh-acp 0.2.1 pinned (`de80fdb94`); the agent-dev image republished.
- C-1 (blocks, every harness): a Move from a local checkout into a container
  cloned into the session-id folder and failed; the restored harness session
  also used the wrong working directory (`f6d49be71`).
- D-9 (every harness): `mj review start` reported success when the daemon
  refused (`5dde3143c`).

Verified live after the fixes on instance `dsh-v`: plan mode with no
permission prompt, Codex-scoped session approval, recovery after killing
dsh-acp mid-command, DSH usage in totals, real file names in changed files,
a podman session on the republished image (dsh-acp 0.2.1), and a managed
clone moved from localhost into podman that kept its conversation.

### Second round (2026-10-23, later)

- DSH sessions can use Mjolnir sub-agents; DSH offers only Mjolnir and None,
  defaulting to Mjolnir when remembered and configured; a DSH session updates
  the shared remembered choice only when it picks Mjolnir (`7feabd81c`,
  `2a1d7e2fb`). DeepSeek's own `@deepseek-ai/dsh-subagent` packages plug into
  DeepSeek's `dsh` CLI, not dsh-acp, and were not used.
- Reverse of C-1: a container session moved to a local checkout failed unless
  its repository was named `project` (`ce450f408`).
- B-5: destroy now removes every checkpoint archive of the session, including
  a Move's handoff archive (`2f1e81621`).
- dsh-acp 0.2.2 (edit diff in permission requests, K2; one card per command,
  K5) released and pinned (`0487a5706`).
- D-10: a session-named review model now takes a review baseline for the
  reviewer Auto will choose; one shared eligibility rule serves the chooser
  and the baseline check (see git log for "Take a review baseline").

- DSH turn review: reviewer children run in plan mode and could not call
  `handback`. The review owner marked `handback` read-only (`82eba5693`), and
  dsh-acp 0.2.3 offers and allows read-only MCP tools in plan mode (pinned
  `3cee6f635`). Verified live: a DSH reviewer handed back "patch is correct"
  and the session showed "Review complete: no material findings".
- `mj wait` prints a failed session's error, `mj sessions` points failed rows
  to the details, and a `mj prompt` that times out while a permission form
  waits names `mj elicitations`, `mj respond` and `--return-on-input`
  (`e98820f9b` and its parent). B-4 is not a defect: `mj usage` keeps the
  harness's raw stop reason; events normalize it.

### Open

- dsh-acp: the native session title is taken from the project-memory
  preamble (A-1).
- `mj new` and `mj resume` return once the request is accepted, so a launch
  failure shows only in `mj wait` and the session (D-1, K6); changing their
  exit codes would change how scripts behave. `mj acp` cannot answer DSH
  permission requests (E-1, documented generically).
- Startup detection (PR #1342) adds a `dsh` profile without a
  `DEEPSEEK_API_KEY` reference, so a key kept in `secrets.toml` leaves the
  profile "not authenticated".
