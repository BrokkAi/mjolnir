# Brief: building one runtime package of the Go port

You are building one Go package that stands in for a Rust library, as part of
porting Mjolnir from Rust to Go. The message that sent you here names the
package, the Rust library and anything specific to it. This brief is the part
that is the same for every package.

## Read first

1. `.agents/docs/go-port/runtime-packages.md`: "Rules for every runtime
   package", your package's row, and "The oracle".
2. `.agents/docs/go-port/translation-rules.md`: all of it. Sections 2 to 5 and
   7 to 8 fix names and types; your package must present them the way product
   code will call them.
3. `go/rs/` (`option.go`, `result.go`, `panic.go`, `registry.go`,
   `fmt_api.go`, `formatter_pending.go`, `reflect_api.go`, `containers.go`):
   the core your package builds on.

## Where things are

- Repository root: the directory holding `go.mod`. Run every command from it.
- The Rust crate you are reproducing: `~/.cargo/registry/src/*/<crate>-<version>/`.
  The Rust standard library: `~/.cache/mjolnir/build/go-port/rust-src/src/rust/library/`
  (`core`, `alloc`, `std`, and their tests in `coretests`, `alloctests`).
  If either is missing, run `go/port/fetch-reference.sh`.
- How Mjolnir uses the library: the Rust sources in `mj-*/` and
  `voice-worker/`. Start with
  `grep -rn '<crate_name>::' --include='*.rs' mj-* voice-worker | less` and
  `grep -rln 'use <crate_name>' --include='*.rs' mj-*`, then read enough call
  sites to know which types, methods and behaviours matter.

## What to produce

- The package under `go/crates/<name>/` (or `go/rs/<name>/`), with a `doc.go`
  that says what it stands in for, the crate version, what is covered, and
  what is not.
- Tests: the crate's own tests for the parts you cover, ported, and
  differential tests against the oracle with a recorded corpus (below).
- Nothing else. Do not edit other packages, `go.mod`, `go.sum`, the plan or the
  rule documents. Other people are building sibling packages in this same
  working tree at this moment.

## How to work

- Follow the Rust source. When you are unsure what Rust does, read the source
  or ask the oracle; do not rely on memory or on documentation prose.
- Keep the package compiling at every step; never leave a file half-written
  between tool calls. Build and test only your package:
  `go build ./go/crates/<name>/ && go vet ./go/crates/<name>/` and
  `go/port/gotest.sh ./go/crates/<name>/`.
- Do not run `go mod tidy`, `go get`, or `gofmt -w` on anything outside your
  package. The Go modules available are the ones already in `go.mod`. If you
  need one that is not there, stop and say so in your report.
- No C: the package must build with `CGO_ENABLED=0`, and for
  `GOOS=linux`, `darwin` and `windows` (`GOOS=windows go build ./go/crates/<name>/`).
- The formatter (`rs.Format`, `rs.Formatter`) and the structural operations
  (`rs.Clone`, `rs.Eq`, `rs.Cmp` beyond numbers and strings) are being written
  by someone else right now and panic with "not yet ported" until they land.
  Give every type that implements `Display` a `String() string` method holding
  the logic, and a `FmtDisplay(f *rs.Formatter) error` that calls
  `f.Pad(x.String())` (or uses the formatter's flags if the Rust impl does).
  Inside your package build text with `strconv` and `strings`.
- Anything you cannot finish is `panic(rs.Todo("<rust path>: <reason>"))`, never
  a plausible-looking stub.
- Do not commit.

## The oracle

`go/port/oracle` is a Rust program that runs the real crate. Your module is
already registered: `go/port/oracle/src/<module>.rs`, built on its own with

    cd go/port/oracle && cargo build --release --features <feature> --target-dir target/<feature>

Edit only your module's file there. It holds
`pub fn handle(request: &serde_json::Value) -> serde_json::Value`; give it one
`"op"` per behaviour you compare, and reply with plain JSON. A panic in the
Rust code is caught and replied as `{"panic": "<message>"}`.

On the Go side:

    func TestDiffAgainstRust(t *testing.T) {
        oracle.Check(t, "<feature>", "diff", func(yield func(any)) {
            // Deterministic: fixed cases first, then seeded random ones.
            yield(map[string]any{"op": "unified", "old": "a\nb\n", "new": "a\nc\n"})
        }, func(request json.RawMessage) any {
            // Decode the request, run the Go code, return what Rust replies with.
        })
    }

with `oracle` being `github.com/BrokkAi/mjolnir/go/rs/rstest/oracle`. Record
the corpus once with `MJ_ORACLE=record go/port/gotest.sh ./go/crates/<name>/`;
it is written gzip-compressed to `testdata/oracle/<corpus>.jsonl.gz` and is
committed with the package. Afterwards plain `go/port/gotest.sh` replays it and
`MJ_ORACLE=live` compares against Rust again. Aim the generated inputs at the
edges: empty input, Unicode, very long input, the boundaries the Rust code
branches on. A few hundred to a few thousand cases per corpus is right. The
corpora are checked out by everyone who clones the repository: keep a
package's recorded corpora under about 1 MB compressed in total, record a
deterministic subset if the full set is larger, and run the full set under
`MJ_ORACLE=live`.

## Report

End with a report for the person merging your work, who has not seen your
session. Lead with the state in one or two sentences. Then, briefly:

- what is covered and what is not (by Rust path);
- the exact test command you ran last and its result line, and whether the
  live oracle comparison passes;
- every `rs.Todo` left, with the reason;
- anything in the Rust library's behaviour that surprised you and that product
  code will trip over;
- anything you needed from `go/rs` or another package that was missing.

Keep it under 400 words. Details belong in `doc.go` and the tests.
