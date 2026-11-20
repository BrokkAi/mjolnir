# `go/crates/anyhow` (anyhow 1.0.102), then `go/crates/tracing` and `go/crates/tracingsubscriber` (tracing 0.1.44, tracing-subscriber 0.3.23)

Oracle modules `go/port/oracle/src/anyhow_oracle.rs` (feature `anyhow`) and
`tracing_oracle.rs` (feature `tracing`). Read rules 5 and 8 of the translation
rules closely: they show how product code calls these packages. Both format
text through `rs.Format` and `rs.Formatter`; if the formatter in `go/rs` is not
finished when you need it, test through plain strings and say which tests wait
for it.

## anyhow

About 350 files use it: `Result`, `Error`, `anyhow!` (650), `bail!` (1,750),
`ensure!`, `.context()`/`.with_context()` (3,700), `{:#}` (1,500),
`downcast_ref`/`downcast`/`is` (170), `chain()`, `root_cause()`. An
`anyhow::Error` is a Go `error` here.

- `anyhow.Msg`, `anyhow.Errorf(format, args...)` with a Rust format string,
  `anyhow.New`, `anyhow.Context(err, msg)` and `anyhow.WithContext(err,
  func() string)` (both nil for a nil err; the closure is not called then),
  `anyhow.Chain`, `anyhow.RootCause`, `anyhow.DowncastRef[T]` and
  `anyhow.Is[T]` looking through context layers as anyhow does (including a
  typed value used as context), `anyhow.Downcast[T]`.
- The text forms exactly: `{}` the outermost message; `{:#}` the chain joined
  by `: `; `{:?}` the message then `Caused by:` with anyhow's indentation and
  numbering, with no backtrace section (confirm how the crate behaves without
  `RUST_BACKTRACE`); `{:#?}`. As `FmtDisplay`/`FmtDebug` and as plain
  `anyhow.DisplayString`, `AlternateString`, `DebugString`; `Error()` is the
  `{}` form.
- Interoperation with Go errors: an `rsio.Error` or any `error` at the root,
  foreign errors contributing their `Unwrap()` sources as `source()` does, and
  `errors.Is`/`errors.As` working through contexts.

Tests: the crate's tests that apply, and differential tests on the four forms
over generated chains (1 to 6 levels; empty, multi-line, brace and Unicode
messages; typed and string contexts; io errors at the root).

## tracing and tracing-subscriber

About 310 files log with `warn!`, `debug!`, `info!`, `error!`, `trace!`, some
spans and `#[instrument]`, configured through the `fmt` layer with an
`EnvFilter` and Mjolnir's own non-blocking rotating writer
(`mj-cli/src/logging.rs`, `mj-core/src/logging.rs`,
`mj-worker-bin/src/main.rs`: read exactly which options they set). Log lines
are read by tests (`mj-cli/tests/logging.rs`) and compared by people, so the
same lines must be written.

- `tracing`: level functions taking the target (each ported file declares
  `const <prefix>_TARGET = "<rust module path>"`), a Rust format string, and
  arguments mixing format arguments with fields (plain, `%` Display and `?`
  Debug). Choose the field-helper names and update only the example in rule 8
  of the translation rules to match. Events without messages, `Enabled`,
  `Level`, spans (`InfoSpan` and friends, `Enter` with a guard, `in_scope`,
  `record`, `Span::current`, instrumenting futures; the span stack per
  goroutine behind a small interface the async runtime will later back),
  `Subscriber`/`Dispatch` (`set_global_default`, scoped `with_default` for
  tests).
- `tracingsubscriber`: the `fmt` layer's `Full` format exactly (timestamp of
  the timer Mjolnir configures and the default, level padding and colours,
  span context prefix, target, message, fields in order with exact value
  rendering), other formats only if used; `EnvFilter` with its directive
  grammar, matching, precedence, invalid-directive warning, `try_new`, `new`,
  `from_env`, `try_from_default_env`, `add_directive`, parse errors with exact
  text; `MakeWriter`, `with_writer`, and the composition API as Mjolnir calls
  it.

Tests: the crates' tests that apply, and differential tests: generated events
(levels, targets, formatted messages, fields of every kind, zero to three
nested spans) through the real `fmt` layer configured each way Mjolnir does,
with an injected fixed timer, comparing bytes; generated filter directives
against generated events for the enabled decision and parse errors.

Report per package.
