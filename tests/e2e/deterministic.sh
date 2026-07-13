#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
bin=${MJ_E2E_BIN:-"$repo/target/debug/mj"}
node=$(command -v node)

if [ ! -x "$bin" ]; then
  echo "build mj first: cargo build" >&2
  exit 2
fi
if ! command -v expect >/dev/null 2>&1; then
  echo "expect is required for the PTY smoke test" >&2
  exit 2
fi

run_case() {
  mode=$1
  root=$(mktemp -d "${TMPDIR:-/tmp}/mj-code-agent-e2e.XXXXXX")
  remove_root() {
    attempts=0
    while [ -e "$root" ] && [ "$attempts" -lt 5 ]; do
      rm -rf "$root" 2>/dev/null || true
      attempts=$((attempts + 1))
      [ -e "$root" ] && sleep 0.1
    done
    test ! -e "$root"
  }
  cleanup_case() {
    status=$?
    if [ "$status" -eq 0 ]; then
      remove_root
    else
      echo "code-agent E2E artifacts preserved at $root" >&2
    fi
  }
  trap cleanup_case EXIT INT TERM
  workspace="$root/workspace"
  mkdir -p "$workspace" "$root/home/.config/mj" "$root/home/Library/Application Support/mj" \
    "$root/home/.cache/mj" "$root/home/Library/Caches/mj" "$root/home/.codex"
  git -C "$workspace" init -q
  git -C "$workspace" config user.email mjolnir@example.test
  git -C "$workspace" config user.name "Mjolnir Tests"
  printf 'seed\n' >"$workspace/seed.txt"
  git -C "$workspace" add seed.txt
  git -C "$workspace" commit -qm seed
  printf 'dirty before council turn\n' >"$workspace/seed.txt"
  printf '{}\n' >"$root/home/.codex/auth.json"
  cp "$repo/src/deepswe_snapshot.json" "$root/home/.cache/mj/deepswe-v1.1.json"
  cp "$repo/src/deepswe_snapshot.json" "$root/home/Library/Caches/mj/deepswe-v1.1.json"
  config="[eitri]\nmodel = \"gpt-5-6-luna\"\n"
  printf '%b' "$config" >"$root/home/.config/mj/config.toml"
  printf '%b' "$config" >"$root/home/Library/Application Support/mj/config.toml"

  wait_reaped() {
    pid_file=$1
    label=$2
    test -f "$pid_file" || return 0
    pid=$(cat "$pid_file")
    attempts=0
    while kill -0 "$pid" 2>/dev/null; do
      attempts=$((attempts + 1))
      if [ "$attempts" -ge 30 ]; then
        echo "$label process $pid was not reaped" >&2
        return 1
      fi
      sleep 0.1
    done
  }

  HOME="$root/home" \
  XDG_CONFIG_HOME="$root/home/.config" \
  XDG_CACHE_HOME="$root/home/.cache" \
  PATH="$repo/tests/e2e/fake-bin:$PATH" \
  MJ_E2E_BIN="$bin" \
  MJ_E2E_MODE="$mode" \
  MJ_E2E_WORKSPACE="$workspace" \
  MJ_E2E_PRIMARY_RESULT="$root/primary-result.json" \
  MJ_E2E_PRIMARY_LOG="$root/primary.log" \
  MJ_E2E_PRIMARY_PID="$root/primary.pid" \
  MJ_E2E_NESTED_LOG="$root/nested.log" \
  MJ_E2E_NESTED_PID="$root/nested.pid" \
  MJ_E2E_LOKI_LOG="$root/loki.log" \
  MJ_E2E_TRANSCRIPT="$root/transcript.log" \
  MJ_E2E_DEBUG_LOG="$root/mj.log" \
  MJ_E2E_AGENT_STDERR="$root/agent.stderr" \
  MJ_E2E_CODE_AGENT_INSTRUCTIONS="Run the deterministic fixture" \
  MJ_E2E_HTTP_UNSUPPORTED="$([ "$mode" = unsupported ] && printf 1 || printf 0)" \
  MJ_E2E_EXIT_ON_RUNTIME_CLOSE=1 \
    expect "$repo/tests/e2e/drive-mj.exp"

  wait_reaped "$root/primary.pid" primary
  wait_reaped "$root/nested.pid" nested
  if grep -a 'MJ_CODE_AGENT_POLICY_READY' "$root/transcript.log" >/dev/null; then
    echo "hidden code-agent session directive leaked into the transcript" >&2
    exit 1
  fi
  if grep -a 'mcp.mj-code-agent.code_agent' "$root/transcript.log" >/dev/null; then
    echo "parent code-agent transport tool leaked into the transcript" >&2
    exit 1
  fi
  if grep -a 'mcp.mj-code-agent.explore_agent' "$root/transcript.log" >/dev/null; then
    echo "parent explore-agent transport tool leaked into the transcript" >&2
    exit 1
  fi
  if grep -a 'F1 Model\|F[1-9] Reasoning' "$root/transcript.log" >/dev/null; then
    echo "Council-owned model or reasoning control leaked into Thor's F-key controls" >&2
    exit 1
  fi
  if [ "$mode" = unsupported ]; then
    test ! -e "$root/primary-result.json"
    grep -a "no Council model is launchable.*mcpCapabilities.http" "$root/transcript.log" >/dev/null
    if grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null; then
      echo "failed ACP initialization claimed a successful connection" >&2
      exit 1
    fi
    if [ -f "$root/primary.log" ] && grep -a '"method":"session/new"' "$root/primary.log" >/dev/null; then
      echo "unsupported primary received session/new" >&2
      exit 1
    fi
  elif [ "$mode" = no-change ]; then
    grep -ai 'waiting for Codex' "$root/transcript.log" >/dev/null
    grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null
    test ! -e "$root/primary-result.json"
    grep -a 'Thor session update' "$root/loki.log" >/dev/null
    grep -a 'no-advice' "$root/loki.log" >/dev/null
    grep -a "PRIMARY.*NO.*CHANGE" "$root/transcript.log" >/dev/null
  elif [ "$mode" = explore ]; then
    grep -ai 'waiting for Codex' "$root/transcript.log" >/dev/null
    grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    test -s "$root/primary-result.json"
    grep -a 'Thor.*Eitri.*explore' "$root/transcript.log" >/dev/null
    grep -a 'search fixture architecture' "$root/transcript.log" >/dev/null
    grep -a 'EXPLORE_E2E_OK' "$root/transcript.log" >/dev/null
    grep -a 'explore-runtime:read-only=true:mcp-servers=0' "$root/nested.log" >/dev/null
    grep -a 'Thoroughness level: very thorough' "$root/nested.log" >/dev/null
    grep -a 'config:reasoning=high' "$root/nested.log" >/dev/null
    node -e 'const fs=require("fs"); const r=JSON.parse(fs.readFileSync(process.argv[1])); const done=Number(fs.readFileSync(process.argv[2],"utf8").match(/completion:(\d+)/)?.[1]); const text=r.response.content?.map(x=>x.text||"").join(""); if(r.error || r.unauthorizedStatus!==401 || r.response.isError || text!=="EXPLORE_E2E_OK" || text.includes("workspace_diff") || text.includes("review Eitri") || !done || r.toolReceivedAt<done) process.exit(1)' "$root/primary-result.json" "$root/nested.log"
    test ! -e "$workspace/eitri-change.txt"
    test ! -e "$workspace/eitri-partial.txt"
    if grep -a 'discrete-review:' "$root/primary.log" >/dev/null; then
      echo "exploration incorrectly triggered Thor's workspace review" >&2
      exit 1
    fi
  elif [ "$mode" = explore-cancel ]; then
    grep -ai 'waiting for Codex' "$root/transcript.log" >/dev/null
    grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    test -s "$root/primary-result.json"
    grep -a 'search fixture architecture' "$root/transcript.log" >/dev/null
    grep -a 'cancel-received' "$root/nested.log" >/dev/null
    grep -a 'explore-runtime:read-only=true:mcp-servers=0' "$root/nested.log" >/dev/null
    node -e 'const r=JSON.parse(require("fs").readFileSync(process.argv[1])); const text=r.response.content?.map(x=>x.text||"").join(""); if(r.error || r.unauthorizedStatus!==401 || !r.response.isError || !text.includes("Eitri cancelled") || text.includes("workspace_diff") || text.includes("review Eitri")) process.exit(1)' "$root/primary-result.json"
    test ! -e "$workspace/eitri-change.txt"
    test ! -e "$workspace/eitri-partial.txt"
  elif [ "$mode" = complete ] || [ "$mode" = loki-eitri ] || [ "$mode" = loki-thor ] || [ "$mode" = thor-review ] || [ "$mode" = details ]; then
    grep -ai 'waiting for Codex' "$root/transcript.log" >/dev/null
    grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    test -s "$root/primary-result.json"
    grep -a "Eitri" "$root/transcript.log" >/dev/null
    if [ "$mode" = details ]; then
      grep -a "details hidden" "$root/transcript.log" >/dev/null
      grep -a "USER_LONG_SUFFIX" "$root/transcript.log" >/dev/null
      grep -a "DELEGATION_LONG_SUFFIX" "$root/transcript.log" >/dev/null
      grep -a "EITRI_LONG_SUFFIX" "$root/transcript.log" >/dev/null
      grep -a "THOR_LONG_SUFFIX" "$root/transcript.log" >/dev/null
    elif [ "$mode" = complete ] || [ "$mode" = loki-eitri ]; then
      grep -a "PRIMARY.*RECEIVED" "$root/transcript.log" >/dev/null
      if grep -a 'discrete-review:' "$root/primary.log" >/dev/null; then
        echo "single Eitri handoff incorrectly triggered Thor's discrete review" >&2
        exit 1
      fi
    else
      grep -a "FINAL" "$root/transcript.log" >/dev/null
    fi
    if [ "$mode" = thor-review ]; then
      grep -a 'no-advice' "$root/loki.log" >/dev/null
      grep -a 'PRIMARY FINAL REVIEWED' "$root/loki.log" >/dev/null
    elif [ "$mode" != details ]; then
      grep -a "CODEAGENT_E2E_OK" "$root/transcript.log" >/dev/null
    fi
    if [ "$mode" = loki-eitri ] || [ "$mode" = loki-thor ]; then
      grep -a '^advise:' "$root/loki.log" >/dev/null
      grep -a "fixture critique" "$root/transcript.log" >/dev/null
    fi
    grep -a "nested-terminal-output" "$root/transcript.log" >/dev/null
    grep -a "codex-metadata-terminal-output" "$root/transcript.log" >/dev/null
    grep -a "changed by Eitri" "$root/loki.log" >/dev/null
    grep -a 'permission:' "$root/nested.log" >/dev/null
    node -e 'const fs=require("fs"); const r=JSON.parse(fs.readFileSync(process.argv[1])); const completions=fs.readFileSync(process.argv[2],"utf8").match(/completion:(\d+)/g)??[]; const done=Number(completions.at(-1)?.split(":")[1]); const multi=["loki-thor","thor-review","details"].includes(process.argv[3]); const responses=r.responses??[r]; const text=x=>x?.content?.map(y=>y.text||"").join("")??""; const first=text(responses[0]?.response); const last=text(r.response); const review="You should review Eitri\u0027s work now."; const firstResult=process.argv[3]==="details" ? first.includes("EITRI_LONG_SUFFIX") : first.startsWith("CODEAGENT_E2E_OK"); const fullDiff=first.includes("<workspace_diff scope=\"eitri-invocation\" authored_by=\"Eitri\">") && first.includes("diff --git a/eitri-change.txt b/eitri-change.txt") && first.includes("eitri-change.txt") && first.includes("+changed by Eitri") && first.includes(review) && !first.includes("seed.txt"); const lastShape=multi ? last.includes("No workspace changes.") && !last.includes("eitri-change.txt") && !last.includes(review) : last===first; if(r.error || r.unauthorizedStatus!==401 || r.response.isError || !firstResult || !fullDiff || !lastShape || responses.length!==(multi?2:1) || !done || r.toolReceivedAt<done) process.exit(1)' "$root/primary-result.json" "$root/nested.log" "$mode"
    if [ "$mode" = loki-thor ] || [ "$mode" = thor-review ] || [ "$mode" = details ]; then
      grep -a 'discrete-review:' "$root/primary.log" >/dev/null
      grep -a 'diff --git a/eitri-change.txt b/eitri-change.txt' "$root/primary.log" >/dev/null
    fi
    if grep -a 'seed.txt' "$root/primary.log" >/dev/null; then
      echo "preexisting dirty file leaked into outer-turn review delta" >&2
      exit 1
    fi
  elif [ "$mode" = failed ]; then
    grep -ai 'waiting for Codex' "$root/transcript.log" >/dev/null
    grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null
    test -s "$root/primary-result.json"
    node -e 'const r=JSON.parse(require("fs").readFileSync(process.argv[1])); const text=r.response.content?.map(x=>x.text||"").join(""); if(r.error || r.unauthorizedStatus!==401 || !r.response.isError || !text.includes("fixture Eitri failure") || !text.includes("<workspace_diff scope=\"eitri-invocation\" authored_by=\"Eitri\">") || !text.includes("diff --git a/eitri-partial.txt b/eitri-partial.txt") || !text.includes("eitri-partial.txt") || !text.includes("You should review Eitri\u0027s work now.") || text.includes("seed.txt")) process.exit(1)' "$root/primary-result.json"
    if grep -a 'discrete-review:' "$root/primary.log" >/dev/null; then
      echo "single failed Eitri handoff incorrectly triggered Thor's discrete review" >&2
      exit 1
    fi
  else
    grep -ai 'waiting for Codex' "$root/transcript.log" >/dev/null
    grep -a 'Connected to Codex' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    test -s "$root/primary-result.json"
    grep -a "Eitri" "$root/transcript.log" >/dev/null
    grep -a "cancel-received" "$root/nested.log" >/dev/null
    node -e 'const r=JSON.parse(require("fs").readFileSync(process.argv[1])); const text=r.response.content?.map(x=>x.text||"").join(""); if(r.error || r.unauthorizedStatus!==401 || !r.response.isError || !text.includes("<workspace_diff scope=\"eitri-invocation\" authored_by=\"Eitri\">") || !text.includes("diff --git a/eitri-partial.txt b/eitri-partial.txt") || !text.includes("eitri-partial.txt") || !text.includes("You should review Eitri\u0027s work now.") || text.includes("seed.txt")) process.exit(1)' "$root/primary-result.json"
    if grep -a 'discrete-review:' "$root/primary.log" >/dev/null; then
      echo "single cancelled Eitri handoff incorrectly triggered Thor's discrete review" >&2
      exit 1
    fi
  fi
  remove_root
  trap - EXIT INT TERM
}

case ${MJ_E2E_CASE:-both} in
  complete) run_case complete ;;
  inline-stream) run_case inline-stream ;;
  cancel) run_case cancel ;;
  failed) run_case failed ;;
  unsupported) run_case unsupported ;;
  no-change) run_case no-change ;;
  loki-eitri) run_case loki-eitri ;;
  loki-thor) run_case loki-thor ;;
  thor-review) run_case thor-review ;;
  details) run_case details ;;
  explore) run_case explore ;;
  explore-cancel) run_case explore-cancel ;;
  both) run_case complete; run_case cancel; run_case unsupported ;;
  council) run_case complete; run_case no-change; run_case inline-stream; run_case cancel; run_case failed; run_case unsupported; run_case loki-eitri; run_case loki-thor; run_case thor-review; run_case details; run_case explore; run_case explore-cancel ;;
  *) echo "MJ_E2E_CASE must be complete, no-change, inline-stream, cancel, failed, unsupported, loki-eitri, loki-thor, thor-review, details, explore, explore-cancel, both, or council" >&2; exit 2 ;;
esac
echo "deterministic code-agent PTY E2E passed"
