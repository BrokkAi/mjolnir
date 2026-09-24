# Luna reliability campaign

This is a local exploratory test campaign for Hel's Linux/WSL runtime. It is not a CI job and it must never use personal Hel state, paid harnesses, remote targets, or real credentials. A finding is useful only when its exact actions and evidence are saved so the behavior can become a deterministic regression.

This runbook defines the missions and evidence for a single campaign operator. To distribute those missions across four concurrent Luna workers with explicit lab ownership and a semantic browser lane, follow `.agents/docs/parallel-luna-testing.md` as the outer orchestration guide and use this document for the mission details.

## Prepare an isolated campaign

Build the current commit and install the pinned browser once:

    cargo build -p brokk-mjolnir
    npm ci --prefix tests/e2e/web
    npx --prefix tests/e2e/web playwright install chromium

Choose a seed, prepare the disposable fake-ACP/local-bare lab, and source the printed environment file in the shell that will start tmux:

    python3 tests/e2e/prepare-luna-lab.py --seed 1 --hel ./target/debug/mj
    source target/reliability-artifacts/luna-manual-seed-1-*/luna-env.sh

The manual-lab preparer gives fake ACP new/load and prompt calls a 15-second
delay by default so M3 has a reproducible cancellation window. Record
`MJ_LUNA_FAKE_ACP_DELAY_MS` in the evidence. Override `--fake-delay-ms` only
for a named timing variant; a zero-delay lab cannot prove M3 cancellation.
During the prompt delay the fake continues reading stdin: `session/cancel`
must appear in `fake-acp.log`, suppress the agent reply, and complete the
prompt with `stopReason: cancelled`. A fake that merely sleeps and reads the
cancel after emitting its reply cannot test prompt cancellation.

Record the build and terminal geometry before doing anything else:

    git rev-parse HEAD | tee "$MJ_LUNA_ARTIFACTS/commit.txt"
    printf 'outer=%sx%s\n' "$(tput cols)" "$(tput lines)" | tee "$MJ_LUNA_ARTIFACTS/dimensions.txt"
    tmux -L hel-luna-1 -f /dev/null new-session -d -s hel -x 140 -y 40
    tmux -L hel-luna-1 split-window -h -t hel:0
    tmux -L hel-luna-1 split-window -v -t hel:0.1
    tmux -L hel-luna-1 send-keys -t hel:0.0 "source '$MJ_LUNA_ARTIFACTS/luna-env.sh'; '$MJ_LUNA_BINARY'" Enter
    tmux -L hel-luna-1 send-keys -t hel:0.1 "source '$MJ_LUNA_ARTIFACTS/luna-env.sh'; '$MJ_LUNA_BINARY'" Enter
    tmux -L hel-luna-1 send-keys -t hel:0.2 "source '$MJ_LUNA_ARTIFACTS/luna-env.sh'; watch -n 1 '$MJ_LUNA_BINARY daemon status'" Enter
    tmux -L hel-luna-1 attach -t hel

Use the first TUI to accept the proposed workspace name. Attach the second TUI to that workspace. Open the Web dialog with `Ctrl+B`, scan its QR code or enter the six-digit code in a mobile-sized browser, and confirm all three surfaces show the same empty workspace. Record the browser name/version and viewport in `dimensions.txt`.

Before each mission, append its identifier and wall-clock time to `notes.md`. Luna should vary pauses and interleavings from the campaign seed, but must write down every key and click exactly; “clicked around” is not a reproduction.

## Mission cards

### M1 — three-client lifecycle

Create a session from the web viewer. Confirm both TUIs show provisioning immediately. Open the conversation in the browser, send one prompt from TUI 1, then queue a second before the fake reply completes. Read from TUI 2 and the browser. Stop from TUI 2, resume from the browser, and stop from TUI 1. Check that each prompt and reply appears exactly once and every surface converges without a manual refresh.

### M2 — rapid and overlapping control

With both TUIs and the browser attached, alternate `Ctrl+E`, Escape, arrow keys, Enter, Tab, and pane-specific refresh keys as quickly as tmux can deliver them. Race Stop against Resume or a second Stop from another surface. A refusal is acceptable when it names the active operation; an unbounded spinner, UI freeze, duplicate lifecycle, or stale terminal state is not.

Also leave one TUI inside the chat while another client stops the session. Wait at least six seconds so the chat's initial actor-handoff window expires, then resume from the other client. Without navigating or reopening the waiting chat, require it to reconnect within 15 seconds, preserve any unsent draft, and accept a prompt exactly once. This deliberately covers resumes that occur after the optimistic handoff window rather than only immediate Stop/Resume pairs.

Treat lifecycle dialogs as state machines, not keystroke assumptions. Capture the Edit dialog, the Stop confirmation, and the settled dashboard separately. In the current Stop confirmation, Stop already has focus: press Enter directly; an extra Tab moves to Cancel. Before timing the stale-chat phase, open Resume and require that it lists the stopped session; “No stopped Hel sessions” invalidates the phase and means Stop must be retried, not reported as a product failure.

For the fake/localhost fixture, completing Resume from that list requires four distinct Enter activations with a capture after each: select the stopped row, select the fake profile at 1/3, select localhost at 2/3, and submit the default review at 3/3. Start the 15-second chat-reconnection window only after the dialog has closed and the resumed session is visibly active/running. A profile or target wizard still on screen, or an empty Active pane, means Resume did not commit and invalidates the phase.

### M3 — cancellation boundaries

Start a new session and cancel during provisioning from a different client. Repeat during resume and during a long fake prompt. Ctrl+X cancels an in-flight lifecycle from the dashboard; it is not the chat cancellation key. In chat, put focus on Prompt and press Escape to cancel the running turn. Escape from Conversations only returns focus to Prompt, so capture focus before the key. Exercise dialog Cancel buttons and Escape separately. Confirm the UI remains responsive, the operation settles, and no worker or worktree survives solely because cancellation won a race.

Do not target a row by position. Record the new session ID from the initiating client, wait until the other client visibly renders an in-flight operation for that exact ID, and only then send Ctrl+X inside the 15-second fixture window. The row names its current provision stage (`Sync`, `Start`, or another stage) while work is active; it need not literally say `Launch`. Capture the remote client's cancellation notice or failure notice as well as both settled rows. A key sent before the exact row appears, or against a prior stale operation row, does not establish a cancellation boundary.

After a cancelled resume, `Stopped` is represented by the session disappearing from Active and appearing in Ctrl+S Resume. Verify the exact session ID in both clients' Resume lists; do not require a stopped row in Active. The initiating client may retain a resume-failure notice while the remote client retains a relay-loss notice. Those surface-local diagnostics need not match when their durable session lists converge.

### M4 — resize, scroll, selection, and clipboard

Resize the tmux client repeatedly through 40×10, 72×18, 140×40, and 200×60 while holding navigation keys. Scroll every pane to both limits. Select transcript text with keyboard and mouse, copy it, paste into a prompt, and repeat with `DISPLAY` and `WAYLAND_DISPLAY` unset in one fresh TUI to force clipboard failure. The failure must appear as a bounded Hel notice; library stderr must never overwrite the alternate screen. Detach and reattach after each resize series.

### M5 — stale browser and authentication

Leave a conversation open, disconnect the browser network, perform prompt and lifecycle work from both TUIs, then reconnect. The page must refresh through SSE without reloading. Expire or remove its session cookie and confirm APIs return to the login screen. Log in again by QR, sign out, and verify Back cannot expose a cached authenticated snapshot or token-bearing URL.

Capture bounded console and network summaries for the whole mission. Expected offline request failures must be limited to the explicit offline interval; otherwise the page should have no uncaught exceptions, JSON parse failures, or missing first-party resources.

### M6 — daemon death

Record the daemon PID from `"$MJ_LUNA_BINARY" daemon status`, send it `SIGTERM`, and keep typing in both TUIs while it restarts. Repeat with `SIGKILL` only after confirming the PID's `/proc/<pid>/environ` contains the campaign's exact `MJ_CONFIG_DIR` and `MJ_DATA_DIR`. The browser and both TUIs must reconnect, revisions must not move backward, and one client stopping a session must update the other two.

### M7 — worker and bridge death

Use the process tree to identify the campaign-owned worker and fake ACP bridge for one session. Verify their environment points at this lab, then kill one generation at a time. Submit a prompt before and after each death. After worker death, sample both clients and the exact process tree once per second for up to 15 seconds so the two-failure recovery threshold and worker restart have a bounded opportunity to complete; distinguish an immediate expected `relay unreachable` state from a failed recovery. The session may show a named recoverable error, but acknowledged transcript events must remain exactly once and Stop must still settle after recovery. Never signal a process identified only by a name or grep match.

### M8 — recovery and shutdown

Interrupt checkpoint/close by killing the daemon, restart with either TUI, and retry Stop. Record the exact key that activates the default Stop button, the action time, every intermediate dialog, and a bounded 30-second settlement timeline. If normal Stop reaches `CloseFailed`, exercise the explicit Force stop confirmation as a separate result rather than reporting the pre-confirmation screen as final. Detach one TUI while work is active, terminate the other with `SIGTERM`, then reattach. Terminal modes, mouse capture, cursor visibility, and bracketed paste must be restored before any final shell message. Quit must remain bounded while supervised background cleanup reports any failure.

## Evidence for every mission

After each mission, capture both panes with escapes intact and refresh the bounded process/log evidence:

    tmux -L hel-luna-1 capture-pane -p -e -S -2000 -t hel:0.0 > "$MJ_LUNA_ARTIFACTS/tui-1.capture"
    tmux -L hel-luna-1 capture-pane -p -e -S -2000 -t hel:0.1 > "$MJ_LUNA_ARTIFACTS/tui-2.capture"
    ps -eo pid=,ppid=,pgid=,sid=,stat=,etimes=,args= > "$MJ_LUNA_ARTIFACTS/process-tree.txt"
    tail -n 2000 "$MJ_DATA_DIR/daemon.log" > "$MJ_LUNA_ARTIFACTS/daemon.log"
    find "$MJ_DATA_DIR/logs" -type f -name '*.log' -print -exec tail -n 500 {} \; > "$MJ_LUNA_ARTIFACTS/controller.log"

Save any browser screenshot and Playwright trace as `browser-failure.png` and `browser-trace.zip`. Copy or update the lab's `trace.json`; do not put viewer codes, QR URLs, cookie values, profile environments, or other authentication material in notes, traces, screenshots, or terminal captures.

Each finding in `notes.md` must contain:

1. commit, seed, terminal size, browser viewport, and mission identifier;
2. exact keys, shell signals, and web actions with pauses that mattered;
3. expected behavior and observed behavior;
4. the first relevant bounded log lines and process IDs;
5. artifact filenames; and
6. a proposed deterministic unit, PTY, browser, property, or named-hook regression.

Do not fix a finding during the campaign. First preserve a minimal reproduction and choose the right deterministic layer.

## Finish and clean up

A Luna lab has no watchdog: it outlives the preparer by design, so nothing stops its daemon or workers until you finish it. Finish every lab you prepare, including one you abandon partway. A lab left running holds a daemon and a worker per session indefinitely.

Quit both TUIs normally when possible, then close the tmux server and finish the lab with the command `prepare-luna-lab.py` printed:

    tmux -L hel-luna-1 kill-server
    python3 tests/e2e/finish-luna-lab.py "$MJ_LUNA_ARTIFACTS"

The finisher stops the daemon, then stops every process the lab owns and scans again until none remain. It uses the same ownership rule as the automated labs, so it also stops workers that re-exec with a cleared environment. It writes `integrity.txt` and `leaks.txt` to the artifact directory. It removes the runtime only when `integrity_check` is `ok`, `foreign_key_check` reports nothing, and no owned process survives; otherwise it exits non-zero, names the problem, and keeps the runtime for diagnosis.

Retain the artifact directory and record whether every mission passed, failed, or was blocked. A no-defect campaign still keeps `notes.md`, captures, bounded logs, process tree, trace, browser evidence when used, and integrity result.
