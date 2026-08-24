#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Mjolnir Computer.app can only be built on macOS." >&2
  exit 1
fi

case "${1:-}" in
  "")
    cargo_profile="debug"
    ;;
  --release)
    cargo_profile="release"
    ;;
  *)
    echo "usage: bash scripts/build-macos-computer-app.sh [--release]" >&2
    exit 2
    ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [[ "$cargo_profile" == "release" ]]; then
  cargo build --release --bin mj --bin mj-computer-host
else
  cargo build --bin mj --bin mj-computer-host
fi

version="$(sed -nE 's/^version = "([^"]+)".*/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$version" ]]; then
  echo "could not read the workspace version from Cargo.toml" >&2
  exit 1
fi

bundle="target/${cargo_profile}/Mjolnir Computer.app"
contents="$bundle/Contents"
rm -rf "$bundle"
mkdir -p "$contents/MacOS"
sed "s/@VERSION@/${version}/g" \
  "assets/macos/Mjolnir Computer.app/Contents/Info.plist" \
  > "$contents/Info.plist"
install -m 0755 "target/${cargo_profile}/mj-computer-host" \
  "$contents/MacOS/mj-computer-host"
plutil -lint "$contents/Info.plist" >/dev/null
# This makes the app launchable for local development. An ad-hoc signature
# has a new identity after every rebuild, so macOS may require its privacy
# switches to be re-enabled after this script changes the host binary.
codesign --force --sign - --timestamp=none "$bundle"
codesign --verify --deep --strict "$bundle"

echo "built $bundle"
echo "development-only ad-hoc signature; re-enable Mjolnir Computer in macOS privacy settings after rebuilding if needed"
echo "run target/${cargo_profile}/mj, then open /mjconfig and select Computer"
