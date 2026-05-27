#!/usr/bin/env bash
set -euo pipefail

OWNER="${MJOLNIR_GITHUB_OWNER:-BrokkAi}"
INSTALL_DIR="${MJOLNIR_INSTALL_DIR:-${INSTALL_DIR:-$HOME/.local/bin}}"

TMP_DIR=""
OS_FAMILY=""
ARCH=""
RUST_TARGET=""
ANVIL_OS=""

log() {
  printf 'mjolnir-installer: %s\n' "$*"
}

warn() {
  printf 'mjolnir-installer: warning: %s\n' "$*" >&2
}

die() {
  printf 'mjolnir-installer: error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
Install the latest mjolnir, anvil, and bifrost binaries.

Usage:
  curl -fsSL https://raw.githubusercontent.com/BrokkAi/mjolnir/master/install.sh | bash

Environment:
  INSTALL_DIR              Install directory. Defaults to ~/.local/bin.
  MJOLNIR_INSTALL_DIR      Same as INSTALL_DIR, with higher precedence.
  MJOLNIR_GITHUB_OWNER     GitHub owner to download from. Defaults to BrokkAi.
  MJOLNIR_VERSION          Optional mjolnir tag to install, for example v0.3.4.
  ANVIL_VERSION            Optional anvil tag to install.
  BIFROST_VERSION          Optional bifrost tag to install.
  GITHUB_TOKEN             Optional token for GitHub API rate limits.
  PROFILE                  Optional shell profile to update when INSTALL_DIR is not on PATH.
EOF
}

cleanup() {
  if [[ -n "${TMP_DIR}" && -d "${TMP_DIR}" ]]; then
    rm -rf "${TMP_DIR}"
  fi
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

curl_args() {
  printf '%s\0' -fsSL --retry 3 --retry-delay 1
  if [[ -n "${GITHUB_TOKEN:-}" ]]; then
    printf '%s\0' -H "Authorization: Bearer ${GITHUB_TOKEN}"
  fi
}

download_file() {
  local url="$1"
  local dest="$2"
  local -a args=()

  while IFS= read -r -d '' arg; do
    args+=("$arg")
  done < <(curl_args)

  curl "${args[@]}" -o "$dest" "$url"
}

detect_platform() {
  local uname_s
  local uname_m

  uname_s="$(uname -s)"
  uname_m="$(uname -m)"

  case "$uname_m" in
    x86_64 | amd64)
      ARCH="x86_64"
      ;;
    arm64 | aarch64)
      ARCH="aarch64"
      ;;
    *)
      die "unsupported CPU architecture: ${uname_m}"
      ;;
  esac

  case "$uname_s" in
    Darwin)
      OS_FAMILY="macos"
      ANVIL_OS="macos"
      RUST_TARGET="${ARCH}-apple-darwin"
      ;;
    Linux)
      OS_FAMILY="linux"
      ANVIL_OS="linux"
      RUST_TARGET="${ARCH}-unknown-linux-gnu"
      ;;
    *)
      die "unsupported OS: ${uname_s}"
      ;;
  esac
}

release_endpoint() {
  local repo="$1"
  local version="$2"

  if [[ -n "$version" ]]; then
    printf 'https://api.github.com/repos/%s/%s/releases/tags/%s\n' "$OWNER" "$repo" "$version"
  else
    printf 'https://api.github.com/repos/%s/%s/releases/latest\n' "$OWNER" "$repo"
  fi
}

fetch_release() {
  local repo="$1"
  local version="$2"
  local dest="$3"
  local endpoint

  endpoint="$(release_endpoint "$repo" "$version")"
  download_file "$endpoint" "$dest"
}

release_tag() {
  local release_file="$1"

  { grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' "$release_file" || true; } |
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
    head -n 1
}

release_asset_urls() {
  local release_file="$1"

  { grep -o '"browser_download_url"[[:space:]]*:[[:space:]]*"[^"]*"' "$release_file" || true; } |
    sed -n 's/.*"browser_download_url"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

available_assets() {
  local release_file="$1"

  release_asset_urls "$release_file" |
    sed 's#.*/##' |
    sed '/[.]sha256$/d' |
    tr '\n' ' '
}

select_asset() {
  local release_file="$1"
  local label="$2"
  local tag="$3"
  shift 3
  local url
  local name
  local pattern

  while IFS= read -r url; do
    name="${url##*/}"
    for pattern in "$@"; do
      if [[ "$name" =~ $pattern ]]; then
        printf '%s\n' "$url"
        return 0
      fi
    done
  done < <(release_asset_urls "$release_file")

  die "no ${label} asset found for ${OS_FAMILY}/${ARCH} in ${OWNER} release ${tag}. Available assets: $(available_assets "$release_file")"
}

checksum_url_for() {
  local release_file="$1"
  local asset_name="$2"
  local checksum_name="${asset_name}.sha256"
  local url

  while IFS= read -r url; do
    if [[ "${url##*/}" == "$checksum_name" ]]; then
      printf '%s\n' "$url"
      return 0
    fi
  done < <(release_asset_urls "$release_file")
}

hash_file() {
  local file="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    shasum -a 256 "$file" | awk '{print $1}'
  fi
}

verify_checksum_if_present() {
  local release_file="$1"
  local asset_name="$2"
  local asset_file="$3"
  local checksum_url
  local checksum_file
  local expected
  local actual

  checksum_url="$(checksum_url_for "$release_file" "$asset_name" || true)"
  if [[ -z "$checksum_url" ]]; then
    warn "no checksum published for ${asset_name}; skipping checksum verification"
    return 0
  fi

  checksum_file="${TMP_DIR}/${asset_name}.sha256"
  download_file "$checksum_url" "$checksum_file"
  expected="$(awk '{print $1}' "$checksum_file" | head -n 1)"
  actual="$(hash_file "$asset_file")"

  if [[ "$expected" != "$actual" ]]; then
    die "checksum mismatch for ${asset_name}: expected ${expected}, got ${actual}"
  fi
}

strip_quarantine() {
  local path="$1"

  if [[ "$OS_FAMILY" == "macos" ]] && command -v xattr >/dev/null 2>&1; then
    xattr -dr com.apple.quarantine "$path" >/dev/null 2>&1 || true
  fi
}

ensure_install_dir() {
  if [[ -d "$INSTALL_DIR" ]]; then
    return 0
  fi

  if mkdir -p "$INSTALL_DIR" 2>/dev/null; then
    return 0
  fi

  command -v sudo >/dev/null 2>&1 || die "cannot create ${INSTALL_DIR}; set INSTALL_DIR to a writable directory"
  sudo mkdir -p "$INSTALL_DIR"
}

install_dir_on_path() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) return 0 ;;
    *) return 1 ;;
  esac
}

can_prompt_on_tty() {
  [[ -r /dev/tty && -w /dev/tty ]] && { : >/dev/tty; } 2>/dev/null
}

shell_quote() {
  printf "'"
  printf '%s' "$1" | sed "s/'/'\\\\''/g"
  printf "'"
}

path_export_line() {
  printf 'export PATH=%s:"$PATH"\n' "$(shell_quote "$INSTALL_DIR")"
}

default_shell_profile() {
  local shell_name

  if [[ -n "${PROFILE:-}" ]]; then
    printf '%s\n' "$PROFILE"
    return 0
  fi

  shell_name="${SHELL:-}"
  shell_name="${shell_name##*/}"
  if [[ -z "$shell_name" ]]; then
    return 1
  fi

  case "$shell_name" in
    zsh)
      printf '%s/.zshrc\n' "$HOME"
      ;;
    bash)
      if [[ "$OS_FAMILY" == "macos" ]]; then
        printf '%s/.bash_profile\n' "$HOME"
      else
        printf '%s/.bashrc\n' "$HOME"
      fi
      ;;
    ksh)
      printf '%s/.kshrc\n' "$HOME"
      ;;
    sh)
      printf '%s/.profile\n' "$HOME"
      ;;
    *)
      return 1
      ;;
  esac
}

append_install_dir_to_profile() {
  local profile="$1"
  local line="$2"
  local profile_dir

  profile_dir="$(dirname "$profile")"
  if ! mkdir -p "$profile_dir" 2>/dev/null; then
    warn "could not create ${profile_dir}; add this manually: ${line}"
    return 1
  fi

  {
    printf '\n# Added by mjolnir installer\n'
    printf '%s\n' "$line"
  } >>"$profile" || {
    warn "could not update ${profile}; add this manually: ${line}"
    return 1
  }

  log "added ${INSTALL_DIR} to PATH in ${profile}"
  log "restart your shell or run: ${line}"
}

ensure_install_dir_on_path() {
  local profile
  local line
  local answer

  if install_dir_on_path; then
    return 0
  fi

  line="$(path_export_line)"
  profile="$(default_shell_profile || true)"

  if [[ -z "$profile" ]]; then
    warn "${INSTALL_DIR} is not on PATH"
    log "add this to your shell profile: ${line}"
    return 0
  fi

  if [[ -f "$profile" ]] && grep -Fq "$INSTALL_DIR" "$profile"; then
    warn "${INSTALL_DIR} is not on the current PATH, but it already appears in ${profile}"
    log "restart your shell or run: ${line}"
    return 0
  fi

  if ! can_prompt_on_tty; then
    warn "${INSTALL_DIR} is not on PATH"
    log "add this to ${profile}: ${line}"
    return 0
  fi

  printf 'mjolnir-installer: %s is not on PATH. Add it to %s? [Y/n] ' "$INSTALL_DIR" "$profile" >/dev/tty
  read -r answer </dev/tty || answer=""

  case "$answer" in
    "" | y | Y | yes | YES)
      append_install_dir_to_profile "$profile" "$line" || true
      ;;
    *)
      warn "${INSTALL_DIR} is not on PATH"
      log "add this to ${profile}: ${line}"
      ;;
  esac
}

install_binary() {
  local src="$1"
  local name="$2"
  local dest="${INSTALL_DIR}/${name}"

  chmod 0755 "$src"
  strip_quarantine "$src"

  if [[ -w "$INSTALL_DIR" ]]; then
    install -m 0755 "$src" "$dest"
    strip_quarantine "$dest"
  else
    command -v sudo >/dev/null 2>&1 || die "cannot write ${INSTALL_DIR}; set INSTALL_DIR to a writable directory"
    sudo install -m 0755 "$src" "$dest"
    if [[ "$OS_FAMILY" == "macos" ]] && command -v xattr >/dev/null 2>&1; then
      sudo xattr -dr com.apple.quarantine "$dest" >/dev/null 2>&1 || true
    fi
  fi

  log "installed ${name} to ${dest}"
}

find_extracted_binary() {
  local dir="$1"
  local name="$2"
  local found

  found="$(find "$dir" -type f -name "$name" -print -quit)"
  if [[ -z "$found" ]]; then
    die "archive did not contain expected binary: ${name}"
  fi
  printf '%s\n' "$found"
}

install_from_asset() {
  local label="$1"
  local repo="$2"
  local bin_name="$3"
  local version="$4"
  shift 4
  local release_file="${TMP_DIR}/${repo}-release.json"
  local tag
  local asset_url
  local asset_name
  local asset_file
  local extract_dir
  local src

  fetch_release "$repo" "$version" "$release_file"
  tag="$(release_tag "$release_file")"
  [[ -n "$tag" ]] || die "could not read latest ${label} release metadata"

  asset_url="$(select_asset "$release_file" "$label" "$tag" "$@")"
  asset_name="${asset_url##*/}"
  asset_file="${TMP_DIR}/${asset_name}"

  log "downloading ${label} ${tag} (${asset_name})"
  download_file "$asset_url" "$asset_file"
  verify_checksum_if_present "$release_file" "$asset_name" "$asset_file"

  extract_dir="${TMP_DIR}/${repo}-extract"

  case "$asset_name" in
    *.tar.gz | *.tgz)
      mkdir -p "$extract_dir"
      tar -xzf "$asset_file" -C "$extract_dir"
      strip_quarantine "$extract_dir"
      src="$(find_extracted_binary "$extract_dir" "$bin_name")"
      install_binary "$src" "$bin_name"
      ;;
    *.zip)
      require_command unzip
      mkdir -p "$extract_dir"
      unzip -q "$asset_file" -d "$extract_dir"
      strip_quarantine "$extract_dir"
      src="$(find_extracted_binary "$extract_dir" "$bin_name")"
      install_binary "$src" "$bin_name"
      ;;
    *)
      install_binary "$asset_file" "$bin_name"
      ;;
  esac
}

install_anvil() {
  local -a patterns=()

  if [[ "$OS_FAMILY" == "macos" ]]; then
    patterns+=("^brokk-anvil-.*-universal-apple-darwin[.]zip$")
  fi
  patterns+=("^brokk-anvil-.*-${RUST_TARGET}[.]zip$")
  patterns+=("^anvil-${ANVIL_OS}-${ARCH}$")

  install_from_asset "anvil" "anvil" "anvil" "${ANVIL_VERSION:-}" "${patterns[@]}"
}

install_bifrost() {
  local -a patterns=()

  if [[ "$OS_FAMILY" == "macos" ]]; then
    patterns+=("^bifrost-.*-universal-apple-darwin[.]tar[.]gz$")
  fi
  patterns+=("^bifrost-.*-${RUST_TARGET}[.]tar[.]gz$")

  install_from_asset "bifrost" "bifrost" "bifrost" "${BIFROST_VERSION:-}" "${patterns[@]}"
}

install_mjolnir() {
  local -a patterns=()

  if [[ "$OS_FAMILY" == "macos" ]]; then
    patterns+=("^brokk-mjolnir-.*-universal-apple-darwin[.]tar[.]gz$")
  fi
  patterns+=("^brokk-mjolnir-.*-${RUST_TARGET}[.]tar[.]gz$")

  install_from_asset "mjolnir" "mjolnir" "mj" "${MJOLNIR_VERSION:-}" "${patterns[@]}"
}

main() {
  if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
  fi

  require_command curl
  require_command awk
  require_command grep
  require_command sed
  require_command tar
  require_command install
  require_command find

  detect_platform
  ensure_install_dir

  TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/mjolnir-installer.XXXXXX")"
  trap cleanup EXIT

  log "installing for ${OS_FAMILY}/${ARCH} into ${INSTALL_DIR}"
  install_anvil
  install_bifrost
  install_mjolnir

  ensure_install_dir_on_path

  log "done"
}

main "$@"
