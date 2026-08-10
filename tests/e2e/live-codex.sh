#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
bin=${MJ_E2E_BIN:-"$repo/target/debug/mj"}
real_home=$HOME
root=$(mktemp -d "${TMPDIR:-/tmp}/mj-subagents-live.XXXXXX")
cleanup() {
  status=$?
  if [ "$status" -eq 0 ]; then
    rm -rf "$root"
  else
    echo "live subagent artifacts preserved at $root" >&2
  fi
}
trap cleanup EXIT INT TERM
workspace="$root/workspace"
mkdir -p "$workspace" "$root/home/.config/mj" "$root/home/Library/Application Support/mj"
git -C "$workspace" init -q
nonce=$(date +%s)-$$
target="$workspace/subagent-live-$nonce.txt"
target_name=$(basename "$target")
token="SUBAGENT_LIVE_OK_$nonce"

config="version = 4\n\n[agent]\nmodel = \"auto\"\n\n[subagents]\nmodel = \"auto\"\n"
printf '%b' "$config" >"$root/home/.config/mj/config.toml"
printf '%b' "$config" >"$root/home/Library/Application Support/mj/config.toml"

before="$root/processes-before"
after="$root/processes-after"
pgrep -f '@agentclientprotocol/codex-acp' >"$before" 2>/dev/null || true

HOME="$root/home" \
XDG_CONFIG_HOME="$root/home/.config" \
CODEX_HOME="${CODEX_HOME:-$real_home/.codex}" \
MJ_E2E_BIN="$bin" \
MJ_E2E_MODE=live \
MJ_E2E_WORKSPACE="$workspace" \
MJ_E2E_TRANSCRIPT="$root/transcript.log" \
MJ_E2E_DEBUG_LOG="$root/mj.log" \
MJ_E2E_AGENT_STDERR="$root/agent.stderr" \
MJ_E2E_LIVE_TOKEN="$token" \
MJ_E2E_LIVE_PROMPT="Use create_subagent to create $target_name containing exactly live-subagent-ok with no trailing newline. The subagent's report will arrive as a user message when it finishes; wait for it and confirm the file was created before answering. Then reply with the word SUBAGENT_LIVE_OK_ immediately followed by $nonce, as one word with no spaces." \
MJ_E2E_EXIT_ON_RUNTIME_CLOSE=1 \
  expect "$repo/tests/e2e/drive-live.exp"

node -e 'const fs=require("fs"); if(!fs.readFileSync(process.argv[1]).equals(Buffer.from("live-subagent-ok"))) process.exit(1)' "$target"
grep -a "subagent" "$root/transcript.log" >/dev/null
# No "workspace changes" status assertion: under the push model the subagent's
# edits land between main-session turns, so the turn-scoped diff status line
# legitimately never fires. The byte-exact file check above is the real proof.
grep -a "$token" "$root/transcript.log" >/dev/null

sleep 1
pgrep -f '@agentclientprotocol/codex-acp' >"$after" 2>/dev/null || true
if comm -13 "$before" "$after" | grep . >/dev/null; then
  echo "live smoke left a codex-acp process behind" >&2
  exit 1
fi

echo "live Codex subagent smoke passed: $token"
