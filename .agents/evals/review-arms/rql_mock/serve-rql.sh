#!/bin/sh
# Invocation from the session key's authorized_keys command, with host-owned arguments.
set -eu
if [ "$#" -ne 2 ] || [ "${SSH_ORIGINAL_COMMAND-}" != "$2" ]; then
    echo "serve-rql: command does not match the key's session" >&2
    exit 64
fi
exec python3 -X utf8 -B "${0%/*}/server.py" "$1" "$2"
