#!/bin/sh
# sshd forced command for the Oz key. SSH_ORIGINAL_COMMAND is
# "<40-hex intro sha> <32-hex mj session id | unknown>".
# Exit 64: malformed command. Exit 65: no fixture for that sha (the fixtures present are the allowlist).
# Host layout (override with SITE_CONTEXT_ROOT for local tests): /srv/review-arms/oz/{fixtures,logs,site_context_server.py}
ROOT="${SITE_CONTEXT_ROOT:-/srv/review-arms/oz}"
set -f
set -- ${SSH_ORIGINAL_COMMAND-}
if [ "$#" -ne 2 ]; then echo "serve-site-context: malformed command" >&2; exit 64; fi
INTRO="$1"; SESSION="$2"
case "$INTRO" in
  *[!0-9a-f]*|"") echo "serve-site-context: malformed sha" >&2; exit 64 ;;
esac
[ "${#INTRO}" -eq 40 ] || { echo "serve-site-context: malformed sha" >&2; exit 64; }
if [ "$SESSION" != unknown ]; then
  case "$SESSION" in
    *[!0-9a-f]*) echo "serve-site-context: malformed session" >&2; exit 64 ;;
  esac
  [ "${#SESSION}" -eq 32 ] || { echo "serve-site-context: malformed session" >&2; exit 64; }
fi
FIXTURE="$ROOT/fixtures/$INTRO.json"
[ -f "$FIXTURE" ] || { echo "serve-site-context: no fixture" >&2; exit 65; }
SITE_CONTEXT_LOG_DIR="$ROOT/logs"
export SITE_CONTEXT_LOG_DIR
exec python3 "$ROOT/site_context_server.py" "$FIXTURE" "$INTRO" "$SESSION"
