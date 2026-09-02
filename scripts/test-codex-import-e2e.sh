#!/usr/bin/env bash
# Run a genuine Codex import/resume cycle on a host with Podman.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
codex_home=${CODEX_HOME:-"$HOME/.codex"}
test_root=${MJ_IMPORT_E2E_CODEX_ROOT:-"${XDG_STATE_HOME:-"$HOME/.local/state"}/mjolnir/import-e2e/codex-native"}

if [[ ! -d "$codex_home" ]]; then
    echo "Codex home does not exist: $codex_home" >&2
    exit 1
fi

export CODEX_HOME="$codex_home"
export MJ_IMPORT_E2E_ROOT="$test_root"
export MJ_IMPORT_E2E_CODEX_SESSION="${MJ_IMPORT_E2E_CODEX_SESSION:-019feb6c-5ffc-7c12-ad99-bdeaeb6be79d}"
export MJ_IMPORT_E2E_CODEX_REPOSITORY="${MJ_IMPORT_E2E_CODEX_REPOSITORY:-BrokkAi/mjolnir}"
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
cargo test --test import_e2e imported_codex_session_resumes_natively -- --ignored --nocapture
