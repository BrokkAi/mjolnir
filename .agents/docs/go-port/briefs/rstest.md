# The test support package: `go/rs/rstest`

No Rust crate stands behind this one: it gives ported tests what Rust's test
harness and `assert!` macros give Rust tests. Start after the formatter in
`go/rs` exists (assertion messages print values with Rust's `Debug`).
`go/rs/rstest/oracle` already exists; do not change its interface.

Read rule 11 of `.agents/docs/go-port/translation-rules.md`, then look at how
the Rust tests are written (the 6,144 tests use `assert_eq!` about 14,700
times, `assert!` 14,000, `unwrap()` constantly, `#[should_panic]`, `matches!`,
`tempfile::tempdir()`, `include_str!` fixtures, and
`mj_core::golden::assert_golden`).

- `AssertEq(t, left, right, messageAndArgs...)`, `AssertNe`, `Assert(t, cond,
  messageAndArgs...)`: compare with `rs.Eq`; on failure report exactly what
  Rust's panic message says (`assertion `left == right` failed: <message>`,
  then `  left: <Debug>` and ` right: <Debug>`), with the Go file and line of
  the caller, and stop the test as a panic does (`t.FailNow`), running the
  test's deferred calls. `AssertMatches(t, value, func(v) bool)` for
  `assert!(matches!(..))`.
- A panic inside a test (from `rs.Unwrap`, `rs.Panicf`, an index out of
  range) must fail that test with Rust's panic text and let the remaining
  tests run, as `cargo test` does: provide `rstest.Run(t, func())` or a
  `defer rstest.Recover(t)` idiom, decide which, and document the one line a
  ported test starts with.
- `ShouldPanic(t, expectedSubstring, func())` for `#[should_panic(expected =
  "..")]` (and without `expected`): fails if no panic or if the message does
  not contain the substring, with cargo's wording.
- `Ignored(t, reason)`: skips unless `MJ_RUN_IGNORED=1` (cargo's `--ignored`).
- `RepoRoot(t)`, `RepoPath(t, "mj-core/tests/fixtures/x")`, `ReadFixture`,
  and `ManifestDir(t, "mj-core")` for `env!("CARGO_MANIFEST_DIR")`.
- `Binary(t, "mj")` / `Binary(t, "mj-worker")`: builds the Go program once per
  test run (with the test tags) into a temporary directory and returns its
  path, for tests that Rust writes with `env!("CARGO_BIN_EXE_mj")`.
- Environment helpers matching what Rust tests do by hand: `SetEnv(t, k, v)`
  restoring on cleanup, a process-wide lock for tests that change the
  environment or the current directory.
- A check, run in `TestMain` helpers or as a function tests can call, that no
  goroutine started by the test is still running and no handle was left
  undropped (`MJ_RS_LEAK_CHECK`), for the packages that opt in.

Tests: each assertion helper's failure output compared with the real `cargo
test` output for the same failing assertion (capture it once from a small
Rust test and record it under `testdata/`).
