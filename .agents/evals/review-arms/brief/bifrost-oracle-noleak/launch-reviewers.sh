#!/bin/bash
# Waits until the codex profile reads above 25%, then runs the ON/AF jobs (driver re-checks per job).
set -u
cd /home/jonathan/Projects/mjolnir/.mj/clones/290e45f22335c0640cdfb32c6f225440/.agents/evals/review-arms || { echo "harness checkout is missing" >&2; exit 2; }
ENV="$(python3 -c "import json;[print(f'export {k}={v}') for k,v in json.load(open('/mnt/optane/review-arms-bifrost-20261116/arms/environment.json')).items() if k!='REVIEW_ARMS_QUOTA_FLOOR']")" || { echo "study environment did not load" >&2; exit 2; }
[ -n "$ENV" ] || { echo "study environment is empty" >&2; exit 2; }
eval "$ENV"
export REVIEW_ARMS_QUOTA_FLOOR=codex=25
until ./mjx quota --profile codex --json | python3 -c "import json,sys;r=json.load(sys.stdin)['profiles'][0]['report'];w=[x.get('remaining_percent') for x in r.get('windows') or []];sys.exit(0 if w and None not in w and min(w)>25 and not r.get('error') else 1)"; do sleep 300; done
date -u +"codex above floor at %FT%TZ"
git -C /home/jonathan/Projects/mjolnir/.mj/clones/290e45f22335c0640cdfb32c6f225440/.agents/evals/review-arms rev-parse HEAD
python3 driver.py --jobs /mnt/optane/review-arms-bifrost-20261116/oracle-noleak-arm/jobs-on-af.json --concurrency 4 --stop-on-failure --same-image /mnt/optane/review-arms-bifrost-20261116/oracle-noleak-arm/image-pin.json
status=$?; echo "driver exit $status"; exit $status
