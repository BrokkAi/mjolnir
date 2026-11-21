#!/bin/bash
# Wait for the first series to finish, then run the rest (never two at once).
until grep -q 'SERIES DONE' /tmp/rql-census/logs/series.log 2>/dev/null; do sleep 30; done
exec /tmp/rql-census/bin/run_series.sh Helvesec/rmux carthage-software/mago nushell/nushell 1jehuang/jcode sinelaw/fresh rust-lang/rust-analyzer rustfs/rustfs web-infra-dev/rspack kreuzberg-dev/kreuzberg
