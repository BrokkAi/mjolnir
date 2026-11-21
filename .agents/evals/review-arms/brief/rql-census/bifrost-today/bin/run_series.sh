#!/bin/bash
# Run repo series sequentially, detached from the agent harness.
cd /tmp/rql-census
for r in "$@"; do
  n=${r#*/}
  /usr/bin/python3 bin/run_repo.py "$r" >> logs/$n.log 2>&1
  echo "$(date -u +%FT%TZ) $r exit=$?" >> logs/series.log
done
echo "$(date -u +%FT%TZ) SERIES DONE $*" >> logs/series.log
