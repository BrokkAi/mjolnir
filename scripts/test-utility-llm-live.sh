#!/usr/bin/env bash
set -euo pipefail

required=(
  MJ_UTILITY_LIVE_CODEX_PROFILE
  MJ_UTILITY_LIVE_GROK_PROFILE
  MJ_UTILITY_LIVE_KIMI_PROFILE
  MJ_UTILITY_LIVE_DEEPSEEK_PROFILE
)

for variable in "${required[@]}"; do
  if [[ -z "${!variable:-}" ]]; then
    echo "error: set $variable to a configured Mjolnir profile id" >&2
    exit 2
  fi
done

cargo test -p brokk-mj-controller \
  hel_utility_llm::tests::utility_llm_live_all_profiles \
  -- --ignored --exact --nocapture
