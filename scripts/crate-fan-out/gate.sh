#!/bin/bash
# Run before pushing a crate move. Each check here has caught a mistake that
# the compiler and the tests did not: see "Surprises & Discoveries" in
# .agents/plans/fan-out-crates-under-size-budget.md.
#
# cargo-machete is the version CI pins (0.9.2). Install it once with
#   cargo install cargo-machete --version 0.9.2 --locked
# or point CARGO_MACHETE at a copy.
cd "$(dirname "$0")/../.." || exit 1
machete=${CARGO_MACHETE:-cargo-machete}
fail=0
step() {
    if "$@" > "$log" 2>&1; then echo "ok    $name"; else echo "FAIL  $name"; tail -15 "$log" | cut -c1-200; fail=1; fi
}
log=$(mktemp)
trap 'rm -f "$log"' EXIT
name="formatting"; step cargo fmt --all -- --check
name="clippy, whole workspace, warnings denied"; step cargo clippy --workspace --all-targets -- -D warnings
name="unused dependencies (cargo machete)"; step "$machete"
name="crate boundaries"; step node scripts/check-crate-boundaries.mjs
name="crate sizes"; step python3 scripts/check-crate-size.py
# .gitignore once hid a new file under a source directory named `targets`.
hidden=$(git ls-files --others --ignored --exclude-standard | grep -vE "^target|/target/|node_modules|__pycache__")
if [ -n "$hidden" ]; then echo "FAIL  source files hidden by ignore rules:"; echo "$hidden"; fail=1; else echo "ok    no source file hidden by ignore rules"; fi
untracked=$(git status --short | grep "^??")
if [ -n "$untracked" ]; then echo "FAIL  untracked files (add or remove them):"; echo "$untracked"; fail=1; else echo "ok    nothing untracked"; fi
exit $fail
