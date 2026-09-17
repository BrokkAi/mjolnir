# Internal environment variables

These `MJ_*` variables are handoffs between one mj process and a child it
starts. They are not settings. Users do not set them, and the documentation
in `docs/` deliberately leaves them out. The documented settings are in
`docs/src/content/docs/configuration.md` under "Process and path overrides".

- `MJ_ORIGINAL_BASH_ENV`, `MJ_ORIGINAL_GIT_CONFIG_GLOBAL`: the worker records
  the values of `BASH_ENV` and `GIT_CONFIG_GLOBAL` it inherited before it
  replaces them for a harness shell, so the harness can restore them.
  Read in `mj-worker/src/worker_runtime/unix.rs`.
- `MJ_TURN_STALL_TIMEOUT_MS`: how long a running turn may go with nothing
  arriving from the harness *and no tool call open* before the worker fails
  the turn. Default 600000 (ten minutes); `0` disables the watchdog. Read in
  `mj-worker/src/acp/drive.rs`.
- `MJ_TURN_TOOL_STALL_TIMEOUT_MS`: how long one tool call may run before the
  worker fails the turn. Default 14400000 (four hours); `0` removes the bound.
  While a tool call is open this is the only bound that applies, because a
  harness blocked in a long build sends nothing at all and failing it loses
  real work. The bound exists only to catch a bridge that died leaving a tool
  card open; a bridge process that exits is detected at once and separately,
  by the `child.wait()` arm of the select in `mj-worker/src/acp.rs`.
  Read in `mj-worker/src/acp/drive.rs`.

Both turn bounds are read from the worker's own process environment. A
container target can set them for every session on it through
`[targets.<id>.container] environment` in the instance configuration; on any
target, a value set for the daemon process is carried to the workers it starts,
because the worker re-execs with a cleared environment and could not inherit it
otherwise (`mj-controller/src/controller/worker_binary/launch.rs`).

`RUST_LOG` is carried to workers the same way and for the same reason: a worker
that has gone quiet is diagnosed from its own log, at
`<worker root>/worker.log`, and its level cannot be raised after the fact on a
process that started with a cleared environment. `RUST_LOG=warn,mj_worker::acp=debug`
makes the turn stall watchdog report, about once a second while a turn runs,
which tool calls it can see, how long the session has been silent, the bounds it
is applying, and its verdict.

`MJ_CONTROLLER_LOCK_EXPECTED`, `MJ_CONTROLLER_LOCK_PROBE` and
`MJ_WORKER_BINARY_OVERRIDE_CHILD` appear in the source but only inside
`#[cfg(test)]` modules, where a test re-runs the test binary as a child.

Test-only hooks (`MJ_TEST_*`, `MJ_CHAOS_ISOLATED`) live behind the
`test-hooks` cargo feature in `mj-core/src/test_hooks.rs` and are not part of
the default build.
