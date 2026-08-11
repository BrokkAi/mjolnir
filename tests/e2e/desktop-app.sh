#!/bin/sh
# End-to-end lifecycle check for the `mj app` desktop shell on a headless
# display: the app-owned server and WebView shell come up, the process stays
# alive with the window open, and a single SIGTERM closes the window, drains
# the server, and exits 0. Requires a binary built with --features desktop-app
# and xvfb-run (Linux WebKitGTK is the only headless-capable CI WebView).
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
bin=${MJ_E2E_BIN:-"$repo/target/debug/mj"}

if [ ! -x "$bin" ]; then
  echo "build mj first: cargo build --features desktop-app" >&2
  exit 2
fi
if ! command -v xvfb-run >/dev/null 2>&1; then
  echo "xvfb-run is required for the desktop shell smoke test" >&2
  exit 2
fi

root=$(mktemp -d "${TMPDIR:-/tmp}/mj-desktop-e2e.XXXXXX")
app_pid=
cleanup() {
  status=$?
  if [ -n "$app_pid" ] && kill -0 "$app_pid" 2>/dev/null; then
    kill -KILL "$app_pid" 2>/dev/null || true
  fi
  if [ "$status" -eq 0 ]; then
    rm -rf "$root"
  else
    echo "desktop E2E artifacts preserved at $root" >&2
    [ -f "$root/app.log" ] && sed -e 's/^/app: /' "$root/app.log" >&2
  fi
}
trap cleanup EXIT INT TERM

workspace="$root/workspace"
mkdir -p "$workspace" "$root/home/.config/mj" "$root/home/.cache/mj" "$root/home/.codex"
# Same fixture surface as deterministic.sh: fake-bin agents on PATH, codex
# credential evidence, and a pinned deepswe snapshot so roster resolution
# never touches the network.
printf '{"OPENAI_API_KEY":"e2e-test-key"}\n' >"$root/home/.codex/auth.json"
cp "$repo/src/deepswe_snapshot.json" "$root/home/.cache/mj/deepswe-v1.1.json"
printf 'version = 4\nonboarding_version = 2\n' >"$root/home/.config/mj/config.toml"

HOME="$root/home" \
XDG_CONFIG_HOME="$root/home/.config" \
XDG_CACHE_HOME="$root/home/.cache" \
PATH="$repo/tests/e2e/fake-bin:$PATH" \
WEBKIT_DISABLE_COMPOSITING_MODE=1 \
MJ_APP_PID="$root/app.pid" \
MJ_APP_BIN="$bin" \
MJ_APP_CWD="$workspace" \
  xvfb-run --auto-servernum -- \
  sh -c 'echo "$$" >"$MJ_APP_PID"; exec "$MJ_APP_BIN" --cwd "$MJ_APP_CWD" app' \
  >"$root/app.log" 2>&1 &
runner_pid=$!

# The origin announcement proves server bind, TLS material, and the pinned
# preflight all succeeded before the window opened.
attempts=0
until grep -q 'Opening the Mjolnir desktop viewer' "$root/app.log" 2>/dev/null; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 600 ]; then
    echo "desktop shell never announced the viewer origin" >&2
    exit 1
  fi
  if ! kill -0 "$runner_pid" 2>/dev/null; then
    echo "mj app exited before opening the viewer" >&2
    wait "$runner_pid" || true
    exit 1
  fi
  sleep 0.1
done

app_pid=$(cat "$root/app.pid")

# The process staying alive is the strongest "window is open" assertion a
# headless runner offers; a WebView or cookie failure exits within this
# window instead.
sleep 5
if ! kill -0 "$app_pid" 2>/dev/null; then
  echo "mj app died after startup" >&2
  wait "$runner_pid" || true
  exit 1
fi

# One graceful SIGTERM must close the window, drain the app-owned server,
# and exit 0 (a second signal would force-exit 143 instead).
kill -TERM "$app_pid"
if ! wait "$runner_pid"; then
  echo "mj app did not exit cleanly after SIGTERM" >&2
  exit 1
fi
app_pid=
echo "desktop app lifecycle smoke passed"
