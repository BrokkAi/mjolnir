"""Run one profile-pinned Sol 6.2/high review per historical diff."""
import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
import driver as d

MODEL, EFFORT, ARM = 'gpt-6.2-sol', 'high', 'U'
CAMPAIGN = d.ROOT / 'sol-62-high'


def prompt(bug, place, before):
    template = json.loads((d.ROOT / 'sol-62-repeats/prompt-Q.json').read_text())['items'][0]['text']
    template = re.sub(r'/workspace/[0-9a-f]{32}/mjolnir', place['cwd'], template)
    template, count = re.subn(r'(diff --no-ext-diff --ignore-submodules=none) [0-9a-f]{40} [0-9a-f]{40}',
                             lambda m: f"{m[1]} {before['base_tree']} {before['head_tree']}", template)
    assert count == 1
    command = ['podman', 'exec', place['container'], 'git', '-C', place['cwd'], 'diff']
    numstat = subprocess.check_output(command + ['--numstat', before['base_tree'], before['head_tree']], text=True)
    short = subprocess.check_output(command + ['--shortstat', before['base_tree'], before['head_tree']], text=True).strip()
    changed = sum(int(n) for line in numstat.splitlines() for n in line.split('\t')[:2] if n.isdigit())
    stat = f'Per-file diffstat for the captured changes ({changed} changed lines):\n'
    stat += f'<changed_files source="git diff --numstat of the captured trees" trust="deterministic" changed_lines="{changed}">\n'
    stat += f"Repository: {place['cwd']} -- {short}\n{numstat}</changed_files>"
    template, count = re.subn(r'Per-file diffstat.*?</changed_files>', lambda _: stat, template, flags=re.S)
    assert count == 1
    context = d.context_text(bug, ARM).strip()
    template, count = re.subn(r'(<user_message index="1" current_outer_turn="true">\n).*?(\n</user_message>)',
                             lambda m: m[1] + context + m[2], template, flags=re.S)
    assert count == 1
    old = 'as the complete text of a single handback call. Do not edit files or push. After calling handback, stop.'
    assert old in template
    return template.replace(old, 'as your complete final response. Do not edit files or push. After returning your findings, stop.')


def text_of(item):
    if isinstance(item.get('text'), str):
        return item['text']
    return ''.join(block.get('text', '') for block in item.get('body', {}).get('content', []) if block.get('type') == 'text')


def run(job):
    bug = d.CORPUS[job['bug']]
    directory = d.ROOT / 'runs' / job['bug'] / ARM
    if (directory / 'done.json').exists():
        if not (directory / 'suspend.json').exists():
            completed_sid = json.loads((directory / 'done.json').read_text())['session_id']
            completed_state = d.session(completed_sid)
            if completed_state.get('lifecycle') == 'live':
                assert completed_state.get('is_idle'), 'Completed review session is unexpectedly busy'
                d.save(directory/'suspend.json',d.mj('suspend','--session',completed_sid,'--discard-untracked',timeout=600))
            else:
                d.save(directory/'suspend.json',{'already_not_live':completed_state.get('lifecycle')})
        return 'done'
    identity = dict(bug=job['bug'], arm=ARM, profile=job['profile'], model=MODEL, effort=EFFORT,
                    intro=bug['intro_commit'], base=bug['base_commit'], protocol='profile-pinned-top-level-review')
    if (directory / 'launch-intent.json').exists():
        assert json.loads((directory / 'launch-intent.json').read_text()) == identity
        if not (directory / 'launch.json').exists():
            raise RuntimeError('Launch acknowledgement missing: reconcile session state before retrying')
        sid = json.loads((directory / 'launch.json').read_text())['session_id']
    else:
        d.save(directory / 'launch-intent.json', identity)
        launch = d.mj('new', '--workspace-id', d.workspace_id(), '--profile', job['profile'], '--target', 'podman',
                      '--bundle', 'mjolnir', '--at', bug['intro_commit'], '--base', bug['base_commit'],
                      '--no-later-history', '--no-project-memory', '--no-github-events', '--no-github-credentials',
                      '--subagents', 'none', '--oracle', 'disabled', '--review', 'off',
                      '--title', 'review-arms ' + hashlib.sha256((job['bug']+ARM).encode()).hexdigest()[:8], timeout=900)
        d.save(directory / 'launch.json', launch)
        sid = launch['session_id']
    state = d.wait_idle(sid) if not (directory / 'prompt-intent.json').exists() else d.session(sid)
    assert state['profile_id'] == job['profile']
    assert state['no_later_history'] and state['at'] == identity['intro'] and state['base'] == identity['base']
    if (directory / 'placement.json').exists():
        place = json.loads((directory / 'placement.json').read_text())
        assert d.container(sid) == place
    else:
        place = d.container(sid)
        d.save(directory / 'placement.json', place)
    expected = json.loads((d.ROOT/'arm-O-image.json').read_text())['image']
    assert place['image'] == expected, f"Image changed: {place['image']} != {expected}; no prompt sent"
    if (directory / 'pre-inventory.json').exists():
        before = json.loads((directory / 'pre-inventory.json').read_text())
    else:
        # No native review baseline is needed: the prompt names both tree hashes.
        before = d.inventory(place, 'audit', identity['base'])
        d.save(directory / 'pre-inventory.json', before)
    reference = json.loads((directory.parent/'O/pre-inventory.json').read_text())
    assert all(before[k] == reference[k] for k in ['head', 'head_tree', 'base_tree', 'diff_sha256'])
    assert not any(before[k] for k in ['remotes','alternates','status','ignored'])
    cid = 'review-arms-sol62-high-' + sid[:16]
    output_dir = directory / 'reviewers' / sid[:12]
    if not (directory / 'prompt-intent.json').exists():
        for key, value in [('model',MODEL), ('effort',EFFORT)]:
            options = {x['key']:x.get('current') for x in d.session(sid)['config_options']}
            if options.get(key) != value:
                d.mj('set-config','--session',sid,'--key',key,'--value',value)
        options = {x['key']:x.get('current') for x in d.session(sid)['config_options']}
        assert all(options[k] == v for k,v in [('model',MODEL),('effort',EFFORT)])
        assert options.get('fast-mode', 'off') == 'off'
        d.save(directory/'settings.json', options)
        (directory/'historical-context.txt').write_text(d.context_text(bug, ARM))
        (directory/'review-prompt.txt').write_text(prompt(bug,place,before))
        d.save(directory/'prompt-intent.json', {'session_id':sid,'command_id':cid})
    if not (directory/'prompt-receipt.json').exists():
        receipt = d.mj('message','--session',sid,'--command-id',cid,'--prompt-file',directory/'review-prompt.txt')
        d.save(directory/'prompt-receipt.json',receipt)
    # Observe the live runtime immediately, before a short review can finish.
    if not (output_dir/'runtime.json').exists():
        runtime = d.reviewer_runtime(sid, place['container'])
        assert runtime and runtime.get('image') == place['image']
        d.save(output_dir/'runtime.json',runtime)
    while True:
        state = d.session(sid)
        outcome = state.get('last_turn_outcome') or {}
        if outcome.get('command_id') == cid and state.get('is_idle'):
            assert outcome['outcome']['kind']=='completed' and outcome['outcome']['stop_reason']=='EndTurn', outcome
            break
        assert not state.get('has_error') and state.get('lifecycle')!='failed', state
        if state.get('attention') == 'needs_input':
            raise RuntimeError('Review is waiting for input; retain evidence and diagnose')
        time.sleep(4)
    assert state['profile_id'] == job['profile'], 'Completed on an unexpected profile'
    d.save(directory/'completion.json',outcome)
    pages = d.transcript(sid)
    d.save(directory/'transcript.json',pages)
    items = [item for page in pages for item in page['items']]
    users = [item for item in items if item['role']=='user']
    assert len(users)==1 and text_of(users[0])==(directory/'review-prompt.txt').read_text()
    d.save(output_dir/'prompt-time.json',{'prompt_created_at_ms':users[0]['created_at_ms']})
    usage = d.mj('usage','--session',sid)
    d.save(output_dir/'usage.json',usage)
    assert d.capture_pilot_rollouts(output_dir,sid,reuse_existing=True,directory_name='codex-rollouts')
    rollouts = sorted((output_dir/'codex-rollouts').glob('rollout-*.jsonl'))
    d.codex_turn_model(usage,cid,MODEL,EFFORT,pages=pages,rollouts=rollouts)
    if usage['turn_selections'][cid]['effort'] is None:
        d.save(output_dir/'native-turn-context.json',d._codex_turn_context(usage,cid,pages,rollouts))
    turns = [turn for turn in usage['turns'] if turn['command_id']==cid]
    assert len(turns)==1 and turns[0]['completed_at_ms']>users[0]['created_at_ms']
    responses = [text_of(item) for item in items if item['role'] in ['agent','message'] and text_of(item).strip()]
    assert responses, 'No final response in captured transcript; inspect without launching another review'
    raw_output = responses[-1].strip()
    (output_dir/'raw-final.md').write_text(raw_output+'\n')
    output = re.sub(r'(?m)^(\s*)(\[P\d\])',r'\1- \2',raw_output)
    (output_dir/'output.md').write_text(output+'\n')
    selection={'profile':job['profile'],'model':MODEL,'effort':EFFORT}
    d.save(output_dir/'stored.json',{'selection':selection,'command_id':cid,'final_message':raw_output,'output_kind':'final_response','normalization':'prefix bare priority headings with a bullet'})
    after = d.inventory(place,'audit',identity['base'])
    d.save(directory/'post-inventory.json',after)
    problems, refs = d.compare_inventories(before,after)
    assert not problems, problems
    reviewer=dict(child=sid,slot=0,profile=job['profile'],model=MODEL,effort=EFFORT,has_output=True,handback=False,
                  output_kind='final_response',image=place['image'])
    d.save(directory/'reviewers.json',[reviewer])
    d.save(directory/'done.json',dict(identity,session_id=sid,reviewers=[reviewer],inventory_problems=problems))
    # Result and evidence are saved before cleanup; a resumed driver never reruns a done review.
    d.save(directory/'suspend.json',d.mj('suspend','--session',sid,'--discard-untracked',timeout=600))
    return 'done'


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jobs',type=pathlib.Path,required=True)
    parser.add_argument('--concurrency',type=int,default=16)
    args=parser.parse_args()
    jobs=json.loads(args.jobs.read_text())
    assert len({(j['bug'],j['arm']) for j in jobs})==len(jobs)
    assert {j['profile'] for j in jobs}<={'codex3','codex4'} and all(j['arm']==ARM for j in jobs)
    def one(job):
        try:
            result=run(job)
            print(json.dumps({'job':job,'state':result}),flush=True)
            return result=='done'
        except Exception as error:
            import traceback
            directory = d.ROOT/'runs'/job['bug']/ARM
            d.save(directory/'error.json',{'error':str(error),'traceback':traceback.format_exc()})
            print(json.dumps({'job':job,'state':'capture-error-reconciling','error':str(error)}),flush=True)
            # The profile slot owns a possibly accepted turn until its command is terminal.
            # Read failures cannot release it and admit another review above the profile width.
            if (directory/'prompt-intent.json').exists():
                intent = json.loads((directory/'prompt-intent.json').read_text())
                sid, command = intent['session_id'], intent['command_id']
                while True:
                    try:
                        state = d.session(sid)
                        outcome = state.get('last_turn_outcome') or {}
                        if (outcome.get('command_id') == command and state.get('is_idle')) or state.get('lifecycle') in ['failed','stopped','suspended']:
                            d.save(directory/'reconciled-turn.json',{'state':state,'command_id':command})
                            break
                        if not (directory/'prompt-receipt.json').exists():
                            # Exact command-ID replay reconciles a lost acknowledgement, never a second sample.
                            receipt = d.mj('message','--session',sid,'--command-id',command,'--prompt-file',directory/'review-prompt.txt')
                            d.save(directory/'prompt-receipt.json',receipt)
                    except Exception as reconciliation_error:
                        d.save(directory/'reconciliation-error.json',{'error':str(reconciliation_error),'command_id':command})
                    time.sleep(4)  # The same poll cadence as the ordinary active-turn loop.
            print(json.dumps({'job':job,'state':'failed','error':str(error)}),flush=True)
            return False
    pools={profile:concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) for profile in sorted({j['profile'] for j in jobs})}
    futures=[pools[j['profile']].submit(one,j) for j in jobs]
    ok=all([f.result() for f in futures])
    for pool in pools.values():pool.shutdown()
    return 0 if ok else 1

if __name__=='__main__':
    sys.exit(main())
