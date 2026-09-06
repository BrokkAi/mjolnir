# Live TUI component acceptance

`tests/e2e/tui_components_tmux.py` drives the built `target/debug/mj` through a
private tmux socket. A seeded `reliability_lab.Lab` supplies isolated config,
data, profile, fake ACP replies, and a local-bare project. The harness first
creates and stops a fake-ACP session, then changes only that lab's
`localhost` target to `local-podman` with a local fixture image. This is the
fixture remedy for the current setup gap: local-bare sessions do not expose
the container settings command. A lab-owned `podman` shim absorbs any
background capacity probes and logs their arguments.

Run after the root agent reports that the debug binary is ready:

```sh
python3 tests/e2e/tui_components_tmux.py --seed 406
```

The run uses 40x10, 72x18, 140x40, and 200x60 windows, records keyboard and
SGR mouse input, tests nested help, F2 palette routing, container fields,
mounts, read-only state, captured drag-outside release, mouse Cancel, detach,
and SIGTERM, and saves screen captures plus `live-evidence.json` below the
printed `target/reliability-artifacts/tui-components-seed-*-<pid>/` directory.
The copied `runtime/` directory contains fake ACP and controller logs after
test-owned processes have been stopped. The harness never uses a personal
tmux server, config, data directory, container runtime, or paid ACP state.
