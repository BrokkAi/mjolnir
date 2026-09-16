# Internal environment variables

These `MJ_*` variables are handoffs between one mj process and a child it
starts. They are not settings. Users do not set them, and the documentation
in `docs/` deliberately leaves them out. The documented settings are in
`docs/src/content/docs/configuration.md` under "Process and path overrides".

- `MJ_ORIGINAL_BASH_ENV`, `MJ_ORIGINAL_GIT_CONFIG_GLOBAL`: the worker records
  the values of `BASH_ENV` and `GIT_CONFIG_GLOBAL` it inherited before it
  replaces them for a harness shell, so the harness can restore them.
  Read in `mj-worker/src/worker_runtime/unix.rs`.
`MJ_CONTROLLER_LOCK_EXPECTED`, `MJ_CONTROLLER_LOCK_PROBE` and
`MJ_WORKER_BINARY_OVERRIDE_CHILD` appear in the source but only inside
`#[cfg(test)]` modules, where a test re-runs the test binary as a child.

Test-only hooks (`MJ_TEST_*`, `MJ_CHAOS_ISOLATED`) live behind the
`test-hooks` cargo feature in `mj-core/src/test_hooks.rs` and are not part of
the default build.
