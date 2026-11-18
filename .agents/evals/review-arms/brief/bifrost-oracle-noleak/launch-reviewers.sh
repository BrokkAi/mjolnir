#!/bin/bash
# Waits until the codex profile reads above 25%, then runs the ON/AF jobs (driver re-checks per job).
set -u
# Run from the checkout this script belongs to (review-arms is two levels up from it).
cd "$(dirname "$(readlink -f "$0")")/../.." || { echo "harness checkout is missing" >&2; exit 2; }
ENV="$(python3 -c "import json,shlex;[print(f'export {k}={shlex.quote(str(v))}') for k,v in json.load(open('/mnt/optane/review-arms-bifrost-20261116/arms/environment.json')).items() if k!='REVIEW_ARMS_QUOTA_FLOOR' and k.isidentifier()]")" || { echo "study environment did not load" >&2; exit 2; }
[ -n "$ENV" ] || { echo "study environment is empty" >&2; exit 2; }
eval "$ENV"
export REVIEW_ARMS_QUOTA_FLOOR=codex=25
until ./mjx quota --profile codex --json | python3 -c "import json,sys;r=json.load(sys.stdin)['profiles'][0]['report'];w=[x.get('remaining_percent') for x in r.get('windows') or []];sys.exit(0 if w and None not in w and min(w)>25 and not r.get('error') else 1)"; do sleep 300; done
date -u +"codex above floor at %FT%TZ"
git rev-parse HEAD
python3 driver.py --jobs /mnt/optane/review-arms-bifrost-20261116/oracle-noleak-arm/jobs-on-af.json --concurrency 4 --stop-on-failure --same-image /mnt/optane/review-arms-bifrost-20261116/oracle-noleak-arm/image-pin.json
status=$?; echo "driver exit $status"; exit $status
