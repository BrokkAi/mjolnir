#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == --inside ]]; then
  # Check the unmodified distribution before installing interactive test tools.
  getconf GNU_LIBC_VERSION
  /artifact/mj --version
  case "$2" in
    debian) apt-get update -qq; apt-get install -y -qq tmux git ;;
    *) dnf install -y -q tmux git ;;
  esac
  export HOME=/tmp/mj-home MJ_CONFIG_DIR=/tmp/mj-config MJ_DATA_DIR=/tmp/mj-data TERM=xterm-256color
  mkdir -p "$HOME" /tmp/project
  cd /tmp/project
  git init -q
  tmux -L mj-compat -f /dev/null new-session -d -s smoke -x 140 -y 40 '/artifact/mj; echo $? > /tmp/mj-exit'
  trap 'tmux -L mj-compat kill-server 2>/dev/null || true' EXIT
  ready=0
  for ((i=0; i<120; i++)); do
    screen="$(tmux -L mj-compat capture-pane -p -t smoke 2>/dev/null || true)"
    if [[ "$screen" == *Sessions* || "$screen" == *Workspaces* ]]; then ready=1; break; fi
    if [[ -f /tmp/mj-exit ]]; then break; fi
    sleep 0.5
  done
  printf '%s\n' "$screen"
  [[ "$ready" == 1 ]] || { echo 'CLI did not render its dashboard' >&2; exit 1; }
  tmux -L mj-compat send-keys -t smoke M-q
  for ((i=0; i<40; i++)); do
    if [[ -f /tmp/mj-exit ]]; then
      [[ "$(cat /tmp/mj-exit)" == 0 ]] || { echo 'CLI exited unsuccessfully' >&2; exit 1; }
      echo 'CLI rendered in tmux and exited cleanly'
      exit 0
    fi
    sleep 0.5
  done
  echo 'CLI did not exit after Alt-Q' >&2
  exit 1
fi

target="${1:?usage: test-linux-cli-runtime.sh GNU_TARGET_TRIPLE}"
runtime="${CONTAINER_RUNTIME:-docker}"
binary_dir="$PWD/target/release-cli/$target/release"
[[ -f "$binary_dir/mj" ]] || { echo "Missing $binary_dir/mj" >&2; exit 1; }
for distribution in rocky debian amazon; do
  case "$distribution" in
    rocky) image=docker.io/library/rockylinux:8 ;;
    debian) image=docker.io/library/debian:12 ;;
    amazon) image=public.ecr.aws/amazonlinux/amazonlinux:2023 ;;
  esac
  "$runtime" run --rm \
    -v "$binary_dir:/artifact:ro" \
    -v "$PWD/scripts/test-linux-cli-runtime.sh:/test.sh:ro" \
    "$image" /bin/bash /test.sh --inside "$distribution"
done
