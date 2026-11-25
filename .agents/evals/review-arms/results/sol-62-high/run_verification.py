"""Wait for the recorded review driver, then run audited Opus-only verification and analysis."""
import concurrent.futures
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

import audit_round as audit

S=pathlib.Path(__file__).resolve().parent
STUDY=S.parents[1]
R,C=audit.R,audit.C
ENV=dict(os.environ,REVIEW_ARMS_DIR=str(R),REVIEW_ARMS_INSTANCE='default',
         REVIEW_ARMS_CORPUS=str(R/'corpus/corpus.jsonl'),REVIEW_ARMS_DIFFS=str(R/'corpus/diffs.jsonl'))
for key in ['REVIEW_ARMS_MJ','REVIEW_ARMS_MJ_WORKER','REVIEW_ARMS_SCRIPTS','REVIEW_ARMS_REPO','REVIEW_ARMS_BUNDLE','REVIEW_ARMS_REPO_DIR']:
    ENV.pop(key,None)


def receipt(name, result):
    (C/name).write_text(json.dumps(result,indent=2)+'\n')
    return result


def run(command,log):
    print(json.dumps({'stage':log,'command':command}),flush=True)
    with (C/log).open('ab') as output:
        result=subprocess.run(command,cwd=STUDY,env=ENV,stdout=output,stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(f'{log}: command failed with status {result.returncode}; inspect retained log, do not replace accepted sessions')


def wait_reviews():
    launch=audit.load(C/'reviews-launch.json');pid=launch['pid'];next_report=0
    while True:
        proc=pathlib.Path('/proc')/str(pid)
        try:
            stat=(proc/'stat').read_text().split(') ',1)[1].split()[0]
            command=(proc/'cmdline').read_bytes().split(b'\0')
            active=stat!='Z' and str(S/'run_reviews.py').encode() in command
        except FileNotFoundError:active=False
        if time.monotonic()>=next_report or not active:
            jobs=audit.load(C/'jobs.json');done=[j for j in jobs if (R/'runs'/j['bug']/'U/done.json').exists()]
            print(json.dumps({'reviews_done':len(done),'expected':70,'driver_active':active}),flush=True)
            next_report=time.monotonic()+60
        if not active:break
        time.sleep(10)
    reviewed=audit.reviewers()
    print(json.dumps({'review_audit':'passed','sessions':len(reviewed['sessions']),'peak':reviewed['peak']}),flush=True)


def prepare():
    audit.check_hashes('baseline.sha256');audit.check_hashes('baseline-external.sha256');audit.check_hashes('new-reviewers.sha256')
    baseline=audit.load(C/'baseline-sessions.json');additions=audit.load(C/'reviewer-audit.json')['sessions']
    jobs=audit.load(C/'baseline-jobs.json')+audit.load(C/'jobs.json')
    assert len(jobs)==1134 and len({(j['bug'],j['arm']) for j in jobs})==1134
    audit.write_once(C/'ready-jobs.json',jobs)
    out=R/audit.ROUND;key=R/'private'/f'key-{audit.ROUND}.json'
    if not out.exists() and not key.exists():
        out.mkdir();shutil.copyfile(R/'verify9/SPEC.md',out/'SPEC.md')
        rows=baseline+additions
        arms=','.join(sorted({r['arm'] for r in rows}))
        configs=','.join(sorted({r['model']+':'+r['effort'] for r in rows}))
        run(['python3','-B','build_verify.py',audit.ROUND,'--arms',arms,'--configs',configs,
             '--ready-jobs',str(C/'ready-jobs.json'),'--bugs-from',str(C/'jobs.json')],'build.log')
    else:
        assert out.exists() and key.exists(), 'Interrupted pool preparation: reconcile original key/batches before resuming'
    result=receipt('pool-audit.json',audit.pool())
    frozen=[out/'SPEC.md',key,*out.glob('batch-*.json')]
    lines=audit.hash_lines(frozen);manifest=C/'pool.sha256'
    if manifest.exists():assert manifest.read_text().splitlines()==lines
    else:manifest.write_text('\n'.join(lines)+'\n')
    print(json.dumps({'pool_audit':'passed',**result}),flush=True)
    return sorted(int(p.stem.split('-')[1]) for p in out.glob('batch-*.json'))


def judge(profile, batches, log):
    command=['python3','-B','verify_driver.py',audit.ROUND,'--kind','verify','--target','localhost',
             '--repo-commit','3c3fa4b13bc32ba99fc349355081ee0d007831fe','--model','opus','--effort','high',
             '--profiles',profile,'--quota-scope','claude','--concurrency','16' if len(batches)>1 else '1']
    for n in batches:command.extend(['--batch',str(n)])
    run(command,log)


def parallel_judges(assignments, prefix):
    # Batch sets are disjoint; the shared runner also locks each batch against duplicate ownership.
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures=[pool.submit(judge,profile,batches,prefix+'-'+profile+'.log') for profile,batches in assignments]
        errors=[]
        for future in futures:
            try:future.result()
            except Exception as error:errors.append(error)
    if errors:raise errors[0]


def verify(batches):
    assert len(batches)>=2 and batches[:2]==[0,1]
    assignments=[(profile,[n for n in batches if n%2==i]) for i,profile in enumerate(['claude4','claude7'])]
    parallel_judges([(profile,ids[:1]) for profile,ids in assignments],'judge-canary')
    canaries=receipt('judge-canary-audit.json',audit.judges([0,1]))
    assert {r['profile'] for r in canaries}=={'claude4','claude7'}
    audit.check_hashes('pool.sha256')
    print(json.dumps({'opus_canaries':'passed','profiles':['claude4','claude7']}),flush=True)
    parallel_judges(assignments,'judges')
    judged=receipt('judge-audit.json',audit.judges())
    assert len(judged)==len(batches)
    audit.check_hashes('pool.sha256');audit.check_hashes('baseline.sha256');audit.check_hashes('baseline-external.sha256');audit.check_hashes('new-reviewers.sha256')
    print(json.dumps({'opus_judges':'passed','batches':len(judged)}),flush=True)


def analysis():
    # A result is accepted only after the caller's complete judge and source audits passed.
    result=subprocess.run(['python3','-B','score.py',audit.ROUND],cwd=STUDY,env=ENV,capture_output=True,text=True,check=True)
    (C/'score.json').write_text(result.stdout)
    score=json.loads(result.stdout)
    assert score['sessions']==1796 and score['diffs']==70 and not score['unverified_diffs_left_out']
    print(json.dumps({'score':'passed','sessions':score['sessions'],'diffs':score['diffs']}),flush=True)
    low,medium,high='gpt-6.2-sol/low','gpt-6.2-sol/medium','gpt-6.2-sol/high'
    for severity in ['high','medium','low','high,medium','high,medium,low']:
        suffix=severity.replace(',','-')
        for first,options in [('low',[low,medium,high]),('medium',[medium,high])]:
            run(['python3','-B','compare.py',audit.ROUND,*options,'--severities',severity,'--bootstrap','20000'],f'paired-{first}-{suffix}.txt')
    caps='Sol/medium=2,Terra/xhigh=2,Sol/xhigh=2,Greptile/base=1,Greptile/plus=1,Ember/xhigh=1,Gemini/high=1,Gemini/medium=1,Kimi/high=1,gpt-6.2-sol/low=3,gpt-6.2-sol/medium=3,gpt-6.2-sol/high=1'
    for severity in ['high,medium,low','high,medium','high']:
        run(['python3','-B','pareto.py',audit.ROUND,'--exclude','DSF/low','--bootstrap','300',
             '--sessions',caps,'--severities',severity],'pareto-'+severity.replace(',','-')+'.log')
    print(json.dumps({'analysis':'complete'}),flush=True)


if __name__=='__main__':
    import fcntl
    os.chdir(STUDY)
    with (C/'campaign.lock').open('w') as owner:
        fcntl.flock(owner,fcntl.LOCK_EX|fcntl.LOCK_NB)
        wait_reviews();batches=prepare();verify(batches);analysis()
