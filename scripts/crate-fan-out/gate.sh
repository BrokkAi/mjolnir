#!/bin/bash
# Run before pushing. Each check here has caught a mistake that
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
# Check-in CI builds for Linux only. Code that only Unix or only Linux compiles
# leaves its imports and helpers unused elsewhere, and the Windows and macOS
# lanes, which gate a release, reject that. These two run when the cross
# compilers are present: llvm-mingw under MJ_LLVM_MINGW and the `zcc`, `zcxx`
# and `zar` wrappers around `zig` under MJ_ZIG_CC (see "Resume here" in the
# fan-out plan for how they were set up).
mingw=${MJ_LLVM_MINGW:-$HOME/.cache/mjolnir/build/llvm-mingw/dist}/bin
zig=${MJ_ZIG_CC:-$HOME/.cache/mjolnir/build/zig}
if [ -x "$mingw/x86_64-w64-mingw32-clang" ]; then
    name="clippy for Windows"
    CC_x86_64_pc_windows_gnu=$mingw/x86_64-w64-mingw32-clang \
    CXX_x86_64_pc_windows_gnu=$mingw/x86_64-w64-mingw32-clang++ \
    AR_x86_64_pc_windows_gnu=$mingw/llvm-ar \
    CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=$mingw/x86_64-w64-mingw32-clang \
        step cargo clippy --target x86_64-pc-windows-gnu --all-targets --keep-going -- -D warnings
else
    echo "skip  clippy for Windows (no llvm-mingw at $mingw)"
fi
if [ -x "$zig/zcc" ]; then
    name="clippy for macOS"
    CC_aarch64_apple_darwin=$zig/zcc CXX_aarch64_apple_darwin=$zig/zcxx AR_aarch64_apple_darwin=$zig/zar \
        step cargo clippy --target aarch64-apple-darwin --all-targets --keep-going -- -D warnings
else
    echo "skip  clippy for macOS (no zig wrappers at $zig)"
fi
name="unused dependencies (cargo machete)"; step "$machete"
name="crate boundaries"; step node scripts/check-crate-boundaries.mjs
name="crate sizes"; step python3 scripts/check-crate-size.py
# .gitignore once hid a new file under a source directory named `targets`.
hidden=$(git ls-files --others --ignored --exclude-standard | grep -vE "^target|/target/|node_modules|__pycache__")
if [ -n "$hidden" ]; then echo "FAIL  source files hidden by ignore rules:"; echo "$hidden"; fail=1; else echo "ok    no source file hidden by ignore rules"; fi
untracked=$(git status --short | grep "^??")
if [ -n "$untracked" ]; then echo "FAIL  untracked files (add or remove them):"; echo "$untracked"; fail=1; else echo "ok    nothing untracked"; fi
exit $fail
