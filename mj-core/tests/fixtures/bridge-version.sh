#!/bin/sh
# A version probe that remains executable while parallel tests write fixtures.
set -eu
[ "$1" = "--version" ]
printf '%s\n' "$MJ_TEST_BRIDGE_VERSION"
