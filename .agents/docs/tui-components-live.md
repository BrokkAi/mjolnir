# Live TUI component acceptance

Build the matching host CLI and worker, then run the isolated terminal harness:

```sh
cargo build -p brokk-mjolnir -p brokk-mj-worker
python3 tests/e2e/tui_components_tmux.py --seed 515
```

## Running it on a busy or shared host

The fixed waits (15 s for the TUI, 20 s for the lab, 8 s in the dialog probes)
can expire before a daemon or session is up. Set `MJ_E2E_TIMEOUT_SCALE=4` to
multiply the waits of 20 s or less (the 150 s review waits are left alone); the
default, 1, changes nothing:

```sh
MJ_E2E_TIMEOUT_SCALE=4 python3 tests/e2e/tui_components_tmux.py --seed 515 --hel target/debug/mj
```

The scale does not rescue a host that is far too busy. At a load average of
100 to 290 the first steps fail whatever the scale, so start the run when the
load is below about 60. On a WSL host the figure is inflated by how the VM's
processors map to the host's, but the failures still track it.

The lab removes the host session's `CLAUDE*`, `CODEX_*` and `MJ_*` variables
from the daemon's environment, so a real profile is not picked up when the
harness runs from inside a Mjolnir session. If a lab config shows a
`[profiles.claude]` table, that protection has regressed.

Exit code 2 with no artifact directory and "live tmux setup gap" in the log
means the shared build cache's garbage collection removed `target/debug/mj` and
`target/debug/mj-worker`. Build them again with the command above; no source
change is involved.

When a step fails, the message keeps only the last 4096 characters of the pane,
which cuts off the top of the dashboard. To see the whole screen, add a
temporary `open("/path", "w").write(tmux.capture())` before and after the step
in a copy of the script, and restore the file afterwards.

The TUI applies a focus change, and registers a new step's click areas, when
the next frame is drawn, and nothing on screen shows that has happened. A key
or click sent right after a step change can therefore act on the previous
frame. The script waits a short, scaled time before such a click or Enter
(`click` in tui_components_actions.py, and the Shift-Tab then Enter at the start
of the workflow). A lost click shows up as the next wait timing out.

Last verified on 2026-11-07: seeds 61172 and 61173 passed with scale 4, 73
captures each, on the final version of the scripts. The `--dialogs-only`, `--navigation-only` and `--subagents-only`
modes, and scale 1, had not been run.

Run outside the restricted sandbox because the application uses loopback and
Unix sockets. The harness selects the freshly built worker explicitly. It uses
a private tmux server and a seeded reliability_lab.Lab with disposable config,
profiles, project, and deterministic fake ACP responses. Short runtime paths
under /tmp/hel-r-* avoid Unix socket length limits; build output and retained
evidence stay under target/reliability-artifacts/.

Chat scenarios cover reverse search over prompts sent from the composer and
through the web action, model filtering and effort selection from the menus
above the composer, Unicode bracketed paste, the one-question-at-a-time form
(Enter answers and moves on, a button pressed and released elsewhere does
nothing, one unanswered question asks before sending), and the plan review.
Dashboard scenarios cover rename and restore, target configuration, Settings
(each level closes with its own Escape), review settings, the palette key held
by an open dialog, the new-session wizard, the four-tab session picker
(Live, Mjolnir, Import, Archived), import search, and Web info.
Keys are the prefix chords (Ctrl+B then a letter), not function keys. The main fixture is now
created through the actual New wizard: empty and relative path validation,
bracketed paste, Back/Next, and mouse Create. The harness checks both exactly
one durable new session and the final review dialog disappearing.

Submission checks also save and restore target IDs and review settings,
Cancel and submit Stop, then Resume the same session through Back/Next and
mouse submission. Dialog disappearance uses the modal marker itself, not a
Sessions heading that can remain visible behind an open dialog.

The fixture then restarts its daemon with a local-Podman target configuration
so container settings are available. A lab-owned podman shim absorbs capacity
probes. The container scenario edits resource fields and mounts, toggles
read-only state, tests captured drag-outside release and mouse Cancel, and
resizes the open form through 40x10, 72x18, 140x40, and 200x60. A separate Save
checks durable CPU/memory values and reopens the editor to verify persistence.
Before typed Force destroy, the harness restores the real bare target and
restarts the daemon: the container overlay does not represent a real container
worker. Destroy tests Cancel, wrong confirmation, and successful submission
against the sole owned fixture session. Below the minimum,
the dashboard shows an explicit size message and recovers on resize.
Detach/reattach and SIGTERM complete the run.

Each run prints its artifact directory. live-evidence.json records the binary
hash, inputs, assertions, captures, and outcome. Runtime logs are copied after
owned process groups stop. Teardown precedes removal of working state. No
personal tmux server, real container runtime, or paid provider is used. These
checks establish terminal input behavior; microphone device capture and an
individual emulator's emoji glyph appearance require the actual device.

Review settings acceptance counts requests in fake-acp.log. The dialog reads the
daemon's cached choices, so opening it, adding a reviewer and cancelling must
start no adapter, even with the session stopped and no worker to ask. This check
lives in tui_review_discovery.py and runs as part of the main harness. (Earlier
versions started an adapter per model and had a Refresh button; that design is
gone.)

Seed 416 completed all 55 recorded assertions at
`target/reliability-artifacts/tui-components-seed-416-3625603/`. Earlier runs
exposed Web dialog sizing, reviewer cancellation, capture across redraw, and
review-settings discovery cancellation bugs; their fixes have regression tests.

Final seed 417 passed 56 assertions, adding reviewer Back-navigation, at
`target/reliability-artifacts/tui-components-seed-417-3690243/`. Its CLI SHA-256 is
`7328a27852a3b5eb685ec109ec9fe4f5901b1df19ec6ff4f3b835e36fce9e0ce`.

The follow-up dialog lifecycle audit passed all 69 recorded checks in seed 515,
at `target/reliability-artifacts/tui-components-seed-515-2261271/`, using CLI
SHA-256 `fdb5b63899ffd5b25cb146742ffdcc82132909a4594517d37af36c7243b3887f`.
Seed 503 reproduced Create leaving its final review open after creating a
durable session. Later runs exposed project-field focus, bare-target Resume
Next, and an unchanged config reply closing a newly opened palette. These now
have event regression tests. Live coverage does not include successful native
import, replacement repository origins, or provisioning cloud targets; their
state transitions remain covered by automated tests.

Palette/review follow-up seed 516 passed 72 recorded checks at
`target/reliability-artifacts/tui-components-seed-516-2647648/`, with CLI SHA-256
`df81d232229d86009ce1bb879722c4f0b09d05aeb7d4673f1a2474fdce4d8f8f`.
It adds F2 at 140x60 with all groups visible and no scrollbar, 100x18 with a
scrollbar and End navigation, then filtering to Review settings with no
scrollbar. It also verifies the Quick and Extended descriptions and the new
model-loading message. The controller's progress test holds repository tooling
verification pending and verifies that model/effort choices arrive first;
the TUI test checks that progress leaves readiness pending and rejects an old
generation after cancellation/reopening.

Final seed 518 passed 72 checks at
`target/reliability-artifacts/tui-components-seed-518-2766281/`, including the
thumb reaching the bottom at End and explicit nested Help dismissal. This run
includes the shared scrollbar correction: Ratatui receives the count of valid
viewport offsets, so the thumb reaches both ends of the track.
Its CLI SHA-256 is
`e1a49740f8a082ec1880aaae7dc54dad75d2ddec941176589ceeb08a31a0ceb0`.

Lightweight review discovery seed 519 passed all 79 checks at
`target/reliability-artifacts/tui-components-seed-519-3412378/`. Request-count
assertions confirmed zero starts for tier/effort edits and cache hits, and one
start for a model cache miss or Refresh. Choices arrived through Help, Save
closed during Refresh, and offline Refresh retained choices while Save worked.
The CLI SHA-256 is
`6fddeb49a422b9f1d13b518c46c48f301d4761765c8e7729696ac91e00f0853f`.

After merging upstream clipboard and session-card changes through d71601b4,
seed 520 passed all 79 checks at
`target/reliability-artifacts/tui-components-seed-520-3510340/`. Its CLI SHA-256
is `9b93b9bb230e2fa64071fe4305dc55df76d910bc508cc3d7d6086f07b7c81553`.
