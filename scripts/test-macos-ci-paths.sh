#!/usr/bin/env bash
set -euo pipefail

# Exercise scripts/macos-ci-paths.sh against a scratch repository, so the test
# proves the real script and the real pattern list make the right calls.

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
classifier="$repository_root/scripts/macos-ci-paths.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

repo="$test_root/repo"
mkdir -p "$repo"
cd "$repo"
git init -q --initial-branch=main
git config user.name 'macOS path classifier test'
git config user.email 'macos-ci-paths@example.invalid'
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
    local expected="$1"
    local description="$2"
    local base_revision="$3"
    local head_revision="$4"
    local stderr_file="$test_root/stderr.out"
    local actual
    actual="$(bash "$classifier" "$base_revision" "$head_revision" 2>"$stderr_file")"
    if [[ "$actual" != "$expected" ]]; then
        printf 'not ok - %s: expected %s, got %s\n' "$description" "$expected" "$actual" >&2
        sed 's/^/    /' "$stderr_file" >&2
        exit 1
    fi
    printf 'ok - %s\n' "$description"
}

start_case
printf '%s\n' 'More guide text.' >>docs/guide.md
make_commit 'docs only' docs/guide.md
expect_decision false 'docs-only change' "$base" HEAD

start_case
printf '%s\n' 'pub fn review_more() {}' >>mj-review/src/lib.rs
make_commit 'neutral rust edit' mj-review/src/lib.rs
expect_decision false 'platform-neutral Rust edit' "$base" HEAD

start_case
printf '%s\n' 'pub fn capture_more() {}' >>voice-worker/src/backend.rs
make_commit 'voice worker edit' voice-worker/src/backend.rs
expect_decision true 'voice-worker edit' "$base" HEAD

start_case
mkdir -p mj-chat/src
printf '%s\n' 'pub fn speech() {}' >mj-chat/src/speech.rs
make_commit 'speech transport' mj-chat/src/speech.rs
expect_decision true 'new mj-chat speech transport' "$base" HEAD

start_case
printf '%s\n' '# lock note' >>Cargo.lock
make_commit 'lock edit' Cargo.lock
expect_decision true 'Cargo.lock edit' "$base" HEAD

start_case
printf '%s\n' '# manifest note' >>mj-core/Cargo.toml
make_commit 'manifest edit' mj-core/Cargo.toml
expect_decision true 'crate manifest edit' "$base" HEAD

start_case
mkdir -p .github/workflows
printf '%s\n' 'name: Docs' >.github/workflows/docs.yml
make_commit 'docs workflow' .github/workflows/docs.yml
expect_decision true 'new CI workflow' "$base" HEAD

start_case
printf '%s\n' 'fn main() {}' >mj-core/build.rs
make_commit 'build script' mj-core/build.rs
expect_decision true 'new crate build script' "$base" HEAD

start_case
cat >>mj-core/src/plain.rs <<'RUST'

#[cfg(target_os = "macos")]
fn mac_only() {}
RUST
make_commit 'new cfg gate' mj-core/src/plain.rs
expect_decision true 'changed file gains a macOS cfg gate' "$base" HEAD

start_case
cat >>mj-core/src/plain.rs <<'RUST'

#[cfg(not(target_os = "linux"))]
fn open_browser() {}
RUST
make_commit 'non-Linux gate' mj-core/src/plain.rs
expect_decision true 'changed file gains a non-Linux cfg gate' "$base" HEAD

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
expect_decision true 'edit that removes an existing macOS cfg gate' "$base" HEAD

start_case
git rm -q mj-tui/src/lib.rs
git commit -qm 'delete cfg gate file'
expect_decision true 'deleted file that held a macOS cfg gate' "$base" HEAD

start_case
git mv voice-worker/src/backend.rs mj-review/src/backend.rs
git commit -qm 'move voice worker file'
expect_decision true 'move out of a listed directory' "$base" HEAD

expect_decision true 'all-zero base revision' 0000000000000000000000000000000000000000 HEAD
expect_decision true 'unknown base revision' 1111111111111111111111111111111111111111 HEAD
expect_decision true 'empty base revision' '' HEAD

start_case
printf '%s\n' 'Mixed doc.' >'docs/a b.md'
printf '%s\n' 'pub fn neutral_more() {}' >>mj-review/src/lib.rs
make_commit 'mixed change' 'docs/a b.md' mj-review/src/lib.rs
expect_decision false 'mixed change with a space in a filename' "$base" HEAD

start_case
printf '%s\n' 'pub fn accented() {}' >'voice-worker/src/é.rs'
make_commit 'accented voice worker file' 'voice-worker/src/é.rs'
expect_decision true 'new voice-worker file with a non-ASCII name' "$base" HEAD

start_case
printf '%s\n' 'Accented doc.' >'docs/résumé.md'
make_commit 'accented docs file' 'docs/résumé.md'
expect_decision false 'non-ASCII name outside the listed paths' "$base" HEAD

printf '%s\n' 'macOS path classifier tests passed'
