#!/usr/bin/env python3
"""Replay frozen live GLiDE requests, gate independence, then evaluate ask_kind.

Run with python3 -I. All evidence and provider results stay under --study.
The recorded activity.request is glide_turn::project's already-redacted output;
match it to an assessment by session and its entire logged probability vector.
No store, daemon, or Rust build is used.
"""
import argparse
import concurrent.futures
import copy
import hashlib
import json
import pathlib
import random
import re
import time
import urllib.error
import urllib.request

ENDPOINT = 'https://api.fastino.ai/v1/systemone'
REPO = pathlib.Path(__file__).resolve().parent.parent
SEED = 2026111705


def dump(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    temporary.replace(path)


def compact(value):
    # Preserve the recorded key order: Fastino can serialize object-valued state
    # into model text, so structurally equal objects need not produce equal scores.
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'))


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True,
                                     separators=(',', ':')).encode()).hexdigest()


def redact(value):
    if isinstance(value, str):
        return re.sub(r'[A-Za-z0-9_.+\-]{24,}', '[redacted]', value)
    if isinstance(value, list):
        return [redact(x) for x in value]
    if isinstance(value, dict):
        return {k: v if k == 'id' or k.endswith('_id') else redact(v) for k, v in value.items()}
    return value


def project(state, history):
    """Reproject the history-derived fields; runtime facts remain as recorded."""
    state = copy.deepcopy(state)
    state['authorization'] = copy.deepcopy(history)
    state['transcript_summary'] = ''
    messages = history['messages']
    latest = next((i for i in range(len(messages) - 1, -1, -1)
                   if messages[i]['role'] == 'user'), None)
    state['latest_user_message'] = messages[latest]['text'] if latest is not None else ''
    state['current_assistant_replies'] = [m['text'] for m in messages[(latest + 1) if latest is not None else 0:]
                                          if m['role'] == 'assistant']
    return redact(state)


def shrink(history):
    """Port of mj-worker/src/acp/verdict_client/glide_turn.rs::shrink."""
    messages = history['messages']
    latest = next((i for i in range(len(messages) - 1, -1, -1)
                   if messages[i]['role'] == 'user'), None)
    if latest is not None and latest > 0:
        removed = messages.pop(0)
    else:
        if not messages:
            return False
        # Rust Iterator::max_by_key chooses the last tied maximum; len is UTF-8 bytes.
        index = max(range(len(messages)), key=lambda i: (len(messages[i]['text'].encode()), i))
        removed = messages[index]
        text = removed['text'].encode()
        if not text:
            return False
        keep = len(text) // 2
        head, tail = keep // 2, keep - keep // 2
        removed['text'] = (text[:head].decode('utf-8', 'ignore')
                           + text[len(text) - tail:].decode('utf-8', 'ignore'))
    if removed['role'] == 'user':
        history['authorization_complete'] = False
    else:
        history['assistant_history_omitted'] = True
    return True


def load_items(study, all_turns=False):
    key = json.loads((study / 'labeling/private/key.json').read_text())
    turns = {(t['session'], t['completed_ordinal']): t for t in
             map(json.loads, (study / 'turns.jsonl').read_text().splitlines())}
    if all_turns:
        labeled = {(v['session'], v['completed_ordinal']): i for i, v in key.items() if not v['seeded']}
        key = {labeled.get(k, 'turn-' + digest([t['session'], t['turn_id']])[:24]):
               dict(session=t['session'], turn_id=t['turn_id'], completed_ordinal=t['completed_ordinal'],
                    seeded=False, unlabeled=k not in labeled) for k, t in turns.items()}
    wanted = {v['session'] for v in key.values() if not v['seeded']}
    requests = {}
    for root in ('raw-decision-logs', 'raw-remote-logs'):
        for path in sorted((study / root).rglob('*.jsonl')):
            for number, line in enumerate(path.open(), 1):
                try:
                    record = json.loads(line)
                except ValueError:
                    continue  # A copied rotating log can end in an incomplete append.
                if record.get('session_id') not in wanted:
                    continue
                tech = record.get('technical') or {}
                if tech.get('contract') == 'glide-turn-v1' and tech.get('request') and tech.get('probabilities'):
                    match = (record['session_id'], digest(tech['probabilities']))
                    requests.setdefault(match, {})[record['id']] = (record, str(path.relative_to(study)), number)
    items = {}
    lineage = []
    missing = []
    for item, meta in sorted(key.items()):
        if meta['seeded']:
            continue
        turn = turns[(meta['session'], meta['completed_ordinal'])]
        matches = list(requests.get((meta['session'], digest(turn['glide'])), {}).values())
        if not matches:
            missing.append(dict(item=None if meta.get('unlabeled') else item, cache_key=item,
                                session=meta['session'], turn_id=meta['turn_id'],
                                completed_ordinal=meta['completed_ordinal'], episode=turn['episode']))
            continue
        # Identical vectors can recur. Preserve evidence identity rather than guess by time.
        states = {digest(r['technical']['request']) for r, _, _ in matches}
        if len(states) != 1:
            raise ValueError(f'{item}: ambiguous matching wire requests ({len(states)} distinct states)')
        record, path, line = matches[0]
        state = record['technical']['request']
        if compact(project(state, state['authorization'])) != compact(state):
            raise ValueError(f'{item}: history reproject differs from recorded wire request')
        items[item] = dict(item=None if meta.get('unlabeled') else item, cache_key=item, session=meta['session'], turn_id=meta['turn_id'],
                           state=state, logged=turn['glide'])
        lineage.append(dict(item=item, session=meta['session'], turn_id=meta['turn_id'],
                            request_id=record['id'], path=path, line=line, state_sha256=digest(state),
                            wire_state_sha256=hashlib.sha256(compact(state).encode()).hexdigest()))
    suffix = '-all' if all_turns else ''
    dump(study / f'familyB/request-lineage{suffix}.json', lineage)
    dump(study / f'familyB/missing-wire-requests{suffix}.json', missing)
    return items


def size_refusal(status, body):
    try:
        error = json.loads(body).get('error')
    except (ValueError, AttributeError):
        return False
    return ((status == 413 and error == 'body_too_large')
            or (status == 400 and isinstance(error, dict) and error.get('code') == 'input_too_long'))


def post(state, questions, key):
    state = copy.deepcopy(state)
    history = copy.deepcopy(state['authorization'])
    attempts, retries, shrunk = 0, 0, False
    events = []
    while True:
        body = compact(dict(model='fastino/GLiDE', state=state, questions=questions, store=False)).encode()
        request = urllib.request.Request(ENDPOINT, data=body, headers={
            'Content-Type': 'application/json', 'X-API-Key': key, 'User-Agent': 'mj-attention-live-eval/1'})
        attempts += 1
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                result = json.load(response)
            return dict(response=result, shrunk=shrunk, attempts=attempts, events=events,
                        state_sha256=digest(state), request_bytes=len(body))
        except urllib.error.HTTPError as error:
            detail = error.read().decode('utf-8', 'replace')
            events.append(dict(status=error.code, detail=detail, attempt=attempts))
            if size_refusal(error.code, detail):
                while shrink(history):
                    candidate = project(state, history)
                    if len(compact(candidate).encode()) < len(compact(state).encode()):
                        state, shrunk = candidate, True
                        break
                else:
                    raise RuntimeError('size refusal: evidence cannot shrink further') from error
                continue
            if error.code == 429 or 500 <= error.code <= 599:
                retries += 1
                delay = error.headers.get('Retry-After', '')
                # Backoff bounds delay, not attempts or request throughput.
                time.sleep(float(delay) if delay.replace('.', '', 1).isdigit() else min(60, 2 ** min(retries, 6)))
                continue
            return dict(error=f'HTTP {error.code}: {detail}', shrunk=shrunk, attempts=attempts, events=events)
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            # Preserve a failed attempt; transport uncertainty must remain visible.
            return dict(error=f'{type(error).__name__}: {error}', shrunk=shrunk,
                        attempts=attempts, events=events)


def cached_call(out, item, questions, key):
    path = out / (item['cache_key'] + '.json')
    identity = dict(state_sha256=digest(item['state']), questions_sha256=digest(questions),
                    wire_sha256=hashlib.sha256(compact(dict(model='fastino/GLiDE', state=item['state'],
                                                            questions=questions, store=False)).encode()).hexdigest())
    if path.exists():
        saved = json.loads(path.read_text())
        if saved['identity'] != identity:
            raise ValueError(f'cache identity changed: {path}')
        if 'response' in saved:
            return saved
    result = post(item['state'], questions, key)
    result['identity'] = identity
    dump(path, result)
    return result


def differences(reference, response, item, comparison):
    differences = []
    answers = response.get('response', {}).get('answers', {})
    for question, value in reference.items():
        for option, probability in value['probabilities'].items():
            actual = answers.get(question, {}).get('probabilities', {}).get(option)
            differences.append(dict(item=item, comparison=comparison, question=question, option=option,
                                    logged=probability, actual=actual,
                                    difference=abs(probability - actual) if actual is not None else None))
    return differences


def gate(study, items, current, wording, key):
    out = study / 'familyB/gate-amendment7'
    population = json.loads((study / 'labeling/private/key.json').read_text())
    sample = random.Random(SEED).sample(sorted(i for i, v in population.items() if not v['seeded']), 50)
    if any(i not in items for i in sample):
        raise ValueError('A sampled item lacks a recorded wire request')
    dump(out / 'sample.json', dict(seed=SEED, items=sample, baseline_items=sample[:10]))
    groups = {}
    for group, ids, questions in [('plus', sample, dict(current, **wording)),
                                  ('baseline', sample[:10], current),
                                  ('repeat', sample[:10], current)]:
        def call(item):
            return item, cached_call(out / group, items[item], questions, key)
        with concurrent.futures.ThreadPoolExecutor() as executor:
            groups[group] = dict(executor.map(call, ids))
        print(f'Gate {group}: {len(groups[group])} complete', flush=True)
    diffs = []
    for item in sample:
        diffs.extend(differences(items[item]['logged'], groups['plus'][item], item, 'reproduction'))
    for item in sample[:10]:
        base = groups['baseline'][item]
        diffs.extend(differences(items[item]['logged'], base, item, 'reproduction'))
        reference = base.get('response', {}).get('answers', {})
        # Missing baseline vectors fail the gate rather than making an empty comparison pass.
        reference = {q: reference.get(q, items[item]['logged'][q]) for q in current}
        diffs.extend(differences(reference, groups['plus'][item], item, 'independence'))
        diffs.extend(differences(reference, groups['repeat'][item], item, 'determinism'))
    measures = {}
    for comparison in ['independence', 'reproduction', 'determinism']:
        rows = [d for d in diffs if d['comparison'] == comparison]
        failures = [d for d in rows if d['difference'] is None or d['difference'] > 0.02]
        measures[comparison] = dict(passed=not failures,
            largest_difference=max((d['difference'] for d in rows if d['difference'] is not None), default=None),
            failed_options=len(failures), failed_items=sorted({d['item'] for d in failures}))
    errors = {f'{g}/{i}': r['error'] for g, rs in groups.items() for i, r in rs.items() if 'error' in r}
    if errors or not measures['independence']['passed'] or not measures['determinism']['passed']:
        branch = 'stop'
    elif measures['reproduction']['passed']:
        branch = 'ask_kind_alone'
    else:
        branch = 'fresh_full_set'
    results = [r for rs in groups.values() for r in rs.values()]
    summary = dict(seed=SEED, branch=branch, measurements=measures, errors=errors,
                   attempts=sum(r['attempts'] for r in results),
                   input_tokens=sum(r.get('response', {}).get('usage', {}).get('input_tokens', 0) for r in results),
                   token_usage=sum(r.get('response', {}).get('token_usage', 0) for r in results))
    dump(out / 'differences.json', diffs)
    dump(out / 'summary.json', summary)
    print(json.dumps(summary), flush=True)
    return branch


def batch(study, items, current, wordings, branch, all_turns=False, amendment=7):
    out = study / 'familyB'
    summaries = {}
    for wording, new_questions in wordings.items():
        questions = dict(current, **new_questions) if branch == 'fresh_full_set' else new_questions
        name = f'answers-{"all-" if all_turns else ""}{wording}.jsonl'
        answers = out / name
        existing = {}
        if answers.exists():
            for line in answers.read_text().splitlines():
                entry = json.loads(line)
                existing[(entry['session'], entry['turn_id'])] = entry
        missing_path = out / ('missing-wire-requests-all.json' if all_turns else 'missing-wire-requests.json')
        exclusions = json.loads(missing_path.read_text())
        metadata = dict(amendment=amendment, branch=branch, wording=wording,
                        questions_sha256=digest(questions),
                        probability_source='fresh_full_set' if branch == 'fresh_full_set' else 'logged_current_plus_fresh_ask_kind',
                        excluded_items=[e['cache_key'] for e in exclusions],
                        excluded_missing_wire=len(exclusions))
        meta_path = answers.with_suffix('.metadata.json')
        if meta_path.exists() and json.loads(meta_path.read_text()) != metadata:
            raise ValueError(f'Answer metadata changed: {answers}')
        dump(meta_path, metadata)
        pending = [i for i, item in items.items() if (item['session'], item['turn_id']) not in existing]
        def evaluate(item):
            return item, cached_call(out / f'raw-amendment{amendment}-{wording}', items[item], questions, key)
        key = (pathlib.Path.home() / '.secrets/fastino_api_key').read_text().strip()
        failures = {}
        with concurrent.futures.ThreadPoolExecutor() as executor, answers.open('a') as stream:
            for item, result in executor.map(evaluate, pending):
                if 'error' in result:
                    failures[item] = result['error']
                    continue
                entry = {k: items[item][k] for k in ('item', 'session', 'turn_id')}
                full_answers = result['response']['answers']
                entry.update(wording=wording, probabilities=full_answers['ask_kind']['probabilities'], shrunk=result['shrunk'])
                if branch == 'fresh_full_set':
                    entry['answers'] = full_answers
                stream.write(json.dumps(entry) + '\n')
                stream.flush()
                existing[(entry['session'], entry['turn_id'])] = entry
                if len(existing) % 100 == 0:
                    print(f'{name}: {len(existing)}/{len(items)}', flush=True)
        records = [json.loads((out / f'raw-amendment{amendment}-{wording}' / (i + '.json')).read_text())
                   for i in items if (out / f'raw-amendment{amendment}-{wording}' / (i + '.json')).exists()]
        summary = dict(file=name, answered=len(existing), expected=len(items), failures=failures,
                       attempts=sum(r['attempts'] for r in records),
                       shrunk=sum(r['shrunk'] for r in records),
                       input_tokens=sum(r.get('response', {}).get('usage', {}).get('input_tokens', 0) for r in records),
                       token_usage=sum(r.get('response', {}).get('token_usage', 0) for r in records))
        dump(out / (name + '.summary.json'), summary)
        summaries[name] = summary
        print(json.dumps(summary), flush=True)
    return summaries


def run(study, phase):
    items = load_items(study)
    out = study / 'familyB'
    current = json.loads((REPO / 'mj-core/src/activity/eot_glide_questions.json').read_text())
    wordings = {w: json.loads((out / f'questions-{w}.json').read_text()) for w in ('A', 'B')}
    print(f'Prepared {len(items)} non-seed items with exact wire-state reprojection.', flush=True)
    if phase == 'prepare':
        return
    if phase == 'wording-c':
        previous_gate = json.loads((out / 'gate-amendment7/summary.json').read_text())
        if previous_gate['branch'] != 'ask_kind_alone' or previous_gate['errors']:
            raise ValueError('Existing gate does not authorize ask_kind alone')
        frozen = out / 'questions-C.json'
        if hashlib.sha256(frozen.read_bytes()).hexdigest()[:12] != 'c38edd067c9f':
            raise ValueError('Wording C differs from Amendment 8')
        if len(items) != 566:
            raise ValueError('Eligible labeled population differs from Amendment 8')
        result = batch(study, items, current, {'C': json.loads(frozen.read_text())},
                       'ask_kind_alone', amendment=8)
        if any(s['answered'] != s['expected'] for s in result.values()):
            raise RuntimeError('Wording C batch incomplete; see summary')
        return
    key = (pathlib.Path.home() / '.secrets/fastino_api_key').read_text().strip()
    branch = gate(study, items, current, wordings['A'], key)
    if branch == 'stop' or phase == 'independence':
        return
    summaries = batch(study, items, current, wordings, branch)
    if any(s['answered'] != s['expected'] for s in summaries.values()):
        raise RuntimeError('Labeled batches incomplete; see file summaries')
    dump(out / 'labeled-complete.json', summaries)
    print('LABELED COMPLETE', flush=True)
    if phase == 'labeled':
        return
    all_items = load_items(study, all_turns=True)
    missing = json.loads((out / 'missing-wire-requests-all.json').read_text())
    if {m['cache_key'] for m in missing} != {'iddf94077', 'turn-4c9a128bf7a15b2ef83f8dc8',
            'turn-4d79e8078b6d696aea947798', 'turn-8874f62686c6871a248a4e79',
            'turn-9b1ff590ab698a40be2cbf85'}:
        raise RuntimeError(f'Additional all-turn exclusions need parent decision: {missing}')
    batch(study, all_items, current, wordings, branch, all_turns=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--study', type=pathlib.Path, default=pathlib.Path('/mnt/optane/tmp/attention-live-20261116'))
    parser.add_argument('--phase', choices=['prepare', 'independence', 'labeled', 'all', 'wording-c'], default='all')
    args = parser.parse_args()
    run(args.study, args.phase)
