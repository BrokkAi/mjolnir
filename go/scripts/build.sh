#!/usr/bin/env bash
# Build the Go port's binaries into go/bin. The marker directory tells the
# binaries they are a development build, so they refuse to start, replace,
# stop or migrate the default instance (see EnsureMayControlStore).
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p bin/.mj-development-build
go build -o bin/ ./cmd/...
