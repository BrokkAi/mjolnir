#!/usr/bin/env python3
"""Evaluate sanitized continuation fixtures; no session discovery or live-store access."""
import argparse
import concurrent.futures
import json
import math
import os
from pathlib import Path
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
QUESTIONS = ROOT / 'mj-core/src/continuation/questions.json'
THRESHOLD = 0.90


def evidence(case, turns):
    messages = case['messages']
    if turns:
        starts = [i for i, m in enumerate(messages) if m['role'] == 'user']
        messages = messages[starts[-turns]:] if len(starts) > turns else messages
    assert messages and messages[-1]['role'] == 'assistant'
    assert all(m['role'] in ('user', 'assistant') and m['text'].strip() for m in messages)
    return {'assistant_history_omitted': False, 'messages': [
        {'id': str(i), 'role': m['role'], 'text': m['text']} for i, m in enumerate(messages)]}


def evaluate(case, turns, key, questions):
    state = evidence(case, turns)
    payload = json.dumps({'model': 'jev-latest', 'state': state, 'questions': questions}).encode()
    assert len(payload) <= 64 * 1024
    start = time.monotonic()
    request = urllib.request.Request('https://api.typesafe.ai/v1/systemone', data=payload,
        headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=15) as response:
        raw = response.read(65537)
        assert len(raw) <= 65536
        result = json.loads(raw)
    scores = {}
    for name in ('unfinished', 'no_input_needed'):
        answer = result['answers'][name]
        value = answer['noul']
        assert answer['type'] == 'noul' and isinstance(value, (int, float)) and not isinstance(value, bool)
        assert math.isfinite(value) and 0 <= value <= 1
        scores[name] = value
    actual = all(value >= THRESHOLD for value in scores.values())
    return {'id': case['id'], 'user_turns': turns or 'all', 'expected_continue': case['expected_continue'],
        'actual_continue': actual, 'scores': scores, 'seconds': time.monotonic() - start,
        'response': result, 'state': state, 'excluded_from_model_scoring': case.get('excluded_from_model_scoring')}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('fixtures', type=Path)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--run', action='store_true', help='Explicitly send sanitized fixtures to TypeSafe')
    mode.add_argument('--report', type=Path, help='Score saved responses locally with the current cutoff')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    cases = json.loads(args.fixtures.read_text())
    for case in cases:
        assert isinstance(case['expected_continue'], bool)
        evidence(case, 0)
    if not args.run and not args.report:
        print(f'Validated {len(cases)} sanitized scenarios; no network requests.')
        return
    if args.report:
        results = json.loads(args.report.read_text())['results']
        by_id = {c['id']: c for c in cases}
        for result in results:
            result['actual_continue'] = all(v >= THRESHOLD for v in result['scores'].values())
            result['excluded_from_model_scoring'] = by_id[result['id']].get('excluded_from_model_scoring')
    else:
        assert args.output, '--output is required for a live evaluation'
        key = os.environ.get('TYPESAFE_API_KEY') or (Path.home() / '.secrets/typesafe_api_key').read_text().strip()
        questions = json.loads(QUESTIONS.read_text())
        jobs = [(case, turns) for case in cases for turns in (1, 3, 0)]
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(lambda job: evaluate(*job, key, questions), jobs))
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps({'threshold': THRESHOLD, 'questions': questions, 'results': results}, indent=2) + '\n')
    for turns in (1, 3, 'all'):
        selected = [r for r in results if r['user_turns'] == turns and not r.get('excluded_from_model_scoring')]
        fp = sum(r['actual_continue'] and not r['expected_continue'] for r in selected)
        tp = sum(r['actual_continue'] and r['expected_continue'] for r in selected)
        positives = sum(r['expected_continue'] for r in selected)
        print(f'{turns} user turns: {fp} false positives; {tp}/{positives} positives continued; {len(selected)} cases')


if __name__ == '__main__':
    main()
