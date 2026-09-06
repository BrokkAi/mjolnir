# Live TUI component acceptance

Build the matching host CLI and worker, then run the isolated terminal harness:

```sh
cargo build -p brokk-mjolnir -p brokk-mj-worker
python3 tests/e2e/tui_components_tmux.py --seed 515
```

Run outside the restricted sandbox because the application uses loopback and
Unix sockets. The harness selects the freshly built worker explicitly. It uses
a private tmux server and a seeded reliability_lab.Lab with disposable config,
profiles, project, and deterministic fake ACP responses. Short runtime paths
under /tmp/hel-r-* avoid Unix socket length limits; build output and retained
evidence stay under target/reliability-artifacts/.

Chat scenarios cover model filtering, effort selection, Unicode bracketed
paste, question submission, pointer release outside a button, reviewer setup
with asynchronous discovery, cancellation restoring the unanswered plan, and
turn-review tabs and cancellation. Dashboard scenarios cover rename and restore,
target configuration, disabled review settings, nested Help during discovery,
new/resume selectors, import search, and Web info. The main fixture is now
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
