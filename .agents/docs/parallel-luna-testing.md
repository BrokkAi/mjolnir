# Parallel Luna reliability campaign

This guide runs four Luna coding agents as concurrent exploratory testers without letting them edit Hel while they test it. It complements `.agents/docs/luna-reliability-runbook.md`: that runbook defines the detailed missions and evidence standard, while this guide defines parallel ownership, browser operation, coordination, and cleanup. The campaign is local and must use only disposable fake-ACP, local-bare Hel state.

## Operating model

Use four workers and three labs. `tui-ux` owns an isolated lab for terminal interaction. `fault-recovery` owns another isolated lab for process failure and recovery. `shared-tui` owns a third lab and its two Hel dashboards. `shared-web` connects to the third lab through one browser. The shared web worker never starts, signals, stops, or deletes a Hel process; `shared-tui` is the sole infrastructure and cleanup owner for that lab.

Each Luna may write only beneath its assigned artifact directory and the disposable runtime named by that lab's `luna-env.sh`. Before launch, create a short, uniquely named role environment wrapper under `/tmp` that sources the generated lab environment and exports the exact role directory as `HEL_LUNA_ROLE_ARTIFACTS`. Every worker command must source that short wrapper and write through `$HEL_LUNA_ROLE_ARTIFACTS`; workers must not declare `role` or `ROLE` aliases, retype either long path, or reconstruct the campaign path at capture sites. It must not edit source, configuration tracked by Git, tests, documentation, or generated files outside those roots. A finding is recorded before anyone attempts a fix.

The browser is semantic-first. Playwright MCP exposes the page's accessibility snapshot, roles, labels, text, DOM state, console messages, and network activity to Luna. Those are the normal navigation and evidence channels. A screenshot is appropriate only for an inherently visual question such as clipping, overlap, responsive layout, focus indication, or color, or when semantic evidence cannot explain a failure. Prefer one targeted element or viewport image and record why it was needed. Do not take screenshots after routine actions.

## Prepare the campaign

Work from the repository root. Build once and install the repository-pinned browser once:

    cargo build -p hel-cli
    npm ci --prefix tests/e2e/web
    npx --prefix tests/e2e/web playwright install chromium

Lab preparation and tmux socket control use loopback and Unix sockets. Run
those commands outside a restricted filesystem/network sandbox; an `EPERM`
from a sandboxed preparation is not a product result. Resolve and record the
browser executable installed by this repository rather than assuming the MCP
server will find it:

    node -e "console.log(require('./tests/e2e/web/node_modules/playwright').chromium.executablePath())"

Choose three different integer seeds. Prepare the two isolated labs and the shared lab, recording the exact `artifacts=`, `runtime=`, and `source` lines printed by every command:

    python3 tests/e2e/prepare-luna-lab.py --seed 1101 --hel ./target/x86_64-unknown-linux-musl/debug/hel
    python3 tests/e2e/prepare-luna-lab.py --seed 1102 --hel ./target/x86_64-unknown-linux-musl/debug/hel
    python3 tests/e2e/prepare-luna-lab.py --seed 1103 --hel ./target/x86_64-unknown-linux-musl/debug/hel

Do not recover paths with a broad glob after several campaigns exist. Copy each printed path into a role-specific variable in the coordinating shell:

    export HEL_PARALLEL_TUI_ENV=/exact/tui-ux/luna-env.sh
    export HEL_PARALLEL_FAULT_ENV=/exact/fault-recovery/luna-env.sh
    export HEL_PARALLEL_SHARED_ENV=/exact/shared/luna-env.sh
    export HEL_PARALLEL_RUN=/exact/new/parallel-campaign-directory

Create each role directory and one short wrapper. Substitute the exact base
environment and role paths; do not build either from a seed or glob:

    mkdir -p /exact/parallel-campaign/workers/tui-ux
    printf '%s\n' \
        'source /exact/tui-lab/luna-env.sh' \
        'export HEL_LUNA_ROLE_ARTIFACTS=/exact/parallel-campaign/workers/tui-ux' \
        > /tmp/hel-luna-1100-tui-ux.sh

Pass only the short wrapper path to that worker. Require
`source /tmp/hel-luna-1100-tui-ux.sh`
at the start of every separate shell command and require all evidence paths to
begin with `$HEL_LUNA_ROLE_ARTIFACTS/`. A failed write caused by an unset or
different path stops the phase until the coordinator verifies that nothing was
created outside the role directory. Remove the exact wrapper during final
campaign cleanup.

Create `HEL_PARALLEL_RUN` beneath `target/reliability-artifacts/`, with one subdirectory per worker. Record the commit, seeds, Luna model, tmux version, terminal dimensions, and pinned Playwright version in `manifest.md`. Never copy a login code, QR URL, cookie, token, or environment containing credentials into that manifest.

Start a fresh tmux server with a control window and one window per Luna. The server name must be unique to this campaign and must not reuse a server that contains Hel dashboard clients:

    tmux -L hel-luna-parallel-1100 -f /dev/null new-session -d -s campaign -n control -x 160 -y 48
    tmux -L hel-luna-parallel-1100 new-window -d -t campaign -n tui-ux
    tmux -L hel-luna-parallel-1100 new-window -d -t campaign -n fault-recovery
    tmux -L hel-luna-parallel-1100 new-window -d -t campaign -n shared-tui
    tmux -L hel-luna-parallel-1100 new-window -d -t campaign -n shared-web

Launch each worker interactively with `codex --no-alt-screen -m gpt-5.6-luna -s workspace-write -a on-request -C /home/jonathan/Projects/hel2` and supervise its approval requests. Pass its exact role, environment-file path, artifact directory, mission assignment, ownership limits, and the instruction to follow this guide plus `.agents/docs/luna-reliability-runbook.md`. Do not combine an explicit sandbox with `--approve-for-me`; the CLI rejects those modes as mutually exclusive, and removing the sandbox to obtain auto-approval is not acceptable. Start a fresh browser worker process for every attempt so its MCP output directory cannot remain pinned to an earlier role directory. The browser invocation additionally configures the repository-pinned Playwright MCP server per process; do not add it to global Codex configuration. Include the resolved executable path explicitly:

    -c 'mcp_servers.playwright.command="/home/jonathan/Projects/hel2/tests/e2e/web/node_modules/.bin/playwright"'
    -c 'mcp_servers.playwright.args=["mcp","--headless","--isolated","--ignore-https-errors","--snapshot-mode","full","--executable-path","/exact/pinned/chromium","--output-dir","/exact/shared-web/browser"]'

Do not add the optional `vision` capability. Screenshots remain available for explicit visual investigation, but structured snapshots are the primary operating mode.

## Worker missions

`tui-ux` runs mission M4 from the reliability runbook and also explores every pane, dialog, hotkey, narrow size, long transcript, scroll boundary, detach/reattach path, and clipboard failure. It owns its daemon and any nested tmux server it creates. It must distinguish terminal corruption from text merely present in scrollback.

`fault-recovery` runs missions M3 and M6 through M8. Before sending any signal it verifies the exact process environment contains its lab's `HEL_CONFIG_DIR`, `HEL_DATA_DIR`, and `HEL_CHAOS_ISOLATED=1`. It never selects a victim using only a process name or grep match. Use exact `/proc/<pid>/environ` matching for the leak audit and tolerate unreadable unrelated `/proc` entries; do not use a broad self-matching `ps | rg` pipeline.

`shared-tui` runs missions M1 and M2. In the normal campaign path it starts TUI 1, creates the workspace, and only then starts TUI 2; this prevents setup mechanics from consuming the lifecycle mission. A separate, explicit concurrency probe may start both selectors against an empty database and accept the same workspace name in both. It creates a phase-unique readiness marker such as `$HEL_PARALLEL_RUN/attempt-2-shared-ready` only after both dashboards show the same workspace and the Web dialog can be opened. It owns lifecycle and cleanup for the shared lab. Before each coordinated race it writes a future Unix timestamp to a phase-unique file such as `$HEL_PARALLEL_RUN/attempt-2-phase-N-go`; both shared workers act when that timestamp arrives and record their observed start time. This makes timing comparable without either agent writing the other's files.

`shared-web` waits for the exact current-attempt readiness marker, authenticates through the disposable Web dialog, and runs mission M5 plus the web half of M1 and M2. It drives the real page through accessibility snapshots and role- or label-based actions. It exercises desktop and mobile viewport sizes, browser history, logout, cookie expiry, offline/online SSE recovery, stale conversations, lifecycle actions, and convergence with both dashboards. It may request shared actions through a phase-unique append-only log such as `$HEL_PARALLEL_RUN/attempt-2-web-requests.md`, but it never manipulates shared Hel processes directly.

For the repository fake/local-bare lab, the exact new-session sequence is:
Tab to move focus from Prompt to the Sessions pane, plain `n`, Enter on the
fake profile, Enter on localhost, replace the project field with the lab's
exact absolute `project` directory, Enter to validate, wait for the 4/4 review,
then Enter on the default Create button. The fake profile's quota row can say
that Codex is unavailable while the configured fake ACP executable remains
fully launchable; quota health is not harness health.

Launch or relaunch the TUI by running plain `$HEL_LUNA_BINARY` in the prepared
environment and selecting the existing workspace when necessary. Hel has no
`dashboard <workspace>` subcommand; treating that invented command's parser
error as a product failure invalidates the phase.

Hel has one surface: Sessions, the transcript, Prompt, Targets, Quota, and a
footer. The footer names the keys for whatever has focus, and it names them
literally — `n new` means the plain letter `n`, not Ctrl+N. Record modifiers
exactly.

The pane keys only work while a pane has focus, and the surface opens with the
keyboard in Prompt, so a sweep has to Tab first or the letters land in the
composer as text. Tab walks Sessions, Prompt, Targets, Quota; Shift+Tab
reverses it, and neither changes pane geometry. Sessions, Targets, and Quota
each expose `▁`, `▪`, and `□` title controls for minimized, standard, and
maximized size. Alt+Z cycles the focused support pane without moving focus.
Alt+G changes an all-standard layout to maximized Sessions with minimized
Targets and Quota; from any customized layout it restores all three to
standard. F2 is the command palette, F3 is Workspaces, F4 is the web viewer,
and Alt+Q detaches.

Escape no longer quits. It cancels a running turn or a shell command and closes
dialogs, so a sweep can use it freely — but a report that Escape "failed to
quit" is a stale expectation, not a defect.

Workers must not fix defects during the campaign. They may minimize a reproduction inside their disposable lab. Once the exact sequence and evidence are preserved, they finish their assigned missions even if another finding has already been recorded.

Role ownership is also a negative constraint: a worker must wait when another
lane is slow, not substitute for it. In particular `shared-tui` never launches
Playwright or another browser, and `shared-web` never starts or signals Hel.
The coordinator interrupts and restarts a phase if a lane crosses that boundary.

## Evidence and coordination

Every worker owns `$HEL_PARALLEL_RUN/workers/<role>/` and no worker writes another role's files. Each directory contains `actions.md`, `result.md`, bounded logs, process evidence, and any role-specific captures. Start reruns in sibling attempt directories; never preserve evidence by copying a directory into one of its own descendants. Append an action before executing it, including wall-clock time and intentional delay. `result.md` ends with `PASS`, `FAIL`, or `BLOCKED` for every assigned mission.

Each finding contains the commit, worker role, lab seed, mission, dimensions or viewport, exact keys/clicks/signals and relevant pauses, expected and observed behavior, the first relevant bounded log lines, artifact paths, and the proposed deterministic regression layer. Browser findings normally include accessibility snapshots, console messages, request status, and current URL. A visual finding may additionally include one targeted screenshot. Give every saved browser artifact an absolute path beneath the role output directory and verify `git status` after capture; Playwright can otherwise resolve a relative filename against the repository root. Login snapshots can include the code as a textbox value, so clear the field and redact saved YAML. Playwright traces and screenshots must not expose viewer credentials.

Use only the current attempt's uniquely named request log. Every request has an identifier. The performing worker records `RESP <request-id>` in its own `actions.md`; it does not edit the request line. A requester must observe that exact response before starting a dependent action. Phase completion uses the exact result and marker files, not the absence of a response in an old request log. Use unique role-owned readiness files and numbered phase files instead of having several agents rewrite one shared status document. Reusing an old marker or request log invalidates the phase.

The coordinating operator watches progress without interacting with a worker's nested Hel tmux server:

    tmux -L hel-luna-parallel-1100 list-windows -t campaign
    tmux -L hel-luna-parallel-1100 capture-pane -p -S -200 -t campaign:tui-ux
    tmux -L hel-luna-parallel-1100 capture-pane -p -S -200 -t campaign:shared-web

Do not treat a Luna final answer as sufficient evidence. Inspect its role directory and reproduce every reported failure before changing code.

## Triage, repair, and rerun

Classify each finding as a product defect, harness/runbook defect, expected refusal, duplicate, or insufficient evidence. Reproduce a product defect with the smallest exact action sequence. Add a deterministic module, PTY, browser, property, or named-hook regression before or with the repair. Fix the source rather than weakening an assertion or hiding an error. Rerun the focused regression, the failed Luna mission with the original seed, and the neighboring shared-client scenario.

If the campaign exposes an ambiguous command, unsafe ownership rule, missing prerequisite, evidence gap, or coordination failure, update this guide in the same repair checkpoint. Add a short `Campaign lessons` entry naming the mistake and the corrected procedure so later campaigns do not rediscover it.

## Finish and acceptance

Wait for all four `result.md` files before cleanup. Each isolated worker stops its daemon, verifies SQLite integrity and foreign keys, checks for processes retaining its exact `HEL_CONFIG_DIR`, terminates its nested tmux server, and only then removes its exact runtime root. `shared-web` closes its browser and writes the current attempt's exact browser-closed marker; `shared-tui` accepts only that campaign-root marker before performing the shared lab's integrity, leak, and cleanup checks. A worker-local or earlier-attempt marker is never sufficient.

The coordinator writes `summary.md` with every mission result, finding classification, repair commit when applicable, original-seed rerun result, SQLite result, leak audit, and cleanup outcome. A campaign passes only when all confirmed defects are repaired and rerun, every lab reports `integrity_check` as `ok` with no foreign-key output, no owned processes remain, and no unresolved finding involves data loss, duplicate transcript events, lifecycle failure, authentication leakage, terminal corruption, UI hang, or cross-client divergence.

Finally kill only the campaign's outer tmux server:

    tmux -L hel-luna-parallel-1100 kill-server

Retain the campaign artifact directory. Never loop a failed case until it turns green and never erase the first failing evidence after a repair.

## Campaign lessons

Add dated entries here after executing this guide. State the operational mistake, its observable consequence, and the corrected procedure.

- 2026-08-30: `codex exec -s workspace-write --approve-for-me` is invalid, and dropping the sandbox for auto-approval is unsafe. Use an interactive, supervised Luna with `-s workspace-write -a on-request`.
- 2026-08-30: restricted execution made socket-based lab preparation fail with `EPERM`. Prepare labs and control tmux outside that sandbox.
- 2026-08-30: Playwright MCP searched for branded Chrome instead of the repository-pinned Chromium. Resolve `chromium.executablePath()` and pass `--executable-path`.
- 2026-08-30: reused readiness markers and request logs let one phase consume another phase's cleanup and instructions. Name every gate and log by attempt, and accept only its exact campaign-root path.
- 2026-08-30: copying prior evidence beneath its own directory recursively duplicated artifacts. Preserve attempts as siblings from the beginning.
- 2026-08-30: relative Playwright output escaped to the repository root, and semantic login YAML retained a textbox code. Use absolute role-owned output paths, clear/redact credentials, and check `git status` after every capture batch.
- 2026-08-30: imprecise hotkey notation caused lowercase `b`/`q` and a dashboard Escape to be misclassified as failures. Record Ctrl modifiers and remember that dashboard Escape intentionally quits.
- 2026-08-30: launching both setup selectors before workspace creation distracted the standard shared mission and exposed a real non-idempotent create race. Keep the standard path sequential and run the simultaneous-empty-list case as its own named probe.
- 2026-08-30: broad process grep matched the auditor itself and unreadable unrelated processes. Match exact lab environment values through `/proc/<pid>/environ`.
- 2026-08-30: `tmux new-window` does not accept the initial-session `-x/-y` sizing flags. Size the initial session or use `resize-window` after creating later windows.
- 2026-08-30: reusing a browser worker kept MCP output pinned to the prior attempt even though the prompt named a new role directory. Start a fresh browser worker with the new absolute `--output-dir` for every attempt.
- 2026-08-30: a TUI worker reconstructed its long artifact path incorrectly, then launched a second browser when the browser lane was slow. Bind the role directory once, prohibit cross-lane substitution explicitly, and have the coordinator interrupt a boundary violation before continuing the phase.
- 2026-08-30: requests were appended without exact completion responses, so one lane waited after the browser had already finished. Require a role-owned `RESP <request-id>` before dependent work and use the phase's result/close marker for final completion.
- 2026-08-30: an immediate Stop/Resume race did not cover a chat whose five-second actor handoff had already expired. Keep one client in the old chat for at least six seconds, resume elsewhere, and require automatic reconnection without reopening the view.
- 2026-08-30: a successful semantic browser run still produced a missing-favicon 404. Preserve bounded console and network summaries and reject unexpected first-party resource failures, even when the tested actions pass.
- 2026-08-30: telling workers to remember a long role path still led two reruns to reconstruct and shorten it in later shell calls; placing the first wrapper under that same long path did not solve the transcription problem. Give each worker one short `/tmp` wrapper path, require every shell to source it, and permit evidence writes only through `$HEL_LUNA_ROLE_ARTIFACTS`.
- 2026-08-30: the zero-delay fake ACP completed provisioning, resume, and prompts before M3 could deliver cancellation, leaving the mission repeatedly blocked. A 1.5-second retry was too tight to distinguish a newly published row from a stale prior operation; even five seconds was consumed by semantic capture, exact-ID selection, and supervised tool latency. Manual Luna labs now default to a recorded 15-second fake-ACP delay; do not claim M3 from a shorter lab unless the remote client visibly identifies and cancels the exact session before settlement.
- 2026-08-30: a rerun assumed its Stop keystrokes landed and nearly classified an empty Resume dialog as a reconnection defect. Capture every lifecycle dialog and require the target session in the stopped list before starting a post-Stop timing phase.
- 2026-08-30: a second rerun started its reconnect clock after only the first Resume wizard step, then mislabeled the still-empty Active pane as a product failure. Capture and complete all four fake/localhost Resume activations, and require a visibly active session before timing downstream convergence.
- 2026-08-30: one M8 attempt added Tab before confirming Stop and silently focused Cancel, wasting its 30-second sample window. Capture focus for each dialog independently; the current Stop confirmation activates Stop with Enter directly.
- 2026-08-30: the exact-environment audit initially recorded `PID/environ` paths, which are evidence but are not valid process identifiers for teardown. Strip both `/proc/` and `/environ`, record numeric PIDs, terminate their process groups, re-audit, and only then remove the runtime.
- 2026-08-30: materializing each whole process environment and piping it through text search made unreadable `/proc` entries and observer self-matches needlessly fragile. Unset the lab exports first, suppress permission failures at the redirection boundary, and compare exact NUL-delimited `HEL_CONFIG_DIR` and `HEL_DATA_DIR` entries in one shell loop.
- 2026-08-30: a worker required the literal word `Launch` and misclassified valid `Sync`/`Start` provision-stage rows. Gate M3 on the exact session ID plus any in-flight stage label, not one presentation string.
- 2026-08-30: a repair prompt invented `hel dashboard <workspace>`, causing a parser error during relaunch. Run the prepared Hel binary directly and use its workspace selector.
- 2026-08-30: accepted resume cancellation durably rolled the session back to `Stopped`, but the remote dashboard kept its `Launch` overlay because the convergence rule recognized only running/error terminal states. Treat the lifecycle's rollback state as authoritative and require both clients to show the same recoverable stopped row before continuing.
- 2026-08-30: the M3 prompt instructed Ctrl+X in chat, where that key is not cancellation, and initially labeled the expected completed reply as a defect. Use Ctrl+X only for dashboard lifecycle operations; put chat focus on Prompt and use Escape for a running turn.
- 2026-08-30: the delayed fake ACP slept inside `session/prompt`, so it could not consume `session/cancel` until after emitting the reply; a valid Hel cancellation was misclassified as a product failure. The fixture now waits on stdin during the delay, returns `stopReason: cancelled`, and must prove the cancel line in its protocol log.
