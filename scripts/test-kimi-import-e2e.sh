#!/usr/bin/env bash
# Run a genuine Kimi Code import/resume cycle on a host with Podman.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
kimi_home=${KIMI_CODE_HOME:-"$HOME/.kimi-code"}
test_root=${MJ_IMPORT_E2E_KIMI_ROOT:-"${XDG_STATE_HOME:-"$HOME/.local/state"}/mjolnir/import-e2e/kimi-native"}

if [[ ! -d "$kimi_home" ]]; then
    echo "Kimi Code home does not exist: $kimi_home" >&2
    exit 1
fi

export KIMI_CODE_HOME="$kimi_home"
export MJ_IMPORT_E2E_ROOT="$test_root"
export MJ_IMPORT_E2E_KIMI_SESSION="${MJ_IMPORT_E2E_KIMI_SESSION:-session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2}"
export MJ_IMPORT_E2E_KIMI_REPOSITORY="${MJ_IMPORT_E2E_KIMI_REPOSITORY:-MoonshotAI/kimi-code}"
export MJ_IMPORT_E2E_IMAGE="${MJ_IMPORT_E2E_IMAGE:-localhost/mjolnir/agent-dev:latest}"
# Keep the test's imported state and archive separate from the user's Mjolnir data.
export MJ_CONFIG_DIR="$test_root/config/mjolnir"
export MJ_DATA_DIR="$test_root/data/mjolnir"
export MJ_WORKER_BINARY="${MJ_WORKER_BINARY:-$repo_root/target/worker/x86_64-unknown-linux-musl/debug/mj-worker}"

mkdir -p "$test_root"
cd "$repo_root"
if [[ ! -x "$MJ_WORKER_BINARY" ]]; then
    cargo build --target-dir target/worker --target x86_64-unknown-linux-musl -p brokk-mj-worker --bin mj-worker
fi
cargo test -p brokk-mjolnir --test import_e2e imported_kimi_session_resumes_natively -- --ignored --nocapture
