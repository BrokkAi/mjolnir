#!/usr/bin/env bash
# Run a genuine Claude Code import/resume cycle on a host with Podman.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
real_claude_home=${CLAUDE_CONFIG_DIR:-"$HOME/.claude"}
test_root=${MJ_IMPORT_E2E_ROOT:-"${XDG_STATE_HOME:-"$HOME/.local/state"}/mjolnir/import-e2e"}

if [[ ! -d "$real_claude_home" ]]; then
    echo "Claude home does not exist: $real_claude_home" >&2
    exit 1
fi

export CLAUDE_CONFIG_DIR="$real_claude_home"
export MJ_IMPORT_E2E_ROOT="$test_root"
export MJ_IMPORT_E2E_REPOSITORY="${MJ_IMPORT_E2E_REPOSITORY:-BrokkAi/mjolnir}"
export MJ_IMPORT_E2E_IMAGE="${MJ_IMPORT_E2E_IMAGE:-localhost/mjolnir/agent-dev:latest}"
# Keep the test's imported state and archive separate from the user's Mjolnir data.
export MJ_CONFIG_DIR="$test_root/config/mjolnir"
export MJ_DATA_DIR="$test_root/data/mjolnir"
export MJ_WORKER_BINARY="${MJ_WORKER_BINARY:-$repo_root/target/x86_64-unknown-linux-musl/debug/mj}"

mkdir -p "$test_root"
cd "$repo_root"
if [[ ! -x "$MJ_WORKER_BINARY" ]]; then
    cargo build --target x86_64-unknown-linux-musl
fi
cargo test -p brokk-mjolnir --test import_e2e imported_claude_session_resumes_natively -- --ignored --nocapture
