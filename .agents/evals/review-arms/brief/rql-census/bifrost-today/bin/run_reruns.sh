#!/bin/bash
cd /tmp/rql-census
for r in j178/prek nushell/nushell 1jehuang/jcode; do
  n=${r#*/}
  /usr/bin/python3 bin/run_repo.py "$r" >> logs/rerun-$n.log 2>&1
  echo "$(date -u +%FT%TZ) rerun $r exit=$?" >> logs/series.log
done
echo "$(date -u +%FT%TZ) RERUNS DONE" >> logs/series.log
