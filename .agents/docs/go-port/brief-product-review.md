# Brief: reviewing ported files against their Rust source

You are checking part of a port of Mjolnir from Rust to Go. The message that
sent you here names a crate and a set of its Rust files. Someone else has
translated them into Go. You did not write the translation, and that is the
point: read it as someone who expects to find mistakes.

A port is correct when the Go code does what the Rust code does on every
path: same results, same text, same order, same behaviour on failure and on
cancellation. Tests passing is necessary and far from sufficient; most of the
mistakes below pass every test.

## Read first

`.agents/docs/go-port/translation-rules.md` in full, and
`.agents/docs/go-port/tokio-contract.md` if any file is async. For each file,
`python3 go/port/portmap.py file <path.rs>`, then the Rust file and the Go
file side by side.

## What to look for

Go through each function with both texts open. For each one ask:

- Is every branch, arm, early return and loop exit there, in the same order,
  with the same condition? Is a `match` that Rust checked as exhaustive still
  covering every variant?
- Is every string literal identical, including format strings, error and log
  text? Is each `rs.Format` given the arguments its string names?
- Numbers: the same integer widths; `saturating_`, `checked_`, `wrapping_`
  arithmetic translated to the matching helper and not to a bare operator;
  `as` casts from floats through the cast helpers; no `int` where the value
  is serialized as a wider or unsigned type.
- Strings and paths: a Unicode-sensitive method translated through `strs` and
  not through `strings`; no `filepath`; lengths and indexes in bytes where
  Rust's are.
- Copies and sharing: a Rust `.clone()` translated to `rs.Clone` (or a plain
  copy only where the type holds no slice, map or pointer); no ranging over a
  slice by value where Rust borrows mutably; no struct copied where Rust moves
  a value that is later mutated through the original.
- Drop: for every value that must be dropped (rule 4), is it dropped where
  Rust drops it, on every path including early returns? Is a lock guard held
  exactly as long as in Rust: not longer (deadlock), not shorter (race)? Is
  anything deferred that Rust drops earlier, or dropped that Rust moved away?
- Async: `ctx` passed down, never stored or inspected; each `select!` with its
  arms in order and `biased` preserved; futures that Rust creates lazily not
  started early; clean-up that Rust does in a destructor written as a `defer`
  and not as code after an awaiting call.
- Errors: context added with `anyhow.Context` in the same places, with the
  same text; `{}`, `{:#}` and `{:?}` of an error not interchanged; a typed
  error still matchable by `anyhow.DowncastRef`.
- Collections: `BTreeMap` order relied on; `min_by_key`/`max_by_key`, stable
  and unstable sorts as the rulebook says; an entry mutated through a pointer
  into the map and not through a copy.
- Platform code under the right build tag, with each platform having exactly
  one definition.
- Tests: every Rust test present under its mapped name (`python3
  go/port/status.py --missing-tests <crate>`), every assertion present, every
  expected value unchanged, nothing skipped, nothing weakened to pass.

Then run the checks:

    go build -tags 'mj_debug mj_test_support mj_test_hooks mj_test_identity mj_golden mj_github_history' ./go/<package>/
    go vet   -tags '...same...' ./go/<package>/
    go/port/gotest.sh ./go/<package>/ -run '^Test<ModulePrefix>_' -count=1

## What to do with what you find

Fix a mistake when the correct translation is unambiguous, in that file only,
keeping the package compiling, and re-run its tests. Leave it and report it
when fixing needs a decision, touches another file, or you are not sure the
Go is wrong. Do not commit.

## Report

Lead with a verdict for each file: matches the Rust, or does not. Then every
finding as one line: `file:line`, what the Rust does, what the Go did, and
whether you fixed it. Then the test command you ran last and its summary
line. Say plainly which functions you compared line by line and which you only
skimmed: an honest "I did not check these" is worth more than a clean report.
Under 600 words.
