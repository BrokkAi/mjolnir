"""Reconstruct exact solo severity yields and new high-impact coverage from verify10."""
import collections
import csv
from fractions import Fraction
import json
import pathlib
from statistics import median, fmean, quantiles

R=pathlib.Path('/mnt/optane/review-arms-20261113');C=R/'sol-62-high';V=R/'verify10'

def csv_rows(path):return list(csv.DictReader(path.open()))

sessions={r['session']:r for r in csv_rows(V/'sessions.csv')}
assert len(sessions)==1796
all_groups={(r['bug'],r['group']):r for r in csv_rows(V/'groups.csv')}
groups=collections.defaultdict(set)
for row in csv_rows(V/'session_groups.csv'):
    key=(row['bug'],row['group']);assert key in all_groups
    if all_groups[key]['verdict']=='confirmed':groups[row['session']].add(key)
by_config=collections.defaultdict(lambda:collections.defaultdict(list))
for sid,row in sessions.items():by_config[(row['model'],row['effort'])][row['bug']].append(sid)
severity={'all':{'low','medium','high'},'low':{'low'},'medium':{'medium'},'high':{'high'},'medium_high':{'medium','high'}}

def total(config,tier,bugs=None):
    wanted=severity[tier];result=Fraction(0)
    for bug,ids in by_config[config].items():
        if bugs is None or bug in bugs:
            result+=Fraction(sum(sum(all_groups[g]['severity'] in wanted for g in groups[sid]) for sid in ids),len(ids))
    return result

summary=[]
for config,bugs in sorted(by_config.items()):
    values={tier:total(config,tier) for tier in severity}
    summary.append(dict(model=config[0],effort=config[1],diffs=len(bugs),sessions=sum(map(len,bugs.values())),
                        **{tier:float(n) for tier,n in values.items()},exact={tier:str(n) for tier,n in values.items()}))
    metered=[sessions[sid] for ids in bugs.values() for sid in ids]
    prices=[float(row['cost_usd']) for row in metered if row['cost_usd']]
    clocks=[float(row['latency_s']) for row in metered if row['latency_s']]
    summary[-1]['mean_cost_usd']=fmean(prices) if len(prices)==len(metered) else None
    summary[-1]['observed_median_latency_s']=median(clocks) if len(clocks)==len(metered) else None
    summary[-1]['observed_p95_latency_s']=quantiles(clocks,n=20,method='inclusive')[18] if len(clocks)>1 and len(clocks)==len(metered) else None
    assert values['all']==values['low']+values['medium']+values['high']
for effort,count in [('low',210),('medium',210),('high',70)]:
    row=next(r for r in summary if (r['model'],r['effort'])==('gpt-6.2-sol',effort))
    assert row['diffs']==70 and row['sessions']==count
with (C/'sol62-per-diff.csv').open('w',newline='') as output:
    writer=csv.DictWriter(output,fieldnames=['bug','effort','samples',*severity])
    writer.writeheader()
    for effort in ['low','medium','high']:
        config=('gpt-6.2-sol',effort)
        for bug,ids in sorted(by_config[config].items()):
            writer.writerow(dict(bug=bug,effort=effort,samples=len(ids),
                                 **{tier:float(total(config,tier,{bug})) for tier in severity}))
high_ids={sid for sid,row in sessions.items() if (row['model'],row['effort'])==('gpt-6.2-sol','high')}
low_ids={sid for sid,row in sessions.items() if (row['model'],row['effort'])==('gpt-6.2-sol','low')}
medium_ids={sid for sid,row in sessions.items() if (row['model'],row['effort'])==('gpt-6.2-sol','medium')}

def union(ids,tier):return {g for sid in ids for g in groups[sid] if all_groups[g]['severity'] in severity[tier]}
novelty={tier:dict(high_union=len(union(high_ids,tier)),
                    not_in_any_of_three_medium=len(union(high_ids,tier)-union(medium_ids,tier)),
                    not_in_any_low_or_medium=len(union(high_ids,tier)-union(low_ids|medium_ids,tier)),
                    not_in_entire_previous_pool=len(union(high_ids,tier)-union(sessions.keys()-high_ids,tier)))
         for tier in ['all','medium_high','high']}
result={'round':'verify10','sessions':len(sessions),'solo':summary,'high_novelty':novelty}
(C/'solo-summary.json').write_text(json.dumps(result,indent=2)+'\n')
with (C/'solo-severity-totals.csv').open('w',newline='') as output:
    writer=csv.DictWriter(output,fieldnames=['model','effort','diffs','sessions',*severity,'mean_cost_usd','observed_median_latency_s','observed_p95_latency_s'])
    writer.writeheader();writer.writerows({k:v for k,v in row.items() if k!='exact'} for row in summary)
# Retain the original matched controls but update DSF yield to the fresh Opus groups.
f=R/'sol-62-repeats/figure';inputs=json.loads((f/'single-model-cost-inputs.json').read_text())
labels={'gpt-6.1-terra':'Terra 6.1','gpt-6.1-sol':'Sol 6.1','gpt-6.2-sol':'Sol 6.2','gemini-4-hydrogen':'Gemini 4 Hydrogen','kimi-code/k3.1-pro':'Kimi K3.1 Pro','muse-ember-1.0':'Ember 1.0','deepseek-flash':'DSF'}
lookup={}
for config in by_config:
    lookup[config[0]+'/'+config[1]]=config
    lookup[labels.get(config[0],config[0])+'/'+config[1]]=config
partial=('deepseek-flash','low');subset=set(by_config[partial]);assert len(subset)==16
calibration=[]
for tier in ['all','medium_high']:
    observed=total(partial,tier);estimates=[]
    for name in inputs['dsf']['reference_controls']:
        control=lookup[name];denominator=total(control,tier,subset);assert denominator>0
        value=float(observed*total(control,tier)/denominator)
        estimates.append(value);calibration.append(dict(severity=tier,control=name,dsf_70diff_est_via_control=value))
    inputs['dsf']['estimate'][tier]['matched_control_calibrated_70']=median(estimates)
    inputs['dsf']['estimate'][tier]['observed_16']=float(observed)
inputs['dsf']['calibration']=calibration
inputs['current_round']='verify10'
(C/'chart-inputs-verify10.json').write_text(json.dumps(inputs,indent=2)+'\n')
# Compare the same raw source findings across rounds; group IDs themselves can change.
def labels_for_round(name):
    key=json.loads((R/'private'/f'key-{name}.json').read_text())['findings']
    labels={}
    for path in (R/name/'out').glob('verify-out-*.json'):
        for commit in json.loads(path.read_text())['commits']:
            for group in commit['groups']:
                for fid in group['findings']:
                    raw=key[fid];identity=(raw['child'],raw['index'])
                    assert identity not in labels
                    labels[identity]=(group['verdict'],group.get('severity'))
    assert len(labels)==len(key)
    return labels
old,new=labels_for_round('verify9'),labels_for_round('verify10')
assert set(old)<=set(new) and len(old)==3304 and len(new)==3550
retained=[identity for identity in old if old[identity][0] in {'confirmed','plausible'} and new[identity][0] in {'confirmed','plausible'}]
consistency={'common_raw_findings':len(old),'same_verdict':sum(old[i][0]==new[i][0] for i in old),
             'both_retained':len(retained),'same_retained_severity':sum(old[i][1]==new[i][1] for i in retained),
             'same_verdict_and_severity':sum(old[i]==new[i] for i in old)}
(C/'judge-consistency.json').write_text(json.dumps(consistency,indent=2)+'\n')
print(json.dumps({'sol62':[r for r in summary if r['model']=='gpt-6.2-sol'],'novelty':novelty,'dsf_projected':inputs['dsf']['estimate']['all']['matched_control_calibrated_70']},indent=2))
