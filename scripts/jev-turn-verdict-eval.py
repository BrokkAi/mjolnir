#!/usr/bin/env python3
"""Evaluate synthetic v3 turn scenarios only; never read session data."""
import argparse
import json
import os
from pathlib import Path
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def scenarios():
    # All text is invented for this evaluation, including the bifrost2-style case.
    return [
        ('approval_and_heap', 'Prepare the release; analyze the heap independently.',
         'The release is ready. May I deploy it now? The independent heap analysis task is still running.',
         1, '', 'input'),
        ('background_only', 'Analyze the heap.',
         'The heap analysis is still running. I will report when it finishes.', 1, '', 'background'),
        ('finished', 'Fix the parser.', 'Implemented the parser fix. All tests passed.', 0, '', 'finished'),
        ('parent_work', 'Fix the parser.', 'I found the cause and am updating the parser now.', 0, '', 'working'),
        ('optional_offer', 'Fix the parser.', 'The fix is complete and tests pass. I can add benchmarks if you would like.', 0, '', 'finished'),
        ('answered_request', 'Use option A and continue.', 'I am implementing option A now.', 0,
         'Assistant: Which option should I use?\nUser: Use option A and continue.', 'working'),
        ('rhetorical', 'Investigate the parser.', 'Why did parsing fail? The delimiter is missing. I am fixing it now.', 0, '', 'working'),
        ('missing_information', 'Configure the service.', 'Which hostname should the service use?', 0, '', 'input'),
        ('unclear', 'Continue.', 'There may be more to do.', 0, '', 'uncertain'),
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run', action='store_true', help='Send only the built-in synthetic scenarios to Jev')
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    questions = json.loads((ROOT / 'mj-core/src/activity/verdict_questions.json').read_text())
    if not args.run:
        print(f'Validated {len(scenarios())} built-in synthetic scenarios; no requests sent.')
        return
    key = os.environ.get('TYPESAFE_API_KEY', '').strip()
    if not key:
        key = (Path.home() / '.secrets/typesafe_api_key').read_text().strip()
    results = []
    for name, prompt, assistant, background, history, expectation in scenarios():
        state = dict(harness='codex', phase='running', silent_for_s=65, tools_in_flight=[],
                     transcript_summary=(history + '\nUser: ' + prompt + '\nAssistant: ' + assistant).strip(),
                     background_commands=background, queued_commands=0, user_prompt_tail=prompt,
                     assistant_text_tail=assistant)
        request = urllib.request.Request('https://api.typesafe.ai/v1/systemone',
            data=json.dumps(dict(model='jev-latest', state=state, questions=questions)).encode(),
            headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=30) as response:
            answer = json.loads(response.read(65537))['answers']
        result = dict(scenario=name, expectation=expectation, answers=answer)
        results.append(result)
        print(json.dumps(result), flush=True)
        if args.output:
            args.output.write_text(json.dumps({'synthetic_only': True, 'questions': questions, 'results': results}, indent=2) + '\n')


if __name__ == '__main__':
    main()
