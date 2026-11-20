# Brief: porting the bodies and tests of Rust files

You are porting Mjolnir from Rust to Go. The message that sent you here names
a crate and a set of its Rust files. Their Go declarations already exist and
compile: every function has the right name and signature and a body that
panics with `rs.Todo`. You replace each of those bodies with the translation
of the Rust body, and you port the files' tests. Other people are doing the
same for other files of this crate, in this same working tree, right now.

The Go code must do what the Rust code does on every path. You translate; you
do not redesign, simplify, modernise or improve. Read rule 0 of the rulebook
twice.

## Read first

1. `.agents/docs/go-port/translation-rules.md`, all of it, carefully. It is
   the specification of how each Rust construct is written in Go.
2. `.agents/docs/go-port/tokio-contract.md` if any of your files is async.
3. For each of your Rust files: `python3 go/port/portmap.py file <path.rs>`
   (its Go file, the Go name of every item and test, the files it needs), the
   whole Rust file including its tests, and its Go declarations file.
4. The `doc.go` of each runtime package your files call (`go/rs/...`,
   `go/crates/...`), for the Go names of library functions. Never guess one:
   open the package.

Do not start agents or forks of your own. The coordinator decides how many
units run at once (the harness allows twenty in total, helpers included), and
every one of them is paid for by the owner.

## How to work

For each file, top to bottom:

1. Translate each function body in place of its `panic(rs.Todo(..))`, in the
   order of the Rust file, carrying every comment over (inside bodies too).
   When a function is done, delete its `// rust: <signature>` line.
2. Keep every string literal byte for byte: format strings (rule 8), error
   messages, log messages, keys, commands. A changed literal is a changed
   behaviour.
3. For every value whose type has a `Drop` that matters (rule 4), decide where
   Rust drops it and write that. For every lock, decide how long Rust holds
   the guard and hold it exactly that long. For every `select!`, keep the arm
   order and the `biased` flag. These three are where translations go wrong
   without failing to compile.
4. Expand each `macro_rules!` use as the macro would expand.
5. Port the file's tests into its `_test.go` file: every test, under the name
   `portmap.py file` gives, with every assertion and the same expected values,
   reading fixtures and goldens from their Rust locations (rule 11). A test
   helper defined in the Rust test module is ported beside the tests.
6. Build, vet and run your tests after each file, not at the end:

       go build -tags 'mj_debug mj_test_support mj_test_hooks mj_test_identity mj_golden mj_github_history' ./go/<package>/
       go/port/gotest.sh ./go/<package>/ -run '^Test<ModulePrefix>_' -count=1

   Keep the package compiling at every step; a broken build stops everyone
   else working in it. Never leave a file half-edited between tool calls.

What you may and may not touch:

- Your files' `.go` and `_test.go`. Nothing else in the crate.
- If a declaration in one of your files is wrong (a signature that cannot
  express what the Rust function does), fix it and every caller, and say so in
  your report.
- If a declaration in someone else's file is wrong, do not edit it: report it.
- If a runtime package lacks a function you need, add it there in one edit
  that leaves that package compiling, with a test, and report it. Do not
  write a local substitute.
- Do not commit. Do not run `go mod tidy`, `go get`, or `gofmt -w` outside
  your files.

When something will not go:

- A body you cannot translate stays `panic(rs.Todo("<rust path>: <reason>"))`.
  Never write a body that looks finished and is not, and never return a
  made-up value to get past a test.
- A test that fails because a function in another file is still `rs.Todo`
  stays as written, unskipped. List it in your report as blocked, with the
  function it waits for. It will be run again when that file lands.
- A test that fails for any other reason means the port is wrong. Find the
  difference between the Rust and the Go. Do not change the test's expected
  value, loosen its comparison, or delete a case.
- If the Rust code itself looks wrong, port it as it is and report it.

## Report

Lead with the state in two sentences. Then:

- per file: bodies ported and `rs.Todo` left (with reasons); tests ported,
  passing, failing, blocked (naming what each blocked test waits for);
- the exact test command you ran last and its summary line;
- declarations you changed, and declarations elsewhere that are wrong;
- functions you added to runtime packages;
- constructs the rulebook did not cover, and what you did;
- places where the Rust code looks wrong.

Under 500 words.
