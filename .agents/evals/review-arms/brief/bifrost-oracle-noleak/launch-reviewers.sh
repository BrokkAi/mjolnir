#!/bin/bash
# Waits until the codex profile reads above 25%, then runs the ON/AF jobs (driver re-checks per job).
set -u
# Run from the checkout this script belongs to (review-arms is two levels up from it).
cd "$(dirname "$(readlink -f "$0")")/../.." || { echo "harness checkout is missing" >&2; exit 2; }
ENV="$(python3 -c "import json,shlex;[print(f'export {k}={shlex.quote(str(v))}') for k,v in json.load(open('/mnt/optane/review-arms-bifrost-20261116/arms/environment.json')).items() if k!='REVIEW_ARMS_QUOTA_FLOOR' and k.isidentifier()]")" || { echo "study environment did not load" >&2; exit 2; }
[ -n "$ENV" ] || { echo "study environment is empty" >&2; exit 2; }
eval "$ENV"
export REVIEW_ARMS_QUOTA_FLOOR=codex=25
# Each wait logs its reason, so a failing quota lookup is distinguishable from a profile below the floor.
until ./mjx quota --profile codex --json | python3 -c "
import json,sys,datetime
now=datetime.datetime.now(datetime.timezone.utc).strftime('%FT%TZ')
try:
    r=json.load(sys.stdin)['profiles'][0]['report']
except Exception as e:
    print(now,'quota lookup failed:',repr(e),file=sys.stderr); sys.exit(1)
w=[x.get('remaining_percent') for x in r.get('windows') or []]
if r.get('error') or not w or None in w:
    print(now,'quota report unusable:',r.get('error') or w,file=sys.stderr); sys.exit(1)
if min(w)<=25:
    print(now,'codex at',min(w),'percent; waiting',file=sys.stderr); sys.exit(1)
"; do sleep 300; done
date -u +"codex above floor at %FT%TZ"
git rev-parse HEAD
python3 driver.py --jobs /mnt/optane/review-arms-bifrost-20261116/oracle-noleak-arm/jobs-on-af.json --concurrency 4 --stop-on-failure --same-image /mnt/optane/review-arms-bifrost-20261116/oracle-noleak-arm/image-pin.json
status=$?; echo "driver exit $status"; exit $status
