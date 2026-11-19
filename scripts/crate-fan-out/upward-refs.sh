#!/bin/bash
# List the references from a group of top-level modules of a crate to the
# other modules of that crate, production (P) and test (T), with the group's
# size. Run it in the crate's src directory:
#   ../../scripts/crate-fan-out/upward-refs.sh "worker_client session_manager"
# It reads `crate::` paths, so `use crate::{a, b}` groups and `use super::*`
# are not counted: the compiler is the real check once files move.
A="$1"; LOW=$(grep -oE "use mj_[a-z_]+::[a-z_]+;" lib.rs | sed 's/.*:://; s/;//' | tr '\n' ' ')
for m in $A; do for f in $m.rs $m; do [ -e $f ] && grep -rHnoE "\bcrate::[a-z_0-9]+(::[A-Za-z_0-9]+)*" $f --include=*.rs; done; done | awk -F: -v A="$A $LOW" 'BEGIN{n=split(A,a," "); for(i=1;i<=n;i++) g[a[i]]=1} { split($0,p,"crate::"); split(p[2],q,"::"); if(!(q[1] in g)) { t=($0 ~ /tests?(\.rs|\/)|test_/) ? "T" : "P"; print t" "$1": crate::"p[2] } }' | sort | uniq -c | sort -k2,2 -k1,1rn
for m in $A; do find $m.rs $m -name '*.rs' 2>/dev/null; done | xargs cat | wc -l
