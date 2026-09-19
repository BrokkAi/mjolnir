#!/bin/sh
# Run a test's stand-in for a command the code under test looks up on PATH.
#
# A test installs a fake command by symlinking this file under the command's
# name and writing the behaviour beside it as "<name>.script". This file is
# checked in and is never written at run time, so no test execs a file it has
# just written -- see install_fake_command in
# mj-core/src/test_hooks.rs for why that matters.
#
# $0 is the symlink the caller exec'd, so its directory and name select the
# script to run. A caller usually replaces PATH with the directory holding the
# fake, so this uses only shell builtins and an absolute interpreter: dirname
# and basename would not be found.
set -eu
case "$0" in
*/*) fake_dir=${0%/*} ;;
*) fake_dir=. ;;
esac
exec /bin/sh "$fake_dir/${0##*/}.script" "$@"
