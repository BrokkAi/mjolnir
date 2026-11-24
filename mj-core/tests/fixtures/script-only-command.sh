#!/bin/sh
# Run the adjacent .script without the lifecycle shortcuts in fake-command.sh.
set -eu
case "$0" in
*/*) fake_dir=${0%/*} ;;
*) fake_dir=. ;;
esac
exec /bin/sh "$fake_dir/${0##*/}.script" "$@"
