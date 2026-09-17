#!/bin/sh
# Stand in for an account's login shell in the login_environment tests.
#
# Model account startup independently of the machine's /etc/profile: verify the
# noninteractive login flags and then source the fixture profile.
#
# This file is checked in and never written at run time. A test's temporary home
# links to it instead of writing a copy, because a sibling thread that forks
# while the write descriptor is open leaves a child holding it and the exec
# fails with ETXTBSY -- see fixture() in mj-core/src/login_environment.rs.
[ "$1" = -l ] && [ "$2" = -c ] || exit 80
. "$HOME/.profile"
exec /bin/sh -c "$3"
