#!/usr/bin/env bash
set -euo pipefail

# Exercise scripts/ci-platform-paths.sh against a scratch repository, so the
# test proves the real script and the real pattern lists make the right calls.

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
classifier="$repository_root/scripts/ci-platform-paths.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

repo="$test_root/repo"
mkdir -p "$repo"
cd "$repo"
git init -q --initial-branch=main
git config user.name 'platform path classifier test'
git config user.email 'ci-platform-paths@example.invalid'
git config commit.gpgsign false

mkdir -p docs mj-review/src mj-tui/src mj-core/src voice-worker/src
printf '%s\n' 'Guide for readers.' >docs/guide.md
printf '%s\n' 'pub fn review() {}' >mj-review/src/lib.rs
printf '%s\n' 'pub fn plain() {}' >mj-core/src/plain.rs
printf '%s\n' 'pub fn capture() {}' >voice-worker/src/backend.rs
printf '%s\n' '[package]' 'name = "mj-core"' >mj-core/Cargo.toml
printf '%s\n' '# lock file' >Cargo.lock
cat >mj-tui/src/lib.rs <<'RUST'
pub fn render() {}

#[cfg(target_os = "macos")]
fn mac() {}

pub fn finish() {}
RUST
git add docs/guide.md mj-review/src/lib.rs mj-tui/src/lib.rs \
    mj-core/src/plain.rs mj-core/Cargo.toml voice-worker/src/backend.rs Cargo.lock
git commit -qm 'base'
base="$(git rev-parse HEAD)"

make_commit() {
    local message="$1"
    shift
    git add -- "$@"
    git commit -qm "$message"
}

# Start every case from the base commit, so cases cannot leak into each other.
start_case() {
    git checkout -q --detach "$base"
}

expect_decision() {
    local platform="$1"
    local expected="$2"
    local description="$3"
    local base_revision="$4"
    local head_revision="$5"
    local stderr_file="$test_root/stderr.out"
    local actual
    actual="$(bash "$classifier" "$platform" "$base_revision" "$head_revision" 2>"$stderr_file")"
    if [[ "$actual" != "$expected" ]]; then
        printf 'not ok - %s %s: expected %s, got %s\n' "$platform" "$description" "$expected" "$actual" >&2
        sed 's/^/    /' "$stderr_file" >&2
        exit 1
    fi
    printf 'ok - %s %s\n' "$platform" "$description"
}

start_case
printf '%s\n' 'More guide text.' >>docs/guide.md
make_commit 'docs only' docs/guide.md
expect_decision macos false 'docs-only change' "$base" HEAD
expect_decision windows false 'docs-only change' "$base" HEAD

start_case
printf '%s\n' 'pub fn review_more() {}' >>mj-review/src/lib.rs
make_commit 'neutral rust edit' mj-review/src/lib.rs
expect_decision macos false 'platform-neutral Rust edit' "$base" HEAD
expect_decision windows false 'platform-neutral Rust edit' "$base" HEAD

start_case
printf '%s\n' 'pub fn capture_more() {}' >>voice-worker/src/backend.rs
make_commit 'voice worker edit' voice-worker/src/backend.rs
expect_decision macos true 'voice-worker edit' "$base" HEAD
expect_decision windows false 'path listed only for macOS' "$base" HEAD

start_case
mkdir -p mj-chat/src
printf '%s\n' 'pub fn speech() {}' >mj-chat/src/speech.rs
make_commit 'speech transport' mj-chat/src/speech.rs
expect_decision macos true 'new mj-chat speech transport' "$base" HEAD

start_case
printf '%s\n' '# lock note' >>Cargo.lock
make_commit 'lock edit' Cargo.lock
expect_decision macos true 'Cargo.lock edit' "$base" HEAD
expect_decision windows true 'Cargo.lock edit' "$base" HEAD

start_case
printf '%s\n' '# manifest note' >>mj-core/Cargo.toml
make_commit 'manifest edit' mj-core/Cargo.toml
expect_decision macos true 'crate manifest edit' "$base" HEAD

start_case
mkdir -p .github/workflows
printf '%s\n' 'name: Docs' >.github/workflows/docs.yml
make_commit 'docs workflow' .github/workflows/docs.yml
expect_decision macos true 'new CI workflow' "$base" HEAD

start_case
printf '%s\n' 'fn main() {}' >mj-core/build.rs
make_commit 'build script' mj-core/build.rs
expect_decision macos true 'new crate build script' "$base" HEAD

start_case
cat >>mj-core/src/plain.rs <<'RUST'

#[cfg(target_os = "macos")]
fn mac_only() {}
RUST
make_commit 'new cfg gate' mj-core/src/plain.rs
expect_decision macos true 'changed file gains a macOS cfg gate' "$base" HEAD
expect_decision windows false 'changed file gains a macOS cfg gate' "$base" HEAD

start_case
cat >>mj-core/src/plain.rs <<'RUST'

#[cfg(not(target_os = "linux"))]
fn open_browser() {}
RUST
make_commit 'non-Linux gate' mj-core/src/plain.rs
expect_decision macos true 'changed file gains a non-Linux cfg gate' "$base" HEAD

# The head drops the cfg gate the base still has, so only the base tree scan
# can see the macOS behavior this edit touched.
start_case
cat >mj-tui/src/lib.rs <<'RUST'
pub fn render() {}

fn mac() {}

pub fn finish() {}

pub fn later() {}
RUST
make_commit 'drop cfg gate' mj-tui/src/lib.rs
expect_decision macos true 'edit that removes an existing macOS cfg gate' "$base" HEAD

start_case
git rm -q mj-tui/src/lib.rs
git commit -qm 'delete cfg gate file'
expect_decision macos true 'deleted file that held a macOS cfg gate' "$base" HEAD

start_case
git mv voice-worker/src/backend.rs mj-review/src/backend.rs
git commit -qm 'move voice worker file'
expect_decision macos true 'move out of a listed directory' "$base" HEAD

expect_decision macos true 'all-zero base revision' 0000000000000000000000000000000000000000 HEAD
expect_decision macos true 'unknown base revision' 1111111111111111111111111111111111111111 HEAD
expect_decision macos true 'empty base revision' '' HEAD

start_case
printf '%s\n' 'Mixed doc.' >'docs/a b.md'
printf '%s\n' 'pub fn neutral_more() {}' >>mj-review/src/lib.rs
make_commit 'mixed change' 'docs/a b.md' mj-review/src/lib.rs
expect_decision macos false 'mixed change with a space in a filename' "$base" HEAD

start_case
printf '%s\n' 'pub fn accented() {}' >'voice-worker/src/é.rs'
make_commit 'accented voice worker file' 'voice-worker/src/é.rs'
expect_decision macos true 'new voice-worker file with a non-ASCII name' "$base" HEAD

start_case
printf '%s\n' 'Accented doc.' >'docs/résumé.md'
make_commit 'accented docs file' 'docs/résumé.md'
expect_decision macos false 'non-ASCII name outside the listed paths' "$base" HEAD

# Windows gates, written the ways the workspace writes them.
windows_gate_case() {
    local description="$1"
    start_case
    cat >>mj-core/src/plain.rs
    make_commit "$description" mj-core/src/plain.rs
    expect_decision windows true "changed file gains $description" "$base" HEAD
    expect_decision macos false "changed file gains $description" "$base" HEAD
}

windows_gate_case 'a cfg(windows) gate' <<'RUST'

#[cfg(windows)]
fn windows_only() {}
RUST

windows_gate_case 'a one-line cfg_attr(windows, ignore) attribute' <<'RUST'

#[test]
#[cfg_attr(windows, ignore = "#1300: fixture worker")]
fn fixture() {}
RUST

# rustfmt moves the predicate of a long cfg_attr onto its own line.
windows_gate_case 'a wrapped cfg_attr(windows, ignore) attribute' <<'RUST'

#[test]
#[cfg_attr(
    windows,
    ignore = "#1300: a long reason that rustfmt wraps onto the next line"
)]
fn fixture() {}
RUST

windows_gate_case 'a not(unix) gate' <<'RUST'

#[cfg(not(unix))]
fn elsewhere() {}
RUST

windows_gate_case 'a nested any(unix, windows) gate' <<'RUST'

#[cfg(any(unix, windows))]
fn desktop() {}
RUST

windows_gate_case 'a cfg!(windows) check' <<'RUST'

fn separator() -> char { if cfg!(windows) { '\\' } else { '/' } }
RUST

windows_gate_case 'a target_os = "windows" gate' <<'RUST'

#[cfg(target_os = "windows")]
fn windows_only() {}
RUST

start_case
cat >>mj-core/src/plain.rs <<'RUST'

pub fn spans(values: &[u8]) -> usize {
    let windows = values.windows(2).count();
    windows
}
RUST
make_commit 'a variable named windows' mj-core/src/plain.rs
expect_decision windows false 'a variable and slice method named windows' "$base" HEAD

start_case
printf '%s\n' 'Write-Host install' >install.ps1
make_commit 'installer' install.ps1
expect_decision windows true 'PowerShell installer edit' "$base" HEAD
expect_decision macos false 'PowerShell installer edit' "$base" HEAD

if bash "$classifier" linux "$base" HEAD >/dev/null 2>"$test_root/stderr.out"; then
    printf '%s\n' 'not ok - an unknown platform was accepted' >&2
    exit 1
fi
printf '%s\n' 'ok - unknown platform is a usage error'

printf '%s\n' 'platform path classifier tests passed'
