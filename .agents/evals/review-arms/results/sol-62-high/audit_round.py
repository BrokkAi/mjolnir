"""Freeze inputs and audit reviewers, blinded membership and actual Opus turns."""
import argparse
import collections
import hashlib
import json
import pathlib
import re
import sqlite3

R=pathlib.Path('/mnt/optane/review-arms-20261113')
C=R/'sol-62-high'
ROUND='verify10'


def load(path):return json.loads(path.read_text())

def write_once(path, obj):
    value=json.dumps(obj,indent=2,sort_keys=True)+'\n'
    try:
        with path.open('x') as out:out.write(value)
    except FileExistsError:
        assert path.read_text()==value,path


def snippets(path):
    text=path.read_text().strip()
    if text.startswith('No findings'):return []
    parts=[s.strip() for s in re.split(r'(?m)^\s*(?=[-*]\s*\[P\d\])',text) if s.strip()]
    bullets=[s for s in parts if re.match(r'[-*]\s*\[P\d\]',s)]
    return bullets or [text]


def hash_lines(paths):
    return [f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.relative_to(R)}' for p in sorted(set(paths))]


def check_hashes(name):
    for line in (C/name).read_text().splitlines():
        digest,relative=line.split('  ',1)
        assert hashlib.sha256((R/relative).read_bytes()).hexdigest()==digest,relative


def freeze():
    key=load(R/'private/key-verify9.json');wanted=set(key['sessions'])
    assert len(wanted)==len(key['sessions'])==1726
    sessions={};jobs={};paths=[R/'private/key-verify9.json',R/'verify9/SPEC.md']
    paths.extend((R/'verify9/out').glob('verify-out-*.json'))
    for p in R.glob('runs/*/*/done.json'):
        done=load(p)
        for reviewer in done['reviewers']:
            sid=reviewer['child']
            if sid not in wanted:continue
            assert sid not in sessions
            sd=p.parent/'reviewers'/sid[:12]
            sessions[sid]=dict(child=sid,bug=done['bug'],arm=done['arm'],model=reviewer['model'],effort=reviewer['effort'],
                               slot=reviewer['slot'],output=str((sd/'output.md').relative_to(R)),findings=len(snippets(sd/'output.md')))
            jobs[(done['bug'],done['arm'])]=dict(bug=done['bug'],arm=done['arm'])
            paths.extend([p,sd/'output.md'])
            if (sd/'usage.json').exists():paths.append(sd/'usage.json')
            else:assert reviewer['model']=='greptile'
            paths.extend(x for x in [sd/'stored.json',sd/'runtime.json',sd/'prompt-time.json',p.parent/'pre-inventory.json',p.parent/'placement.json'] if x.exists())
    assert sessions.keys()==wanted and len(jobs)==1064
    write_once(C/'baseline-sessions.json',sorted(sessions.values(),key=lambda x:x['child']))
    write_once(C/'baseline-jobs.json',sorted(jobs.values(),key=lambda x:(x['bug'],x['arm'])))
    lines=hash_lines(paths);target=C/'baseline.sha256'
    if target.exists():assert target.read_text().splitlines()==lines
    else:target.write_text('\n'.join(lines)+'\n')
    return dict(sessions=len(sessions),jobs=len(jobs),hashed_files=len(lines))


def reviewers(canaries=False):
    jobs=load(C/('canaries.json' if canaries else 'jobs.json'))
    assert len(jobs)==(2 if canaries else 70)
    original=load(R/'arm-O-jobs.json');assert len(original)==70
    if not canaries:assert {j['bug'] for j in jobs}=={j['bug'] for j in original}
    rows=[];events=collections.defaultdict(list);frozen=[C/'jobs.json',C/'canaries.json']
    for job in jobs:
        path=R/'runs'/job['bug']/'U';done=load(path/'done.json');reviewer,=done['reviewers']
        sid=reviewer['child'];sd=path/'reviewers'/sid[:12]
        assert done['profile']==job['profile'] and done['protocol']=='profile-pinned-top-level-review'
        assert (reviewer['model'],reviewer['effort'],reviewer['profile'])==('gpt-6.2-sol','high',job['profile'])
        assert reviewer['has_output'] and not reviewer['handback'] and reviewer['output_kind']=='final_response'
        assert not done['inventory_problems']
        old=load(path.parent/'O/done.json');assert (done['intro'],done['base'])==(old['intro'],old['base'])
        before=load(path/'pre-inventory.json');anchor=load(path.parent/'O/pre-inventory.json')
        assert all(before[k]==anchor[k] for k in ['head','head_tree','base_tree','diff_sha256'])
        assert not any(before[k] for k in ['remotes','alternates','status','ignored'])
        assert (path/'historical-context.txt').read_bytes()==(path.parent/'O/historical-context.txt').read_bytes()
        stored=load(sd/'stored.json');assert stored['selection']=={'profile':job['profile'],'model':'gpt-6.2-sol','effort':'high'}
        output=(sd/'output.md').read_text().strip();assert output and output==re.sub(r'(?m)^(\s*)(\[P\d\])',r'\1- \2',stored['final_message'])
        assert (sd/'raw-final.md').read_text().strip()==stored['final_message']
        usage=load(sd/'usage.json');turn,=usage['turns'];command=stored['command_id']
        assert turn['command_id']==command and turn['outcome']=={'kind':'completed','stop_reason':'EndTurn'}
        import sys
        sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[2]))
        import driver
        driver.codex_turn_model(usage,command,'gpt-6.2-sol','high',pages=load(path/'transcript.json'),rollouts=sorted((sd/'codex-rollouts').glob('rollout-*.jsonl')))
        prompt=load(sd/'prompt-time.json')['prompt_created_at_ms'];end=turn['completed_at_ms'];assert end>prompt
        runtime=load(sd/'runtime.json');assert runtime['image']==load(R/'arm-O-image.json')['image']
        assert runtime['image_digest']=='sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db'
        assert runtime['reviewer_state_at_probe']['lifecycle']=='live'
        assert isinstance(runtime['processes'],list) and any('codex' in s and 'app-server' in s for s in runtime['processes'])
        rows.append(dict(child=sid,bug=job['bug'],arm='U',model='gpt-6.2-sol',effort='high',profile=job['profile'],slot=0,
                         output=str((sd/'output.md').relative_to(R)),findings=len(snippets(sd/'output.md')),seconds=(end-prompt)/1000,
                         runtime_cli=runtime['mj_cli_version_at_observation']))
        events[job['profile']].extend([(prompt,1),(end,-1)])
        frozen.extend(path/n for n in ['done.json','placement.json','pre-inventory.json','post-inventory.json','historical-context.txt','review-prompt.txt','settings.json','completion.json'])
        frozen.extend(sd/n for n in ['output.md','raw-final.md','stored.json','runtime.json','usage.json','prompt-time.json'])
        frozen.extend((sd/'codex-rollouts').glob('rollout-*.jsonl'))
    assert len({r['child'] for r in rows})==len(rows)
    peak={}
    for profile,ev in events.items():
        active=highest=0
        for _,step in sorted(ev):active+=step;highest=max(highest,active)
        assert 0<highest<=16 and active==0
        peak[profile]=highest
    if not canaries:
        assert collections.Counter(r['profile'] for r in rows)=={'codex3':35,'codex4':35}
        target=C/'new-reviewers.sha256';lines=hash_lines(frozen)
        if target.exists():assert target.read_text().splitlines()==lines
        else:target.write_text('\n'.join(lines)+'\n')
        write_once(C/'reviewer-audit.json',dict(sessions=rows,peak=peak))
    return dict(sessions=rows,peak=peak)


def pool():
    check_hashes('baseline.sha256');check_hashes('new-reviewers.sha256')
    expected={s['child']:s for s in load(C/'baseline-sessions.json')+load(C/'reviewer-audit.json')['sessions']}
    key=load(R/'private'/f'key-{ROUND}.json')
    assert len(expected)==len(key['sessions'])==len(set(key['sessions']))==1796
    assert set(key['sessions'])==set(expected)
    assert (R/'private'/f'key-{ROUND}.json').stat().st_mode&0o077==0
    assert (R/ROUND/'SPEC.md').read_bytes()==(R/'verify9/SPEC.md').read_bytes()
    sources={sid:snippets(R/row['output']) for sid,row in expected.items()}
    assert all(len(sources[sid])==row['findings'] for sid,row in expected.items())
    commits={load(R/'runs'/bug/'O/done.json')['intro']:(bug,load(R/'runs'/bug/'O/done.json')['base']) for bug in {s['bug'] for s in expected.values()}}
    seen=set();fids=set();seen_commits=set()
    for path in (R/ROUND).glob('batch-*.json'):
        for entry in load(path):
            assert set(entry)=={'intro','base','findings'}
            assert entry['intro'] not in seen_commits;seen_commits.add(entry['intro'])
            bug,base=commits[entry['intro']];assert entry['base']==base
            for finding in entry['findings']:
                assert set(finding)=={'id','text'}
                fid=finding['id'];assert fid not in fids;fids.add(fid)
                ref=key['findings'][fid];sid,index=ref['child'],ref['index'];row=expected[sid]
                assert ref['bug']==bug and all(ref[k]==row[k] for k in ['bug','arm','model','slot'])
                assert (sid,index) not in seen;seen.add((sid,index))
                assert finding['text']==sources[sid][index]
    assert fids==key['findings'].keys() and seen=={(sid,i) for sid,findings in sources.items() for i in range(len(findings))}
    assert len(commits)==70
    return dict(sessions=len(expected),diffs=70,raw_findings=len(seen),judged_diffs=len(seen_commits),zero_finding_sessions=sum(not x for x in sources.values()))


def judges(batches=None):
    paths=sorted((R/ROUND).glob('batch-*.json')) if not batches else [R/ROUND/f'batch-{n}.json' for n in batches]
    assert paths
    results=[]
    with sqlite3.connect(f'file:{pathlib.Path.home()}/.local/share/mjolnir/mj.sqlite3?mode=ro',uri=True,timeout=30) as db:
        for path in paths:
            n=int(path.stem.split('-')[1]);profile=['claude4','claude7'][n%2]
            record=load(R/ROUND/'sessions'/f'batch-{n}.json');out=load(R/ROUND/'out'/f'verify-out-{n}.json')
            sid=record['session_id'];cid=f'review-arms-{ROUND}-batch-{n}-{sid[:12]}'
            assert record['prompted'] and record['profile']==profile
            assert {k:record['config'][k] for k in ['model','effort','fast','ultracode']}==dict(model='opus',effort='high',fast='off',ultracode='off')
            producer=out['producer'];assert (producer['session_id'],producer['profile'],producer['launch_profile'])==(sid,profile,profile)
            assert producer['config']==record['config']
            selection,=db.execute('SELECT s.command_id,s.model,s.effort,s.profile,u.body FROM session_turn_selections s LEFT JOIN session_turn_usage u USING (session_id,command_id) WHERE s.session_id=?',(sid,)).fetchall()
            command,model,effort,actual,body=selection;assert (command,model,effort,actual)==(cid,'opus','high',profile)
            turn=json.loads(body);assert turn['outcome']==dict(kind='completed',stop_reason='EndTurn')
            models=turn['usage']['provider_details']['model_usage']
            assert 'claude-opus-5-5' in models and models.keys()<={'claude-opus-5-5','claude-haiku-5-5'}
            want={entry['intro']:collections.Counter(f['id'] for f in entry['findings']) for entry in load(path)}
            assert len(out['commits'])==len(want)
            for commit in out['commits']:
                ids=want.pop(commit['intro']);groups=set()
                for group in commit['groups']:
                    assert group['group'] and group['group'] not in groups;groups.add(group['group'])
                    assert group['findings'] and group['summary'].strip() and group['evidence'].strip()
                    assert group['confidence'] in ['high','medium','low'] and group['verdict'] in ['confirmed','plausible','rejected']
                    assert group['severity'] in (['high','medium','low',None] if group['verdict']=='rejected' else ['high','medium','low'])
                assert ids==collections.Counter(fid for group in commit['groups'] for fid in group['findings'])
            assert not want
            results.append(dict(batch=n,session=sid,command=cid,profile=profile,provider='claude-opus-5-5'))
    assert len(results)==len({r['session'] for r in results})
    return results

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    sub=parser.add_subparsers(dest='mode',required=True)
    sub.add_parser('freeze');sub.add_parser('pool')
    sub.add_parser('reviewers').add_argument('--canaries',action='store_true')
    sub.add_parser('judges').add_argument('--batch',action='append',type=int)
    args=parser.parse_args()
    result=freeze() if args.mode=='freeze' else reviewers(args.canaries) if args.mode=='reviewers' else pool() if args.mode=='pool' else judges(args.batch)
    print(json.dumps(result,indent=2))
