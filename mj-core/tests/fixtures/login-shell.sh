#!/bin/sh
# Stand in for an account's shell in the login_environment tests.
#
# Model account startup independently of the machine's /etc/profile: verify the
# probe starts an interactive login shell, then source the fixture login and
# interactive startup files. Exiting 80 makes a dropped flag fail every test
# that uses this fixture instead of quietly changing what they exercise.
#
# This file is checked in and never written at run time. A test's temporary home
# links to it instead of writing a copy, because a sibling thread that forks
# while the write descriptor is open leaves a child holding it and the exec
# fails with ETXTBSY -- see fixture() in mj-core/src/login_environment.rs.
[ "$1" = -l ] && [ "$2" = -i ] && [ "$3" = -c ] || exit 80
. "$HOME/.profile"
[ -f "$HOME/.shellrc" ] && . "$HOME/.shellrc"
exec /bin/sh -c "$4"
