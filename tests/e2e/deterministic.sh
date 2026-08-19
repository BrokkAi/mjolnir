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
  root=$(mktemp -d "${TMPDIR:-/tmp}/mj-subagents-e2e.XXXXXX")
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
      echo "subagent E2E artifacts preserved at $root" >&2
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
  printf 'dirty before the primary turn\n' >"$workspace/seed.txt"
  # Codex detection requires real-looking credential evidence in auth.json
  # (an OPENAI_API_KEY or OAuth tokens); an empty object is "not authenticated"
  # and the adapter never enters the roster.
  printf '{"OPENAI_API_KEY":"e2e-test-key"}\n' >"$root/home/.codex/auth.json"
  cp "$repo/src/deepswe_snapshot.json" "$root/home/.cache/mj/deepswe-v1.1.json"
  cp "$repo/src/deepswe_snapshot.json" "$root/home/Library/Caches/mj/deepswe-v1.1.json"
  # Both version markers are load-bearing: stale schema starts fresh, while
  # stale onboarding content opens the product-update card instead of a
  # session, so the pinned fixture routes would never run.
  config="version = 4\nonboarding_version = 2\n\n[agent]\nreasoning_effort = \"high\"\n\n[subagents]\nmodel = \"gpt-5-6-luna\"\nreasoning_effort = \"high\"\n"
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
  MJ_E2E_TRANSCRIPT="$root/transcript.log" \
  MJ_E2E_REVIEW_LOG="$root/review-lane.log" \
  MJ_E2E_DEBUG_LOG="$root/mj.log" \
  MJ_E2E_AGENT_STDERR="$root/agent.stderr" \
  MJ_E2E_SUBAGENT_PROMPT="Run the deterministic fixture" \
  MJ_E2E_EXIT_ON_RUNTIME_CLOSE=1 \
    expect "$repo/tests/e2e/drive-mj.exp"

  wait_reaped "$root/primary.pid" primary
  wait_reaped "$root/nested.pid" nested
  if grep -a 'mj-subagent-policy' "$root/transcript.log" >/dev/null; then
    echo "hidden subagent session directive leaked into the transcript" >&2
    exit 1
  fi
  if grep -a 'mcp.mj-subagents.create_subagent' "$root/transcript.log" >/dev/null; then
    echo "parent create_subagent transport tool leaked into the transcript" >&2
    exit 1
  fi
  if grep -a 'mcp.mj-subagents.subagent_cancel' "$root/transcript.log" >/dev/null; then
    echo "parent subagent_cancel transport tool leaked into the transcript" >&2
    exit 1
  fi
  if grep -a 'F1 Model\|F[1-9] Reasoning' "$root/transcript.log" >/dev/null; then
    echo "harness-owned model or reasoning control leaked into the primary's F-key controls" >&2
    exit 1
  fi
  if [ "$mode" != no-change ]; then
    grep -a 'tools:create_subagent,subagent_cancel' "$root/primary.log" >/dev/null
  fi
  if [ "$mode" = no-change ]; then
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    test ! -e "$root/primary-result.json"
    grep -a "PRIMARY.*NO.*CHANGE" "$root/transcript.log" >/dev/null
  elif [ "$mode" = terminal-output ]; then
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    grep -a 'ansi red' "$root/transcript.log" >/dev/null
    grep -a 'progress 100%' "$root/transcript.log" >/dev/null
    grep -a 'placed' "$root/transcript.log" >/dev/null
    grep -a 'SAFE_TERMINAL_TAIL' "$root/transcript.log" >/dev/null
    grep -a 'TERMINAL_E2E_DONE' "$root/transcript.log" >/dev/null
    if grep -a 'HOSTILE_OSC\|HOSTILE_DCS' "$root/transcript.log" >/dev/null; then
      echo "terminal control payload leaked into the transcript" >&2
      exit 1
    fi
  elif [ "$mode" = parallel ]; then
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    test -s "$root/primary-result.json"
    # Two create_subagent calls, both admitted, both reported back.
    test "$(grep -ac '^launched:' "$root/primary.log")" -eq 2
    grep -a '^launched:1$' "$root/primary.log" >/dev/null
    grep -a '^launched:2$' "$root/primary.log" >/dev/null
    grep -a 'subagent_result id="1"' "$root/primary.log" >/dev/null
    grep -a 'subagent_result id="2"' "$root/primary.log" >/dev/null
    "$node" -e 'const fs=require("fs"); const r=JSON.parse(fs.readFileSync(process.argv[1])); const launches=r.launches??[]; if(r.error || r.unauthorizedRejected!==true || launches.length!==2) process.exit(1); for(const launch of launches){ const s=launch.response?.structuredContent; if(!s || s.status!=="started" || !s.subagentId || !s.agent || !s.model) process.exit(1); const text=launch.response?.content?.map(x=>x.text||"").join("")??""; if(!text.includes("running in the background") || text.includes("workspace_diff")) process.exit(1);} ' "$root/primary-result.json"
  elif [ "$mode" = complete ] || [ "$mode" = review ] || [ "$mode" = details ]; then
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    test -s "$root/primary-result.json"
    grep -a "subagent" "$root/transcript.log" >/dev/null
    # create_subagent returns before the subagent finishes: the started
    # acknowledgement must carry no report, and the report must arrive later as
    # an injected <subagent_result> user message.
    "$node" -e 'const fs=require("fs"); const r=JSON.parse(fs.readFileSync(process.argv[1])); const completions=fs.readFileSync(process.argv[2],"utf8").match(/completion:(\d+)/g)??[]; const done=Number(completions.at(0)?.split(":")[1]); const launch=(r.launches??[r])[0]; const text=launch.response?.content?.map(x=>x.text||"").join("")??""; if(r.error || r.unauthorizedRejected!==true || launch.response?.isError) process.exit(1); if(!text.includes("running in the background") || text.includes("workspace_diff") || text.includes("SUBAGENT_E2E_OK")) process.exit(1); if(launch.response?.structuredContent?.status!=="started") process.exit(1); if(done && launch.toolReceivedAt>done) process.exit(1);' "$root/primary-result.json" "$root/nested.log"
    grep -a '^injection:1:' "$root/primary.log" >/dev/null
    grep -a 'subagent_result id="1"' "$root/primary.log" >/dev/null
    grep -a 'outcome="completed"' "$root/primary.log" >/dev/null
    grep -a '<activity_summary>' "$root/primary.log" >/dev/null
    grep -a '<workspace_diff>' "$root/primary.log" >/dev/null
    grep -a "Spot-check this report's claims" "$root/primary.log" >/dev/null
    if [ "$mode" = details ]; then
      grep -a "details hidden" "$root/transcript.log" >/dev/null
      grep -a "USER_LONG_SUFFIX" "$root/transcript.log" >/dev/null
      grep -a "DELEGATION_LONG_SUFFIX" "$root/transcript.log" >/dev/null
      grep -a "SUBAGENT_LONG_SUFFIX" "$root/transcript.log" >/dev/null
      grep -a "PRIMARY_LONG_SUFFIX" "$root/transcript.log" >/dev/null
    elif [ "$mode" = complete ]; then
      # One subagent is enough to qualify for the discrete review, so this case
      # deliberately asserts nothing about whether the review ran: the driver
      # cancels shortly after the injected report is answered.
      grep -a "PRIMARY.*RECEIVED" "$root/transcript.log" >/dev/null
      test "$(grep -ac '^injected-blocks:1$' "$root/primary.log")" -eq 1
    else
      grep -a 'PRIMARY FINAL REVIEWED' "$root/transcript.log" >/dev/null
    fi
    # Terminal output visibility on screen is racy: the delegation entry
    # collapses ("details hidden") once the report is injected, so a frame
    # showing the raw output may never be captured. The injected report's
    # activity summary is the deterministic record that both the real
    # terminal and the codex-metadata terminal ran.
    grep -a "fixture terminal command" "$root/primary.log" >/dev/null
    grep -a "fixture codex metadata command" "$root/primary.log" >/dev/null
    if [ "$mode" = complete ]; then
      # A single subagent owns the workspace, so its report carries the diff.
      grep -a "changed by the subagent" "$root/primary.log" >/dev/null
    else
      # review/details launch two concurrent subagents in one workspace, so
      # the per-run diff is suppressed with the shared-workspace note.
      grep -a "shared this workspace" "$root/primary.log" >/dev/null
    fi
    grep -a 'permission:' "$root/nested.log" >/dev/null
    if [ "$mode" = review ] || [ "$mode" = details ]; then
      # Review only fires once the pool has drained, so it must come after both
      # reports were injected. It fans out over read-only specialist lanes, a
      # synthesis pass vets them, and the surviving findings come back to the
      # primary as a corrective turn.
      test "$(grep -ac '^review-lane:' "$root/primary.log")" -ge 1
      # The supervisor is intent-aware: a separate read-only analyst session
      # distils the turn's intent from the session's own user messages first.
      grep -a '^review-intent:1$' "$root/primary.log" >/dev/null
      grep -a '^review-synthesis:1$' "$root/primary.log" >/dev/null
      grep -a 'discrete-review:' "$root/primary.log" >/dev/null
      # The reviewed diff travels in the lane packet, not in the corrective
      # prompt: the primary already holds its own turn evidence.
      grep -a 'diff --git a/subagent-change.txt b/subagent-change.txt' "$root/review-lane.log" >/dev/null
      if grep -a 'seed.txt' "$root/review-lane.log" >/dev/null; then
        echo "a preexisting dirty file leaked into the reviewed turn delta" >&2
        exit 1
      fi
    fi
    if grep -a 'seed.txt' "$root/primary.log" >/dev/null; then
      echo "a preexisting dirty file leaked into the outer-turn review delta" >&2
      exit 1
    fi
  elif [ "$mode" = failed ]; then
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    test -s "$root/primary-result.json"
    # The launch itself still succeeds; the failure travels in the report.
    "$node" -e 'const r=JSON.parse(require("fs").readFileSync(process.argv[1])); const launch=(r.launches??[r])[0]; if(r.error || r.unauthorizedRejected!==true || launch.response?.isError) process.exit(1);' "$root/primary-result.json"
    grep -a 'outcome="failed"' "$root/primary.log" >/dev/null
    grep -a 'fixture subagent failure' "$root/primary.log" >/dev/null
  elif [ "$mode" = inline-stream ]; then
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    grep -a "subagent" "$root/transcript.log" >/dev/null
    test "$(grep -ac '^prompt-started$' "$root/nested.log")" -eq 1
    # The driver quits while the subagent is still mid-turn; teardown must not
    # inject a report for it. Whether the fixture observes a session/cancel
    # before the process tree dies is a teardown race, so no cancel-received
    # assertion here -- the deliberate cancel path is the `cancel` case.
    if grep -a '^injection:' "$root/primary.log" >/dev/null; then
      echo "a torn-down subagent still injected a report" >&2
      exit 1
    fi
  else
    grep -a 'Agents · primary' "$root/transcript.log" >/dev/null
    test "$(grep -ac '^session-directive:' "$root/primary.log")" -eq 1
    grep -a "subagent" "$root/transcript.log" >/dev/null
    # subagent_cancel interrupted the hung turn while mj was alive, so the
    # session/cancel round-trip is deterministic here.
    grep -a "cancel-received" "$root/nested.log" >/dev/null
    test "$(grep -ac '^prompt-started$' "$root/nested.log")" -eq 1
    grep -a '^cancel-result:' "$root/primary.log" >/dev/null
    if grep -a '^cancel-result:ERROR:' "$root/primary.log" >/dev/null; then
      echo "subagent_cancel returned an error" >&2
      exit 1
    fi
    # A caller cancel stops the subagent; no report may be injected for it.
    # (The discrete review may still fire afterwards: the cancelled subagent's
    # partial edits are a real workspace change from a turn that delegated.)
    if grep -a '^injection:' "$root/primary.log" >/dev/null; then
      echo "a cancelled subagent still injected a report" >&2
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
  no-change) run_case no-change ;;
  terminal-output) run_case terminal-output ;;
  review) run_case review ;;
  details) run_case details ;;
  parallel) run_case parallel ;;
  both) run_case complete; run_case terminal-output; run_case cancel ;;
  subagents) run_case complete; run_case no-change; run_case terminal-output; run_case inline-stream; run_case cancel; run_case failed; run_case review; run_case details; run_case parallel ;;
  *) echo "MJ_E2E_CASE must be complete, no-change, terminal-output, inline-stream, cancel, failed, review, details, parallel, both, or subagents" >&2; exit 2 ;;
esac
echo "deterministic subagent PTY E2E passed"
