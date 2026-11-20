#!/usr/bin/env bash
# Runs `go test` the way the port's tests must run:
#
#   - with the build tags that stand for the Cargo features the Rust tests
#     enable (translation-rules.md, rule 10);
#   - without C;
#   - without the instance variables a Mjolnir session exports, which would
#     point a test at the live default instance (AGENTS.md, Testing Guidelines);
#   - in a UTF-8 English locale, so Git and the C library answer as the
#     goldens expect.
#
# Usage: go/port/gotest.sh [go test flags] <packages>
#        go/port/gotest.sh ./go/rs/... -run TestOption
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

unset MJ_CONFIG_DIR MJ_DATA_DIR MJ_SESSION_ID MJ_SESSION_BASH_ENV
export LC_ALL=C.UTF-8 LANGUAGE=en
export CGO_ENABLED=0

exec go test -tags 'mj_debug mj_test_support mj_test_hooks mj_test_identity mj_golden mj_github_history' "$@"
